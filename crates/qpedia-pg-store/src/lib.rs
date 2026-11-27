//! Postgres + pgvector storage. See SPEC-v2.md.
//!
//! Single backend for every piece of structured state: tenants, sources,
//! sessions, jobs, audit, folder_acls, folders, connectors, oidc_pending,
//! and the `wiki_pages` search index (pgvector + tsvector hybrid).
//! Tenant isolation is enforced by Postgres Row Level Security; callers
//! open a tenant-scoped transaction via [`PgStore::begin_for`], which
//! sets the role + GUC the policies read.

pub mod audit;
pub mod connectors;
pub mod events;
pub mod folder_acls;
pub mod folders;
pub mod jobs;
pub mod llm_config;
pub mod oauth_grants;
pub mod oidc_pending;
pub mod sessions;
pub mod slug;
pub mod sources;
pub mod versions;
pub mod telemetry;
pub mod tenants;
pub mod wiki;
pub mod workspaces;

pub use events::{EventSink, NoopEventSink, NoopTenantHook, TenantHook};
pub use telemetry::{with_db_span, DbErrorCategory, DbErrorClassify, DbSystem};
pub use folders::FolderRow;
pub use llm_config::LlmConfigRow;
pub use oauth_grants::OAuthGrant;
pub use oidc_pending::PendingLogin;
pub use slug::{slugify, slugify_folder, unique_connector_name, unique_source_slug, unique_wiki_path};
pub use sessions::SessionRow;
pub use tenants::TenantRow;
pub use wiki::{SearchHit, WikiPageUpsert};
pub use workspaces::{Invite, Member, WorkspaceDomain, WorkspaceMembership};

use anyhow::{Context, Result};
use qpedia_core::tenant::Tenant;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::info;

/// Top-level Postgres store. Hold one instance per process; clone is cheap
/// (sqlx pools and the sink registry are both `Arc`-internal).
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
    event_sinks: Arc<RwLock<Vec<Arc<dyn EventSink>>>>,
    tenant_hooks: Arc<RwLock<Vec<Arc<dyn TenantHook>>>>,
}

/// Select whether startup migrates a database or checks a separately migrated one.
#[derive(Clone, Copy)]
pub enum SchemaMode {
    Migrate,
    Verify,
}

impl SchemaMode {
    pub fn from_env() -> Result<Self> {
        match std::env::var("QPEDIA_SCHEMA_MODE") {
            Ok(mode) if mode == "verify" => Ok(Self::Verify),
            Ok(mode) if mode == "migrate" => Ok(Self::Migrate),
            Err(std::env::VarError::NotPresent) => Ok(Self::Migrate),
            _ => anyhow::bail!("QPEDIA_SCHEMA_MODE must be verify or migrate"),
        }
    }
}

impl PgStore {
    /// Connect, run migrations as the connecting role (must have BYPASSRLS
    /// for the migrations to install RLS policies — typically the
    /// `qpedia_admin` role or the postgres superuser).
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with_schema(url, SchemaMode::from_env()?).await
    }

    pub async fn connect_with_schema(url: &str, mode: SchemaMode) -> Result<Self> {
        let opts = PgConnectOptions::from_str(url).context("parse Postgres DSN")?;
        let max_connections: u32 = std::env::var("QPEDIA_DB_MAX_CONN")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(match mode {
                SchemaMode::Verify => 4,
                SchemaMode::Migrate => 16,
            });
        anyhow::ensure!(max_connections > 0, "QPEDIA_DB_MAX_CONN must be positive");
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(opts)
            .await
            .context("connect Postgres")?;

        let migrator = sqlx::migrate!("./migrations");
        match mode {
            SchemaMode::Verify => {
                let applied: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
                    "SELECT version,checksum,success FROM _sqlx_migrations ORDER BY version")
                    .fetch_all(&pool).await.context("schema must be migrated separately")?;
                let expected: Vec<_> = migrator.iter().filter(|m| !m.migration_type.is_down_migration()).collect();
                anyhow::ensure!(applied.len() == expected.len(), "schema migration inventory mismatch");
                for (actual, expected) in applied.iter().zip(expected) {
                    anyhow::ensure!(actual.0 == expected.version && actual.1.as_slice() == expected.checksum.as_ref() && actual.2,
                        "schema migration checksum mismatch");
                }
                let (privileged, role_limit): (bool, i32) = sqlx::query_as(
                    "SELECT rolsuper OR rolcreatedb OR rolcreaterole OR rolbypassrls,
                            rolconnlimit FROM pg_roles WHERE rolname = current_user",
                )
                .fetch_one(&pool)
                .await?;
                anyhow::ensure!(!privileged, "runtime database role must be restricted");
                anyhow::ensure!(
                    role_limit < 0 || max_connections <= role_limit as u32,
                    "QPEDIA_DB_MAX_CONN exceeds the runtime database role's connection limit"
                );
                let ddl: bool = sqlx::query_scalar(
                    "SELECT has_schema_privilege(current_user, 'public', 'CREATE') OR EXISTS (
                         SELECT FROM pg_class c
                         JOIN pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = 'public'
                            AND pg_has_role(current_user, c.relowner, 'MEMBER')
                     )",
                )
                .fetch_one(&pool)
                .await?;
                anyhow::ensure!(!ddl, "runtime database role cannot own or create public relations");
            }
            SchemaMode::Migrate => {
                migrator.run(&pool).await.context("apply Postgres migrations")?;
            }
        }

        info!("Postgres pool ready");
        Ok(Self {
            pool,
            event_sinks: Arc::new(RwLock::new(Vec::new())),
            tenant_hooks: Arc::new(RwLock::new(Vec::new())),
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Lightweight connectivity probe for the readiness/health endpoint.
    ///
    /// Runs `SELECT 1` against the pool and returns the underlying
    /// [`sqlx::Error`] on failure so the caller can render a cause string.
    /// The caller is responsible for bounding this with a timeout.
    pub async fn ping(&self) -> std::result::Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await.map(|_| ())
    }

    /// Register an audit sink. Every subsequent successful
    /// [`PgStore::write_audit`] call will fire it on a detached task
    /// after the row is durably committed. Cheap to call; all `PgStore`
    /// clones share the same registry.
    pub fn register_event_sink(&self, sink: Arc<dyn EventSink>) {
        if let Ok(mut g) = self.event_sinks.write() {
            g.push(sink);
        }
    }

    /// Snapshot the currently registered sinks. Used by `write_audit`
    /// to fire them after committing.
    pub(crate) fn event_sinks_snapshot(&self) -> Vec<Arc<dyn EventSink>> {
        self.event_sinks
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Register a tenant lifecycle hook. Every successful
    /// [`PgStore::upsert_tenant`] call will fire `on_upsert` on a
    /// detached task after the row is durably committed; future tenant
    /// delete plumbing will fire `on_delete` similarly.
    pub fn register_tenant_hook(&self, hook: Arc<dyn TenantHook>) {
        if let Ok(mut g) = self.tenant_hooks.write() {
            g.push(hook);
        }
    }

    /// Snapshot the currently registered tenant hooks.
    pub(crate) fn tenant_hooks_snapshot(&self) -> Vec<Arc<dyn TenantHook>> {
        self.tenant_hooks
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default()
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
