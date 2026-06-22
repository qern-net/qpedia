//! Per-tenant LLM configuration (BYO model + BYO credentials).
//!
//! Backed by the RLS-isolated `llm_config` table (migration 0008). The API key
//! is encrypted at rest with pgcrypto symmetric encryption under the deployment
//! master key `QPEDIA_SECRET_KEY`; the plaintext key never leaves this module
//! except via [`PgStore::resolve_llm_config`] (used to build a provider) and is
//! never returned to clients — UIs see only `api_key_hint`.
//!
//! Resolution contract: a tenant with no row, or a row whose key is NULL,
//! falls back to the deployment env provider — so BYOL at deploy level stays
//! the default and this table is purely additive.

use crate::PgStore;
use anyhow::{anyhow, Context, Result};
use qpedia_core::tenant::Tenant;
use sqlx::Row;

/// A tenant's stored LLM config. `api_key` is populated **only** by
/// [`PgStore::resolve_llm_config`] (decrypted); [`PgStore::get_llm_config`]
/// leaves it `None` and reports presence via `has_api_key`.
#[derive(Debug, Clone, Default)]
pub struct LlmConfigRow {
    pub provider: String,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub api_key_hint: Option<String>,
    pub base_url: Option<String>,
    pub has_api_key: bool,
}

fn master_key() -> Result<String> {
    std::env::var("QPEDIA_SECRET_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("QPEDIA_SECRET_KEY is not set — cannot store/read BYO LLM keys"))
}

impl PgStore {
    /// Display view: provider, model, key hint, base_url, and whether a BYO key
    /// is stored. Never decrypts. `None` when the tenant has no config row.
    pub async fn get_llm_config(&self, tenant: &Tenant) -> Result<Option<LlmConfigRow>> {
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT provider, model, api_key_hint, base_url, \
                    (api_key_ciphertext IS NOT NULL) AS has_api_key \
             FROM llm_config LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .context("get_llm_config")?;
        tx.commit().await.ok();

        let Some(row) = row else { return Ok(None) };
        Ok(Some(LlmConfigRow {
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            api_key: None,
            api_key_hint: row.try_get("api_key_hint")?,
            base_url: row.try_get("base_url")?,
            has_api_key: row.try_get("has_api_key")?,
        }))
    }

    /// Resolution view: same as [`get_llm_config`](Self::get_llm_config) but
    /// **decrypts** the BYO key into `api_key` (when present). Used to build a
    /// per-tenant provider. `None` when the tenant has no config row.
    pub async fn resolve_llm_config(&self, tenant: &Tenant) -> Result<Option<LlmConfigRow>> {
        let key = master_key()?;
        let mut tx = self.begin_for(tenant).await?;
        let row = sqlx::query(
            "SELECT provider, model, base_url, api_key_hint, \
                    CASE WHEN api_key_ciphertext IS NULL THEN NULL \
                         ELSE pgp_sym_decrypt(api_key_ciphertext, $1) END AS api_key \
             FROM llm_config LIMIT 1",
        )
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await
        .context("resolve_llm_config")?;
        tx.commit().await.ok();

        let Some(row) = row else { return Ok(None) };
        let api_key: Option<String> = row.try_get("api_key")?;
        Ok(Some(LlmConfigRow {
            provider: row.try_get("provider")?,
            model: row.try_get("model")?,
            has_api_key: api_key.is_some(),
            api_key,
            api_key_hint: row.try_get("api_key_hint")?,
            base_url: row.try_get("base_url")?,
        }))
    }

    /// Upsert a tenant's config. `provider`/`model`/`base_url` are always set;
    /// the BYO key is updated only when `api_key` is `Some` (pass `None` to keep
    /// the existing stored key). Encrypts with the deployment master key.
    pub async fn set_llm_config(
        &self,
        tenant: &Tenant,
        provider: &str,
        model: Option<&str>,
        api_key: Option<&str>,
        base_url: Option<&str>,
        updated_by: &str,
    ) -> Result<()> {
        // Only require the master key when actually storing a secret.
        let key = if api_key.is_some() { Some(master_key()?) } else { None };
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query(
            "INSERT INTO llm_config \
               (tenant_id, provider, model, api_key_ciphertext, api_key_hint, base_url, updated_by, updated_at) \
             VALUES ($1, $2, $3, \
               CASE WHEN $4::text IS NULL THEN NULL ELSE pgp_sym_encrypt($4, $5) END, \
               CASE WHEN $4::text IS NULL THEN NULL ELSE right($4, 4) END, \
               $6, $7, now()) \
             ON CONFLICT (tenant_id) DO UPDATE SET \
               provider   = EXCLUDED.provider, \
               model      = EXCLUDED.model, \
               base_url   = EXCLUDED.base_url, \
               updated_by = EXCLUDED.updated_by, \
               updated_at = now(), \
               api_key_ciphertext = CASE WHEN $4::text IS NULL \
                   THEN llm_config.api_key_ciphertext ELSE pgp_sym_encrypt($4, $5) END, \
               api_key_hint = CASE WHEN $4::text IS NULL \
                   THEN llm_config.api_key_hint ELSE right($4, 4) END",
        )
        .bind(tenant.as_str())
        .bind(provider)
        .bind(model)
        .bind(api_key)
        .bind(key)
        .bind(base_url)
        .bind(updated_by)
        .execute(&mut *tx)
        .await
        .context("set_llm_config")?;
        tx.commit().await?;
        Ok(())
    }

    /// Remove a tenant's config entirely — reverts to the deployment provider.
    pub async fn clear_llm_config(&self, tenant: &Tenant) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM llm_config")
            .execute(&mut *tx)
            .await
            .context("clear_llm_config")?;
        tx.commit().await?;
        Ok(())
    }
}
