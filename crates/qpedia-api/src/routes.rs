//! HTTP handlers for the OSS application. All exposed as `pub(crate)` so
//! `app::core_router` can mount them; the public application surface is
//! `AppBuilder` in `app`. Helper request/response types stay private to
//! this module.

use crate::app::AppState;
use crate::auth::{
    self, effective_wiki_acl, filter_sources, mint_session, oidc_callback, oidc_login,
    oidc_logout, AuthMode, CallbackQuery, LoginQuery, User,
};
use axum::{
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Redirect, Response,
    },
    Json,
};
use chrono::Utc;
use futures::stream::StreamExt;
use qpedia_core::{
    acl::Acl,
    source::{Source, SourceStatus},
    SourceId,
};
use qpedia_pg_store::SearchHit;
use qpedia_retriever::{ChatEvent, ChatRequest, Retriever};
use qpedia_store::blob::{BlobKind, BlobStorage};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use tracing::{error, info};

// ============================================================================
// health / version
// ============================================================================

pub(crate) async fn healthz() -> &'static str {
    "ok"
}

pub(crate) async fn version() -> Json<Value> {
    Json(json!({
        "name": "qpedia-api",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ============================================================================
// auth routes
// ============================================================================

pub(crate) async fn auth_me(user: User) -> Json<Value> {
    Json(json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "groups": user.groups,
        "tenant": user.tenant.as_str(),
        "is_admin": user.is_admin(),
    }))
}

pub(crate) async fn auth_login_route(
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

pub(crate) async fn auth_callback_route(
    State(s): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> Result<(HeaderMap, Redirect), Response> {
    oidc_callback(&s.auth, &s.ctx.db, q).await
}

pub(crate) async fn auth_logout_route(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> (HeaderMap, Redirect) {
    oidc_logout(&s.auth, &s.ctx.db, &headers).await
}

#[derive(Deserialize)]
pub(crate) struct FirebaseLoginBody {
    id_token: String,
}

/// Exchange a Firebase ID token for a qpedia session cookie. Frontend
/// signs the user in via the Firebase JS SDK (any provider — Google,
/// GitHub, Microsoft, X, Apple, generic OIDC SSO) and POSTs the
/// resulting ID token here.
pub(crate) async fn firebase_login_route(
    State(s): State<AppState>,
    Json(body): Json<FirebaseLoginBody>,
) -> Result<(HeaderMap, Json<Value>), ApiError> {
    let verifier = s
        .auth
        .firebase
        .as_ref()
        .ok_or_else(|| ApiError::Bad(
            "Firebase auth not configured (set QPEDIA_FIREBASE_PROJECT_ID)".into(),
        ))?;

    let claims = verifier
        .verify(&body.id_token)
        .await
        .map_err(|e| ApiError::Bad(format!("invalid Firebase ID token: {e}")))?;

    let tenant = if let Some(t) = &claims.tenant_id {
        qpedia_core::tenant::Tenant::new(t.clone())
    } else if let Some(domain) = claims
        .email
        .as_deref()
        .and_then(|e| e.split_once('@').map(|(_, d)| d.to_string()))
    {
        match s.ctx.db.tenant_by_email_domain(&domain).await {
            Ok(Some(t)) => t,
            _ => std::env::var("QPEDIA_DEV_TENANT")
                .ok()
                .map(qpedia_core::tenant::Tenant::new)
                .unwrap_or_else(qpedia_core::tenant::Tenant::default_tenant),
        }
    } else {
        std::env::var("QPEDIA_DEV_TENANT")
            .ok()
            .map(qpedia_core::tenant::Tenant::new)
            .unwrap_or_else(qpedia_core::tenant::Tenant::default_tenant)
    };

    let token = auth::random_token(32);
    let token_hash = auth::hash_token(&token);
    let user_id = format!("firebase:{}", claims.sub);
    mint_session(
        &s.ctx.db,
        &token_hash,
        &tenant,
        &user_id,
        claims.email.as_deref(),
        claims.name.as_deref(),
        Some(&claims.firebase.sign_in_provider),
        &claims.groups,
        auth::SESSION_TTL_SECS,
    )
    .await
    .map_err(ApiError::Internal)?;

    let mut headers = HeaderMap::new();
    headers.append(
        axum::http::header::SET_COOKIE,
        auth::set_session_cookie(&token, auth::SESSION_TTL_SECS),
    );

    info!(
        user = %user_id,
        tenant = %tenant,
        provider = %claims.firebase.sign_in_provider,
        "firebase login"
    );

    Ok((
        headers,
        Json(json!({
            "user_id": user_id,
            "tenant": tenant.as_str(),
            "email": claims.email,
            "name": claims.name,
            "provider": claims.firebase.sign_in_provider,
            "groups": claims.groups,
        })),
    ))
}

// ============================================================================
// sources
// ============================================================================

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    folder: Option<String>,
    limit: Option<i64>,
}

pub(crate) async fn list_sources(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Source>>, ApiError> {
    let folder = q.folder.unwrap_or_else(|| "/".into());
    let limit = q.limit.unwrap_or(100).min(1000);
    let raw = s
        .ctx
        .db
        .list_sources(&user.tenant, &folder, limit * 5)
        .await
        .map_err(ApiError::Internal)?;
    let mut filtered = filter_sources(&user, raw);
    filtered.truncate(limit as usize);
    Ok(Json(filtered))
}

pub(crate) async fn get_source(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<Json<Source>, ApiError> {
    match s
        .ctx
        .db
        .get_source_in(&user.tenant, &SourceId::from(id))
        .await
        .map_err(ApiError::Internal)?
    {
        Some(src) if user.can_read(&src.acl) => Ok(Json(src)),
        Some(_) | None => Err(ApiError::NotFound),
    }
}

pub(crate) async fn download_source_original(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse as _;
    let sid = SourceId::from(id);
    let src = match s
        .ctx
        .db
        .get_source_in(&user.tenant, &sid)
        .await
        .map_err(ApiError::Internal)?
    {
        Some(src) if user.can_read(&src.acl) => src,
        Some(_) | None => return Err(ApiError::NotFound),
    };
    let ext = std::path::Path::new(&src.filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let bytes = s
        .ctx
        .blob
        .get(&sid, BlobKind::Original, ext)
        .await
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
    )
        .into_response())
}

/// Enqueue a Remove job. Cleanup (wiki commit, pgvector, blobs, row delete)
/// happens async; the source row remains visible until the job completes.
pub(crate) async fn delete_source(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let sid = SourceId::from(id);
    let Some(existing) = s
        .ctx
        .db
        .get_source_in(&user.tenant, &sid)
        .await
        .map_err(ApiError::Internal)?
    else {
        return Err(ApiError::NotFound);
    };
    if !user.can_read(&existing.acl) {
        return Err(ApiError::NotFound);
    }
    let job = qpedia_ingest::remove_job(&user.tenant, &sid).map_err(ApiError::Internal)?;
    let job_id = job.id.to_string();
    s.ctx.db.enqueue(&user.tenant, &job).await.map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "source.remove.requested",
            Some(sid.as_str()),
            None,
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "kind": "remove",
            "source_id": sid.as_str(),
            "state": "queued",
        })),
    ))
}

// ============================================================================
// folders + move
// ============================================================================

#[derive(Deserialize)]
pub(crate) struct MoveSourceBody {
    folder_path: String,
}

pub(crate) async fn move_source(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
    Json(body): Json<MoveSourceBody>,
) -> Result<Json<Value>, ApiError> {
    let sid = SourceId::from(id);
    let Some(existing) = s
        .ctx
        .db
        .get_source_in(&user.tenant, &sid)
        .await
        .map_err(ApiError::Internal)?
    else {
        return Err(ApiError::NotFound);
    };
    if !user.can_read(&existing.acl) {
        return Err(ApiError::NotFound);
    }
    let folder = qpedia_pg_store::slugify_folder(&body.folder_path);
    s.ctx
        .db
        .update_folder_path(&user.tenant, &sid, &folder)
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "source.move",
            Some(sid.as_str()),
            Some(&json!({ "folder_path": folder })),
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "id": sid.as_str(), "folder_path": folder })))
}

pub(crate) async fn list_folders(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    let rows = s
        .ctx
        .db
        .list_folders(&user.tenant)
        .await
        .map_err(ApiError::Internal)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|f| json!({ "path": f.path, "pinned": f.pinned }))
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub(crate) struct CreateFolderBody {
    path: String,
    #[serde(default = "default_true")]
    pinned: bool,
}

pub(crate) async fn create_folder(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<CreateFolderBody>,
) -> Result<Json<Value>, ApiError> {
    if body.path.trim().is_empty() || body.path.trim() == "/" {
        return Err(ApiError::Bad("folder path required".into()));
    }
    let path = s
        .ctx
        .db
        .create_folder(&user.tenant, &body.path, body.pinned, &user.id)
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "folder.create",
            Some(&path),
            Some(&json!({ "pinned": body.pinned })),
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "path": path, "pinned": body.pinned })))
}

#[derive(Deserialize)]
pub(crate) struct PatchFolderBody {
    path: String,
    pinned: bool,
}

pub(crate) async fn patch_folder(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<PatchFolderBody>,
) -> Result<Json<Value>, ApiError> {
    s.ctx
        .db
        .set_folder_pinned(&user.tenant, &body.path, body.pinned, &user.id)
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "folder.set_pinned",
            Some(&body.path),
            Some(&json!({ "pinned": body.pinned })),
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "path": body.path, "pinned": body.pinned })))
}

#[derive(Deserialize)]
pub(crate) struct DeleteFolderQuery {
    path: String,
}

pub(crate) async fn delete_folder(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<DeleteFolderQuery>,
) -> Result<Json<Value>, ApiError> {
    let n = s
        .ctx
        .db
        .folder_source_count(&user.tenant, &q.path)
        .await
        .map_err(ApiError::Internal)?;
    if n > 0 {
        return Err(ApiError::Bad(format!(
            "folder not empty: {n} file(s) here or in subfolders — move them out first"
        )));
    }
    s.ctx
        .db
        .delete_folder(&user.tenant, &q.path)
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(&user.tenant, &user.id, "folder.delete", Some(&q.path), None)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "deleted": q.path })))
}

// ============================================================================
// wiki + search + chat
// ============================================================================

#[derive(Deserialize)]
pub(crate) struct WikiListQuery {
    prefix: Option<String>,
}

pub(crate) async fn list_wiki_pages(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<WikiListQuery>,
) -> Result<Json<Value>, ApiError> {
    let prefix = q.prefix.unwrap_or_default();
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let pages = wiki.list_pages(&prefix).await.map_err(ApiError::Internal)?;
    Ok(Json(json!({ "prefix": prefix, "pages": pages })))
}

#[derive(Deserialize)]
pub(crate) struct WikiSearchQuery {
    q: String,
    limit: Option<usize>,
}

pub(crate) async fn search_wiki(
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
                allowed.push(json!({
                    "path": h.path,
                    "title": h.title,
                    "snippet": h.snippet,
                    "score": h.score,
                }));
                if allowed.len() >= limit {
                    break;
                }
            }
        }
    }
    Ok(Json(json!({ "query": q.q, "mode": mode, "hits": allowed })))
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
    if let Some(embedder) = &s.ctx.embedder {
        let qv = embedder
            .embed(&[query])
            .await
            .map_err(ApiError::Internal)?
            .into_iter()
            .next()
            .unwrap_or_default();
        match s
            .ctx
            .db
            .hybrid_search(&user.tenant, query, qv, 0.7, limit as i64)
            .await
        {
            Ok(h) if !h.is_empty() => return Ok(("hybrid", h)),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "pg hybrid search failed; falling back"),
        }
    }
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let hits = wiki
        .search_text(query, limit)
        .await
        .map_err(ApiError::Internal)?
        .into_iter()
        .map(|h| SearchHit {
            path: h.path,
            title: h.title,
            snippet: h.snippet,
            score: 0.0,
        })
        .collect();
    Ok(("filesystem", hits))
}

pub(crate) async fn chat(
    State(s): State<AppState>,
    user: User,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let llm = s.ctx.llm.clone().ok_or_else(|| {
        ApiError::Bad(
            "chat requires an LLM provider — set ANTHROPIC_API_KEY or OPENAI_API_KEY".into(),
        )
    })?;

    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let retriever = Retriever {
        embedder: s.ctx.embedder.clone(),
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

pub(crate) async fn get_wiki_page(
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

pub(crate) async fn upload_source(
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
            "folder_path" => {
                folder_path = field
                    .text()
                    .await
                    .map_err(|e| ApiError::Bad(e.to_string()))?;
            }
            "file" => {
                filename = field.file_name().unwrap_or("upload.bin").to_string();
                let b = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::Bad(e.to_string()))?;
                bytes = Some(b);
            }
            _ => {
                let _ = field.bytes().await;
            }
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

    let slug = qpedia_pg_store::unique_source_slug(&s.ctx.db, &user.tenant, &filename)
        .await
        .map_err(ApiError::Internal)?;
    let id = SourceId::from(slug);
    let now = Utc::now();
    let upload_acl = match s
        .ctx
        .db
        .resolve_folder_acl(&user.tenant, &folder_path)
        .await
        .map_err(ApiError::Internal)?
    {
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
    s.ctx.db.insert_source(&src).await.map_err(ApiError::Internal)?;

    let ext = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    s.ctx.blob.put(&id, BlobKind::Original, ext, bytes).await?;

    let job = qpedia_ingest::ingest_job(&user.tenant, &id).map_err(ApiError::Internal)?;
    s.ctx.db.enqueue(&user.tenant, &job).await.map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "source.upload",
            Some(id.as_str()),
            None,
        )
        .await
        .map_err(ApiError::Internal)?;

    info!(id = %id, mime = %mime, "source enqueued");
    Ok(Json(src))
}

// ============================================================================
// admin: lint / reembed / folder ACLs / connectors / bootstrap
// ============================================================================

pub(crate) async fn enqueue_lint(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let job = qpedia_ingest::lint_job(&user.tenant).map_err(ApiError::Internal)?;
    let job_id = job.id.to_string();
    s.ctx.db.enqueue(&user.tenant, &job).await.map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "job_id": job_id,
        "kind": "lint",
        "tenant": user.tenant.as_str(),
        "state": "queued",
    })))
}

pub(crate) async fn enqueue_reembed(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let job = qpedia_ingest::reembed_job(&user.tenant).map_err(ApiError::Internal)?;
    let job_id = job.id.to_string();
    s.ctx.db.enqueue(&user.tenant, &job).await.map_err(ApiError::Internal)?;
    info!(tenant = %user.tenant, job_id = %job_id, "reembed job enqueued");
    Ok(Json(json!({
        "job_id": job_id,
        "kind": "reembed",
        "tenant": user.tenant.as_str(),
        "state": "queued",
    })))
}

#[derive(Deserialize)]
pub(crate) struct FolderAclBody {
    folder_path: String,
    acl: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct FolderAclDeleteQuery {
    folder_path: String,
}

pub(crate) async fn list_folder_acls(
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
        .map_err(ApiError::Internal)?;
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

pub(crate) async fn set_folder_acl(
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
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "folder_acl.set",
            Some(&body.folder_path),
            Some(&json!({ "acl": body.acl })),
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "folder_path": body.folder_path, "acl": body.acl })))
}

pub(crate) async fn delete_folder_acl(
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
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "folder_acl.delete",
            Some(&q.folder_path),
            None,
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "deleted": q.folder_path })))
}

#[derive(Deserialize)]
pub(crate) struct CreateConnectorBody {
    kind: String,
    name: String,
    config: Value,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

pub(crate) async fn list_connectors(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let rows = s
        .ctx
        .db
        .list_connectors(&user.tenant)
        .await
        .map_err(ApiError::Internal)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "tenant": c.tenant,
                "kind": c.kind,
                "name": c.name,
                "cursor": c.cursor,
                "enabled": c.enabled,
                "last_run_at": c.last_run_at.map(|t| t.to_rfc3339()),
                "last_error": c.last_error,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

pub(crate) async fn create_connector(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<CreateConnectorBody>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let cfg = qpedia_connectors::ConnectorConfig {
        id: String::new(),
        tenant: user.tenant.as_str().to_string(),
        kind: body.kind.clone(),
        name: body.name.clone(),
        config_json: body.config,
        cursor: None,
        enabled: body.enabled,
        last_run_at: None,
        last_error: None,
    };
    let id = s
        .ctx
        .db
        .insert_connector(&user.tenant, &cfg)
        .await
        .map_err(ApiError::Internal)?;
    let id = id.to_string();
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "connector.create",
            Some(&id),
            Some(&json!({ "kind": cfg.kind, "name": cfg.name, "tenant": cfg.tenant })),
        )
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "id": id,
        "kind": cfg.kind,
        "name": cfg.name,
        "enabled": cfg.enabled,
    })))
}

pub(crate) async fn delete_connector_route(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let existing = s
        .ctx
        .db
        .get_connector(&user.tenant, &id)
        .await
        .map_err(ApiError::Internal)?
        .ok_or(ApiError::NotFound)?;
    if existing.tenant != user.tenant.as_str() {
        return Err(ApiError::NotFound);
    }
    s.ctx
        .db
        .delete_connector(&user.tenant, &id)
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(&user.tenant, &user.id, "connector.delete", Some(&id), None)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "deleted": id })))
}

pub(crate) async fn trigger_connector_sync(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let existing = s
        .ctx
        .db
        .get_connector(&user.tenant, &id)
        .await
        .map_err(ApiError::Internal)?
        .ok_or(ApiError::NotFound)?;
    if existing.tenant != user.tenant.as_str() {
        return Err(ApiError::NotFound);
    }
    let job = qpedia_ingest::sync_job(&user.tenant, &id).map_err(ApiError::Internal)?;
    let job_id = job.id.to_string();
    s.ctx.db.enqueue(&user.tenant, &job).await.map_err(ApiError::Internal)?;
    Ok(Json(json!({ "job_id": job_id, "connector_id": id, "state": "queued" })))
}

// ---------- first-run bootstrap ----------

#[derive(Deserialize)]
pub(crate) struct BootstrapFolderAcl {
    folder_path: String,
    acl: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct BootstrapFolder {
    path: String,
    #[serde(default = "default_true")]
    pinned: bool,
}

#[derive(Deserialize)]
pub(crate) struct BootstrapBody {
    tenant_id: String,
    display_name: String,
    #[serde(default)]
    email_domain: Option<String>,
    #[serde(default)]
    initial_folder_acls: Vec<BootstrapFolderAcl>,
    #[serde(default)]
    initial_folders: Vec<BootstrapFolder>,
}

/// One-shot tenant bootstrap. See `OPEN-CORE.md` for the rationale on
/// where SaaS-level bootstrap belongs (qpedia-pvt overlay would gate this
/// more carefully than admin=group=admin).
pub(crate) async fn bootstrap_tenant(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<BootstrapBody>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let target = qpedia_core::tenant::Tenant::new(body.tenant_id);
    if target.as_str().is_empty() {
        return Err(ApiError::Bad("tenant_id required".into()));
    }
    if body.display_name.trim().is_empty() {
        return Err(ApiError::Bad("display_name required".into()));
    }

    s.ctx
        .db
        .upsert_tenant(&target, &body.display_name, body.email_domain.as_deref())
        .await
        .map_err(ApiError::Internal)?;

    let mut acls_applied = 0usize;
    for entry in &body.initial_folder_acls {
        if entry.folder_path.trim().is_empty() {
            continue;
        }
        let acl = Acl::from_iter(entry.acl.iter().cloned());
        s.ctx
            .db
            .set_folder_acl(&target, &entry.folder_path, &acl, &user.id)
            .await
            .map_err(ApiError::Internal)?;
        acls_applied += 1;
    }

    let mut folders_applied = 0usize;
    for f in &body.initial_folders {
        if f.path.trim().is_empty() || f.path.trim() == "/" {
            continue;
        }
        s.ctx
            .db
            .create_folder(&target, &f.path, f.pinned, &user.id)
            .await
            .map_err(ApiError::Internal)?;
        folders_applied += 1;
    }

    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "tenant.bootstrap",
            Some(target.as_str()),
            Some(&json!({
                "target_tenant": target.as_str(),
                "display_name": body.display_name,
                "email_domain": body.email_domain,
                "folder_acls": acls_applied,
                "folders": folders_applied,
            })),
        )
        .await
        .map_err(ApiError::Internal)?;

    Ok(Json(json!({
        "tenant": target.as_str(),
        "display_name": body.display_name,
        "email_domain": body.email_domain,
        "folder_acls": acls_applied,
        "folders": folders_applied,
        "firebase_project_id": std::env::var("QPEDIA_FIREBASE_PROJECT_ID").ok(),
    })))
}

pub(crate) async fn last_lint_report(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    match wiki.read_page("_meta/lint.json").await {
        Ok(Some(text)) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Ok(Json(v)),
            Err(_) => Ok(Json(json!({ "raw": text }))),
        },
        Ok(None) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Internal(e)),
    }
}

const STALLED_AFTER_SECS: i64 = 30 * 60;

pub(crate) async fn list_stalled_sources(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let sources = s
        .ctx
        .db
        .list_stalled(&user.tenant, STALLED_AFTER_SECS, 200)
        .await
        .map_err(ApiError::Internal)?;
    let count = sources.len();
    Ok(Json(json!({ "sources": sources, "count": count })))
}

pub(crate) async fn resume_stalled_sources(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let stalled = s
        .ctx
        .db
        .list_stalled(&user.tenant, STALLED_AFTER_SECS, 1000)
        .await
        .map_err(ApiError::Internal)?;
    let count = stalled.len();
    for src in &stalled {
        let job = qpedia_ingest::ingest_job(&user.tenant, &src.id).map_err(ApiError::Internal)?;
        s.ctx.db.enqueue(&user.tenant, &job).await.map_err(ApiError::Internal)?;
    }
    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "admin.resume_stalled",
            None,
            Some(&json!({ "count": count, "tenant": user.tenant.as_str() })),
        )
        .await
        .map_err(ApiError::Internal)?;
    info!(count, tenant = %user.tenant, "stalled sources re-enqueued by admin");
    Ok(Json(json!({ "enqueued": count })))
}

// ============================================================================
// error mapping (exposed so overlays can return the same error shape)
// ============================================================================

#[derive(Debug)]
pub enum ApiError {
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
    fn from(e: std::io::Error) -> Self {
        ApiError::Internal(e.into())
    }
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
