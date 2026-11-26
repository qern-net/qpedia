use anyhow::Result;
use qpedia_core::wiki::{DiffBundle, DiffOp, WikiPageScope};
use qpedia_store::{SearchHit, WikiRepo};
use std::collections::HashMap;

fn page(path: &str, source: &str, body: &str) -> DiffOp {
    DiffOp::Create {
        path: path.into(),
        content: format!("---\nsource_ids: [{source}]\n---\n{body}"),
        rationale: "fixture".into(),
    }
}

async fn commit(wiki: &WikiRepo, operations: Vec<DiffOp>) -> Result<()> {
    wiki.commit_bundle(&DiffBundle {
        ingest_id: "fixture".into(),
        summary: "fixture".into(),
        operations,
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn annotations_do_not_manufacture_stored_content_matches() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let wiki = WikiRepo::open_or_init(dir.path(), "fixture", "fixture@example.test").await?;
    commit(
        &wiki,
        vec![
            page("summaries/a.md", "a", "# Unrelated\nOrdinary exhibit."),
            page("summaries/z.md", "z", "# Relevant\nCurrent court order."),
        ],
    )
    .await?;
    let reader = wiki
        .with_page_scope(WikiPageScope::Knowledge)
        .with_source_filings(HashMap::from([
            ("a".into(), "/Inbox/Unrelated.txt".into()),
            ("z".into(), "/Inbox/Order.txt".into()),
        ]));
    assert!(reader
        .read_page("summaries/a.md")
        .await?
        .unwrap()
        .contains("Current source filing paths"));
    let hits = reader.search_text("current", 1).await?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "summaries/z.md");
    assert!(!hits[0].snippet.contains("Current source filing paths"));
    assert!(reader.search_text("filing paths", 10).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn folder_candidates_are_ranked_before_limits_and_exact_keys_stay_first() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let wiki = WikiRepo::open_or_init(dir.path(), "fixture", "fixture@example.test").await?;
    commit(
        &wiki,
        vec![
            page("summaries/a.md", "a", "# Travel\nUnrelated receipt."),
            page(
                "summaries/z.md",
                "z",
                "# Expert report\nThe expert report contains the answer.",
            ),
        ],
    )
    .await?;
    let reader = wiki
        .with_page_scope(WikiPageScope::Knowledge)
        .with_source_filings(HashMap::from([
            ("a".into(), "/Inbox/Receipt.txt".into()),
            ("z".into(), "/Inbox/Report.txt".into()),
        ]));
    let query = "expert report in /Inbox";
    assert_eq!(
        reader.search_filings(query, 1).await?[0].path,
        "summaries/z.md"
    );
    let hits = reader
        .merge_search_hits(
            query,
            vec![SearchHit {
                path: "summaries/z.md".into(),
                title: "Expert report".into(),
                snippet: "The answer".into(),
            }],
            1,
        )
        .await?;
    assert_eq!(hits[0].0.path, "summaries/z.md");
    assert!(hits[0].1 > 0.);
    let hits = reader
        .merge_search_hits(
            "/Inbox/Receipt.txt",
            vec![SearchHit {
                path: "summaries/z.md".into(),
                title: "Expert report".into(),
                snippet: "Unrelated semantic hit".into(),
            }],
            1,
        )
        .await?;
    assert_eq!(hits[0].0.path, "summaries/a.md");
    assert_eq!(hits[0].0.snippet, "/Inbox/Receipt.txt");
    assert!(reader.merge_search_hits(query, vec![], 0).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn provenance_cache_is_shared_commit_invalidated_and_request_scoped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let wiki = WikiRepo::open_or_init(dir.path(), "fixture", "fixture@example.test").await?;
    commit(
        &wiki,
        vec![
            page("summaries/exhibit.md", "exhibit", "# Exhibit\nEvidence."),
            page(
                "summaries/unrelated.md",
                "unrelated",
                "# Unrelated\nOther evidence.",
            ),
        ],
    )
    .await?;
    let filings = HashMap::from([
        ("exhibit".into(), "/Inbox/Exhibit.txt".into()),
        ("pending".into(), "/Inbox/Pending.txt".into()),
    ]);
    let reader = wiki
        .clone()
        .with_page_scope(WikiPageScope::Knowledge)
        .with_source_filings(filings.clone());
    assert_eq!(
        reader.search_filings("/Inbox/Exhibit.txt", 1).await?.len(),
        1
    );
    // A warm lookup cannot scan/read unrelated pages again. This creates a read
    // error there without changing HEAD, then performs a cloned request lookup.
    let unrelated = dir.path().join("summaries/unrelated.md");
    tokio::fs::remove_file(&unrelated).await?;
    tokio::fs::create_dir(&unrelated).await?;
    assert_eq!(
        reader
            .clone()
            .search_filings("/Inbox/Exhibit.txt", 1)
            .await?
            .len(),
        1
    );
    assert!(reader
        .search_filings("/Inbox/Pending.txt", 1)
        .await?
        .is_empty());
    assert!(wiki
        .clone()
        .with_source_filings(HashMap::new())
        .search_filings("/Inbox", 10)
        .await?
        .is_empty());
    let renamed = wiki
        .clone()
        .with_page_scope(WikiPageScope::Knowledge)
        .with_source_filings(HashMap::from([(
            "exhibit".into(),
            "/Appeals/Renamed.txt".into(),
        )]));
    assert!(renamed
        .search_filings("/Inbox/Exhibit.txt", 1)
        .await?
        .is_empty());
    assert_eq!(
        renamed
            .search_filings("/Appeals/Renamed.txt", 1)
            .await?
            .len(),
        1
    );
    tokio::fs::remove_dir(&unrelated).await?;
    tokio::fs::write(&unrelated, "# Unrelated").await?;
    commit(
        &wiki,
        vec![page(
            "summaries/pending.md",
            "pending",
            "# Newly ingested\nPending source now has a page.",
        )],
    )
    .await?;
    assert_eq!(
        reader.search_filings("/Inbox/Pending.txt", 1).await?[0].path,
        "summaries/pending.md"
    );
    Ok(())
}

#[tokio::test]
async fn wiki_index_can_resume_ingest_or_removal_after_other_commits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let wiki = WikiRepo::open_or_init(dir.path(), "fixture", "fixture@example.test").await?;
    let path = "summaries/first source.md";
    wiki.commit_bundle(&DiffBundle {
        ingest_id: "first".into(),
        summary: "ingest the first source".into(),
        operations: vec![page(path, "first", "# First")],
    }).await?;
    wiki.commit_bundle(&DiffBundle {
        ingest_id: "remove-first".into(),
        summary: "remove the first source".into(),
        operations: vec![DiffOp::Delete {path: path.into(), rationale: "fixture".into()}],
    }).await?;
    wiki.commit_bundle(&DiffBundle {
        ingest_id: "later".into(),
        summary: "mentions ingest(first) and ingest(remove-first)".into(),
        operations: vec![page("summaries/later.md", "later", "# Later")],
    }).await?;

    assert_eq!(wiki.changed_in_head().await?, ["summaries/later.md"]);
    assert_eq!(wiki.changed_in_latest_ingest("first").await?, Some(vec![path.into()]));
    assert_eq!(wiki.changed_in_latest_ingest("remove-first").await?, Some(vec![path.into()]));
    assert!(wiki.changed_in_latest_ingest("missing").await?.is_none());
    Ok(())
}
