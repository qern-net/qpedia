use crate::{sources::row_to_source, PgStore};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use qpedia_core::{job::Job, source::Source, tenant::Tenant, SourceId};
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
                }));
            }
        }
        bail!("no available source slug")
    }

    /// An older running job may have advanced the pipeline after the upload.
    /// Reset it once, when the matching revision reaches the front of the queue.
    pub async fn start_version_ingest(
        &self,
        tenant: &Tenant,
        id: &SourceId,
        sha: &str,
    ) -> Result<bool> {
        let mut tx = self.begin_for(tenant).await?;
        let restart = sqlx::query_scalar::<_, bool>(
            "SELECT restart_version_ingest FROM sources WHERE slug=$1 AND sha256=$2 FOR UPDATE",
        )
        .bind(id.as_str())
        .bind(sha)
        .fetch_optional(&mut *tx)
        .await?;
        if restart == Some(true) {
            sqlx::query(
                "UPDATE sources SET restart_version_ingest=false,status='pending',
                classification=NULL,language=NULL,ingested_at=NULL WHERE slug=$1",
            )
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(restart.is_some())
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
    pub async fn commit(mut self, updated: &Source, job: &Job) -> Result<()> {
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
