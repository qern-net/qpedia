use anyhow::Result;
use qpedia_core::{wiki::WikiPageScope, Tenant};
use qpedia_store::WikiRepoStore;

#[tokio::test]
async fn embedded_reader_searches_only_knowledge_and_leaves_worker_view_intact() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let store = WikiRepoStore::new(temp.path(), "test", "test@example.com");
    let tenant = Tenant::new("case-test");
    let worker = store.get(&tenant).await?;
    let internal = ["QPEDIA.md", "index.md", "log.md", "_meta/catalog.md"];
    let knowledge = ["summaries/index.md", "topics/log.md"];
    for path in internal.iter().chain(&knowledge) {
        tokio::fs::write(worker.root().join(path), format!("# {path}\nCase keyword.\n")).await?;
    }

    let reader = store.get(&tenant).await?.with_page_scope(WikiPageScope::Knowledge);
    assert_eq!(reader.list_pages("").await?, knowledge);
    assert!(reader.list_pages("_meta").await?.is_empty());
    assert_eq!(reader.search_text("keyword", 1).await?[0].path, knowledge[0]);
    for path in internal {
        assert!(reader.read_page(path).await?.is_none());
        assert!(worker.read_page(path).await?.is_some());
    }
    for path in ["./index.md", "/index.md", "summaries/../log.md", "_meta\\catalog.md"] {
        assert!(reader.read_page(path).await?.is_none());
    }
    assert!(reader.read_page(knowledge[0]).await?.is_some());
    assert_eq!(store.get(&tenant).await?.search_text("keyword", 10).await?.len(), 6);
    Ok(())
}
