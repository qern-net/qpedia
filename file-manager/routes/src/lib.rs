//! Folder views and ZIP export for hosts embedding the QPedia 1.4 engine.
use axum::{
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use qpedia_api::{ApiError, AppState, User};
use qpedia_core::SourceId;
use qpedia_file_archive::{check_component, Archive, MAX_BYTES, MAX_ENTRIES};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/files", get(list))
        .route("/api/v1/files/archive", get(archive))
}

#[derive(Deserialize)]
struct Selection {
    folder: String,
    #[serde(default)]
    page: u32,
}

fn folder(path: &str) -> Result<&str, ApiError> {
    if path == "/" {
        return Ok(path);
    }
    let parts = path
        .strip_prefix('/')
        .ok_or_else(|| ApiError::Bad("Invalid folder".into()))?;
    if path.len() > 768 || parts.split('/').any(|p| check_component(p).is_err()) {
        return Err(ApiError::Bad("Invalid folder".into()));
    }
    Ok(path)
}

fn child_prefix(path: &str) -> String {
    format!("{}/", path.trim_end_matches('/'))
}
fn folder_item(parent: &str, name: &str) -> Value {
    json!({"id":format!("{}{name}", child_prefix(parent)), "name":name})
}
fn db_error(e: sqlx::Error) -> ApiError {
    ApiError::Internal(e.into())
}

// Catalog filtering, folder boundaries, readiness and pagination are shared with
// the Node adapter. The host supplies the authorization boundary and canonical fields.
const SELECT_FILES: &str = concat!(
    "WITH authorized_files AS (
      SELECT slug, folder_path, filename, sha256, size_bytes, status,
      jsonb_array_length(prior_versions)+1 AS version,
      rtrim(folder_path,'/') || '/' AS parent_folder, true AS downloadable,
      filename AS sort_key, slug AS item_key FROM sources WHERE $2 OR acl && $3::text[]
    ), file_scope AS (SELECT $1::text AS folder, $4::boolean AS recursive, false AS ready_only,
      $5::bigint AS take_count, $6::bigint AS skip_count) ",
    include_str!("../../select-files.sql")
);
async fn list(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<Selection>,
) -> Result<Json<Value>, ApiError> {
    let path = folder(&q.folder)?;
    let mut tx = s
        .ctx
        .db
        .begin_for(&user.tenant)
        .await
        .map_err(ApiError::Internal)?;
    let rows = sqlx::query(SELECT_FILES)
        .bind(child_prefix(path))
        .bind(user.is_admin())
        .bind(&user.groups)
        .bind(false)
        .bind(51_i64)
        .bind(i64::from(q.page) * 50)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_error)?;
    // Only explicit folders and folders containing a visible file participate in navigation.
    let children: Vec<String> = sqlx::query_scalar("SELECT DISTINCT split_part(substr(path,length($1)+1),'/',1) AS child
        FROM (SELECT path FROM folders WHERE pinned UNION SELECT folder_path FROM sources WHERE $2 OR acl && $3::text[]) visible
        WHERE starts_with(path,$1) AND length(path)>length($1) ORDER BY child")
        .bind(child_prefix(path)).bind(user.is_admin()).bind(&user.groups).fetch_all(&mut *tx).await.map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    let files: Vec<_> = rows.iter().take(50).map(|r| json!({
        "id":r.get::<String,_>("slug"), "name":r.get::<String,_>("filename"),
        "size":r.get::<i64,_>("size_bytes"), "status":r.get::<String,_>("status"), "version":r.get::<i32,_>("version")
    })).collect();
    let folders: Vec<_> = children.iter().map(|n| folder_item(path, n)).collect();
    let mut parent = String::new();
    let ancestors: Vec<_> = path
        .split('/')
        .filter(|n| !n.is_empty())
        .map(|n| {
            let item = folder_item(&parent, n);
            parent.push('/');
            parent.push_str(n);
            item
        })
        .collect();
    Ok(Json(
        json!({"files":files,"folders":folders,"ancestors":ancestors,"hasMore":rows.len()>50}),
    ))
}

async fn archive(
    State(s): State<AppState>,
    user: User,
    Query(q): Query<Selection>,
) -> Result<Response, ApiError> {
    let path = folder(&q.folder)?;
    let prefix = child_prefix(path);
    let mut tx = s
        .ctx
        .db
        .begin_for(&user.tenant)
        .await
        .map_err(ApiError::Internal)?;
    let rows = sqlx::query(SELECT_FILES)
        .bind(&prefix)
        .bind(user.is_admin())
        .bind(&user.groups)
        .bind(true)
        .bind(MAX_ENTRIES as i64 + 1)
        .bind(0_i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    let size: Option<u64> = rows.iter().try_fold(0u64, |n, r| {
        n.checked_add(u64::try_from(r.get::<i64, _>("size_bytes")).ok()?)
    });
    if rows.len() > MAX_ENTRIES || size.unwrap_or(u64::MAX) > MAX_BYTES as u64 {
        return Err(ApiError::Bad(
            "Folder exceeds the ZIP limit. Download a smaller folder.".into(),
        ));
    }
    let mut writer = Archive::default();
    for row in rows {
        let sid = SourceId::from(row.get::<String, _>("slug"));
        let filename: String = row.get("filename");
        check_component(&filename).map_err(|e| ApiError::Bad(e.to_string()))?;
        let original_folder: String = row.get("folder_path");
        let sha: String = row.get("sha256");
        let current = s
            .ctx
            .db
            .get_source_in(&user.tenant, &sid)
            .await
            .map_err(ApiError::Internal)?
            .ok_or(ApiError::NotFound)?;
        if !user.can_read(&current.acl) || current.folder_path != original_folder {
            return Err(ApiError::NotFound);
        }
        let bytes = s
            .ctx
            .blob
            .original(&user.tenant, &sid, &filename, &sha)
            .await?;
        let relative = original_folder.strip_prefix(&prefix).unwrap_or("");
        let entry = if relative.is_empty() {
            filename
        } else {
            format!("{relative}/{filename}")
        };
        writer
            .add(&entry, &bytes)
            .map_err(|e| ApiError::Bad(e.to_string()))?;
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=files.zip",
            ),
            (header::CACHE_CONTROL, "private, no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        writer.finish()?,
    )
        .into_response())
}
