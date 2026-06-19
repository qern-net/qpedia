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
    /// SDK provider guard owning the telemetry pipeline for the process
    /// lifetime. Held here so `serve()` can perform a bounded shutdown flush.
    /// `None` when telemetry is disabled / console-only, or for builders that
    /// don't own the pipeline (e.g. tests using `build()`).
    telemetry_guard: Option<crate::telemetry::TelemetryGuard>,
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

        // Telemetry: build the pure config from the environment, then
        // initialize the pipeline. Console logging is always installed; the
        // OTLP layer is added only when telemetry is enabled with a usable
        // endpoint, and any exporter init failure falls back to console-only
        // without aborting startup (Req 2.6). Overlay binaries that
        // pre-install a subscriber are tolerated (init_telemetry uses
        // try_init), so this is safe to call unconditionally.
        let telemetry_cfg = crate::telemetry::TelemetryConfig::from_env();
        let telemetry = crate::telemetry::init_telemetry(&telemetry_cfg);
        let telemetry_guard = telemetry.guard;

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
        let reranker = qpedia_embed::reranker_from_env(data_dir.join("models"));

        let ctx = IngestContext::new(db, blob, wiki_store, extractors, llm, embedder, reranker);
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
            telemetry_guard,
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
    /// was called), bind, and `axum::serve`. Installs a graceful-shutdown
    /// signal handler (ctrl-c / SIGTERM); after the server stops, flushes
    /// telemetry within the bounded timeout, logging any signal types that
    /// did not flush. Serving begins regardless of collector reachability
    /// (Req 9.1) — startup never blocks on the collector.
    pub async fn serve(mut self) -> Result<()> {
        let spawn_workers = self.spawn_workers;
        let bind = self.bind;
        let data_dir_display = self.ctx.wiki_store.root().display().to_string();
        let ctx_for_workers = self.ctx.clone();
        // Take the guard out before `build()` consumes `self`.
        let telemetry_guard = self.telemetry_guard.take();

        let app = self.build();

        if spawn_workers {
            spawn_job_runner(ctx_for_workers.clone());
            spawn_connector_scheduler(ctx_for_workers.clone());
            spawn_queue_depth_sampler(ctx_for_workers);
        }

        info!(%bind, wiki_root = %data_dir_display, "qpedia-api starting");
        let listener = tokio::net::TcpListener::bind(bind).await?;
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;

        // Server has stopped accepting connections — perform the bounded
        // telemetry flush (Req 2.4 / 2.7) before the process exits.
        if let Some(guard) = telemetry_guard {
            let outcome = guard.shutdown().await;
            if outcome.all_flushed() {
                info!("telemetry: all signals flushed on shutdown");
            } else {
                tracing::error!(
                    incomplete = ?outcome.incomplete_signals(),
                    "telemetry: shutdown flush timed out for some signal types"
                );
            }
        }
        Ok(())
    }
}

/// Resolve a graceful-shutdown future: completes on ctrl-c or, on Unix, on
/// SIGTERM. Used by `serve()` so in-flight requests drain and telemetry can
/// be flushed before exit.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received; draining in-flight requests");
}

// ---------- core router (the OSS routes) ---------------------------------------

fn core_router(upload_limit: usize) -> Router<AppState> {
    Router::new()
        .route("/healthz", get(routes::healthz))
        .route("/api/v1/health", get(routes::health))
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
        // ---- observability (task 11.2) ----
        // Authenticating Grafana reverse proxy. Catch-all for ALL methods at
        // `/api/v1/observability/grafana/*path`; this is the *sole* ingress to
        // the internal-only Grafana (the `otel-lgtm` service publishes no host
        // ports). The handler resolves the QPEDIA `User` first (401 when
        // absent), strips client `X-WEBAUTH-*` headers, and injects the trusted
        // identity + mapped Grafana org role. Registered BEFORE the trailing
        // `.layer(http_trace)` so proxy traffic is itself traced (design §9 —
        // this route is deliberately NOT in the HTTP_Span excluded set).
        .route(
            "/api/v1/observability/grafana/*path",
            axum::routing::any(crate::observability_proxy::grafana_proxy),
        )
        // HTTP tracing layer (task 5.4): opens an `HTTP_Span` per non-excluded
        // request, continuing the inbound W3C trace context. Added via
        // `Router::layer` so it wraps all routes *after* routing has matched,
        // which is what makes `MatchedPath` available to the middleware for the
        // `http.route` template. Excluded paths (/healthz, /metrics) take the
        // no-span fast path inside the middleware.
        .layer(axum::middleware::from_fn(
            crate::telemetry::http_layer::http_trace,
        ))
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

/// Periodically sample the pending-job count per kind and record it as the
/// `jobs.queue.depth` gauge (Req 5.8). Completion-time signals (`jobs.completed`,
/// `Job_Span`) capture *throughput* and *outcome* but not the current backlog,
/// so this sampler is the only source of live queue depth over time.
///
/// The sample goes through `PgStore::pending_job_counts_by_kind` (the
/// job-visibility/queue model), per project steering, rather than a hidden
/// side query. Interval is `QPEDIA_QUEUE_DEPTH_SAMPLE_SECS` (default 15s).
/// Uses the process-global meter installed by the telemetry pipeline; a no-op
/// meter when telemetry is disabled, so this is always safe to spawn.
fn spawn_queue_depth_sampler(ctx: IngestContext) {
    let interval_secs: u64 = std::env::var("QPEDIA_QUEUE_DEPTH_SAMPLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(15);
    tokio::spawn(async move {
        let gauge = opentelemetry::global::meter("qpedia-ingest")
            .u64_gauge("jobs.queue.depth")
            .with_description("Current number of pending (queued) jobs, labeled by kind")
            .build();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            match ctx.db.pending_job_counts_by_kind().await {
                Ok(counts) => {
                    for (kind, n) in counts {
                        gauge.record(
                            n.max(0) as u64,
                            &[opentelemetry::KeyValue::new("kind", kind)],
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "queue-depth sample failed"),
            }
        }
    });
    info!(interval_secs, "jobs.queue.depth sampler started");
}
