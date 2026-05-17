//! Connector CRUD. Re-uses `qpedia_connectors::ConnectorConfig` as the
//! row shape so admin endpoints can echo it directly (minus secrets).

use crate::PgStore;
use anyhow::{Context, Result};
use chrono::Utc;
use qpedia_connectors::ConnectorConfig;
use qpedia_core::tenant::Tenant;
use sqlx::Row;

impl PgStore {
    pub async fn list_connectors(&self, tenant: &Tenant) -> Result<Vec<ConnectorConfig>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT id, tenant_id, kind, name, config_json, cursor, enabled, last_run_at, last_error \
             FROM connectors ORDER BY created_at DESC",
        )
        .fetch_all(&mut *tx)
        .await
        .context("list_connectors")?;
        tx.commit().await.ok();
        rows.into_iter().map(row_to_connector).collect()
    }

    pub async fn get_connector(&self, tenant: &Tenant, id: &str) -> Result<Option<ConnectorConfig>> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, kind, name, config_json, cursor, enabled, last_run_at, last_error \
             FROM connectors WHERE id::text = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .context("get_connector")?;
        tx.commit().await.ok();
        row.map(row_to_connector).transpose()
    }

    pub async fn get_connector_by_name(
        &self,
        tenant: &Tenant,
        name: &str,
    ) -> Result<Option<ConnectorConfig>> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT id, tenant_id, kind, name, config_json, cursor, enabled, last_run_at, last_error \
             FROM connectors WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .context("get_connector_by_name")?;
        tx.commit().await.ok();
        row.map(row_to_connector).transpose()
    }

    pub async fn insert_connector(&self, tenant: &Tenant, c: &ConnectorConfig) -> Result<i64> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "INSERT INTO connectors (tenant_id, kind, name, config_json, enabled) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(tenant.as_str())
        .bind(&c.kind)
        .bind(&c.name)
        .bind(&c.config_json)
        .bind(c.enabled)
        .fetch_one(&mut *tx)
        .await
        .context("insert_connector")?;
        tx.commit().await?;
        Ok(row.try_get::<i64, _>("id")?)
    }

    pub async fn update_connector_cursor(
        &self,
        tenant: &Tenant,
        id: &str,
        cursor: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "UPDATE connectors SET cursor = $1, last_run_at = now(), last_error = $2 \
             WHERE id::text = $3",
        )
        .bind(cursor)
        .bind(error)
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("update_connector_cursor")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_connector(&self, tenant: &Tenant, id: &str) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM connectors WHERE id::text = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .context("delete_connector")?;
        tx.commit().await?;
        Ok(())
    }

    /// Connectors due for sync — enabled, stale, capped. Cross-tenant by
    /// design (the scheduler runs as the admin pool).
    pub async fn due_connectors(&self, older_than_ms: i64, limit: i64) -> Result<Vec<ConnectorConfig>> {
        let cutoff = Utc::now() - chrono::Duration::milliseconds(older_than_ms);
        let rows = sqlx::query(
            "SELECT id, tenant_id, kind, name, config_json, cursor, enabled, last_run_at, last_error \
             FROM connectors \
             WHERE enabled = TRUE AND (last_run_at IS NULL OR last_run_at < $1) \
             ORDER BY last_run_at ASC NULLS FIRST LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit)
        .fetch_all(self.pool())
        .await
        .context("due_connectors")?;
        rows.into_iter().map(row_to_connector).collect()
    }
}

fn row_to_connector(row: sqlx::postgres::PgRow) -> Result<ConnectorConfig> {
    let id: i64 = row.try_get("id")?;
    let last_run_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("last_run_at").ok();
    Ok(ConnectorConfig {
        id: id.to_string(),
        tenant: row.try_get("tenant_id")?,
        kind: row.try_get("kind")?,
        name: row.try_get("name")?,
        config_json: row.try_get("config_json")?,
        cursor: row.try_get("cursor").ok(),
        enabled: row.try_get("enabled")?,
        last_run_at,
        last_error: row.try_get("last_error").ok(),
    })
}
