//! Cross-boundary trace-context propagation for background jobs.
//!
//! A job is enqueued in one async task (often while handling an HTTP request)
//! and executed later by the `JobRunner` in a different task. To link the
//! `Job_Span` to the originating `Transaction_Trace`, the W3C trace context is
//! serialized into the job payload at enqueue time and re-established as the
//! parent when the job starts.
//!
//! `TraceCarrier` is a serializable W3C trace-context carrier stored in the
//! existing `jobs.payload` JSON column. The field is additive and optional, so
//! payloads written before this feature (with no `trace` field) still
//! deserialize cleanly with `trace = None` and simply start a fresh root span.

use opentelemetry::propagation::{Extractor, Injector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A serializable W3C trace-context carrier embedded in a job payload.
///
/// Round-trips through `serde_json`, so it survives storage in the existing
/// Postgres `jobs.payload` JSON column with no schema migration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TraceCarrier {
    #[serde(default)]
    pub fields: HashMap<String, String>,
}

impl Injector for TraceCarrier {
    fn set(&mut self, key: &str, value: String) {
        self.fields.insert(key.to_string(), value);
    }
}

impl Extractor for TraceCarrier {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    fn keys(&self) -> Vec<&str> {
        self.fields.keys().map(String::as_str).collect()
    }
}

/// Inject the currently active trace context into `carrier` using the globally
/// configured `TextMapPropagator`.
///
/// Call this at enqueue time, under an active span (e.g. an `HTTP_Span`), so
/// the later `Job_Span` can re-establish the same trace as its parent. If no
/// propagator is configured or there is no active context, the carrier is left
/// with whatever the propagator chooses to write (typically nothing).
pub fn inject_current(carrier: &mut TraceCarrier) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let cx = tracing::Span::current().context();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, carrier);
    });
}

/// Extract an `opentelemetry::Context` from `carrier` using the globally
/// configured `TextMapPropagator`.
///
/// Call this at job start to re-establish the originating trace as the parent
/// of the `Job_Span`. An empty or contextless carrier yields a context with no
/// remote parent, which callers treat as a fresh root.
pub fn extractor(carrier: &TraceCarrier) -> opentelemetry::Context {
    opentelemetry::global::get_text_map_propagator(|propagator| propagator.extract(carrier))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry::Context;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use proptest::prelude::*;

    /// Install the W3C `TraceContextPropagator` as the process-global
    /// propagator. `inject_current`/`extractor` both go through
    /// `opentelemetry::global::get_text_map_propagator`, so a global propagator
    /// must be set for the round-trip to carry anything. Setting it repeatedly
    /// is harmless (idempotent assignment).
    fn install_w3c_propagator() {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    }

    prop_compose! {
        /// An arbitrary but **valid** W3C span context: a non-zero 128-bit
        /// trace id and a non-zero 64-bit span id (W3C requires both non-zero
        /// for the context to be valid and thus injectable), marked remote.
        fn arb_span_context()(
            trace_raw in 1u128..=u128::MAX,
            span_raw in 1u64..=u64::MAX,
            sampled in any::<bool>(),
        ) -> SpanContext {
            let trace_id = TraceId::from_bytes(trace_raw.to_be_bytes());
            let span_id = SpanId::from_bytes(span_raw.to_be_bytes());
            let flags = if sampled { TraceFlags::SAMPLED } else { TraceFlags::default() };
            SpanContext::new(trace_id, span_id, flags, true, TraceState::default())
        }
    }

    // -----------------------------------------------------------------
    // Property 6: Trace-context carrier propagation round-trip
    // Feature: otel-lgtm-observability, Property 6
    // Validates: Requirements 5.2, 5.3
    // -----------------------------------------------------------------
    proptest! {
        #[test]
        fn prop6_trace_context_carrier_round_trip(span_context in arb_span_context()) {
            install_w3c_propagator();

            // Make an arbitrary-but-valid trace context the active context, as
            // it would be when a job is enqueued under an active HTTP_Span.
            // With no tracing subscriber installed, `inject_current` falls back
            // to `Context::current()`, which this guard pins to our context.
            let parent_cx = Context::new().with_remote_span_context(span_context.clone());
            let _guard = parent_cx.attach();

            // Inject the active context into a fresh carrier (enqueue side).
            let mut carrier = TraceCarrier::default();
            inject_current(&mut carrier);

            // The carrier must round-trip through serde_json (it is stored in
            // the jobs.payload JSON column) preserving every field exactly.
            let json = serde_json::to_string(&carrier)
                .expect("TraceCarrier serializes");
            let restored: TraceCarrier = serde_json::from_str(&json)
                .expect("TraceCarrier deserializes");
            prop_assert_eq!(&restored, &carrier);

            // Extracting from the round-tripped carrier (job-start side) must
            // re-establish the same trace as a remote parent (Req 5.2, 5.3).
            let extracted = extractor(&restored);
            let extracted_sc = extracted.span().span_context().clone();
            prop_assert!(extracted_sc.is_valid(), "extracted span context should be valid");
            prop_assert_eq!(extracted_sc.trace_id(), span_context.trace_id());
            prop_assert_eq!(extracted_sc.span_id(), span_context.span_id());
            prop_assert!(extracted_sc.is_remote(), "extracted context should be remote");
        }
    }

    /// An empty carrier (e.g. a job payload written before this feature, or a
    /// scheduled-tick job with no originating trace) yields a context with no
    /// valid remote span context, which callers treat as a fresh root.
    #[test]
    fn unit_empty_carrier_yields_invalid_span_context() {
        install_w3c_propagator();
        let carrier = TraceCarrier::default();
        let cx = extractor(&carrier);
        assert!(
            !cx.span().span_context().is_valid(),
            "empty carrier must not yield a valid span context"
        );
    }

    /// An empty `TraceCarrier` also round-trips through serde_json unchanged.
    #[test]
    fn unit_empty_carrier_serde_round_trip() {
        let carrier = TraceCarrier::default();
        let json = serde_json::to_string(&carrier).expect("serialize");
        let restored: TraceCarrier = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, carrier);
    }
}
