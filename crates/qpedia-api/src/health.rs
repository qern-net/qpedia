//! Pure health aggregation and HTTP status mapping.
//!
//! This module holds the **I/O-free** core of the readiness/health endpoint
//! (design §6, "Health endpoint"). The endpoint handler in [`crate::routes`]
//! performs the actual dependency probes (Postgres `SELECT 1` under a 5s
//! timeout) and builds the [`Vec<DepReport>`]; this module then aggregates
//! those reports into an overall [`Aggregate`] and maps that aggregate to an
//! HTTP status code.
//!
//! Keeping [`aggregate`] and [`http_status`] pure and total lets the
//! correctness property (task 8.2) exercise them in isolation with `proptest`.
//! Neither function performs I/O nor panics for any input.

/// Liveness/health status of a single backing dependency (Req 7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepStatus {
    /// The dependency responded successfully within its per-dependency timeout.
    Up,
    /// The dependency check errored or timed out (Req 7.4).
    Down,
}

/// A per-dependency health report (design §6).
///
/// `cause` carries a human-readable indication of the failure when `status`
/// is [`DepStatus::Down`] (Req 7.4); it is `None` for an `Up` dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepReport {
    /// The dependency name (e.g. `postgres`), used as the gauge label.
    pub name: String,
    /// The resolved per-dependency status.
    pub status: DepStatus,
    /// An indication of the failure cause when `status` is `Down`.
    pub cause: Option<String>,
}

/// Aggregate health status across all checked dependencies (Req 7.2, 7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    /// Every checked dependency is `Up`.
    Healthy,
    /// At least one checked dependency is `Down`.
    Degraded,
}

/// The full health response: an aggregate plus the per-dependency reports
/// (design §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    /// The aggregate status derived from `deps` via [`aggregate`].
    pub status: Aggregate,
    /// The per-dependency reports backing the aggregate.
    pub deps: Vec<DepReport>,
}

/// Aggregate per-dependency reports into an overall status (Req 7.2, 7.3).
///
/// Returns [`Aggregate::Healthy`] **iff** every dependency in `deps` is
/// [`DepStatus::Up`]; otherwise returns [`Aggregate::Degraded`]. An empty
/// `deps` slice has no `Down` dependencies and so aggregates to `Healthy`.
///
/// Pure and total: never panics and performs no I/O.
pub fn aggregate(deps: &[DepReport]) -> Aggregate {
    if deps.iter().all(|d| d.status == DepStatus::Up) {
        Aggregate::Healthy
    } else {
        Aggregate::Degraded
    }
}

/// Map an aggregate status to its HTTP response status code (Req 7.2, 7.3).
///
/// [`Aggregate::Healthy`] maps to `200` and [`Aggregate::Degraded`] maps to
/// `503`.
///
/// Pure and total: never panics and performs no I/O.
pub fn http_status(agg: Aggregate) -> u16 {
    match agg {
        Aggregate::Healthy => 200,
        Aggregate::Degraded => 503,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn up(name: &str) -> DepReport {
        DepReport {
            name: name.to_string(),
            status: DepStatus::Up,
            cause: None,
        }
    }

    fn down(name: &str, cause: &str) -> DepReport {
        DepReport {
            name: name.to_string(),
            status: DepStatus::Down,
            cause: Some(cause.to_string()),
        }
    }

    #[test]
    fn empty_deps_is_healthy() {
        assert_eq!(aggregate(&[]), Aggregate::Healthy);
    }

    #[test]
    fn all_up_is_healthy() {
        let deps = [up("postgres"), up("weaviate")];
        assert_eq!(aggregate(&deps), Aggregate::Healthy);
    }

    #[test]
    fn any_down_is_degraded() {
        let deps = [up("postgres"), down("weaviate", "connection refused")];
        assert_eq!(aggregate(&deps), Aggregate::Degraded);
    }

    #[test]
    fn all_down_is_degraded() {
        let deps = [down("postgres", "timeout")];
        assert_eq!(aggregate(&deps), Aggregate::Degraded);
    }

    #[test]
    fn healthy_maps_to_200() {
        assert_eq!(http_status(Aggregate::Healthy), 200);
    }

    #[test]
    fn degraded_maps_to_503() {
        assert_eq!(http_status(Aggregate::Degraded), 503);
    }

    // -----------------------------------------------------------------
    // Property 8: Health aggregation and HTTP status mapping
    // Feature: otel-lgtm-observability, Property 8
    // Validates: Requirements 7.2, 7.3
    // -----------------------------------------------------------------
    //
    // For an arbitrary Vec<DepReport> (arbitrary name, arbitrary Up/Down
    // status, arbitrary optional cause):
    //   - `aggregate` returns Healthy iff every dep is Up (equivalently
    //     Degraded iff at least one dep is Down; empty vec → Healthy);
    //   - `http_status(aggregate(deps))` is 200 when all Up else 503;
    //   - both functions are total (never panic) for any input.

    /// Strategy for an arbitrary single dependency report: arbitrary name,
    /// arbitrary Up/Down status, and an arbitrary optional cause.
    fn dep_report() -> impl Strategy<Value = DepReport> {
        (".*", any::<bool>(), proptest::option::of(".*")).prop_map(|(name, is_up, cause)| {
            DepReport {
                name,
                status: if is_up { DepStatus::Up } else { DepStatus::Down },
                cause,
            }
        })
    }

    proptest! {
        #[test]
        fn prop8_health_aggregation_and_status_mapping(
            deps in proptest::collection::vec(dep_report(), 0..16)
        ) {
            // Totality: neither call panics for any input.
            let agg = aggregate(&deps);
            let code = http_status(agg);

            // Oracle: Healthy iff every dep is Up (empty vec → no Down → Healthy).
            let all_up = deps.iter().all(|d| d.status == DepStatus::Up);
            let any_down = deps.iter().any(|d| d.status == DepStatus::Down);

            // Healthy iff all Up; equivalently Degraded iff at least one Down.
            prop_assert_eq!(agg == Aggregate::Healthy, all_up);
            prop_assert_eq!(agg == Aggregate::Degraded, any_down);

            // Status mapping: 200 when all Up else 503.
            let expected_code = if all_up { 200 } else { 503 };
            prop_assert_eq!(code, expected_code, "deps = {:?}", deps);
        }
    }

    // -----------------------------------------------------------------
    // Task 8.5: health dependency outcomes + fast liveness probe.
    // Validates: Requirements 7.4, 7.7
    // -----------------------------------------------------------------
    //
    // The `/api/v1/health` readiness handler (`crate::routes::health`) probes
    // Postgres with `SELECT 1` under a 5s timeout. The two failure branches —
    // an error (`Ok(Err(e))`) and a timeout (`Err(_elapsed)`) — both build a
    // `DepReport` with `status = Down` and a `Some(cause)` string (Req 7.4).
    // Exercising the live-DB handler requires a running Postgres pool, so that
    // path is covered by the HTTP integration tests (task 8.3 / §6); here we
    // assert the pure outcome mapping the handler relies on: a Down dependency
    // (from either branch) carries a cause and aggregates to Degraded → 503.

    /// Mirrors the handler's `Ok(Err(e))` branch: a dependency *error* maps to
    /// `Down` with the error string as its cause (Req 7.4).
    #[test]
    fn error_outcome_is_down_with_cause_and_degrades() {
        let report = down("postgres", "connection refused");
        assert_eq!(report.status, DepStatus::Down);
        assert_eq!(report.cause.as_deref(), Some("connection refused"));

        let deps = [report];
        assert_eq!(aggregate(&deps), Aggregate::Degraded);
        assert_eq!(http_status(aggregate(&deps)), 503);
    }

    /// Mirrors the handler's `Err(_)` (elapsed) branch: a per-dependency
    /// *timeout* maps to `Down` with a "timed out after 5s" cause (Req 7.4).
    #[test]
    fn timeout_outcome_is_down_with_cause_and_degrades() {
        // Same shape the handler builds on `tokio::time::timeout` elapsing.
        let report = down("postgres", "timed out after 5s");
        assert_eq!(report.status, DepStatus::Down);
        assert!(report
            .cause
            .as_deref()
            .is_some_and(|c| c.contains("timed out")));

        let deps = [report];
        assert_eq!(aggregate(&deps), Aggregate::Degraded);
        assert_eq!(http_status(aggregate(&deps)), 503);
    }

    /// A mix where one dependency is `Up` and another is `Down` (with cause)
    /// still degrades to 503 — a single failing dependency is enough (Req 7.2,
    /// 7.4).
    #[test]
    fn one_down_among_up_degrades_to_503() {
        let deps = [up("postgres"), down("weaviate", "timed out after 5s")];
        assert_eq!(aggregate(&deps), Aggregate::Degraded);
        assert_eq!(http_status(aggregate(&deps)), 503);
    }

    /// The fast liveness probe `/healthz` (`crate::routes::healthz`) must
    /// return `200 OK` quickly and *independently of dependency status*
    /// (Req 7.7). It performs no dependency probes at all — it never touches
    /// the DB — so we can mount it on a minimal router with no `AppState` and
    /// assert it answers 200 well within the 1s liveness budget. Because the
    /// handler does not consult any dependency, a Postgres outage (or any
    /// other `Down` dependency that would degrade `/api/v1/health`) cannot
    /// affect this response.
    #[tokio::test]
    async fn healthz_returns_200_quickly_regardless_of_dependencies() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
            routing::get,
            Router,
        };
        use tower::ServiceExt; // for `oneshot`

        // Minimal router mounting ONLY the liveness probe, mirroring how
        // `core_router` (app.rs) mounts `/healthz` but with zero dependencies.
        let app: Router = Router::new().route("/healthz", get(crate::routes::healthz));

        let start = std::time::Instant::now();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), StatusCode::OK);
        // Fast liveness budget (Req 7.7): with no dependency checks this is
        // effectively instantaneous; a generous 1s bound guards regressions.
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "liveness probe took {elapsed:?}, expected < 1s"
        );
    }
}
