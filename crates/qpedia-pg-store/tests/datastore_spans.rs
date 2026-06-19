//! Integration test for the `Datastore_Span` helper (`with_db_span`) — task 7.5.
//!
//! Unlike the in-crate unit tests (which run with no OTel layer installed, so
//! the span mutators are no-ops and only the pass-through is asserted), this
//! test installs a real `tracing-opentelemetry` layer backed by an in-memory
//! span exporter and asserts the **exported spans** carry the attributes the
//! design requires:
//!
//! - `db.system = postgresql`, the logical `db.operation`, and a `db.duration_ms`
//!   for BOTH success and failure (Req 6.1, 6.4);
//! - a child of the active span when one exists, else a root (Req 6.2, 6.3);
//! - the `tenant` attribute inherited from the active context baggage when a
//!   producer attached one via `context_with_tenant`, and omitted otherwise
//!   (Req 6.6 / task 7.4);
//! - error status + a `db.error.category` on failure (Req 6.5).
//!
//! No database is required: `with_db_span` is generic over the wrapped future,
//! so the operations here are plain `async { Ok/Err }` blocks. The test runs on
//! the default current-thread `#[tokio::test]` runtime so a thread-local
//! `set_default` subscriber covers the whole async flow.

use opentelemetry::trace::{FutureExt, Status, TracerProvider as _};
use opentelemetry::Value;
use opentelemetry_sdk::export::trace::SpanData;
use opentelemetry_sdk::testing::trace::{InMemorySpanExporter, InMemorySpanExporterBuilder};
use opentelemetry_sdk::trace::TracerProvider;
use tracing::Instrument;
use tracing_subscriber::prelude::*;

use qpedia_pg_store::telemetry::context_with_tenant;
use qpedia_pg_store::{with_db_span, DbSystem};

/// Build an in-memory-backed `tracing` subscriber and return it with the
/// exporter so the test can read finished spans after the work completes.
fn test_subscriber() -> (impl tracing::Subscriber + Send + Sync, InMemorySpanExporter) {
    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("qpedia-pg-store-test");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    // Leak the provider so its processor lives for the test (simplest; the
    // process exits right after).
    Box::leak(Box::new(provider));
    (tracing_subscriber::registry().with(layer), exporter)
}

fn datastore_spans(exporter: &InMemorySpanExporter) -> Vec<SpanData> {
    exporter
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .filter(|s| s.name == "Datastore_Span")
        .collect()
}

fn str_attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| match &kv.value {
        Value::String(s) => s.to_string(),
        other => format!("{other:?}"),
    })
}

fn has_attr(span: &SpanData, key: &str) -> bool {
    span.attributes.iter().any(|kv| kv.key.as_str() == key)
}

/// A root datastore span records system/operation/duration and an OK status,
/// and has no parent (Req 6.1, 6.3, 6.4).
#[tokio::test]
async fn root_span_records_system_operation_duration() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let r: Result<u32, sqlx::Error> =
        with_db_span(DbSystem::Postgresql, "select_one", async { Ok(7u32) }).await;
    assert_eq!(r.unwrap(), 7);

    let spans = datastore_spans(&exporter);
    assert_eq!(spans.len(), 1, "exactly one Datastore_Span");
    let s = &spans[0];
    assert_eq!(str_attr(s, "db.system").as_deref(), Some("postgresql"));
    assert_eq!(str_attr(s, "db.operation").as_deref(), Some("select_one"));
    assert!(has_attr(s, "db.duration_ms"), "duration recorded on success");
    assert_eq!(s.status, Status::Ok);
    // A root span has no valid parent span id.
    assert!(!s.parent_span_id.to_string().chars().any(|c| c != '0'));
    // No producer attached a tenant → attribute omitted (Req 6.6).
    assert!(!has_attr(s, "tenant"), "tenant omitted with no baggage");
}

/// When opened under an active parent span, the datastore span is its child
/// (Req 6.2).
#[tokio::test]
async fn span_is_child_of_active_parent() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Simulate a Job_Span / HTTP_Span as the active parent.
    let parent = tracing::info_span!("Job_Span");
    async {
        let _: Result<(), sqlx::Error> =
            with_db_span(DbSystem::Postgresql, "claim_next_job", async { Ok(()) }).await;
    }
    .instrument(parent)
    .await;

    let all = exporter.get_finished_spans().unwrap();
    let parent = all.iter().find(|s| s.name == "Job_Span").expect("parent exported");
    let child = all.iter().find(|s| s.name == "Datastore_Span").expect("child exported");
    assert_eq!(
        child.parent_span_id,
        parent.span_context.span_id(),
        "datastore span must be a child of the active Job_Span"
    );
}

/// The datastore span inherits the `tenant` attribute from context baggage a
/// producer attached via `context_with_tenant` (Req 6.6 / task 7.4).
#[tokio::test]
async fn span_inherits_tenant_from_baggage() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let ctx = context_with_tenant("acme");
    let _: Result<(), sqlx::Error> =
        with_db_span(DbSystem::Postgresql, "hybrid_search", async { Ok(()) })
            .with_context(ctx)
            .await;

    let spans = datastore_spans(&exporter);
    assert_eq!(spans.len(), 1);
    assert_eq!(
        str_attr(&spans[0], "tenant").as_deref(),
        Some("acme"),
        "tenant must be inherited from the attached context baggage"
    );
}

/// A failed operation sets the span status to error and records one
/// `db.error.category` plus the duration (Req 6.4, 6.5).
#[tokio::test]
async fn failure_sets_error_status_and_category() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let r: Result<u32, sqlx::Error> = with_db_span(DbSystem::Postgresql, "enqueue_job", async {
        Err(sqlx::Error::PoolTimedOut)
    })
    .await;
    assert!(r.is_err());

    let spans = datastore_spans(&exporter);
    assert_eq!(spans.len(), 1);
    let s = &spans[0];
    assert!(matches!(s.status, Status::Error { .. }), "5xx-equivalent error status");
    // PoolTimedOut → timeout category.
    assert_eq!(str_attr(s, "db.error.category").as_deref(), Some("timeout"));
    assert!(has_attr(s, "db.duration_ms"), "duration recorded on failure too");
}
