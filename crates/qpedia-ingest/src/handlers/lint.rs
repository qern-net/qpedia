//! Lint job handler. Runs the linter for a tenant, writes the report to
//! `_meta/lint.json` in that tenant's wiki repo as a single commit,
//! and audits.

use crate::runner::IngestContext;
use anyhow::Result;
use qpedia_core::{tenant::Tenant, wiki::{DiffBundle, DiffOp}};
use qpedia_lint::Linter;
use tracing::info;

pub async fn run(ctx: &IngestContext, tenant: &Tenant) -> Result<()> {
    let wiki = ctx.wiki_store.get(tenant).await?;

    let linter = Linter::new(
        wiki.clone(),
        ctx.db.clone(),
        ctx.weaviate.clone(),
        ctx.llm.clone(),
        tenant.clone(),
    );
    let report = linter.run().await?;

    let json = serde_json::to_string_pretty(&report)?;
    let bundle = DiffBundle {
        ingest_id: format!("lint-{}", report.generated_at),
        summary: format!(
            "lint({}): {} pages, {} issues",
            tenant,
            report.page_count,
            report.issue_count()
        ),
        operations: vec![DiffOp::Patch {
            path: "_meta/lint.json".into(),
            new_content: json,
            rationale: "lint report".into(),
        }],
    };

    match wiki.commit_bundle(&bundle).await {
        Ok(sha) => {
            info!(sha = %sha, tenant = %tenant, issues = report.issue_count(), "lint commit landed");
        }
        Err(e) if e.to_string().contains("no changes to commit") => {
            info!(tenant = %tenant, "lint: report unchanged since last run");
        }
        Err(e) => return Err(e),
    }

    let mut report_value = serde_json::to_value(&report)?;
    if let Some(obj) = report_value.as_object_mut() {
        obj.insert("tenant".into(), serde_json::Value::String(tenant.to_string()));
    }
    let _ = ctx
        .db
        .audit("qpedia-bot", "lint.run", Some(tenant.as_str()), Some(&report_value))
        .await;
    Ok(())
}
