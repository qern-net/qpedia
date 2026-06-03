//! Audit log. RLS-scoped writes.
//!
//! After the row is durably committed, every registered [`EventSink`]
//! (see [`PgStore::register_event_sink`]) fires on a detached task.
//! That keeps the originating handler off the sink path entirely — a
//! slow SIEM forwarder can never delay or fail a write_audit caller.

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

        // Fire registered sinks best-effort, after the row is durably
        // committed. Detached so the originating handler returns
        // immediately; sinks can't slow it down or fail it.
        let sinks = self.event_sinks_snapshot();
        if !sinks.is_empty() {
            let tenant = tenant.clone();
            let actor = actor.to_string();
            let action = action.to_string();
            let target = target.map(|s| s.to_string());
            let metadata = metadata.cloned();
            tokio::spawn(async move {
                for sink in sinks {
                    sink.record(
                        &tenant,
                        &actor,
                        &action,
                        target.as_deref(),
                        metadata.as_ref(),
                    )
                    .await;
                }
            });
        }

        Ok(())
    }
}
