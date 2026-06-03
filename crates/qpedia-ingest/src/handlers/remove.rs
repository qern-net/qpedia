//! Remove pipeline: a source is being deleted. See SPEC-v2.md §7.
//!
//! Steps:
//!   1. Find every wiki page whose frontmatter `source_ids` includes :id.
//!   2. For each affected page:
//!      - Only source → stage Delete.
//!      - Other sources remain → stage Patch removing :id from frontmatter.
//!   3. Update index.md to remove links to deleted pages.
//!   4. Append a line to log.md.
//!   5. Commit the bundle as a single git commit.
//!   6. Delete removed-page rows from wiki_pages; re-embed patched pages.
//!   7. rm -rf the source's blob directory.
//!   8. DELETE the row from `sources`.
//!
//! Idempotent: if the source row is already gone the job exits cleanly.
//! If the git commit already happened (retry after partial failure) the
//! pages won't be found in the wiki and the bundle will be empty — the
//! handler skips the commit and proceeds to blob/DB cleanup.

use crate::handlers::embed::embed_changed_pages;
use crate::runner::IngestContext;
use anyhow::Result;
use chrono::Utc;
use qpedia_core::{
    tenant::Tenant,
    wiki::{DiffBundle, DiffOp},
    SourceId,
};
use qpedia_store::blob::BlobStorage;
use tracing::{info, warn};

pub async fn run(ctx: &IngestContext, tenant: &Tenant, source_id: &SourceId) -> Result<()> {
    info!(id = %source_id, "remove: start");

    // Fetch the source so we know we're operating on the right tenant's wiki.
    // If the row is already gone (idempotent retry) skip straight to cleanup.
    if ctx.db.get_source_in(tenant, source_id).await?.is_none() {
        info!(id = %source_id, "remove: source already gone, cleaning up blobs");
        let _ = ctx.blob.delete_all(source_id).await;
        return Ok(());
    }
    let wiki = ctx.wiki_store.get(tenant).await?;

    // ── 1. Plan: walk wiki, decide delete vs patch per affected page ──────

    let mut to_delete: Vec<String> = Vec::new();
    let mut to_patch: Vec<(String, String)> = Vec::new();

    for path in wiki.list_pages("").await? {
        // Skip system files — they're handled separately below.
        if is_system_path(&path) {
            continue;
        }
        let Some(content) = wiki.read_page(&path).await? else { continue };
        let source_ids = extract_source_ids(&content);
        if !source_ids.iter().any(|s| s == source_id.as_str()) {
            continue;
        }
        let remaining: Vec<String> = source_ids
            .into_iter()
            .filter(|s| s.as_str() != source_id.as_str())
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

    // ── 2. Build the diff bundle ──────────────────────────────────────────

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

    // ── 3. Update index.md — remove links to deleted pages ───────────────

    if !to_delete.is_empty() {
        if let Some(index_content) = wiki.read_page("index.md").await? {
            let new_index = remove_wikilinks_from_index(&index_content, &to_delete);
            if new_index != index_content {
                ops.push(DiffOp::Patch {
                    path: "index.md".into(),
                    new_content: new_index,
                    rationale: format!(
                        "remove {} page(s) deleted with source {source_id}",
                        to_delete.len()
                    ),
                });
            }
        }
    }

    // ── 4. Append to log.md ───────────────────────────────────────────────

    if let Some(log_content) = wiki.read_page("log.md").await? {
        let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let log_line = if to_delete.is_empty() && to_patch.is_empty() {
            format!("\n- {timestamp} remove({source_id}): no wiki pages affected\n")
        } else {
            format!(
                "\n- {timestamp} remove({source_id}): deleted {} page(s), patched {} page(s)\n",
                to_delete.len(),
                to_patch.len()
            )
        };
        ops.push(DiffOp::Patch {
            path: "log.md".into(),
            new_content: format!("{}{}", log_content.trim_end(), log_line),
            rationale: format!("log source {source_id} removal"),
        });
    }

    // ── 5. Commit ─────────────────────────────────────────────────────────

    if !ops.is_empty() {
        let bundle = DiffBundle {
            ingest_id: format!("remove-{source_id}"),
            summary: format!(
                "source {source_id} removed: {} deleted, {} patched",
                to_delete.len(),
                to_patch.len()
            ),
            operations: ops,
        };
        let sha = wiki.commit_bundle(&bundle).await?;
        info!(id = %source_id, sha = %sha, "remove: wiki committed");

        // ── 6a. Drop deleted pages from wiki_pages ─────────────────────────
        for path in &to_delete {
            if let Err(e) = ctx.db.delete_wiki_page(tenant, path).await {
                warn!(path = %path, error = %e, "wiki_pages delete failed (non-fatal)");
            }
        }

        // ── 6b. Re-embed patched pages (HEAD diff captures the patches) ───
        if !to_patch.is_empty() {
            match embed_changed_pages(ctx, tenant).await {
                Ok(embedded) => info!(id = %source_id, embedded = embedded.len(), "remove: re-embedded patched pages"),
                Err(e) => warn!(id = %source_id, error = %e, "remove: re-embed failed (non-fatal)"),
            }
        }
    } else {
        info!(id = %source_id, "remove: no wiki changes needed");
    }

    // ── 7. Drop raw blobs ─────────────────────────────────────────────────

    if let Err(e) = ctx.blob.delete_all(source_id).await {
        warn!(id = %source_id, error = %e, "remove: blob cleanup failed (non-fatal)");
    }

    // ── 8. Drop the DB row ────────────────────────────────────────────────

    ctx.db.delete_source(tenant, source_id).await?;

    let _ = ctx
        .db
        .write_audit(
            tenant,
            "qpedia-bot",
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

// ── helpers ───────────────────────────────────────────────────────────────────

fn is_system_path(path: &str) -> bool {
    matches!(path, "index.md" | "log.md" | "QPEDIA.md") || path.starts_with("_meta/")
}

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

/// Rewrite the frontmatter `source_ids` line to the given remaining list.
/// Returns content unchanged if no frontmatter is found.
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

/// Remove all `[[path]]` wikilinks from index.md that point to any of the
/// deleted paths. Handles both bare `[[path]]` and inline list items like
/// `- [[path]] — description`. Lines that become empty after removal are
/// dropped entirely to keep the index tidy.
fn remove_wikilinks_from_index(content: &str, deleted_paths: &[String]) -> String {
    let deleted_set: std::collections::HashSet<&str> =
        deleted_paths.iter().map(|s| s.as_str()).collect();

    content
        .lines()
        .filter(|line| {
            // Keep the line unless it contains a wikilink to a deleted page.
            let mut keep = true;
            let mut bytes = line.as_bytes();
            while let Some(start) = find_subseq(bytes, b"[[") {
                let after = &bytes[start + 2..];
                if let Some(end) = find_subseq(after, b"]]") {
                    if let Ok(target) = std::str::from_utf8(&after[..end]) {
                        let path_only = target.trim().split('#').next().unwrap_or(target.trim());
                        if deleted_set.contains(path_only) {
                            keep = false;
                            break;
                        }
                    }
                    bytes = &after[end + 2..];
                } else {
                    break;
                }
            }
            keep
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
