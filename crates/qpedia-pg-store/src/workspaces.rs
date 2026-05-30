//! Workspace membership + invites. See migrations/0006_workspaces.sql and
//! AUTH-DESIGN.md.
//!
//! A workspace is a tenant; these add who-belongs-where and email invites.
//! Most queries are tenant-scoped (`begin_for`). Two are inherently
//! cross-tenant and use the unscoped admin pool (BYPASSRLS), keyed on a
//! capability: listing a user's workspaces (by user_id) and accepting an
//! invite (by secret token).

use crate::PgStore;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use qpedia_core::tenant::Tenant;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct WorkspaceMembership {
    pub tenant: Tenant,
    pub name: String,
    pub role: String,
    /// "individual" if the tenant id starts with `u-`, else "org".
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct Member {
    pub user_id: String,
    pub email: Option<String>,
    pub role: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Invite {
    pub id: i64,
    pub tenant: Tenant,
    pub email: String,
    pub role: String,
    pub invited_by: String,
    pub expires_at: DateTime<Utc>,
    pub accepted_at: Option<DateTime<Utc>>,
}

impl PgStore {
    // ---------- membership ----------

    /// Insert a membership if one doesn't already exist (idempotent).
    /// Tenant-scoped.
    pub async fn ensure_membership(
        &self,
        tenant: &Tenant,
        user_id: &str,
        email: Option<&str>,
        role: &str,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO workspace_members (tenant_id, user_id, email, role) \
             VALUES ($1,$2,$3,$4) \
             ON CONFLICT (tenant_id, user_id) DO NOTHING",
        )
        .bind(tenant.as_str())
        .bind(user_id)
        .bind(email)
        .bind(role)
        .execute(&mut *tx)
        .await
        .context("ensure_membership")?;
        tx.commit().await?;
        Ok(())
    }

    /// Membership role of `user_id` in `tenant`, or None if not a member.
    /// Uses the admin pool so it works before the caller's session is
    /// scoped to this tenant (e.g. during a workspace switch).
    pub async fn membership_role(&self, tenant: &Tenant, user_id: &str) -> Result<Option<String>> {
        let row: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE tenant_id = $1 AND user_id = $2",
        )
        .bind(tenant.as_str())
        .bind(user_id)
        .fetch_optional(self.pool())
        .await
        .context("membership_role")?;
        Ok(row)
    }

    /// All workspaces a user belongs to (cross-tenant; admin pool).
    pub async fn list_user_workspaces(&self, user_id: &str) -> Result<Vec<WorkspaceMembership>> {
        let rows = sqlx::query(
            "SELECT m.tenant_id, coalesce(t.display_name, m.tenant_id) AS name, m.role \
             FROM workspace_members m \
             JOIN tenants t ON t.id = m.tenant_id \
             WHERE m.user_id = $1 \
             ORDER BY (m.tenant_id LIKE 'u-%') DESC, name",
        )
        .bind(user_id)
        .fetch_all(self.pool())
        .await
        .context("list_user_workspaces")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let id: String = r.get("tenant_id");
                let kind = if id.starts_with("u-") { "individual" } else { "org" };
                WorkspaceMembership {
                    tenant: Tenant::new(id),
                    name: r.get("name"),
                    role: r.get("role"),
                    kind: kind.into(),
                }
            })
            .collect())
    }

    pub async fn list_members(&self, tenant: &Tenant) -> Result<Vec<Member>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT user_id, email, role, created_at FROM workspace_members \
             ORDER BY (role='owner') DESC, (role='admin') DESC, created_at",
        )
        .fetch_all(&mut *tx)
        .await
        .context("list_members")?;
        tx.commit().await.ok();
        Ok(rows
            .into_iter()
            .map(|r| Member {
                user_id: r.get("user_id"),
                email: r.try_get("email").ok(),
                role: r.get("role"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    /// Remove a member. Refuses to remove the last owner (caller should
    /// also guard, but this is the backstop).
    pub async fn remove_member(&self, tenant: &Tenant, user_id: &str) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        // Don't strand a workspace with no owner.
        let owners: i64 =
            sqlx::query_scalar("SELECT count(*) FROM workspace_members WHERE role = 'owner'")
                .fetch_one(&mut *tx)
                .await
                .context("count owners")?;
        let target_role: Option<String> =
            sqlx::query_scalar("SELECT role FROM workspace_members WHERE user_id = $1")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await
                .context("target role")?;
        if target_role.as_deref() == Some("owner") && owners <= 1 {
            return Err(anyhow!("cannot remove the last owner"));
        }
        sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .context("remove_member")?;
        tx.commit().await?;
        Ok(())
    }

    // ---------- org creation ----------

    /// Create an org workspace and make `owner` its first member (owner).
    /// `slug` must already be deduped/slugified by the caller.
    pub async fn create_org_workspace(
        &self,
        slug: &Tenant,
        name: &str,
        owner_user_id: &str,
        owner_email: Option<&str>,
    ) -> Result<()> {
        // Tenant row (admin pool).
        self.upsert_tenant(slug, name, None).await?;
        // Owner membership (tenant-scoped).
        self.ensure_membership(slug, owner_user_id, owner_email, "owner")
            .await?;
        Ok(())
    }

    // ---------- invites ----------

    #[allow(clippy::too_many_arguments)]
    pub async fn create_invite(
        &self,
        tenant: &Tenant,
        email: &str,
        role: &str,
        token: &str,
        invited_by: &str,
        ttl_secs: i64,
    ) -> Result<i64> {
        let expires_at = Utc::now() + Duration::seconds(ttl_secs);
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "INSERT INTO workspace_invites \
             (tenant_id, email, role, token, invited_by, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
        )
        .bind(tenant.as_str())
        .bind(email.trim().to_ascii_lowercase())
        .bind(role)
        .bind(token)
        .bind(invited_by)
        .bind(expires_at)
        .fetch_one(&mut *tx)
        .await
        .context("create_invite")?;
        tx.commit().await?;
        Ok(row.try_get::<i64, _>("id")?)
    }

    /// Pending (unaccepted, unexpired) invites for a workspace.
    pub async fn list_invites(&self, tenant: &Tenant) -> Result<Vec<Invite>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, email, role, invited_by, expires_at, accepted_at \
             FROM workspace_invites \
             WHERE accepted_at IS NULL AND expires_at > now() \
             ORDER BY created_at DESC",
        )
        .fetch_all(&mut *tx)
        .await
        .context("list_invites")?;
        tx.commit().await.ok();
        rows.into_iter().map(row_to_invite).collect()
    }

    pub async fn delete_invite(&self, tenant: &Tenant, id: i64) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM workspace_invites WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .context("delete_invite")?;
        tx.commit().await?;
        Ok(())
    }

    /// Look up an invite by its token (cross-tenant; admin pool). Returns
    /// the invite even if expired/accepted so the caller can give a
    /// precise message; validity is checked in `accept_invite`.
    pub async fn get_invite_by_token(&self, token: &str) -> Result<Option<Invite>> {
        let row = sqlx::query(
            "SELECT id, tenant_id, email, role, invited_by, expires_at, accepted_at \
             FROM workspace_invites WHERE token = $1",
        )
        .bind(token)
        .fetch_optional(self.pool())
        .await
        .context("get_invite_by_token")?;
        row.map(row_to_invite).transpose()
    }

    /// Accept an invite: validate it, add the membership, mark accepted.
    /// Returns the workspace the user joined. Cross-tenant by token.
    pub async fn accept_invite(
        &self,
        token: &str,
        user_id: &str,
        email: Option<&str>,
    ) -> Result<Tenant> {
        let invite = self
            .get_invite_by_token(token)
            .await?
            .ok_or_else(|| anyhow!("invite not found"))?;
        if invite.accepted_at.is_some() {
            return Err(anyhow!("invite already accepted"));
        }
        if invite.expires_at <= Utc::now() {
            return Err(anyhow!("invite expired"));
        }
        // Add membership in the invite's workspace (tenant-scoped) + mark
        // the invite accepted.
        self.ensure_membership(&invite.tenant, user_id, email, &invite.role)
            .await?;
        let mut tx = self.begin_for(&invite.tenant).await?;
        sqlx::query("UPDATE workspace_invites SET accepted_at = now() WHERE id = $1")
            .bind(invite.id)
            .execute(&mut *tx)
            .await
            .context("mark invite accepted")?;
        tx.commit().await?;
        Ok(invite.tenant)
    }
}

fn row_to_invite(row: sqlx::postgres::PgRow) -> Result<Invite> {
    Ok(Invite {
        id: row.try_get("id")?,
        tenant: Tenant::new(row.try_get::<String, _>("tenant_id")?),
        email: row.try_get("email")?,
        role: row.try_get("role")?,
        invited_by: row.try_get("invited_by")?,
        expires_at: row.try_get("expires_at")?,
        accepted_at: row.try_get("accepted_at").ok(),
    })
}
