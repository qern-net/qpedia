//! Pure HTTP instrumentation predicates.
//!
//! This module holds the **I/O-free** decision logic for the HTTP tracing
//! layer (design §3, "HTTP instrumentation layer"). Keeping these functions
//! pure and total lets the correctness properties (tasks 5.2 and 5.3) exercise
//! them in isolation with `proptest`:
//!
//! - [`is_excluded`] — whether a matched route bypasses `HTTP_Span` creation
//!   (Req 4.7): the liveness/health path and the metrics-scrape path.
//! - [`classify_http_status`] — maps a numeric HTTP status code to the span
//!   error status, where error iff the code is in the 5xx (500–599) range
//!   (Req 4.6).
//!
//! Neither function performs any I/O, allocates beyond trivial comparisons, or
//! panics for any input.

/// The liveness/health probe path. Excluded from `HTTP_Span` creation so the
/// trace stream is not dominated by health polling (Req 4.7, 7.7).
pub const LIVENESS_PATH: &str = "/healthz";

/// The metrics-scrape path. Excluded from `HTTP_Span` creation so scrape
/// traffic does not generate request spans (Req 4.7).
pub const METRICS_PATH: &str = "/metrics";

/// The readiness/health path. Excluded from `HTTP_Span` creation so dependency
/// readiness polling does not dominate the trace stream (Req 4.7, design §6).
pub const READINESS_PATH: &str = "/api/v1/health";

/// The default set of excluded paths: the liveness/health path, the
/// metrics-scrape path, and the readiness/health path. Suitable to pass as the
/// `excluded` argument to [`is_excluded`].
pub const EXCLUDED_PATHS: &[&str] = &[LIVENESS_PATH, METRICS_PATH, READINESS_PATH];

/// Span error-status classification for an HTTP response (Req 4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpSpanStatus {
    /// Not an error: the status code is outside the 5xx server-error range.
    /// The span status is left unset.
    Unset,
    /// Error: the status code is in the 5xx (500–599) server-error range, so
    /// the span status is set to error.
    Error,
}

/// Does this matched route bypass `HTTP_Span` creation? (Req 4.7)
///
/// Returns `true` exactly when `matched_route` equals one of the configured
/// `excluded` paths (the liveness/health path and the metrics-scrape path).
/// The comparison is an exact match on the matched route template.
///
/// Pure and total: never panics, performs no I/O, and the result depends only
/// on the two arguments.
pub fn is_excluded(matched_route: &str, excluded: &[&str]) -> bool {
    excluded.iter().any(|&p| p == matched_route)
}

/// Classify an HTTP status code into the span error status (Req 4.6).
///
/// Returns [`HttpSpanStatus::Error`] iff `status` is in the inclusive range
/// `500..=599` (server errors); every other code — including informational,
/// success, redirection, client-error, and any out-of-spec value — maps to
/// [`HttpSpanStatus::Unset`].
///
/// Pure and total over all `u16` inputs: never panics and performs no I/O.
pub fn classify_http_status(status: u16) -> HttpSpanStatus {
    if (500..=599).contains(&status) {
        HttpSpanStatus::Error
    } else {
        HttpSpanStatus::Unset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn excluded_paths_match() {
        assert!(is_excluded("/healthz", EXCLUDED_PATHS));
        assert!(is_excluded("/metrics", EXCLUDED_PATHS));
    }

    #[test]
    fn non_excluded_paths_do_not_match() {
        assert!(!is_excluded("/api/v1/sources", EXCLUDED_PATHS));
        assert!(!is_excluded("/", EXCLUDED_PATHS));
        // Partial / prefix matches must not count: exact match only.
        assert!(!is_excluded("/healthz/extra", EXCLUDED_PATHS));
        assert!(!is_excluded("/metricsx", EXCLUDED_PATHS));
    }

    #[test]
    fn empty_excluded_set_excludes_nothing() {
        assert!(!is_excluded("/healthz", &[]));
    }

    #[test]
    fn five_xx_is_error() {
        assert_eq!(classify_http_status(500), HttpSpanStatus::Error);
        assert_eq!(classify_http_status(503), HttpSpanStatus::Error);
        assert_eq!(classify_http_status(599), HttpSpanStatus::Error);
    }

    #[test]
    fn non_five_xx_is_unset() {
        for code in [100u16, 200, 204, 301, 400, 404, 499, 600, 0, u16::MAX] {
            assert_eq!(classify_http_status(code), HttpSpanStatus::Unset);
        }
    }

    // -----------------------------------------------------------------
    // Property 4: HTTP status error classification
    // Feature: otel-lgtm-observability, Property 4
    // Validates: Requirements 4.6
    // -----------------------------------------------------------------
    //
    // Over every possible `u16` status code, `classify_http_status` returns
    // `Error` iff the code is in the inclusive 5xx server-error range
    // (500..=599); every other code maps to `Unset`. The classifier is total:
    // it never panics for any input.
    proptest! {
        #[test]
        fn prop4_http_status_error_iff_5xx(status in any::<u16>()) {
            let classified = classify_http_status(status);
            let is_server_error = (500..=599).contains(&status);

            if is_server_error {
                prop_assert_eq!(classified, HttpSpanStatus::Error, "status = {}", status);
            } else {
                prop_assert_eq!(classified, HttpSpanStatus::Unset, "status = {}", status);
            }
        }
    }

    proptest! {
        // Exhaust the full u16 range explicitly so no code in 0..=u16::MAX is
        // left unclassified (defends the boundaries 499/500 and 599/600).
        #![proptest_config(ProptestConfig::with_cases(1000))]
        #[test]
        fn prop4_full_u16_range_is_total(status in 0u16..=u16::MAX) {
            let expected = if (500..=599).contains(&status) {
                HttpSpanStatus::Error
            } else {
                HttpSpanStatus::Unset
            };
            prop_assert_eq!(classify_http_status(status), expected, "status = {}", status);
        }
    }

    // -----------------------------------------------------------------
    // Property 5: Excluded-path predicate
    // Feature: otel-lgtm-observability, Property 5
    // Validates: Requirements 4.7
    // -----------------------------------------------------------------
    //
    // For an arbitrary matched route and an arbitrary excluded set,
    // `is_excluded` returns `true` iff the route is an exact member of the
    // excluded set. The predicate is total and depends only on its arguments;
    // partial/prefix matches never count.
    proptest! {
        #[test]
        fn prop5_excluded_iff_exact_member(
            route in ".*",
            excluded in proptest::collection::vec(".*", 0..8),
        ) {
            // Build the `&[&str]` view the predicate expects.
            let excluded_refs: Vec<&str> = excluded.iter().map(String::as_str).collect();

            let got = is_excluded(&route, &excluded_refs);
            // Oracle: membership by exact string equality.
            let expected = excluded.iter().any(|p| p == &route);

            prop_assert_eq!(got, expected, "route = {:?}, excluded = {:?}", route, excluded);
        }
    }

    proptest! {
        // When the route is drawn from the excluded set itself, the predicate
        // must always report `true`; when drawn from a disjoint space (a
        // sentinel guaranteed absent), it must always report `false`.
        #[test]
        fn prop5_membership_round_trip(
            excluded in proptest::collection::vec("[a-z/]{1,12}", 1..8),
            pick in any::<prop::sample::Index>(),
        ) {
            let excluded_refs: Vec<&str> = excluded.iter().map(String::as_str).collect();

            // A member is always excluded.
            let member = &excluded[pick.index(excluded.len())];
            prop_assert!(is_excluded(member, &excluded_refs), "member = {:?}", member);

            // A value that cannot be in the set (contains a char outside the
            // generated alphabet) is never excluded.
            let absent = "\u{0}ABSENT";
            prop_assert!(!is_excluded(absent, &excluded_refs));
        }
    }
}
