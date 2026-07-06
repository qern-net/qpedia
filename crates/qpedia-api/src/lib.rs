//! Qpedia HTTP API as a composable library.
//!
//! The binary `qpedia-api` is a 10-line wrapper around `AppBuilder`. The
//! library surface lets the `qpedia-pvt` SaaS overlay add routes, inject
//! typed state, and register lifecycle / observability hooks — all
//! without forking `main.rs`. See `OPEN-CORE.md` for the open-core
//! split strategy.
//!
//! Minimal use from the OSS binary:
//!
//! ```no_run
//! # async fn _x() -> anyhow::Result<()> {
//! qpedia_api::AppBuilder::from_env().await?.serve().await
//! # }
//! ```
//!
//! Minimal overlay use (in `qpedia-pvt-api`):
//!
//! ```no_run
//! # use qpedia_api::AppBuilder;
//! # async fn _x() -> anyhow::Result<()> {
//! AppBuilder::from_env().await?
//!     // .with_state_extension(billing_service)
//!     // .with_routes(billing_router())
//!     // .with_event_sink(siem_sink)
//!     // .with_tenant_hook(provisioning_hook)
//!     .serve().await
//! # }
//! ```

pub mod app;
pub mod auth;
pub mod firebase;
pub mod health;
pub mod rate_limit;
pub mod routes;
pub mod telemetry;

pub use app::{
    AppBuilder, AppState, EventSink, Extensions, NoopEventSink, NoopTenantHook, TenantHook,
};
pub use auth::{
    hash_token, is_superadmin_user, mint_session, read_bearer, AuthExtractorState, AuthMode,
    AuthState, ExternalAuthProvider, User,
};
pub use rate_limit::ChatRateLimiter;
pub use routes::ApiError;
