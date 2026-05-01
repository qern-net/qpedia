//! JobRunner: a single-worker tokio task that drains the SQLite job queue
//! and dispatches to phase handlers. See DESIGN.md §5.1.

use crate::handlers;
use anyhow::Result;
use chrono::Utc;
use qpedia_core::{
    job::{Job, JobKind, JobState},
    JobId, SourceId,
};
use qpedia_embed::Embedder;
use qpedia_extract::ExtractorRegistry;
use qpedia_llm::LlmProvider;
use qpedia_store::{
    blob::BlobStore,
    sqlite::JobQueue,
    weaviate::WeaviateStore,
    SqliteStore, WikiRepo,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct IngestContext {
    pub db: SqliteStore,
    pub blob: BlobStore,
    pub wiki: WikiRepo,
    pub extractors: Arc<ExtractorRegistry>,
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub embedder: Option<Arc<dyn Embedder>>,
    pub weaviate: Option<Arc<WeaviateStore>>,
}

impl IngestContext {
    pub fn new(
        db: SqliteStore,
        blob: BlobStore,
        wiki: WikiRepo,
        extractors: Arc<ExtractorRegistry>,
        llm: Option<Arc<dyn LlmProvider>>,
        embedder: Option<Arc<dyn Embedder>>,
        weaviate: Option<Arc<WeaviateStore>>,
    ) -> Self {
        Self { db, blob, wiki, extractors, llm, embedder, weaviate }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestPayload {
    pub source_id: SourceId,
}

/// Build an Ingest job for a given source.
pub fn ingest_job(source_id: &SourceId) -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Ingest,
        payload: serde_json::to_value(IngestPayload { source_id: source_id.clone() })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 5,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

/// Build a Lint job (no payload — operates on the whole wiki).
pub fn lint_job() -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Lint,
        payload: serde_json::Value::Null,
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
            weaviate = self.ctx.weaviate.is_some(),
            wiki = %self.ctx.wiki.root().display(),
            "job runner started"
        );
        loop {
            match self.ctx.db.claim_next(&self.worker_id, 5 * 60_000).await {
                Ok(Some(job)) => {
                    let id = job.id.clone();
                    if let Err(e) = self.handle(job).await {
                        error!(job = %id, error = %e, "job failed");
                        let _ = self.ctx.db.fail(&id, &e.to_string(), Some(5_000)).await;
                    } else {
                        let _ = self.ctx.db.complete(&id).await;
                    }
                }
                Ok(None) => tokio::time::sleep(self.poll_idle).await,
                Err(e) => {
                    warn!(error = %e, "claim_next error; backing off");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn handle(&self, job: Job) -> Result<()> {
        match job.kind {
            JobKind::Ingest => {
                let p: IngestPayload = serde_json::from_value(job.payload)?;
                handlers::ingest::run(&self.ctx, &p.source_id).await
            }
            JobKind::Remove => {
                warn!(job = %job.id, "Remove handler not yet implemented");
                Ok(())
            }
            JobKind::Lint => handlers::lint::run(&self.ctx).await,
            JobKind::Reembed => {
                warn!(job = %job.id, "Reembed handler not yet implemented");
                Ok(())
            }
        }
    }
}
