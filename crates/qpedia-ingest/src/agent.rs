//! The ingest agent: bounded tool-using LLM loop that produces a DiffBundle.
//! See DESIGN.md §6.
//!
//! Uses provider-native tool use (Anthropic `tool_use`/`tool_result` blocks,
//! OpenAI `tool_calls` arrays). The system prompt no longer carries protocol
//! instructions — the LLM API itself enforces tool shape and parsing.

use anyhow::{anyhow, Result};
use chrono::Utc;
use qpedia_core::{
    source::Source,
    tenant::Tenant,
    wiki::{DiffBundle, DiffOp},
    SourceId,
};
use qpedia_embed::Embedder;
use qpedia_llm::{current_model, CompleteReq, LlmProvider, Message, ToolCall, ToolDef};
use qpedia_pg_store::PgStore;
use qpedia_store::{
    blob::{BlobKind, BlobStorage, BlobStore},
    wikirepo::SearchHit,
    WikiRepo,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info};

// Budgets — see DESIGN.md §6.2.
pub const MAX_TURNS: u32 = 18;
pub const MAX_OPS: usize = 20;
pub const MAX_BUNDLE_BYTES: usize = 500 * 1024;
pub const MAX_PAGE_BYTES: usize = 50 * 1024;
pub const MAX_TOOL_RESULT_CHARS: usize = 6000;
pub const SOURCE_SNIPPET_CHARS: usize = 12_000;

const SYSTEM_INSTRUCTIONS: &str = r#"
You are integrating ONE new source into an existing wiki. Use the tools to
read the source, look at relevant existing pages, then propose new or
patched pages. Call `done(summary)` exactly once at the end.

PAGE FORMAT — required for every wiki page

Every page (new or patched) must start with YAML frontmatter:

---
title: "<noun phrase>"
kind: summary | concept | entity | comparison
source_ids: ["<source ulid>", ...]
tags: ["short", "lowercase", "tags"]
---

# <Title (same as frontmatter)>

<body in markdown>

Cite source facts as [^src:<source_id>].
Link other wiki pages as [[concepts/foo.md]].

INGEST PROTOCOL

1. read_source(SOURCE_ID) to ground yourself.
2. search_wiki for topics this source touches; read_page on candidates.
3. propose_new for a summary at "summaries/<SOURCE_ID>.md".
4. propose_new or propose_patch for any concept/entity pages (1–3 max).
5. propose_patch on "index.md" to list new pages under their sections.
6. propose_patch on "log.md" — APPEND one timestamped line.
7. done(summary) with one sentence.

BUDGETS

- Max 18 turns, 20 staged operations, 500KB bundle, 50KB per page.
- Stop early if the source is redundant — call done with a note.
"#;

pub struct StagedOps {
    pub ops: Vec<DiffOp>,
    pub created_paths: std::collections::HashSet<String>,
    pub patched_paths: std::collections::HashSet<String>,
    pub bytes: usize,
}

impl StagedOps {
    fn new() -> Self {
        Self {
            ops: Vec::new(),
            created_paths: Default::default(),
            patched_paths: Default::default(),
            bytes: 0,
        }
    }
}

pub struct AgentDeps<'a> {
    pub llm: Arc<dyn LlmProvider>,
    pub wiki: &'a WikiRepo,
    pub tenant: Tenant,
    pub blob: &'a BlobStore,
    pub db: &'a PgStore,
    pub embedder: Option<Arc<dyn Embedder>>,
}

/// Run the agent on a source, producing a DiffBundle.
/// Caller commits, audits, and updates status.
pub async fn run_agent(deps: &AgentDeps<'_>, src: &Source) -> Result<DiffBundle> {
    let qpedia_md = deps
        .wiki
        .read_page("QPEDIA.md")
        .await?
        .unwrap_or_else(|| "(QPEDIA.md missing)".into());

    let system = format!("{qpedia_md}\n\n---\n{SYSTEM_INSTRUCTIONS}");
    let user_msg = build_initial_user_msg(src);

    let tools = tool_defs();
    let mut messages: Vec<Message> = vec![Message::user(user_msg)];
    let mut staged = StagedOps::new();

    for turn in 0..MAX_TURNS {
        let req = CompleteReq {
            model: current_model(),
            system: Some(system.clone()),
            messages: messages.clone(),
            tools: tools.clone(),
            max_tokens: 4096,
            temperature: 0.1,
        };

        let resp = deps
            .llm
            .complete(req)
            .await
            .map_err(|e| anyhow!("agent llm call: {e}"))?;

        debug!(
            turn,
            text_len = resp.content.len(),
            tool_calls = resp.tool_calls.len(),
            "agent turn"
        );

        // Record assistant turn with both text and tool_calls so the next
        // request has the full history (provider-native turn shape).
        messages.push(Message::assistant(resp.content.clone(), resp.tool_calls.clone()));

        if resp.tool_calls.is_empty() {
            return Err(anyhow!(
                "agent stopped without calling a tool (turn {turn}); \
                 last text: {}",
                resp.content
            ));
        }

        // Execute each tool call, appending a Tool-role message per result.
        let mut done_summary: Option<String> = None;
        for call in &resp.tool_calls {
            if call.name == "done" {
                done_summary = Some(
                    call.arguments
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                );
                messages.push(Message::tool(&call.id, "ok", false));
                continue;
            }

            let exec = execute_tool(call, deps, src, &mut staged).await;
            let (text, is_error) = match exec {
                Ok(r) => (truncate(&r, MAX_TOOL_RESULT_CHARS), false),
                Err(e) => (format!("error: {e}"), true),
            };
            messages.push(Message::tool(&call.id, text, is_error));

            if staged.ops.len() > MAX_OPS {
                return Err(anyhow!("agent exceeded op budget ({} > {})", staged.ops.len(), MAX_OPS));
            }
            if staged.bytes > MAX_BUNDLE_BYTES {
                return Err(anyhow!("agent exceeded bundle byte cap"));
            }
        }

        if let Some(summary) = done_summary {
            info!(turn, ops = staged.ops.len(), %summary, "agent done");
            return Ok(DiffBundle {
                ingest_id: src.id.as_str().to_string(),
                summary,
                operations: staged.ops,
            });
        }
    }

    Err(anyhow!("agent exhausted {MAX_TURNS} turns without calling done"))
}

fn build_initial_user_msg(src: &Source) -> String {
    let (doc_type, language, hints) = src
        .classification
        .as_ref()
        .map(|c| {
            let dt = c.get("doc_type").and_then(|v| v.as_str()).unwrap_or("unknown");
            let lang = c.get("language").and_then(|v| v.as_str()).unwrap_or("und");
            let hints = c
                .get("hints")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            (dt.to_string(), lang.to_string(), hints)
        })
        .unwrap_or_else(|| ("unknown".into(), "und".into(), String::new()));

    format!(
        "INGEST_REQUEST\n\
         SOURCE_ID: {sid}\n\
         Filename: {fname}\n\
         Mime: {mime}\n\
         Doc type: {dt}\n\
         Language: {lang}\n\
         Hints: {hints}\n\
         \n\
         Use the tools to read this source, look at the existing wiki, and \
         decide what to write or update. Begin.",
        sid = src.id, fname = src.filename, mime = src.mime,
        dt = doc_type, lang = language, hints = hints,
    )
}

fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit { return s.to_string(); }
    let mut out = s.chars().take(limit).collect::<String>();
    out.push_str("\n... [truncated]");
    out
}

// ---------- tool definitions ----------

fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "search_wiki".into(),
            description:
                "Hybrid (BM25 + vector) search over wiki pages. Returns array of {path, title, snippet}.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "search query"},
                    "limit": {"type": "integer", "default": 5, "minimum": 1, "maximum": 20}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "list_pages".into(),
            description: "List wiki page paths under a prefix (e.g. 'concepts/' or '' for all).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prefix": {"type": "string", "default": ""}
                },
                "required": []
            }),
        },
        ToolDef {
            name: "read_page".into(),
            description: "Return the full markdown of a wiki page (frontmatter + body).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "read_source".into(),
            description:
                "Return the first ~12000 chars of a raw source's extracted text. Use on the SOURCE_ID you were given.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"source_id": {"type": "string"}},
                "required": []
            }),
        },
        ToolDef {
            name: "propose_new".into(),
            description:
                "Stage a NEW wiki page. content is full markdown including frontmatter. Path must not already exist.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "e.g. summaries/<source_id>.md"},
                    "content": {"type": "string", "description": "full markdown including --- frontmatter"},
                    "rationale": {"type": "string"}
                },
                "required": ["path", "content", "rationale"]
            }),
        },
        ToolDef {
            name: "propose_patch".into(),
            description:
                "Stage a REPLACEMENT for an existing page. new_content is the full markdown.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "new_content": {"type": "string"},
                    "rationale": {"type": "string"}
                },
                "required": ["path", "new_content", "rationale"]
            }),
        },
        ToolDef {
            name: "done".into(),
            description:
                "Finalize. Runtime validates and commits all staged ops as one git commit. Call exactly once.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"summary": {"type": "string"}},
                "required": ["summary"]
            }),
        },
    ]
}

// ---------- tool dispatch ----------

async fn execute_tool(
    call: &ToolCall,
    deps: &AgentDeps<'_>,
    src: &Source,
    staged: &mut StagedOps,
) -> Result<String> {
    let args = &call.arguments;
    match call.name.as_str() {
        "search_wiki" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let hits: Vec<SearchHit> = match &deps.embedder {
                Some(embedder) => {
                    let qv = embedder
                        .embed(&[query])
                        .await?
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                    match deps
                        .db
                        .hybrid_search(&deps.tenant, query, qv, 0.7, limit as i64)
                        .await
                    {
                        Ok(rows) if !rows.is_empty() => rows
                            .into_iter()
                            .map(|r| SearchHit {
                                path: r.path,
                                title: r.title,
                                snippet: r.snippet,
                            })
                            .collect(),
                        Ok(_) => deps.wiki.search_text(query, limit).await?,
                        Err(e) => {
                            tracing::warn!(error = %e, "pg hybrid search failed, falling back to fs");
                            deps.wiki.search_text(query, limit).await?
                        }
                    }
                }
                _ => deps.wiki.search_text(query, limit).await?,
            };
            Ok(serde_json::to_string(&hits)?)
        }
        "list_pages" => {
            let prefix = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
            let pages = deps.wiki.list_pages(prefix).await?;
            Ok(serde_json::to_string(&pages)?)
        }
        "read_page" => {
            let path = require_str(args, "path")?;
            for op in &staged.ops {
                match op {
                    DiffOp::Create { path: p, content, .. }
                    | DiffOp::Patch { path: p, new_content: content, .. }
                        if p == path =>
                    {
                        return Ok(content.clone());
                    }
                    _ => {}
                }
            }
            match deps.wiki.read_page(path).await? {
                Some(c) => Ok(c),
                None => Err(anyhow!("not found: {path}")),
            }
        }
        "read_source" => {
            let sid = args.get("source_id").and_then(|v| v.as_str()).unwrap_or(src.id.as_str());
            let bytes = deps
                .blob
                .get(&SourceId::from(sid.to_string()), BlobKind::Extracted, "txt")
                .await?;
            let text = String::from_utf8_lossy(&bytes).to_string();
            Ok(text.chars().take(SOURCE_SNIPPET_CHARS).collect())
        }
        "propose_new" => {
            let path = require_str(args, "path")?;
            let content = require_str(args, "content")?;
            let rationale = args.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string();
            check_path(path)?;
            check_page_size(path, content)?;
            if staged.created_paths.contains(path)
                || staged.patched_paths.contains(path)
                || deps.wiki.read_page(path).await?.is_some()
            {
                return Err(anyhow!("path already exists or already staged: {path}"));
            }
            let content = ensure_frontmatter_id_and_timestamps(content, path);
            staged.bytes += content.len();
            staged.created_paths.insert(path.to_string());
            staged.ops.push(DiffOp::Create { path: path.to_string(), content, rationale });
            Ok("ok".into())
        }
        "propose_patch" => {
            let path = require_str(args, "path")?;
            let new_content = require_str(args, "new_content")?;
            let rationale = args.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string();
            check_path(path)?;
            check_page_size(path, new_content)?;
            if !staged.created_paths.contains(path)
                && deps.wiki.read_page(path).await?.is_none()
            {
                return Err(anyhow!("path not found: {path}"));
            }
            staged.bytes += new_content.len();
            staged.patched_paths.insert(path.to_string());
            staged.ops.push(DiffOp::Patch {
                path: path.to_string(),
                new_content: new_content.to_string(),
                rationale,
            });
            Ok("ok".into())
        }
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn require_str<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("missing {key}"))
}

fn check_path(path: &str) -> Result<()> {
    if path.starts_with('/') || path.contains("..") {
        return Err(anyhow!("invalid path: {path}"));
    }
    if path == "QPEDIA.md" {
        return Err(anyhow!("forbidden: QPEDIA.md is system-owned"));
    }
    if !path.ends_with(".md") {
        return Err(anyhow!("path must end in .md: {path}"));
    }
    Ok(())
}

fn check_page_size(path: &str, content: &str) -> Result<()> {
    if content.len() > MAX_PAGE_BYTES {
        return Err(anyhow!(
            "page {path} exceeds {MAX_PAGE_BYTES} bytes ({} provided)",
            content.len()
        ));
    }
    Ok(())
}

/// Inject `id` and timestamps into frontmatter if the agent forgot.
/// Idempotent: if those fields exist, returns content unchanged.
fn ensure_frontmatter_id_and_timestamps(content: &str, _path: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let id = ulid::Ulid::new().to_string();

    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return format!(
            "---\nid: {id}\ncreated_at: {now}\nupdated_at: {now}\n---\n{content}"
        );
    }
    let after_first = trimmed.strip_prefix("---").unwrap();
    if let Some(end_rel) = after_first.find("\n---") {
        let mut fm = after_first[..end_rel].to_string();
        let body = &after_first[end_rel + "\n---".len()..];
        let mut additions = String::new();
        if !fm.contains("id:") { additions.push_str(&format!("\nid: {id}")); }
        if !fm.contains("created_at:") { additions.push_str(&format!("\ncreated_at: {now}")); }
        if !fm.contains("updated_at:") { additions.push_str(&format!("\nupdated_at: {now}")); }
        fm.push_str(&additions);
        format!("---{fm}\n---{body}")
    } else {
        content.to_string()
    }
}
