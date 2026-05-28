//! Postgres + pgvector storage. See SPEC-v2.md.
//!
//! Replaces `qpedia_store::sqlite` and `qpedia_store::weaviate` with a
//! single backend. Tenant isolation is enforced by Postgres Row Level
//! Security; the application must call [`PgStore::set_tenant`] on every
//! borrowed connection before issuing tenant-scoped queries.

pub mod audit;
pub mod connectors;
pub mod folder_acls;
pub mod folders;
pub mod jobs;
pub mod oidc_pending;
pub mod sessions;
pub mod slug;
pub mod sources;
pub mod tenants;
pub mod wiki;

pub use folders::FolderRow;
pub use oidc_pending::PendingLogin;
pub use slug::{slugify, slugify_folder, unique_connector_name, unique_source_slug, unique_wiki_path};
pub use sessions::SessionRow;
pub use tenants::TenantRow;
pub use wiki::{SearchHit, WikiPageUpsert};

use anyhow::{Context, Result};
use qpedia_core::tenant::Tenant;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};
use std::str::FromStr;
use std::time::Duration;
use tracing::info;

/// Top-level Postgres store. Hold one instance per process; clone is cheap
/// (sqlx pools are Arc-internally).
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connect, run migrations as the connecting role (must have BYPASSRLS
    /// for the migrations to install RLS policies — typically the
    /// `qpedia_admin` role or the postgres superuser).
    pub async fn connect(url: &str) -> Result<Self> {
        let opts = PgConnectOptions::from_str(url).context("parse Postgres DSN")?;
        let pool = PgPoolOptions::new()
            .max_connections(
                std::env::var("QPEDIA_DB_MAX_CONN")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(16),
            )
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(opts)
            .await
            .context("connect Postgres")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("apply Postgres migrations")?;

        info!("Postgres pool ready");
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Open a transaction scoped to `tenant`. Two things happen inside
    /// the tx so RLS engages even when the connecting role has
    /// `BYPASSRLS` (which is true for dev where we connect as
    /// `qpedia_admin`):
    ///   1. `SET LOCAL ROLE qpedia_app` switches to the runtime role.
    ///   2. `set_config('qpedia.tenant', ...)` populates the GUC the
    ///      `tenant_isolation` policies read.
    /// Both reset on commit/rollback. RLS rejects any cross-tenant
    /// read/write inside the tx; misuse fails closed.
    pub async fn begin_for<'a>(&'a self, tenant: &Tenant) -> Result<Transaction<'a, Postgres>> {
        let mut tx = self.pool.begin().await.context("begin tx")?;
        sqlx::query("SET LOCAL ROLE qpedia_app")
            .execute(&mut *tx)
            .await
            .context("SET LOCAL ROLE qpedia_app")?;
        sqlx::query("SELECT set_config('qpedia.tenant', $1, true)")
            .bind(tenant.as_str())
            .execute(&mut *tx)
            .await
            .context("set qpedia.tenant GUC")?;
        Ok(tx)
    }

    /// Cross-tenant transaction. Only useful when running as a role
    /// that has BYPASSRLS (migrations, qpedia-migrate); regular app
    /// queries should always go through `begin_for(&tenant)`.
    pub async fn begin_admin<'a>(&'a self) -> Result<Transaction<'a, Postgres>> {
        self.pool.begin().await.context("begin admin tx")
    }
}
