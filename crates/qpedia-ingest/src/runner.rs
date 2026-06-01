//! JobRunner: a single-worker tokio task that drains the Postgres job queue
//! and dispatches to phase handlers. See SPEC-v2.md §5.1.

use crate::handlers;
use anyhow::Result;
use chrono::Utc;
use qpedia_core::{
    job::{Job, JobKind, JobState},
    source::SourceStatus,
    tenant::Tenant,
    JobId, SourceId,
};
use qpedia_embed::Embedder;
use qpedia_extract::ExtractorRegistry;
use qpedia_llm::LlmProvider;
use qpedia_pg_store::PgStore;
use qpedia_store::{blob::BlobStore, WikiRepoStore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct IngestContext {
    pub db: PgStore,
    pub blob: BlobStore,
    /// Per-tenant wiki repo factory. Handlers resolve `wiki_store.get(&tenant).await?`
    /// for the tenant of the source / job they're processing.
    pub wiki_store: WikiRepoStore,
    pub extractors: Arc<ExtractorRegistry>,
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub embedder: Option<Arc<dyn Embedder>>,
}

impl IngestContext {
    pub fn new(
        db: PgStore,
        blob: BlobStore,
        wiki_store: WikiRepoStore,
        extractors: Arc<ExtractorRegistry>,
        llm: Option<Arc<dyn LlmProvider>>,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> Self {
        Self { db, blob, wiki_store, extractors, llm, embedder }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestPayload {
    pub tenant: Tenant,
    pub source_id: SourceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovePayload {
    pub tenant: Tenant,
    pub source_id: SourceId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintPayload {
    pub tenant: Tenant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReembedPayload {
    pub tenant: Tenant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPayload {
    pub tenant: Tenant,
    pub connector_id: String,
}

/// Build an Ingest job for a given source.
pub fn ingest_job(tenant: &Tenant, source_id: &SourceId) -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Ingest,
        payload: serde_json::to_value(IngestPayload {
            tenant: tenant.clone(),
            source_id: source_id.clone(),
        })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 5,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

/// Build a Remove job for a given source.
pub fn remove_job(tenant: &Tenant, source_id: &SourceId) -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Remove,
        payload: serde_json::to_value(RemovePayload {
            tenant: tenant.clone(),
            source_id: source_id.clone(),
        })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 5,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

/// Build a Sync job for an external connector.
pub fn sync_job(tenant: &Tenant, connector_id: &str) -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Sync,
        payload: serde_json::to_value(SyncPayload {
            tenant: tenant.clone(),
            connector_id: connector_id.to_string(),
        })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 3,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

/// Build a Reembed job: clear pgvector for the tenant and rebuild from git.
pub fn reembed_job(tenant: &Tenant) -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Reembed,
        payload: serde_json::to_value(ReembedPayload { tenant: tenant.clone() })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 3,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

/// Build a Lint job for a tenant's wiki.
pub fn lint_job(tenant: &Tenant) -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Lint,
        payload: serde_json::to_value(LintPayload { tenant: tenant.clone() })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 3,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

pub struct JobRunner {
    ctx: IngestContext,
    worker_id: String,
    poll_idle: Duration,
}

impl JobRunner {
    pub fn new(ctx: IngestContext, worker_id: impl Into<String>) -> Self {
        Self {
            ctx,
            worker_id: worker_id.into(),
            poll_idle: Duration::from_secs(1),
        }
    }

    pub async fn run(self) {
        info!(
            worker = %self.worker_id,
            llm = self.ctx.llm.as_ref().map(|p| p.name()),
            embedder = self.ctx.embedder.as_ref().map(|e| e.name()),
            wiki_root = %self.ctx.wiki_store.root().display(),
            "job runner started"
        );
        loop {
            match self.ctx.db.claim_next_job(&self.worker_id, 5 * 60_000).await {
                Ok(Some(job)) => {
                    let id = job.id.clone();
                    // Capture the ingest target up front: if the job dies
                    // permanently we mark the source `failed` so it surfaces
                    // as a terminal error instead of being stranded
                    // mid-pipeline (e.g. stuck at `extracting`).
                    let ingest_target = matches!(job.kind, JobKind::Ingest)
                        .then(|| serde_json::from_value::<IngestPayload>(job.payload.clone()).ok())
                        .flatten();
                    if let Err(e) = self.handle(job).await {
                        let msg = format!("{e:#}");
                        error!(job = %id, error = %msg, "job failed");
                        match self.ctx.db.fail_job(&id, &msg, Some(5_000)).await {
                            Ok(true) => {
                                if let Some(p) = ingest_target {
                                    warn!(job = %id, source = %p.source_id, "ingest job exhausted retries — marking source failed");
                                    let _ = self
                                        .ctx
                                        .db
                                        .update_status(&p.tenant, &p.source_id, SourceStatus::Failed)
                                        .await;
                                    let _ = self
                                        .ctx
                                        .db
                                        .write_audit(
                                            &p.tenant,
                                            "qpedia-bot",
                                            "source.failed",
                                            Some(p.source_id.as_str()),
                                            Some(&serde_json::json!({
                                                "reason": "ingest job exhausted retries",
                                                "error": msg,
                                            })),
                                        )
                                        .await;
                                }
                            }
                            Ok(false) => {} // re-queued for another attempt
                            Err(fe) => warn!(job = %id, error = %format!("{fe:#}"), "fail_job errored"),
                        }
                    } else {
                        let _ = self.ctx.db.complete_job(&id).await;
                    }
                }
                Ok(None) => tokio::time::sleep(self.poll_idle).await,
                Err(e) => {
                    // {:#} renders the anyhow error with its full cause chain
                    // (sqlx error included); plain Display would mask the SQL
                    // detail behind the .context() label.
                    warn!(error = %format!("{e:#}"), "claim_next error; backing off");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn handle(&self, job: Job) -> Result<()> {
        match job.kind {
            JobKind::Ingest => {
                let p: IngestPayload = serde_json::from_value(job.payload)?;
                handlers::ingest::run(&self.ctx, &p.tenant, &p.source_id).await
            }
            JobKind::Remove => {
                let p: RemovePayload = serde_json::from_value(job.payload)?;
                handlers::remove::run(&self.ctx, &p.tenant, &p.source_id).await
            }
            JobKind::Lint => {
                let p: LintPayload = serde_json::from_value(job.payload)?;
                handlers::lint::run(&self.ctx, &p.tenant).await
            }
            JobKind::Reembed => {
                let p: ReembedPayload = serde_json::from_value(job.payload)?;
                handlers::reembed::run(&self.ctx, &p.tenant).await
            }
            JobKind::Sync => {
                let p: SyncPayload = serde_json::from_value(job.payload)?;
                handlers::sync::run(&self.ctx, &p.tenant, &p.connector_id).await
            }
        }
    }
}
