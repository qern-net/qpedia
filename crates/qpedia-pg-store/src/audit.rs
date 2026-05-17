//! Audit log. RLS-scoped writes.

use crate::PgStore;
use anyhow::{Context, Result};
use qpedia_core::tenant::Tenant;

impl PgStore {
    pub async fn write_audit(
        &self,
        tenant: &Tenant,
        actor: &str,
        action: &str,
        target: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO audit (tenant_id, actor, action, target, metadata) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(tenant.as_str())
        .bind(actor)
        .bind(action)
        .bind(target)
        .bind(metadata)
        .execute(&mut *tx)
        .await
        .context("write_audit")?;
        tx.commit().await?;
        Ok(())
    }
}
