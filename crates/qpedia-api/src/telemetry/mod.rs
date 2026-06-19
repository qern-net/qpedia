//! Telemetry configuration (pure logic).
//!
//! This module holds the parsed, validated telemetry configuration used to
//! drive the OpenTelemetry pipeline. The parsing surface is intentionally
//! **pure**: [`TelemetryConfig::from_vars`] takes an injected variable lookup
//! closure and performs no process-environment or other I/O, so it is fully
//! unit- and property-testable. The exporter wiring (in a later task) consumes
//! the resolved config.
//!
//! See the `otel-lgtm-observability` design doc, §2 (`qpedia-telemetry`).

/// Pure HTTP instrumentation predicates (design §3): the excluded-path
/// predicate and the HTTP-status → span-status classifier.
pub mod http;

/// HTTP request tracing tower layer (design §3): the I/O middleware that opens
/// an `HTTP_Span` per non-excluded request, built on the pure predicates in
/// [`http`].
pub mod http_layer;

use std::time::Duration;
use std::time::Instant;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;

/// Constant maximum number of spans/records per export batch.
const MAX_BATCH_SIZE: usize = 512;
/// Constant connect / per-export flush bound.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
/// Constant shutdown-flush bound.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// Constant cap on in-memory pending batches before oldest are dropped.
const MAX_BUFFERED_BATCHES: usize = 2048;

/// Maximum delay before a non-full batch is exported (spans, logs) and the
/// periodic metric reader interval. Req 2.3 bounds this at 5 seconds.
const MAX_EXPORT_DELAY: Duration = Duration::from_secs(5);

/// 60-second default window for the dropped-record warning throttle (Req 9.5).
const DROP_WARNING_WINDOW: Duration = Duration::from_secs(60);

/// Documented default OTLP collector endpoint (the compose service address).
const DEFAULT_ENDPOINT: &str = "http://otel-lgtm:4317";
/// Documented default `RUST_LOG`-style filter applied when `RUST_LOG` is
/// unset or fails to parse.
const DEFAULT_LOG_FILTER: &str = "info";
/// Documented default deployment environment.
const DEFAULT_DEPLOYMENT_ENVIRONMENT: &str = "development";

/// Fixed service name reported as the `service.name` resource attribute.
const SERVICE_NAME: &str = "qpedia-api";

/// Environment variable names consumed by [`TelemetryConfig::from_vars`].
const VAR_TELEMETRY_ENABLED: &str = "QPEDIA_TELEMETRY_ENABLED";
const VAR_OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const VAR_RUST_LOG: &str = "RUST_LOG";
const VAR_DEPLOYMENT_ENVIRONMENT: &str = "OTEL_DEPLOYMENT_ENVIRONMENT";

/// Inclusive accepted length range for the OTLP endpoint value.
const ENDPOINT_LEN: std::ops::RangeInclusive<usize> = 1..=2048;
/// Inclusive accepted length range for the deployment-environment value.
const DEPLOYMENT_ENVIRONMENT_LEN: std::ops::RangeInclusive<usize> = 1..=256;

/// Parsed, validated telemetry configuration. Pure data; no I/O.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Whether telemetry export is enabled. Unset or any value outside the
    /// disabled set means enabled.
    pub enabled: bool,
    /// Resolved OTLP endpoint. `None` => skip exporter (invalid endpoint, or
    /// telemetry disabled), handled by caller policy.
    pub endpoint: Option<Url>,
    /// `false` => the configured endpoint was invalid; the caller records a
    /// startup warning and falls back to console-only.
    pub endpoint_valid: bool,
    /// `service.name` resource attribute (`"qpedia-api"`).
    pub service_name: String,
    /// `service.version` resource attribute (`CARGO_PKG_VERSION`).
    pub service_version: String,
    /// `deployment.environment` resource attribute (default `"development"`).
    pub deployment_environment: String,
    /// Resolved log filter (`RUST_LOG` or documented default).
    pub log_filter: String,
    /// Maximum spans/records per export batch.
    pub max_batch_size: usize,
    /// Connect / per-export flush bound.
    pub export_timeout: Duration,
    /// Shutdown-flush bound.
    pub flush_timeout: Duration,
    /// Cap on in-memory pending batches.
    pub max_buffered_batches: usize,
}

impl TelemetryConfig {
    /// Pure constructor over an injected variable lookup.
    ///
    /// This function is **total**: it never panics for any input and always
    /// returns a fully-populated config. All variability comes from the
    /// injected `vars` closure, so tests can drive every branch without
    /// touching the process environment.
    pub fn from_vars(vars: &dyn Fn(&str) -> Option<String>) -> Self {
        let enabled = parse_enabled(vars(VAR_TELEMETRY_ENABLED));
        let (endpoint, endpoint_valid) = resolve_endpoint(vars(VAR_OTLP_ENDPOINT));
        let log_filter = resolve_log_filter(vars(VAR_RUST_LOG));
        let deployment_environment = resolve_deployment_environment(vars(VAR_DEPLOYMENT_ENVIRONMENT));

        // "Skip exporter" policy: when the endpoint is invalid OR telemetry is
        // disabled, there is no usable endpoint to export to.
        let endpoint = if enabled && endpoint_valid {
            endpoint
        } else {
            None
        };

        Self {
            enabled,
            endpoint,
            endpoint_valid,
            service_name: SERVICE_NAME.to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
            deployment_environment,
            log_filter,
            max_batch_size: MAX_BATCH_SIZE,
            export_timeout: EXPORT_TIMEOUT,
            flush_timeout: FLUSH_TIMEOUT,
            max_buffered_batches: MAX_BUFFERED_BATCHES,
        }
    }

    /// Convenience constructor that reads from the process environment.
    ///
    /// Thin I/O wrapper over [`from_vars`](Self::from_vars); the pure logic
    /// lives entirely in `from_vars`.
    pub fn from_env() -> Self {
        Self::from_vars(&|k| std::env::var(k).ok())
    }
}

// ===================================================================
// Telemetry pipeline initialization, shutdown flush, and degradation
// ===================================================================
//
// Everything below this line is the I/O layer that consumes the pure
// `TelemetryConfig`. The console logger is installed first and
// independently so any OTLP failure still leaves a working logger
// (Req 2.6) — telemetry never takes down the app.

/// The three OpenTelemetry signal types the pipeline exports. Used by
/// [`FlushOutcome`] to report which signals failed to flush at shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalType {
    /// Distributed-tracing spans.
    Traces,
    /// Metric data points.
    Metrics,
    /// Log records.
    Logs,
}

/// The result of a bounded shutdown flush: which signal types did **not**
/// finish flushing within the timeout.
///
/// Constructible and inspectable without any exporter I/O so the flush
/// reporting logic is unit-testable (see [`FlushOutcome::from_signals`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushOutcome {
    /// `true` => trace spans did not flush in time.
    pub traces_incomplete: bool,
    /// `true` => metrics did not flush in time.
    pub metrics_incomplete: bool,
    /// `true` => log records did not flush in time.
    pub logs_incomplete: bool,
}

impl FlushOutcome {
    /// Build an outcome from per-signal "flushed OK" booleans. `true` means
    /// the signal flushed completely; the stored field is the negation
    /// ("incomplete").
    pub fn from_signals(traces_ok: bool, metrics_ok: bool, logs_ok: bool) -> Self {
        Self {
            traces_incomplete: !traces_ok,
            metrics_incomplete: !metrics_ok,
            logs_incomplete: !logs_ok,
        }
    }

    /// The "everything timed out" outcome used when the whole flush exceeds
    /// the timeout or the flush task fails to join.
    pub fn all_incomplete() -> Self {
        Self {
            traces_incomplete: true,
            metrics_incomplete: true,
            logs_incomplete: true,
        }
    }

    /// `true` iff every signal flushed within the timeout.
    pub fn all_flushed(&self) -> bool {
        !self.traces_incomplete && !self.metrics_incomplete && !self.logs_incomplete
    }

    /// The set of signal types that did not flush in time, in a stable order
    /// (`Traces`, `Metrics`, `Logs`). Suitable for logging at shutdown.
    pub fn incomplete_signals(&self) -> Vec<SignalType> {
        let mut out = Vec::new();
        if self.traces_incomplete {
            out.push(SignalType::Traces);
        }
        if self.metrics_incomplete {
            out.push(SignalType::Metrics);
        }
        if self.logs_incomplete {
            out.push(SignalType::Logs);
        }
        out
    }
}

/// Outcome of [`init_telemetry`].
///
/// `layer_installed == false` means the OpenTelemetry layer was **not**
/// attached and the process is running console-only (telemetry disabled,
/// no usable endpoint, exporter init failure, or a subscriber was already
/// installed by a test/overlay binary).
pub struct TelemetryInit {
    /// Whether the OpenTelemetry trace/log layer was installed on the global
    /// subscriber.
    pub layer_installed: bool,
    /// The guard owning the SDK providers; `None` in the console-only paths.
    /// Held by the caller for the process lifetime; its [`TelemetryGuard::shutdown`]
    /// performs the bounded flush.
    pub guard: Option<TelemetryGuard>,
}

/// Owns the SDK providers so they live for the process lifetime and can be
/// flushed on shutdown. Dropping the guard flushes best-effort (provider
/// `Drop`); prefer the bounded [`TelemetryGuard::shutdown`].
pub struct TelemetryGuard {
    providers: OtelProviders,
    flush_timeout: Duration,
}

/// The SDK providers backing the three signals. Private; only the guard
/// touches them.
struct OtelProviders {
    tracer_provider: opentelemetry_sdk::trace::TracerProvider,
    meter_provider: opentelemetry_sdk::metrics::SdkMeterProvider,
    logger_provider: opentelemetry_sdk::logs::LoggerProvider,
}

impl TelemetryGuard {
    /// Flush all pending spans, metrics, and log records within
    /// `flush_timeout` (10s, Req 2.4). On timeout, abort the remaining flush
    /// and report which signal types did not complete (Req 2.7).
    ///
    /// The flush runs on a blocking thread (the SDK flush calls are
    /// synchronous) and is wrapped in [`tokio::time::timeout`] so this never
    /// blocks past `flush_timeout`; if the timeout elapses the flush task is
    /// detached and the process is free to exit.
    pub async fn shutdown(self) -> FlushOutcome {
        let timeout = self.flush_timeout;
        let providers = self.providers;

        let handle = tokio::task::spawn_blocking(move || {
            // `force_flush` returns per-processor results; treat any error as
            // "this signal did not flush". `all` on an empty vec is `true`.
            let traces_ok = providers
                .tracer_provider
                .force_flush()
                .iter()
                .all(|r| r.is_ok());
            let metrics_ok = providers.meter_provider.force_flush().is_ok();
            let logs_ok = providers
                .logger_provider
                .force_flush()
                .iter()
                .all(|r| r.is_ok());

            // Best-effort orderly shutdown to release exporter resources.
            let _ = providers.tracer_provider.shutdown();
            let _ = providers.meter_provider.shutdown();
            let _ = providers.logger_provider.shutdown();

            (traces_ok, metrics_ok, logs_ok)
        });

        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok((t, m, l))) => FlushOutcome::from_signals(t, m, l),
            // Join error or timeout => nothing is known to have flushed.
            _ => FlushOutcome::all_incomplete(),
        }
    }
}

/// OTLP transport selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OtlpTransport {
    /// gRPC over tonic (default, OTLP port 4317).
    Grpc,
    /// HTTP with binary protobuf (OTLP port 4318, path-based).
    HttpBinary,
}

/// Pure transport selection from the resolved endpoint.
///
/// gRPC is the default (port 4317). HTTP/protobuf is selected when the
/// endpoint indicates path-based OTLP: either it targets the conventional
/// HTTP ingest port 4318, or it carries a non-empty URL path (e.g.
/// `http://host:4318/v1/traces`). Total; never panics.
fn select_transport(endpoint: &Url) -> OtlpTransport {
    let has_path = !endpoint.path().trim_matches('/').is_empty();
    if endpoint.port() == Some(4318) || has_path {
        OtlpTransport::HttpBinary
    } else {
        OtlpTransport::Grpc
    }
}

/// Build the OpenTelemetry SDK [`opentelemetry_sdk::Resource`] from the
/// resolved config, using stable semantic-convention attribute keys.
fn build_resource(cfg: &TelemetryConfig) -> opentelemetry_sdk::Resource {
    use opentelemetry::KeyValue;
    use opentelemetry_semantic_conventions::resource::{
        SERVICE_NAME as SC_SERVICE_NAME, SERVICE_VERSION,
    };

    // `deployment.environment` is the key required by Req 2.2. The newer
    // `deployment.environment.name` semconv constant is gated behind the
    // crate's experimental feature, so the stable key string is used here.
    const DEPLOYMENT_ENVIRONMENT: &str = "deployment.environment";

    opentelemetry_sdk::Resource::new(vec![
        KeyValue::new(SC_SERVICE_NAME, cfg.service_name.clone()),
        KeyValue::new(SERVICE_VERSION, cfg.service_version.clone()),
        KeyValue::new(DEPLOYMENT_ENVIRONMENT, cfg.deployment_environment.clone()),
    ])
}

/// Batch config for spans: drop-oldest queue cap at `max_buffered_batches`
/// (Req 9.4), export at `max_batch_size` or after the 5s delay (Req 2.3).
fn trace_batch_config(cfg: &TelemetryConfig) -> opentelemetry_sdk::trace::BatchConfig {
    opentelemetry_sdk::trace::BatchConfigBuilder::default()
        .with_max_queue_size(cfg.max_buffered_batches)
        .with_max_export_batch_size(cfg.max_batch_size)
        .with_scheduled_delay(MAX_EXPORT_DELAY)
        .build()
}

/// Batch config for logs, mirroring [`trace_batch_config`].
fn log_batch_config(cfg: &TelemetryConfig) -> opentelemetry_sdk::logs::BatchConfig {
    opentelemetry_sdk::logs::BatchConfigBuilder::default()
        .with_max_queue_size(cfg.max_buffered_batches)
        .with_max_export_batch_size(cfg.max_batch_size)
        .with_scheduled_delay(MAX_EXPORT_DELAY)
        .build()
}

/// Build the three SDK providers wired to OTLP exporters over the selected
/// transport. Returns an error string on exporter build failure so the
/// caller can fall back to console-only (Req 2.6). Must be called from
/// within a Tokio runtime (the batch processors and periodic reader spawn
/// background tasks).
fn build_otel_providers(cfg: &TelemetryConfig) -> Result<OtelProviders, String> {
    use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};

    let endpoint = cfg
        .endpoint
        .as_ref()
        .ok_or_else(|| "no OTLP endpoint configured".to_string())?;
    let endpoint_str = endpoint.as_str().trim_end_matches('/').to_string();
    let transport = select_transport(endpoint);
    let resource = build_resource(cfg);

    // ---- span exporter ----
    let span_exporter = match transport {
        OtlpTransport::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint_str.clone())
            .with_timeout(cfg.export_timeout)
            .build()
            .map_err(|e| format!("span exporter (grpc): {e}"))?,
        OtlpTransport::HttpBinary => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint_str.clone())
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(cfg.export_timeout)
            .with_http_client(reqwest::Client::new())
            .build()
            .map_err(|e| format!("span exporter (http): {e}"))?,
    };

    // ---- metric exporter ----
    let metric_exporter = match transport {
        OtlpTransport::Grpc => opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint_str.clone())
            .with_timeout(cfg.export_timeout)
            .build()
            .map_err(|e| format!("metric exporter (grpc): {e}"))?,
        OtlpTransport::HttpBinary => opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(endpoint_str.clone())
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(cfg.export_timeout)
            .with_http_client(reqwest::Client::new())
            .build()
            .map_err(|e| format!("metric exporter (http): {e}"))?,
    };

    // ---- log exporter ----
    let log_exporter = match transport {
        OtlpTransport::Grpc => opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint_str.clone())
            .with_timeout(cfg.export_timeout)
            .build()
            .map_err(|e| format!("log exporter (grpc): {e}"))?,
        OtlpTransport::HttpBinary => opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(endpoint_str)
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(cfg.export_timeout)
            .with_http_client(reqwest::Client::new())
            .build()
            .map_err(|e| format!("log exporter (http): {e}"))?,
    };

    // ---- providers (batched span/log processors + periodic metric reader) ----
    let span_processor = opentelemetry_sdk::trace::BatchSpanProcessor::builder(
        span_exporter,
        opentelemetry_sdk::runtime::Tokio,
    )
    .with_batch_config(trace_batch_config(cfg))
    .build();
    let tracer_provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_span_processor(span_processor)
        .with_resource(resource.clone())
        .build();

    let metric_reader = opentelemetry_sdk::metrics::PeriodicReader::builder(
        metric_exporter,
        opentelemetry_sdk::runtime::Tokio,
    )
    .with_interval(MAX_EXPORT_DELAY)
    .build();
    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(metric_reader)
        .with_resource(resource.clone())
        .build();

    let log_processor = opentelemetry_sdk::logs::BatchLogProcessor::builder(
        log_exporter,
        opentelemetry_sdk::runtime::Tokio,
    )
    .with_batch_config(log_batch_config(cfg))
    .build();
    let logger_provider = opentelemetry_sdk::logs::LoggerProvider::builder()
        .with_log_processor(log_processor)
        .with_resource(resource)
        .build();

    Ok(OtelProviders {
        tracer_provider,
        meter_provider,
        logger_provider,
    })
}

/// Build an [`EnvFilter`](tracing_subscriber::EnvFilter) from the resolved
/// filter string, falling back to the documented default if it somehow
/// fails to parse. `cfg.log_filter` is already validated by `from_vars`, so
/// the fallback is defensive.
fn make_env_filter(filter: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_new(filter)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER))
}

/// Initialize the telemetry pipeline.
///
/// Always installs a console (`fmt`) layer gated by an `EnvFilter`. When
/// `cfg.enabled && cfg.endpoint.is_some()`, additionally attaches the
/// `tracing-opentelemetry` trace layer, the OTel log bridge, and an SDK
/// meter provider feeding the OTLP exporter.
///
/// Robustness:
/// - On exporter init failure / invalid / unreachable endpoint, logs an
///   error and falls back to console-only **without terminating** (Req 2.6).
/// - When telemetry is disabled, installs only console + `EnvFilter` and
///   returns `layer_installed = false`, `guard = None` (Req 2.8).
/// - Installs the global subscriber with `try_init`, so a pre-installed
///   subscriber (tests / overlay binaries) does not cause a panic.
pub fn init_telemetry(cfg: &TelemetryConfig) -> TelemetryInit {
    let env_filter = make_env_filter(&cfg.log_filter);

    if cfg.enabled && cfg.endpoint.is_some() {
        match build_otel_providers(cfg) {
            Ok(providers) => {
                // Propagate W3C trace context across process boundaries and
                // expose the providers process-globally so instrumentation
                // (metrics, manual spans) can reach them.
                opentelemetry::global::set_text_map_propagator(
                    opentelemetry_sdk::propagation::TraceContextPropagator::new(),
                );
                opentelemetry::global::set_tracer_provider(providers.tracer_provider.clone());
                opentelemetry::global::set_meter_provider(providers.meter_provider.clone());

                let tracer = {
                    use opentelemetry::trace::TracerProvider as _;
                    providers.tracer_provider.tracer(SERVICE_NAME)
                };
                let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);
                let log_bridge = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                    &providers.logger_provider,
                );

                let installed = tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer())
                    .with(otel_trace_layer)
                    .with(log_bridge)
                    .try_init()
                    .is_ok();

                if installed {
                    tracing::info!("telemetry: OTLP pipeline initialized");
                } else {
                    tracing::debug!(
                        "telemetry: a global subscriber was already installed; OTel layer not attached"
                    );
                }

                TelemetryInit {
                    layer_installed: installed,
                    guard: Some(TelemetryGuard {
                        providers,
                        flush_timeout: cfg.flush_timeout,
                    }),
                }
            }
            Err(err) => {
                // Console-only fallback; do NOT terminate (Req 2.6).
                let _ = tracing_subscriber::registry()
                    .with(make_env_filter(&cfg.log_filter))
                    .with(tracing_subscriber::fmt::layer())
                    .try_init();
                tracing::error!(
                    error = %err,
                    "telemetry: OTLP exporter init failed; continuing with console logging only"
                );
                TelemetryInit {
                    layer_installed: false,
                    guard: None,
                }
            }
        }
    } else {
        // Disabled, or no usable endpoint: console-only (Req 2.8 / 3.8).
        let _ = tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .try_init();
        if !cfg.enabled {
            tracing::info!("telemetry: export disabled; console logging only");
        } else {
            tracing::warn!("telemetry: no usable OTLP endpoint; console logging only");
        }
        TelemetryInit {
            layer_installed: false,
            guard: None,
        }
    }
}

/// Pure drop-accounting state machine for the export-failure warning
/// throttle (Req 9.5).
///
/// Records dropped-record counts and decides when a warning is due: at most
/// one warning per `window` (60s by default), carrying the **cumulative**
/// count dropped since the previously emitted warning. Drops outside an
/// emitting call accumulate silently. Deterministic given the injected
/// `now`; never panics.
#[derive(Debug, Clone)]
pub struct DropThrottle {
    window: Duration,
    dropped_since_last_warning: u64,
    last_warning: Option<Instant>,
}

impl DropThrottle {
    /// Construct with an explicit warning window.
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            dropped_since_last_warning: 0,
            last_warning: None,
        }
    }

    /// Record `n` dropped records observed at `now`.
    ///
    /// Returns `Some(cumulative)` exactly when a warning is due — that is,
    /// when no warning has been emitted yet, or at least `window` has elapsed
    /// since the last emitted warning — where `cumulative` is the total
    /// dropped count since the previous warning (including this `n`). Returns
    /// `None` while accumulating silently within the window.
    ///
    /// Total and panic-free: the count uses saturating addition and the
    /// elapsed comparison uses saturating duration math.
    pub fn on_drop(&mut self, n: u64, now: Instant) -> Option<u64> {
        self.dropped_since_last_warning = self.dropped_since_last_warning.saturating_add(n);

        let due = match self.last_warning {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.window,
        };

        if due && self.dropped_since_last_warning > 0 {
            let cumulative = self.dropped_since_last_warning;
            self.dropped_since_last_warning = 0;
            self.last_warning = Some(now);
            Some(cumulative)
        } else {
            None
        }
    }
}

impl Default for DropThrottle {
    /// 60-second window per Req 9.5.
    fn default() -> Self {
        Self::new(DROP_WARNING_WINDOW)
    }
}

/// Disabled-value parsing: total function `Option<String> -> bool`.
///
/// After trimming + lowercasing, the set `{0, false, off, no}` means disabled.
/// Unset or any other value means enabled.
fn parse_enabled(raw: Option<String>) -> bool {
    match raw {
        None => true,
        Some(v) => {
            let normalized = v.trim().to_ascii_lowercase();
            !matches!(normalized.as_str(), "0" | "false" | "off" | "no")
        }
    }
}

/// Endpoint resolution: returns `(endpoint, endpoint_valid)`.
///
/// - Unset → documented default endpoint, `endpoint_valid = true`.
/// - Set + valid absolute URL within the accepted length range → use it,
///   `endpoint_valid = true`.
/// - Set + invalid (bad URL, or length out of range) → `None`,
///   `endpoint_valid = false`.
fn resolve_endpoint(raw: Option<String>) -> (Option<Url>, bool) {
    match raw {
        None => {
            // The documented default is a known-good absolute URL.
            let default = Url::parse(DEFAULT_ENDPOINT).expect("default endpoint is a valid URL");
            (Some(default), true)
        }
        Some(value) => {
            if !ENDPOINT_LEN.contains(&value.len()) {
                return (None, false);
            }
            // `Url::parse` only accepts absolute URLs; relative inputs fail.
            match Url::parse(&value) {
                Ok(url) => (Some(url), true),
                Err(_) => (None, false),
            }
        }
    }
}

/// `RUST_LOG` filter resolution.
///
/// If unset or unparseable, fall back to the documented default filter and
/// continue. The resolved filter string is stored verbatim.
fn resolve_log_filter(raw: Option<String>) -> String {
    match raw {
        Some(value) if filter_parses(&value) => value,
        _ => DEFAULT_LOG_FILTER.to_string(),
    }
}

/// Returns whether a filter expression parses as a valid `EnvFilter`.
///
/// `EnvFilter::try_new` is documented to return a `Result`, but in practice it
/// can *panic* on some malformed inputs (e.g. directives that split a
/// multi-byte UTF-8 char on a non-boundary). Since `from_vars` must be total
/// (Req 3.7: an unparseable `RUST_LOG` falls back to the default rather than
/// aborting), treat a panicking parse as "does not parse".
fn filter_parses(value: &str) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracing_subscriber::EnvFilter::try_new(value).is_ok()
    }))
    .unwrap_or(false)
}

/// `deployment.environment` resolution.
///
/// Trim the configured value; if unset, empty, or out of the accepted length
/// range, fall back to the documented default.
fn resolve_deployment_environment(raw: Option<String>) -> String {
    match raw {
        Some(value) => {
            let trimmed = value.trim();
            if DEPLOYMENT_ENVIRONMENT_LEN.contains(&trimmed.len()) {
                trimmed.to_string()
            } else {
                DEFAULT_DEPLOYMENT_ENVIRONMENT.to_string()
            }
        }
        None => DEFAULT_DEPLOYMENT_ENVIRONMENT.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Build an injected variable lookup that returns `value` for `key` and
    /// `None` for every other variable. Lets a test drive a single env var
    /// through `from_vars` without touching the process environment.
    fn single_var(key: &'static str, value: Option<String>) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            if k == key {
                value.clone()
            } else {
                None
            }
        }
    }

    /// Build an injected variable lookup over an explicit set of four vars.
    fn vars_map(
        enabled: Option<String>,
        endpoint: Option<String>,
        rust_log: Option<String>,
        deployment_env: Option<String>,
    ) -> impl Fn(&str) -> Option<String> {
        move |k: &str| match k {
            VAR_TELEMETRY_ENABLED => enabled.clone(),
            VAR_OTLP_ENDPOINT => endpoint.clone(),
            VAR_RUST_LOG => rust_log.clone(),
            VAR_DEPLOYMENT_ENVIRONMENT => deployment_env.clone(),
            _ => None,
        }
    }

    /// The disabled set, mirrored from the production logic for the oracle.
    fn is_disabled_value(raw: &str) -> bool {
        let normalized = raw.trim().to_ascii_lowercase();
        matches!(normalized.as_str(), "0" | "false" | "off" | "no")
    }

    // -----------------------------------------------------------------
    // Property 1: Disabled-value parsing is a total, case-insensitive negation
    // Feature: otel-lgtm-observability, Property 1
    // Validates: Requirements 3.3, 3.6
    // -----------------------------------------------------------------
    proptest! {
        #[test]
        fn prop1_disabled_value_parsing_is_total_case_insensitive_negation(raw in ".*") {
            // Drive an arbitrary string through from_vars by injecting only the
            // telemetry-enable variable. The call must be total (never panic).
            let cfg = TelemetryConfig::from_vars(&single_var(VAR_TELEMETRY_ENABLED, Some(raw.clone())));

            // enabled == false iff the trimmed+lowercased value is in the
            // disabled set; otherwise enabled.
            let expected_enabled = !is_disabled_value(&raw);
            prop_assert_eq!(cfg.enabled, expected_enabled, "raw = {:?}", raw);
        }
    }

    proptest! {
        #[test]
        fn prop1_case_insensitivity_of_disabled_tokens(
            token in prop::sample::select(vec!["0", "false", "off", "no"]),
            pad_left in "[ \t]*",
            pad_right in "[ \t]*",
        ) {
            // Any case variation surrounded by surrounding ASCII whitespace must
            // still disable. Build a mixed-case rendering of the token.
            let mixed: String = token
                .chars()
                .enumerate()
                .map(|(i, c)| if i % 2 == 0 { c.to_ascii_uppercase() } else { c })
                .collect();
            let raw = format!("{pad_left}{mixed}{pad_right}");
            let cfg = TelemetryConfig::from_vars(&single_var(VAR_TELEMETRY_ENABLED, Some(raw.clone())));
            prop_assert!(!cfg.enabled, "expected disabled for raw = {:?}", raw);
        }
    }

    #[test]
    fn unit_disabled_value_examples_case_insensitive() {
        for raw in ["FALSE", "Off", " No ", "0", "  false  ", "OFF", "nO"] {
            let cfg = TelemetryConfig::from_vars(&single_var(VAR_TELEMETRY_ENABLED, Some(raw.to_string())));
            assert!(!cfg.enabled, "expected disabled for {raw:?}");
        }
        // Unset and other strings enable.
        let unset = TelemetryConfig::from_vars(&single_var(VAR_TELEMETRY_ENABLED, None));
        assert!(unset.enabled, "unset should enable");
        for raw in ["1", "true", "yes", "on", "anything", ""] {
            let cfg = TelemetryConfig::from_vars(&single_var(VAR_TELEMETRY_ENABLED, Some(raw.to_string())));
            assert!(cfg.enabled, "expected enabled for {raw:?}");
        }
    }

    // -----------------------------------------------------------------
    // Property 2: Endpoint resolution and validity
    // Feature: otel-lgtm-observability, Property 2
    // Validates: Requirements 3.1, 3.2, 3.8
    // -----------------------------------------------------------------

    /// Strategy mixing unset, arbitrary strings, and likely-valid URLs so the
    /// property exercises both the valid and invalid branches.
    fn endpoint_input() -> impl Strategy<Value = Option<String>> {
        prop_oneof![
            1 => Just(None),
            3 => ".*".prop_map(Some),
            2 => "https?://[a-z][a-z0-9-]{0,20}(:[0-9]{1,5})?(/[a-z0-9]{0,10})?".prop_map(Some),
        ]
    }

    proptest! {
        #[test]
        fn prop2_endpoint_resolution_and_validity(raw in endpoint_input()) {
            // Telemetry enabled (enable var left unset) so a valid endpoint is
            // surfaced as Some(..).
            let cfg = TelemetryConfig::from_vars(&single_var(VAR_OTLP_ENDPOINT, raw.clone()));

            match &raw {
                None => {
                    // Unset → documented compose default, valid.
                    prop_assert!(cfg.endpoint_valid);
                    prop_assert_eq!(
                        cfg.endpoint.as_ref().map(|u| u.as_str().to_string()),
                        Some(Url::parse(DEFAULT_ENDPOINT).unwrap().as_str().to_string())
                    );
                }
                Some(value) => {
                    // Validity flag is consistent with: within length bound AND
                    // url::Url::parse accepts the value (absolute URL).
                    let within_len = (1..=2048).contains(&value.len());
                    let parsed = Url::parse(value);
                    let expected_valid = within_len && parsed.is_ok();
                    prop_assert_eq!(cfg.endpoint_valid, expected_valid, "value = {:?}", value);

                    if expected_valid {
                        prop_assert_eq!(
                            cfg.endpoint.as_ref().map(|u| u.as_str().to_string()),
                            Some(parsed.unwrap().as_str().to_string())
                        );
                    } else {
                        // Invalid endpoint while enabled → no usable endpoint.
                        prop_assert!(cfg.endpoint.is_none(), "value = {:?}", value);
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Property 3: Config resolution always yields usable defaults and never panics
    // Feature: otel-lgtm-observability, Property 3
    // Validates: Requirements 3.5, 3.7
    // -----------------------------------------------------------------

    /// An arbitrary Some(string)/None for any consumed variable.
    fn opt_string() -> impl Strategy<Value = Option<String>> {
        prop_oneof![
            1 => Just(None),
            3 => ".*".prop_map(Some),
        ]
    }

    proptest! {
        #[test]
        fn prop3_config_defaults_are_usable_and_total(
            enabled in opt_string(),
            endpoint in opt_string(),
            rust_log in opt_string(),
            deployment_env in opt_string(),
        ) {
            // Never panics, always fully populated.
            let cfg = TelemetryConfig::from_vars(&vars_map(
                enabled,
                endpoint,
                rust_log,
                deployment_env,
            ));

            // deployment_environment is non-empty and within 1..=256.
            let de_len = cfg.deployment_environment.len();
            prop_assert!((1..=256).contains(&de_len), "deployment_environment len = {de_len}");

            // log_filter is always a parseable EnvFilter (falls back to default).
            prop_assert!(
                tracing_subscriber::EnvFilter::try_new(&cfg.log_filter).is_ok(),
                "log_filter not parseable: {:?}",
                cfg.log_filter
            );

            // Fixed identity attributes.
            prop_assert_eq!(cfg.service_name.as_str(), "qpedia-api");
            prop_assert_eq!(cfg.service_version.as_str(), env!("CARGO_PKG_VERSION"));

            // Constant batch/timeout/buffer fields hold their documented constants.
            prop_assert_eq!(cfg.max_batch_size, MAX_BATCH_SIZE);
            prop_assert_eq!(cfg.export_timeout, EXPORT_TIMEOUT);
            prop_assert_eq!(cfg.flush_timeout, FLUSH_TIMEOUT);
            prop_assert_eq!(cfg.max_buffered_batches, MAX_BUFFERED_BATCHES);
        }
    }

    // -----------------------------------------------------------------
    // Unit test 2.5: endpoint default example
    // Validates: Requirement 3.2
    // -----------------------------------------------------------------
    #[test]
    fn unit_endpoint_defaults_to_compose_address_when_unset() {
        // OTEL_EXPORTER_OTLP_ENDPOINT unset → documented compose default.
        let cfg = TelemetryConfig::from_vars(&single_var(VAR_OTLP_ENDPOINT, None));
        assert!(cfg.endpoint_valid);
        assert_eq!(
            cfg.endpoint.as_ref().map(Url::as_str),
            Some("http://otel-lgtm:4317/")
        );
    }

    // -----------------------------------------------------------------
    // Property 9: Dropped-record warning throttle
    // Feature: otel-lgtm-observability, Property 9
    // Validates: Requirements 9.5
    // -----------------------------------------------------------------
    //
    // Across an arbitrary sequence of (n, time-delta) drops driven by a
    // monotonically advancing Instant:
    //   (a) at most one warning is emitted per `window` — consecutive emitted
    //       warnings are spaced at least `window` apart; and
    //   (b) whenever a warning is emitted, its value equals the cumulative
    //       dropped count since the previous warning, so the sum of all
    //       emitted values plus the still-pending count equals the total
    //       dropped (no record lost or double-counted).
    proptest! {
        #[test]
        fn prop9_dropped_record_warning_throttle(
            // Bounded so the accumulated time and counts cannot overflow.
            events in proptest::collection::vec(
                (0u64..=1_000_000u64, 0u64..=180_000u64),
                0..200,
            ),
        ) {
            let window = Duration::from_secs(60);
            let mut throttle = DropThrottle::new(window);

            let base = Instant::now();
            let mut elapsed_ms: u64 = 0;

            let mut total_dropped: u128 = 0;
            let mut emitted_sum: u128 = 0;
            // (timestamp_ms, value) of each emitted warning.
            let mut emissions: Vec<(u64, u64)> = Vec::new();

            for (n, delta_ms) in events {
                elapsed_ms = elapsed_ms.saturating_add(delta_ms);
                total_dropped = total_dropped.saturating_add(n as u128);
                let now = base + Duration::from_millis(elapsed_ms);

                if let Some(cumulative) = throttle.on_drop(n, now) {
                    // (b1) An emitted warning always carries a positive count.
                    prop_assert!(cumulative > 0);
                    emitted_sum = emitted_sum.saturating_add(cumulative as u128);
                    emissions.push((elapsed_ms, cumulative));
                }
            }

            // (a) Consecutive emissions are at least `window` apart.
            for pair in emissions.windows(2) {
                let gap_ms = pair[1].0 - pair[0].0;
                prop_assert!(
                    gap_ms >= window.as_millis() as u64,
                    "emissions {} and {} only {}ms apart",
                    pair[0].0, pair[1].0, gap_ms
                );
            }

            // (b2) No drops lost or double-counted: emitted + pending == total.
            let pending = throttle.dropped_since_last_warning as u128;
            prop_assert_eq!(
                emitted_sum + pending,
                total_dropped,
                "emitted({}) + pending({}) != total({})",
                emitted_sum, pending, total_dropped
            );
        }
    }

    #[test]
    fn unit_drop_throttle_first_drop_warns_then_accumulates_silently() {
        let window = Duration::from_secs(60);
        let mut throttle = DropThrottle::new(window);
        let base = Instant::now();

        // First drop warns immediately, carrying its own count.
        assert_eq!(throttle.on_drop(5, base), Some(5));
        // Within the window: accumulate silently.
        assert_eq!(throttle.on_drop(3, base + Duration::from_secs(10)), None);
        assert_eq!(throttle.on_drop(2, base + Duration::from_secs(30)), None);
        // After the window elapses: emit the cumulative since the last warning.
        assert_eq!(throttle.on_drop(1, base + Duration::from_secs(61)), Some(6));
    }

    // -----------------------------------------------------------------
    // Unit test 3.5: disabled pipeline installs no OTel layer
    // Validates: Requirement 2.8
    // -----------------------------------------------------------------
    #[test]
    fn unit_disabled_telemetry_installs_no_otel_layer() {
        // Disabled config → console-only: no OTel layer, no guard.
        let cfg = TelemetryConfig::from_vars(&single_var(
            VAR_TELEMETRY_ENABLED,
            Some("false".to_string()),
        ));
        assert!(!cfg.enabled);

        let init = init_telemetry(&cfg);
        assert!(!init.layer_installed, "disabled telemetry must not install an OTel layer");
        assert!(init.guard.is_none(), "disabled telemetry must not return a guard");
    }

    // -----------------------------------------------------------------
    // Unit test 3.5: FlushOutcome reports the incomplete signal-type set
    // Validates: Requirement 2.7
    // -----------------------------------------------------------------
    #[test]
    fn unit_flush_outcome_reports_incomplete_signals() {
        // All signals flushed OK → nothing incomplete.
        let ok = FlushOutcome::from_signals(true, true, true);
        assert!(ok.all_flushed());
        assert!(ok.incomplete_signals().is_empty());

        // Traces and logs timed out; metrics flushed.
        let partial = FlushOutcome::from_signals(false, true, false);
        assert!(!partial.all_flushed());
        assert_eq!(
            partial.incomplete_signals(),
            vec![SignalType::Traces, SignalType::Logs]
        );

        // Total timeout → every signal reported incomplete, in stable order.
        let none = FlushOutcome::all_incomplete();
        assert!(!none.all_flushed());
        assert_eq!(
            none.incomplete_signals(),
            vec![SignalType::Traces, SignalType::Metrics, SignalType::Logs]
        );
    }
}
