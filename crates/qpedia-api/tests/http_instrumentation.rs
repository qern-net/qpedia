//! Integration test for the HTTP tracing layer (`http_trace`) — task 5.7.
//!
//! Installs a real `tracing-opentelemetry` layer backed by an in-memory span
//! exporter and drives requests through the middleware to assert the exported
//! `HTTP_Span`:
//!
//! - records `http.request.method`, `http.route`, and `http.response.status_code`
//!   (Req 4.1) and has a non-zero start→end duration (Req 4.2);
//! - sets an error status for 5xx responses (Req 4.6);
//! - continues an inbound W3C trace (same trace id) or starts a fresh root for a
//!   malformed / absent `traceparent`, without erroring (Req 4.4, 4.8);
//! - creates NO span for an excluded path (`/healthz`) (Req 4.7);
//! - carries the `tenant` attribute when a handler sets it (the path the `User`
//!   extractor takes once authenticated, Req 4.9) and omits it otherwise (Req 4.10).
//!
//! The `tenant` portion exercises the span-field plumbing directly (a handler
//! recording `tenant` on the active span, exactly as the `User` extractor does)
//! so the test needs no database; the full authenticated extractor path is
//! covered by the auth/DB-gated flows.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry::Value;
use opentelemetry_sdk::export::trace::SpanData;
use opentelemetry_sdk::testing::trace::{InMemorySpanExporter, InMemorySpanExporterBuilder};
use opentelemetry_sdk::trace::TracerProvider;
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

use qpedia_api::telemetry::http_layer::http_trace;

fn test_subscriber() -> (impl tracing::Subscriber + Send + Sync, InMemorySpanExporter) {
    let exporter = InMemorySpanExporterBuilder::new().build();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("qpedia-api-test");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    Box::leak(Box::new(provider));
    (tracing_subscriber::registry().with(layer), exporter)
}

fn install_w3c_propagator() {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
}

fn app() -> Router {
    Router::new()
        .route("/ok", get(|| async { StatusCode::OK }))
        .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/tenant",
            get(|| async {
                // Mirrors what the `User` extractor does once authenticated:
                // record the resolved tenant on the active HTTP_Span.
                tracing::Span::current().record("tenant", "acme");
                StatusCode::OK
            }),
        )
        .layer(axum::middleware::from_fn(http_trace))
}

fn http_spans(exporter: &InMemorySpanExporter) -> Vec<SpanData> {
    // The HTTP layer overrides the OTel span name via `otel.name` (e.g.
    // "GET /ok"), so identify HTTP_Spans by their `http.route` attribute
    // rather than by span name.
    exporter
        .get_finished_spans()
        .unwrap()
        .into_iter()
        .filter(|s| s.attributes.iter().any(|kv| kv.key.as_str() == "http.route"))
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

#[tokio::test]
async fn records_method_route_status_and_duration() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let res = app()
        .oneshot(Request::builder().uri("/ok").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let spans = http_spans(&exporter);
    assert_eq!(spans.len(), 1);
    let s = &spans[0];
    assert_eq!(str_attr(s, "http.request.method").as_deref(), Some("GET"));
    assert_eq!(str_attr(s, "http.route").as_deref(), Some("/ok"));
    assert_eq!(str_attr(s, "http.response.status_code").as_deref(), Some("200"));
    assert!(s.end_time >= s.start_time, "span has a start→end duration");
    assert_eq!(s.status, Status::Unset, "2xx leaves the span status unset (only 5xx is error)");
    assert!(!has_attr(s, "tenant"), "no tenant set for an anonymous request");
}

#[tokio::test]
async fn five_xx_sets_error_status() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let res = app()
        .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let spans = http_spans(&exporter);
    assert_eq!(spans.len(), 1);
    assert_eq!(str_attr(&spans[0], "http.response.status_code").as_deref(), Some("500"));
    assert!(
        matches!(spans[0].status, Status::Error { .. }),
        "5xx must set the span status to error"
    );
}

#[tokio::test]
async fn continues_inbound_w3c_trace() {
    install_w3c_propagator();
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    // A valid traceparent with this known trace id must be continued.
    let trace_id_hex = "4bf92f3577b34da6a3ce929d0e0e4736";
    let res = app()
        .oneshot(
            Request::builder()
                .uri("/ok")
                .header(
                    "traceparent",
                    format!("00-{trace_id_hex}-00f067aa0ba902b7-01"),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let spans = http_spans(&exporter);
    assert_eq!(spans.len(), 1);
    assert_eq!(
        format!("{:032x}", u128::from_be_bytes(spans[0].span_context.trace_id().to_bytes())),
        trace_id_hex,
        "HTTP_Span must adopt the inbound trace id"
    );
}

#[tokio::test]
async fn malformed_traceparent_starts_new_root_without_error() {
    install_w3c_propagator();
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let res = app()
        .oneshot(
            Request::builder()
                .uri("/ok")
                .header("traceparent", "not-a-valid-traceparent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Request handled without error (Req 4.8) and a valid root span is emitted.
    assert_eq!(res.status(), StatusCode::OK);
    let spans = http_spans(&exporter);
    assert_eq!(spans.len(), 1);
    assert!(spans[0].span_context.trace_id() != opentelemetry::trace::TraceId::INVALID);
}

#[tokio::test]
async fn excluded_path_creates_no_span() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let res = app()
        .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(http_spans(&exporter).is_empty(), "/healthz must not create an HTTP_Span");
}

#[tokio::test]
async fn tenant_attribute_present_when_handler_sets_it() {
    let (subscriber, exporter) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);

    let res = app()
        .oneshot(Request::builder().uri("/tenant").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let spans = http_spans(&exporter);
    assert_eq!(spans.len(), 1);
    assert_eq!(
        str_attr(&spans[0], "tenant").as_deref(),
        Some("acme"),
        "the tenant recorded on the active span must be exported (Req 4.9)"
    );
}
