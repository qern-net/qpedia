//! Ingest pipeline — the agentic loop that turns raw sources into wiki pages.
//! See DESIGN.md §5 and §6.

pub mod agent;
pub mod handlers;
pub mod pipeline;
pub mod runner;
pub mod validator;

pub use handlers::remove::remove_job;
pub use pipeline::IngestPipeline;
pub use runner::{
    ingest_job, lint_job, sync_job, IngestContext, IngestPayload, JobRunner, LintPayload, SyncPayload,
};
