//! Remove pipeline: a source is being deleted. See DESIGN.md §7.
//!
//! Steps:
//!   1. Find every wiki page whose frontmatter `source_ids` includes :id.
//!   2. For each affected page:
//!      - Only source -> stage Delete.
//!      - Other sources remain -> stage Patch removing :id from frontmatter.
//!        (Content body is left intact; admins can re-trigger an ingest of
//!        a remaining source if a fresh re-synthesis is desired.)
//!   3. Commit the bundle as a single git commit.
//!   4. Delete deleted-page objects from Weaviate; re-embed patched pages.
//!   5. rm -rf the source's blob directory.
//!   6. DELETE the row from `sources`.
//!
//! Best-effort cleanup: any failure leaves the source row in place so the
//! job can retry without losing track of what's still half-removed.

use crate::handlers::embed::embed_changed_pages;
use crate::runner::IngestContext;
use anyhow::{anyhow, Result};
use chrono::Utc;
use qpedia_core::{
    wiki::{DiffBundle, DiffOp},
    JobId, SourceId,
};
use qpedia_core::job::{Job, JobKind, JobState};
use qpedia_store::{
    blob::BlobStorage,
    sqlite::SourceStore,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovePayload {
    pub source_id: SourceId,
}

/// Build a Remove job for a given source.
pub fn remove_job(source_id: &SourceId) -> Result<Job> {
    let now = Utc::now();
    Ok(Job {
        id: JobId::new(),
        kind: JobKind::Remove,
        payload: serde_json::to_value(RemovePayload { source_id: source_id.clone() })?,
        state: JobState::Queued,
        attempt: 0,
        max_attempts: 5,
        next_run_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    })
}

pub async fn run(ctx: &IngestContext, source_id: &SourceId) -> Result<()> {
    info!(id = %source_id, "remove: start");

    // Fetch the source first so we know which tenant's wiki to touch.
    let src = ctx
        .db
        .get_source(source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found: {source_id}"))?;
    let tenant = src.tenant.clone();
    let wiki = ctx.wiki_store.get(&tenant).await?;

    // 1. Plan: walk wiki, decide delete vs patch per affected page.
    let mut to_delete: Vec<String> = Vec::new();
    let mut to_patch: Vec<(String, String)> = Vec::new();

    for path in wiki.list_pages("").await? {
        let Some(content) = wiki.read_page(&path).await? else { continue };
        let source_ids = extract_source_ids(&content);
        if !source_ids.iter().any(|s| s == source_id.as_str()) {
            continue;
        }
        let remaining: Vec<String> = source_ids
            .iter()
            .filter(|s| s.as_str() != source_id.as_str())
            .cloned()
            .collect();

        if remaining.is_empty() {
            to_delete.push(path);
        } else {
            let new_content = remove_source_id_from_frontmatter(&content, &remaining);
            to_patch.push((path, new_content));
        }
    }

    info!(
        id = %source_id,
        delete = to_delete.len(),
        patch = to_patch.len(),
        "remove: planned"
    );

    // 2. Commit (only if there's something to do).
    if !to_delete.is_empty() || !to_patch.is_empty() {
        let mut ops: Vec<DiffOp> = Vec::new();
        for path in &to_delete {
            ops.push(DiffOp::Delete {
                path: path.clone(),
                rationale: format!("source {source_id} removed"),
            });
        }
        for (path, new_content) in &to_patch {
            ops.push(DiffOp::Patch {
                path: path.clone(),
                new_content: new_content.clone(),
                rationale: format!("source {source_id} removed (other sources remain)"),
            });
        }
        let bundle = DiffBundle {
            ingest_id: format!("remove-{source_id}"),
            summary: format!(
                "source {source_id} removed: {} delete, {} patch",
                to_delete.len(),
                to_patch.len()
            ),
            operations: ops,
        };
        let sha = wiki.commit_bundle(&bundle).await?;
        info!(id = %source_id, sha = %sha, "remove: wiki committed");

        // 3a. Drop deleted pages from Weaviate.
        if let Some(weaviate) = &ctx.weaviate {
            for path in &to_delete {
                if let Err(e) = weaviate.delete_page(&tenant, path).await {
                    warn!(path = %path, error = %e, "weaviate delete failed");
                }
            }
        }

        // 3b. Re-embed patched pages (HEAD diff captures the patches).
        if !to_patch.is_empty() {
            let embedded = embed_changed_pages(ctx, &tenant).await.unwrap_or_default();
            info!(id = %source_id, embedded = embedded.len(), "remove: re-embedded patched pages");
        }
    }

    // 4. Drop raw blobs.
    if let Err(e) = ctx.blob.delete_all(source_id).await {
        warn!(id = %source_id, error = %e, "remove: blob cleanup failed");
    }

    // 5. Drop the DB row.
    ctx.db.delete_source(source_id).await?;

    let _ = ctx
        .db
        .audit(
            "user",
            "source.removed",
            Some(source_id.as_str()),
            Some(&serde_json::json!({
                "deleted_pages": to_delete,
                "patched_pages": to_patch.iter().map(|(p, _)| p).collect::<Vec<_>>(),
            })),
        )
        .await;

    info!(id = %source_id, "remove: done");
    Ok(())
}

// ---------- helpers ----------

fn extract_source_ids(content: &str) -> Vec<String> {
    let trimmed = content.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else { return Vec::new() };
    let Some(end) = after.find("\n---") else { return Vec::new() };
    let fm = &after[..end];
    let mut out = Vec::new();
    for line in fm.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("source_ids:") {
            let s = rest.trim().trim_start_matches('[').trim_end_matches(']');
            for x in s.split(',') {
                let x = x.trim().trim_matches('"').trim_matches('\'');
                if !x.is_empty() {
                    out.push(x.to_string());
                }
            }
        }
    }
    out
}

/// Rewrite the frontmatter `source_ids` line to a new list. If the page
/// has no frontmatter at all, return content unchanged.
fn remove_source_id_from_frontmatter(content: &str, remaining: &[String]) -> String {
    let leading_ws_len = content.len() - content.trim_start().len();
    let leading_ws = &content[..leading_ws_len];
    let body_with_fm = &content[leading_ws_len..];

    let Some(after_first) = body_with_fm.strip_prefix("---") else {
        return content.to_string();
    };
    let Some(end_rel) = after_first.find("\n---") else {
        return content.to_string();
    };
    let fm = &after_first[..end_rel];
    let after_fm = &after_first[end_rel..]; // starts with "\n---"

    let new_list = remaining
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ");

    let mut new_fm_lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in fm.lines() {
        if line.trim_start().starts_with("source_ids:") {
            // Preserve any leading whitespace.
            let indent_end = line.len() - line.trim_start().len();
            new_fm_lines.push(format!("{}source_ids: [{new_list}]", &line[..indent_end]));
            replaced = true;
        } else {
            new_fm_lines.push(line.to_string());
        }
    }
    if !replaced {
        new_fm_lines.push(format!("source_ids: [{new_list}]"));
    }
    let new_fm = new_fm_lines.join("\n");

    format!("{leading_ws}---{new_fm}{after_fm}")
}
