//! Sources CRUD. Public identifier is the slug (`SourceId` carries it);
//! the BIGSERIAL `id` column is internal-only and never escapes this
//! module. All queries are RLS-scoped via `begin_for(&tenant)`.

use crate::{with_db_span, DbSystem, PgStore};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use qpedia_core::{
    acl::Acl,
    source::{Source, SourceStatus},
    tenant::Tenant,
    SourceId,
};
use sqlx::Row;
use std::collections::{BTreeSet, HashMap};

impl PgStore {
    pub async fn insert_source(&self, src: &Source) -> Result<()> {
        with_db_span(DbSystem::Postgresql, "insert_source", async move {
            let mut tx = self.begin_for(&src.tenant).await?;
            anyhow::ensure!(Self::insert_source_in(&mut tx, src, None).await?, "source slug already exists");
            tx.commit().await.context("commit insert source")
        }).await
    }

    /// The caller commits only after the original has been written. A concurrent
    /// retry waits on the unique (tenant, slug) and cannot enqueue a second ingest.
    pub async fn stage_upload(
        &self, src: &Source, filing: Option<(&str, i32)>, job: &qpedia_core::job::Job,
    ) -> Result<Option<sqlx::Transaction<'_, sqlx::Postgres>>> {
        let mut tx = self.begin_for(&src.tenant).await?;
        if !Self::insert_source_in(&mut tx, src, filing).await? {
            tx.rollback().await?;
            return Ok(None);
        }
        Self::enqueue_in(&mut tx, &src.tenant, job).await?;
        Ok(Some(tx))
    }

    async fn insert_source_in(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, src: &Source, filing: Option<(&str, i32)>,
    ) -> Result<bool> {
        let status = serde_json::to_string(&src.status)?;
        let acl: Vec<String> = src.acl.0.iter().cloned().collect();
        let result = sqlx::query(
                "INSERT INTO sources \
                 (tenant_id, slug, folder_path, filename, mime, sha256, size_bytes, \
                  acl, status, language, classification, created_at, ingested_at, filing_name, filing_revision) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)
                 ON CONFLICT (tenant_id,slug) DO NOTHING",
            )
            .bind(src.tenant.as_str())
            .bind(src.id.as_str())
            .bind(&src.folder_path)
            .bind(&src.filename)
            .bind(&src.mime)
            .bind(&src.sha256)
            .bind(src.size_bytes as i64)
            .bind(&acl)
            .bind(status.trim_matches('"'))
            .bind(src.language.as_deref())
            .bind(src.classification.clone())
            .bind(src.created_at)
            .bind(src.ingested_at)
            .bind(filing.map(|(name, _)| name))
            .bind(filing.map_or(0, |(_, revision)| revision))
            .execute(&mut **tx)
            .await
            .context("insert source")?;

        Ok(result.rows_affected() == 1)
    }

    pub async fn get_source_in(
        &self,
        tenant: &Tenant,
        slug: &SourceId,
    ) -> Result<Option<Source>> {
        with_db_span(DbSystem::Postgresql, "get_source", async move {
            let mut tx = self.begin_for(tenant).await?;
            let row = sqlx::query(
                "SELECT slug, tenant_id, folder_path, filename, mime, sha256, size_bytes, \
                        acl, status, language, classification, created_at, ingested_at \
                 FROM sources WHERE slug = $1",
            )
            .bind(slug.as_str())
            .fetch_optional(&mut *tx)
            .await
            .context("get_source_in")?;
            tx.commit().await.ok();
            row.map(row_to_source).transpose()
        })
        .await
    }

    pub async fn list_sources(
        &self,
        tenant: &Tenant,
        folder_prefix: &str,
        limit: i64,
    ) -> Result<Vec<Source>> {
        with_db_span(DbSystem::Postgresql, "list_sources", async move {
            let mut tx = self.begin_for(tenant).await?;
            let rows = sqlx::query(
                "SELECT slug, tenant_id, folder_path, filename, mime, sha256, size_bytes, \
                        acl, status, language, classification, created_at, ingested_at \
                 FROM sources \
                 WHERE starts_with(folder_path, $1) \
                 ORDER BY created_at DESC \
                 LIMIT $2",
            )
            .bind(folder_prefix)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .context("list_sources")?;
            tx.commit().await.ok();
            rows.into_iter().map(row_to_source).collect()
        })
        .await
    }

    pub async fn update_status(
        &self,
        tenant: &Tenant,
        slug: &SourceId,
        status: SourceStatus,
    ) -> Result<()> {
        with_db_span(DbSystem::Postgresql, "update_source_status", async move {
            let mut tx = self.begin_for(tenant).await?;
            let s = serde_json::to_string(&status)?;
            let s = s.trim_matches('"').to_string();
            sqlx::query("UPDATE sources SET status = $1 WHERE slug = $2")
                .bind(s)
                .bind(slug.as_str())
                .execute(&mut *tx)
                .await
                .context("update_status")?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn update_classification(
        &self,
        tenant: &Tenant,
        slug: &SourceId,
        classification: &serde_json::Value,
    ) -> Result<()> {
        with_db_span(DbSystem::Postgresql, "update_source_classification", async move {
            let mut tx = self.begin_for(tenant).await?;
            sqlx::query("UPDATE sources SET classification = $1 WHERE slug = $2")
                .bind(classification)
                .bind(slug.as_str())
                .execute(&mut *tx)
                .await
                .context("update_classification")?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn update_language(
        &self,
        tenant: &Tenant,
        slug: &SourceId,
        language: &str,
    ) -> Result<()> {
        with_db_span(DbSystem::Postgresql, "update_source_language", async move {
            let mut tx = self.begin_for(tenant).await?;
            sqlx::query("UPDATE sources SET language = $1 WHERE slug = $2")
                .bind(language)
                .bind(slug.as_str())
                .execute(&mut *tx)
                .await
                .context("update_language")?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn update_folder_path(
        &self,
        tenant: &Tenant,
        slug: &SourceId,
        folder_path: &str,
    ) -> Result<bool> {
        with_db_span(DbSystem::Postgresql, "update_source_folder_path", async move {
            let mut tx = self.begin_for(tenant).await?;
            let result = sqlx::query("UPDATE sources SET folder_path = $1 WHERE slug = $2 AND filing_revision=0")
                .bind(folder_path)
                .bind(slug.as_str())
                .execute(&mut *tx)
                .await
                .context("update_folder_path")?;
            tx.commit().await?;
            Ok(result.rows_affected() == 1)
        })
        .await
    }

    /// An older in-flight move cannot overwrite the newer filing. Equal revisions
    /// acknowledge a retry only when identity and the full key match.
    pub async fn apply_source_filing(
        &self, tenant: &Tenant, slug: &SourceId, digest: &str, folder: &str, name: &str, revision: i32,
    ) -> Result<bool> {
        let mut tx = self.begin_for(tenant).await?;
        let result = sqlx::query("UPDATE sources SET folder_path=$3, filing_name=$4, filing_revision=$5
            WHERE slug=$1 AND sha256=$2 AND (filing_revision<$5 OR
            (filing_revision=$5 AND folder_path=$3 AND filing_name=$4))")
            .bind(slug.as_str()).bind(digest).bind(folder).bind(name).bind(revision)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn source_filings(&self, tenant: &Tenant, groups: &[String], admin: bool) -> Result<HashMap<String, String>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT slug, rtrim(folder_path, '/') || '/' || filing_name FROM sources
             WHERE filing_revision>0 AND ($1 OR acl && $2::text[])")
            .bind(admin).bind(groups).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows.into_iter().collect())
    }

    /// Reset a source row for a replace-in-place re-upload. The public
    /// slug, folder_path, ACL, and created_at are preserved; the file
    /// identity (filename / mime / sha256 / size) is overwritten and
    /// the classification / language / ingested_at are cleared so the
    /// pipeline can re-run from `Pending` against the new bytes.
    pub async fn replace_source_blob(
        &self,
        tenant: &Tenant,
        slug: &SourceId,
        filename: &str,
        mime: &str,
        sha256: &str,
        size_bytes: u64,
    ) -> Result<()> {
        with_db_span(DbSystem::Postgresql, "replace_source_blob", async move {
            let mut tx = self.begin_for(tenant).await?;
            sqlx::query(
                "UPDATE sources SET \
                     filename       = $1, \
                     mime           = $2, \
                     sha256         = $3, \
                     size_bytes     = $4, \
                     status         = 'pending', \
                     classification = NULL, \
                     language       = NULL, \
                     ingested_at    = NULL \
                 WHERE slug = $5",
            )
            .bind(filename)
            .bind(mime)
            .bind(sha256)
            .bind(size_bytes as i64)
            .bind(slug.as_str())
            .execute(&mut *tx)
            .await
            .context("replace_source_blob")?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn delete_source(&self, tenant: &Tenant, slug: &SourceId) -> Result<()> {
        with_db_span(DbSystem::Postgresql, "delete_source", async move {
            let mut tx = self.begin_for(tenant).await?;
            sqlx::query("DELETE FROM sources WHERE slug = $1")
                .bind(slug.as_str())
                .execute(&mut *tx)
                .await
                .context("delete_source")?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    /// Sources still sitting at folder_path = "/" but with a classification —
    /// candidates for the lint pass's auto-organize sweep.
    pub async fn list_unorganized(
        &self,
        tenant: &Tenant,
        limit: i64,
    ) -> Result<Vec<Source>> {
        with_db_span(DbSystem::Postgresql, "list_unorganized_sources", async move {
            let mut tx = self.begin_for(tenant).await?;
            let rows = sqlx::query(
                "SELECT slug, tenant_id, folder_path, filename, mime, sha256, size_bytes, \
                        acl, status, language, classification, created_at, ingested_at \
                 FROM sources \
                 WHERE folder_path = '/' AND filing_revision=0 AND classification IS NOT NULL \
                 ORDER BY created_at DESC \
                 LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .context("list_unorganized")?;
            tx.commit().await.ok();
            rows.into_iter().map(row_to_source).collect()
        })
        .await
    }

    /// Sources stuck in mid-pipeline states for too long — admin "resume"
    /// surfaces these so an operator can re-enqueue them.
    pub async fn list_stalled(
        &self,
        tenant: &Tenant,
        older_than_secs: i64,
        limit: i64,
    ) -> Result<Vec<Source>> {
        with_db_span(DbSystem::Postgresql, "list_stalled_sources", async move {
            let mut tx = self.begin_for(tenant).await?;
            let cutoff = chrono::Utc::now() - chrono::Duration::seconds(older_than_secs);
            let rows = sqlx::query(
                "SELECT slug, tenant_id, folder_path, filename, mime, sha256, size_bytes, \
                        acl, status, language, classification, created_at, ingested_at \
                 FROM sources \
                 WHERE status NOT IN ('done','tainted','failed') AND created_at < $1 \
                 ORDER BY created_at ASC \
                 LIMIT $2",
            )
            .bind(cutoff)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .context("list_stalled")?;
            tx.commit().await.ok();
            rows.into_iter().map(row_to_source).collect()
        })
        .await
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
    let slug: String = row.try_get("slug").context("slug")?;

    Ok(Source {
        id: SourceId::from(slug),
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
