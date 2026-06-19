//! Authenticating Grafana reverse proxy (`/api/v1/observability/grafana/*path`).
//!
//! This is the **sole ingress** to the internal-only `otel-lgtm` Grafana
//! instance (design §9). `qpedia-api` is the single source of truth for
//! identity: Grafana is configured to trust the `X-WEBAUTH-USER` /
//! `X-WEBAUTH-ROLE` headers this proxy injects, and the `otel-lgtm` service
//! publishes no host ports, so the only way a request reaches Grafana is
//! through this handler — after the QPEDIA session has been validated.
//!
//! Security invariants (all enforced *before* anything is forwarded):
//!
//! 1. **Authenticated-first.** The [`User`] extractor runs before the handler
//!    body; an absent/invalid session yields `401` and Grafana never sees the
//!    request (the body is not even read).
//! 2. **No header spoofing.** Any client-supplied `X-WEBAUTH-*` header is
//!    stripped, then the proxy injects the QPEDIA identity + mapped Grafana
//!    org role from the pure resolver in [`crate::observability`].
//! 3. **View gating.** A request targeting a dashboard outside the caller's
//!    permitted catalog-view set (§11 matrix) returns `403`.
//! 4. **Tenant pinning.** For every non-superadmin view the `var-tenant`
//!    dashboard variable is pinned to the caller's own tenant; any attempt to
//!    widen it (request a different tenant) returns `403`.
//! 5. **Bounded upstream.** A 10s timeout caps the upstream call; connect
//!    failure or timeout returns `502`/`503` with a small JSON body — the
//!    proxy never returns `500` and never hangs.
//!
//! This route is deliberately **not** in the `HTTP_Span` excluded set, so
//! proxy traffic is itself traced.

use crate::app::AppState;
use crate::auth::User;
use crate::observability::{resolve_user, CatalogView, EffectiveView};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header::HeaderName, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use std::sync::OnceLock;
use std::time::Duration;
use tracing::warn;

/// Lowercase prefix of every header the proxy strips from inbound requests so
/// a client cannot spoof its Grafana identity/role. The injected headers are
/// `X-WEBAUTH-USER` (matching `GF_AUTH_PROXY_HEADER_NAME`) and `X-WEBAUTH-ROLE`
/// (matching `GF_AUTH_PROXY_HEADERS=Role:...`).
const WEBAUTH_PREFIX: &str = "x-webauth-";

/// The Grafana dashboard-variable that scopes a dashboard to one tenant.
const TENANT_VAR: &str = "var-tenant";

/// Default internal upstream for the Grafana instance inside the compose
/// network. Overridable via `QPEDIA_GRAFANA_UPSTREAM` (no trailing slash).
const DEFAULT_UPSTREAM: &str = "http://otel-lgtm:3000";

/// Upstream timeout (Req 10.11). No Grafana request may take longer; on
/// expiry the proxy returns `503` rather than hanging.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Body-read cap for forwarded request bodies. Matches the API upload ceiling.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// The seven catalog views, used to map a requested Grafana path/dashboard to
/// the [`CatalogView`] it represents (when one applies).
const ALL_VIEWS: [CatalogView; 7] = [
    CatalogView::ServiceOverview,
    CatalogView::LogsExplorer,
    CatalogView::TraceExplorer,
    CatalogView::DbDatastorePerformance,
    CatalogView::IngestionJobQueue,
    CatalogView::DependencyHealth,
    CatalogView::AnomaliesAlerts,
];

/// Shared HTTP client for upstream calls. Built once (connection pooling),
/// with the 10s timeout baked in so every forwarded request is bounded.
fn upstream_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(UPSTREAM_TIMEOUT)
            .build()
            .expect("build grafana proxy http client")
    })
}

/// Resolve the upstream base URL (no trailing slash).
fn upstream_base() -> String {
    std::env::var("QPEDIA_GRAFANA_UPSTREAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_UPSTREAM.to_string())
}

/// Map a requested Grafana path (plus query string) to the [`CatalogView`] it
/// targets, if any. Returns `None` for generic UI assets / API calls that are
/// not a specific dashboard — those are required to render *any* view and are
/// not view-gated. Matching is by the canonical dashboard slug appearing in
/// the path or query (e.g. `d/service-overview/...` or `?dashboard=...`).
///
/// Pure and total; never panics.
pub fn request_catalog_view(path: &str, query: Option<&str>) -> Option<CatalogView> {
    let haystack = match query {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };
    ALL_VIEWS.into_iter().find(|v| haystack.contains(v.slug()))
}

/// Outcome of pinning the `var-tenant` dashboard variable for a non-superadmin
/// caller.
#[derive(Debug, PartialEq, Eq)]
pub enum TenantPin {
    /// The (possibly rewritten) query string to forward upstream.
    Pinned(String),
    /// The caller tried to view a tenant other than their own — reject (403).
    Widened,
}

/// Pin the `var-tenant` query parameter to `tenant` for a tenant-scoped
/// (non-superadmin) caller (design §11 "scope is enforced by the proxy"):
///
/// - If `var-tenant` is present and *all* of its values equal `tenant`, the
///   query is forwarded unchanged ([`TenantPin::Pinned`]).
/// - If any `var-tenant` value differs from `tenant`, it is a widening attempt
///   ([`TenantPin::Widened`] → caller gets `403`).
/// - If `var-tenant` is absent, it is appended pinned to `tenant` so the
///   dashboard is scoped even when the client omitted it.
///
/// Other query parameters are preserved (and re-encoded). Pure and total.
pub fn pin_tenant_query(query: Option<&str>, tenant: &str) -> TenantPin {
    use url::form_urlencoded;

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut saw_tenant = false;

    if let Some(q) = query {
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            if k == TENANT_VAR {
                saw_tenant = true;
                if v != tenant {
                    return TenantPin::Widened;
                }
            }
            pairs.push((k.into_owned(), v.into_owned()));
        }
    }

    if !saw_tenant {
        pairs.push((TENANT_VAR.to_string(), tenant.to_string()));
    }

    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in &pairs {
        ser.append_pair(k, v);
    }
    TenantPin::Pinned(ser.finish())
}

/// Whether a header is hop-by-hop (or otherwise managed by the HTTP client)
/// and must not be copied verbatim across the proxy boundary. `content-length`
/// and `transfer-encoding` are excluded because the body is re-framed.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// Whether an inbound header is a client-supplied `X-WEBAUTH-*` header that
/// must be stripped before the proxy injects its own identity/role.
fn is_webauth(name: &HeaderName) -> bool {
    name.as_str()
        .to_ascii_lowercase()
        .starts_with(WEBAUTH_PREFIX)
}

/// Build a small JSON error response with the given status. Used for the
/// `403`/`502`/`503` paths so the frontend gets a structured body.
fn json_error(status: StatusCode, msg: &str) -> Response<Body> {
    let body = format!("{{\"error\":{}}}", serde_json::Value::String(msg.into()));
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("build json error response")
}

/// Authenticating Grafana reverse-proxy handler.
///
/// Mounted (in task 11.2) as a catch-all for every method at
/// `/api/v1/observability/grafana/*path`. The [`User`] extractor guarantees a
/// valid session before this runs; everything else (view gating, tenant
/// pinning, header injection, bounded forwarding) happens here.
pub async fn grafana_proxy(
    State(_state): State<AppState>,
    user: User,
    Path(path): Path<String>,
    req: Request,
) -> Response<Body> {
    let resolution = resolve_user(&user);

    // --- view gating (§11 matrix) -----------------------------------------
    let (parts, body) = req.into_parts();
    let query = parts.uri.query();

    if let Some(view) = request_catalog_view(&path, query) {
        if !resolution.permitted_views.contains(&view) {
            return json_error(
                StatusCode::FORBIDDEN,
                "requested observability view is not permitted for your role",
            );
        }
    }

    // --- tenant pinning (non-superadmin) ----------------------------------
    let forward_query: Option<String> = if resolution.view == EffectiveView::Superadmin {
        query.map(|q| q.to_string())
    } else {
        match pin_tenant_query(query, user.tenant.as_str()) {
            TenantPin::Pinned(q) => Some(q),
            TenantPin::Widened => {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "cross-tenant observability access is not permitted",
                );
            }
        }
    };

    // --- build the upstream URL -------------------------------------------
    let mut url = format!("{}/{}", upstream_base(), path);
    if let Some(q) = forward_query.as_deref() {
        if !q.is_empty() {
            url.push('?');
            url.push_str(q);
        }
    }

    // --- forward headers: drop hop-by-hop + strip client X-WEBAUTH-* ------
    let mut headers = HeaderMap::new();
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) || is_webauth(name) {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }

    // --- inject the trusted identity + mapped Grafana org role ------------
    let (identity, grafana_role) = crate::observability::grafana_proxy_headers(&user);
    if let Ok(v) = HeaderValue::from_str(&identity) {
        headers.insert(HeaderName::from_static("x-webauth-user"), v);
    }
    headers.insert(
        HeaderName::from_static("x-webauth-role"),
        HeaderValue::from_static(grafana_role.as_str()),
    );

    // --- read the request body (bounded) ----------------------------------
    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return json_error(StatusCode::BAD_REQUEST, "failed to read request body");
        }
    };

    // --- forward to the upstream, bounded by the 10s timeout --------------
    let resp = upstream_client()
        .request(parts.method.clone(), &url)
        .headers(headers)
        .body(body_bytes)
        .send()
        .await;

    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            // Never 500, never hang: timeout → 503, connect/other → 502.
            let (status, label) = if e.is_timeout() {
                (StatusCode::SERVICE_UNAVAILABLE, "timed out")
            } else {
                (StatusCode::BAD_GATEWAY, "unreachable")
            };
            warn!(error = %e, %url, "grafana upstream {label}");
            return json_error(status, "observability backend temporarily unavailable");
        }
    };

    // --- relay the upstream status, headers, and body back to the client --
    let status = resp.status();
    let upstream_headers = resp.headers().clone();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, %url, "grafana upstream body read failed");
            return json_error(
                StatusCode::BAD_GATEWAY,
                "observability backend response was truncated",
            );
        }
    };

    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| json_error(StatusCode::BAD_GATEWAY, "invalid upstream response").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_dashboard_slugs_to_views() {
        assert_eq!(
            request_catalog_view("d/service-overview/service-overview", None),
            Some(CatalogView::ServiceOverview)
        );
        assert_eq!(
            request_catalog_view("api/dashboards/uid/abc", Some("dashboard=trace-explorer")),
            Some(CatalogView::TraceExplorer)
        );
        assert_eq!(
            request_catalog_view("d/anomalies-alerts/x", None),
            Some(CatalogView::AnomaliesAlerts)
        );
    }

    #[test]
    fn generic_assets_are_not_view_gated() {
        assert_eq!(request_catalog_view("public/build/app.js", None), None);
        assert_eq!(request_catalog_view("api/health", None), None);
        assert_eq!(request_catalog_view("", None), None);
    }

    #[test]
    fn pins_tenant_when_absent() {
        match pin_tenant_query(Some("from=now-1h&to=now"), "acme") {
            TenantPin::Pinned(q) => {
                assert!(q.contains("from=now-1h"));
                assert!(q.contains("to=now"));
                assert!(q.contains("var-tenant=acme"));
            }
            TenantPin::Widened => panic!("should not be widened"),
        }
    }

    #[test]
    fn pins_tenant_when_query_empty() {
        match pin_tenant_query(None, "u-bob") {
            TenantPin::Pinned(q) => assert_eq!(q, "var-tenant=u-bob"),
            TenantPin::Widened => panic!("should not be widened"),
        }
    }

    #[test]
    fn accepts_matching_tenant() {
        assert_eq!(
            pin_tenant_query(Some("var-tenant=acme&from=now-6h"), "acme"),
            TenantPin::Pinned("var-tenant=acme&from=now-6h".to_string())
        );
    }

    #[test]
    fn rejects_widening_to_other_tenant() {
        assert_eq!(
            pin_tenant_query(Some("var-tenant=other-corp"), "acme"),
            TenantPin::Widened
        );
        // Even when one value matches, any mismatch is a widening attempt.
        assert_eq!(
            pin_tenant_query(Some("var-tenant=acme&var-tenant=other"), "acme"),
            TenantPin::Widened
        );
    }

    #[test]
    fn hop_by_hop_and_webauth_detection() {
        assert!(is_hop_by_hop(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop(&HeaderName::from_static("content-length")));
        assert!(!is_hop_by_hop(&HeaderName::from_static("cookie")));
        assert!(is_webauth(&HeaderName::from_static("x-webauth-user")));
        assert!(is_webauth(&HeaderName::from_static("x-webauth-role")));
        assert!(!is_webauth(&HeaderName::from_static("x-real-ip")));
    }
}
