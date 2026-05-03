//! Query-time retrieval. See DESIGN.md §8.
//!
//! Two-phase agentic retriever:
//!   1. **Gather**: bounded tool loop where the LLM uses
//!      `search_wiki` / `read_page` / `follow_links` / `done` to
//!      collect the pages it needs. Non-streaming, non-tool replies
//!      end the loop early. Hard caps on turns, pages loaded, and
//!      total content bytes prevent runaway walks.
//!   2. **Synthesize**: streaming completion with all gathered pages
//!      as the user prompt and no tools. Tokens flow out via SSE.
//!
//! ACL filtering is applied inside every tool call so the agent never
//! sees content the caller can't read.

use anyhow::{anyhow, Result};
use futures::stream::{Stream, StreamExt};
use qpedia_embed::Embedder;
use qpedia_llm::{
    current_model, CompleteReq, LlmProvider, Message, Role, ToolCall, ToolDef,
};
use qpedia_store::{
    sqlite::SourceStore, weaviate::WeaviateStore, SearchHit, SqliteStore, WikiRepo,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use tracing::{debug, info, warn};

// ---------- budgets ----------

pub const GATHER_TURNS: u32 = 6;
pub const MAX_PAGES_LOADED: usize = 8;
pub const MAX_CONTEXT_BYTES: usize = 30_000;
pub const MAX_TOOL_RESULT_CHARS: usize = 6_000;

// ---------- prompts ----------

const GATHER_SYSTEM: &str = r#"
You are gathering context to answer a user question from the qpedia
wiki. Use the tools to search and read relevant pages. When you have
enough context, call done() and the system will synthesize the
answer for the user.

Rules:
- Prefer search_wiki first to find candidate pages.
- Use read_page on the 2-5 most promising hits.
- Use follow_links to surface closely-related pages when useful.
- Stop once you have enough to answer — don't read more than ~5 pages.
- If the wiki contains nothing relevant, still call done() — the
  synthesis phase will tell the user honestly.
"#;

const SYNTHESIS_SYSTEM: &str = r#"
You are answering a question using the qpedia wiki. The user is shown
the retrieved pages alongside your answer.

Rules:
- Synthesize an answer from the retrieved pages when they contain it.
- Cite pages inline as [[path/to/page.md]] when you draw on them.
- If the retrieved pages do NOT contain the answer, say so plainly
  and suggest a refinement or what to ingest. Do not fabricate.
- Be concise. Bullet points for fact-dense content; short paragraphs
  otherwise.
"#;

// ---------- public types ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub message: String,
    #[serde(default)]
    pub history: Vec<ChatTurn>,
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
}

fn default_max_pages() -> usize { 5 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    pub path: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    Meta { retrieved: Vec<Citation>, mode: String },
    Token { text: String },
    Done,
    Error { message: String },
}

#[derive(Clone)]
pub struct Retriever {
    pub embedder: Option<Arc<dyn Embedder>>,
    pub weaviate: Option<Arc<WeaviateStore>>,
    pub wiki: WikiRepo,
    pub db: SqliteStore,
    pub llm: Arc<dyn LlmProvider>,
    /// Caller-provided groups. Empty = anonymous (admin-only ACL pages
    /// won't be readable). Set to ["admin"] for unrestricted access.
    pub user_groups: Vec<String>,
}

// ---------- gathered page ----------

#[derive(Clone)]
struct GatheredPage {
    path: String,
    title: String,
    content: String,
}

// ---------- run ----------

impl Retriever {
    pub fn chat(self, req: ChatRequest) -> impl Stream<Item = ChatEvent> + Send + 'static {
        async_stream::stream! {
            // Phase 1: gather context via agent loop.
            let gathered = match self.gather(&req).await {
                Ok(g) => g,
                Err(e) => {
                    yield ChatEvent::Error { message: format!("gather: {e}") };
                    return;
                }
            };

            let mode = if self.weaviate.is_some() { "hybrid+graph" } else { "filesystem+graph" };
            yield ChatEvent::Meta {
                retrieved: gathered.iter().map(|p| Citation {
                    path: p.path.clone(),
                    title: p.title.clone(),
                }).collect(),
                mode: mode.into(),
            };

            // Phase 2: synthesize a streaming answer with no tools.
            let messages = build_synthesis_messages(&req, &gathered);
            let llm_req = CompleteReq {
                model: current_model(),
                system: Some(SYNTHESIS_SYSTEM.into()),
                messages,
                tools: Vec::new(),
                max_tokens: 2048,
                temperature: 0.3,
            };

            let mut stream = match self.llm.stream(llm_req).await {
                Ok(s) => s,
                Err(e) => {
                    yield ChatEvent::Error { message: format!("llm stream: {e}") };
                    return;
                }
            };

            let mut total = 0usize;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(t) => {
                        total += t.text.len();
                        yield ChatEvent::Token { text: t.text };
                    }
                    Err(e) => {
                        yield ChatEvent::Error { message: format!("llm: {e}") };
                        return;
                    }
                }
            }
            info!(chars = total, pages = gathered.len(), "agentic chat stream complete");
            yield ChatEvent::Done;
        }
    }

    /// Phase 1: bounded agent loop. Returns the pages the agent decided
    /// were worth loading. Falls back to a single hybrid_search if the
    /// agent loop fails or yields nothing.
    async fn gather(&self, req: &ChatRequest) -> Result<Vec<GatheredPage>> {
        let tools = gather_tool_defs();
        let mut messages: Vec<Message> = Vec::new();
        for h in &req.history {
            let role = match h.role {
                ChatRole::User => Role::User,
                ChatRole::Assistant => Role::Assistant,
            };
            messages.push(Message {
                role,
                content: h.content.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                is_error: None,
            });
        }
        messages.push(Message::user(format!(
            "USER QUESTION:\n{}\n\nGather the wiki context you need, then call done().",
            req.message
        )));

        let mut loaded: HashMap<String, GatheredPage> = HashMap::new();
        let mut total_bytes = 0usize;

        for turn in 0..GATHER_TURNS {
            let llm_req = CompleteReq {
                model: current_model(),
                system: Some(GATHER_SYSTEM.into()),
                messages: messages.clone(),
                tools: tools.clone(),
                max_tokens: 1024,
                temperature: 0.0,
            };
            let resp = self.llm.complete(llm_req).await
                .map_err(|e| anyhow!("gather llm: {e}"))?;

            debug!(turn, calls = resp.tool_calls.len(), "gather turn");
            messages.push(Message::assistant(resp.content.clone(), resp.tool_calls.clone()));

            if resp.tool_calls.is_empty() {
                // No tool — treat as "ready, synthesize".
                break;
            }

            let mut done_signaled = false;
            for tc in &resp.tool_calls {
                if tc.name == "done" {
                    done_signaled = true;
                    messages.push(Message::tool(&tc.id, "ok", false));
                    continue;
                }
                let exec = self.execute_gather_tool(tc, &mut loaded, &mut total_bytes).await;
                let (text, is_error) = match exec {
                    Ok(r) => (truncate(&r, MAX_TOOL_RESULT_CHARS), false),
                    Err(e) => (format!("error: {e}"), true),
                };
                messages.push(Message::tool(&tc.id, text, is_error));

                if loaded.len() >= MAX_PAGES_LOADED || total_bytes >= MAX_CONTEXT_BYTES {
                    done_signaled = true;
                    break;
                }
            }
            if done_signaled { break; }
        }

        // Fallback: if the agent loaded nothing, do a one-shot hybrid search
        // so the synthesis phase isn't operating on an empty context.
        if loaded.is_empty() {
            let max = req.max_pages.max(1).min(MAX_PAGES_LOADED);
            let hits = self.do_search(&req.message, max).await.unwrap_or_default();
            for h in hits {
                if let Ok(Some(content)) = self.wiki.read_page(&h.path).await {
                    if self.user_can_read_page(&content).await {
                        loaded.insert(h.path.clone(), GatheredPage {
                            path: h.path,
                            title: h.title,
                            content,
                        });
                        if loaded.len() >= max { break; }
                    }
                }
            }
        }

        let mut out: Vec<GatheredPage> = loaded.into_values().collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn execute_gather_tool(
        &self,
        tc: &ToolCall,
        loaded: &mut HashMap<String, GatheredPage>,
        total_bytes: &mut usize,
    ) -> Result<String> {
        let args = &tc.arguments;
        match tc.name.as_str() {
            "search_wiki" => {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
                let hits = self.do_search(query, limit.max(1).min(20)).await?;
                // ACL filter — drop pages the user can't read.
                let mut allowed = Vec::with_capacity(hits.len());
                for h in hits {
                    if let Ok(Some(c)) = self.wiki.read_page(&h.path).await {
                        if self.user_can_read_page(&c).await {
                            allowed.push(h);
                        }
                    }
                }
                Ok(serde_json::to_string(&allowed)?)
            }
            "read_page" => {
                let path = args.get("path").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing path"))?;
                let content = self.wiki.read_page(path).await?
                    .ok_or_else(|| anyhow!("not found: {path}"))?;
                if !self.user_can_read_page(&content).await {
                    return Err(anyhow!("not found: {path}")); // info-leak avoidance
                }
                let title = extract_title(&content).unwrap_or_else(|| path.to_string());

                // Track in `loaded` for the synthesis phase.
                if !loaded.contains_key(path) {
                    let take_chars = (MAX_CONTEXT_BYTES.saturating_sub(*total_bytes)).max(1024);
                    let truncated: String = content.chars().take(take_chars).collect();
                    *total_bytes += truncated.len();
                    loaded.insert(path.into(), GatheredPage {
                        path: path.into(),
                        title,
                        content: truncated,
                    });
                }
                // Return the full content to the agent's context.
                Ok(content)
            }
            "follow_links" => {
                let from = args.get("from").and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("missing from"))?;
                let content = self.wiki.read_page(from).await?
                    .ok_or_else(|| anyhow!("not found: {from}"))?;
                if !self.user_can_read_page(&content).await {
                    return Err(anyhow!("not found: {from}"));
                }
                let links: Vec<String> = extract_wikilinks(&content)
                    .into_iter()
                    .map(|t| t.split('#').next().unwrap_or(&t).to_string())
                    .collect();
                Ok(serde_json::to_string(&links)?)
            }
            other => Err(anyhow!("unknown tool: {other}")),
        }
    }

    async fn do_search(&self, query: &str, k: usize) -> Result<Vec<SearchHit>> {
        if let (Some(emb), Some(wv)) = (&self.embedder, &self.weaviate) {
            let qv = emb.embed(&[query]).await?.into_iter().next().unwrap_or_default();
            match wv.hybrid_search(query, &qv, k).await {
                Ok(h) if !h.is_empty() => return Ok(h),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "weaviate search failed; falling back"),
            }
        }
        Ok(self.wiki.search_text(query, k).await?)
    }

    async fn user_can_read_page(&self, content: &str) -> bool {
        if self.user_groups.iter().any(|g| g == "admin") {
            return true;
        }
        let source_ids = parse_source_ids(content);
        if source_ids.is_empty() {
            return true; // system page (index/log/QPEDIA) — readable by all
        }
        let mut acl: BTreeSet<String> = BTreeSet::new();
        for sid in source_ids {
            if let Ok(Some(src)) = self.db.get_source(&sid.into()).await {
                for g in src.acl.0.iter() {
                    acl.insert(g.clone());
                }
            }
        }
        if acl.is_empty() {
            return false; // admin-only
        }
        self.user_groups.iter().any(|g| acl.contains(g))
    }
}

// ---------- helpers ----------

fn build_synthesis_messages(req: &ChatRequest, gathered: &[GatheredPage]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(req.history.len() + 1);
    for h in &req.history {
        let role = match h.role {
            ChatRole::User => Role::User,
            ChatRole::Assistant => Role::Assistant,
        };
        out.push(Message {
            role,
            content: h.content.clone(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: None,
        });
    }
    out.push(Message::user(build_user_msg(&req.message, gathered)));
    out
}

fn build_user_msg(question: &str, pages: &[GatheredPage]) -> String {
    let mut s = String::with_capacity(2048);
    if pages.is_empty() {
        s.push_str("RETRIEVED PAGES: (none — wiki search returned no results)\n\n");
    } else {
        s.push_str("RETRIEVED PAGES:\n\n");
        for (i, p) in pages.iter().enumerate() {
            s.push_str(&format!("--- {}. {}\n", i + 1, p.path));
            let truncated: String = p.content.chars().take(8000).collect();
            s.push_str(&truncated);
            if !truncated.ends_with('\n') { s.push('\n'); }
            s.push('\n');
        }
    }
    s.push_str("QUESTION: ");
    s.push_str(question);
    s
}

fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit { return s.to_string(); }
    let mut out = s.chars().take(limit).collect::<String>();
    out.push_str("\n... [truncated]");
    out
}

fn extract_title(content: &str) -> Option<String> {
    // Prefer frontmatter title; fall back to first H1.
    let trimmed = content.trim_start();
    if let Some(after) = trimmed.strip_prefix("---") {
        if let Some(end) = after.find("\n---") {
            for line in after[..end].lines() {
                let line = line.trim_start();
                if let Some(rest) = line.strip_prefix("title:") {
                    return Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
                }
            }
        }
    }
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn parse_source_ids(content: &str) -> Vec<String> {
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

/// Code-span aware [[wikilink]] extraction.
fn extract_wikilinks(content: &str) -> Vec<String> {
    let chars: Vec<char> = content.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut in_inline = false;
    let mut in_fence = false;
    while i < chars.len() {
        if !in_inline
            && i + 2 < chars.len()
            && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`'
        {
            in_fence = !in_fence;
            i += 3;
            continue;
        }
        if !in_fence && chars[i] == '`' {
            in_inline = !in_inline;
            i += 1;
            continue;
        }
        if !in_inline && !in_fence
            && i + 1 < chars.len() && chars[i] == '[' && chars[i + 1] == '['
        {
            let mut j = i + 2;
            while j + 1 < chars.len() && !(chars[j] == ']' && chars[j + 1] == ']') {
                j += 1;
            }
            if j + 1 < chars.len() {
                let target: String = chars[i + 2..j].iter().collect::<String>().trim().to_string();
                if !target.is_empty() {
                    out.push(target);
                }
                i = j + 2;
                continue;
            } else {
                break;
            }
        }
        i += 1;
    }
    out
}

// ---------- tool definitions ----------

fn gather_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "search_wiki".into(),
            description:
                "Hybrid (BM25 + vector) search over wiki pages. Returns array of {path, title, snippet}.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 5, "minimum": 1, "maximum": 20}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "read_page".into(),
            description:
                "Load the full markdown of a wiki page (frontmatter + body). The page is added to the context that will be shown to the user with the answer.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "follow_links".into(),
            description:
                "Return the [[wikilinks]] outbound from a wiki page (without loading its body). Use to traverse to related pages.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"from": {"type": "string"}},
                "required": ["from"]
            }),
        },
        ToolDef {
            name: "done".into(),
            description:
                "Stop gathering — you have enough context to answer. Call this exactly once at the end.".into(),
            input_schema: json!({"type": "object", "properties": {}, "required": []}),
        },
    ]
}
