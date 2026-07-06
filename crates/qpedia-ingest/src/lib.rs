//! Ingest pipeline — the agentic loop that turns raw sources into wiki pages.
//! See SPEC-v2.md §5 and §6.

pub mod agent;
pub mod handlers;
pub mod pipeline;
pub mod runner;
pub mod telemetry;
pub mod validator;

pub use pipeline::IngestPipeline;
pub use runner::{
    ingest_job, lint_job, reembed_job, remove_job, sync_job,
    IngestContext, IngestPayload, JobRunner, LintPayload, ReembedPayload, RemovePayload,
    SyncPayload,
};
pub use telemetry::{extractor, inject_current, TraceCarrier};
