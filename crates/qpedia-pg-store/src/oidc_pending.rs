//! Short-lived OIDC handshake state. See migrations/0003_oidc_pending.sql.
//!
//! `save_pending` is called from /auth/login (PKCE challenge + nonce
//! minted); `take_pending` is called from /auth/callback (state echoed
//! back by the IdP). Rows are consumed on take and swept on a 10-minute
//! TTL — there's no tenant in play yet since the user isn't
//! authenticated, so this table is not RLS-scoped.

use crate::PgStore;
use anyhow::{Context, Result};
use sqlx::Row;

const TTL_SECS: i64 = 600;

#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub pkce_verifier: String,
    pub nonce: String,
    pub redirect_after: Option<String>,
}

impl PgStore {
    pub async fn save_pending(
        &self,
        state: &str,
        pkce_verifier: &str,
        nonce: &str,
        redirect_after: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO oidc_pending (state, pkce_verifier, nonce, redirect_after) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(state)
        .bind(pkce_verifier)
        .bind(nonce)
        .bind(redirect_after)
        .execute(self.pool())
        .await
        .context("save oidc_pending")?;
        // Best-effort sweep of stale rows.
        let _ = sqlx::query(
            "DELETE FROM oidc_pending WHERE created_at < now() - make_interval(secs => $1)",
        )
        .bind(TTL_SECS as f64)
        .execute(self.pool())
        .await;
        Ok(())
    }

    pub async fn take_pending(&self, state: &str) -> Result<Option<PendingLogin>> {
        let row = sqlx::query(
            "DELETE FROM oidc_pending \
             WHERE state = $1 AND created_at > now() - make_interval(secs => $2) \
             RETURNING pkce_verifier, nonce, redirect_after",
        )
        .bind(state)
        .bind(TTL_SECS as f64)
        .fetch_optional(self.pool())
        .await
        .context("take oidc_pending")?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(PendingLogin {
            pkce_verifier: r.try_get("pkce_verifier")?,
            nonce: r.try_get("nonce")?,
            redirect_after: r.try_get("redirect_after").ok(),
        }))
    }
}
