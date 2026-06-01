//! Job queue. Workers use the admin pool to `claim_next` across all
//! tenants; the claimed job carries its tenant_id, which the handler
//! uses to scope every subsequent query.

use crate::PgStore;
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
        let mut tx = self.begin_for(tenant).await?;
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
        .execute(&mut *tx)
        .await
        .context("enqueue job")?;
        tx.commit().await?;
        Ok(())
    }

    /// Claim the next due job across all tenants. Uses the admin pool so
    /// workers see every tenant's queue. The returned job carries
    /// tenant_id, which the handler uses to scope its work.
    pub async fn claim_next_job(&self, worker_id: &str, lease_ms: i64) -> Result<Option<Job>> {
        let now = Utc::now();
        let lease_until = now + chrono::Duration::milliseconds(lease_ms);
        let row = sqlx::query(
            "UPDATE jobs SET \
                 state = 'running', \
                 locked_by = $1, \
                 locked_until = $2, \
                 attempt = attempt + 1, \
                 updated_at = $3 \
             WHERE id = ( \
                 SELECT id FROM jobs \
                 WHERE state = 'queued' AND next_run_at <= $3 \
                 ORDER BY next_run_at ASC \
                 FOR UPDATE SKIP LOCKED LIMIT 1 \
             ) \
             RETURNING id, tenant_id, kind, payload, state, attempt, max_attempts, \
                       next_run_at, locked_by, locked_until, last_error, created_at, updated_at",
        )
        .bind(worker_id)
        .bind(lease_until)
        .bind(now)
        .fetch_optional(self.pool())
        .await
        .context("claim_next_job")?;
        row.map(row_to_job).transpose()
    }

    pub async fn complete_job(&self, id: &JobId) -> Result<()> {
        sqlx::query("UPDATE jobs SET state = 'done', updated_at = now() WHERE id::text = $1")
            .bind(id.as_str())
            .execute(self.pool())
            .await
            .context("complete_job")?;
        Ok(())
    }

    /// Record a job failure. With `retry_in_ms`, the job re-queues unless it
    /// has exhausted `max_attempts`, in which case it goes `dead`. Returns
    /// `true` when the job is now permanently `dead` (so the caller can mark
    /// the underlying source `failed`).
    pub async fn fail_job(
        &self,
        id: &JobId,
        err: &str,
        retry_in_ms: Option<i64>,
    ) -> Result<bool> {
        let now = Utc::now();
        if let Some(delay) = retry_in_ms {
            let next = now + chrono::Duration::milliseconds(delay);
            let row = sqlx::query(
                "UPDATE jobs SET \
                     state = CASE WHEN attempt >= max_attempts THEN 'dead' ELSE 'queued' END, \
                     last_error = $1, \
                     next_run_at = $2, \
                     locked_by = NULL, \
                     locked_until = NULL, \
                     updated_at = $3 \
                 WHERE id::text = $4 \
                 RETURNING state",
            )
            .bind(err)
            .bind(next)
            .bind(now)
            .bind(id.as_str())
            .fetch_one(self.pool())
            .await
            .context("fail_job with retry")?;
            let state: String = row.get("state");
            Ok(state == "dead")
        } else {
            sqlx::query(
                "UPDATE jobs SET state = 'dead', last_error = $1, updated_at = $2 \
                 WHERE id::text = $3",
            )
            .bind(err)
            .bind(now)
            .bind(id.as_str())
            .execute(self.pool())
            .await
            .context("fail_job dead")?;
            Ok(true)
        }
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
