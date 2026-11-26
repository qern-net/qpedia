//! Git-backed wiki repository. See DESIGN.md §2.1, §2.3.
//!
//! v0 implementation shells out to `git` CLI for commit creation. This is
//! deliberate: gix's commit API is significantly more involved and the
//! per-commit cost (~50ms subprocess) is irrelevant at our throughput.
//! A future swap to `gix` keeps this module's surface unchanged.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use qpedia_core::{tenant::Tenant, wiki::{DiffBundle, DiffOp, WikiPageScope}};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// QPEDIA.md template — the schema/style guide loaded into every agent call.
const QPEDIA_MD: &str = r#"# QPEDIA.md — Wiki Style & Operations Guide

This file governs how the LLM (qpedia-bot) writes and maintains this wiki.

## Page kinds & hierarchy
Organize pages into a SHALLOW, navigable tree — at most `kind/category/page`:
- **topics/**: hub pages (kind: concept) that introduce a subject area and
  link down into its concepts and entities. High-level entry points for
  readers and search.
- **concepts/<category>/**: ideas, processes, frameworks, grouped by domain
  category (e.g. law, finance, theology, governance). Title = noun phrase.
- **entities/<type>/**: people, organizations, places, products, systems,
  works. Title = proper noun.
- **comparisons/**: "X vs Y" syntheses. Created when 2+ entities/concepts overlap.
- **summaries/**: one page per raw source. Created on ingest (a leaf).

## Categorization
- Reuse existing categories/types — read `index.md` before minting a new one.
- Categorize only when it aids navigation; a lone page sits one level up.
- Keep it SHALLOW (category + page). Page granularity follows topic
  coherence, NOT folder depth — never split a page just to fill a category.

## Writing rules
- Every page is standalone-readable. Never write "see above" — link instead.
- Every claim has a source citation: `[^src:<source_id>]`.
- Prefer bullet lists over paragraphs for fact-dense content.
- Page length target: 300–2000 words. Split only when covering ≥2 subjects.
- New terms introduced → create a concept page → link.

## Linking
- Use `[[path/to/page.md]]` for internal links.
- Every page should have ≥2 outbound links (unless it's a leaf summary).
- Topic hubs link down to their pages; concept/entity pages link up to the hub.
- When adding a fact that contradicts an existing page, add a `> ⚠ contradicts [[...]]` callout.

## Ingest protocol
1. Read the source.
2. search_wiki for related pages; read `index.md` for existing categories.
3. Create one summary page at `summaries/<source_id>.md`.
4. Create/update the concept/entity pages under their category/type path.
5. Maintain the subject-area hub in `topics/` (create once ≥3 related pages exist).
6. Update `index.md` (hierarchical catalog: kind → category → page).
7. Append one line to `log.md`.

## Forbidden
- Never rewrite content you can't derive from the sources.
- Never delete pages with external inbound links — mark deprecated instead.
- Never modify QPEDIA.md itself.
"#;

const INDEX_MD: &str = r#"# Index

LLM-maintained catalog of every page, grouped by kind → category.

## Topics
<!-- subject-area hub pages -->

## Concepts
<!-- by category, e.g. ### law / ### finance -->

## Entities
<!-- by type, e.g. ### people / ### organizations -->

## Comparisons
<!-- "X vs Y" syntheses -->

## Summaries
<!-- one entry per ingested source -->
"#;

const LOG_MD: &str = r#"# Log

Append-only chronological history of ingests, queries, and lint passes.

"#;

#[derive(Clone)]
pub struct WikiRepo {
    root: PathBuf,
    author_name: String,
    author_email: String,
    page_scope: WikiPageScope,
    source_filings: Arc<HashMap<String, String>>,
    provenance: Arc<Mutex<Option<Arc<ProvenanceIndex>>>>,
}

struct ProvenanceIndex {
    head: String,
    pages: HashMap<String, FilingPage>,
    by_source: HashMap<String, Vec<String>>,
}

struct FilingPage {
    title: String,
    content: String,
    source_ids: Vec<String>,
}

impl WikiRepo {
    /// Open the repo at `root`, initializing + seeding it on first use.
    pub async fn open_or_init(
        root: impl AsRef<Path>,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await.context("create wiki dir")?;

        let author_name = author_name.into();
        let author_email = author_email.into();

        // Sanity: git on PATH?
        if Command::new("git").arg("--version").output().await.is_err() {
            return Err(anyhow!("`git` not found on PATH — install git or use the qpedia container image"));
        }

        let repo = Self { root: root.clone(), author_name, author_email, page_scope: WikiPageScope::All,
            source_filings: Arc::default(), provenance: Arc::default() };

        if !root.join(".git").exists() {
            info!(path = %root.display(), "initializing wiki repo");
            repo.git(&["init", "-q", "-b", "main"]).await?;
            repo.git(&["config", "user.name", &repo.author_name]).await?;
            repo.git(&["config", "user.email", &repo.author_email]).await?;
            // Scoped to this repo (no global config writes).

            // Seed required files.
            tokio::fs::write(root.join("QPEDIA.md"), QPEDIA_MD).await?;
            tokio::fs::write(root.join("index.md"), INDEX_MD).await?;
            tokio::fs::write(root.join("log.md"), LOG_MD).await?;
            for d in &["topics", "concepts", "entities", "comparisons", "summaries", "_meta"] {
                let dir = root.join(d);
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(dir.join(".gitkeep"), "").await?;
            }
            repo.git(&["add", "-A"]).await?;
            repo.git(&["commit", "-q", "-m", "qpedia: initial wiki seed"]).await?;
            info!("wiki repo seeded");
        }

        Ok(repo)
    }

    pub fn root(&self) -> &Path { &self.root }

    pub fn with_page_scope(mut self, scope: WikiPageScope) -> Self {
        self.page_scope = scope;
        self
    }

    pub fn page_scope(&self) -> WikiPageScope { self.page_scope }

    /// Attached to the request's reader, never to the cached writer or the Git tree.
    pub fn with_source_filings(mut self, filings: HashMap<String, String>) -> Self {
        self.source_filings = Arc::new(filings);
        self
    }

    pub fn filing_paths<'a>(&'a self, content: &str) -> Vec<&'a str> {
        if self.source_filings.is_empty() { return vec![]; }
        let Ok(ids) = crate::page_source_ids(content) else { return vec![]; };
        ids.iter().filter_map(|id| self.source_filings.get(id).map(String::as_str)).collect()
    }

    /// Shared across request readers. Only committed wiki changes invalidate this
    /// reverse-provenance index; current authorized filing labels stay per request.
    async fn provenance_index(&self) -> Result<Arc<ProvenanceIndex>> {
        let head = self.git(&["rev-parse", "HEAD"]).await.ok();
        let mut cache = self.provenance.lock().await;
        if let (Some(head), Some(index)) = (&head, cache.as_ref()) {
            if head == &index.head { return Ok(index.clone()); }
        }
        let mut index = ProvenanceIndex { head: head.clone().unwrap_or_default(), pages: HashMap::new(), by_source: HashMap::new() };
        let all = self.clone().with_page_scope(WikiPageScope::All);
        for path in all.list_pages("").await? {
            let Some(content) = all.read_stored_page(&path).await? else { continue; };
            let Ok(source_ids) = crate::page_source_ids(&content) else { continue; };
            for source in &source_ids {
                index.by_source.entry(source.clone()).or_default().push(path.clone());
            }
            let title = extract_title(&content).unwrap_or_else(|| path.clone());
            index.pages.insert(path, FilingPage { title, content, source_ids });
        }
        let index = Arc::new(index);
        // Never retain a snapshot built across a concurrent commit or without Git.
        if head.is_some() && self.git(&["rev-parse", "HEAD"]).await.ok() == head {
            *cache = Some(index.clone());
        }
        Ok(index)
    }

    pub async fn exact_filing_pages(&self, query: &str) -> Result<HashSet<String>> {
        let q = query.trim().to_lowercase();
        let sources: Vec<_> = self.source_filings.iter().filter(|(_, key)| exact_filing(key, &q)).map(|(source, _)| source).collect();
        if sources.is_empty() { return Ok(HashSet::new()); }
        let index = self.provenance_index().await?;
        Ok(sources.into_iter().filter_map(|source| index.by_source.get(source)).flatten()
            .filter(|path| self.page_scope.includes(path)).cloned().collect())
    }

    pub async fn search_filings(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let q = query.trim().to_lowercase();
        if q.is_empty() || limit == 0 { return Ok(vec![]); }
        let matching: HashSet<_> = self.source_filings.iter()
            .filter(|(_, key)| filing_matches(key, &q)).map(|(source, _)| source.as_str()).collect();
        if matching.is_empty() { return Ok(vec![]); }
        let index = self.provenance_index().await?;
        let paths: HashSet<_> = matching.iter().filter_map(|source| index.by_source.get(*source)).flatten().collect();
        let mut hits = Vec::new();
        for path in paths {
            if !self.page_scope.includes(path) { continue; }
            let page = &index.pages[path];
            let keys: Vec<_> = page.source_ids.iter().filter(|id| matching.contains(id.as_str()))
                .filter_map(|id| self.source_filings.get(id)).collect();
            let exact = keys.iter().any(|key| exact_filing(key, &q));
            let score = relevance(&q, &page.title, &page.content);
            let labels = keys.iter().map(|key| key.as_str()).collect::<Vec<_>>().join(" · ");
            let snippet = if exact { labels } else {
                format!("{} — {}", labels, page.content.chars().take(500).collect::<String>().replace('\n', " "))
            };
            hits.push((exact, score, SearchHit { path: path.clone(), title: page.title.clone(), snippet }));
        }
        // Rank ALL matching pages before taking a bounded candidate pool.
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.path.cmp(&b.2.path)));
        Ok(hits.into_iter().take(limit).map(|(_, _, hit)| hit).collect())
    }

    /// Reciprocal-rank fusion avoids comparing incompatible PG and filesystem
    /// scores. Exact filing keys remain first; broad folder matches compete with
    /// content hits instead of exhausting the result budget ahead of them.
    pub async fn merge_search_hits(&self, query: &str, content: Vec<SearchHit>, limit: usize) -> Result<Vec<(SearchHit, f64)>> {
        if limit == 0 { return Ok(vec![]); }
        let exact = self.exact_filing_pages(query).await?;
        let filings = self.search_filings(query, limit).await?;
        let mut candidates: HashMap<String, (SearchHit, f64, usize)> = HashMap::new();
        for list in [content, filings] {
            let mut seen = HashSet::new();
            for (rank, hit) in list.into_iter().enumerate() {
                if !seen.insert(hit.path.clone()) { continue; }
                let order = candidates.len();
                let entry = candidates.entry(hit.path.clone()).or_insert_with(|| (hit.clone(), 0., order));
                entry.1 += 1. / (60. + rank as f64);
                if exact.contains(&hit.path) { entry.0 = hit; }
            }
        }
        let mut candidates: Vec<_> = candidates.into_values().collect();
        candidates.sort_by(|a, b| exact.contains(&b.0.path).cmp(&exact.contains(&a.0.path))
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| relevance(query, &b.0.title, &b.0.snippet).cmp(&relevance(query, &a.0.title, &a.0.snippet)))
            .then(a.2.cmp(&b.2)));
        Ok(candidates.into_iter().take(limit).map(|(hit, score, _)| {
            let score = if exact.contains(&hit.path) { 1. + score } else { score };
            (hit, score)
        }).collect())
    }

    pub async fn read_page(&self, path: &str) -> Result<Option<String>> {
        let Some(mut content) = self.read_stored_page(path).await? else { return Ok(None); };
        let keys = self.filing_paths(&content);
        if !keys.is_empty() {
            let (_, body) = crate::page_metadata::split(&content)?;
            let offset = content.len() - body.len();
            content.insert_str(offset, &format!("Current source filing paths: {}\n\n", serde_json::to_string(&keys)?));
        }
        Ok(Some(content))
    }

    async fn read_stored_page(&self, path: &str) -> Result<Option<String>> {
        if !self.page_scope.includes(path) { return Ok(None); }
        let safe = self.safe_join(path)?;
        match tokio::fs::read_to_string(&safe).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_pages(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let scope = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.safe_join(prefix)?
        };
        if !scope.exists() {
            return Ok(out);
        }
        walk_md(&self.root, &scope, &mut out)?;
        out.retain(|path| self.page_scope.includes(path));
        out.sort();
        Ok(out)
    }

    /// Markdown paths touched by the HEAD commit (works for the root commit
    /// too via `git show --name-only`). Returns paths relative to the repo
    /// root, with forward slashes.
    pub async fn changed_in_head(&self) -> Result<Vec<String>> {
        let out = self.git(&["show", "--pretty=format:", "--name-only", "HEAD"]).await?;
        Ok(out
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .filter(|l| l.ends_with(".md"))
            .map(|l| l.replace('\\', "/"))
            .collect())
    }

    /// Filesystem-based text search over wiki pages. Returns matches with
    /// `(path, title, snippet)`. Title is the first H1, falling back to the
    /// frontmatter `title` or the file path. Snippet is ~160 chars around
    /// the first hit. Used as the fallback when the Postgres hybrid search
    /// returns no rows (e.g. before the embedder model is loaded, or when
    /// the `wiki_pages` table is empty pending a `reembed`).
    pub async fn search_text(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let pages = self.list_pages("").await?;
        let mut hits = Vec::new();
        for path in pages {
            if hits.len() >= limit { break; }
            let Some(content) = self.read_stored_page(&path).await? else { continue };
            let lower = content.to_lowercase();
            let Some(pos) = lower.find(&q) else { continue };

            let title = extract_title(&content).unwrap_or_else(|| path.clone());
            // A lowercase mapping can expand a character. Translate to a character
            // position before constructing the snippet from the original UTF-8 text.
            let mut lower_bytes = 0;
            let index = content.chars().take_while(|c| {
                if lower_bytes >= pos { return false; }
                lower_bytes += c.to_lowercase().map(char::len_utf8).sum::<usize>();
                true
            }).count();
            let snippet = content.chars().skip(index.saturating_sub(80)).take(200)
                .collect::<String>().replace('\n', " ");
            hits.push(SearchHit { path, title, snippet });
        }
        Ok(hits)
    }

    /// Apply a DiffBundle as a single git commit. Returns the commit sha.
    pub async fn commit_bundle(&self, bundle: &DiffBundle) -> Result<String> {
        let mut total_bytes = 0usize;

        // Prepare all replacements before touching the working tree.
        let mut operations = bundle.operations.clone();
        let mut prepared: HashMap<String, Option<String>> = HashMap::new();
        for op in &mut operations {
            let (path, content, creating) = match op {
                DiffOp::Create { path, content, .. } => (path, content, true),
                DiffOp::Patch { path, new_content, .. } => (path, new_content, false),
                DiffOp::Delete { path, .. } => {
                    prepared.insert(path.clone(), None);
                    continue;
                }
                DiffOp::Link { .. } => continue,
            };
            if crate::page_metadata::is_managed(path) {
                let previous = match prepared.get(path) {
                    Some(page) => page.clone(),
                    None => self.read_page(path).await?,
                };
                anyhow::ensure!(creating == previous.is_none(), "unexpected page existence for {path}");
                *content = self.stamp_page(path, content, previous.as_deref()).await?;
                prepared.insert(path.clone(), Some(content.clone()));
            }
        }
        // Validate up-front: paths safe, sizes within caps.
        for op in &operations {
            match op {
                DiffOp::Create { path, content, .. } |
                DiffOp::Patch  { path, new_content: content, .. } => {
                    if content.len() > 50 * 1024 {
                        return Err(anyhow!("page {path} exceeds 50KB cap ({} bytes)", content.len()));
                    }
                    self.safe_join(path)?;  // validates path
                    total_bytes += content.len();
                }
                DiffOp::Delete { path, .. } => { self.safe_join(path)?; }
                DiffOp::Link   { from, to, .. } => {
                    self.safe_join(from)?;
                    self.safe_join(to)?;
                }
            }
        }
        if total_bytes > 500 * 1024 {
            return Err(anyhow!("bundle exceeds 500KB cap ({total_bytes} bytes)"));
        }

        // Apply ops to working tree.
        for op in &operations {
            match op {
                DiffOp::Create { path, content, .. } => {
                    let full = self.safe_join(path)?;
                    if let Some(parent) = full.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&full, content).await
                        .with_context(|| format!("write {path}"))?;
                }
                DiffOp::Patch { path, new_content, .. } => {
                    let full = self.safe_join(path)?;
                    tokio::fs::write(&full, new_content).await
                        .with_context(|| format!("patch {path}"))?;
                }
                DiffOp::Delete { path, .. } => {
                    let full = self.safe_join(path)?;
                    if full.exists() {
                        tokio::fs::remove_file(&full).await
                            .with_context(|| format!("delete {path}"))?;
                    } else {
                        warn!(%path, "delete target missing; skipping");
                    }
                }
                DiffOp::Link { .. } => {
                    // Edges are stored in frontmatter (`links_out`); no separate file.
                    // The page Patch op carrying the new frontmatter does the work.
                }
            }
        }

        // Stage everything (covers creates, patches, deletes).
        self.git(&["add", "-A"]).await?;

        // Anything to commit?
        let status = self.git(&["status", "--porcelain"]).await?;
        if status.trim().is_empty() {
            return Err(anyhow!("commit_bundle: no changes to commit"));
        }

        let msg = if bundle.summary.is_empty() {
            format!("ingest({})", bundle.ingest_id)
        } else {
            format!("ingest({}): {}", bundle.ingest_id, bundle.summary)
        };
        self.git(&["commit", "-q", "-m", &msg]).await?;
        let sha = self.git(&["rev-parse", "HEAD"]).await?.trim().to_string();
        info!(sha = %sha, ops = bundle.operations.len(), "wiki commit landed");
        Ok(sha)
    }

    // ---------- internals ----------

    fn safe_join(&self, rel: &str) -> Result<PathBuf> {
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() {
            return Err(anyhow!("empty path"));
        }
        if rel.starts_with(".git") || rel.contains("/.git/") {
            return Err(anyhow!("path under .git/ not allowed: {rel}"));
        }
        let p = Path::new(rel);
        for comp in p.components() {
            use std::path::Component::*;
            match comp {
                Normal(_) => {}
                CurDir => {}
                ParentDir => return Err(anyhow!("path traversal not allowed: {rel}")),
                RootDir | Prefix(_) => return Err(anyhow!("absolute path not allowed: {rel}")),
            }
        }
        Ok(self.root.join(p))
    }

    pub(crate) async fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .await
            .with_context(|| format!("spawn git {args:?}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn walk_md(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" { continue; }
        if path.is_dir() {
            walk_md(root, &path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

fn exact_filing(key: &str, query: &str) -> bool {
    !query.is_empty() && key.to_lowercase().trim_start_matches('/') == query.trim_start_matches('/')
}

fn relevance(query: &str, title: &str, content: &str) -> usize {
    let title = title.to_lowercase();
    let content = content.to_lowercase();
    let terms: HashSet<_> = query.split(|c: char| !c.is_alphanumeric()).filter(|term| term.len() > 1
        && !matches!(*term, "the" | "and" | "what" | "was" | "in" | "of" | "to" | "is" | "for")).collect();
    terms.into_iter().map(|term| usize::from(content.contains(term)) + 3 * usize::from(title.contains(term))).sum()
}

// Match a literal search or a folder (including an ancestor) named in a question.
fn filing_matches(key: &str, query: &str) -> bool {
    let key = key.to_lowercase();
    if key.contains(query) { return true; }
    let relative = key.trim_start_matches('/');
    let boundary = |c: char| c.is_whitespace() || "\"'()[]?!,.:".contains(c);
    relative.match_indices('/').map(|(index, _)| index).chain(std::iter::once(relative.len())).any(|end| {
        let prefix = &relative[..end];
        query.match_indices(prefix).any(|(i, _)| {
            let before = query[..i].trim_end_matches('/').chars().next_back();
            let after = &query[i + end..];
            let after = after.strip_prefix('/').unwrap_or(after).chars().next();
            before.is_none_or(boundary) && after.is_none_or(boundary)
        })
    })
}

/// Convenience helper for callers that want a current ISO-8601 timestamp string.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

/// Manages one git repo per tenant. Lazy-initializes on first lookup so
/// every tenant — including ones that materialize on a user's first
/// sign-in — gets a fresh wiki without manual setup.
#[derive(Clone)]
pub struct WikiRepoStore {
    root: PathBuf,
    author_name: String,
    author_email: String,
    cache: Arc<Mutex<HashMap<String, WikiRepo>>>,
}

impl WikiRepoStore {
    pub fn new(
        root: impl AsRef<Path>,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
    ) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            author_name: author_name.into(),
            author_email: author_email.into(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Open or initialize the wiki repo for `tenant`. Repos live under
    /// `<root>/<tenant>/`, one git history per tenant. WikiRepo is
    /// cheap-Clone (PathBuf + a couple of strings), so callers get
    /// their own copy without needing Arc.
    pub async fn get(&self, tenant: &Tenant) -> Result<WikiRepo> {
        {
            let guard = self.cache.lock().await;
            if let Some(r) = guard.get(tenant.as_str()) {
                return Ok(r.clone());
            }
        }
        let path = self.root.join(tenant.as_str());
        let repo = WikiRepo::open_or_init(&path, &self.author_name, &self.author_email).await?;
        let mut guard = self.cache.lock().await;
        guard
            .entry(tenant.as_str().to_string())
            .or_insert_with(|| repo.clone());
        Ok(repo)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub snippet: String,
}

fn extract_title(content: &str) -> Option<String> {
    // Prefer first markdown H1 outside the frontmatter block.
    let mut in_fm = false;
    let mut seen_fm_open = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            if !seen_fm_open { in_fm = true; seen_fm_open = true; continue; }
            if in_fm { in_fm = false; continue; }
        }
        if in_fm { continue; }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
    // Fallback: frontmatter title field.
    let mut in_fm2 = false;
    let mut opens = 0;
    for line in content.lines() {
        let t = line.trim();
        if t == "---" { opens += 1; in_fm2 = opens == 1; continue; }
        if !in_fm2 { continue; }
        if let Some(rest) = t.strip_prefix("title:") {
            return Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}
