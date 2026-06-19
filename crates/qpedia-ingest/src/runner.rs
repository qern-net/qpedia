//! JobRunner: a single-worker tokio task that drains the Postgres job queue
//! and dispatches to phase handlers. See SPEC-v2.md §5.1.

use crate::handlers;
use crate::telemetry::TraceCarrier;
use anyhow::Result;
use chrono::Utc;
use qpedia_core::{
    job::{Job, JobKind, JobState},
    source::SourceStatus,
    tenant::Tenant,
    JobId, SourceId,
};
use qpedia_embed::{Embedder, Reranker};
use qpedia_extract::ExtractorRegistry;
use qpedia_llm::LlmProvider;
use qpedia_pg_store::PgStore;
use qpedia_store::{blob::BlobStore, WikiRepoStore};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
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
            // Lease length must exceed the slowest job so a legitimately
            // long run (Marker OCR up to ~10 min + agent LLM turns + embed)
            // isn't reclaimed by another worker mid-flight. A crashed worker's
            // jobs become reclaimable once this lease lapses.
            match self.ctx.db.claim_next_job(&self.worker_id, 30 * 60_000).await {
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
                span.set_parent(parent_cx);
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
        let dispatch = self.dispatch(kind, payload).instrument(span.clone());
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
    async fn dispatch(&self, kind: JobKind, payload: serde_json::Value) -> Result<()> {
        match kind {
            JobKind::Ingest => {
                let p: IngestPayload = serde_json::from_value(payload)?;
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
