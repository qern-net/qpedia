//! Integration tests for the authenticating Grafana reverse proxy
//! (`/api/v1/observability/grafana/*path`) — task 11.3.
//!
//! The proxy is the *sole* ingress to the internal-only `otel-lgtm` Grafana
//! instance, so it carries the entire identity-trust security contract. These
//! tests exercise that contract on two levels:
//!
//! ## 1. Pure-logic tests (always run, deterministic, no DB)
//!
//! The handler's security decisions are factored into pure, public helpers in
//! `qpedia_api::observability_proxy` (`request_catalog_view`, `pin_tenant_query`)
//! and `qpedia_api::observability` (the effective-view / Grafana-role / permitted
//! -view resolver). The handler is a thin sequencing of exactly these decisions:
//!
//! ```text
//! let resolution = resolve_user(&user);
//! if let Some(view) = request_catalog_view(path, query) {
//!     if !resolution.permitted_views.contains(&view) { return 403; }   // out-of-view
//! }
//! if resolution.view != Superadmin {
//!     match pin_tenant_query(query, tenant) {                          // tenant pinning
//!         Widened => return 403, Pinned(q) => forward(q),
//!     }
//! }
//! inject X-WEBAUTH-USER = identity, X-WEBAUTH-ROLE = grafana_role        // header inject
//! ```
//!
//! So testing those helpers (composed the same way the handler composes them)
//! is real coverage of the 403-gating, 403-widening and header-mapping contract
//! without needing a live authenticated session. These run everywhere.
//!
//! ## 2. Live end-to-end tests (gated on `QPEDIA_DB_URL`, skipped otherwise)
//!
//! Constructing the real `grafana_proxy` handler requires a full `AppState`,
//! which requires a `PgStore` (there is no DB-less constructor). When a test
//! Postgres is available we build the real router, point the upstream at a
//! local mock (an axum server on an ephemeral port that echoes the headers it
//! received), and assert the full handler behavior end-to-end:
//!   - unauthenticated request → 401, **nothing forwarded** (mock sees 0 hits);
//!   - authenticated request reaches the upstream with the injected
//!     `X-WEBAUTH-USER` / `X-WEBAUTH-ROLE` and any client `X-WEBAUTH-*` stripped;
//!   - non-superadmin tenant-widening attempt → 403, nothing forwarded;
//!   - out-of-permitted-view request → 403, nothing forwarded;
//!   - upstream down → 502/503 (never 500, never hangs) with the router still
//!     answering subsequent requests.
//!
//! This mirrors the established repo pattern (`qpedia-pg-store/tests/smoke.rs`),
//! which skips when `QPEDIA_DB_URL` is unset rather than failing.
//!
//! Requirements: 10.5, 10.6, 10.8, 10.9, 10.11.

use qpedia_api::observability::{
    grafana_proxy_headers, resolve_view, CatalogView, EffectiveView, GrafanaRole,
};
use qpedia_api::observability_proxy::{pin_tenant_query, request_catalog_view, TenantPin};
use qpedia_api::User;
use qpedia_core::tenant::Tenant;

// ---------------------------------------------------------------------------
// Pure-logic tests — the security-contract decisions the handler sequences.
// ---------------------------------------------------------------------------

fn user(email: Option<&str>, groups: &[&str], tenant: &str) -> User {
    User {
        id: "uid-test".into(),
        email: email.map(str::to_string),
        name: None,
        groups: groups.iter().map(|s| s.to_string()).collect(),
        tenant: Tenant::new(tenant),
    }
}

/// The handler only view-gates a request when `request_catalog_view` maps the
/// path/query to a concrete dashboard. Each of the seven catalog slugs must
/// resolve to its view; generic UI/API assets resolve to `None` (not gated),
/// so the proxy can serve the assets any dashboard needs. (Req 10.8)
#[test]
fn request_catalog_view_maps_dashboards_and_ignores_generic_assets() {
    let cases = [
        ("d/service-overview/abc", CatalogView::ServiceOverview),
        ("d/logs-explorer/abc", CatalogView::LogsExplorer),
        ("d/trace-explorer/abc", CatalogView::TraceExplorer),
        ("d/db-datastore-performance/abc", CatalogView::DbDatastorePerformance),
        ("d/ingestion-job-queue/abc", CatalogView::IngestionJobQueue),
        ("d/dependency-health/abc", CatalogView::DependencyHealth),
        ("d/anomalies-alerts/abc", CatalogView::AnomaliesAlerts),
    ];
    for (path, expected) in cases {
        assert_eq!(
            request_catalog_view(path, None),
            Some(expected),
            "{path} should map to {expected:?}"
        );
    }
    // A dashboard can also be selected via a query parameter.
    assert_eq!(
        request_catalog_view("api/dashboards/uid/x", Some("dashboard=trace-explorer")),
        Some(CatalogView::TraceExplorer)
    );
    // Generic assets / API calls are not a specific dashboard → not view-gated.
    for generic in ["public/build/app.js", "api/health", "api/datasources", ""] {
        assert_eq!(request_catalog_view(generic, None), None, "{generic:?} not gated");
    }
}

/// Out-of-permitted-view → 403. A `member` may only see Service Overview and
/// Ingestion & Job Queue; a request mapping to any other dashboard is gated
/// (the handler returns 403 because `permitted_views` does not contain it).
/// `admin` and `superadmin` permit all seven, so they are never gated. (Req 10.8)
#[test]
fn out_of_permitted_view_is_gated_per_role() {
    // Member: the dashboards NOT in their permitted set must be rejected.
    let member = EffectiveView::Member;
    let permitted = member.permitted_views();
    assert!(permitted.contains(&CatalogView::ServiceOverview));
    assert!(permitted.contains(&CatalogView::IngestionJobQueue));
    for denied in [
        CatalogView::LogsExplorer,
        CatalogView::TraceExplorer,
        CatalogView::DbDatastorePerformance,
        CatalogView::DependencyHealth,
        CatalogView::AnomaliesAlerts,
    ] {
        assert!(
            !permitted.contains(&denied),
            "member must be gated out of {denied:?}"
        );
    }

    // Compose exactly as the handler does: map a path to a view, then check
    // membership in the caller's permitted set. A member hitting trace-explorer
    // is denied; a member hitting service-overview is allowed.
    let denied_view = request_catalog_view("d/trace-explorer/x", None).unwrap();
    assert!(!member.permitted_views().contains(&denied_view));
    let allowed_view = request_catalog_view("d/service-overview/x", None).unwrap();
    assert!(member.permitted_views().contains(&allowed_view));

    // Admin and superadmin permit all seven (never gated).
    for view in [EffectiveView::Admin, EffectiveView::Superadmin] {
        assert_eq!(view.permitted_views().len(), 7, "{view:?} sees all seven");
        for cv in [
            CatalogView::ServiceOverview,
            CatalogView::LogsExplorer,
            CatalogView::TraceExplorer,
            CatalogView::DbDatastorePerformance,
            CatalogView::IngestionJobQueue,
            CatalogView::DependencyHealth,
            CatalogView::AnomaliesAlerts,
        ] {
            assert!(view.permitted_views().contains(&cv));
        }
    }
}

/// Non-superadmin tenant pinning → widening attempt = 403. The handler pins
/// `var-tenant` to the caller's own tenant for every non-superadmin view; any
/// value that differs is a widening attempt the handler turns into a 403.
/// (Req 10.8 cross-tenant denial)
#[test]
fn tenant_widening_is_rejected_and_matching_is_pinned() {
    // Widening: a different tenant value, or any mismatched value among many.
    assert_eq!(
        pin_tenant_query(Some("var-tenant=other-corp"), "acme"),
        TenantPin::Widened
    );
    assert_eq!(
        pin_tenant_query(Some("var-tenant=acme&var-tenant=other"), "acme"),
        TenantPin::Widened
    );

    // Matching tenant: forwarded unchanged.
    assert_eq!(
        pin_tenant_query(Some("var-tenant=acme&from=now-6h"), "acme"),
        TenantPin::Pinned("var-tenant=acme&from=now-6h".to_string())
    );

    // Absent: the proxy appends the pin so the dashboard is always scoped.
    match pin_tenant_query(Some("from=now-1h"), "acme") {
        TenantPin::Pinned(q) => {
            assert!(q.contains("from=now-1h"));
            assert!(q.contains("var-tenant=acme"));
        }
        TenantPin::Widened => panic!("absent var-tenant must be pinned, not widened"),
    }
    match pin_tenant_query(None, "u-bob") {
        TenantPin::Pinned(q) => assert_eq!(q, "var-tenant=u-bob"),
        TenantPin::Widened => panic!("empty query must be pinned, not widened"),
    }
}

/// Superadmin is never tenant-pinned: the handler forwards the query as-is for
/// the superadmin effective view, so a superadmin can legitimately request any
/// tenant. The pure resolver places a superadmin in that branch regardless of
/// group/tenant. (Req 10.6 superadmin all-tenant)
#[test]
fn superadmin_view_skips_tenant_pinning() {
    assert_eq!(resolve_view(true, true, true), EffectiveView::Superadmin);
    assert_eq!(resolve_view(true, false, false), EffectiveView::Superadmin);
    // Non-superadmin views are the ones that get pinned.
    assert_ne!(resolve_view(false, true, false), EffectiveView::Superadmin); // admin
    assert_ne!(resolve_view(false, true, true), EffectiveView::Superadmin); // owner
    assert_ne!(resolve_view(false, false, false), EffectiveView::Superadmin); // member
}

/// Injected `X-WEBAUTH-ROLE` value mapping: superadmin/admin → `Admin`,
/// owner → `Editor`, member → `Viewer` — the exact strings the handler writes
/// into the header. (Req 10.6)
#[test]
fn grafana_role_header_values_are_mapped_per_view() {
    assert_eq!(EffectiveView::Superadmin.grafana_role().as_str(), "Admin");
    assert_eq!(EffectiveView::Admin.grafana_role().as_str(), "Admin");
    assert_eq!(EffectiveView::Owner.grafana_role().as_str(), "Editor");
    assert_eq!(EffectiveView::Member.grafana_role().as_str(), "Viewer");
    assert_eq!(GrafanaRole::Admin.as_str(), "Admin");
    assert_eq!(GrafanaRole::Editor.as_str(), "Editor");
    assert_eq!(GrafanaRole::Viewer.as_str(), "Viewer");
}

/// Injected `X-WEBAUTH-USER` identity: the verified email when present, else
/// the stable user id. This value is env-independent (the role half reads the
/// superadmin allowlist; the identity half does not), so it is asserted here
/// without touching process env. (Req 10.6)
#[test]
fn grafana_proxy_header_identity_prefers_email_then_id() {
    let (ident, _role) = grafana_proxy_headers(&user(Some("alice@x.com"), &["member"], "acme"));
    assert_eq!(ident, "alice@x.com");

    let (ident, _role) = grafana_proxy_headers(&user(None, &["member"], "acme"));
    assert_eq!(ident, "uid-test");
}

// ---------------------------------------------------------------------------
// Live end-to-end tests — gated on a real Postgres via QPEDIA_DB_URL.
// ---------------------------------------------------------------------------

mod live {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::routing::any;
    use axum::Router;
    use tower::ServiceExt; // for `oneshot`

    use qpedia_api::auth::hash_token;
    use qpedia_api::observability_proxy::grafana_proxy;
    use qpedia_api::{AppState, AuthMode, AuthState, ChatRateLimiter, Extensions};
    use qpedia_core::tenant::Tenant;
    use qpedia_ingest::IngestContext;
    use qpedia_pg_store::PgStore;
    use qpedia_store::{blob::BlobStore, WikiRepoStore};

    const PROXY_PREFIX: &str = "/api/v1/observability/grafana";

    /// Unique-enough suffix for temp dirs / tenant ids within one run.
    fn uniq() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{}-{}", std::process::id(), n)
    }

    /// Shared state for the mock upstream: a hit counter and the headers seen
    /// on the most recent request (so we can assert injection + stripping).
    #[derive(Clone, Default)]
    struct MockState {
        hits: Arc<AtomicUsize>,
        last_headers: Arc<Mutex<Option<HeaderMap>>>,
    }

    async fn mock_echo(State(st): State<MockState>, headers: HeaderMap) -> &'static str {
        st.hits.fetch_add(1, Ordering::SeqCst);
        *st.last_headers.lock().unwrap() = Some(headers);
        "upstream-ok"
    }

    /// Spawn a local mock upstream on an ephemeral port. Returns its
    /// `http://127.0.0.1:<port>` base URL and the shared `MockState`.
    async fn spawn_mock_upstream() -> (String, MockState) {
        let state = MockState::default();
        let app = Router::new()
            .fallback(any(mock_echo))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock upstream");
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{}", addr.port()), state)
    }

    /// Build a real `AppState` around a connected `PgStore`. Only the DB is
    /// load-bearing for the proxy; the blob/wiki/extractor/reranker/embedder
    /// components are constructed cheaply (the reranker is lazy and is never
    /// invoked by the proxy).
    fn build_state(db: PgStore, mode: AuthMode) -> AppState {
        let base = std::env::temp_dir().join(format!("qpedia-proxy-test-{}", uniq()));
        let blob = BlobStore::open(base.join("raw")).expect("open blob store");
        let wiki = WikiRepoStore::new(base.join("wiki"), "test-bot", "bot@test.local");
        let extractors = Arc::new(qpedia_extract::ExtractorRegistry::with_default());
        let reranker = qpedia_embed::reranker_from_env(base.join("models"));
        let ctx = IngestContext::new(db, blob, wiki, extractors, None, None, reranker);
        AppState {
            ctx,
            auth: AuthState { mode, firebase: None, service: None, oauth: None },
            extensions: Extensions::default(),
            chat_rate_limiter: Arc::new(ChatRateLimiter::from_env()),
        }
    }

    /// Mount only the proxy route (mirrors how `core_router` registers it) so
    /// we exercise the real `grafana_proxy` handler in isolation.
    fn proxy_router(state: AppState) -> Router {
        Router::new()
            .route(&format!("{PROXY_PREFIX}/*path"), any(grafana_proxy))
            .with_state(state)
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    /// Resolve the test DB DSN; `None` means "skip" (matching the repo's
    /// `pg-store/tests/smoke.rs` convention).
    fn test_db_url() -> Option<String> {
        std::env::var("QPEDIA_DB_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Full end-to-end security contract against a real router + mock upstream.
    ///
    /// All scenarios live in one test so this single test owns the
    /// process-global `QPEDIA_GRAFANA_UPSTREAM` env var (read per-request by the
    /// handler) without racing parallel tests.
    ///
    /// Requirements: 10.5, 10.6, 10.8, 10.9, 10.11.
    #[tokio::test]
    async fn live_proxy_security_contract() {
        let Some(url) = test_db_url() else {
            eprintln!(
                "skipping live_proxy_security_contract: set QPEDIA_DB_URL to a test \
                 Postgres to run the end-to-end proxy tests"
            );
            return;
        };

        let db = PgStore::connect(&url)
            .await
            .expect("connect + migrate test Postgres");

        // A clean, non-allowlisted superadmin set so dev_admin / the member are
        // resolved as ordinary (non-superadmin) callers and tenant pinning is
        // exercised. (Other tests in this binary do not depend on this var.)
        std::env::set_var("QPEDIA_ADMIN_EMAILS", "");

        let (upstream, mock) = spawn_mock_upstream().await;

        // -----------------------------------------------------------------
        // 1. Unauthenticated → 401, nothing forwarded.
        //    Session auth mode + no cookie: the `User` extractor rejects before
        //    the handler body runs, so the upstream is never contacted.
        // -----------------------------------------------------------------
        std::env::set_var("QPEDIA_GRAFANA_UPSTREAM", &upstream);
        {
            let app = proxy_router(build_state(db.clone(), AuthMode::Session));
            let before = mock.hits.load(Ordering::SeqCst);
            let res = app
                .oneshot(get(&format!("{PROXY_PREFIX}/api/health")))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "no session must yield 401"
            );
            assert_eq!(
                mock.hits.load(Ordering::SeqCst),
                before,
                "401 must forward nothing to the upstream"
            );
        }

        // -----------------------------------------------------------------
        // 2. Authenticated (dev_admin) → reaches upstream with injected
        //    identity/role; client-supplied X-WEBAUTH-* headers are stripped.
        //    Use a NON-dashboard path so view-gating does not apply.
        // -----------------------------------------------------------------
        {
            let app = proxy_router(build_state(db.clone(), AuthMode::Dev));
            let req = Request::builder()
                .method("GET")
                .uri(format!("{PROXY_PREFIX}/api/datasources"))
                // Spoofed identity/role a client should never be able to set.
                .header("x-webauth-user", "attacker@evil.com")
                .header("x-webauth-role", "Admin")
                .header("x-webauth-anything", "nope")
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "authenticated request forwards");

            let seen = mock.last_headers.lock().unwrap().clone().expect("upstream saw a request");
            // dev_admin → email admin@dev.local, admin group, org tenant → Admin.
            assert_eq!(
                seen.get("x-webauth-user").map(|v| v.to_str().unwrap()),
                Some("admin@dev.local"),
                "proxy must inject the QPEDIA identity"
            );
            assert_eq!(
                seen.get("x-webauth-role").map(|v| v.to_str().unwrap()),
                Some("Admin"),
                "proxy must inject the mapped Grafana role"
            );
            // The spoofed values must be gone (exactly one value, the injected one).
            assert_eq!(
                seen.get_all("x-webauth-user").iter().count(),
                1,
                "client X-WEBAUTH-USER must be stripped, not appended"
            );
            assert!(
                !seen
                    .get_all("x-webauth-user")
                    .iter()
                    .any(|v| v.to_str().unwrap() == "attacker@evil.com"),
                "spoofed X-WEBAUTH-USER must be stripped"
            );
            assert!(
                seen.get("x-webauth-anything").is_none(),
                "all client X-WEBAUTH-* headers must be stripped"
            );
        }

        // -----------------------------------------------------------------
        // 3. Non-superadmin tenant-widening attempt → 403, nothing forwarded.
        //    dev_admin (org tenant "default") requesting a different tenant.
        // -----------------------------------------------------------------
        {
            let app = proxy_router(build_state(db.clone(), AuthMode::Dev));
            let before = mock.hits.load(Ordering::SeqCst);
            let res = app
                .oneshot(get(&format!(
                    "{PROXY_PREFIX}/api/datasources?var-tenant=other-corp"
                )))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::FORBIDDEN,
                "cross-tenant widening must be 403"
            );
            assert_eq!(
                mock.hits.load(Ordering::SeqCst),
                before,
                "widening 403 must forward nothing"
            );
        }

        // -----------------------------------------------------------------
        // 4. Out-of-permitted-view → 403 for a `member` session; a permitted
        //    view forwards with the Viewer role injected.
        // -----------------------------------------------------------------
        {
            let tenant = Tenant::new(format!("proxytest-org-{}", uniq()));
            db.upsert_tenant(&tenant, "Proxy Test Org", None)
                .await
                .expect("seed tenant");

            let token = format!("tok-{}", uniq());
            let email = "member@proxytest.example";
            qpedia_api::mint_session(
                &db,
                &hash_token(&token),
                &tenant,
                "member:1",
                Some(email),
                None,
                Some("test"),
                &["member".to_string()],
                3600,
            )
            .await
            .expect("mint member session");

            let cookie = format!("qpedia_session={token}");

            // 4a. Denied view (trace-explorer is not in a member's set) → 403.
            let app = proxy_router(build_state(db.clone(), AuthMode::Session));
            let before = mock.hits.load(Ordering::SeqCst);
            let req = Request::builder()
                .method("GET")
                .uri(format!("{PROXY_PREFIX}/d/trace-explorer/x"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::FORBIDDEN,
                "member must be gated out of trace-explorer"
            );
            assert_eq!(
                mock.hits.load(Ordering::SeqCst),
                before,
                "out-of-view 403 must forward nothing"
            );

            // 4b. Permitted view (service-overview) → forwards with Viewer role.
            let app = proxy_router(build_state(db.clone(), AuthMode::Session));
            let req = Request::builder()
                .method("GET")
                .uri(format!("{PROXY_PREFIX}/d/service-overview/x"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::OK,
                "member may see service-overview"
            );
            let seen = mock.last_headers.lock().unwrap().clone().unwrap();
            assert_eq!(
                seen.get("x-webauth-user").map(|v| v.to_str().unwrap()),
                Some(email)
            );
            assert_eq!(
                seen.get("x-webauth-role").map(|v| v.to_str().unwrap()),
                Some("Viewer"),
                "member maps to the Grafana Viewer role"
            );
        }

        // -----------------------------------------------------------------
        // 5. Upstream down → 502/503 (never 500, never hang); the router keeps
        //    serving. Point the upstream at a closed port so the connect fails
        //    fast and deterministically.
        // -----------------------------------------------------------------
        std::env::set_var("QPEDIA_GRAFANA_UPSTREAM", "http://127.0.0.1:1");
        {
            let app = proxy_router(build_state(db.clone(), AuthMode::Dev));
            let res = tokio::time::timeout(
                Duration::from_secs(20),
                app.oneshot(get(&format!("{PROXY_PREFIX}/api/health"))),
            )
            .await
            .expect("proxy must not hang on a dead upstream")
            .unwrap();
            assert!(
                matches!(
                    res.status(),
                    StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
                ),
                "dead upstream must yield 502/503, got {}",
                res.status()
            );

            // The app is still healthy: a second request is answered promptly
            // (no panic, no hang) with the same bounded error.
            let app2 = proxy_router(build_state(db.clone(), AuthMode::Dev));
            let res2 = tokio::time::timeout(
                Duration::from_secs(20),
                app2.oneshot(get(&format!("{PROXY_PREFIX}/api/datasources"))),
            )
            .await
            .expect("router must keep serving after an upstream failure")
            .unwrap();
            assert!(
                matches!(
                    res2.status(),
                    StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
                ),
                "router must still answer after upstream failure, got {}",
                res2.status()
            );
        }

        std::env::remove_var("QPEDIA_GRA