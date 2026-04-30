use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use qpedia_core::{
    acl::Acl,
    source::{Source, SourceStatus},
    SourceId,
};
use qpedia_embed::embedder_from_env;
use qpedia_extract::ExtractorRegistry;
use qpedia_ingest::{ingest_job, IngestContext, JobRunner};
use qpedia_llm::provider_from_env;
use qpedia_store::{
    blob::{BlobKind, BlobStorage, BlobStore},
    sqlite::{JobQueue, SourceStore},
    weaviate::weaviate_from_env,
    SearchHit, SqliteStore, WikiRepo,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    ctx: IngestContext,
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
    let wiki = WikiRepo::open_or_init(&wiki_root, author_name, author_email).await?;
    let extractors = Arc::new(ExtractorRegistry::with_default());
    let llm = provider_from_env()?;
    if llm.is_none() {
        info!("no LLM provider configured — ingest will stop at Extracted");
    }

    // Local embedder (always available; downloads model on first use).
    let embedder = Some(embedder_from_env(data_dir.join("models")));

    // Weaviate is optional: degrades to fs-grep if unset/unreachable.
    let weaviate = weaviate_from_env().await.map(Arc::new);

    let ctx = IngestContext::new(db, blob, wiki, extractors, llm, embedder, weaviate);

    // Spawn the background job runner.
    let runner = JobRunner::new(ctx.clone(), "worker-1");
    tokio::spawn(runner.run());

    let state = AppState { ctx };

    let bind: SocketAddr = std::env::var("QPEDIA_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()?;

    // Allow uploads up to 256MB. Single docs in our scope rarely exceed
    // this; truly huge corpora should ship via the bulk-ingest path (TODO).
    let upload_limit = 256 * 1024 * 1024;

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/version", get(version))
        .route(
            "/api/v1/sources",
            post(upload_source).get(list_sources).layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/api/v1/sources/:id", get(get_source))
        .route("/api/v1/wiki/list", get(list_wiki_pages))
        .route("/api/v1/wiki/search", get(search_wiki))
        .route("/api/v1/wiki/pages/*path", get(get_wiki_page))
        .with_state(state);

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

#[derive(Deserialize)]
struct ListQuery {
    folder: Option<String>,
    limit: Option<i64>,
}

async fn list_sources(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Source>>, ApiError> {
    let folder = q.folder.unwrap_or_else(|| "/".into());
    let limit = q.limit.unwrap_or(100).min(1000);
    let rows = s.ctx.db.list_sources(&folder, limit).await?;
    Ok(Json(rows))
}

async fn get_source(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Source>, ApiError> {
    match s.ctx.db.get_source(&SourceId::from(id)).await? {
        Some(src) => Ok(Json(src)),
        None => Err(ApiError::NotFound),
    }
}

#[derive(Deserialize)]
struct WikiListQuery {
    prefix: Option<String>,
}

async fn list_wiki_pages(
    State(s): State<AppState>,
    Query(q): Query<WikiListQuery>,
) -> Result<Json<Value>, ApiError> {
    let prefix = q.prefix.unwrap_or_default();
    let pages = s
        .ctx
        .wiki
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
    Query(q): Query<WikiSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = q.limit.unwrap_or(10).min(50);
    let (mode, hits) = run_search(&s, &q.q, limit).await?;
    Ok(Json(json!({"query": q.q, "mode": mode, "hits": hits})))
}

async fn run_search(s: &AppState, query: &str, limit: usize) -> Result<(&'static str, Vec<SearchHit>), ApiError> {
    if let (Some(embedder), Some(weaviate)) = (&s.ctx.embedder, &s.ctx.weaviate) {
        let qv = embedder
            .embed(&[query])
            .await
            .map_err(ApiError::Internal)?
            .into_iter()
            .next()
            .unwrap_or_default();
        match weaviate.hybrid_search(query, &qv, limit).await {
            Ok(h) if !h.is_empty() => return Ok(("hybrid", h)),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "weaviate search failed; falling back"),
        }
    }
    let hits = s.ctx.wiki.search_text(query, limit).await.map_err(ApiError::Internal)?;
    Ok(("filesystem", hits))
}

async fn get_wiki_page(
    State(s): State<AppState>,
    Path(path): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse as _;
    match s.ctx.wiki.read_page(&path).await {
        Ok(Some(content)) => Ok((
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            content,
        )
            .into_response()),
        Ok(None) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Internal(e)),
    }
}

async fn upload_source(
    State(s): State<AppState>,
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
    let src = Source {
        id: id.clone(),
        folder_path,
        filename: filename.clone(),
        mime: mime.clone(),
        sha256,
        size_bytes: bytes.len() as u64,
        acl: Acl::default(),
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
    s.ctx.db.audit("user:anon", "source.upload", Some(id.as_str()), None).await?;

    info!(id = %id, mime = %mime, "source enqueued");
    Ok(Json(src))
}

// ---------- error mapping ----------

#[derive(Debug)]
enum ApiError {
    NotFound,
    Bad(String),
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
            ApiError::Internal(e) => {
                error!(error = %e, "internal");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}
