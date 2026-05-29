//! OAuth grant CRUD. See migrations/0005_oauth_grants.sql.
//!
//! A grant is a durable `(tenant, provider, scope, subject)` →
//! refresh-token mapping that backs the SSO-aligned connectors. The
//! connector resolves a live access token from the refresh token at
//! sync time (refreshing on expiry). `subject = ""` means an
//! org/tenant-level grant; a non-empty subject scopes the grant to one
//! user's resources.

use crate::PgStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use qpedia_core::tenant::Tenant;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct OAuthGrant {
    pub id: i64,
    pub provider: String,
    pub scope: String,
    pub subject: String,
    pub access_token: Option<String>,
    pub refresh_token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub granted_by: String,
}

impl PgStore {
    /// Insert or update a grant. Re-authorizing the same
    /// `(tenant, provider, scope, subject)` overwrites the tokens.
    /// Returns the row id.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_oauth_grant(
        &self,
        tenant: &Tenant,
        provider: &str,
        scope: &str,
        subject: &str,
        access_token: Option<&str>,
        refresh_token: &str,
        expires_at: Option<DateTime<Utc>>,
        granted_by: &str,
    ) -> Result<i64> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "INSERT INTO oauth_grants \
             (tenant_id, provider, scope, subject, access_token, refresh_token, expires_at, granted_by) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8) \
             ON CONFLICT (tenant_id, provider, scope, subject) DO UPDATE SET \
               access_token  = EXCLUDED.access_token, \
               refresh_token = EXCLUDED.refresh_token, \
               expires_at    = EXCLUDED.expires_at, \
               granted_by    = EXCLUDED.granted_by, \
               updated_at    = now() \
             RETURNING id",
        )
        .bind(tenant.as_str())
        .bind(provider)
        .bind(scope)
        .bind(subject)
        .bind(access_token)
        .bind(refresh_token)
        .bind(expires_at)
        .bind(granted_by)
        .fetch_one(&mut *tx)
        .await
        .context("upsert_oauth_grant")?;
        tx.commit().await?;
        Ok(row.try_get::<i64, _>("id")?)
    }

    pub async fn get_oauth_grant(
        &self,
        tenant: &Tenant,
        id: i64,
    ) -> Result<Option<OAuthGrant>> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT id, provider, scope, subject, access_token, refresh_token, expires_at, granted_by \
             FROM oauth_grants WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .context("get_oauth_grant")?;
        tx.commit().await.ok();
        row.map(row_to_grant).transpose()
    }

    /// Look a grant up by its logical key. Handy for "does this tenant
    /// already have a Drive grant for this user?".
    pub async fn find_oauth_grant(
        &self,
        tenant: &Tenant,
        provider: &str,
        scope: &str,
        subject: &str,
    ) -> Result<Option<OAuthGrant>> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT id, provider, scope, subject, access_token, refresh_token, expires_at, granted_by \
             FROM oauth_grants \
             WHERE provider = $1 AND scope = $2 AND subject = $3",
        )
        .bind(provider)
        .bind(scope)
        .bind(subject)
        .fetch_optional(&mut *tx)
        .await
        .context("find_oauth_grant")?;
        tx.commit().await.ok();
        row.map(row_to_grant).transpose()
    }

    pub async fn list_oauth_grants(&self, tenant: &Tenant) -> Result<Vec<OAuthGrant>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, provider, scope, subject, access_token, refresh_token, expires_at, granted_by \
             FROM oauth_grants ORDER BY provider, scope, subject",
        )
        .fetch_all(&mut *tx)
        .await
        .context("list_oauth_grants")?;
        tx.commit().await.ok();
        rows.into_iter().map(row_to_grant).collect()
    }

    /// Cache a freshly-minted access token + expiry against an existing
    /// grant so concurrent syncs reuse it until it expires.
    pub async fn update_oauth_access_token(
        &self,
        tenant: &Tenant,
        id: i64,
        access_token: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "UPDATE oauth_grants SET access_token = $1, expires_at = $2, updated_at = now() \
             WHERE id = $3",
        )
        .bind(access_token)
        .bind(expires_at)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("update_oauth_access_token")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_oauth_grant(&self, tenant: &Tenant, id: i64) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM oauth_grants WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .context("delete_oauth_grant")?;
        tx.commit().await?;
        Ok(())
    }
}

fn row_to_grant(row: sqlx::postgres::PgRow) -> Result<OAuthGrant> {
    Ok(OAuthGrant {
        id: row.try_get("id")?,
        provider: row.try_get("provider")?,
        scope: row.try_get("scope")?,
        subject: row.try_get("subject")?,
        access_token: row.try_get("access_token").ok(),
        refresh_token: row.try_get("refresh_token")?,
        expires_at: row.try_get("expires_at").ok(),
        granted_by: row.try_get("granted_by")?,
    })
}
