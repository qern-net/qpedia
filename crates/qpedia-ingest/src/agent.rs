//! The ingest agent: bounded tool-using LLM loop that produces a DiffBundle.
//! See DESIGN.md §6.
//!
//! v1 protocol: structured-text tool calls. Each assistant turn outputs a
//! single JSON object `{"tool": "<name>", "args": {...}}`. The runtime
//! executes the tool, appends a `Tool result for <name>: <json>` user turn,
//! and reprompts. The model signals completion with `tool: "done"`.
//!
//! Why text-not-native: the same protocol works across Anthropic / OpenAI /
//! local OSS models without a per-provider tool-API translation layer. We
//! pay a small reliability tax (model occasionally adds preamble), which the
//! parser tolerates by stripping fences and seeking the first `{`. Future
//! upgrade: switch to provider-native tool-use blocks for the highest-end
//! Claude/GPT models while keeping this protocol as the OSS fallback.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use qpedia_core::{
    source::Source,
    wiki::{DiffBundle, DiffOp},
    SourceId,
};
use qpedia_embed::Embedder;
use qpedia_llm::{current_model, CompleteReq, LlmProvider, Message, Role};
use qpedia_store::{
    blob::{BlobKind, BlobStorage, BlobStore},
    weaviate::WeaviateStore,
    wikirepo::SearchHit,
    SqliteStore, WikiRepo,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, info};

// Budgets — see DESIGN.md §6.2.
pub const MAX_TURNS: u32 = 18;
pub const MAX_OPS: usize = 20;
pub const MAX_BUNDLE_BYTES: usize = 500 * 1024;
pub const MAX_PAGE_BYTES: usize = 50 * 1024;
pub const MAX_TOOL_RESULT_CHARS: usize = 6000;
pub const SOURCE_SNIPPET_CHARS: usize = 12_000;

const PROTOCOL_INSTRUCTIONS: &str = r#"
TOOL PROTOCOL — read carefully.

Each of your turns must output ONE JSON object and nothing else:
  {"tool": "<name>", "args": { ... }}

No markdown fences, no preamble, no commentary. The runtime will execute the
tool and reply with a "Tool result for <name>:" message. Then it's your turn
again. Repeat until you call `done`.

AVAILABLE TOOLS

- search_wiki(query: string, limit?: int=5)
    Substring search across all wiki pages. Returns
    [{path, title, snippet}].

- list_pages(prefix: string)
    List wiki page paths under a prefix (e.g. "concepts/" or "" for all).

- read_page(path: string)
    Return the full markdown of a wiki page (frontmatter + body).

- read_source(source_id: string)
    Return the first ~12000 chars of a raw source's extracted text.
    Use this on the SOURCE_ID you were given to pull more context.

- propose_new(path: string, content: string, rationale: string)
    Stage a NEW wiki page. `content` is the full markdown including
    frontmatter (--- block). Path must not already exist.

- propose_patch(path: string, new_content: string, rationale: string)
    Stage a REPLACEMENT for an existing page. `new_content` is the full
    markdown (frontmatter + body). Path must already exist.

- done(summary: string)
    Finalize. The runtime validates and commits all staged changes as a
    single git commit. Call this exactly once at the end.

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

You are integrating ONE new source into an existing wiki. Steps you decide
the order of:

1. read_source(SOURCE_ID) to ground yourself.
2. search_wiki for topics this source touches; read_page on candidates.
3. propose_new for a summary at "summaries/<SOURCE_ID>.md".
4. propose_new or propose_patch for any concept/entity pages this source
   creates or refines (1-3 max in v1; don't go nuts).
5. propose_patch on "index.md" — add the new pages under their sections.
6. propose_patch on "log.md" — APPEND one timestamped line.
7. done(summary) with a one-sentence summary.

BUDGETS

- Max 18 turns, 20 staged operations, 500KB bundle, 50KB per page.
- Stop early if the source is redundant (call done with a note).

Begin.
"#;

#[derive(Debug, Deserialize)]
struct AgentToolCall {
    tool: String,
    #[serde(default)]
    args: serde_json::Value,
}

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
    pub blob: &'a BlobStore,
    pub db: &'a SqliteStore,
    pub embedder: Option<Arc<dyn Embedder>>,
    pub weaviate: Option<Arc<WeaviateStore>>,
}

/// Run the agent on a source, producing a DiffBundle.
/// Caller commits, audits, and updates status.
pub async fn run_agent(deps: &AgentDeps<'_>, src: &Source) -> Result<DiffBundle> {
    let qpedia_md = deps
        .wiki
        .read_page("QPEDIA.md")
        .await?
        .unwrap_or_else(|| "(QPEDIA.md missing)".into());

    let system = format!("{qpedia_md}\n\n---\n{PROTOCOL_INSTRUCTIONS}");
    let user_msg = build_initial_user_msg(src);

    let mut messages: Vec<Message> = vec![Message {
        role: Role::User,
        content: user_msg,
        tool_calls: Vec::new(),
    }];
    let mut staged = StagedOps::new();

    for turn in 0..MAX_TURNS {
        let req = CompleteReq {
            model: current_model(),
            system: Some(system.clone()),
            messages: messages.clone(),
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: 0.1,
        };

        let resp = deps.llm.complete(req).await
            .map_err(|e| anyhow!("agent llm call: {e}"))?;
        let assistant_text = resp.content.trim().to_string();
        debug!(turn, %assistant_text, "agent turn");

        // Record assistant turn so the next call has the full history.
        messages.push(Message {
            role: Role::Assistant,
            content: assistant_text.clone(),
            tool_calls: Vec::new(),
        });

        let call = parse_tool_call(&assistant_text)
            .with_context(|| format!("turn {turn} not a tool call:\n{assistant_text}"))?;

        if call.tool == "done" {
            let summary = call
                .args
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            info!(turn, ops = staged.ops.len(), %summary, "agent done");
            return Ok(DiffBundle {
                ingest_id: src.id.as_str().to_string(),
                summary,
                operations: staged.ops,
            });
        }

        let result = execute_tool(&call, deps, src, &mut staged)
            .await
            .map(|r| truncate(&r, MAX_TOOL_RESULT_CHARS))
            .unwrap_or_else(|e| format!("error: {e}"));

        messages.push(Message {
            role: Role::User,
            content: format!("Tool result for {}:\n{}", call.tool, result),
            tool_calls: Vec::new(),
        });

        if staged.ops.len() > MAX_OPS {
            return Err(anyhow!("agent exceeded op budget ({} > {})", staged.ops.len(), MAX_OPS));
        }
        if staged.bytes > MAX_BUNDLE_BYTES {
            return Err(anyhow!("agent exceeded bundle byte cap"));
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
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
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

fn parse_tool_call(text: &str) -> Result<AgentToolCall> {
    // Strip code fences if any and find the first {...} block.
    let trimmed = text.trim();
    let no_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_end_matches("```")
        .trim();
    let start = no_fence.find('{').ok_or_else(|| anyhow!("no JSON object"))?;
    let candidate = &no_fence[start..];
    let parsed: AgentToolCall =
        serde_json::from_str(candidate).map_err(|e| anyhow!("invalid JSON: {e}"))?;
    Ok(parsed)
}

fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit { return s.to_string(); }
    let mut out = s.chars().take(limit).collect::<String>();
    out.push_str("\n... [truncated]");
    out
}

async fn execute_tool(
    call: &AgentToolCall,
    deps: &AgentDeps<'_>,
    src: &Source,
    staged: &mut StagedOps,
) -> Result<String> {
    let args = &call.args;
    match call.tool.as_str() {
        "search_wiki" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let hits: Vec<SearchHit> = match (&deps.embedder, &deps.weaviate) {
                (Some(embedder), Some(weaviate)) => {
                    // Hybrid search via Weaviate. On any error, fall back to fs.
                    let qv = embedder.embed(&[query]).await?.into_iter().next().unwrap_or_default();
                    match weaviate.hybrid_search(query, &qv, limit).await {
                        Ok(h) if !h.is_empty() => h,
                        Ok(_) => deps.wiki.search_text(query, limit).await?,
                        Err(e) => {
                            tracing::warn!(error = %e, "weaviate search failed, falling back to fs");
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
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("missing path"))?;
            // Allow reading from staged-but-uncommitted ops too.
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
            let sid = args
                .get("source_id")
                .and_then(|v| v.as_str())
                .unwrap_or(src.id.as_str());
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
            staged.ops.push(DiffOp::Create {
                path: path.to_string(),
                content,
                rationale,
            });
            Ok("ok".into())
        }
        "propose_patch" => {
            let path = require_str(args, "path")?;
            let new_content = require_str(args, "new_content")?;
            let rationale = args.get("rationale").and_then(|v| v.as_str()).unwrap_or("").to_string();
            check_path(path)?;
            check_page_size(path, new_content)?;
            // Allow patching brand-new pages staged this turn (rare but legal).
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

fn require_str<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str> {
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

    // Quick check: locate the first frontmatter block.
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        // Wrap entire content with a minimal block.
        return format!(
            "---\nid: {id}\ncreated_at: {now}\nupdated_at: {now}\n---\n{content}"
        );
    }
    // Find the closing ---.
    let after_first = trimmed.strip_prefix("---").unwrap();
    if let Some(end_rel) = after_first.find("\n---") {
        let mut fm = after_first[..end_rel].to_string();
        let body = &after_first[end_rel + "\n---".len()..];
        let mut additions = String::new();
        if !fm.contains("id:") {
            additions.push_str(&format!("\nid: {id}"));
        }
        if !fm.contains("created_at:") {
            additions.push_str(&format!("\ncreated_at: {now}"));
        }
        if !fm.contains("updated_at:") {
            additions.push_str(&format!("\nupdated_at: {now}"));
        }
        fm.push_str(&additions);
        format!("---{fm}\n---{body}")
    } else {
        // Malformed frontmatter; leave alone, validator will reject.
        content.to_string()
    }
}
