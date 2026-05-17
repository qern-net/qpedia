//! Folder ACL CRUD + ancestor-walk resolution.

use crate::{slug::slugify_folder, PgStore};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use qpedia_core::{acl::Acl, tenant::Tenant};
use sqlx::Row;
use std::collections::BTreeSet;

impl PgStore {
    pub async fn list_folder_acls(
        &self,
        tenant: &Tenant,
    ) -> Result<Vec<(String, Acl, DateTime<Utc>, String)>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT folder_path, acl, updated_at, updated_by FROM folder_acls \
             ORDER BY folder_path",
        )
        .fetch_all(&mut *tx)
        .await
        .context("list_folder_acls")?;
        tx.commit().await.ok();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let path: String = row.try_get("folder_path")?;
            let acl: Vec<String> = row.try_get("acl")?;
            let updated_at: DateTime<Utc> = row.try_get("updated_at")?;
            let updated_by: String = row.try_get("updated_by")?;
            out.push((path, Acl(BTreeSet::from_iter(acl)), updated_at, updated_by));
        }
        Ok(out)
    }

    pub async fn set_folder_acl(
        &self,
        tenant: &Tenant,
        folder_path: &str,
        acl: &Acl,
        updated_by: &str,
    ) -> Result<()> {
        let path = slugify_folder(folder_path);
        let acl_vec: Vec<String> = acl.0.iter().cloned().collect();
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO folder_acls (tenant_id, folder_path, acl, updated_at, updated_by) \
             VALUES ($1, $2, $3, now(), $4) \
             ON CONFLICT (tenant_id, folder_path) DO UPDATE \
               SET acl        = EXCLUDED.acl, \
                   updated_at = now(), \
                   updated_by = EXCLUDED.updated_by",
        )
        .bind(tenant.as_str())
        .bind(&path)
        .bind(&acl_vec)
        .bind(updated_by)
        .execute(&mut *tx)
        .await
        .context("set_folder_acl")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_folder_acl(&self, tenant: &Tenant, folder_path: &str) -> Result<()> {
        let path = slugify_folder(folder_path);
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM folder_acls WHERE folder_path = $1")
            .bind(&path)
            .execute(&mut *tx)
            .await
            .context("delete_folder_acl")?;
        tx.commit().await?;
        Ok(())
    }

    /// Closest-ancestor lookup. Builds the chain `[/foo/bar, /foo, /]`
    /// and returns the longest matching row's ACL, or None.
    pub async fn resolve_folder_acl(&self, tenant: &Tenant, folder_path: &str) -> Result<Option<Acl>> {
        let normalized = slugify_folder(folder_path);
        let mut candidates = vec![normalized.clone()];
        let mut cur = normalized.as_str();
        while let Some(idx) = cur.rfind('/') {
            let parent = if idx == 0 { "/".to_string() } else { cur[..idx].to_string() };
            if !candidates.contains(&parent) {
                candidates.push(parent.clone());
            }
            if idx == 0 { break; }
            cur = &cur[..idx];
        }
        if !candidates.contains(&"/".to_string()) {
            candidates.push("/".into());
        }
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT acl FROM folder_acls \
             WHERE folder_path = ANY($1) \
             ORDER BY length(folder_path) DESC LIMIT 1",
        )
        .bind(&candidates)
        .fetch_optional(&mut *tx)
        .await
        .context("resolve_folder_acl")?;
        tx.commit().await.ok();
        let Some(row) = row else { return Ok(None) };
        let acl: Vec<String> = row.try_get("acl")?;
        Ok(Some(Acl(BTreeSet::from_iter(acl))))
    }
}
