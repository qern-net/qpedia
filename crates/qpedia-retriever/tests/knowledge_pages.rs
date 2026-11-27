use async_trait::async_trait;
use futures::{stream, stream::BoxStream, StreamExt};
use qpedia_core::{wiki::WikiPageScope, Tenant};
use qpedia_embed::reranker_from_env;
use qpedia_llm::{CompleteReq, CompleteResp, LlmError, LlmProvider, StopReason, Token, ToolCall};
use qpedia_retriever::{ChatEvent, ChatRequest, Retriever};
use serde_json::json;
use std::sync::Arc;

const CASE_PAGE: &str = "summaries/index.md";

struct FixtureModel { use_tools: bool }

#[async_trait]
impl LlmProvider for FixtureModel {
    fn name(&self) -> &str { "fixture" }

    async fn complete(&self, req: CompleteReq) -> Result<CompleteResp, LlmError> {
        let tool_results: Vec<_> = req.messages.iter().filter(|m| m.tool_call_id.is_some()).collect();
        let mut calls = Vec::new();
        if self.use_tools && tool_results.is_empty() {
            for (id, name, arguments) in [
                ("catalog", "read_page", json!({"path":"index.md"})),
                ("history", "read_page", json!({"path":"log.md"})),
                ("links", "follow_links", json!({"from":CASE_PAGE})),
                ("search", "search_wiki", json!({"query":"exhibit", "limit":1})),
                ("source", "read_page", json!({"path":CASE_PAGE})),
            ] {
                calls.push(ToolCall { id: id.into(), name: name.into(), arguments });
            }
        } else {
            if self.use_tools {
                assert_eq!(tool_results.len(), 5);
                assert!(tool_results.iter().all(|m| !m.content.contains("internal fixture")));
                assert_eq!(tool_results[0].is_error, Some(true));
                assert_eq!(tool_results[1].is_error, Some(true));
                assert_eq!(serde_json::from_str::<Vec<String>>(&tool_results[2].content).unwrap(), [CASE_PAGE]);
                assert!(tool_results[3].content.contains(CASE_PAGE));
            }
            calls.push(ToolCall { id:"stop".into(), name:"done".into(), arguments:json!({}) });
        }
        Ok(CompleteResp { content: String::new(), tool_calls: calls, usage: Default::default(),
                          stop_reason: StopReason::ToolUse })
    }

    async fn stream(&self, req: CompleteReq) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError> {
        let prompt = &req.messages.last().unwrap().content;
        assert!(prompt.contains("Filed case exhibit"));
        assert!(!prompt.contains("internal fixture"));
        Ok(stream::empty().boxed())
    }
}

#[tokio::test]
async fn chat_tools_and_fallback_cite_only_genuine_case_pages() -> anyhow::Result<()> {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else { return Ok(()); };
    let db = qpedia_pg_store::PgStore::connect(&url).await?;
    let dir = tempfile::tempdir()?;
    let wiki = qpedia_store::WikiRepo::open_or_init(dir.path(), "test", "test@example.com").await?;
    for path in ["index.md", "log.md", "QPEDIA.md", "_meta/state.md"] {
        tokio::fs::write(dir.path().join(path), "exhibit internal fixture").await?;
    }
    tokio::fs::write(dir.path().join(CASE_PAGE),
        "Filed case exhibit. [[index.md]] [[log.md]] [[_meta/state.md]] [[summaries/index.md#detail]]").await?;
    for use_tools in [true, false] {
        let retriever = Retriever { wiki: wiki.clone().with_page_scope(WikiPageScope::Knowledge), db: db.clone(),
            llm: Arc::new(FixtureModel { use_tools }), embedder: None, reranker: reranker_from_env(dir.path()),
            tenant: Tenant::new("case-fixture"), user_groups: vec!["admin".into()] };
        let events: Vec<_> = retriever.chat(ChatRequest { message: "exhibit".into(), max_pages: 1,
                                                         history: Vec::new() }).collect().await;
        let [ChatEvent::Meta { retrieved, .. }, ChatEvent::Done] = events.as_slice()
            else { panic!("unexpected chat events: {events:?}"); };
        assert_eq!(retrieved.iter().map(|citation| citation.path.as_str()).collect::<Vec<_>>(), [CASE_PAGE]);
    }
    Ok(())
}
