//! Sources CRUD via RLS-scoped transactions.

use crate::PgStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use qpedia_core::{
    acl::Acl,
    source::{Source, SourceStatus},
    tenant::Tenant,
    SourceId,
};
use sqlx::Row;
use std::collections::BTreeSet;

impl PgStore {
    pub async fn pg_insert_source(&self, src: &Source) -> Result<()> {
        let mut tx = self.begin_for(&src.tenant).await?;
        let status = serde_json::to_string(&src.status)?;
        let status = status.trim_matches('"').to_string();
        let acl: Vec<String> = src.acl.0.iter().cloned().collect();

        sqlx::query(
            "INSERT INTO sources \
             (id, tenant_id, folder_path, filename, mime, sha256, size_bytes, \
              acl, status, language, classification, created_at, ingested_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )
        .bind(src.id.as_str())
        .bind(src.tenant.as_str())
        .bind(&src.folder_path)
        .bind(&src.filename)
        .bind(&src.mime)
        .bind(&src.sha256)
        .bind(src.size_bytes as i64)
        .bind(&acl)
        .bind(status)
        .bind(src.language.as_deref())
        .bind(src.classification.clone())
        .bind(src.created_at)
        .bind(src.ingested_at)
        .execute(&mut *tx)
        .await
        .context("insert source")?;

        tx.commit().await.context("commit insert source")?;
        Ok(())
    }

    pub async fn pg_get_source_in(
        &self,
        tenant: &Tenant,
        id: &SourceId,
    ) -> Result<Option<Source>> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, folder_path, filename, mime, sha256, size_bytes, \
                    acl, status, language, classification, created_at, ingested_at \
             FROM sources WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .context("get_source_in")?;
        tx.commit().await.ok();
        row.map(row_to_source).transpose()
    }

    pub async fn pg_list_sources(
        &self,
        tenant: &Tenant,
        folder_prefix: &str,
        limit: i64,
    ) -> Result<Vec<Source>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, folder_path, filename, mime, sha256, size_bytes, \
                    acl, status, language, classification, created_at, ingested_at \
             FROM sources \
             WHERE folder_path LIKE $1 \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(format!("{folder_prefix}%"))
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .context("list_sources")?;
        tx.commit().await.ok();
        rows.into_iter().map(row_to_source).collect()
    }

    pub async fn pg_update_status(
        &self,
        tenant: &Tenant,
        id: &SourceId,
        status: SourceStatus,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        let s = serde_json::to_string(&status)?;
        let s = s.trim_matches('"').to_string();
        sqlx::query("UPDATE sources SET status = $1 WHERE id = $2")
            .bind(s)
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .context("update_status")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn pg_update_classification(
        &self,
        tenant: &Tenant,
        id: &SourceId,
        classification: &serde_json::Value,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("UPDATE sources SET classification = $1 WHERE id = $2")
            .bind(classification)
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .context("update_classification")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn pg_update_language(
        &self,
        tenant: &Tenant,
        id: &SourceId,
        language: &str,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("UPDATE sources SET language = $1 WHERE id = $2")
            .bind(language)
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .context("update_language")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn pg_update_folder_path(
        &self,
        tenant: &Tenant,
        id: &SourceId,
        folder_path: &str,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("UPDATE sources SET folder_path = $1 WHERE id = $2")
            .bind(folder_path)
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .context("update_folder_path")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn pg_delete_source(&self, tenant: &Tenant, id: &SourceId) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM sources WHERE id = $1")
            .bind(id.as_str())
            .execute(&mut *tx)
            .await
            .context("delete_source")?;
        tx.commit().await?;
        Ok(())
    }
}

fn row_to_source(row: sqlx::postgres::PgRow) -> Result<Source> {
    let acl: Vec<String> = row.try_get("acl").context("acl")?;
    let status_s: String = row.try_get("status").context("status")?;
    let status: SourceStatus = serde_json::from_str(&format!("\"{status_s}\""))
        .context("parse status enum")?;
    let classification: Option<serde_json::Value> = row.try_get("classification").ok();
    let created_at: DateTime<Utc> = row.try_get("created_at").context("created_at")?;
    let ingested_at: Option<DateTime<Utc>> = row.try_get("ingested_at").ok();
    let size: i64 = row.try_get("size_bytes").context("size_bytes")?;
    let tenant_id: String = row.try_get("tenant_id").context("tenant_id")?;
    let id: String = row.try_get("id").context("id")?;

    Ok(Source {
        id: SourceId::from(id),
        tenant: Tenant::new(tenant_id),
        folder_path: row.try_get("folder_path").context("folder_path")?,
        filename: row.try_get("filename").context("filename")?,
        mime: row.try_get("mime").context("mime")?,
        sha256: row.try_get("sha256").context("sha256")?,
        size_bytes: size as u64,
        acl: Acl(BTreeSet::from_iter(acl)),
        status,
        language: row.try_get("language").ok(),
        created_at,
        ingested_at,
        classification,
    })
}
