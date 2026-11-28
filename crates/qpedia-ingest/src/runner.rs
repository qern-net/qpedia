//! JobRunner: a single-worker tokio task that drains the Postgres job queue
//! and dispatches to phase handlers. See SPEC-v2.md §5.1.

use crate::handlers;
use crate::telemetry::TraceCarrier;
use anyhow::Result;
use chrono::Utc;
use qpedia_core::{
    job::{Job, JobKind, JobState},
    tenant::Tenant,
    JobId, SourceId,
};
use qpedia_embed::{Embedder, Reranker};
use qpedia_extract::ExtractorRegistry;
use qpedia_llm::LlmProvider;
use qpedia_pg_store::PgStore;
use qpedia_store::{blob::BlobStore, WikiRepoStore};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
use std::time::Duration;
use tracing::{error, info, warn, Instrument};

/// Process-global `jobs.completed` counter (Req 5.7). Lazily built once from
/// the per-crate `qpedia-ingest` meter and memoized for the process lifetime.
/// Reuses the global meter provider installed by the telemetry pipeline (a
/// no-op meter when telemetry is disabled, so this is always safe to call).
fn jobs_completed_counter() -> &'static opentelemetry::metrics::Counter<u64> {
    static COUNTER: OnceLock<opentelemetry::metrics::Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("qpedia-ingest")
            .u64_counter("jobs.completed")
            .with_description("Count of completed jobs, labeled by kind and outcome")
            .build()
    })
}

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
    /// Cross-encoder reranker for the retrieval gather phase. Mandatory
    /// (not `Option`) — a single instance, lazily initialized, shared
    /// across requests. See `qpedia-embed::rerank`.
    pub reranker: Arc<dyn Reranker>,
}

impl IngestContext {
    pub fn new(
        db: PgStore,
        blob: BlobStore,
        wiki_store: WikiRepoStore,
        extractors: Arc<ExtractorRegistry>,
        llm: Option<Arc<dyn LlmProvider>>,
        embedder: Option<Arc<dyn Embedder>>,
        reranker: Arc<dyn Reranker>,
    ) -> Self {
        Self { db, blob, wiki_store, extractors, llm, embedder, reranker }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestPayload {
    pub tenant: Tenant,
    pub source_id: SourceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceCarrier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovePayload {
    pub tenant: Tenant,
    pub source_id: SourceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceCarrier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintPayload {
    pub tenant: Tenant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceCarrier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReembedPayload {
    pub tenant: Tenant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceCarrier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPayload {
    pub tenant: Tenant,
    pub connector_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceCarrier>,
}

/// Capture the currently-active W3C trace context for embedding in a job
/// payload (Req 5.2). Returns `Some(carrier)` when a job is built under an
/// active span (e.g. an `HTTP_Span` while handling an upload, or a `Job_Span`
/// while a handler enqueues a follow-on job) so the later `Job_Span` links to
/// the same `Transaction_Trace`; returns `None` when there is no active context
/// (e.g. the scheduled connector tick) so that job starts a fresh root.
fn current_trace() -> Option<TraceCarrier> {
    let mut carrier = TraceCarrier::default();
    crate::telemetry::inject_current(&mut carrier);
    if carrier.fields.is_empty() {
        None
    } else {
        Some(carrier)
    }
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
            trace: current_trace(),
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
            trace: current_trace(),
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
            trace: current_trace(),
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
        payload: serde_json::to_value(ReembedPayload { tenant: tenant.clone(), trace: current_trace() })?,
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
        payload: serde_json::to_value(LintPayload { tenant: tenant.clone(), trace: current_trace() })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 3,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

const JOB_LEASE_MS: i64 = 30 * 60_000;
const RENEW_CLAIM_EVERY: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
struct JobPersistence {
    active_write: Mutex<()>,
    claim_lost: AtomicBool,
}

tokio::task_local! {
    static JOB_PERSISTENCE: Arc<JobPersistence>;
}

/// Let a short, already-started persistence operation finish on claim loss.
/// Callers outside a job (including direct handler tests) write normally.
pub(crate) async fn persist<T>(write: impl Future<Output = Result<T>>) -> Result<T> {
    let Ok(persistence) = JOB_PERSISTENCE.try_with(Arc::clone) else {
        return write.await;
    };
    let _write_guard = persistence.active_write.lock().await;
    anyhow::ensure!(!persistence.claim_lost.load(Ordering::Relaxed), "job claim lost");
    let result = write.await;
    // Stop the handler at this boundary before it can start its next phase.
    anyhow::ensure!(!persistence.claim_lost.load(Ordering::Relaxed), "job claim lost");
    result
}

async fn run_until_claim_loss<T>(
    handler: impl Future<Output = T>,
    watch_claim: impl Future<Output = ()>,
) -> Option<T> {
    let persistence = Arc::new(JobPersistence::default());
    let handler = JOB_PERSISTENCE.scope(persistence.clone(), handler);
    tokio::pin!(handler);
    tokio::select! {
        biased;
        _ = watch_claim => {}
        result = &mut handler => return Some(result),
    }
    persistence.claim_lost.store(true, Ordering::Relaxed);
    // Keep polling the handler while it holds the write guard. Waiting for the
    // guard on its own would suspend the future responsible for releasing it.
    tokio::select! {
        biased;
        _ = persistence.active_write.lock() => {}
        _ = &mut handler => {}
    }
    None
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
            match self.ctx.db.claim_next_job(&self.worker_id, JOB_LEASE_MS).await {
                Ok(Some(job)) => {
                    let id = job.id.clone();
                    let attempt = job.attempt;
                    // Capture the ingest target up front: if the job dies
                    // permanently we mark the source `failed` so it surfaces
                    // as a terminal error instead of being stranded
                    // mid-pipeline (e.g. stuck at `extracting`).
                    let ingest_target = matches!(job.kind, JobKind::Ingest)
                        .then(|| serde_json::from_value::<IngestPayload>(job.payload.clone()).ok())
                        .flatten();
                    let Some(result) = self.run_with_claim(job).await else { continue; };
                    if let Err(e) = result {
                        let msg = format!("{e:#}");
                        error!(job = %id, error = %msg, "job failed");
                        let failure = if let Some(p) = &ingest_target {
                            self.ctx.db.fail_ingest_job_claim(
                                &p.tenant, &p.source_id, &id, attempt, &msg, Some(5_000),
                            ).await
                        } else {
                            self.ctx.db.fail_job_claim(&id, attempt, &msg, Some(5_000)).await
                        };
                        match failure {
                            Ok(Some(true)) => {
                                if let Some(p) = ingest_target {
                                    warn!(job = %id, source = %p.source_id, "ingest job exhausted retries and marked source failed");
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
                            Ok(Some(false)) => {}
                            Ok(None) => warn!(job = %id, "job claim expired before failure was recorded"),
                            Err(fe) => warn!(job = %id, error = %format!("{fe:#}"), "fail_job errored"),
                        }
                    } else {
                        match self.ctx.db.complete_job_claim(&id, attempt).await {
                            Ok(true) => {}
                            Ok(false) => warn!(job = %id, "job claim expired before completion was recorded"),
                            Err(e) => warn!(job = %id, error = %e, "could not record job completion"),
                        }
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

    async fn run_with_claim(&self, job: Job) -> Option<Result<()>> {
        let id = job.id.clone();
        let attempt = job.attempt;
        run_until_claim_loss(self.handle(job), self.watch_claim(&id, attempt)).await
    }

    async fn watch_claim(&self, id: &JobId, attempt: u32) {
        let first_tick = tokio::time::Instant::now() + RENEW_CLAIM_EVERY;
        let mut renew = tokio::time::interval_at(first_tick, RENEW_CLAIM_EVERY);
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            renew.tick().await;
            match self.ctx.db.renew_job_claim(id, attempt, JOB_LEASE_MS).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(job = %id, "job lease lost");
                    return;
                }
                Err(error) => {
                    warn!(job = %id, %error, "job lease could not be renewed");
                    return;
                }
            }
        }
    }

    async fn handle(&self, job: Job) -> Result<()> {
        let job_id = job.id.clone();
        let kind = job.kind;
        let payload = job.payload;

        // Extract the span-relevant fields (tenant, source.id where applicable,
        // and the embedded W3C trace carrier) from the payload without
        // consuming it — the matching `dispatch` arm re-deserializes the full
        // typed payload. Unknown/missing fields are ignored, so this is safe
        // for every `JobKind` (Lint/Reembed/Sync carry no `source_id`, and
        // pre-feature payloads carry no `trace`).
        let meta: JobSpanMeta = serde_json::from_value(payload.clone()).unwrap_or_default();

        // Open the `Job_Span`. Outcome/duration/status fields are filled in
        // after the handler returns. Dotted keys match the design's span
        // attribute conventions; `otel.status_code`/`otel.status_message` are
        // recognized by the tracing-opentelemetry bridge to set span status.
        let span = tracing::info_span!(
            "Job_Span",
            job.kind = ?kind,
            job.id = %job_id,
            tenant = tracing::field::Empty,
            source.id = tracing::field::Empty,
            job.outcome = tracing::field::Empty,
            job.duration_ms = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            otel.status_message = tracing::field::Empty,
        );
        if let Some(t) = meta.tenant.as_ref() {
            span.record("tenant", tracing::field::display(t));
        }
        if let Some(s) = meta.source_id.as_ref() {
            span.record("source.id", tracing::field::display(s));
        }

        // Re-establish the originating trace as the parent when the payload
        // carries a valid embedded context; otherwise the span stays a fresh
        // root (e.g. scheduled-tick jobs or pre-feature payloads).
        if let Some(carrier) = meta.trace.as_ref() {
            use opentelemetry::trace::TraceContextExt;
            use tracing_opentelemetry::OpenTelemetrySpanExt;
            let parent_cx = crate::telemetry::extractor(carrier);
            if parent_cx.span().span_context().is_valid() {
                let _ = span.set_parent(parent_cx);
            }
        }

        let start = std::time::Instant::now();
        // Drive the handler under the `Job_Span` (so its spans nest beneath it)
        // AND, when the tenant is known, under an OTel context carrying the
        // tenant in baggage so each child `Datastore_Span` inherits the
        // `tenant` attribute (Req 6.6, task 7.4). Baggage — not the span
        // attribute — is what flows to children, so a producer must attach it
        // for the duration of the work it drives (see the pg-store
        // `context_with_tenant` contract). When the tenant is unknown the
        // dispatch runs under the span alone and datastore spans omit `tenant`.
        let dispatch = self.dispatch(&job_id, job.attempt, kind, payload).instrument(span.clone());
        let result = if let Some(t) = meta.tenant.as_ref() {
            use opentelemetry::trace::FutureExt;
            let cx = qpedia_pg_store::telemetry::context_with_tenant(t.as_str());
            dispatch.with_context(cx).await
        } else {
            dispatch.await
        };
        let duration_ms = start.elapsed().as_millis() as i64;
        span.record("job.duration_ms", duration_ms);
        let outcome = match &result {
            Ok(()) => {
                span.record("job.outcome", "success");
                span.record("otel.status_code", "OK");
                "success"
            }
            Err(e) => {
                span.record("job.outcome", "failure");
                span.record("otel.status_code", "ERROR");
                span.record("otel.status_message", tracing::field::display(format!("{e:#}")));
                "failure"
            }
        };

        // Increment the completed-jobs counter (Req 5.7). The `kind` label
        // matches the `Job_Span`'s `job.kind` (Debug form) and the `outcome`
        // label is derived from the same `result` as the span above, so the
        // counter and span always agree. `tenant` is added when known to match
        // the design's metric inventory and enable per-tenant scoping.
        let mut attrs = vec![
            opentelemetry::KeyValue::new("kind", format!("{kind:?}")),
            opentelemetry::KeyValue::new("outcome", outcome),
        ];
        if let Some(t) = meta.tenant.as_ref() {
            attrs.push(opentelemetry::KeyValue::new("tenant", t.to_string()));
        }
        jobs_completed_counter().add(1, &attrs);

        result
    }

    /// Deserialize the typed payload for `kind` and run its handler. The match
    /// must cover every `JobKind` variant (the compiler enforces this).
    async fn dispatch(
        &self,
        job_id: &JobId,
        attempt: u32,
        kind: JobKind,
        payload: serde_json::Value,
    ) -> Result<()> {
        match kind {
            JobKind::Ingest => {
                let sha = payload.get("upload_sha256").and_then(|v| v.as_str()).map(str::to_owned);
                let p: IngestPayload = serde_json::from_value(payload)?;
                if !self.ctx.db.start_claimed_source_ingest(
                    &p.tenant, &p.source_id, job_id, attempt, sha.as_deref(),
                ).await? {
                    return Ok(());
                }
                handlers::ingest::run(&self.ctx, &p.tenant, &p.source_id).await
            }
            JobKind::Remove => {
                let p: RemovePayload = serde_json::from_value(payload)?;
                handlers::remove::run(&self.ctx, &p.tenant, &p.source_id).await
            }
            JobKind::Lint => {
                let p: LintPayload = serde_json::from_value(payload)?;
                handlers::lint::run(&self.ctx, &p.tenant).await
            }
            JobKind::Reembed => {
                let p: ReembedPayload = serde_json::from_value(payload)?;
                handlers::reembed::run(&self.ctx, &p.tenant).await
            }
            JobKind::Sync => {
                let p: SyncPayload = serde_json::from_value(payload)?;
                handlers::sync::run(&self.ctx, &p.tenant, &p.connector_id).await
            }
        }
    }
}

/// Span-relevant fields lifted from any job payload to populate the `Job_Span`.
/// Every field is optional so a single struct deserializes from any
/// `JobKind`'s payload (and from pre-feature payloads with no `trace`).
#[derive(Debug, Default, Deserialize)]
struct JobSpanMeta {
    #[serde(default)]
    tenant: Option<Tenant>,
    #[serde(default)]
    source_id: Option<SourceId>,
    #[serde(default)]
    trace: Option<TraceCarrier>,
}

#[cfg(test)]
mod claim_tests {
    use super::*;
    use tokio::sync::{oneshot, Semaphore};

    #[tokio::test]
    async fn renewal_waiting_for_the_only_connection_does_not_suspend_the_handler() {
        let connections = Semaphore::new(1);
        let (taken_tx, taken_rx) = oneshot::channel();
        let (renew_started_tx, renew_started_rx) = oneshot::channel();
        let (renewed_tx, renewed_rx) = oneshot::channel();
        let pool = &connections;
        let handler = async move {
            let connection = pool.acquire().await.unwrap();
            taken_tx.send(()).unwrap();
            renew_started_rx.await.unwrap();
            drop(connection);
            renewed_rx.await.unwrap();
            "finished"
        };
        let watch = async move {
            taken_rx.await.unwrap();
            renew_started_tx.send(()).unwrap();
            let connection = pool.acquire().await.unwrap();
            renewed_tx.send(()).unwrap();
            drop(connection);
            std::future::pending().await
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            run_until_claim_loss(handler, watch),
        )
        .await
        .expect("both operations can use the one connection");
        assert_eq!(outcome, Some("finished"));
    }

    #[tokio::test]
    async fn claim_loss_finishes_an_active_write_and_stops_the_next_phase() {
        let writes = Mutex::new(Vec::new());
        let (started_tx, started_rx) = oneshot::channel();
        let (lost_tx, lost_rx) = oneshot::channel();
        let log = &writes;
        let handler = async move {
            persist(async {
                log.lock().await.push("first file");
                started_tx.send(()).unwrap();
                lost_rx.await.unwrap();
                log.lock().await.push("second file");
                Ok(())
            })
            .await?;
            log.lock().await.push("next phase");
            Ok::<_, anyhow::Error>(())
        };
        let watch = async move {
            started_rx.await.unwrap();
            lost_tx.send(()).unwrap();
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            run_until_claim_loss(handler, watch),
        )
        .await
        .expect("active write should drain");
        assert!(outcome.is_none());
        assert_eq!(*writes.lock().await, ["first file", "second file"]);
    }
}
