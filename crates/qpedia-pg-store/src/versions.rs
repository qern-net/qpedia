use crate::{sources::row_to_source, PgStore};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use qpedia_core::{job::Job, source::Source, tenant::Tenant, JobId, SourceId};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};

#[derive(Serialize, Deserialize)]
pub struct SourceVersion {
    pub version: u64,
    pub filename: String,
    pub mime: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub uploaded_at: DateTime<Utc>,
}

// Generate the same snapshot for a history read and when archiving the current head.
const HEAD: &str = "jsonb_build_object('version',jsonb_array_length(prior_versions)+1,
    'filename',filename,'mime',mime,'sha256',sha256,'size_bytes',size_bytes,
    'uploaded_at',coalesce(version_uploaded_at,created_at))";

pub struct SourceUpload<'a> {
    tx: Transaction<'a, Postgres>,
    pub source: Source,
    pub created: bool,
    replacement_ready: bool,
}

impl PgStore {
    /// Hold the name/row lock until the immutable blob, metadata and job are ready.
    pub async fn begin_source_upload<'a>(
        &'a self,
        proposed: &Source,
        by_id: Option<&SourceId>,
    ) -> Result<Option<SourceUpload<'a>>> {
        let mut tx = self.begin_for(&proposed.tenant).await?;
        let found = if let Some(id) = by_id {
            sqlx::query("SELECT * FROM sources WHERE slug=$1 AND filing_revision=0 FOR UPDATE")
                .bind(id.as_str())
                .fetch_optional(&mut *tx)
                .await?
        } else {
            // PostgreSQL row locks alone cannot serialize two first uploads.
            let key = serde_json::to_string(&(
                &proposed.tenant,
                &proposed.folder_path,
                &proposed.filename,
            ))?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                .bind(key)
                .execute(&mut *tx)
                .await?;
            let acl: Vec<_> = proposed.acl.0.iter().cloned().collect();
            sqlx::query(
                "SELECT * FROM sources WHERE filing_revision=0
                AND md5(folder_path)=md5($1) AND folder_path=$1
                AND md5(filename)=md5($2) AND filename=$2
                AND acl @> $3::text[] AND acl <@ $3::text[]
                ORDER BY created_at,id LIMIT 1 FOR UPDATE",
            )
            .bind(&proposed.folder_path)
            .bind(&proposed.filename)
            .bind(acl)
            .fetch_optional(&mut *tx)
            .await?
        };
        if let Some(row) = found {
            return Ok(Some(SourceUpload {
                tx,
                source: row_to_source(row)?,
                created: false,
                replacement_ready: false,
            }));
        }
        if by_id.is_some() {
            return Ok(None);
        }

        let base = crate::slugify(&proposed.filename);
        let mut source = proposed.clone();
        for suffix in 1..=1000 {
            source.id = SourceId::from(if suffix == 1 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            });
            if Self::insert_source_in(&mut tx, &source, None).await? {
                return Ok(Some(SourceUpload {
                    tx,
                    source,
                    created: true,
                    replacement_ready: false,
                }));
            }
        }
        bail!("no available source slug")
    }

    /// Synchronize with a source upload before the handler reads or writes anything.
    /// Uploads that get the row first publish their job before this check runs;
    /// uploads that get it later see the running claim and cannot replace the head.
    pub async fn start_claimed_source_ingest(
        &self,
        tenant: &Tenant,
        id: &SourceId,
        job_id: &JobId,
        attempt: u32,
        expected_sha: Option<&str>,
    ) -> Result<bool> {
        let mut tx = self.begin_for(tenant).await?;
        let source = sqlx::query_as::<_, (String, bool)>(
            "SELECT sha256, restart_version_ingest FROM sources WHERE slug=$1 FOR UPDATE",
        )
        .bind(id.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((sha, restart)) = source else {
            return Ok(false);
        };
        if expected_sha.is_some_and(|expected| expected != sha) {
            return Ok(false);
        }
        // Read the queue in a new statement, after any uploader holding the source row
        // has committed. This also fences legacy jobs without a hash in their payload.
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM jobs claim
                WHERE claim.id=$1 AND claim.attempt=$2 AND claim.kind='ingest'
                  AND claim.state='running' AND claim.locked_until > clock_timestamp()
                  AND claim.payload->>'source_id'=$3
                  AND NOT EXISTS (
                      SELECT 1 FROM jobs newer
                      WHERE newer.tenant_id=claim.tenant_id AND newer.id>claim.id
                        AND newer.kind='ingest' AND newer.state IN ('queued','running')
                        AND newer.payload->>'source_id'=$3
                        AND newer.payload->>'upload_sha256' IS NOT NULL
                  )
            )",
        )
        .bind(job_id.as_str().parse::<i64>().context("invalid claimed job id")?)
        .bind(attempt as i32)
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if !current {
            return Ok(false);
        }
        if restart {
            sqlx::query(
                "UPDATE sources SET restart_version_ingest=false,status='pending',
                classification=NULL,language=NULL,ingested_at=NULL WHERE slug=$1",
            )
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn source_versions(
        &self,
        tenant: &Tenant,
        id: &SourceId,
    ) -> Result<Option<(Source, Vec<SourceVersion>)>> {
        let mut tx = self.begin_for(tenant).await?;
        let query = format!("SELECT *, prior_versions || jsonb_build_array({HEAD}) AS versions FROM sources WHERE slug=$1");
        let row = sqlx::query(&query)
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        row.map(|row| {
            let versions = serde_json::from_value(row.try_get("versions")?)?;
            Ok((row_to_source(row)?, versions))
        })
        .transpose()
    }
}

impl SourceUpload<'_> {
    /// Check before writing blobs, while still holding the source row lock.
    /// Treat expired but unreclaimed jobs as busy too: a lease can expire
    /// before its worker has stopped. A worker starting after this check must
    /// wait for the source lock and then validate its claim against the new head.
    pub async fn replacement_ready(&mut self) -> Result<bool> {
        if self.created || self.replacement_ready {
            return Ok(true);
        }
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM jobs WHERE tenant_id=$1 AND kind='ingest' AND state='running'
                  AND payload->>'source_id'=$2
            )",
        )
        .bind(self.source.tenant.as_str())
        .bind(self.source.id.as_str())
        .fetch_one(&mut *self.tx)
        .await?;
        self.replacement_ready = !busy;
        Ok(self.replacement_ready)
    }

    pub async fn commit(mut self, updated: &Source, job: &Job) -> Result<()> {
        anyhow::ensure!(self.replacement_ready().await?, "source ingestion is in progress");
        if !self.created {
            let query = format!("UPDATE sources SET prior_versions=prior_versions || jsonb_build_array({HEAD}),
                restart_version_ingest=true,version_uploaded_at=now(),filename=$2,mime=$3,sha256=$4,size_bytes=$5,status='pending',
                classification=NULL,language=NULL,ingested_at=NULL WHERE slug=$1");
            sqlx::query(&query)
                .bind(self.source.id.as_str())
                .bind(&updated.filename)
                .bind(&updated.mime)
                .bind(&updated.sha256)
                .bind(updated.size_bytes as i64)
                .execute(&mut *self.tx)
                .await?;
        }
        PgStore::enqueue_in(&mut self.tx, &updated.tenant, job).await?;
        self.tx.commit().await?;
        Ok(())
    }
}
