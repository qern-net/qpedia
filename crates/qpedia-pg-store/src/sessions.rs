//! Session lookup. Sessions are RLS-scoped by tenant; they identify
//! *who* the request is for, while RLS enforces *what tenant's data*
//! they can touch.
//!
//! The lookup path is a special case: at the moment we read a session
//! from the cookie, we don't yet know the tenant, so we use the admin
//! pool (BYPASSRLS) for this single read. Every read after that goes
//! through `begin_for(&tenant)` and respects RLS.

use crate::PgStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use qpedia_core::tenant::Tenant;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub tenant: Tenant,
    pub user_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub groups: Vec<String>,
    pub expires_at: DateTime<Utc>,
}

impl PgStore {
    pub async fn pg_create_session(
        &self,
        token_hash: &str,
        tenant: &Tenant,
        user_id: &str,
        email: Option<&str>,
        name: Option<&str>,
        provider: Option<&str>,
        groups: &[String],
        firebase_id_token_expires_at: Option<DateTime<Utc>>,
        session_expires_at: DateTime<Utc>,
    ) -> Result<()> {
        // Sessions are RLS-scoped: open a tx for this tenant first so the
        // INSERT passes the policy's WITH CHECK.
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO sessions \
             (token_hash, tenant_id, user_id, user_email, user_name, provider, \
              groups, firebase_id_token_expires_at, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
             ON CONFLICT (token_hash) DO UPDATE SET \
               tenant_id = EXCLUDED.tenant_id, \
               user_id = EXCLUDED.user_id, \
               user_email = EXCLUDED.user_email, \
               user_name = EXCLUDED.user_name, \
               provider = EXCLUDED.provider, \
               groups = EXCLUDED.groups, \
               firebase_id_token_expires_at = EXCLUDED.firebase_id_token_expires_at, \
               expires_at = EXCLUDED.expires_at",
        )
        .bind(token_hash)
        .bind(tenant.as_str())
        .bind(user_id)
        .bind(email)
        .bind(name)
        .bind(provider)
        .bind(groups)
        .bind(firebase_id_token_expires_at)
        .bind(session_expires_at)
        .execute(&mut *tx)
        .await
        .context("insert session")?;
        tx.commit().await?;
        Ok(())
    }

    /// Lookup a session by token hash. Uses the raw pool (no tenant GUC
    /// set yet — we're discovering the tenant from the cookie). The DSN
    /// for this lookup MUST be a BYPASSRLS role; for the runtime app
    /// role this single query would silently return no rows.
    ///
    /// In v2 we read sessions via a sibling admin pool. For development
    /// the same pool works because the test role has BYPASSRLS.
    pub async fn pg_lookup_session_unscoped(&self, token_hash: &str) -> Result<Option<SessionRow>> {
        let row = sqlx::query(
            "SELECT tenant_id, user_id, user_email, user_name, provider, groups, expires_at \
             FROM sessions \
             WHERE token_hash = $1 AND expires_at > now()",
        )
        .bind(token_hash)
        .fetch_optional(self.pool())
        .await
        .context("lookup_session")?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(SessionRow {
            tenant: Tenant::new(r.get::<String, _>("tenant_id")),
            user_id: r.get("user_id"),
            email: r.get("user_email"),
            name: r.get("user_name"),
            provider: r.get("provider"),
            groups: r.get("groups"),
            expires_at: r.get("expires_at"),
        }))
    }

    pub async fn pg_delete_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(self.pool())
            .await
            .context("delete_session")?;
        Ok(())
    }
}
