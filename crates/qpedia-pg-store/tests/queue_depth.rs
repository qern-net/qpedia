//! DB-gated integration test for the `jobs.queue.depth` source query (task 6.8,
//! queue-depth half).
//!
//! Gated on `QPEDIA_DB_URL` (skips with a `skip:` note when unset, matching
//! `smoke.rs`). Asserts that `PgStore::pending_job_counts_by_kind` — the query
//! the `jobs.queue.depth` gauge sampler records from (app.rs
//! `spawn_queue_depth_sampler`) — reflects jobs that have been enqueued and are
//! still pending (`state = 'queued'`), grouped by kind (Req 5.8).
//!
//! The query is intentionally cross-tenant (it samples the whole queue for the
//! gauge), so the assertions are lower-bounds (`>=`): a shared CI database may
//! carry queued jobs from other tenants/tests, but never fewer than the ones
//! this test just enqueued.

use chrono::Utc;
use qpedia_core::{
    job::{Job, JobKind, JobState},
    tenant::Tenant,
    JobId,
};
use qpedia_pg_store::PgStore;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_tenant() -> Tenant {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    Tenant::new(format!("ci-qd-{nanos:032}"))
}

fn queued_job(tenant: &Tenant, kind: JobKind) -> Job {
    let now = Utc::now();
    Job {
        id: JobId::new(),
        kind,
        payload: json!({ "tenant": tenant.as_str() }),
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 5,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn pending_counts_reflect_enqueued_jobs() {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        eprintln!("skip: QPEDIA_DB_URL not set (CI: pgvector service container expected)");
        return;
    };

    let db = PgStore::connect(&url).await.expect("connect + migrate");
    let tenant = unique_tenant();
    db.upsert_tenant(&tenant, "QD Tenant", None)
        .await
        .expect("upsert tenant");

    // Enqueue a known mix: 3 Ingest + 2 Lint, all left in `queued`.
    for _ in 0..3 {
        db.enqueue(&tenant, &queued_job(&tenant, JobKind::Ingest))
            .await
            .expect("enqueue ingest");
    }
    for _ in 0..2 {
        db.enqueue(&tenant, &queued_job(&tenant, JobKind::Lint))
            .await
            .expect("enqueue lint");
    }

    let counts = db
        .pending_job_counts_by_kind()
        .await
        .expect("pending_job_counts_by_kind");

    let get = |k: &str| counts.iter().find(|(kind, _)| kind == k).map(|(_, n)| *n).unwrap_or(0);

    // Lower bounds: at least the jobs this test enqueued are pending by kind.
    assert!(get("ingest") >= 3, "expected >=3 queued ingest jobs, got {}", get("ingest"));
    assert!(get("lint") >= 2, "expected >=2 queued lint jobs, got {}", get("lint"));
    assert!(
        counts.iter().all(|(_, n)| *n >= 0),
        "counts must be non-negative"
    );
}
