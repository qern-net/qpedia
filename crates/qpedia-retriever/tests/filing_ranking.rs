use anyhow::Result;
use async_trait::async_trait;
use futures::{stream, stream::BoxStream, StreamExt};
use qpedia_core::{
    wiki::{DiffBundle, DiffOp, WikiPageScope},
    Tenant,
};
use qpedia_embed::{Embedder, Reranker};
use qpedia_llm::{CompleteReq, CompleteResp, LlmError, LlmProvider, StopReason, Token};
use qpedia_pg_store::{PgStore, WikiPageUpsert};
use qpedia_retriever::{ChatEvent, ChatRequest, Retriever};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct Model {
    calls: AtomicUsize,
    exact: bool,
}
#[async_trait]
impl Embedder for Model {
    fn name(&self) -> &str {
        "synthetic"
    }
    fn dimensions(&self) -> usize {
        384
    }
    async fn embed(&self, input: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(vec![vec![1.; 384]; input.len()])
    }
}
#[async_trait]
impl Reranker for Model {
    fn name(&self) -> &str {
        "synthetic"
    }
    async fn scores(&self, _: &str, docs: &[&str]) -> Result<Vec<f32>> {
        // Both filing AND content candidates must reach reranking before k=1.
        assert!(docs.len() >= 2);
        Ok(docs
            .iter()
            .map(|doc| {
                if doc.contains("Expert report") {
                    10.
                } else {
                    0.
                }
            })
            .collect())
    }
}
#[async_trait]
impl LlmProvider for Model {
    fn name(&self) -> &str {
        "synthetic"
    }
    async fn complete(&self, _: CompleteReq) -> Result<CompleteResp, LlmError> {
        Ok(CompleteResp {
            content: String::new(),
            tool_calls: vec![],
            usage: Default::default(),
            stop_reason: StopReason::EndTurn,
        })
    }
    async fn stream(
        &self,
        req: CompleteReq,
    ) -> Result<BoxStream<'static, Result<Token, LlmError>>, LlmError> {
        assert!(req
            .messages
            .last()
            .unwrap()
            .content
            .contains(if self.exact {
                "Travel receipt"
            } else {
                "Expert report"
            }));
        Ok(stream::empty().boxed())
    }
}

#[tokio::test]
async fn chat_searches_content_and_reranks_broad_filing_matches_before_limiting() -> Result<()> {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        return Ok(());
    };
    let db = PgStore::connect(&url).await?;
    let dir = tempfile::tempdir()?;
    let tenant = Tenant::new(
        format!(
            "retrieval-{}",
            dir.path().file_name().unwrap().to_string_lossy()
        )
        .to_lowercase(),
    );
    db.upsert_tenant(&tenant, "Synthetic fixture", None).await?;
    let wiki =
        qpedia_store::WikiRepo::open_or_init(dir.path(), "fixture", "fixture@example.test").await?;
    wiki.commit_bundle(&DiffBundle {ingest_id:"fixture".into(),summary:"fixture".into(),operations:vec![
        DiffOp::Create {path:"summaries/a.md".into(),content:"---\nsource_ids: [a]\n---\n# Travel receipt\nCosts".into(),rationale:"fixture".into()},
        DiffOp::Create {path:"summaries/z.md".into(),content:"---\nsource_ids: [z]\n---\n# Expert report\nThe expert report answers the question.".into(),rationale:"fixture".into()}]}).await?;
    db.upsert_wiki_page(
        &tenant,
        &WikiPageUpsert {
            page_id: "expert".into(),
            path: "summaries/z.md".into(),
            title: "Expert report".into(),
            kind: "concept".into(),
            content: "The expert report answers the question.".into(),
            tags: vec![],
            source_ids: vec!["z".into()],
        },
        vec![1.; 384],
    )
    .await?;
    let wiki = wiki
        .with_page_scope(WikiPageScope::Knowledge)
        .with_source_filings(HashMap::from([
            ("a".into(), "/Inbox/Receipt.txt".into()),
            ("z".into(), "/Inbox/Report.txt".into()),
        ]));
    for exact in [false, true] {
        let model = Arc::new(Model {
            calls: AtomicUsize::new(0),
            exact,
        });
        let retriever = Retriever {
            wiki: wiki.clone(),
            db: db.clone(),
            tenant: tenant.clone(),
            user_groups: vec!["admin".into()],
            embedder: Some(model.clone()),
            reranker: model.clone(),
            llm: model.clone(),
        };
        let events: Vec<_> = retriever
            .chat(ChatRequest {
                message: if exact {
                    "/Inbox/Receipt.txt"
                } else {
                    "expert report in /Inbox"
                }
                .into(),
                max_pages: 1,
                history: vec![],
            })
            .collect()
            .await;
        assert!(
            model.calls.load(Ordering::Relaxed) > 0,
            "filing hits must not suppress content search"
        );
        let [ChatEvent::Meta { retrieved, .. }, ChatEvent::Done] = events.as_slice() else {
            panic!("unexpected events: {events:?}")
        };
        assert_eq!(retrieved.len(), 1);
        assert_eq!(
            retrieved[0].path,
            if exact {
                "summaries/a.md"
            } else {
                "summaries/z.md"
            }
        );
    }
    Ok(())
}
