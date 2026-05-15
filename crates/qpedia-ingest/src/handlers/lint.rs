//! Lint job handler. Runs the linter, writes the report to
//! `_meta/lint.json` in the wiki repo as a single commit, and audits.

use crate::runner::IngestContext;
use anyhow::Result;
use qpedia_core::wiki::{DiffBundle, DiffOp};
use qpedia_lint::Linter;
use tracing::info;

pub async fn run(ctx: &IngestContext) -> Result<()> {
    let linter = Linter::new(
        ctx.wiki.clone(),
        ctx.db.clone(),
        ctx.weaviate.clone(),
        ctx.llm.clone(),
    );
    let report = linter.run().await?;

    let json = serde_json::to_string_pretty(&report)?;
    let bundle = DiffBundle {
        ingest_id: format!("lint-{}", report.generated_at),
        summary: format!(
            "lint: {} pages, {} issues ({} orphans, {} broken, {} drift, {} stale)",
            report.page_count,
            report.issue_count(),
            report.orphans.len(),
            report.broken_links.len(),
            report.index_drift.missing_from_index.len()
                + report.index_drift.stale_in_index.len(),
            report.stale_source_ids.len()
        ),
        operations: vec![DiffOp::Patch {
            path: "_meta/lint.json".into(),
            new_content: json,
            rationale: "lint report".into(),
        }],
    };

    // No-op if the report didn't change content (commit_bundle errors with
    // "no changes to commit" — we treat that as success).
    match ctx.wiki.commit_bundle(&bundle).await {
        Ok(sha) => {
            info!(sha = %sha, issues = report.issue_count(), "lint commit landed");
        }
        Err(e) if e.to_string().contains("no changes to commit") => {
            info!("lint: report unchanged since last run");
        }
        Err(e) => return Err(e),
    }

    let report_value = serde_json::to_value(&report)?;
    let _ = ctx
        .db
        .audit("qpedia-bot", "lint.run", None, Some(&report_value))
        .await;
    Ok(())
}
