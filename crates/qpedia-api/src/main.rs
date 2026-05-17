mod auth;

use auth::{
    effective_wiki_acl, filter_sources, oidc_callback, oidc_login, oidc_logout,
    AuthExtractorState, AuthMode, AuthState, CallbackQuery, LoginQuery, User,
};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Redirect, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::StreamExt;
use qpedia_retriever::{ChatEvent, ChatRequest, Retriever};
use std::convert::Infallible;
use chrono::Utc;
use qpedia_core::{
    acl::Acl,
    source::{Source, SourceStatus},
    SourceId,
};
use qpedia_embed::embedder_from_env;
use qpedia_extract::ExtractorRegistry;
use qpedia_ingest::{ingest_job, lint_job, remove_job, sync_job, IngestContext, JobRunner};
use qpedia_llm::provider_from_env;
use qpedia_store::{
    blob::{BlobKind, BlobStorage, BlobStore},
    sqlite::{JobQueue, SourceStore},
    weaviate::weaviate_from_env,
    SearchHit, SqliteStore, WikiRepoStore,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    ctx: IngestContext,
    auth: AuthState,
}

impl axum::extract::FromRef<AppState> for AuthExtractorState {
    fn from_ref(s: &AppState) -> Self {
        AuthExtractorState {
            auth: s.auth.clone(),
            db: s.ctx.db.clone(),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "qpedia=info,tower_http=info".into()),
        )
        .init();

    let data_dir: PathBuf = std::env::var("QPEDIA_DATA_DIR")
        .unwrap_or_else(|_| "./data".into())
        .into();

    let db_path = data_dir.join("sqlite").join("qpedia.db");
    let raw_root = data_dir.join("raw");
    let wiki_root = data_dir.join("wiki");

    let author_name = std::env::var("QPEDIA_WIKI_AUTHOR_NAME")
        .unwrap_or_else(|_| "qpedia-bot".into());
    let author_email = std::env::var("QPEDIA_WIKI_AUTHOR_EMAIL")
        .unwrap_or_else(|_| "bot@qpedia.local".into());

    let db = SqliteStore::open(&db_path).await?;
    let blob = BlobStore::open(&raw_root)?;
    let wiki_store = WikiRepoStore::new(&wiki_root, author_name, author_email);
    let extractors = Arc::new(ExtractorRegistry::with_default());
    let llm = provider_from_env()?;
    if llm.is_none() {
        info!("no LLM provider configured — ingest will stop at Extracted");
    }

    // Local embedder (always available; downloads model on first use).
    let embedder = Some(embedder_from_env(data_dir.join("models")));

    // Weaviate is optional: degrades to fs-grep if unset/unreachable.
    let weaviate = weaviate_from_env().await.map(Arc::new);

    let ctx = IngestContext::new(db, blob, wiki_store, extractors, llm, embedder, weaviate);

    let auth = AuthState::from_env().await?;

    // Spawn the background job runner.
    let runner = JobRunner::new(ctx.clone(), "worker-1");
    tokio::spawn(runner.run());

    // Connector scheduler: every QPEDIA_SYNC_INTERVAL_SECS (default 300),
    // enqueue a Sync job for each enabled connector whose last_run_at is
    // older than QPEDIA_SYNC_STALE_SECS (default 900). Idempotent — if a
    // sync is already pending for a connector, the next due tick simply
    // queues another, which the runner serializes one-per-connector via
    // the connector's own update_connector_cursor on completion.
    {
        let sched_ctx = ctx.clone();
        let tick_secs: u64 = std::env::var("QPEDIA_SYNC_INTERVAL_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(300);
        let stale_ms: i64 = std::env::var("QPEDIA_SYNC_STALE_SECS")
            .ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or(900) * 1000;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
            tick.tick().await; // skip the immediate first fire
            loop {
                tick.tick().await;
                match sched_ctx.db.due_connectors(stale_ms, 50).await {
                    Ok(due) if !due.is_empty() => {
                        for c in due {
                            if let Ok(job) = qpedia_ingest::sync_job(&c.id) {
                                if let Err(e) = sched_ctx.db.enqueue(&job).await {
                                    tracing::warn!(connector = %c.name, error = %e, "scheduler: enqueue failed");
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

    let state = AppState { ctx, auth };

    let bind: SocketAddr = std::env::var("QPEDIA_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()?;

    // Allow uploads up to 256MB. Single docs in our scope rarely exceed
    // this; truly huge corpora should ship via the bulk-ingest path (TODO).
    let upload_limit = 256 * 1024 * 1024;

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/version", get(version))
        .route("/api/v1/auth/me", get(auth_me))
        .route("/auth/login", get(auth_login_route))
        .route("/auth/callback", get(auth_callback_route))
        .route("/auth/logout", get(auth_logout_route).post(auth_logout_route))
        .route(
            "/api/v1/sources",
            post(upload_source).get(list_sources).layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/api/v1/sources/:id", get(get_source).delete(delete_source))
        .route("/api/v1/sources/:id/original", get(download_source_original))
        .route("/api/v1/wiki/list", get(list_wiki_pages))
        .route("/api/v1/wiki/search", get(search_wiki))
        .route("/api/v1/wiki/pages/*path", get(get_wiki_page))
        .route("/api/v1/chat", post(chat))
        .route("/api/v1/admin/lint", post(enqueue_lint).get(last_lint_report))
        .route(
            "/api/v1/admin/folder-acls",
            get(list_folder_acls).put(set_folder_acl).delete(delete_folder_acl),
        )
        .route("/api/v1/admin/sources/stalled", get(list_stalled_sources))
        .route("/api/v1/admin/sources/resume", post(resume_stalled_sources))
        .route("/api/v1/admin/connectors", get(list_connectors).post(create_connector))
        .route("/api/v1/admin/connectors/:id", axum::routing::delete(delete_connector_route))
        .route("/api/v1/admin/connectors/:id/sync", post(trigger_connector_sync))
        .with_state(state);

    // Optional static SPA. In dev the user runs `npm run dev` (vite) and hits
    // :5173 which proxies to us; in prod or after `npm run build` we serve
    // from QPEDIA_WEB_DIR (default ./web/build, /app/web in container).
    let web_dir: PathBuf = std::env::var("QPEDIA_WEB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let in_container = PathBuf::from("/app/web");
            if in_container.exists() { in_container } else { PathBuf::from("./web/build") }
        });
    let app = if web_dir.join("index.html").exists() {
        info!(web_dir = %web_dir.display(), "serving static SPA");
        app.fallback_service(
            ServeDir::new(&web_dir).fallback(ServeFile::new(web_dir.join("index.html"))),
        )
    } else {
        info!(web_dir = %web_dir.display(), "no SPA build present — API-only (use `npm run dev` for the frontend)");
        app
    };

    info!(%bind, data_dir = %data_dir.display(), "qpedia-api starting");
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str { "ok" }

async fn version() -> Json<Value> {
    Json(json!({
        "name": "qpedia-api",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---------- auth routes ----------

async fn auth_me(user: User) -> Json<Value> {
    Json(json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "groups": user.groups,
        "tenant": user.tenant.as_str(),
        "is_admin": user.is_admin(),
    }))
}

async fn auth_login_route(
    State(s): State<AppState>,
    Query(q): Query<LoginQuery>,
) -> Result<Redirect, Response> {
    if matches!(s.auth.mode, AuthMode::Dev) {
        return Err((
            StatusCode::BAD_REQUEST,
            "auth is in dev mode — every request is admin",
        )
            .into_response());
    }
    oidc_login(&s.auth, &s.ctx.db, q).await
}

async fn auth_callback_route(
    State(s): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> Result<(HeaderMap, Redirect), Response> {
    oidc_callback(&s.auth, &s.ctx.db, q).await
}

async fn auth_logout_route(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> (HeaderMap, Redirect) {
    oidc_logout(&s.auth, &s.ctx.db, &headers).await
}

#[derive(Deserialize)]
struct ListQuery {
    folder: Option<String>,
    limit: Option<i64>,
}

async fn list_sources(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Source>>, ApiError> {
    let folder = q.folder.unwrap_or_else(|| "/".into());
    let limit = q.limit.unwrap_or(100).min(1000);
    let raw = s.ctx.db.list_sources(&user.tenant, &folder, limit * 5).await?;
    let mut filtered = filter_sources(&user, raw);
    filtered.truncate(limit as usize);
    Ok(Json(filtered))
}

async fn get_source(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<Json<Source>, ApiError> {
    match s.ctx.db.get_source_in(&user.tenant, &SourceId::from(id)).await? {
        Some(src) if user.can_read(&src.acl) => Ok(Json(src)),
        Some(_) => Err(ApiError::NotFound),
        None => Err(ApiError::NotFound),
    }
}

async fn download_source_original(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse as _;
    let sid = SourceId::from(id);
    let src = match s.ctx.db.get_source_in(&user.tenant, &sid).await? {
        Some(src) if user.can_read(&src.acl) => src,
        Some(_) | None => return Err(ApiError::NotFound),
    };
    let ext = std::path::Path::new(&src.filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let bytes = s.ctx.blob.get(&sid, BlobKind::Original, ext).await
        .map_err(|_| ApiError::NotFound)?;
    let disposition = format!(
        "attachment; filename=\"{}\"",
        src.filename.replace('"', "_")
    );
    Ok((
        [
            (header::CONTENT_TYPE, src.mime.clone()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    ).into_response())
}

/// Enqueue a Remove job. Cleanup (wiki commit, Weaviate, blobs, row delete)
/// happens async; the source row remains visible until the job completes.
async fn delete_source(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let sid = SourceId::from(id);
    let Some(existing) = s.ctx.db.get_source_in(&user.tenant, &sid).await? else {
        return Err(ApiError::NotFound);
    };
    if !user.can_read(&existing.acl) {
        return Err(ApiError::NotFound);
    }
    let job = remove_job(&sid).map_err(ApiError::Internal)?;
    let job_id = job.id.to_string();
    s.ctx.db.enqueue(&job).await?;
    s.ctx
        .db
        .audit(&user.id, "source.remove.requested", Some(sid.as_str()), None)
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"job_id": job_id, "kind": "remove", "source_id": sid.as_str(), "state": "queued"})),
    ))
}

#[derive(Deserialize)]
struct WikiListQuery {
    prefix: Option<String>,
}

async fn list_wiki_pages(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<WikiListQuery>,
) -> Result<Json<Value>, ApiError> {
    let prefix = q.prefix.unwrap_or_default();
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let pages = wiki
        .list_pages(&prefix)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "prefix": prefix, "pages": pages })))
}

#[derive(Deserialize)]
struct WikiSearchQuery {
    q: String,
    limit: Option<usize>,
}

async fn search_wiki(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<WikiSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = q.limit.unwrap_or(10).min(50);
    let (mode, hits) = run_search(&s, &user, &q.q, limit * 3).await?;
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;

    let mut allowed = Vec::with_capacity(hits.len());
    for h in hits {
        if let Some(content) = wiki.read_page(&h.path).await.ok().flatten() {
            let source_ids = parse_source_ids(&content);
            let acl = effective_wiki_acl(&s.ctx.db, &user.tenant, &source_ids).await;
            if source_ids.is_empty() || user.can_read(&acl) {
                allowed.push(h);
                if allowed.len() >= limit as usize { break; }
            }
        }
    }
    Ok(Json(json!({"query": q.q, "mode": mode, "hits": allowed})))
}

/// Parse `source_ids: [...]` from frontmatter without pulling in the lint
/// crate — duplicated intentionally to keep deps clean.
fn parse_source_ids(content: &str) -> Vec<String> {
    let trimmed = content.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else { return Vec::new() };
    let Some(end) = after.find("\n---") else { return Vec::new() };
    let fm = &after[..end];
    let mut out = Vec::new();
    for line in fm.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("source_ids:") {
            let s = rest.trim().trim_start_matches('[').trim_end_matches(']');
            for x in s.split(',') {
                let x = x.trim().trim_matches('"').trim_matches('\'');
                if !x.is_empty() {
                    out.push(x.to_string());
                }
            }
        }
    }
    out
}

async fn run_search(
    s: &AppState,
    user: &User,
    query: &str,
    limit: usize,
) -> Result<(&'static str, Vec<SearchHit>), ApiError> {
    if let (Some(embedder), Some(weaviate)) = (&s.ctx.embedder, &s.ctx.weaviate) {
        let qv = embedder
            .embed(&[query])
            .await
            .map_err(ApiError::Internal)?
            .into_iter()
            .next()
            .unwrap_or_default();
        match weaviate.hybrid_search(&user.tenant, query, &qv, limit).await {
            Ok(h) if !h.is_empty() => return Ok(("hybrid", h)),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "weaviate search failed; falling back"),
        }
    }
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let hits = wiki.search_text(query, limit).await.map_err(ApiError::Internal)?;
    Ok(("filesystem", hits))
}

async fn enqueue_lint(State(s): State<AppState>, user: User) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let job = lint_job(&user.tenant).map_err(ApiError::Internal)?;
    let job_id = job.id.to_string();
    s.ctx.db.enqueue(&job).await?;
    Ok(Json(json!({"job_id": job_id, "kind": "lint", "tenant": user.tenant.as_str(), "state": "queued"})))
}

#[derive(Deserialize)]
struct FolderAclBody {
    folder_path: String,
    acl: Vec<String>,
}

#[derive(Deserialize)]
struct FolderAclDeleteQuery {
    folder_path: String,
}

async fn list_folder_acls(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let rows = s
        .ctx
        .db
        .list_folder_acls(&user.tenant)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(path, acl, updated_at, updated_by)| {
            json!({
                "folder_path": path,
                "acl": acl.0.iter().cloned().collect::<Vec<_>>(),
                "updated_at": updated_at.to_rfc3339(),
                "updated_by": updated_by,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn set_folder_acl(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<FolderAclBody>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let acl = Acl::from_iter(body.acl.iter().cloned());
    s.ctx
        .db
        .set_folder_acl(&user.tenant, &body.folder_path, &acl, &user.id)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    s.ctx
        .db
        .audit(
            &user.id,
            "folder_acl.set",
            Some(&body.folder_path),
            Some(&json!({"acl": body.acl})),
        )
        .await?;
    Ok(Json(json!({"folder_path": body.folder_path, "acl": body.acl})))
}

async fn delete_folder_acl(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<FolderAclDeleteQuery>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    s.ctx
        .db
        .delete_folder_acl(&user.tenant, &q.folder_path)
        .await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    s.ctx
        .db
        .audit(&user.id, "folder_acl.delete", Some(&q.folder_path), None)
        .await?;
    Ok(Json(json!({"deleted": q.folder_path})))
}

// ---------- connectors (admin) ----------

#[derive(Deserialize)]
struct CreateConnectorBody {
    kind: String,
    name: String,
    config: Value,
    #[serde(default = "default_true")]
    enabled: bool,
}
fn default_true() -> bool { true }

async fn list_connectors(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() { return Err(ApiError::Forbidden); }
    let rows = s.ctx.db.list_connectors(&user.tenant).await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    // Don't echo config_json — it carries credentials.
    let items: Vec<Value> = rows.into_iter().map(|c| json!({
        "id": c.id,
        "tenant": c.tenant,
        "kind": c.kind,
        "name": c.name,
        "cursor": c.cursor,
        "enabled": c.enabled,
        "last_run_at": c.last_run_at.map(|t| t.to_rfc3339()),
        "last_error": c.last_error,
    })).collect();
    Ok(Json(json!({ "items": items })))
}

async fn create_connector(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<CreateConnectorBody>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() { return Err(ApiError::Forbidden); }
    let id = ulid::Ulid::new().to_string();
    let cfg = qpedia_connectors::ConnectorConfig {
        id: id.clone(),
        tenant: user.tenant.as_str().to_string(),
        kind: body.kind.clone(),
        name: body.name.clone(),
        config_json: body.config,
        cursor: None,
        enabled: body.enabled,
        last_run_at: None,
        last_error: None,
    };
    s.ctx.db.insert_connector(&cfg).await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    s.ctx.db.audit(&user.id, "connector.create", Some(&id),
        Some(&json!({"kind": cfg.kind, "name": cfg.name, "tenant": cfg.tenant}))).await?;
    Ok(Json(json!({"id": id, "kind": cfg.kind, "name": cfg.name, "enabled": cfg.enabled})))
}

async fn delete_connector_route(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() { return Err(ApiError::Forbidden); }
    let existing = s.ctx.db.get_connector(&id).await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?
        .ok_or(ApiError::NotFound)?;
    if existing.tenant != user.tenant.as_str() {
        return Err(ApiError::NotFound);
    }
    s.ctx.db.delete_connector(&id).await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    s.ctx.db.audit(&user.id, "connector.delete", Some(&id), None).await?;
    Ok(Json(json!({"deleted": id})))
}

async fn trigger_connector_sync(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() { return Err(ApiError::Forbidden); }
    let existing = s.ctx.db.get_connector(&id).await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?
        .ok_or(ApiError::NotFound)?;
    if existing.tenant != user.tenant.as_str() {
        return Err(ApiError::NotFound);
    }
    let job = sync_job(&id).map_err(ApiError::Internal)?;
    let job_id = job.id.to_string();
    s.ctx.db.enqueue(&job).await?;
    Ok(Json(json!({"job_id": job_id, "connector_id": id, "state": "queued"})))
}

async fn last_lint_report(State(s): State<AppState>, user: User) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    match wiki.read_page("_meta/lint.json").await {
        Ok(Some(text)) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Ok(Json(v)),
            Err(_) => Ok(Json(json!({"raw": text}))),
        },
        Ok(None) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Internal(e)),
    }
}

async fn list_stalled_sources(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let sources = s.ctx.db.list_stalled(&user.tenant, 200).await?;
    let count = sources.len();
    Ok(Json(json!({ "sources": sources, "count": count })))
}

async fn resume_stalled_sources(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let stalled = s.ctx.db.list_stalled(&user.tenant, 1000).await?;
    let count = stalled.len();
    for src in &stalled {
        let job = ingest_job(&src.id).map_err(ApiError::Internal)?;
        s.ctx.db.enqueue(&job).await?;
    }
    s.ctx.db.audit(&user.id, "admin.resume_stalled", None,
        Some(&json!({"count": count, "tenant": user.tenant.as_str()}))).await?;
    info!(count, tenant = %user.tenant, "stalled sources re-enqueued by admin");
    Ok(Json(json!({ "enqueued": count })))
}

async fn chat(
    State(s): State<AppState>,
    user: User,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let llm = s.ctx.llm.clone().ok_or_else(|| {
        ApiError::Bad("chat requires an LLM provider — set ANTHROPIC_API_KEY or OPENAI_API_KEY".into())
    })?;

    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let retriever = Retriever {
        embedder: s.ctx.embedder.clone(),
        weaviate: s.ctx.weaviate.clone(),
        wiki,
        db: s.ctx.db.clone(),
        llm,
        user_groups: user.groups.clone(),
        tenant: user.tenant.clone(),
    };

    let event_stream = retriever.chat(req).map(|ev| {
        let event = match ev {
            ChatEvent::Meta { .. } => Event::default()
                .event("meta")
                .json_data(&ev)
                .unwrap_or_else(|_| Event::default().event("meta")),
            ChatEvent::Token { .. } => Event::default()
                .event("token")
                .json_data(&ev)
                .unwrap_or_else(|_| Event::default().event("token")),
            ChatEvent::Done => Event::default().event("done").data(""),
            ChatEvent::Error { .. } => Event::default()
                .event("error")
                .json_data(&ev)
                .unwrap_or_else(|_| Event::default().event("error")),
        };
        Ok::<_, Infallible>(event)
    });

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

async fn get_wiki_page(
    State(s): State<AppState>,
    user: User,
    Path(path): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse as _;
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let content = match wiki.read_page(&path).await {
        Ok(Some(c)) => c,
        Ok(None) => return Err(ApiError::NotFound),
        Err(e) => return Err(ApiError::Internal(e)),
    };
    let source_ids = parse_source_ids(&content);
    if !source_ids.is_empty() {
        let acl = effective_wiki_acl(&s.ctx.db, &user.tenant, &source_ids).await;
        if !user.can_read(&acl) {
            return Err(ApiError::NotFound);
        }
    }
    Ok((
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        content,
    )
        .into_response())
}

async fn upload_source(
    State(s): State<AppState>,
    user: User,
    mut mp: Multipart,
) -> Result<Json<Source>, ApiError> {
    let mut folder_path = "/".to_string();
    let mut filename = String::new();
    let mut bytes: Option<bytes::Bytes> = None;

    while let Some(field) = mp.next_field().await.map_err(|e| ApiError::Bad(e.to_string()))? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "folder_path" => folder_path = field.text().await.map_err(|e| ApiError::Bad(e.to_string()))?,
            "file" => {
                filename = field.file_name().unwrap_or("upload.bin").to_string();
                let b = field.bytes().await.map_err(|e| ApiError::Bad(e.to_string()))?;
                bytes = Some(b);
            }
            _ => { let _ = field.bytes().await; }
        }
    }

    let bytes = bytes.ok_or_else(|| ApiError::Bad("missing 'file' field".into()))?;
    if filename.is_empty() {
        return Err(ApiError::Bad("missing filename".into()));
    }

    let mime = mime_guess::from_path(&filename)
        .first_or_octet_stream()
        .to_string();

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha256 = hex::encode(hasher.finalize());

    let id = SourceId::new();
    let now = Utc::now();
    // ACL resolution: prefer a folder-level ACL (closest ancestor wins);
    // fall back to the uploader's groups so single-user / dev mode keeps
    // working without configuration.
    let upload_acl = match s.ctx.db.resolve_folder_acl(&user.tenant, &folder_path).await? {
        Some(acl) => acl,
        None => Acl::from_iter(user.groups.iter().cloned()),
    };
    let src = Source {
        id: id.clone(),
        tenant: user.tenant.clone(),
        folder_path,
        filename: filename.clone(),
        mime: mime.clone(),
        sha256,
        size_bytes: bytes.len() as u64,
        acl: upload_acl,
        status: SourceStatus::Pending,
        language: None,
        created_at: now,
        ingested_at: None,
        classification: None,
    };
    s.ctx.db.insert_source(&src).await?;

    // Persist raw bytes.
    let ext = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    s.ctx.blob.put(&id, BlobKind::Original, ext, bytes).await?;

    // Hand off to the background runner.
    let job = ingest_job(&id).map_err(|e| ApiError::Internal(e))?;
    s.ctx.db.enqueue(&job).await?;
    s.ctx.db.audit(&user.id, "source.upload", Some(id.as_str()), None).await?;

    info!(id = %id, mime = %mime, "source enqueued");
    Ok(Json(src))
}

// ---------- error mapping ----------

#[derive(Debug)]
enum ApiError {
    NotFound,
    Bad(String),
    Forbidden,
    Internal(anyhow::Error),
}

impl From<qpedia_core::Error> for ApiError {
    fn from(e: qpedia_core::Error) -> Self {
        match e {
            qpedia_core::Error::NotFound(_) => ApiError::NotFound,
            qpedia_core::Error::Invalid(s) => ApiError::Bad(s),
            other => ApiError::Internal(anyhow::anyhow!(other.to_string())),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self { ApiError::Internal(e.into()) }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Bad(s) => (StatusCode::BAD_REQUEST, s),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".into()),
            ApiError::Internal(e) => {
                error!(error = %e, "internal");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}
