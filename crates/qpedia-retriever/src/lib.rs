//! Query-time retrieval: hybrid search over Weaviate, load top-K wiki pages,
//! stream a grounded answer from the LLM with citations.
//! See DESIGN.md §8.
//!
//! v1 is single-shot — no graph walk yet. The model gets the retrieved pages
//! verbatim as context and must cite them inline as `[[path/to/page.md]]`.
//! The agentic graph-walk (DESIGN.md §8.2) is a strict superset that drops
//! in here once tool-streaming is wired.

use anyhow::Result;
use futures::stream::{Stream, StreamExt};
use qpedia_embed::Embedder;
use qpedia_llm::{current_model, CompleteReq, LlmProvider, Message, Role};
use qpedia_store::{weaviate::WeaviateStore, SearchHit, WikiRepo};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

const SYSTEM_PROMPT: &str = r#"
You are answering a question using the qpedia wiki. The user is shown the
retrieved pages alongside your answer.

Rules:
- Synthesize an answer from the retrieved pages when they contain the answer.
- Cite pages inline as [[path/to/page.md]] when you draw on them.
- If the retrieved pages do NOT contain the answer, say so plainly and
  suggest a refinement or what to ingest. Do not fabricate.
- Be concise. Bullet points for fact-dense answers; short paragraphs otherwise.
"#;

/// What a chat caller hands us per turn.
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

/// One event emitted to the chat stream.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// Sent first: which pages were retrieved, and how (hybrid|filesystem).
    Meta { retrieved: Vec<Citation>, mode: String },
    /// A token of the answer.
    Token { text: String },
    /// Stream finished cleanly.
    Done,
    /// Stream aborted with an error.
    Error { message: String },
}

#[derive(Clone)]
pub struct Retriever {
    pub embedder: Option<Arc<dyn Embedder>>,
    pub weaviate: Option<Arc<WeaviateStore>>,
    pub wiki: WikiRepo,
    pub llm: Arc<dyn LlmProvider>,
}

impl Retriever {
    /// Stream a chat reply: retrieve, prompt, stream tokens, citations.
    pub fn chat(self, req: ChatRequest) -> impl Stream<Item = ChatEvent> + Send + 'static {
        async_stream::stream! {
            // 1. Retrieve.
            let (mode, hits) = match self.retrieve(&req.message, req.max_pages.max(1).min(20)).await {
                Ok(r) => r,
                Err(e) => {
                    yield ChatEvent::Error { message: format!("retrieve: {e}") };
                    return;
                }
            };

            // 2. Load full markdown for each hit.
            let mut pages: Vec<(String, String, String)> = Vec::new(); // (path, title, content)
            for h in &hits {
                match self.wiki.read_page(&h.path).await {
                    Ok(Some(c)) => pages.push((h.path.clone(), h.title.clone(), c)),
                    Ok(None)    => warn!(path = %h.path, "search hit missing on disk"),
                    Err(e)      => warn!(path = %h.path, error = %e, "page read failed"),
                }
            }

            yield ChatEvent::Meta {
                retrieved: pages.iter().map(|(p, t, _)| Citation {
                    path: p.clone(), title: t.clone(),
                }).collect(),
                mode: mode.into(),
            };

            // 3. Build prompt: retrieved pages + history + current message.
            let user_msg = build_user_msg(&req.message, &pages);

            let mut messages: Vec<Message> = Vec::with_capacity(req.history.len() + 1);
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
            messages.push(Message::user(user_msg));

            let llm_req = CompleteReq {
                model: current_model(),
                system: Some(SYSTEM_PROMPT.into()),
                messages,
                tools: Vec::new(),
                max_tokens: 2048,
                temperature: 0.3,
            };

            // 4. Stream tokens from the LLM.
            let mut token_stream = match self.llm.stream(llm_req).await {
                Ok(s) => s,
                Err(e) => {
                    yield ChatEvent::Error { message: format!("llm stream: {e}") };
                    return;
                }
            };

            let mut total = 0usize;
            while let Some(item) = token_stream.next().await {
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
            info!(chars = total, pages = pages.len(), "chat stream complete");

            yield ChatEvent::Done;
        }
    }

    async fn retrieve(&self, query: &str, k: usize) -> Result<(&'static str, Vec<SearchHit>)> {
        if let (Some(emb), Some(wv)) = (&self.embedder, &self.weaviate) {
            let qv = emb.embed(&[query]).await?.into_iter().next().unwrap_or_default();
            match wv.hybrid_search(query, &qv, k).await {
                Ok(h) if !h.is_empty() => return Ok(("hybrid", h)),
                Ok(_) => {}
                Err(e) => warn!(error = %e, "weaviate search failed; falling back"),
            }
        }
        let hits = self.wiki.search_text(query, k).await?;
        Ok(("filesystem", hits))
    }
}

fn build_user_msg(question: &str, pages: &[(String, String, String)]) -> String {
    let mut s = String::with_capacity(2048);
    if pages.is_empty() {
        s.push_str("RETRIEVED PAGES: (none — wiki search returned no results)\n\n");
    } else {
        s.push_str("RETRIEVED PAGES:\n\n");
        for (i, (path, _title, content)) in pages.iter().enumerate() {
            s.push_str(&format!("--- {}. {}\n", i + 1, path));
            // Trim each page's content so the prompt stays bounded; LLMWiki
            // pages are already short by design (300-2000 words).
            let truncated: String = content.chars().take(8000).collect();
            s.push_str(&truncated);
            if !truncated.ends_with('\n') { s.push('\n'); }
            s.push('\n');
        }
    }
    s.push_str("QUESTION: ");
    s.push_str(question);
    s
}
