use anyhow::{ensure, Context, Result};
use chrono::Utc;
use qpedia_core::{
    job::{Job, JobKind, JobState},
    source::{Source, SourceStatus},
    tenant::Tenant,
    JobId, SourceId,
};
use qpedia_pg_store::{PgStore, SchemaMode};
use serde_json::json;
use sqlx::PgPool;
use url::Url;

#[tokio::test]
async fn worker_claims_and_request_policy_remain_separate() -> Result<()> {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        return Ok(());
    };
    let server = PgPool::connect(&url).await?;
    let suffix = ulid::Ulid::new().to_string().to_ascii_lowercase();
    let database = format!("qpedia_worker_test_{suffix}");
    let worker = format!("qpedia_test_worker_{suffix}");
    let inherited = format!("qpedia_test_group_{suffix}");
    sqlx::query(&format!("CREATE DATABASE {database}"))
        .execute(&server)
        .await?;
    let mut owner_url = Url::parse(&url)?;
    owner_url.set_path(&database);

    let result = check_fixture(&owner_url, &worker, &inherited).await;
    sqlx::query(&format!("DROP DATABASE {database} WITH (FORCE)"))
        .execute(&server)
        .await?;
    sqlx::query(&format!("DROP ROLE IF EXISTS {worker}, {inherited}"))
        .execute(&server)
        .await?;
    result
}

async fn check_fixture(owner_url: &Url, worker: &str, inherited: &str) -> Result<()> {
    let admin = PgStore::connect_with_schema(owner_url.as_str(), SchemaMode::Migrate).await?;
    let owner = admin.pool();
    sqlx::raw_sql(&format!(
        "CREATE ROLE {worker} LOGIN NOINHERIT PASSWORD 'fixture-only';
         CREATE ROLE {inherited} NOLOGIN;
         GRANT qpedia_app TO {worker} WITH INHERIT FALSE;
         REVOKE CREATE ON SCHEMA public FROM PUBLIC;
         GRANT USAGE ON SCHEMA public TO {worker};
         GRANT SELECT ON _sqlx_migrations TO {worker};
         GRANT SELECT, UPDATE ON jobs TO {worker};
         CREATE POLICY worker_access ON jobs FOR ALL TO {worker} USING (true) WITH CHECK (true);"
    ))
    .execute(owner)
    .await?;

    let mut url = owner_url.clone();
    url.set_username(worker)
        .map_err(|_| anyhow::anyhow!("invalid test URL user"))?;
    url.set_password(Some("fixture-only"))
        .map_err(|_| anyhow::anyhow!("invalid test URL password"))?;
    let store = PgStore::connect_with_schema(url.as_str(), SchemaMode::Verify).await?;
    store.verify_job_worker_role().await?;

    sqlx::raw_sql(
        "INSERT INTO tenants (id, display_name) VALUES ('worker-test', 'Fixture');
         INSERT INTO jobs (tenant_id, kind, payload, state, next_run_at)
         VALUES ('worker-test', 'ingest', '{}', 'queued', now());",
    )
    .execute(owner)
    .await?;
    let first = store
        .claim_next_job("fixture-worker", 60_000)
        .await?
        .context("initial claim")?;
    assert!(
        store
            .renew_job_claim(&first.id, first.attempt, 120_000)
            .await?
    );
    assert!(store
        .claim_next_job("another-worker", 60_000)
        .await?
        .is_none());

    // Reclaim immediately after simulated expiry, then ensure the stale
    // attempt can no longer renew, finish, or reschedule this job.
    sqlx::query("UPDATE jobs SET locked_until = clock_timestamp() - interval '1 second'")
        .execute(owner)
        .await?;
    let next = store
        .claim_next_job("fixture-worker", 60_000)
        .await?
        .context("reclaimed job")?;
    assert_eq!(next.attempt, first.attempt + 1);
    assert!(
        !store
            .renew_job_claim(&first.id, first.attempt, 120_000)
            .await?
    );
    assert!(!store.complete_job_claim(&first.id, first.attempt).await?);
    assert!(store
        .fail_job_claim(&first.id, first.attempt, "stale", Some(0))
        .await?
        .is_none());
    assert!(store.complete_job_claim(&next.id, next.attempt).await?);

    check_ingest_handoff(&store, owner).await?;

    // These are configuration checks against an empty fixture role, not
    // tenant queries. A broad grant must never qualify as worker-only.
    for role in ["PUBLIC", "qpedia_app", inherited] {
        if role == inherited {
            sqlx::query(&format!(
                "GRANT {inherited} TO qpedia_app WITH INHERIT TRUE"
            ))
            .execute(owner)
            .await?;
        }
        sqlx::query(&format!(
            "CREATE POLICY review_fixture ON jobs FOR SELECT TO {role} USING (true)"
        ))
        .execute(owner)
        .await?;
        ensure!(
            store.verify_job_worker_role().await.is_err(),
            "tenant-facing grant should be rejected"
        );
        sqlx::query("DROP POLICY review_fixture ON jobs")
            .execute(owner)
            .await?;
    }
    store.verify_job_worker_role().await?;
    Ok(())
}

async fn check_ingest_handoff(store: &PgStore, owner: &PgPool) -> Result<()> {
    let tenant = Tenant::new("worker-test");
    let id = SourceId::from("handoff".to_owned());
    let original = Source {
        id: id.clone(),
        tenant: tenant.clone(),
        folder_path: "/".into(),
        filename: "handoff.txt".into(),
        mime: "text/plain".into(),
        sha256: "a".repeat(64),
        size_bytes: 1,
        acl: Default::default(),
        status: SourceStatus::Pending,
        language: None,
        created_at: Utc::now(),
        ingested_at: None,
        classification: None,
    };
    store.insert_source(&original).await?;
    store
        .enqueue(&tenant, &ingest_job(&tenant, &id, None))
        .await?;
    let mut replacement = original.clone();
    replacement.sha256 = "b".repeat(64);
    let mut upload = store
        .begin_source_upload(&replacement, Some(&id))
        .await?
        .context("lock existing upload")?;
    assert!(upload.replacement_ready().await?);
    // An old job can be claimed while the upload holds the source lock.
    // Its handler must skip the new head even if its payload has no hash.
    let old = store
        .claim_next_job("first-worker", 60_000)
        .await?
        .context("claim original ingest during replacement")?;
    upload
        .commit(
            &replacement,
            &ingest_job(&tenant, &id, Some(&replacement.sha256)),
        )
        .await?;
    assert!(
        !store
            .start_claimed_source_ingest(&tenant, &id, &old.id, old.attempt, None)
            .await?
    );
    assert_eq!(
        store
            .fail_ingest_job_claim(&tenant, &id, &old.id, old.attempt, "old job failed", None)
            .await?,
        Some(false),
    );
    let head = store
        .get_source_in(&tenant, &id)
        .await?
        .context("get replacement")?;
    assert_eq!(head.sha256, replacement.sha256);
    assert_eq!(head.status, SourceStatus::Pending);

    let current = store
        .claim_next_job("second-worker", 60_000)
        .await?
        .context("claim replacement ingest")?;
    assert!(
        store
            .start_claimed_source_ingest(
                &tenant,
                &id,
                &current.id,
                current.attempt,
                Some(&replacement.sha256),
            )
            .await?
    );
    let mut upload = store
        .begin_source_upload(&original, Some(&id))
        .await?
        .context("lock active source for replacement")?;
    assert!(!upload.replacement_ready().await?);
    assert!(upload
        .commit(&original, &ingest_job(&tenant, &id, Some(&original.sha256)))
        .await
        .is_err());

    store
        .enqueue(&tenant, &ingest_job(&tenant, &id, None))
        .await?;
    assert!(store
        .claim_next_job("first-worker", 60_000)
        .await?
        .is_none());
    // If persisting the failed status errors, the preceding job must still be
    // running and the successor must remain blocked.
    sqlx::query("ALTER TABLE sources ADD CONSTRAINT reject_failed CHECK (status <> 'failed')")
        .execute(owner)
        .await?;
    assert!(store
        .fail_ingest_job_claim(
            &tenant,
            &id,
            &current.id,
            current.attempt,
            "exhausted",
            Some(0),
        )
        .await
        .is_err());
    assert!(store
        .claim_next_job("first-worker", 60_000)
        .await?
        .is_none());
    sqlx::query("ALTER TABLE sources DROP CONSTRAINT reject_failed")
        .execute(owner)
        .await?;
    assert_eq!(
        store
            .fail_ingest_job_claim(
                &tenant,
                &id,
                &current.id,
                current.attempt,
                "exhausted",
                Some(0)
            )
            .await?,
        Some(true),
    );
    let successor = store
        .claim_next_job("first-worker", 60_000)
        .await?
        .context("claim successor after failure commits")?;
    let head = store
        .get_source_in(&tenant, &id)
        .await?
        .context("get failed head")?;
    assert_eq!(head.sha256, replacement.sha256);
    assert_eq!(head.status, SourceStatus::Failed);
    assert!(
        store
            .complete_job_claim(&successor.id, successor.attempt)
            .await?
    );
    Ok(())
}

fn ingest_job(tenant: &Tenant, id: &SourceId, sha: Option<&str>) -> Job {
    let now = Utc::now();
    let mut payload = json!({"source_id": id, "tenant": tenant});
    if let Some(sha) = sha {
        payload["upload_sha256"] = json!(sha);
    }
    Job {
        id: JobId::new(),
        kind: JobKind::Ingest,
        payload,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 1,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}
