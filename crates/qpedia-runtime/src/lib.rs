//! Optional wiring for hosts that run the engine inside their own application.

use anyhow::Result;
use axum::Router;
use qpedia_api::{AppBuilder, AuthMode, AuthState, ExternalAuthProvider};
use qpedia_ingest::JobRunner;
use qpedia_pg_store::SchemaMode;

/// Open an embedded engine using a separately managed, verified schema.
/// The host must supply its authenticator and protect the routes it exposes.
pub async fn from_env() -> Result<AppBuilder> {
    AppBuilder::from_env_with(
        Some(SchemaMode::Verify),
        Some(AuthState {
            mode: AuthMode::Session,
            firebase: None,
            external: None,
        }),
    )
    .await
}

/// Build the engine router and its ingest worker. The caller owns route
/// filtering, the network listener, and the lifetime of the worker.
pub async fn into_parts(
    builder: AppBuilder,
    auth: impl ExternalAuthProvider + 'static,
    worker_id: &str,
) -> Result<(Router, JobRunner)> {
    builder.ctx().db.verify_job_worker_role().await?;
    let worker = JobRunner::new(builder.ctx().clone(), worker_id);
    Ok((builder.with_auth_provider(auth).build(), worker))
}
