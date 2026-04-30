//! Distill phase: runs the multi-page agent loop, validates the produced
//! DiffBundle, and commits it to the wiki repo.
//!
//! See DESIGN.md §5 (state machine), §6 (agent), §6.4 (validator).

use crate::agent::{run_agent, AgentDeps};
use crate::runner::IngestContext;
use crate::validator;
use anyhow::{anyhow, Result};
use qpedia_core::{source::SourceStatus, SourceId};
use qpedia_store::sqlite::SourceStore;
use tracing::{info, warn};

pub async fn run(ctx: &IngestContext, source_id: &SourceId) -> Result<()> {
    let llm = ctx
        .llm
        .clone()
        .ok_or_else(|| anyhow!("distill called without an LLM provider"))?;

    let src = ctx
        .db
        .get_source(source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found: {source_id}"))?;

    ctx.db.update_status(source_id, SourceStatus::AgentDistilling).await?;

    let deps = AgentDeps {
        llm: llm.clone(),
        wiki: &ctx.wiki,
        blob: &ctx.blob,
        db: &ctx.db,
        embedder: ctx.embedder.clone(),
        weaviate: ctx.weaviate.clone(),
    };

    let bundle = run_agent(&deps, &src).await?;
    if bundle.operations.is_empty() {
        warn!(id = %source_id, "agent returned empty bundle; nothing to commit");
        ctx.db.update_status(source_id, SourceStatus::Committed).await?;
        return Ok(());
    }

    let report = validator::validate(&bundle, &ctx.wiki).await?;
    if !report.errors.is_empty() {
        let summary: String = report
            .errors
            .iter()
            .take(5)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(anyhow!(
            "validation failed ({} errors): {}",
            report.errors.len(),
            summary
        ));
    }

    let sha = ctx.wiki.commit_bundle(&bundle).await?;

    ctx.db.update_status(source_id, SourceStatus::Committed).await?;
    ctx.db
        .audit(
            "qpedia-bot",
            "wiki.committed",
            Some(source_id.as_str()),
            Some(&serde_json::json!({
                "sha": sha,
                "ops": bundle.operations.len(),
                "touched_pages": report.touched,
                "bundle_bytes": report.bytes,
                "summary": bundle.summary,
            })),
        )
        .await?;

    info!(
        id = %source_id,
        sha = %sha,
        ops = bundle.operations.len(),
        bytes = report.bytes,
        "distill commit landed"
    );
    Ok(())
}
