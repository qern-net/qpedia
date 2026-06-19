//! Config smoke tests for the OTel-LGTM observability surface (task 14.3).
//!
//! These are *config* tests, not runtime tests: they parse the repo-root
//! `docker-compose.yml` and the Grafana dashboard JSON catalog and assert the
//! invariants the spec pins down (internal-only networking, pinned image tag,
//! auth-proxy env, named volume, healthcheck timings, and the seven-dashboard
//! Observability Views Catalog).
//!
//! Why a Rust integration test: it lives next to the engine, runs in CI under
//! `cargo test`, and fails loudly if someone re-introduces a host port binding,
//! bumps the image to `latest`, or drops a dashboard / `tenant` variable.
//!
//! Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 1.7, 8.3, 8.4, 8.5, 8.6, 8.7, 8.8,
//! 10.7, 10.9.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use serde_yaml::Value as Yaml;

/// The workspace root is two levels up from this crate's manifest dir
/// (`crates/qpedia-api` -> repo root).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root from CARGO_MANIFEST_DIR")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn compose_raw() -> String {
    read(&repo_root().join("docker-compose.yml"))
}

fn compose_yaml() -> Yaml {
    serde_yaml::from_str(&compose_raw()).expect("docker-compose.yml is valid YAML")
}

/// docker-compose.yml with comment content stripped, so raw-text guards inspect
/// *actual config* only. The file's INTERNAL-ONLY documentation block
/// deliberately mentions the removed `127.0.0.1:3000:3000` binding in prose, and
/// that mention must not trip the "no host binding" assertions.
fn compose_code_only() -> String {
    compose_raw()
        .lines()
        .map(|line| match line.find('#') {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fetch `services.<name>` as a YAML mapping value.
fn service<'a>(doc: &'a Yaml, name: &str) -> &'a Yaml {
    doc.get("services")
        .and_then(|s| s.get(name))
        .unwrap_or_else(|| panic!("services.{name} is missing"))
}

/// Read an `environment:` value as a string (compose env values are scalars).
fn env_str<'a>(svc: &'a Yaml, key: &str) -> Option<&'a str> {
    svc.get("environment")
        .and_then(|e| e.get(key))
        .and_then(|v| v.as_str())
}

// ----------------------------------------------------------------------------
// docker-compose.yml smoke tests
// ----------------------------------------------------------------------------

/// Req 1.2: the otel-lgtm image is pinned to an explicit, non-`latest` tag.
#[test]
fn otel_lgtm_image_is_pinned_non_latest() {
    let doc = compose_yaml();
    let image = service(&doc, "otel-lgtm")
        .get("image")
        .and_then(Yaml::as_str)
        .expect("otel-lgtm.image present");

    assert!(
        image.starts_with("grafana/otel-lgtm:"),
        "expected grafana/otel-lgtm image, got {image:?}"
    );
    let tag = image
        .rsplit_once(':')
        .map(|(_, t)| t)
        .expect("image has an explicit :tag");
    assert!(!tag.is_empty(), "image tag must not be empty: {image:?}");
    assert_ne!(tag, "latest", "image tag must be pinned, not `latest`");
}

/// Req 1.1, 1.3, 10.7, 10.9: the otel-lgtm service publishes NO host ports.
/// 4317/4318 (OTLP) and 3000 (Grafana) are reachable to siblings only via
/// `expose:`. The qpedia-api authenticating reverse proxy is the sole ingress
/// to Grafana, which is what makes the auth-proxy header-trust safe.
#[test]
fn otel_lgtm_is_internal_only_no_host_ports() {
    let doc = compose_yaml();
    let svc = service(&doc, "otel-lgtm");

    // Hard structural invariant: there is no `ports:` key at all.
    assert!(
        svc.get("ports").is_none(),
        "otel-lgtm must NOT declare a `ports:` host binding (internal-only via expose)"
    );

    // `expose:` advertises the three internal ports to sibling services.
    let expose: Vec<String> = svc
        .get("expose")
        .and_then(Yaml::as_sequence)
        .expect("otel-lgtm.expose present")
        .iter()
        .map(|v| match v {
            Yaml::String(s) => s.clone(),
            Yaml::Number(n) => n.to_string(),
            other => panic!("unexpected expose entry: {other:?}"),
        })
        .collect();
    for port in ["4317", "4318", "3000"] {
        assert!(
            expose.iter().any(|e| e == port),
            "expected expose to contain {port}, got {expose:?}"
        );
    }

    // Belt-and-suspenders against the *prior* host bindings creeping back in.
    // Assert against the comment-stripped config text so a quoted host mapping
    // anywhere in the file is caught, while the INTERNAL-ONLY documentation
    // block (which mentions the removed bindings in prose) does not false-trip.
    let raw = compose_code_only();
    assert!(
        !raw.contains("127.0.0.1:3000"),
        "the removed `127.0.0.1:3000` Grafana host binding must not be present"
    );
    assert!(
        !raw.contains("127.0.0.1:4317") && !raw.contains("127.0.0.1:4318"),
        "OTLP ports 4317/4318 must not be published to the host"
    );
    // No `0.0.0.0` / wide host mapping of the OTLP or Grafana ports either.
    for mapping in ["4317:4317", "4318:4318", "3000:3000"] {
        assert!(
            !raw.contains(mapping),
            "host port mapping {mapping:?} must not be published for otel-lgtm"
        );
    }
}

/// Req 10.7, 10.9: Grafana auth-proxy env makes the QPEDIA session the only
/// identity source and serves Grafana from the proxied sub-path.
#[test]
fn otel_lgtm_auth_proxy_env_is_configured() {
    let doc = compose_yaml();
    let svc = service(&doc, "otel-lgtm");

    assert_eq!(env_str(svc, "GF_AUTH_PROXY_ENABLED"), Some("true"));
    assert_eq!(
        env_str(svc, "GF_AUTH_PROXY_HEADER_NAME"),
        Some("X-WEBAUTH-USER")
    );
    // Role-header mapping must carry the X-WEBAUTH-ROLE header.
    let headers = env_str(svc, "GF_AUTH_PROXY_HEADERS")
        .expect("GF_AUTH_PROXY_HEADERS present");
    assert!(
        headers.contains("X-WEBAUTH-ROLE"),
        "GF_AUTH_PROXY_HEADERS must map the role header, got {headers:?}"
    );
    assert_eq!(env_str(svc, "GF_AUTH_DISABLE_LOGIN_FORM"), Some("true"));
    assert_eq!(env_str(svc, "GF_SERVER_SERVE_FROM_SUB_PATH"), Some("true"));
    assert_eq!(env_str(svc, "GF_SECURITY_ALLOW_EMBEDDING"), Some("true"));
}

/// Req 1.5: a named `otel-data` volume exists and is mounted at /data.
#[test]
fn otel_data_named_volume_exists_and_is_mounted() {
    let doc = compose_yaml();

    // Top-level named volume declared.
    assert!(
        doc.get("volumes")
            .and_then(|v| v.get("otel-data"))
            .is_some(),
        "top-level `volumes:` must declare `otel-data`"
    );

    // Service mounts it at /data.
    let mounts: Vec<String> = service(&doc, "otel-lgtm")
        .get("volumes")
        .and_then(Yaml::as_sequence)
        .expect("otel-lgtm.volumes present")
        .iter()
        .filter_map(Yaml::as_str)
        .map(str::to_string)
        .collect();
    assert!(
        mounts.iter().any(|m| m.starts_with("otel-data:/data")),
        "otel-lgtm must mount otel-data at /data, got {mounts:?}"
    );
}

/// Req 1.4: healthcheck timings are pinned (interval 10s, timeout 5s,
/// start_period 30s, retries 5).
#[test]
fn otel_lgtm_healthcheck_values() {
    let doc = compose_yaml();
    let hc = service(&doc, "otel-lgtm")
        .get("healthcheck")
        .expect("otel-lgtm.healthcheck present");

    assert_eq!(hc.get("interval").and_then(Yaml::as_str), Some("10s"));
    assert_eq!(hc.get("timeout").and_then(Yaml::as_str), Some("5s"));
    assert_eq!(hc.get("start_period").and_then(Yaml::as_str), Some("30s"));
    // retries is a scalar number.
    assert_eq!(hc.get("retries").and_then(Yaml::as_u64), Some(5));
}

/// Req 1.7: the app -> otel-lgtm health `depends_on` contract.
///
/// NOTE: this OSS compose intentionally has NO live `app` service — the app
/// image lives in the deployment overlay (see the file header). The
/// `depends_on: { otel-lgtm: { condition: service_healthy } }` +
/// `OTEL_EXPORTER_OTLP_ENDPOINT` contract is therefore documented as a COMMENT
/// block for the overlay's app service. So this test asserts the *documented
/// contract* is present in the file text rather than looking for a live
/// service that deliberately doesn't exist here.
#[test]
fn app_health_depends_on_contract_is_documented() {
    let doc = compose_yaml();
    assert!(
        doc.get("services").and_then(|s| s.get("app")).is_none(),
        "this OSS compose must not define a live `app` service (it lives in the overlay)"
    );

    let raw = compose_raw();
    // The commented overlay contract must spell out the health dependency...
    assert!(
        raw.contains("depends_on:"),
        "the documented overlay app contract must include depends_on:"
    );
    assert!(
        raw.contains("condition: service_healthy"),
        "the documented overlay app contract must gate startup on service_healthy"
    );
    // ...and the OTLP endpoint wiring on the internal compose network.
    assert!(
        raw.contains("OTEL_EXPORTER_OTLP_ENDPOINT") && raw.contains("http://otel-lgtm:4317"),
        "the documented overlay app contract must wire OTEL_EXPORTER_OTLP_ENDPOINT to the collector"
    );
}

// ----------------------------------------------------------------------------
// Grafana dashboard catalog smoke tests
// ----------------------------------------------------------------------------

fn dashboards_dir() -> PathBuf {
    repo_root()
        .join("observability")
        .join("grafana")
        .join("dashboards")
}

fn load_dashboard(file: &str) -> Json {
    let path = dashboards_dir().join(file);
    serde_json::from_str(&read(&path)).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The seven-dashboard Observability Views Catalog: (filename, uid, title).
const CATALOG: &[(&str, &str, &str)] = &[
    (
        "service-overview.json",
        "qpedia-service-overview",
        "Service Overview",
    ),
    ("logs-explorer.json", "qpedia-logs-explorer", "Logs Explorer"),
    (
        "trace-explorer.json",
        "qpedia-trace-explorer",
        "Trace Explorer",
    ),
    (
        "db-datastore-performance.json",
        "qpedia-db-datastore-performance",
        "DB / Datastore Performance",
    ),
    (
        "ingestion-job-queue-performance.json",
        "qpedia-ingestion-job-queue",
        "Ingestion & Job Queue Performance",
    ),
    (
        "dependency-health.json",
        "qpedia-dependency-health",
        "Dependency Health",
    ),
    (
        "anomalies-and-alerts.json",
        "qpedia-anomalies-alerts",
        "Anomalies & Alerts",
    ),
];

/// Collect all panel titles from a dashboard.
fn panel_titles(dash: &Json) -> Vec<String> {
    dash.get("panels")
        .and_then(Json::as_array)
        .map(|panels| {
            panels
                .iter()
                .filter_map(|p| p.get("title").and_then(Json::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Collect all target `expr` strings from a dashboard's panels.
fn panel_exprs(dash: &Json) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(panels) = dash.get("panels").and_then(Json::as_array) {
        for p in panels {
            if let Some(targets) = p.get("targets").and_then(Json::as_array) {
                for t in targets {
                    if let Some(e) = t.get("expr").and_then(Json::as_str) {
                        out.push(e.to_string());
                    }
                }
            }
        }
    }
    out
}

fn has_panel_containing(dash: &Json, needle: &str) -> bool {
    let needle = needle.to_lowercase();
    panel_titles(dash)
        .iter()
        .any(|t| t.to_lowercase().contains(&needle))
}

/// Req 8.3-8.8: all seven catalog dashboards are present with the correct
/// uid + title, and every dashboard declares a `tenant` template variable.
#[test]
fn all_seven_dashboards_present_with_tenant_variable() {
    for (file, uid, title) in CATALOG {
        let dash = load_dashboard(file);

        assert_eq!(
            dash.get("uid").and_then(Json::as_str),
            Some(*uid),
            "{file}: unexpected uid"
        );
        assert_eq!(
            dash.get("title").and_then(Json::as_str),
            Some(*title),
            "{file}: unexpected title"
        );

        let vars = dash
            .get("templating")
            .and_then(|t| t.get("list"))
            .and_then(Json::as_array)
            .unwrap_or_else(|| panic!("{file}: templating.list missing"));
        let has_tenant = vars.iter().any(|v| {
            v.get("name").and_then(Json::as_str) == Some("tenant")
        });
        assert!(
            has_tenant,
            "{file}: must declare a `tenant` template variable"
        );
    }
}

/// Req 8.3: Service Overview carries the required HTTP + dependency-health
/// panels.
#[test]
fn service_overview_required_panels() {
    let dash = load_dashboard("service-overview.json");

    assert!(
        has_panel_containing(&dash, "request rate"),
        "Service Overview needs an HTTP request-rate panel"
    );
    assert!(
        has_panel_containing(&dash, "error rate"),
        "Service Overview needs an error-rate panel"
    );
    assert!(
        has_panel_containing(&dash, "duration"),
        "Service Overview needs a request-duration (p50/p95/p99) panel"
    );
    assert!(
        has_panel_containing(&dash, "dependency health"),
        "Service Overview needs a dependency-health summary panel"
    );

    // The duration panel should reference the http server duration histogram
    // at the three quantiles.
    let exprs = panel_exprs(&dash).join("\n");
    assert!(
        exprs.contains("http_server_request_duration_milliseconds"),
        "Service Overview must query the http server duration metric"
    );
    for q in ["0.50", "0.95", "0.99"] {
        assert!(
            exprs.contains(&format!("histogram_quantile({q}")),
            "Service Overview must chart the p{} quantile",
            q.trim_start_matches("0.")
        );
    }
}

/// Req 8.7: Ingestion & Job Queue Performance carries job throughput, failure
/// counts, and the queue-depth panel sourced from `jobs.queue.depth`.
#[test]
fn ingestion_job_queue_required_panels() {
    let dash = load_dashboard("ingestion-job-queue-performance.json");

    assert!(
        has_panel_containing(&dash, "throughput"),
        "Ingestion dashboard needs a job-throughput panel"
    );
    assert!(
        has_panel_containing(&dash, "failure"),
        "Ingestion dashboard needs a job-failure-counts panel"
    );
    assert!(
        has_panel_containing(&dash, "queue depth"),
        "Ingestion dashboard needs a queue-depth panel"
    );

    let exprs = panel_exprs(&dash).join("\n");
    assert!(
        exprs.contains("jobs_queue_depth"),
        "queue-depth panel must read the jobs.queue.depth gauge"
    );
    assert!(
        exprs.contains("jobs_completed_total"),
        "throughput/failure panels must read the jobs.completed counter"
    );
}

/// Req 8.7: Dependency Health renders per-dependency health over the time
/// range via the dependency.up gauge.
#[test]
fn dependency_health_required_panels() {
    let dash = load_dashboard("dependency-health.json");

    let exprs = panel_exprs(&dash).join("\n");
    assert!(
        exprs.contains("dependency_up"),
        "Dependency Health must read the dependency.up gauge"
    );
    // A per-dependency timeline over the dashboard range (grouped by dependency).
    assert!(
        exprs.contains("by (dependency)"),
        "Dependency Health must break down by dependency"
    );
}

/// Req 8.8: Anomalies & Alerts carries threshold alert rules (NOT ML).
#[test]
fn anomalies_and_alerts_uses_threshold_rules_not_ml() {
    let dash = load_dashboard("anomalies-and-alerts.json");

    let panels = dash
        .get("panels")
        .and_then(Json::as_array)
        .expect("anomalies dashboard has panels");

    // Threshold evaluator types only — these are classic Grafana threshold
    // alert rules, not anomaly/ML detectors.
    const THRESHOLD_EVALS: &[&str] =
        &["gt", "lt", "within_range", "outside_range", "no_value"];

    let mut alert_count = 0usize;
    for p in panels {
        if let Some(alert) = p.get("alert") {
            alert_count += 1;
            let conditions = alert
                .get("conditions")
                .and_then(Json::as_array)
                .expect("alert has conditions");
            assert!(!conditions.is_empty(), "alert must have >=1 condition");
            for c in conditions {
                let eval_type = c
                    .get("evaluator")
                    .and_then(|e| e.get("type"))
                    .and_then(Json::as_str)
                    .expect("condition has an evaluator type");
                assert!(
                    THRESHOLD_EVALS.contains(&eval_type),
                    "alert evaluator {eval_type:?} must be a threshold type, not ML"
                );
                // Threshold evaluators carry numeric params (the threshold).
                let params = c
                    .get("evaluator")
                    .and_then(|e| e.get("params"))
                    .and_then(Json::as_array)
                    .expect("threshold evaluator has params");
                assert!(
                    params.iter().any(Json::is_number),
                    "threshold evaluator must carry a numeric threshold"
                );
            }
        }
    }
    assert!(
        alert_count >= 1,
        "Anomalies & Alerts must define at least one threshold alert rule"
    );

    // Guard against ML-style detectors sneaking in: no ML/forecast datasources
    // or panel types anywhere in the dashboard JSON.
    let raw = read(&dashboards_dir().join("anomalies-and-alerts.json")).to_lowercase();
    for ml_marker in ["machine learning", "\"forecast\"", "anomaly detection"] {
        assert!(
            !raw.contains(ml_marker),
            "Anomalies & Alerts must be threshold-based, found ML marker {ml_marker:?}"
        );
    }
}
