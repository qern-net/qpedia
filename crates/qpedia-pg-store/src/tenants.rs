//! Tenant CRUD. Top-level — not subject to RLS.

use crate::PgStore;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use qpedia_core::tenant::Tenant;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct TenantRow {
    pub id: Tenant,
    pub display_name: String,
    pub email_domain: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl PgStore {
    pub async fn upsert_tenant(
        &self,
        id: &Tenant,
        display_name: &str,
        email_domain: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO tenants (id, display_name, email_domain) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE \
               SET display_name = EXCLUDED.display_name, \
                   email_domain  = EXCLUDED.email_domain",
        )
        .bind(id.as_str())
        .bind(display_name)
        .bind(email_domain)
        .execute(self.pool())
        .await
        .context("upsert tenant")?;

        // Fire registered tenant hooks best-effort after the row is
        // durably committed. Detached so the originating handler returns
        // immediately; hooks can't slow it down or fail it. Mirrors the
        // EventSink integration in `audit::write_audit`.
        let hooks = self.tenant_hooks_snapshot();
        if !hooks.is_empty() {
            let tenant = id.clone();
            let display_name = display_name.to_string();
            let email_domain = email_domain.map(|s| s.to_string());
            tokio::spawn(async move {
                for hook in hooks {
                    hook.on_upsert(
                        &tenant,
                        &display_name,
                        email_domain.as_deref(),
                    )
                    .await;
                }
            });
        }

        Ok(())
    }

    pub async fn get_tenant(&self, id: &Tenant) -> Result<Option<TenantRow>> {
        let row = sqlx::query(
            "SELECT id, display_name, email_domain, created_at FROM tenants WHERE id = $1",
        )
        .bind(id.as_str())
        .fetch_optional(self.pool())
        .await
        .context("get_tenant")?;
        Ok(row.map(|r| TenantRow {
            id: Tenant::new(r.get::<String, _>("id")),
            display_name: r.get("display_name"),
            email_domain: r.get("email_domain"),
            created_at: r.get("created_at"),
        }))
    }

    /// Resolve a tenant by email domain (e.g. acme.com -> tenant "acme").
    /// Returns None if no tenant has registered this domain.
    pub async fn tenant_by_email_domain(&self, domain: &str) -> Result<Option<Tenant>> {
        let row = sqlx::query("SELECT id FROM tenants WHERE email_domain = $1")
            .bind(domain)
            .fetch_optional(self.pool())
            .await
            .context("tenant_by_email_domain")?;
        Ok(row.map(|r| Tenant::new(r.get::<String, _>("id"))))
    }
}
