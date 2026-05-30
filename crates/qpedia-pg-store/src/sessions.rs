//! Session lookup. RLS-scoped writes (we know the tenant when minting).
//! Reads happen *before* we know the tenant (the cookie is the only
//! input) so they use the unscoped pool — that connection runs as the
//! BYPASSRLS role, sees every tenant's row, and trusts the token_hash
//! lookup. The row carries `tenant_id`, which the caller then uses for
//! every subsequent query.

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
    pub async fn create_session(
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
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO sessions \
             (token_hash, tenant_id, user_id, user_email, user_name, provider, \
              groups, firebase_id_token_expires_at, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) \
             ON CONFLICT (token_hash) DO UPDATE SET \
               tenant_id  = EXCLUDED.tenant_id, \
               user_id    = EXCLUDED.user_id, \
               user_email = EXCLUDED.user_email, \
               user_name  = EXCLUDED.user_name, \
               provider   = EXCLUDED.provider, \
               groups     = EXCLUDED.groups, \
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
        .context("create_session")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn lookup_session(&self, token_hash: &str) -> Result<Option<SessionRow>> {
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

    /// Point an existing session at a different workspace (and update the
    /// groups derived from the user's role there). Used by the workspace
    /// switcher. Admin pool: the lookup-by-token isn't tenant-scoped, and
    /// the new tenant may differ from the old.
    pub async fn update_session_workspace(
        &self,
        token_hash: &str,
        tenant: &Tenant,
        groups: &[String],
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET tenant_id = $1, groups = $2 WHERE token_hash = $3",
        )
        .bind(tenant.as_str())
        .bind(groups)
        .bind(token_hash)
        .execute(self.pool())
        .await
        .context("update_session_workspace")?;
        Ok(())
    }

    pub async fn delete_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(self.pool())
            .await
            .context("delete_session")?;
        Ok(())
    }
}
