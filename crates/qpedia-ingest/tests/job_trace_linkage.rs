//! Integration test for request→job trace linkage (task 6.8, trace half).
//!
//! Asserts the cross-async-boundary linkage mechanism end-to-end at the trace
//! level, without a database:
//!
//! - A job built under an active span embeds the current W3C trace context in
//!   its payload (`trace: Some(..)`), so the later `Job_Span` can re-establish
//!   the same `Transaction_Trace` (Req 5.2).
//! - Re-establishing that context as a `Job_Span` parent yields a span sharing
//!   the originating trace id, parented to the enqueuing span (Req 5.3).
//! - A job built with no active span (a scheduled tick) embeds no context and
//!   so starts a fresh root (Req 5.3, scheduled path).
//!
//! The completed-jobs counter, `Job_Span` attributes, and the live queue-depth
//! gauge run inside the `JobRunner` against a real Postgres; that half is
//! covered by the DB-gated `qpedia-pg-store` queue-depth test and the runner's
//! own wiring (verified by `cargo check`).

use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
use opentelemetry_sdk::testing::trace::{InMemorySpanExporter, InMemorySpanExporterBuilder};
use opentelemetry_sdk::trace::TracerProvider;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::prelude::*;

use qpedia_core::{tenant::Tenant, SourceId};
use qpedia_ingest::{extractor, ingest_job, sync_job, IngestPayload, SyncPayload};

fn test_subscriber() -> (impl tracing::Subscriber + Send + Sync, InMemorySpanExporter, TracerProvider) {
    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("qpedia-ingest-test");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    (tracing_subscriber::registry().with(layer), exporter, provider)
}

fn install_w3c_propagator() {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
}

/// A job enqueued under an active span links to that trace; the re-established
/// Job_Span shares the trace id and is a child of the enqueuing span.
#[tokio::test]
async fn job_links_to_enqueuing_trace() {
    install_w3c_propagator();
    let (subscriber, _exporter, _provider) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let tenant = Tenant::new("acme");
    let src = SourceId::new();

    // --- enqueue side: build the job under an active span ---
    let enqueue_span = tracing::info_span!("Enqueue_Span");
    let (carrier, enqueue_trace_id, enqueue_span_id) = {
        let _e = enqueue_span.enter();
        let job = ingest_job(&tenant, &src).expect("build ingest job");
        let payload: IngestPayload =
            serde_json::from_value(job.payload).expect("decode ingest payload");
        // Read the enqueuing span's own ids from its OTel context so the
        // assertion is independent of span-export ordering/visibility.
        let sc = enqueue_span.context().span().span_context().clone();
        (
            payload.trace.expect("an active span must embed a trace carrier"),
            sc.trace_id(),
            sc.span_id(),
        )
    };

    // --- worker side: re-establish the embedded context as the Job_Span parent ---
    let parent_cx = extractor(&carrier);
    assert!(
        parent_cx.span().span_context().is_valid(),
        "extracted parent context must be valid"
    );
    // The embedded carrier must round-trip the enqueuing trace id AND span id
    // (the latter becomes the Job_Span's parent), so the later Job_Span links
    // to the same Transaction_Trace (Req 5.2, 5.3).
    assert_eq!(
        parent_cx.span().span_context().trace_id(),
        enqueue_trace_id,
        "embedded carrier must round-trip the enqueuing trace id"
    );
    assert_eq!(
        parent_cx.span().span_context().span_id(),
        enqueue_span_id,
        "embedded carrier must carry the enqueuing span id as the Job_Span parent"
    );

    let job_span = tracing::info_span!("Job_Span");
    job_span.set_parent(parent_cx);
    let job_trace_id = job_span.context().span().span_context().trace_id();
    assert_eq!(
        job_trace_id, enqueue_trace_id,
        "Job_Span must share the enqueuing trace id after re-establishing the parent"
    );
}

/// A job built with no active span (a scheduled connector tick) embeds no trace
/// context, so the later Job_Span would start a fresh root.
#[tokio::test]
async fn scheduled_job_has_no_embedded_trace() {
    install_w3c_propagator();
    let (subscriber, _exporter, _provider) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    // No active span here (mirrors the connector scheduler tick).
    let job = sync_job(&Tenant::new("acme"), "connector-1").expect("build sync job");
    let payload: SyncPayload = serde_json::from_value(job.payload).expect("decode sync payload");
    assert!(
        payload.trace.is_none(),
        "a job built with no active span must not embed a trace carrier"
    );
}
