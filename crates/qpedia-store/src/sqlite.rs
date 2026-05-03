//! SQLite-backed persistence: jobs, sources, audit. See DESIGN.md §2.5.
//!
//! Uses runtime queries (no compile-time `query!` macro) to avoid the
//! DATABASE_URL build-time requirement. Speed is irrelevant at our scale.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use qpedia_core::{
    acl::Acl,
    job::{Job, JobKind, JobState},
    source::{Source, SourceStatus},
    Error, JobId, Result, SourceId,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Connect and run migrations. `path` is a filesystem path, e.g.
    /// `/data/sqlite/qpedia.db`. Creates the file if missing.
    pub async fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("sqlite connect: {e}")))?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("migrate: {e}")))?;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

// ---------- Source CRUD ----------

#[async_trait]
pub trait SourceStore: Send + Sync {
    async fn insert_source(&self, src: &Source) -> Result<()>;
    async fn get_source(&self, id: &SourceId) -> Result<Option<Source>>;
    async fn list_sources(&self, folder_prefix: &str, limit: i64) -> Result<Vec<Source>>;
    async fn update_status(&self, id: &SourceId, status: SourceStatus) -> Result<()>;
    async fn update_language(&self, id: &SourceId, language: &str) -> Result<()>;
    async fn update_classification(&self, id: &SourceId, classification: &serde_json::Value) -> Result<()>;
    async fn delete_source(&self, id: &SourceId) -> Result<()>;
}

#[async_trait]
impl SourceStore for SqliteStore {
    async fn insert_source(&self, src: &Source) -> Result<()> {
        let acl_json = serde_json::to_string(&src.acl)?;
        let status = serde_json::to_string(&src.status)?;
        // serde_json wraps in quotes for unit-like enums; strip them.
        let status = status.trim_matches('"').to_string();

        let classification_json = src.classification.as_ref().map(|v| v.to_string());
        sqlx::query(
            "INSERT INTO sources \
             (id, folder_path, filename, mime, sha256, size_bytes, acl_json, status, language, created_at, ingested_at, classification_json) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(src.id.as_str())
        .bind(&src.folder_path)
        .bind(&src.filename)
        .bind(&src.mime)
        .bind(&src.sha256)
        .bind(src.size_bytes as i64)
        .bind(acl_json)
        .bind(status)
        .bind(src.language.as_deref())
        .bind(src.created_at.timestamp_millis())
        .bind(src.ingested_at.map(|t| t.timestamp_millis()))
        .bind(classification_json)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn get_source(&self, id: &SourceId) -> Result<Option<Source>> {
        let row = sqlx::query("SELECT * FROM sources WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx)?;
        row.map(row_to_source).transpose()
    }

    async fn list_sources(&self, folder_prefix: &str, limit: i64) -> Result<Vec<Source>> {
        let rows = sqlx::query(
            "SELECT * FROM sources WHERE folder_path LIKE ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(format!("{folder_prefix}%"))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx)?;
        rows.into_iter().map(row_to_source).collect()
    }

    async fn update_status(&self, id: &SourceId, status: SourceStatus) -> Result<()> {
        let s = serde_json::to_string(&status)?;
        let s = s.trim_matches('"').to_string();
        sqlx::query("UPDATE sources SET status = ? WHERE id = ?")
            .bind(s)
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    async fn update_language(&self, id: &SourceId, language: &str) -> Result<()> {
        sqlx::query("UPDATE sources SET language = ? WHERE id = ?")
            .bind(language)
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    async fn update_classification(&self, id: &SourceId, classification: &serde_json::Value) -> Result<()> {
        sqlx::query("UPDATE sources SET classification_json = ? WHERE id = ?")
            .bind(classification.to_string())
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    async fn delete_source(&self, id: &SourceId) -> Result<()> {
        sqlx::query("DELETE FROM sources WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }
}

// ---------- Auth: sessions + OIDC pending state ----------

#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub groups: Vec<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct PendingAuth {
    pub pkce_verifier: String,
    pub nonce: String,
    pub redirect_after: Option<String>,
}

impl SqliteStore {
    pub async fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        email: Option<&str>,
        name: Option<&str>,
        groups: &[String],
        ttl_secs: i64,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let expires = now + ttl_secs * 1000;
        sqlx::query(
            "INSERT OR REPLACE INTO sessions \
             (token_hash, user_id, user_email, user_name, groups_json, expires_at, created_at) \
             VALUES (?,?,?,?,?,?,?)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(email)
        .bind(name)
        .bind(serde_json::to_string(groups)?)
        .bind(expires)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    pub async fn lookup_session(&self, token_hash: &str) -> Result<Option<Session>> {
        let now = Utc::now().timestamp_millis();
        let row = sqlx::query(
            "SELECT user_id, user_email, user_name, groups_json, expires_at \
             FROM sessions WHERE token_hash = ? AND expires_at > ?",
        )
        .bind(token_hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let Some(row) = row else { return Ok(None) };

        let groups_json: String = row.try_get("groups_json").map_err(map_sqlx)?;
        let groups: Vec<String> = serde_json::from_str(&groups_json)?;
        let expires_ms: i64 = row.try_get("expires_at").map_err(map_sqlx)?;

        Ok(Some(Session {
            user_id: row.try_get("user_id").map_err(map_sqlx)?,
            email: row.try_get("user_email").map_err(map_sqlx)?,
            name: row.try_get("user_name").map_err(map_sqlx)?,
            groups,
            expires_at: ms_to_dt(expires_ms),
        }))
    }

    pub async fn delete_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    pub async fn save_pending(
        &self,
        state: &str,
        pkce_verifier: &str,
        nonce: &str,
        redirect_after: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT OR REPLACE INTO auth_pending \
             (state, pkce_verifier, nonce, redirect_after, created_at) \
             VALUES (?,?,?,?,?)",
        )
        .bind(state)
        .bind(pkce_verifier)
        .bind(nonce)
        .bind(redirect_after)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    /// Atomically read-and-delete a pending row. Returns None if missing
    /// or older than 10 minutes.
    pub async fn take_pending(&self, state: &str) -> Result<Option<PendingAuth>> {
        let cutoff = Utc::now().timestamp_millis() - 10 * 60 * 1000;
        let row = sqlx::query(
            "DELETE FROM auth_pending WHERE state = ? AND created_at >= ? \
             RETURNING pkce_verifier, nonce, redirect_after",
        )
        .bind(state)
        .bind(cutoff)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(PendingAuth {
            pkce_verifier: row.try_get("pkce_verifier").map_err(map_sqlx)?,
            nonce: row.try_get("nonce").map_err(map_sqlx)?,
            redirect_after: row.try_get("redirect_after").map_err(map_sqlx)?,
        }))
    }

    /// Best-effort: clear stale pending rows so the table doesn't grow.
    pub async fn purge_pending(&self, older_than_secs: i64) -> Result<()> {
        let cutoff = Utc::now().timestamp_millis() - older_than_secs * 1000;
        sqlx::query("DELETE FROM auth_pending WHERE created_at < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }
}

fn row_to_source(row: sqlx::sqlite::SqliteRow) -> Result<Source> {
    let id: String = row.try_get("id").map_err(map_sqlx)?;
    let acl_json: String = row.try_get("acl_json").map_err(map_sqlx)?;
    let status_s: String = row.try_get("status").map_err(map_sqlx)?;
    let created_ms: i64 = row.try_get("created_at").map_err(map_sqlx)?;
    let ingested_ms: Option<i64> = row.try_get("ingested_at").map_err(map_sqlx)?;
    let size: i64 = row.try_get("size_bytes").map_err(map_sqlx)?;
    let classification_json: Option<String> = row.try_get("classification_json").map_err(map_sqlx)?;

    let acl: Acl = serde_json::from_str(&acl_json)?;
    // Parse status enum by re-quoting JSON.
    let status: SourceStatus = serde_json::from_str(&format!("\"{status_s}\""))?;
    let classification = classification_json
        .map(|s| serde_json::from_str::<serde_json::Value>(&s))
        .transpose()?;

    Ok(Source {
        id: SourceId::from(id),
        folder_path: row.try_get("folder_path").map_err(map_sqlx)?,
        filename: row.try_get("filename").map_err(map_sqlx)?,
        mime: row.try_get("mime").map_err(map_sqlx)?,
        sha256: row.try_get("sha256").map_err(map_sqlx)?,
        size_bytes: size as u64,
        acl,
        status,
        language: row.try_get("language").map_err(map_sqlx)?,
        created_at: ms_to_dt(created_ms),
        ingested_at: ingested_ms.map(ms_to_dt),
        classification,
    })
}

fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ms).unwrap_or_else(Utc::now)
}

// ---------- Job queue ----------

#[async_trait]
pub trait JobQueue: Send + Sync {
    async fn enqueue(&self, job: &Job) -> Result<()>;
    async fn claim_next(&self, worker_id: &str, lease_ms: i64) -> Result<Option<Job>>;
    async fn complete(&self, id: &JobId) -> Result<()>;
    async fn fail(&self, id: &JobId, err: &str, retry_in_ms: Option<i64>) -> Result<()>;
}

#[async_trait]
impl JobQueue for SqliteStore {
    async fn enqueue(&self, job: &Job) -> Result<()> {
        let kind = enum_to_str(&job.kind)?;
        let state = enum_to_str(&job.state)?;
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload_json, state, attempt, max_attempts, next_run_at, created_at, updated_at) \
             VALUES (?,?,?,?,?,?,?,?,?)",
        )
        .bind(job.id.as_str())
        .bind(kind)
        .bind(serde_json::to_string(&job.payload)?)
        .bind(state)
        .bind(job.attempt as i64)
        .bind(job.max_attempts as i64)
        .bind(job.next_run_at.timestamp_millis())
        .bind(job.created_at.timestamp_millis())
        .bind(job.updated_at.timestamp_millis())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx)?;
        Ok(())
    }

    async fn claim_next(&self, worker_id: &str, lease_ms: i64) -> Result<Option<Job>> {
        let now = Utc::now().timestamp_millis();
        let lease_until = now + lease_ms;

        // Atomic claim via UPDATE...RETURNING (SQLite 3.35+).
        let row = sqlx::query(
            "UPDATE jobs SET state = 'running', locked_by = ?, locked_until = ?, attempt = attempt + 1, updated_at = ? \
             WHERE id = ( \
               SELECT id FROM jobs \
               WHERE state = 'queued' AND next_run_at <= ? \
               ORDER BY next_run_at LIMIT 1 \
             ) RETURNING *",
        )
        .bind(worker_id)
        .bind(lease_until)
        .bind(now)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx)?;

        row.map(row_to_job).transpose()
    }

    async fn complete(&self, id: &JobId) -> Result<()> {
        sqlx::query("UPDATE jobs SET state = 'done', locked_by = NULL, locked_until = NULL, updated_at = ? WHERE id = ?")
            .bind(Utc::now().timestamp_millis())
            .bind(id.as_str())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }

    async fn fail(&self, id: &JobId, err: &str, retry_in_ms: Option<i64>) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        match retry_in_ms {
            Some(delay) => {
                sqlx::query(
                    "UPDATE jobs SET state = CASE WHEN attempt >= max_attempts THEN 'dead' ELSE 'queued' END, \
                     last_error = ?, next_run_at = ?, locked_by = NULL, locked_until = NULL, updated_at = ? WHERE id = ?",
                )
                .bind(err)
                .bind(now + delay)
                .bind(now)
                .bind(id.as_str())
                .execute(&self.pool)
                .await
                .map_err(map_sqlx)?;
            }
            None => {
                sqlx::query("UPDATE jobs SET state = 'failed', last_error = ?, locked_by = NULL, locked_until = NULL, updated_at = ? WHERE id = ?")
                    .bind(err)
                    .bind(now)
                    .bind(id.as_str())
                    .execute(&self.pool)
                    .await
                    .map_err(map_sqlx)?;
            }
        }
        Ok(())
    }
}

fn row_to_job(row: sqlx::sqlite::SqliteRow) -> Result<Job> {
    let id: String = row.try_get("id").map_err(map_sqlx)?;
    let kind_s: String = row.try_get("kind").map_err(map_sqlx)?;
    let state_s: String = row.try_get("state").map_err(map_sqlx)?;
    let payload_s: String = row.try_get("payload_json").map_err(map_sqlx)?;
    let attempt: i64 = row.try_get("attempt").map_err(map_sqlx)?;
    let max_attempts: i64 = row.try_get("max_attempts").map_err(map_sqlx)?;
    let next_run_at: i64 = row.try_get("next_run_at").map_err(map_sqlx)?;
    let last_error: Option<String> = row.try_get("last_error").map_err(map_sqlx)?;
    let created_at: i64 = row.try_get("created_at").map_err(map_sqlx)?;
    let updated_at: i64 = row.try_get("updated_at").map_err(map_sqlx)?;

    Ok(Job {
        id: JobId::from(id),
        kind: str_to_enum::<JobKind>(&kind_s)?,
        payload: serde_json::from_str(&payload_s)?,
        state: str_to_enum::<JobState>(&state_s)?,
        attempt: attempt as u32,
        max_attempts: max_attempts as u32,
        next_run_at: ms_to_dt(next_run_at),
        last_error,
        created_at: ms_to_dt(created_at),
        updated_at: ms_to_dt(updated_at),
    })
}

fn enum_to_str<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string(v)?.trim_matches('"').to_string())
}

fn str_to_enum<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_str(&format!("\"{s}\""))?)
}

// ---------- Audit ----------

impl SqliteStore {
    pub async fn audit(&self, actor: &str, action: &str, target: Option<&str>, metadata: Option<&serde_json::Value>) -> Result<()> {
        sqlx::query("INSERT INTO audit (actor, action, target, metadata, at) VALUES (?,?,?,?,?)")
            .bind(actor)
            .bind(action)
            .bind(target)
            .bind(metadata.map(|m| m.to_string()))
            .bind(Utc::now().timestamp_millis())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx)?;
        Ok(())
    }
}

fn map_sqlx(e: sqlx::Error) -> Error {
    Error::Other(anyhow::anyhow!("sqlite: {e}"))
}
