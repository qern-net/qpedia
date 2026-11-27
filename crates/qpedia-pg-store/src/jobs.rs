//! Job queue. Workers use the admin pool to `claim_next` across all
//! tenants; the claimed job carries its tenant_id, which the handler
//! uses to scope every subsequent query.

use crate::{with_db_span, DbSystem, PgStore};
use anyhow::{Context, Result};
use chrono::Utc;
use qpedia_core::{
    job::{Job, JobKind, JobState},
    tenant::Tenant,
    JobId,
};
use serde_json::{json, Value};
use sqlx::Row;

impl PgStore {
    pub async fn enqueue(&self, tenant: &Tenant, job: &Job) -> Result<()> {
        with_db_span(DbSystem::Postgresql, "enqueue_job", async move {
            let mut tx = self.begin_for(tenant).await?;
            Self::enqueue_in(&mut tx, tenant, job).await?;
            tx.commit().await?;
            Ok(())
        }).await
    }

    pub(crate) async fn enqueue_in(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tenant: &Tenant, job: &Job) -> Result<()> {
        sqlx::query(
                "INSERT INTO jobs \
                 (tenant_id, kind, payload, state, attempt, max_attempts, next_run_at, last_error, created_at, updated_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
            )
            .bind(tenant.as_str())
            .bind(kind_str(&job.kind))
            .bind(&job.payload)
            .bind(state_str(&job.state))
            .bind(job.attempt as i32)
            .bind(job.max_attempts as i32)
            .bind(job.next_run_at)
            .bind(job.last_error.as_deref())
            .bind(job.created_at)
            .bind(job.updated_at)
            .execute(&mut **tx)
            .await
            .context("enqueue job")?;
        Ok(())
    }

    /// Check the capability of a restricted connection before starting a
    /// worker. Cross-tenant access is limited to the durable job table.
    pub async fn verify_job_worker_role(&self) -> Result<()> {
        let allowed: bool = sqlx::query_scalar(
            "WITH policies AS (
                 SELECT policyname, cmd, permissive, qual, with_check,
                     EXISTS (
                         SELECT FROM unnest(roles) AS role(name)
                         WHERE CASE WHEN role.name = 'public' THEN true
                                    ELSE pg_has_role(current_user, role.name, 'USAGE')
                               END
                     ) AS worker_policy,
                     EXISTS (
                         SELECT FROM unnest(roles) AS role(name)
                         WHERE CASE WHEN role.name = 'public' THEN true
                                    ELSE pg_has_role('qpedia_app', role.name, 'USAGE')
                               END
                     ) AS tenant_policy
                 FROM pg_policies
                 WHERE schemaname = 'public' AND tablename = 'jobs'
             )
             SELECT has_table_privilege(current_user, 'public.jobs', 'SELECT')
                AND has_table_privilege(current_user, 'public.jobs', 'UPDATE')
                AND EXISTS (
                    SELECT FROM pg_class
                    WHERE oid = 'public.jobs'::regclass AND relrowsecurity
                      AND NOT pg_has_role('qpedia_app', relowner, 'USAGE')
                )
                AND EXISTS (
                    SELECT FROM pg_roles WHERE rolname = 'qpedia_app'
                      AND NOT (rolsuper OR rolbypassrls)
                )
                AND EXISTS (
                    SELECT FROM policies WHERE tenant_policy AND permissive = 'PERMISSIVE'
                      AND policyname = 'tenant_isolation' AND cmd = 'ALL'
                )
                AND NOT EXISTS (
                    SELECT FROM policies WHERE tenant_policy AND permissive = 'PERMISSIVE'
                      AND (
                          policyname <> 'tenant_isolation'
                          OR qual IS DISTINCT FROM '(tenant_id = current_setting(''qpedia.tenant''::text, true))'
                          OR with_check IS DISTINCT FROM qual
                      )
                )
                AND EXISTS (
                    SELECT FROM policies WHERE worker_policy AND NOT tenant_policy
                      AND permissive = 'PERMISSIVE'
                      AND cmd IN ('ALL', 'SELECT') AND coalesce(qual, 'true') = 'true'
                )
                AND EXISTS (
                    SELECT FROM policies WHERE worker_policy AND NOT tenant_policy
                      AND permissive = 'PERMISSIVE'
                      AND cmd IN ('ALL', 'UPDATE') AND coalesce(qual, 'true') = 'true'
                      AND coalesce(with_check, qual, 'true') = 'true'
                )
                AND NOT EXISTS (
                    SELECT FROM policies WHERE worker_policy AND permissive = 'RESTRICTIVE'
                      AND cmd IN ('ALL', 'SELECT', 'UPDATE')
                )",
        )
        .fetch_one(self.pool())
        .await
        .context("verify restricted worker queue access")?;
        anyhow::ensure!(
            allowed,
            "jobs require the tenant isolation policy and worker-only cross-tenant SELECT and UPDATE policies"
        );
        Ok(())
    }

    /// Claim the next due job across all tenants. Uses the worker pool so
    /// workers see every tenant's queue. The returned job carries
    /// tenant_id, which the handler uses to scope its work.
    pub async fn claim_next_job(&self, worker_id: &str, lease_ms: i64) -> Result<Option<Job>> {
        with_db_span(DbSystem::Postgresql, "claim_next_job", async move {
            // Use the database clock for claims and renewals, including
            // reclaiming an expired job from another worker.
            let row = sqlx::query(
                "WITH tick AS MATERIALIZED (SELECT clock_timestamp() AS ts)
                 UPDATE jobs AS claimed SET
                     state = 'running',
                     locked_by = $1,
                     locked_until = tick.ts + $2 * interval '1 millisecond',
                     attempt = claimed.attempt + 1,
                     updated_at = tick.ts
                 FROM tick
                 WHERE claimed.id = (
                     SELECT id FROM jobs
                     WHERE (state = 'queued' AND next_run_at <= tick.ts)
                        OR (state = 'running' AND locked_until <= tick.ts)
                     ORDER BY next_run_at ASC
                     FOR UPDATE SKIP LOCKED LIMIT 1
                 )
                 RETURNING claimed.*",
            )
            .bind(worker_id)
            .bind(lease_ms as f64)
            .fetch_optional(self.pool())
            .await
            .context("claim_next_job")?;
            row.map(row_to_job).transpose()
        })
        .await
    }

    /// Renew only the live attempt. The attempt number distinguishes claims
    /// even when more than one process uses the same worker name.
    pub async fn renew_job_claim(&self, id: &JobId, attempt: u32, lease_ms: i64) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE jobs SET locked_until = clock_timestamp() + $3 * interval '1 millisecond'
             WHERE id::text = $1 AND attempt = $2 AND state = 'running'
               AND locked_until > clock_timestamp()",
        )
        .bind(id.as_str())
        .bind(attempt as i32)
        .bind(lease_ms as f64)
        .execute(self.pool())
        .await
        .context("renew job claim")?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn complete_job(&self, id: &JobId) -> Result<()> {
        self.complete_with_claim(id, None).await?;
        Ok(())
    }

    pub async fn complete_job_claim(&self, id: &JobId, attempt: u32) -> Result<bool> {
        self.complete_with_claim(id, Some(attempt as i32)).await
    }

    async fn complete_with_claim(&self, id: &JobId, attempt: Option<i32>) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE jobs SET state = 'done', updated_at = now(), locked_by = NULL, locked_until = NULL
             WHERE id::text = $1 AND ($2::integer IS NULL OR
                 (attempt = $2 AND state = 'running' AND locked_until > clock_timestamp()))",
        )
        .bind(id.as_str())
        .bind(attempt)
        .execute(self.pool())
        .await
        .context("complete job")?;
        Ok(result.rows_affected() == 1)
    }

    /// Record a failed job, returning whether retries are exhausted.
    pub async fn fail_job(
        &self,
        id: &JobId,
        err: &str,
        retry_in_ms: Option<i64>,
    ) -> Result<bool> {
        let state = self.fail_with_claim(id, err, retry_in_ms, None).await?;
        state.or_else(|| retry_in_ms.is_none().then_some(true)).context("job no longer exists")
    }

    /// `None` indicates that the claim has expired or been superseded.
    pub async fn fail_job_claim(
        &self,
        id: &JobId,
        attempt: u32,
        err: &str,
        retry_in_ms: Option<i64>,
    ) -> Result<Option<bool>> {
        self.fail_with_claim(id, err, retry_in_ms, Some(attempt as i32)).await
    }

    async fn fail_with_claim(
        &self,
        id: &JobId,
        err: &str,
        retry_in_ms: Option<i64>,
        attempt: Option<i32>,
    ) -> Result<Option<bool>> {
        let now = Utc::now();
        let next = retry_in_ms.map(|ms| now + chrono::Duration::milliseconds(ms));
        let state: Option<String> = sqlx::query_scalar(
            "UPDATE jobs SET
                 state = CASE WHEN $2::timestamptz IS NULL OR attempt >= max_attempts
                              THEN 'dead' ELSE 'queued' END,
                 last_error = $1, next_run_at = coalesce($2, next_run_at),
                 locked_by = NULL, locked_until = NULL, updated_at = $3
             WHERE id::text = $4 AND ($5::integer IS NULL OR
                 (attempt = $5 AND state = 'running' AND locked_until > clock_timestamp()))
             RETURNING state",
        )
        .bind(err)
        .bind(next)
        .bind(now)
        .bind(id.as_str())
        .bind(attempt)
        .fetch_optional(self.pool())
        .await
        .context("fail job")?;
        Ok(state.map(|s| s == "dead"))
    }

    /// Count currently-pending (`queued`) jobs grouped by kind, across all
    /// tenants (admin pool, like [`claim_next_job`](Self::claim_next_job)).
    ///
    /// Drives the `jobs.queue.depth` gauge (Req 5.8): completion-time signals
    /// (`jobs.completed`, `Job_Span`) cannot express *current* depth, so this
    /// is sampled periodically by the queue-depth sampler. Routed through
    /// [`with_db_span`] so the sample is itself an inspectable datastore op.
    pub async fn pending_job_counts_by_kind(&self) -> Result<Vec<(String, i64)>> {
        with_db_span(DbSystem::Postgresql, "pending_job_counts_by_kind", async move {
            let rows = sqlx::query(
                "SELECT kind, count(*)::bigint AS n FROM jobs WHERE state = 'queued' GROUP BY kind",
            )
            .fetch_all(self.pool())
            .await
            .context("pending_job_counts_by_kind")?;
            Ok(rows
                .iter()
                .map(|r| (r.get::<String, _>("kind"), r.get::<i64, _>("n")))
                .collect())
        })
        .await
    }

    /// Snapshot of the job queue for the tenant: counts by state, the
    /// currently active jobs (running first, with their worker + source +
    /// age), and recent dead jobs with their error. Drives the Admin queue
    /// view. Tenant-scoped via RLS.
    pub async fn queue_overview(&self, tenant: &Tenant) -> Result<Value> {
        let mut tx = self.begin_for(tenant).await?;

        let by_state = sqlx::query("SELECT state, count(*)::bigint AS n FROM jobs GROUP BY state")
            .fetch_all(&mut *tx)
            .await
            .context("queue by_state")?;
        let mut states = serde_json::Map::new();
        for r in &by_state {
            states.insert(r.get::<String, _>("state"), json!(r.get::<i64, _>("n")));
        }

        // Running first (the live processors), then the queued backlog.
        let active = sqlx::query(
            "SELECT id, kind, state, locked_by, \
                    payload->>'source_id' AS source_id, \
                    EXTRACT(EPOCH FROM (now() - updated_at))::bigint AS age_secs \
             FROM jobs WHERE state IN ('running', 'queued') \
             ORDER BY (state = 'running') DESC, updated_at ASC \
             LIMIT 100",
        )
        .fetch_all(&mut *tx)
        .await
        .context("queue active")?;
        let active: Vec<Value> = active
            .iter()
            .map(|r| {
                json!({
                    "id": r.get::<i64, _>("id").to_string(),
                    "kind": r.get::<String, _>("kind"),
                    "state": r.get::<String, _>("state"),
                    "worker": r.try_get::<Option<String>, _>("locked_by").ok().flatten(),
                    "source": r.try_get::<Option<String>, _>("source_id").ok().flatten(),
                    "age_secs": r.get::<i64, _>("age_secs"),
                })
            })
            .collect();

        let dead = sqlx::query(
            "SELECT id, kind, payload->>'source_id' AS source_id, last_error \
             FROM jobs WHERE state = 'dead' ORDER BY updated_at DESC LIMIT 20",
        )
        .fetch_all(&mut *tx)
        .await
        .context("queue dead")?;
        let dead: Vec<Value> = dead
            .iter()
            .map(|r| {
                json!({
                    "id": r.get::<i64, _>("id").to_string(),
                    "kind": r.get::<String, _>("kind"),
                    "source": r.try_get::<Option<String>, _>("source_id").ok().flatten(),
                    "error": r.try_get::<Option<String>, _>("last_error").ok().flatten(),
                })
            })
            .collect();

        tx.commit().await.ok();
        Ok(json!({ "by_state": states, "active": active, "dead": dead }))
    }
}

fn kind_str(k: &JobKind) -> String {
    serde_json::to_string(k)
        .unwrap_or_else(|_| "\"ingest\"".into())
        .trim_matches('"')
        .to_string()
}
fn state_str(s: &JobState) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"queued\"".into())
        .trim_matches('"')
        .to_string()
}

fn row_to_job(row: sqlx::postgres::PgRow) -> Result<Job> {
    let id: i64 = row.try_get("id")?;
    let kind_s: String = row.try_get("kind")?;
    let state_s: String = row.try_get("state")?;
    let kind: JobKind = serde_json::from_str(&format!("\"{kind_s}\""))
        .context("parse job kind")?;
    let state: JobState = serde_json::from_str(&format!("\"{state_s}\""))
        .context("parse job state")?;
    Ok(Job {
        id: JobId::from(id.to_string()),
        kind,
        payload: row.try_get("payload")?,
        state,
        attempt: row.try_get::<i32, _>("attempt")? as u32,
        max_attempts: row.try_get::<i32, _>("max_attempts")? as u32,
        next_run_at: row.try_get("next_run_at")?,
        last_error: row.try_get("last_error").ok(),
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

// Keep Tenant import alive (referenced in argument types above).
#[allow(dead_code)]
fn _force_use(_: &Tenant) {}
