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

/// Public, unauthenticated: tells the frontend which login UI to show.
/// `mode` is dev | firebase | oidc; `firebase` is whether a Firebase
/// verifier is configured. The `/login` page routes off this.
pub(crate) async fn auth_config(State(s): State<AppState>) -> Json<Value> {
    let mode = match s.auth.mode {
        AuthMode::Dev => "dev",
        AuthMode::Session => "firebase",
        AuthMode::Oidc(_) => "oidc",
    };
    Json(json!({
        "mode": mode,
        "firebase": s.auth.firebase.is_some(),
    }))
}

pub(crate) async fn auth_me(user: User) -> Json<Value> {
    let kind = if user.tenant.as_str().starts_with(INDIVIDUAL_TENANT_PREFIX) {
        "individual"
    } else {
        "org"
    };
    Json(json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "groups": user.groups,
        "tenant": user.tenant.as_str(),
        "tenant_kind": kind,
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

/// Prefix marking an individual (per-user) tenant. Org tenants never
/// start with this; the `auth_me` endpoint reports the workspace kind
/// from it.
pub(crate) const INDIVIDUAL_TENANT_PREFIX: &str = "u-";

/// The individual (per-user) tenant id for a Firebase uid. Stable per
/// user, so two people at the same email domain never collide.
pub(crate) fn individual_tenant_id(sub: &str) -> String {
    format!("{INDIVIDUAL_TENANT_PREFIX}{}", qpedia_pg_store::slugify(sub))
}

/// Resolve the tenant for a Firebase login.
///   1. Explicit `tenant_id` custom claim — set only by the (future)
///      org SSO / invite flow when a user is provisioned into an org.
///   2. Otherwise → the user's **individual** workspace `u-<uid>`,
///      isolated per user.
///
/// Every login starts individual, even a corporate-domain email; joining
/// or creating an org is an explicit, separately-authorized action (see
/// AUTH-DESIGN.md). The resolved tenant is upserted so the row exists for
/// the FK + RLS before the session is minted.
async fn resolve_firebase_tenant(
    db: &qpedia_pg_store::PgStore,
    claims: &crate::firebase::FirebaseClaims,
) -> Result<qpedia_core::tenant::Tenant, ApiError> {
    use qpedia_core::tenant::Tenant;

    if let Some(t) = claims.tenant_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let tenant = Tenant::new(t);
        ensure_tenant(db, &tenant, t, None).await?;
        return Ok(tenant);
    }

    let tenant = Tenant::new(individual_tenant_id(&claims.sub));
    let display = claims.email.as_deref().unwrap_or("Individual");
    ensure_tenant(db, &tenant, display, None).await?;
    Ok(tenant)
}

/// Create the tenant row if it doesn't already exist (preserves the
/// display name / domain of registered tenants).
async fn ensure_tenant(
    db: &qpedia_pg_store::PgStore,
    tenant: &qpedia_core::tenant::Tenant,
    display_name: &str,
    email_domain: Option<&str>,
) -> Result<(), ApiError> {
    if db
        .get_tenant(tenant)
        .await
        .map_err(ApiError::Internal)?
        .is_none()
    {
        db.upsert_tenant(tenant, display_name, email_domain)
            .await
            .map_err(ApiError::Internal)?;
    }
    Ok(())
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

    let tenant = resolve_firebase_tenant(&s.ctx.db, &claims).await?;

    // Groups: start from the Firebase custom claim, add the platform
    // admin-by-email bootstrap (QPEDIA_ADMIN_EMAILS), and — crucially —
    // grant `admin` when this is the user's OWN individual workspace.
    // RLS scopes that power to `u-<uid>`, so it's "owner of my own space,"
    // never cross-tenant. Org admin is decided by the org flow, not here.
    let mut groups = auth::augment_admin_by_email(claims.email.as_deref(), claims.groups.clone());
    if tenant.as_str() == individual_tenant_id(&claims.sub)
        && !groups.iter().any(|g| g == "admin")
    {
        groups.push("admin".to_string());
    }

    let token = auth::random_token(32);
    let token_hash = auth::hash_token(&token);
    let user_id = format!("firebase:{}", claims.sub);

    // Ensure a membership row exists for the resolved workspace — owner of
    // your own individual space, member otherwise. Idempotent.
    let role = if tenant.as_str() == individual_tenant_id(&claims.sub) {
        "owner"
    } else {
        "member"
    };
    let _ = s
        .ctx
        .db
        .ensure_membership(&tenant, &user_id, claims.email.as_deref(), role)
        .await;

    mint_session(
        &s.ctx.db,
        &token_hash,
        &tenant,
        &user_id,
        claims.email.as_deref(),
        claims.name.as_deref(),
        Some(&claims.firebase.sign_in_provider),
        &groups,
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
            "groups": groups,
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

/// Replace a source's file in place. Same slug, same folder, same ACL —
/// only the underlying bytes (and the metadata derived from them) change.
/// The pipeline re-runs from `Pending`; the ingest agent updates existing
/// wiki pages that reference this `source_id` instead of creating
/// duplicates (its `propose_new` would be rejected by the validator
/// since those paths already exist).
///
/// Useful for: corrected scans, redacted versions, freshened reports.
/// If the new bytes hash to the same SHA256 as the existing source,
/// this is a no-op (returns 200 with the unchanged row).
pub(crate) async fn replace_source(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<String>,
    mut mp: Multipart,
) -> Result<Json<Source>, ApiError> {
    let sid = SourceId::from(id);

    // Existing source must exist and be writable by the caller.
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

    let mut filename = String::new();
    let mut bytes: Option<bytes::Bytes> = None;
    while let Some(field) = mp.next_field().await.map_err(|e| ApiError::Bad(e.to_string()))? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                filename = field
                    .file_name()
                    .unwrap_or(existing.filename.as_str())
                    .to_string();
                bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| ApiError::Bad(e.to_string()))?,
                );
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = bytes.ok_or_else(|| ApiError::Bad("missing 'file' field".into()))?;
    if filename.is_empty() {
        filename = existing.filename.clone();
    }

    // Identical bytes — nothing to do. Return the existing row so the
    // client UI just re-renders the unchanged source.
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha256 = hex::encode(hasher.finalize());
    if sha256 == existing.sha256 {
        info!(id = %sid, "replace: identical bytes — no-op");
        return Ok(Json(existing));
    }

    let mime = mime_guess::from_path(&filename)
        .first_or_octet_stream()
        .to_string();
    let size = bytes.len() as u64;

    // Persist new blob first; on failure the DB row still points at the
    // old SHA256 + filename and the source remains in its prior status.
    let ext = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    s.ctx.blob.put(&sid, BlobKind::Original, ext, bytes).await?;

    s.ctx
        .db
        .replace_source_blob(&user.tenant, &sid, &filename, &mime, &sha256, size)
        .await
        .map_err(ApiError::Internal)?;

    let job = qpedia_ingest::ingest_job(&user.tenant, &sid).map_err(ApiError::Internal)?;
    s.ctx
        .db
        .enqueue(&user.tenant, &job)
        .await
        .map_err(ApiError::Internal)?;

    s.ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "source.replaced",
            Some(sid.as_str()),
            Some(&json!({
                "old_sha256": existing.sha256,
                "new_sha256": sha256,
                "filename":   filename,
            })),
        )
        .await
        .map_err(ApiError::Internal)?;

    // Return the refreshed row so the client can re-render immediately.
    let updated = s
        .ctx
        .db
        .get_source_in(&user.tenant, &sid)
        .await
        .map_err(ApiError::Internal)?
        .ok_or(ApiError::NotFound)?;
    info!(id = %sid, mime = %mime, "source replaced; ingest re-enqueued");
    Ok(Json(updated))
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
            .hybrid_search(&user.tenant, query, qv, limit as i64)
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
    // Token-bucket guard. One token per chat invocation; defaults are
    // generous (30 RPM, burst 10) but cap LLM spend per tenant.
    if let Err(retry_after) = s.chat_rate_limiter.check(&user.tenant) {
        return Err(ApiError::TooManyRequests(retry_after));
    }

    let llm = s.ctx.llm.clone().ok_or_else(|| {
        ApiError::Bad(
            "chat requires an LLM provider — set ANTHROPIC_API_KEY or OPENAI_API_KEY".into(),
        )
    })?;

    let wiki = s.ctx.wiki_store.get(&user.tenant).await.map_err(ApiError::Internal)?;
    let retriever = Retriever {
        embedder: s.ctx.embedder.clone(),
        reranker: s.ctx.reranker.clone(),
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

// ============================================================================
// Google Drive connect (SSO-aligned OAuth authorization-code flow)
// ============================================================================
//
// Two steps. The admin's browser drives both:
//   1. GET .../google/authorize  -> JSON { authorize_url }. Frontend sets
//      window.location to it. We stash a random CSRF `state` in
//      oidc_pending (reused as a generic short-lived state store).
//   2. Google redirects the browser (top-level GET, session cookie rides
//      along under SameSite=Lax) to .../google/callback?code&state. We
//      verify state, exchange the code for a refresh token, record an
//      oauth_grant, and create a `gdrive` connector wired with the
//      refresh token. Redirect to /admin.
//
// Requires GOOGLE_OAUTH_CLIENT_ID / _SECRET in the environment, plus a
// redirect URI registered on the Google OAuth client that matches
// GOOGLE_OAUTH_REDIRECT_URL.

fn google_oauth_env() -> Result<(String, String, String), ApiError> {
    let client_id = std::env::var("GOOGLE_OAUTH_CLIENT_ID")
        .map_err(|_| ApiError::Bad("GOOGLE_OAUTH_CLIENT_ID not set".into()))?;
    let client_secret = std::env::var("GOOGLE_OAUTH_CLIENT_SECRET")
        .map_err(|_| ApiError::Bad("GOOGLE_OAUTH_CLIENT_SECRET not set".into()))?;
    let redirect = std::env::var("GOOGLE_OAUTH_REDIRECT_URL").unwrap_or_else(|_| {
        "http://127.0.0.1:8080/api/v1/connectors/google/callback".into()
    });
    Ok((client_id, client_secret, redirect))
}

#[derive(Deserialize)]
pub(crate) struct GoogleAuthorizeQuery {
    /// Optional Drive folder id to restrict ingestion to.
    folder_id: Option<String>,
}

/// Step 1: hand the frontend a Google consent URL. Admin only.
pub(crate) async fn google_authorize(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<GoogleAuthorizeQuery>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let (client_id, _secret, redirect) = google_oauth_env()?;

    // Random CSRF state, stashed in oidc_pending (a generic short-lived
    // state store). redirect_after carries the optional folder id so the
    // callback can wire it into the connector config.
    let state = auth::random_token(24);
    let folder = q.folder_id.unwrap_or_default();
    s.ctx
        .db
        .save_pending(&state, "n/a", "n/a", Some(&folder))
        .await
        .map_err(ApiError::Internal)?;

    let url = qpedia_connectors::oauth::consent_url(
        &client_id,
        &redirect,
        qpedia_connectors::oauth::DRIVE_READONLY_SCOPE,
        &state,
    );
    Ok(Json(json!({ "authorize_url": url })))
}

#[derive(Deserialize)]
pub(crate) struct GoogleCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Step 2: Google redirects here. Exchange the code, record the grant,
/// create the connector, redirect to /admin. Admin only (the session
/// cookie rides along on the top-level redirect).
pub(crate) async fn google_callback(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<GoogleCallbackQuery>,
) -> Result<Redirect, Response> {
    let forbidden =
        || (StatusCode::FORBIDDEN, "admin required").into_response();
    if !user.is_admin() {
        return Err(forbidden());
    }
    let fail = |msg: String| {
        // Bounce back to the admin page with an error query the UI shows.
        Redirect::to(&format!("/admin?google_error={}", urlencode_q(&msg)))
    };

    if let Some(err) = q.error {
        return Ok(fail(format!("google denied: {err}")));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Ok(fail("missing code/state".into()));
    };

    // Consume + validate the CSRF state.
    let pending = match s.ctx.db.take_pending(&state).await {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(fail("unknown or expired state".into())),
        Err(e) => return Ok(fail(format!("state lookup: {e}"))),
    };
    let folder_id = pending.redirect_after.unwrap_or_default();

    let (client_id, client_secret, redirect) = match google_oauth_env() {
        Ok(v) => v,
        Err(_) => return Ok(fail("google oauth env not configured".into())),
    };

    // Exchange the authorization code for a durable refresh token.
    let http = reqwest::Client::new();
    let tokens = match qpedia_connectors::oauth::exchange_code(
        &http,
        &client_id,
        &client_secret,
        &code,
        &redirect,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return Ok(fail(format!("token exchange: {e}"))),
    };
    let Some(refresh_token) = tokens.refresh_token else {
        return Ok(fail(
            "google returned no refresh token (revoke prior grant and retry)".into(),
        ));
    };

    // Record the grant (single source of truth for the refresh token,
    // for audit + future revocation). Org-level: subject = "".
    if let Err(e) = s
        .ctx
        .db
        .upsert_oauth_grant(
            &user.tenant,
            "google",
            "drive.readonly",
            "",
            Some(&tokens.access_token),
            &refresh_token,
            Some(tokens.expires_at),
            &user.id,
        )
        .await
    {
        return Ok(fail(format!("store grant: {e}")));
    }

    // Create a gdrive connector wired with the refresh token. The
    // connector config duplicates the refresh token for now; the grant
    // row remains the audit/revocation record.
    let name = match qpedia_pg_store::unique_connector_name(&s.ctx.db, &user.tenant, "google-drive")
        .await
    {
        Ok(n) => n,
        Err(e) => return Ok(fail(format!("name connector: {e}"))),
    };
    let mut config = json!({
        "client_id": client_id,
        "client_secret": client_secret,
        "refresh_token": refresh_token,
    });
    if !folder_id.is_empty() {
        config["folder_id"] = json!(folder_id);
    }
    let cfg = qpedia_connectors::ConnectorConfig {
        id: String::new(),
        tenant: user.tenant.as_str().to_string(),
        kind: "gdrive".into(),
        name: name.clone(),
        config_json: config,
        cursor: None,
        enabled: true,
        last_run_at: None,
        last_error: None,
    };
    let cid = match s.ctx.db.insert_connector(&user.tenant, &cfg).await {
        Ok(id) => id.to_string(),
        Err(e) => return Ok(fail(format!("create connector: {e}"))),
    };

    let _ = s
        .ctx
        .db
        .write_audit(
            &user.tenant,
            &user.id,
            "connector.google.connected",
            Some(&cid),
            Some(&json!({ "name": name, "folder_id": folder_id })),
        )
        .await;

    info!(tenant = %user.tenant, connector = %name, "google drive connected");
    Ok(Redirect::to("/admin?google_connected=1"))
}

/// Tiny query-value encoder for the redirect error message.
fn urlencode_q(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ============================================================================
// workspaces — membership, org creation, invites (Band 4.1)
// ============================================================================

/// Map a membership role to the session groups it grants *within that
/// workspace*. RLS scopes the power to the tenant, so "admin" here means
/// "admin of this workspace," never cross-tenant.
fn role_to_groups(role: &str) -> Vec<String> {
    match role {
        "owner" | "admin" => vec!["admin".to_string()],
        _ => vec!["member".to_string()],
    }
}

/// Slugify an org name into a tenant id, avoiding the individual (`u-`)
/// and reserved (`default`) namespaces so the workspace-kind heuristic
/// stays correct.
fn org_slug_base(name: &str) -> String {
    let base = qpedia_pg_store::slugify(name);
    if base.starts_with("u-") || base == "default" {
        format!("team-{base}")
    } else {
        base
    }
}

/// List the workspaces the caller belongs to, plus which is active.
pub(crate) async fn list_workspaces(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    let rows = s
        .ctx
        .db
        .list_user_workspaces(&user.id)
        .await
        .map_err(ApiError::Internal)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|w| {
            json!({
                "tenant": w.tenant.as_str(),
                "name": w.name,
                "role": w.role,
                "kind": w.kind,
                "active": w.tenant.as_str() == user.tenant.as_str(),
            })
        })
        .collect();
    Ok(Json(json!({ "workspaces": items, "active": user.tenant.as_str() })))
}

#[derive(Deserialize)]
pub(crate) struct CreateWorkspaceBody {
    name: String,
}

/// Create an org workspace; the caller becomes its owner.
pub(crate) async fn create_workspace(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<CreateWorkspaceBody>,
) -> Result<Json<Value>, ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::Bad("workspace name required".into()));
    }
    // Unique tenant slug.
    let base = org_slug_base(name);
    let mut slug = base.clone();
    let mut n = 2u32;
    loop {
        let t = qpedia_core::tenant::Tenant::new(slug.clone());
        if s.ctx.db.get_tenant(&t).await.map_err(ApiError::Internal)?.is_none() {
            break;
        }
        slug = format!("{base}-{n}");
        n += 1;
        if n > 1000 {
            return Err(ApiError::Internal(anyhow::anyhow!("no free workspace slug")));
        }
    }
    let tenant = qpedia_core::tenant::Tenant::new(slug);
    s.ctx
        .db
        .create_org_workspace(&tenant, name, &user.id, user.email.as_deref())
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(&tenant, &user.id, "workspace.create", Some(tenant.as_str()), Some(&json!({"name": name})))
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "tenant": tenant.as_str(), "name": name, "role": "owner" })))
}

/// Switch the active workspace. Verifies membership, then re-points the
/// session cookie at the target tenant with the role's groups.
pub(crate) async fn switch_workspace(
    State(s): State<AppState>,
    user: User,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let target = qpedia_core::tenant::Tenant::new(id);
    let role = s
        .ctx
        .db
        .membership_role(&target, &user.id)
        .await
        .map_err(ApiError::Internal)?
        .ok_or(ApiError::Forbidden)?; // not a member
    let token = auth::read_session_token(&headers).ok_or(ApiError::Forbidden)?;
    s.ctx
        .db
        .update_session_workspace(&auth::hash_token(&token), &target, &role_to_groups(&role))
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "tenant": target.as_str(), "role": role })))
}

pub(crate) async fn list_workspace_members(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let members = s.ctx.db.list_members(&user.tenant).await.map_err(ApiError::Internal)?;
    let items: Vec<Value> = members
        .into_iter()
        .map(|m| {
            json!({
                "user_id": m.user_id,
                "email": m.email,
                "role": m.role,
                "joined_at": m.created_at.to_rfc3339(),
                "is_you": m.user_id == user.id,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

pub(crate) async fn remove_workspace_member(
    State(s): State<AppState>,
    user: User,
    Path(target_user): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    s.ctx
        .db
        .remove_member(&user.tenant, &target_user)
        .await
        .map_err(|e| ApiError::Bad(e.to_string()))?;
    s.ctx
        .db
        .write_audit(&user.tenant, &user.id, "workspace.member.remove", Some(&target_user), None)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "removed": target_user })))
}

const INVITE_TTL_SECS: i64 = 14 * 24 * 60 * 60; // 14 days

#[derive(Deserialize)]
pub(crate) struct CreateInviteBody {
    email: String,
    #[serde(default)]
    role: Option<String>,
}

pub(crate) async fn create_workspace_invite(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<CreateInviteBody>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let email = body.email.trim().to_ascii_lowercase();
    if !email.contains('@') {
        return Err(ApiError::Bad("valid email required".into()));
    }
    let role = match body.role.as_deref() {
        Some("admin") => "admin",
        _ => "member",
    };
    // Individual workspaces can't have other members — invites are an
    // org concept.
    if user.tenant.as_str().starts_with(INDIVIDUAL_TENANT_PREFIX) {
        return Err(ApiError::Bad(
            "create an organization workspace before inviting people".into(),
        ));
    }
    let token = auth::random_token(24);
    let id = s
        .ctx
        .db
        .create_invite(&user.tenant, &email, role, &token, &user.id, INVITE_TTL_SECS)
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(&user.tenant, &user.id, "workspace.invite.create", Some(&email), Some(&json!({"role": role})))
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "id": id,
        "email": email,
        "role": role,
        // The accept link the inviter shares. (Email delivery is a
        // follow-up; for now the admin copies this.)
        "invite_path": format!("/invite/{token}"),
        "token": token,
    })))
}

pub(crate) async fn list_workspace_invites(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let invites = s.ctx.db.list_invites(&user.tenant).await.map_err(ApiError::Internal)?;
    let items: Vec<Value> = invites
        .into_iter()
        .map(|i| {
            json!({
                "id": i.id,
                "email": i.email,
                "role": i.role,
                "expires_at": i.expires_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

pub(crate) async fn delete_workspace_invite(
    State(s): State<AppState>,
    user: User,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    s.ctx.db.delete_invite(&user.tenant, id).await.map_err(ApiError::Internal)?;
    Ok(Json(json!({ "deleted": id })))
}

/// Preview an invite by token (for the accept page). Requires login so
/// we can show whether the invitee's email matches.
pub(crate) async fn get_invite(
    State(s): State<AppState>,
    _user: User,
    Path(token): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let invite = s
        .ctx
        .db
        .get_invite_by_token(&token)
        .await
        .map_err(ApiError::Internal)?
        .ok_or(ApiError::NotFound)?;
    let name = s
        .ctx
        .db
        .get_tenant(&invite.tenant)
        .await
        .ok()
        .flatten()
        .map(|t| t.display_name)
        .unwrap_or_else(|| invite.tenant.as_str().to_string());
    let valid = invite.accepted_at.is_none() && invite.expires_at > chrono::Utc::now();
    Ok(Json(json!({
        "workspace": name,
        "tenant": invite.tenant.as_str(),
        "role": invite.role,
        "email": invite.email,
        "valid": valid,
    })))
}

/// Accept an invite and switch the session into the joined workspace.
pub(crate) async fn accept_invite(
    State(s): State<AppState>,
    user: User,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let tenant = s
        .ctx
        .db
        .accept_invite(&token, &user.id, user.email.as_deref())
        .await
        .map_err(|e| ApiError::Bad(e.to_string()))?;
    let role = s
        .ctx
        .db
        .membership_role(&tenant, &user.id)
        .await
        .map_err(ApiError::Internal)?
        .unwrap_or_else(|| "member".to_string());
    // Switch the session into the workspace they just joined.
    if let Some(tok) = auth::read_session_token(&headers) {
        let _ = s
            .ctx
            .db
            .update_session_workspace(&auth::hash_token(&tok), &tenant, &role_to_groups(&role))
            .await;
    }
    s.ctx
        .db
        .write_audit(&tenant, &user.id, "workspace.invite.accept", Some(tenant.as_str()), None)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "tenant": tenant.as_str(), "role": role })))
}

// ---------- domain verification (DNS-TXT method; Band 4.2) ----------

const DOMAIN_TXT_PREFIX: &str = "qpedia-verify=";

/// Normalize user-entered domain input to a bare hostname.
fn normalize_domain(input: &str) -> String {
    let mut d = input.trim().to_ascii_lowercase();
    for p in ["https://", "http://"] {
        if let Some(rest) = d.strip_prefix(p) {
            d = rest.to_string();
        }
    }
    d = d.split('/').next().unwrap_or(&d).to_string();
    d = d.trim_start_matches("www.").trim_end_matches('.').to_string();
    d
}

/// Resolve a domain's TXT records via DNS-over-HTTPS (Google's JSON API),
/// so it works in any container without special resolver config and
/// without a heavyweight DNS crate.
async fn txt_records(domain: &str) -> Result<Vec<String>, ApiError> {
    let url = format!("https://dns.google/resolve?name={}&type=TXT", urlencode_q(domain));
    let resp = reqwest::Client::new()
        .get(&url)
        .header("accept", "application/dns-json")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| ApiError::Internal(e.into()))?;
    let v: Value = resp.json().await.map_err(|e| ApiError::Internal(e.into()))?;
    Ok(parse_txt_answer(&v))
}

/// Extract TXT strings (type 16) from a DoH JSON response, stripping the
/// quoting. Pure, so it's unit-testable without the network.
fn parse_txt_answer(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(ans) = v.get("Answer").and_then(|a| a.as_array()) {
        for a in ans {
            if a.get("type").and_then(|t| t.as_i64()) == Some(16) {
                if let Some(data) = a.get("data").and_then(|d| d.as_str()) {
                    out.push(data.replace('"', ""));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod domain_tests {
    use super::{normalize_domain, parse_txt_answer};
    use serde_json::json;

    #[test]
    fn normalizes_domain_input() {
        assert_eq!(normalize_domain("  https://WWW.Acme.com/foo "), "acme.com");
        assert_eq!(normalize_domain("acme.com."), "acme.com");
        assert_eq!(normalize_domain("http://sub.acme.co.uk"), "sub.acme.co.uk");
    }

    #[test]
    fn parses_txt_answer() {
        let v = json!({
            "Answer": [
                { "type": 16, "data": "\"qpedia-verify=abc123\"" },
                { "type": 16, "data": "\"v=spf1 include:_spf.google.com ~all\"" },
                { "type": 1,  "data": "1.2.3.4" }
            ]
        });
        let txt = parse_txt_answer(&v);
        assert!(txt.contains(&"qpedia-verify=abc123".to_string()));
        assert_eq!(txt.len(), 2, "only TXT (type 16) records");
    }

    #[test]
    fn empty_answer() {
        assert!(parse_txt_answer(&json!({})).is_empty());
    }
}

pub(crate) async fn list_domains(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let rows = s
        .ctx
        .db
        .list_workspace_domains(&user.tenant)
        .await
        .map_err(ApiError::Internal)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|d| {
            json!({
                "domain": d.domain,
                "verified": d.verified,
                "verified_via": d.verified_via,
                "verified_at": d.verified_at.map(|t| t.to_rfc3339()),
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub(crate) struct AddDomainBody {
    domain: String,
}

/// Claim a domain (unverified) and return the DNS TXT record to add.
pub(crate) async fn add_domain(
    State(s): State<AppState>,
    user: User,
    Json(body): Json<AddDomainBody>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    if user.tenant.as_str().starts_with(INDIVIDUAL_TENANT_PREFIX) {
        return Err(ApiError::Bad(
            "create an organization workspace before adding a domain".into(),
        ));
    }
    let domain = normalize_domain(&body.domain);
    if !domain.contains('.') || domain.contains(' ') {
        return Err(ApiError::Bad("enter a valid domain, e.g. acme.com".into()));
    }
    let token = auth::random_token(16);
    s.ctx
        .db
        .claim_domain(&user.tenant, &domain, &user.id, &token)
        .await
        .map_err(ApiError::Internal)?;
    s.ctx
        .db
        .write_audit(&user.tenant, &user.id, "workspace.domain.claim", Some(&domain), None)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "domain": domain,
        "verified": false,
        // The record the admin adds to their DNS: a TXT on the apex with
        // this exact value (alongside any existing TXT records).
        "txt_name": domain,
        "txt_value": format!("{DOMAIN_TXT_PREFIX}{token}"),
    })))
}

/// Check the DNS TXT record and, if present, mark the domain verified.
pub(crate) async fn verify_domain(
    State(s): State<AppState>,
    user: User,
    Path(domain): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let domain = normalize_domain(&domain);
    let token = s
        .ctx
        .db
        .domain_verification_token(&user.tenant, &domain)
        .await
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::Bad("domain not claimed in this workspace".into()))?;
    let expected = format!("{DOMAIN_TXT_PREFIX}{token}");
    let records = txt_records(&domain).await?;
    if !records.iter().any(|r| r == &expected) {
        return Err(ApiError::Bad(format!(
            "TXT record not found yet. Add a TXT record on {domain} with value \"{expected}\", then retry (DNS can take a few minutes)."
        )));
    }
    s.ctx
        .db
        .verify_domain(&user.tenant, &domain, "dns")
        .await
        .map_err(|e| ApiError::Bad(e.to_string()))?; // e.g. already verified elsewhere
    s.ctx
        .db
        .write_audit(&user.tenant, &user.id, "workspace.domain.verify", Some(&domain), Some(&json!({"via": "dns"})))
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({ "domain": domain, "verified": true, "verified_via": "dns" })))
}

pub(crate) async fn delete_domain(
    State(s): State<AppState>,
    user: User,
    Path(domain): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let domain = normalize_domain(&domain);
    s.ctx.db.delete_domain(&user.tenant, &domain).await.map_err(ApiError::Internal)?;
    Ok(Json(json!({ "deleted": domain })))
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

/// Live job-queue snapshot for the Admin "Processing queue" view: counts by
/// state, the active jobs (running processors first), and recent failures.
pub(crate) async fn queue_overview(
    State(s): State<AppState>,
    user: User,
) -> Result<Json<Value>, ApiError> {
    if !user.is_admin() {
        return Err(ApiError::Forbidden);
    }
    let overview = s
        .ctx
        .db
        .queue_overview(&user.tenant)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(overview))
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
    /// 429 with `Retry-After` header. The carried `u64` is seconds.
    TooManyRequests(u64),
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
        // 429 needs a `Retry-After` header in addition to the JSON body,
        // so it's threaded separately from the simple (status, msg) path.
        if let ApiError::TooManyRequests(secs) = self {
            let mut resp = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": format!("rate limit exceeded; retry after {secs}s"),
                    "retry_after_seconds": secs,
                })),
            )
                .into_response();
            if let Ok(v) = secs.to_string().parse() {
                resp.headers_mut().insert("retry-after", v);
            }
            return resp;
        }
        let (status, msg) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Bad(s) => (StatusCode::BAD_REQUEST, s),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".into()),
            ApiError::Internal(e) => {
                error!(error = %e, "internal");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
            ApiError::TooManyRequests(_) => unreachable!("handled above"),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}
