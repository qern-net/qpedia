//! Folder nodes for the File Explorer tree. See migrations/0004_folders.sql.
//!
//! A folder node may be explicit (a row here, e.g. from `+ new folder`) or
//! implicit (derived from a `sources.folder_path`). [`list_folders`] returns
//! the union so the UI can render empty user-made folders alongside ones that
//! only exist because they contain files. `pinned` is TRUE only for explicit
//! rows the user locked; derived folders are never pinned.

use crate::{slug::slugify_folder, PgStore};
use anyhow::{Context, Result};
use qpedia_core::tenant::Tenant;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct FolderRow {
    pub path: String,
    pub pinned: bool,
}

impl PgStore {
    /// All folder nodes for a tenant: explicit rows unioned with the distinct
    /// non-root folder_paths of existing sources. `pinned` is OR-ed so an
    /// explicit pinned row wins over a derived (unpinned) duplicate.
    pub async fn list_folders(&self, tenant: &Tenant) -> Result<Vec<FolderRow>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT path, bool_or(pinned) AS pinned FROM ( \
                 SELECT path, pinned FROM folders \
                 UNION ALL \
                 SELECT DISTINCT folder_path AS path, FALSE AS pinned FROM sources \
             ) u \
             WHERE path <> '/' \
             GROUP BY path \
             ORDER BY path",
        )
        .fetch_all(&mut *tx)
        .await
        .context("list_folders")?;
        tx.commit().await.ok();
        Ok(rows
            .into_iter()
            .map(|r| FolderRow {
                path: r.get("path"),
                pinned: r.get::<bool, _>("pinned"),
            })
            .collect())
    }

    /// Create or re-pin a folder. Slugifies the path. Idempotent.
    pub async fn create_folder(
        &self,
        tenant: &Tenant,
        path: &str,
        pinned: bool,
        created_by: &str,
    ) -> Result<String> {
        let path = slugify_folder(path);
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO folders (tenant_id, path, pinned, created_by) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant_id, path) DO UPDATE SET pinned = EXCLUDED.pinned",
        )
        .bind(tenant.as_str())
        .bind(&path)
        .bind(pinned)
        .bind(created_by)
        .execute(&mut *tx)
        .await
        .context("create_folder")?;
        tx.commit().await?;
        Ok(path)
    }

    /// Toggle a folder's pinned flag. Inserts a row if the folder was only
    /// implicit (so unlocking/locking a derived folder is well-defined).
    pub async fn set_folder_pinned(
        &self,
        tenant: &Tenant,
        path: &str,
        pinned: bool,
        actor: &str,
    ) -> Result<()> {
        let path = slugify_folder(path);
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO folders (tenant_id, path, pinned, created_by) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant_id, path) DO UPDATE SET pinned = EXCLUDED.pinned",
        )
        .bind(tenant.as_str())
        .bind(&path)
        .bind(pinned)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .context("set_folder_pinned")?;
        tx.commit().await?;
        Ok(())
    }

    /// True if the folder is explicitly pinned. Used by the auto-organizer
    /// to avoid dumping AI files into a user-locked folder.
    pub async fn is_folder_pinned(&self, tenant: &Tenant, path: &str) -> Result<bool> {
        let path = slugify_folder(path);
        let mut tx = self.begin_for(tenant).await?;
        let row: Option<bool> =
            sqlx::query_scalar("SELECT pinned FROM folders WHERE path = $1")
                .bind(&path)
                .fetch_optional(&mut *tx)
                .await
                .context("is_folder_pinned")?;
        tx.commit().await.ok();
        Ok(row.unwrap_or(false))
    }

    /// Count sources at `path` or any descendant — used to gate deletion.
    pub async fn folder_source_count(&self, tenant: &Tenant, path: &str) -> Result<i64> {
        let path = slugify_folder(path);
        let descendant = format!("{}/%", path.trim_end_matches('/'));
        let mut tx = self.begin_for(tenant).await?;
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sources WHERE folder_path = $1 OR folder_path LIKE $2",
        )
        .bind(&path)
        .bind(&descendant)
        .fetch_one(&mut *tx)
        .await
        .context("folder_source_count")?;
        tx.commit().await.ok();
        Ok(n)
    }

    /// Delete an explicit folder row. Caller must ensure it is empty
    /// (see [`folder_source_count`]). Removing the row reverts the node to
    /// implicit — it disappears from the tree once it has no files.
    pub async fn delete_folder(&self, tenant: &Tenant, path: &str) -> Result<()> {
        let path = slugify_folder(path);
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM folders WHERE path = $1")
            .bind(&path)
            .execute(&mut *tx)
            .await
            .context("delete_folder")?;
        tx.commit().await?;
        Ok(())
    }
}
