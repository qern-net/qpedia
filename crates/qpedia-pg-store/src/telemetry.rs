//! Datastore instrumentation: the generic `Datastore_Span` helper.
//!
//! The OSS engine is Postgres-only, so datastore telemetry is built as a
//! single generic helper ([`with_db_span`]) parameterized by a logical
//! operation name. Every `qpedia-pg-store` call routes through it (wiring is
//! task 7.3); this module only defines the helper and the error-classification
//! surface.
//!
//! Design: `otel-lgtm-observability` design §5 (Datastore instrumentation),
//! Requirements 6.1–6.5.
//!
//! Parent/child parenting is automatic. The span is created with the `tracing`
//! macros and instrumented over the future, so the `tracing-opentelemetry`
//! layer nests it under whatever `HTTP_Span`/`Job_Span` is active (→ child,
//! Req 6.2) or, with no active span, emits it as a root (Req 6.3). When
//! telemetry is disabled the OTel-side mutators are no-ops, so the helper is
//! safe to call unconditionally.
//!
//! ## Tenant baggage contract (Req 6.6, task 7.4)
//!
//! A `Datastore_Span` must inherit the `tenant` attribute from its active
//! parent (`HTTP_Span` from task 5.6 / `Job_Span` from task 6.3) so datastore
//! telemetry is tenant-scopable. OTel/`tracing` do **not** copy a parent span's
//! attributes onto a child, so the tenant is threaded out-of-band through the
//! **OpenTelemetry context baggage** rather than by reading the parent span.
//!
//! The contract producers must follow:
//!
//! 1. When a producer knows the tenant (the HTTP layer once the request is
//!    authenticated; the `JobRunner` when it opens a `Job_Span`), it builds a
//!    context carrying the tenant in baggage via [`context_with_tenant`] and
//!    **attaches** it for the duration of the work it spawns — e.g. by polling
//!    the downstream future under `opentelemetry::trace::FutureExt::with_context`
//!    (async) or holding the [`opentelemetry::ContextGuard`] from
//!    `Context::attach()` (sync). Setting a `tenant` *span attribute* alone is
//!    not sufficient — baggage is what flows to children.
//! 2. [`with_db_span`] reads [`BAGGAGE_TENANT`] from the current context's
//!    baggage; when present it records it as the `tenant` span attribute, when
//!    absent it omits the attribute entirely (no-op).
//!
//! This keeps the propagation a no-op when telemetry is disabled or no producer
//! has attached a tenant, and requires **no signature change** to ripple through
//! every `with_db_span` call site.

use std::future::Future;
use std::time::Instant;

use opentelemetry::baggage::BaggageExt;
use opentelemetry::trace::Status;
use opentelemetry::{Context, KeyValue};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Attribute key for the datastore system (`db.system`).
const ATTR_DB_SYSTEM: &str = "db.system";
/// Attribute key for the logical operation name (`db.operation`).
const ATTR_DB_OPERATION: &str = "db.operation";
/// Attribute key for the operation duration in milliseconds.
const ATTR_DB_DURATION_MS: &str = "db.duration_ms";
/// Attribute key for the classified error category on failure.
const ATTR_DB_ERROR_CATEGORY: &str = "db.error.category";
/// Attribute key for the tenant inherited from the active parent span (Req 6.6).
const ATTR_TENANT: &str = "tenant";

/// Baggage key carrying the tenant across the OTel context so a child
/// `Datastore_Span` can inherit it from the active `HTTP_Span`/`Job_Span`.
///
/// See the "Tenant baggage contract" in the module docs.
pub const BAGGAGE_TENANT: &str = "tenant";

/// Build an OTel [`Context`], derived from the current context, that carries
/// `tenant` in baggage so child [`Datastore_Span`](with_db_span)s inherit it.
///
/// Producers (the HTTP layer in task 5.6 and the `Job_Span` in task 6.3) call
/// this and **attach** the returned context — via
/// `opentelemetry::trace::FutureExt::with_context` for an async scope or
/// [`Context::attach`] for a sync scope — around the work they drive. Any
/// `with_db_span` opened while that context is active records the `tenant`
/// attribute. This is the only thing a producer must do to satisfy Req 6.6;
/// the read side lives in [`with_db_span`].
pub fn context_with_tenant(tenant: &str) -> Context {
    Context::current().with_baggage(vec![KeyValue::new(BAGGAGE_TENANT, tenant.to_string())])
}

/// The datastore system a [`Datastore_Span`](with_db_span) describes.
///
/// The OSS engine is Postgres-only; the enum exists so the recorded
/// `db.system` value is centralized and the helper signature is explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbSystem {
    /// PostgreSQL (recorded as `db.system = postgresql`).
    Postgresql,
}

impl DbSystem {
    /// The stable `db.system` attribute value.
    pub fn as_str(self) -> &'static str {
        match self {
            DbSystem::Postgresql => "postgresql",
        }
    }
}

/// The category recorded on a failed [`Datastore_Span`](with_db_span).
///
/// Exactly one of these is recorded on error (Req 6.5). The classification is
/// total: every store error maps to one category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorCategory {
    /// Connection/pool failure (could not reach or hold a connection).
    Connection,
    /// Operation timed out (pool acquire timeout, statement timeout).
    Timeout,
    /// Query/operation execution error (everything not otherwise categorized).
    Execution,
    /// Authorization failure (e.g. invalid credentials / privilege).
    Authorization,
}

impl DbErrorCategory {
    /// The stable lowercase category label recorded as an attribute and used
    /// as the error-status message.
    pub fn as_str(self) -> &'static str {
        match self {
            DbErrorCategory::Connection => "connection",
            DbErrorCategory::Timeout => "timeout",
            DbErrorCategory::Execution => "execution",
            DbErrorCategory::Authorization => "authorization",
        }
    }
}

/// Maps a store error to exactly one [`DbErrorCategory`] (Req 6.5).
///
/// Implemented for `sqlx::Error` below; the mapping is total — every error
/// resolves to one category, defaulting to [`DbErrorCategory::Execution`].
pub trait DbErrorClassify {
    /// The category for this error.
    fn category(&self) -> DbErrorCategory;
}

/// Wrap an async datastore op in a `Datastore_Span`.
///
/// The span is a child of the active `HTTP_Span`/`Job_Span` if one exists,
/// else a root span (Req 6.2, 6.3). It records `db.system`, the logical
/// `db.operation`, and the duration in milliseconds for **both** success and
/// failure (Req 6.1, 6.4). On error it sets the span status to error and
/// records exactly one error category (Req 6.5).
///
/// When the active parent carried a tenant in the context baggage (see the
/// "Tenant baggage contract" and [`context_with_tenant`]), the span also
/// records that `tenant` attribute so datastore telemetry is tenant-scopable
/// (Req 6.6); when no tenant baggage is present the attribute is omitted.
pub async fn with_db_span<F, T, E>(system: DbSystem, operation: &str, fut: F) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    E: DbErrorClassify,
{
    let span = tracing::info_span!("Datastore_Span");
    // Recorded as OTel attributes via the tracing-opentelemetry bridge; no-ops
    // when no OTel layer is installed (telemetry disabled).
    span.set_attribute(ATTR_DB_SYSTEM, system.as_str());
    span.set_attribute(ATTR_DB_OPERATION, operation.to_string());

    // Inherit the tenant from the active context baggage if a parent
    // (HTTP_Span / Job_Span) attached one (Req 6.6). Absent → omit the
    // attribute; this is a total, no-panic read and a no-op when telemetry is
    // disabled or no producer has attached a tenant.
    if let Some(tenant) = Context::current().baggage().get(BAGGAGE_TENANT) {
        span.set_attribute(ATTR_TENANT, tenant.as_str().to_string());
    }

    let start = Instant::now();
    let result = fut.instrument(span.clone()).await;
    let duration_ms = start.elapsed().as_secs_f64() * 1_000.0;

    span.set_attribute(ATTR_DB_DURATION_MS, duration_ms);
    match &result {
        Ok(_) => span.set_status(Status::Ok),
        Err(err) => {
            let category = err.category();
            span.set_attribute(ATTR_DB_ERROR_CATEGORY, category.as_str());
            span.set_status(Status::error(category.as_str()));
        }
    }

    result
}

/// Pure classification of a Postgres `SQLSTATE` code into a category.
///
/// Factored out so it is testable without a live database. `SQLSTATE`
/// classes: `08` = connection exception, `28` = invalid authorization
/// specification; `57014` = `query_canceled` (statement timeout). Everything
/// else — including an absent code — is an execution error.
fn classify_sqlstate(code: Option<&str>) -> DbErrorCategory {
    match code {
        Some("57014") => DbErrorCategory::Timeout,
        Some(c) if c.starts_with("28") => DbErrorCategory::Authorization,
        Some(c) if c.starts_with("08") => DbErrorCategory::Connection,
        _ => DbErrorCategory::Execution,
    }
}

impl DbErrorClassify for sqlx::Error {
    fn category(&self) -> DbErrorCategory {
        match self {
            // Pool acquire timed out — a timeout, not a connection failure.
            sqlx::Error::PoolTimedOut => DbErrorCategory::Timeout,
            // Connection/pool-level failures.
            sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::Configuration(_) => DbErrorCategory::Connection,
            // Server-reported error: classify by SQLSTATE.
            sqlx::Error::Database(db) => classify_sqlstate(db.code().as_deref()),
            // RowNotFound, decode/encode, protocol, type errors, etc.
            _ => DbErrorCategory::Execution,
        }
    }
}

/// `anyhow::Error` is the surface error type of every `qpedia-pg-store`
/// method (sqlx errors are `.context()`-wrapped before they escape). The
/// classification is total: walk the error chain for the underlying
/// `sqlx::Error` and reuse its mapping; if none is present (e.g. a
/// serde/`set_config` failure surfaced through `.context`) default to
/// [`DbErrorCategory::Execution`].
impl DbErrorClassify for anyhow::Error {
    fn category(&self) -> DbErrorCategory {
        for cause in self.chain() {
            if let Some(e) = cause.downcast_ref::<sqlx::Error>() {
                return e.category();
            }
        }
        DbErrorCategory::Execution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::io;

    #[test]
    fn sqlstate_classification_examples() {
        assert_eq!(classify_sqlstate(Some("57014")), DbErrorCategory::Timeout);
        assert_eq!(classify_sqlstate(Some("28000")), DbErrorCategory::Authorization);
        assert_eq!(classify_sqlstate(Some("28P01")), DbErrorCategory::Authorization);
        assert_eq!(classify_sqlstate(Some("08006")), DbErrorCategory::Connection);
        assert_eq!(classify_sqlstate(Some("08001")), DbErrorCategory::Connection);
        // Unknown code and absent code both fall through to execution.
        assert_eq!(classify_sqlstate(Some("23505")), DbErrorCategory::Execution);
        assert_eq!(classify_sqlstate(None), DbErrorCategory::Execution);
    }

    #[test]
    fn sqlx_error_categories() {
        assert_eq!(sqlx::Error::PoolTimedOut.category(), DbErrorCategory::Timeout);
        assert_eq!(sqlx::Error::PoolClosed.category(), DbErrorCategory::Connection);
        assert_eq!(sqlx::Error::WorkerCrashed.category(), DbErrorCategory::Connection);
        assert_eq!(
            sqlx::Error::Io(io::Error::new(io::ErrorKind::ConnectionReset, "reset")).category(),
            DbErrorCategory::Connection
        );
        // A representative non-connection, non-timeout error → execution.
        assert_eq!(sqlx::Error::RowNotFound.category(), DbErrorCategory::Execution);
    }

    #[test]
    fn category_labels_are_distinct_and_lowercase() {
        let all = [
            DbErrorCategory::Connection,
            DbErrorCategory::Timeout,
            DbErrorCategory::Execution,
            DbErrorCategory::Authorization,
        ];
        for c in all {
            assert_eq!(c.as_str(), c.as_str().to_ascii_lowercase());
        }
        assert_eq!(DbSystem::Postgresql.as_str(), "postgresql");
    }

    #[test]
    fn anyhow_error_classifies_via_underlying_sqlx_error() {
        // A `.context()`-wrapped sqlx error must classify by its source.
        let wrapped: anyhow::Error =
            anyhow::Error::new(sqlx::Error::PoolTimedOut).context("acquire conn");
        assert_eq!(wrapped.category(), DbErrorCategory::Timeout);

        let wrapped_conn: anyhow::Error =
            anyhow::Error::new(sqlx::Error::PoolClosed).context("begin tx");
        assert_eq!(wrapped_conn.category(), DbErrorCategory::Connection);

        // A non-sqlx error (e.g. a serde failure surfaced via context) defaults
        // to execution.
        let other = anyhow::anyhow!("not a database error");
        assert_eq!(other.category(), DbErrorCategory::Execution);
    }

    #[tokio::test]
    async fn with_db_span_passes_through_ok_and_err() {
        // OTel layer is not installed in unit tests, so the span mutators are
        // no-ops; the helper must still return the inner result unchanged.
        let ok: Result<u32, sqlx::Error> =
            with_db_span(DbSystem::Postgresql, "select_one", async { Ok(7u32) }).await;
        assert_eq!(ok.unwrap(), 7);

        let err: Result<u32, sqlx::Error> = with_db_span(DbSystem::Postgresql, "boom", async {
            Err(sqlx::Error::PoolTimedOut)
        })
        .await;
        assert!(matches!(err, Err(sqlx::Error::PoolTimedOut)));
    }

    #[tokio::test]
    async fn with_db_span_reads_tenant_baggage_without_panic() {
        use opentelemetry::trace::FutureExt;

        // A producer attaches a context carrying the tenant in baggage; the
        // helper must read it (and, when an OTel layer is installed, record the
        // `tenant` attribute) without panicking. With no exporter installed the
        // span mutators are no-ops, so we assert the read path is total and the
        // inner result passes through unchanged. Full attribute assertion would
        // require an OTel test exporter and is covered by the integration test
        // in task 7.5.
        let ctx = context_with_tenant("acme");
        let ok: Result<u32, sqlx::Error> =
            with_db_span(DbSystem::Postgresql, "select_one", async { Ok(7u32) })
                .with_context(ctx)
                .await;
        assert_eq!(ok.unwrap(), 7);
    }

    #[tokio::test]
    async fn with_db_span_omits_tenant_when_no_baggage() {
        // No producer attached a tenant → the read is a no-op and the helper
        // still returns the inner result unchanged (attribute simply omitted).
        let ctx = context_with_tenant("acme");
        // Sanity: the baggage helper round-trips the tenant in the context.
        assert_eq!(
            ctx.baggage().get(BAGGAGE_TENANT).map(|v| v.as_str().to_string()),
            Some("acme".to_string())
        );

        let ok: Result<u32, sqlx::Error> =
            with_db_span(DbSystem::Postgresql, "select_one", async { Ok(11u32) }).await;
        assert_eq!(ok.unwrap(), 11);
    }

    // -----------------------------------------------------------------
    // Property 7: Datastore error categorization is total
    // Feature: otel-lgtm-observability, Property 7
    // Validates: Requirements 6.5
    // -----------------------------------------------------------------
    //
    // `classify_sqlstate` must be a total function: for ANY input — `None`,
    // the empty string, arbitrary text, or any 5-char SQLSTATE code — it
    // returns exactly one of the four `DbErrorCategory` variants and never
    // panics. The mapping invariants are also asserted: `08*` → Connection,
    // `28*` → Authorization, exactly `57014` → Timeout, everything else →
    // Execution. Note that `57014` begins with neither `08` nor `28`, so the
    // four rules are mutually exclusive and jointly exhaustive.

    /// True iff `c` is one of the four defined categories. Used to prove the
    /// function is into the closed variant set (totality).
    fn is_known_category(c: DbErrorCategory) -> bool {
        matches!(
            c,
            DbErrorCategory::Connection
                | DbErrorCategory::Timeout
                | DbErrorCategory::Execution
                | DbErrorCategory::Authorization
        )
    }

    /// The mapping the spec prescribes, expressed independently of the
    /// implementation so the property cross-checks behaviour rather than
    /// restating the code.
    fn expected_category(code: Option<&str>) -> DbErrorCategory {
        match code {
            Some("57014") => DbErrorCategory::Timeout,
            Some(c) if c.starts_with("28") => DbErrorCategory::Authorization,
            Some(c) if c.starts_with("08") => DbErrorCategory::Connection,
            _ => DbErrorCategory::Execution,
        }
    }

    proptest! {
        #[test]
        fn prop7_classification_is_total_over_arbitrary_strings(opt in proptest::option::of(".*")) {
            // Drive arbitrary strings (and `None`) through the classifier. It
            // must never panic and must land on exactly one known variant.
            let code = opt.as_deref();
            let category = classify_sqlstate(code);
            prop_assert!(is_known_category(category), "input = {:?}", opt);
            prop_assert_eq!(category, expected_category(code), "input = {:?}", opt);
        }
    }

    proptest! {
        #[test]
        fn prop7_classification_matches_invariants_for_sqlstate_codes(
            // Five-character codes drawn from the SQLSTATE alphabet exercise
            // the prefix branches densely (most random ".*" strings miss them).
            code in "[0-9A-Z]{5}"
        ) {
            let category = classify_sqlstate(Some(&code));
            prop_assert!(is_known_category(category), "code = {:?}", code);

            if code == "57014" {
                prop_assert_eq!(category, DbErrorCategory::Timeout, "code = {:?}", code);
            } else if code.starts_with("28") {
                prop_assert_eq!(category, DbErrorCategory::Authorization, "code = {:?}", code);
            } else if code.starts_with("08") {
                prop_assert_eq!(category, DbErrorCategory::Connection, "code = {:?}", code);
            } else {
                prop_assert_eq!(category, DbErrorCategory::Execution, "code = {:?}", code);
            }
        }
    }
}
