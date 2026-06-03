//! Audit / observability extension points.
//!
//! Every call to [`PgStore::write_audit`] writes the row to the
//! Postgres `audit` table and *then* fires every registered
//! [`EventSink`] best-effort on a detached task. Overlays register
//! sinks (SIEM forwarders, S3 exporters, etc.) via
//! [`PgStore::register_event_sink`] — usually from
//! `AppBuilder::with_event_sink` in `qpedia-api`.
//!
//! Sinks must not propagate errors back to the request: they run on a
//! `tokio::spawn` after the row is durably committed, so a slow or
//! failing sink can never block / fail the originating handler.

use qpedia_core::tenant::Tenant;
use std::fmt::Debug;

/// Sink for audit + observability events. Implementations should keep
/// `record` fast / non-blocking — sinks are fired in sequence inside a
/// detached task per write_audit call.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync + Debug + 'static {
    /// Called after every successful audit write. Best-effort; sinks
    /// must not propagate errors back to the request.
    async fn record(
        &self,
        tenant: &Tenant,
        actor: &str,
        action: &str,
        target: Option<&str>,
        metadata: Option<&serde_json::Value>,
    );
}

/// No-op sink. Overlays register their own with
/// `AppBuilder::with_event_sink`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEventSink;

#[async_trait::async_trait]
impl EventSink for NoopEventSink {
    async fn record(
        &self,
        _tenant: &Tenant,
        _actor: &str,
        _action: &str,
        _target: Option<&str>,
        _metadata: Option<&serde_json::Value>,
    ) {
    }
}

// ---------- TenantHook ---------------------------------------------------------

/// Lifecycle hook fired on tenant create / update / delete.
///
/// Hooks fire on a detached task after the DB row is durably committed,
/// so they can't slow down or fail the originating handler. Overlays
/// register hooks via `AppBuilder::with_tenant_hook` to provision
/// billing rows, send onboarding email, push a SaaS workflow, etc.
#[async_trait::async_trait]
pub trait TenantHook: Send + Sync + Debug + 'static {
    /// A tenant row was just inserted or upserted.
    async fn on_upsert(
        &self,
        tenant: &Tenant,
        display_name: &str,
        email_domain: Option<&str>,
    );

    /// A tenant row was just deleted. Default: no-op (tenant deletion
    /// is not yet wired into the OSS bootstrap path; see ROADMAP §3 ops).
    async fn on_delete(&self, _tenant: &Tenant) {}
}

/// No-op hook. Overlays register their own with
/// `AppBuilder::with_tenant_hook`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTenantHook;

#[async_trait::async_trait]
impl TenantHook for NoopTenantHook {
    async fn on_upsert(
        &self,
        _tenant: &Tenant,
        _display_name: &str,
        _email_domain: Option<&str>,
    ) {
    }
}
