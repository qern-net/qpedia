//! HTTP request tracing tower layer (design §3, "HTTP instrumentation layer").
//!
//! This is the **I/O** side of HTTP instrumentation. The pure decision logic it
//! relies on — [`is_excluded`], [`classify_http_status`], and [`EXCLUDED_PATHS`]
//! — lives in the sibling [`super::http`] module and is exercised in isolation
//! by Properties 4 and 5; this module only orchestrates spans around it.
//!
//! Per non-excluded request, [`http_trace`]:
//! - extracts/continues the W3C `traceparent` / `tracestate` from the request
//!   headers via the global propagator, starting a fresh root trace (no error)
//!   when they are absent or malformed (Req 4.4, 4.8);
//! - opens an `HTTP_Span` named `"<METHOD> <route>"` recording
//!   `http.request.method` and `http.route` (Req 4.1), and on completion the
//!   `http.response.status_code` plus the span's intrinsic start→end duration in
//!   ms (Req 4.2);
//! - sets the span status to error for 5xx responses (Req 4.6);
//! - because the span is a `tracing` span bridged to OpenTelemetry by the
//!   pipeline's `tracing-opentelemetry` layer, log records emitted while it is
//!   active carry the active `trace_id` / `span_id` (Req 4.5);
//! - skips span creation entirely for excluded paths (Req 4.7).
//!
//! Tenant attribution (Req 4.9/4.10, task 5.6) is intentionally NOT handled
//! here — that label is layered on in a later task. This layer owns the span,
//! the status, log correlation, and the `http.server.request.duration` metric
//! (Req 4.3, task 5.5): on completion of a non-excluded request it records an
//! `http.server.request.duration` histogram in milliseconds labeled by the
//! matched route template (`http.route`) and the numeric response status code
//! (`http.response.status_code`).

use std::sync::OnceLock;
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::metrics::Histogram;
use opentelemetry::propagation::Extractor;
use opentelemetry::KeyValue;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use super::http::{classify_http_status, is_excluded, HttpSpanStatus, EXCLUDED_PATHS};

/// Memoized `http.server.request.duration` histogram instrument.
///
/// The instrument is created once from the process-global meter and reused on
/// every request: creating it per-call would be cheap but pointless churn, and
/// the global meter is installed by the telemetry pipeline at startup (before
/// any request is served). When telemetry is disabled the global meter is a
/// no-op, so the memoized instrument is a no-op too and recording costs
/// nothing.
fn request_duration_histogram() -> &'static Histogram<f64> {
    static HISTOGRAM: OnceLock<Histogram<f64>> = OnceLock::new();
    HISTOGRAM.get_or_init(|| {
        let meter = opentelemetry::global::meter("qpedia-api");
        meter
            .f64_histogram("http.server.request.duration")
            .with_unit("ms")
            .with_description("Duration of inbound HTTP server requests in milliseconds")
            .build()
    })
}

/// Adapts an [`axum::http::HeaderMap`] to the OpenTelemetry [`Extractor`] trait
/// so the global W3C propagator can read `traceparent` / `tracestate` off the
/// inbound request headers.
struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// The matched route template for a request, falling back to the raw URI path
/// when no route matched (e.g. a 404). Used for both the excluded-path check
/// and the `http.route` attribute / span name.
fn route_template(req: &Request) -> String {
    req.extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string())
}

/// Axum middleware implementing the HTTP tracing layer. Apply to the router
/// with [`axum::middleware::from_fn`].
///
/// Safe whether or not the OTel pipeline is installed: when telemetry is
/// disabled the global propagator is a no-op (so every request becomes a new
/// root) and the `tracing` span is simply never bridged to an OTel span.
pub async fn http_trace(req: Request, next: Next) -> Response {
    let route = route_template(&req);

    // Excluded paths (liveness/health, metrics scrape) get no HTTP_Span
    // (Req 4.7) — pass straight through with no span created.
    if is_excluded(&route, EXCLUDED_PATHS) {
        return next.run(req).await;
    }

    let method = req.method().clone();

    // Extract the incoming W3C trace context. When the headers are absent or
    // malformed the propagator yields an empty context, which makes the span a
    // new root rather than erroring (Req 4.4, 4.8).
    let parent_cx = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(req.headers()))
    });

    // Span name is "<METHOD> <route>"; `otel.name` overrides the static macro
    // name on the bridged OTel span. The status-code / status fields are
    // declared empty and recorded once the response is known.
    let span_name = format!("{method} {route}");
    let span = tracing::info_span!(
        "HTTP_Span",
        otel.name = %span_name,
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        "http.request.method" = %method,
        "http.route" = %route,
        "http.response.status_code" = tracing::field::Empty,
        tenant = tracing::field::Empty,
    );
    // Continue the inbound trace (or start a fresh root when the extracted
    // context is empty) by adopting it as the span's parent.
    span.set_parent(parent_cx);

    // Run the inner service with the span active so log records emitted during
    // handling are correlated with the active trace/span ids (Req 4.5). The
    // OTel span's intrinsic start→end timestamps capture the request duration
    // in ms (Req 4.2). We also wall-clock the handler here so the
    // `http.server.request.duration` metric can be recorded independently of
    // the span backend (Req 4.3).
    let started = Instant::now();
    let response = next.run(req).instrument(span.clone()).await;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;

    let status = response.status().as_u16();
    span.record("http.response.status_code", status);
    if classify_http_status(status) == HttpSpanStatus::Error {
        // 5xx → span status error (Req 4.6).
        span.record("otel.status_code", "ERROR");
    }

    // Record the server-duration histogram (ms) for this non-excluded request,
    // labeled by the matched route template and the numeric status code
    // (Req 4.3). Tenant attribution is added in a later task.
    request_duration_histogram().record(
        elapsed_ms,
        &[
            KeyValue::new("http.route", route),
            KeyValue::new("http.response.status_code", i64::from(status)),
        ],
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt; // for `oneshot`

    /// A handler that returns 200.
    async fn ok_handler() -> StatusCode {
        StatusCode::OK
    }

    /// A handler that returns 500.
    async fn boom_handler() -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    fn app() -> Router {
        Router::new()
            .route("/ok", get(ok_handler))
            .route("/boom", get(boom_handler))
            .route("/healthz", get(ok_handler))
            .layer(axum::middleware::from_fn(http_trace))
    }

    /// The layer is transparent to the response: a 2xx route still returns 200
    /// and the request is handled without error even with no incoming trace
    /// headers (new-root path, Req 4.8).
    #[tokio::test]
    async fn passes_through_success_without_trace_headers() {
        let res = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// A 5xx response is passed through unchanged (the span status is set to
    /// error internally; the response itself is untouched).
    #[tokio::test]
    async fn passes_through_server_error() {
        let res = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/boom")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// An excluded path (`/healthz`) is served normally — the layer takes the
    /// no-span fast path (Req 4.7) and does not alter the response.
    #[tokio::test]
    async fn excluded_path_is_served_normally() {
        let res = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// A request carrying a valid W3C `traceparent` is handled without error
    /// (continuation path, Req 4.4); the layer must not reject or alter it.
    #[tokio::test]
    async fn continues_incoming_trace_context() {
        let res = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .header(
                        "traceparent",
                        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// A malformed `traceparent` must not error the request — the layer falls
    /// back to a new root trace (Req 4.8).
    #[tokio::test]
    async fn malformed_trace_context_starts_new_root() {
        let res = app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .header("traceparent", "not-a-valid-traceparent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
