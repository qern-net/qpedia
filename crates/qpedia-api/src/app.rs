//! `AppBuilder` — the composable application surface.
//!
//! Built from environment (`AppBuilder::from_env`), augmented by overlays
//! via `.with_*` methods, finalized to an `axum::Router` (`build()`) or
//! served directly (`serve()`).
//!
//! ## Extension model
//!
//! - **Routes:** [`AppBuilder::with_routes`] merges any `Router<AppState>`
//!   into the final application. Overlay handlers extract their own state
//!   from the [`Extensions`] container on `AppState`.
//! - **State extensions:** [`AppBuilder::with_state_extension`] stores a
//!   typed value (e.g. a billing client). Handlers retrieve it with
//!   `state.extensions.get::<MyType>()`.
//! - **Event sinks:** [`AppBuilder::with_event_sink`] registers an
//!   [`EventSink`] for audit / observability events. Default is a no-op;
//!   the tracing + Postgres `audit` table writes happen unconditionally
//!   at the existing call sites. Integration of registered sinks into
//!   those call sites lands in Band 0.2.
//! - **Tenant hooks:** [`AppBuilder::with_tenant_hook`] registers a
//!   [`TenantHook`] that fires on tenant create / delete. Default is a
//!   no-op. Wiring at `/api/v1/admin/bootstrap` and the (future) tenant
//!   admin endpoints lands in Band 0.3.

use crate::auth::{AuthExtractorState, AuthState};
use crate::rate_limit::ChatRateLimiter;
use crate::routes;
use anyhow::{Context, Result};
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use qpedia_core::tenant::Tenant;
use qpedia_embed::embedder_from_env;
use qpedia_extract::ExtractorRegistry;
use qpedia_ingest::{sync_job, IngestContext, JobRunner};
use qpedia_llm::provider_from_env;
use qpedia_pg_store::PgStore;
use qpedia_store::{blob::BlobStore, WikiRepoStore};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};
use tracing::info;

// ---------- AppState + extension container -------------------------------------

/// Shared state held by axum and cloned per request.
///
/// Core fields (`ctx`, `auth`) are public so overlay routes can read them.
/// Custom services injected via [`AppBuilder::with_state_extension`] are
/// retrieved from the [`Extensions`] container.
#[derive(Clone)]
pub struct AppState {
    pub ctx: IngestContext,
    pub auth: AuthState,
    pub extensions: Extensions,
    /// Per-tenant token-bucket limiter for `POST /api/v1/chat`.
    /// Overlay-overridable via [`AppBuilder::with_chat_rate_limiter`].
    pub chat_rate_limiter: Arc<ChatRateLimiter>,
}

/// Typed extension map. Cheap-Clone (Arc-backed).
#[derive(Clone, Default)]
pub struct Extensions {
    inner: Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Extensions {
    /// Fetch the extension of type `T` if any was registered with
    /// [`AppBuilder::with_state_extension`].
    pub fn get<T: Any + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.inner
            .get(&TypeId::of::<T>())
            .and_then(|v| Arc::clone(v).downcast::<T>().ok())
    }
}

impl axum::extract::FromRef<AppState> for AuthExtractorState {
    fn from_ref(s: &AppState) -> Self {
        AuthExtractorState {
            auth: s.auth.clone(),
            db: s.ctx.db.clone(),
        }
    }
}

// ---------- EventSink: re-exported from pg-store; integrated in Band 0.2 ------
//
// The trait + NoopEventSink live in `qpedia_pg_store::events` so every
// caller of `db.write_audit(...)` fires registered sinks — including
// background-job handlers in qpedia-ingest, not just HTTP routes.
// `AppBuilder::with_event_sink` below registers via `PgStore::register_event_sink`.

pub use qpedia_pg_store::{EventSink, NoopEventSink};

// ---------- TenantHook: re-exported from pg-store; integrated in Band 0.3 ----
//
// Same shape as EventSink: trait + NoopTenantHook live in
// `qpedia_pg_store::events`. Hooks fire from `PgStore::upsert_tenant`
// on a detached task after the row is durably committed. The
// `/api/v1/admin/bootstrap` route inherits firing automatically since
// it calls `db.upsert_tenant(...)`.

pub use qpedia_pg_store::{NoopTenantHook, TenantHook};

// ---------- AppBuilder ---------------------------------------------------------

const DEFAULT_UPLOAD_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Composable application builder. See module-level docs.
pub struct AppBuilder {
    ctx: IngestContext,
    auth: AuthState,
    bind: SocketAddr,
    web_dir: PathBuf,
    upload_limit_bytes: usize,
    extension_map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    extra_routers: Vec<Router<AppState>>,
    chat_rate_limiter: Arc<ChatRateLimiter>,
    spawn_workers: bool,
}

impl AppBuilder {
    /// Construct from environment variables. Mirrors the original `main()`:
    /// connect Postgres, open blob + wiki stores, load LLM + embedder, build
    /// the `IngestContext`, set up auth. Initializes `tracing` if no
    /// subscriber is set yet (idempotent for tests that pre-install one).
    pub async fn from_env() -> Result<Self> {
        // Load .env for local (non-Docker) runs. Real environment variables
        // always win over .env entries. Missing file is fine.
        let _ = dotenvy::dotenv();

        // Best-effort tracing init — overlay binaries that need a custom
        // subscriber can install one before calling from_env.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "qpedia=info,tower_http=info".into()),
            )
            .try_init();

        let data_dir: PathBuf = std::env::var("QPEDIA_DATA_DIR")
            .unwrap_or_else(|_| "./data".into())
            .into();

        let raw_root = data_dir.join("raw");
        let wiki_root = data_dir.join("wiki");

        let author_name = std::env::var("QPEDIA_WIKI_AUTHOR_NAME")
            .unwrap_or_else(|_| "qpedia-bot".into());
        let author_email = std::env::var("QPEDIA_WIKI_AUTHOR_EMAIL")
            .unwrap_or_else(|_| "bot@qpedia.local".into());

        // Canonical DSN var is QPEDIA_DB_URL; DATABASE_URL is accepted as a
        // fallback for tooling that sets the sqlx-conventional name.
        let pg_dsn = std::env::var("QPEDIA_DB_URL")
            .or_else(|_| std::env::var("QPEDIA_DATABASE_URL"))
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| {
                "postgres://qpedia_admin:qpedia-dev@127.0.0.1:5432/qpedia?sslmode=disable".into()
            });
        let db = PgStore::connect(&pg_dsn).await?;
        let blob = BlobStore::open(&raw_root)?;
        let wiki_store = WikiRepoStore::new(&wiki_root, author_name, author_email);
        let extractors = Arc::new(ExtractorRegistry::with_default());
        let llm = provider_from_env()?;
        if llm.is_none() {
            info!("no LLM provider configured — ingest will stop at Extracted");
        }
        let embedder = Some(embedder_from_env(data_dir.join("models")));

        let ctx = IngestContext::new(db, blob, wiki_store, extractors, llm, embedder);
        let auth = AuthState::from_env().await?;

        let bind: SocketAddr = std::env::var("QPEDIA_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8080".into())
            .parse()
            .context("parse QPEDIA_BIND")?;

        let web_dir: PathBuf = std::env::var("QPEDIA_WEB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let in_container = PathBuf::from("/app/web");
                if in_container.exists() {
                    in_container
                } else {
                    PathBuf::from("./web/build")
                }
            });

        Ok(Self {
            ctx,
            auth,
            bind,
            web_dir,
            upload_limit_bytes: DEFAULT_UPLOAD_LIMIT_BYTES,
            extension_map: HashMap::new(),
            extra_routers: Vec::new(),
            chat_rate_limiter: Arc::new(ChatRateLimiter::from_env()),
            spawn_workers: true,
        })
    }

    /// Replace the default in-process [`ChatRateLimiter`]. Overlays
    /// register e.g. a Redis-backed limiter that survives multiple
    /// `qpedia-api` replicas.
    pub fn with_chat_rate_limiter(mut self, limiter: Arc<ChatRateLimiter>) -> Self {
        self.chat_rate_limiter = limiter;
        self
    }

    /// Merge an additional `Router<AppState>` into the final application.
    /// Overlay routes share `AppState`, so handlers can `State<AppState>`
    /// and pull their own service from `state.extensions.get::<T>()`.
    pub fn with_routes(mut self, router: Router<AppState>) -> Self {
        self.extra_routers.push(router);
        self
    }

    /// Inject a typed extension into `AppState`. Handlers retrieve it via
    /// `state.extensions.get::<T>()`.
    pub fn with_state_extension<T: Any + Send + Sync + 'static>(mut self, value: T) -> Self {
        self.extension_map
            .insert(TypeId::of::<T>(), Arc::new(value));
        self
    }

    /// Register an [`EventSink`]. Delegates to
    /// [`PgStore::register_event_sink`], so every `db.write_audit(...)`
    /// caller (including background-job handlers) fires the sink after
    /// committing the audit row. See trait docs.
    pub fn with_event_sink<S: EventSink>(self, sink: S) -> Self {
        self.ctx.db.register_event_sink(Arc::new(sink));
        self
    }

    /// Register a [`TenantHook`]. Delegates to
    /// [`PgStore::register_tenant_hook`], so every `db.upsert_tenant(...)`
    /// caller (including `/api/v1/admin/bootstrap`) fires the hook after
    /// committing the tenant row. See trait docs.
    pub fn with_tenant_hook<H: TenantHook>(self, hook: H) -> Self {
        self.ctx.db.register_tenant_hook(Arc::new(hook));
        self
    }

    /// Override the bind address. Defaults to `QPEDIA_BIND` or `0.0.0.0:8080`.
    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.bind = addr;
        self
    }

    /// Override the static SPA directory. Defaults to `QPEDIA_WEB_DIR`, then
    /// `/app/web` (in container), then `./web/build`.
    pub fn web_dir(mut self, dir: PathBuf) -> Self {
        self.web_dir = dir;
        self
    }

    /// Override the multipart upload byte limit. Default 256 MiB.
    pub fn upload_limit_bytes(mut self, bytes: usize) -> Self {
        self.upload_limit_bytes = bytes;
        self
    }

    /// Skip spawning the background `JobRunner` and connector scheduler.
    /// Useful for one-shot tasks, tests, or read-only deployments.
    pub fn without_workers(mut self) -> Self {
        self.spawn_workers = false;
        self
    }

    /// Borrowed view of the runtime ingest context. Useful in tests + for
    /// overlay startup code that needs the DB / blob store before serving.
    pub fn ctx(&self) -> &IngestContext {
        &self.ctx
    }

    /// Build the `axum::Router` with all routes merged + state applied.
    /// Does **not** spawn workers and does **not** bind.
    pub fn build(self) -> Router {
        let upload_limit = self.upload_limit_bytes;
        let state = AppState {
            ctx: self.ctx,
            auth: self.auth,
            extensions: Extensions {
                inner: Arc::new(self.extension_map),
            },
            chat_rate_limiter: self.chat_rate_limiter,
        };

        let mut router = core_router(upload_limit);
        for r in self.extra_routers {
            router = router.merge(r);
        }
        let app = router.with_state(state);

        if self.web_dir.join("index.html").exists() {
            info!(web_dir = %self.web_dir.display(), "serving static SPA");
            app.fallback_service(
                ServeDir::new(&self.web_dir)
                    .fallback(ServeFile::new(self.web_dir.join("index.html"))),
            )
        } else {
            info!(
                web_dir = %self.web_dir.display(),
                "no SPA build present — API-only (use `npm run dev` for the frontend)"
            );
            app
        }
    }

    /// Convenience: spawn workers (unless [`without_workers`](Self::without_workers)
    /// was called), bind, and `axum::serve`. Runs forever or until error.
    pub async fn serve(self) -> Result<()> {
        let spawn_workers = self.spawn_workers;
        let bind = self.bind;
        let data_dir_display = self.ctx.wiki_store.root().display().to_string();
        let ctx_for_workers = self.ctx.clone();

        let app = self.build();

        if spawn_workers {
            spawn_job_runner(ctx_for_workers.clone());
            spawn_connector_scheduler(ctx_for_workers);
        }

        info!(%bind, wiki_root = %data_dir_display, "qpedia-api starting");
        let listener = tokio::net::TcpListener::bind(bind).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

// ---------- core router (the OSS routes) ---------------------------------------

fn core_router(upload_limit: usize) -> Router<AppState> {
    Router::new()
        .route("/healthz", get(routes::healthz))
        .route("/api/v1/version", get(routes::version))
        .route("/api/v1/auth/config", get(routes::auth_config))
        .route("/api/v1/auth/me", get(routes::auth_me))
        .route("/auth/login", get(routes::auth_login_route))
        .route("/auth/callback", get(routes::auth_callback_route))
        .route(
            "/auth/logout",
            get(routes::auth_logout_route).post(routes::auth_logout_route),
        )
        .route("/api/v1/auth/firebase/login", post(routes::firebase_login_route))
        .route(
            "/api/v1/sources",
            post(routes::upload_source)
                .get(routes::list_sources)
                .layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route(
            "/api/v1/sources/:id",
            get(routes::get_source).delete(routes::delete_source),
        )
        .route(
            "/api/v1/sources/:id/original",
            get(routes::download_source_original),
        )
        .route(
            "/api/v1/sources/:id/replace",
            post(routes::replace_source).layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/api/v1/sources/:id/move", post(routes::move_source))
        .route(
            "/api/v1/folders",
            get(routes::list_folders)
                .post(routes::create_folder)
                .patch(routes::patch_folder)
                .delete(routes::delete_folder),
        )
        .route("/api/v1/wiki/list", get(routes::list_wiki_pages))
        .route("/api/v1/wiki/search", get(routes::search_wiki))
        .route("/api/v1/wiki/pages/*path", get(routes::get_wiki_page))
        .route("/api/v1/chat", post(routes::chat))
        .route(
            "/api/v1/admin/lint",
            post(routes::enqueue_lint).get(routes::last_lint_report),
        )
        .route("/api/v1/admin/reembed", post(routes::enqueue_reembed))
        .route(
            "/api/v1/admin/folder-acls",
            get(routes::list_folder_acls)
                .put(routes::set_folder_acl)
                .delete(routes::delete_folder_acl),
        )
        .route("/api/v1/admin/queue", get(routes::queue_overview))
        .route(
            "/api/v1/admin/sources/stalled",
            get(routes::list_stalled_sources),
        )
        .route(
            "/api/v1/admin/sources/resume",
            post(routes::resume_stalled_sources),
        )
        .route(
            "/api/v1/admin/connectors",
            get(routes::list_connectors).post(routes::create_connector),
        )
        .route(
            "/api/v1/admin/connectors/:id",
            axum::routing::delete(routes::delete_connector_route),
        )
        .route(
            "/api/v1/admin/connectors/:id/sync",
            post(routes::trigger_connector_sync),
        )
        .route(
            "/api/v1/connectors/google/authorize",
            get(routes::google_authorize),
        )
        .route(
            "/api/v1/connectors/google/callback",
            get(routes::google_callback),
        )
        .route("/api/v1/admin/bootstrap", post(routes::bootstrap_tenant))
        // ---- workspaces (Band 4.1) ----
        .route(
            "/api/v1/workspaces",
            get(routes::list_workspaces).post(routes::create_workspace),
        )
        .route(
            "/api/v1/workspaces/:id/switch",
            post(routes::switch_workspace),
        )
        .route(
            "/api/v1/workspaces/members",
            get(routes::list_workspace_members),
        )
        .route(
            "/api/v1/workspaces/members/:user_id",
            axum::routing::delete(routes::remove_workspace_member),
        )
        .route(
            "/api/v1/workspaces/invites",
            get(routes::list_workspace_invites).post(routes::create_workspace_invite),
        )
        .route(
            "/api/v1/workspaces/invites/:id",
            axum::routing::delete(routes::delete_workspace_invite),
        )
        .route(
            "/api/v1/invites/:token",
            get(routes::get_invite).post(routes::accept_invite),
        )
        .route(
            "/api/v1/workspaces/domains",
            get(routes::list_domains).post(routes::add_domain),
        )
        .route(
            "/api/v1/workspaces/domains/:domain/verify",
            post(routes::verify_domain),
        )
        .route(
            "/api/v1/workspaces/domains/:domain",
            axum::routing::delete(routes::delete_domain),
        )
}

// ---------- background spawns --------------------------------------------------

/// Spawn the JobRunner pool. Size comes from `QPEDIA_WORKERS` (default 1,
/// clamped to `[1, 32]`); each worker has a distinct id `worker-N` so
/// the `jobs.locked_by` column tells you which one holds a lease.
///
/// Concurrent claims are safe because `claim_next_job` uses
/// `SELECT … FOR UPDATE SKIP LOCKED LIMIT 1` — two workers polling at
/// the same instant pick different rows.
fn spawn_job_runner(ctx: IngestContext) {
    let workers: usize = std::env::var("QPEDIA_WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(|n: usize| n.clamp(1, 32))
        .unwrap_or(1);
    for i in 1..=workers {
        let runner = JobRunner::new(ctx.clone(), format!("worker-{i}"));
        tokio::spawn(runner.run());
    }
    info!(workers, "job runner pool started");
}

fn spawn_connector_scheduler(ctx: IngestContext) {
    let tick_secs: u64 = std::env::var("QPEDIA_SYNC_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let stale_ms: i64 = std::env::var("QPEDIA_SYNC_STALE_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(900)
        * 1000;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
        tick.tick().await; // skip the immediate first fire
        loop {
            tick.tick().await;
            match ctx.db.due_connectors(stale_ms, 50).await {
                Ok(due) if !due.is_empty() => {
                    for c in due {
                        let tenant = Tenant::new(c.tenant.clone());
                        if let Ok(job) = sync_job(&tenant, &c.id) {
                            if let Err(e) = ctx.db.enqueue(&tenant, &job).await {
                                tracing::warn!(
                                    connector = %c.name,
                                    error = %e,
                                    "scheduler: enqueue failed"
                                );
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "scheduler: due_connectors failed"),
            }
        }
    });
    info!(tick_secs, stale_ms, "connector sync scheduler started");
}
