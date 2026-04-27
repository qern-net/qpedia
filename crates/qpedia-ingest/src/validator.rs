//! Deterministic validator for agent-produced DiffBundles. See DESIGN.md §6.4.

use anyhow::Result;
use pulldown_cmark::Parser;
use qpedia_core::wiki::{DiffBundle, DiffOp};
use qpedia_store::WikiRepo;
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("page {path} markdown parse error: {reason}")]
    MarkdownParse { path: String, reason: String },

    #[error("page {path} frontmatter invalid: {reason}")]
    Frontmatter { path: String, reason: String },

    #[error("unresolved wikilink [[{target}]] in {path}")]
    UnresolvedLink { path: String, target: String },

    #[error("page {path} exceeds size cap")]
    PageTooLarge { path: String },

    #[error("bundle has no operations")]
    Empty,
}

#[derive(Debug)]
pub struct ValidationReport {
    pub errors: Vec<ValidationError>,
    pub touched: usize,
    pub bytes: usize,
}

pub async fn validate(bundle: &DiffBundle, wiki: &WikiRepo) -> Result<ValidationReport> {
    let mut errors = Vec::new();
    let mut bytes = 0usize;
    let mut touched_paths: HashSet<String> = HashSet::new();

    if bundle.operations.is_empty() {
        errors.push(ValidationError::Empty);
        return Ok(ValidationReport { errors, touched: 0, bytes: 0 });
    }

    // Collect the post-bundle path universe so wikilinks can resolve against
    // both existing pages and pages being created in this bundle.
    let existing: HashSet<String> = wiki.list_pages("").await?.into_iter().collect();
    let mut post_universe: HashSet<String> = existing.clone();
    for op in &bundle.operations {
        match op {
            DiffOp::Create { path, .. } => { post_universe.insert(path.clone()); }
            DiffOp::Delete { path, .. }  => { post_universe.remove(path); }
            _ => {}
        }
    }

    for op in &bundle.operations {
        match op {
            DiffOp::Create { path, content, .. }
            | DiffOp::Patch { path, new_content: content, .. } => {
                touched_paths.insert(path.clone());
                bytes += content.len();
                check_page(path, content, &post_universe, &mut errors);
            }
            DiffOp::Delete { .. } => {}
            DiffOp::Link { .. } => {}
        }
    }

    Ok(ValidationReport {
        errors,
        touched: touched_paths.len(),
        bytes,
    })
}

fn check_page(
    path: &str,
    content: &str,
    universe: &HashSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    // 1. Markdown parses (pulldown-cmark is lenient; we just exercise the iterator).
    let _ = Parser::new(content).count();

    // 2. Frontmatter sanity. Required: title, kind. We do a forgiving line-scan
    //    rather than full YAML parse to avoid choking on the agent's quoting.
    let fm = extract_frontmatter(content);
    match fm {
        None => errors.push(ValidationError::Frontmatter {
            path: path.into(),
            reason: "no frontmatter block".into(),
        }),
        Some(fm) => {
            if !line_has_key(&fm, "title") {
                errors.push(ValidationError::Frontmatter {
                    path: path.into(),
                    reason: "missing 'title'".into(),
                });
            }
            if !line_has_key(&fm, "kind") {
                errors.push(ValidationError::Frontmatter {
                    path: path.into(),
                    reason: "missing 'kind'".into(),
                });
            }
        }
    }

    // 3. Wikilinks resolve.
    for target in extract_wikilinks(content) {
        // Targets may be raw paths like "concepts/foo.md" or path#anchor.
        let path_only = target.split('#').next().unwrap_or(&target).to_string();
        if !universe.contains(&path_only) {
            errors.push(ValidationError::UnresolvedLink {
                path: path.into(),
                target,
            });
        }
    }
}

fn extract_frontmatter(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    let after_first = trimmed.strip_prefix("---")?;
    let end = after_first.find("\n---")?;
    Some(after_first[..end].to_string())
}

fn line_has_key(fm: &str, key: &str) -> bool {
    let needle = format!("{key}:");
    fm.lines().any(|l| l.trim_start().starts_with(&needle))
}

fn extract_wikilinks(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut bytes = content.as_bytes();
    while let Some(start) = find_subseq(bytes, b"[[") {
        let after = &bytes[start + 2..];
        if let Some(end) = find_subseq(after, b"]]") {
            if let Ok(s) = std::str::from_utf8(&after[..end]) {
                out.push(s.trim().to_string());
            }
            bytes = &after[end + 2..];
        } else {
            break;
        }
    }
    out
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}
