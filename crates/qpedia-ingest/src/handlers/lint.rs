//! Lint job handler. Runs the linter for a tenant, writes the report to
//! `_meta/lint.json` in that tenant's wiki repo as a single commit,
//! and audits. Also re-organizes any sources still in the root folder
//! that now have a classification (catches sources uploaded before
//! auto-organization was enabled, or uploaded directly via API).

use crate::runner::IngestContext;
use anyhow::Result;
use qpedia_core::{tenant::Tenant, wiki::{DiffBundle, DiffOp}};
use qpedia_lint::Linter;
use qpedia_store::sqlite::SourceStore;
use tracing::info;

pub async fn run(ctx: &IngestContext, tenant: &Tenant) -> Result<()> {
    // ── Reorganize unorganized sources ────────────────────────────────────
    // Sources that were uploaded before auto-organization, or uploaded via
    // API without a folder, may still sit in "/". Move them now.
    let unorganized = ctx.db.list_unorganized(tenant, 500).await?;
    let mut reorganized = 0usize;
    for src in &unorganized {
        if let Some(doc_type) = src.classification
            .as_ref()
            .and_then(|c| c.get("doc_type"))
            .and_then(|v| v.as_str())
        {
            let folder = format!("/{}", doc_type.trim());
            if let Err(e) = ctx.db.update_folder_path(&src.id, &folder).await {
                tracing::warn!(id = %src.id, error = %e, "lint: failed to reorganize source");
            } else {
                reorganized += 1;
            }
        }
    }
    if reorganized > 0 {
        info!(tenant = %tenant, reorganized, "lint: reorganized sources into doc_type folders");
        let _ = ctx.db.audit(
            "qpedia-bot",
            "lint.reorganize",
            Some(tenant.as_str()),
            Some(&serde_json::json!({"reorganized": reorganized})),
        ).await;
    }

    // ── Run wiki linter ───────────────────────────────────────────────────
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
