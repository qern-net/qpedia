use super::*;
use crate::auth::{AuthMode, ExternalAuthProvider, User};
use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request, StatusCode},
};
use futures::{stream, stream::BoxStream, StreamExt};
use qpedia_core::wiki::{DiffBundle, DiffOp, WikiPageScope};
use qpedia_embed::{Embedder, Reranker};
use qpedia_llm::{CompleteReq, CompleteResp, LlmError, LlmProvider, StopReason, Token};
use qpedia_pg_store::WikiPageUpsert;
use serde_json::{json, Value};
use sha2::Digest;
use tower::ServiceExt;

struct Fixture(Tenant);

#[async_trait]
impl ExternalAuthProvider for Fixture {
    async fn authenticate(&self, _: &HeaderMap) -> Option<User> {
        Some(User {
            id: "fixture-writer".into(),
            email: None,
            name: None,
            groups: vec!["admin".into()],
            tenant: self.0.clone(),
        })
    }
}

#[async_trait]
impl Embedder for Fixture {
    fn name(&self) -> &str {
        "synthetic"
    }
    fn dimensions(&self) -> usize {
        384
    }
    async fn embed(&self, input: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(vec![vec![1.; 384]; input.len()])
    }
}

#[async_trait]
impl LlmProvider for Fixture {
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
        // Echo only a filing actually present in the retrieved page supplied for synthesis.
        let line = req
            .messages
            .last()
            .unwrap()
            .content
            .lines()
            .find_map(|s| s.strip_prefix("Current source filing paths: "))
            .expect("no filing in retrieved context");
        let keys: Vec<String> = serde_json::from_str(line).unwrap();
        Ok(stream::iter([Ok(Token {
            text: keys[0].clone(),
        })])
        .boxed())
    }
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    mime: &str,
    body: String,
) -> Result<(StatusCode, String)> {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", mime)
                .body(Body::from(body))?,
        )
        .await?;
    Ok((
        res.status(),
        String::from_utf8(to_bytes(res.into_body(), 1_000_000).await?.to_vec())?,
    ))
}

#[tokio::test]
async fn embedded_upload_move_and_retrieval_share_the_current_filing() -> Result<()> {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        return Ok(());
    };
    let db = PgStore::connect(&url).await?;
    let tenant = Tenant::new(format!(
        "filing-{}",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    db.upsert_tenant(&tenant, "Synthetic counsel", None).await?;
    let temp = tempfile::tempdir()?;
    let fixture = Arc::new(Fixture(tenant.clone()));
    let ctx = IngestContext::new(
        db.clone(),
        BlobStore::open(temp.path().join("raw"))?,
        WikiRepoStore::new(temp.path().join("wiki"), "fixture", "test@example.test"),
        Arc::new(ExtractorRegistry::with_default()),
        Some(fixture.clone()),
        None,
        fixture.clone(),
    );
    let wiki = ctx.wiki_store.get(&tenant).await?;
    let mut state = AppState {
        ctx,
        auth: AuthState {
            mode: AuthMode::Session,
            firebase: None,
            external: Some(fixture.clone()),
        },
        extensions: Extensions {
            inner: Arc::new(HashMap::from([
                (TypeId::of::<WikiPageScope>(), Arc::new(WikiPageScope::Knowledge) as Arc<dyn Any + Send + Sync>),
                (TypeId::of::<crate::VersionedSourceFiling>(), Arc::new(crate::VersionedSourceFiling) as Arc<dyn Any + Send + Sync>),
            ])),
        },
        chat_rate_limiter: Arc::new(ChatRateLimiter::from_env()),
    };
    let app = core_router(DEFAULT_UPLOAD_LIMIT_BYTES).with_state(state.clone());
    let body = "--test\r\nContent-Disposition: form-data; name=\"folder_path\"\r\n\r\n/Inbox\r\n\
        --test\r\nContent-Disposition: form-data; name=\"filing_name\"\r\n\r\nOld.txt\r\n\
        --test\r\nContent-Disposition: form-data; name=\"filing_revision\"\r\n\r\n1\r\n\
        --test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"stable-id.txt\"\r\n\r\nSynthetic exhibit\r\n--test--\r\n";
    let (status, receipt) = request(
        &app,
        "POST",
        "/api/v1/sources",
        "multipart/form-data; boundary=test",
        body.into(),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let receipt: Value = serde_json::from_str(&receipt)?;
    assert_eq!(receipt["folder_path"], "/Inbox");
    assert_eq!(receipt["filing_name"], "Old.txt");
    assert_eq!(receipt["filing_revision"], 1);
    let slug = receipt["id"].as_str().unwrap();
    let uri = format!("/api/v1/sources/{slug}/move");
    let path = "summaries/exhibit.md";
    let original = format!("---\ntitle: Exhibit\nsource_ids: [{slug}]\n---\nSynthetic exhibit.");
    wiki.commit_bundle(&DiffBundle {
        ingest_id: "fixture".into(),
        summary: "fixture".into(),
        operations: vec![DiffOp::Create {
            path: path.into(),
            content: original,
            rationale: "fixture".into(),
        }],
    })
    .await?;
    let stored = wiki.read_page(path).await?.unwrap();
    let search = |q: &str| {
        format!(
            "/api/v1/wiki/search?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("q", q)
                .append_pair("limit", "1")
                .finish()
        )
    };
    let (_, before) = request(&app, "GET", &search("/Inbox/Old.txt"), "", String::new()).await?;
    assert!(before.contains("/Inbox/Old.txt"));
    let filing = json!({"folder_path":"/Appeals %_/Été","filing_name":"Receipt ①.txt","filing_revision":3,"sha256":receipt["sha256"]});
    for (payload, expected) in [
        (filing.clone(), StatusCode::OK),
        (filing.clone(), StatusCode::OK),
        (
            json!({"folder_path":"/stale","filing_revision":2,"filing_name":"Old.txt","sha256":receipt["sha256"]}),
            StatusCode::CONFLICT,
        ),
        (
            json!({"folder_path":"/wrong","filing_revision":3,"filing_name":"Other.txt","sha256":receipt["sha256"]}),
            StatusCode::CONFLICT,
        ),
        (
            json!({"folder_path":"/wrong","filing_revision":4,"filing_name":"Other.txt","sha256":"different"}),
            StatusCode::CONFLICT,
        ),
    ] {
        assert_eq!(
            request(&app, "POST", &uri, "application/json", payload.to_string())
                .await?
                .0,
            expected
        );
    }
    let new_key = "/Appeals %_/Été/Receipt ①.txt";
    let upload_type = "multipart/form-data; boundary=test";
    let (status, replay) =
        request(&app, "POST", "/api/v1/sources", upload_type, body.into()).await?;
    assert_eq!(status, StatusCode::OK);
    let replay: Value = serde_json::from_str(&replay)?;
    assert_eq!(replay["id"], slug);
    assert_eq!(replay["folder_path"], "/Appeals %_/Été");
    let (status, _) = request(
        &app,
        "POST",
        "/api/v1/sources",
        upload_type,
        body.replace("Synthetic exhibit", "Different bytes"),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(db.list_sources(&tenant, "/", 3).await?.len(), 1);
    assert_eq!(
        db.queue_overview(&tenant).await?["active"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (_, old) = request(&app, "GET", &search("/Inbox/Old.txt"), "", String::new()).await?;
    assert_eq!(serde_json::from_str::<Value>(&old)?["hits"], json!([]));
    let (_, page) = request(
        &app,
        "GET",
        "/api/v1/wiki/pages/summaries/exhibit.md",
        "",
        String::new(),
    )
    .await?;
    assert!(page.contains(new_key) && !page.contains("/Inbox/Old.txt"));
    for hybrid in [false, true] {
        if hybrid {
            // Only an unrelated page is vector-indexed. The filing hit must still rank first.
            state.ctx.embedder = Some(fixture.clone());
            db.upsert_wiki_page(
                &tenant,
                &WikiPageUpsert {
                    page_id: "other".into(),
                    path: "topics/other.md".into(),
                    kind: "concept".into(),
                    title: "Other".into(),
                    content: "Different page".into(),
                    tags: vec![],
                    source_ids: vec![],
                },
                vec![1.; 384],
            )
            .await?;
        }
        let reader = core_router(DEFAULT_UPLOAD_LIMIT_BYTES).with_state(state.clone());
        let (status, result) = request(&reader, "GET", &search(new_key), "", String::new()).await?;
        assert_eq!(status, StatusCode::OK, "{result}");
        let result: Value = serde_json::from_str(&result)?;
        assert_eq!(result["mode"], if hybrid { "hybrid" } else { "filesystem" });
        assert_eq!(result["hits"][0]["path"], path);
        assert_eq!(result["hits"][0]["snippet"], new_key);
        let (status, chat) = request(
            &reader,
            "POST",
            "/api/v1/chat",
            "application/json",
            json!({"message":"What was filed in /Appeals %_?","max_pages":1,"history":[]})
                .to_string(),
        )
        .await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            chat.contains("event: token") && chat.contains(new_key) && chat.contains(path),
            "{chat}"
        );
        assert!(!chat.contains("Old.txt"));
    }
    assert_eq!(wiki.read_page(path).await?, Some(stored)); // includes the original page ID and timestamps
    assert!(db
        .source_filings(&tenant, &["outsider".into()], false)
        .await?
        .is_empty());
    let other = Tenant::new(format!("other-{}", tenant.as_str()));
    assert!(db.source_filings(&other, &[], true).await?.is_empty());
    let id = qpedia_core::SourceId::from(slug.to_string());
    assert!(
        !db.apply_source_filing(
            &other,
            &id,
            receipt["sha256"].as_str().unwrap(),
            "/",
            "Other.txt",
            4
        )
        .await?
    );
    let root = json!({"folder_path":"/","filing_name":"Root.txt","filing_revision":4,"sha256":receipt["sha256"]});
    assert_eq!(
        request(&app, "POST", &uri, "application/json", root.to_string())
            .await?
            .0,
        StatusCode::OK
    );
    db.update_folder_path(&tenant, &id, "/automatic-category")
        .await?;
    let source = db.get_source_in(&tenant, &id).await?.unwrap();
    assert_eq!(source.folder_path, "/");
    assert_eq!(source.filename, "stable-id.txt");
    assert_eq!(source.sha256, receipt["sha256"]);
    Ok(())
}

#[async_trait]
impl Reranker for Fixture {
    fn name(&self) -> &str {
        "synthetic"
    }
    async fn scores(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        Ok(docs
            .iter()
            .map(|doc| {
                query
                    .split_whitespace()
                    .filter(|word| doc.to_lowercase().contains(&word.to_lowercase()))
                    .count() as f32
            })
            .collect())
    }
}

fn filing_state(db: PgStore, tenant: &Tenant, temp: &std::path::Path) -> Result<AppState> {
    let fixture = Arc::new(Fixture(tenant.clone()));
    Ok(AppState {
        ctx: IngestContext::new(
            db,
            BlobStore::open(temp.join("raw"))?,
            WikiRepoStore::new(temp.join("wiki"), "fixture", "fixture@example.test"),
            Arc::new(ExtractorRegistry::with_default()),
            Some(fixture.clone()),
            None,
            fixture.clone(),
        ),
        auth: AuthState {
            mode: AuthMode::Session,
            firebase: None,
            external: Some(fixture),
        },
        extensions: Extensions {
            inner: Arc::new(HashMap::from([
                (TypeId::of::<WikiPageScope>(), Arc::new(WikiPageScope::Knowledge) as Arc<dyn Any + Send + Sync>),
                (TypeId::of::<crate::VersionedSourceFiling>(), Arc::new(crate::VersionedSourceFiling) as Arc<dyn Any + Send + Sync>),
            ])),
        },
        chat_rate_limiter: Arc::new(ChatRateLimiter::from_env()),
    })
}

fn upload_body(identifier: &str, name: &str, content: &str) -> String {
    // Metadata deliberately follows the file part.
    format!("--test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{identifier}\"\r\n\r\n{content}\r\n\
        --test\r\nContent-Disposition: form-data; name=\"folder_path\"\r\n\r\n/Inbox\r\n\
        --test\r\nContent-Disposition: form-data; name=\"filing_name\"\r\n\r\n{name}\r\n\
        --test\r\nContent-Disposition: form-data; name=\"filing_revision\"\r\n\r\n1\r\n--test--\r\n")
}

#[tokio::test]
async fn host_upload_mime_and_complete_identifiers_survive_collisions_and_replays() -> Result<()> {
    use qpedia_store::BlobKind;
    use qpedia_store::BlobStorage;
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        return Ok(());
    };
    let db = PgStore::connect(&url).await?;
    let tenant = Tenant::new(format!("upload-review-{}", ulid::Ulid::new()).to_lowercase());
    db.upsert_tenant(&tenant, "Synthetic fixture", None).await?;
    let temp = tempfile::tempdir()?;
    let state = filing_state(db.clone(), &tenant, temp.path())?;
    let app = core_router(DEFAULT_UPLOAD_LIMIT_BYTES).with_state(state.clone());
    let prefix = "x".repeat(90);
    let identifiers = [
        "doc_1.txt".to_string(),
        "doc-1.txt".into(),
        format!("{prefix}one"),
        format!("{prefix}two"),
        "20c27e70-d81b-409b-a849-becddedb5c12".into(),
    ];
    let mut ids = std::collections::HashSet::new();
    for identifier in &identifiers {
        let body = upload_body(identifier, "Report.pdf", "%PDF-synthetic");
        let (status, text) = request(
            &app,
            "POST",
            "/api/v1/sources",
            "multipart/form-data; boundary=test",
            body.clone(),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{text}");
        let value: Value = serde_json::from_str(&text)?;
        assert_eq!(value["mime"], "application/pdf");
        assert_eq!(value["filename"], identifier.as_str());
        let id = value["id"].as_str().unwrap();
        assert!(
            ids.insert(id.to_string()),
            "distinct identifiers must not collide"
        );
        let (status, replay) = request(
            &app,
            "POST",
            "/api/v1/sources",
            "multipart/form-data; boundary=test",
            body,
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(serde_json::from_str::<Value>(&replay)?["id"], id);
        let ext = std::path::Path::new(identifier)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("bin");
        assert_eq!(
            state
                .ctx
                .blob
                .get(&tenant, &id.to_string().into(), BlobKind::Original, ext)
                .await?
                .as_ref(),
            b"%PDF-synthetic"
        );
    }
    assert_eq!(
        db.list_sources(&tenant, "/", 20).await?.len(),
        identifiers.len()
    );
    // A later ordinary upload with a legacy-looking slug must not change a
    // previously established canonical host binding on the next retry.
    let canonical = format!(
        "host-{}",
        hex::encode(sha2::Sha256::digest(identifiers[0].as_bytes()))
    );
    let mut late_legacy = db
        .get_source_in(&tenant, &canonical.clone().into())
        .await?
        .unwrap();
    late_legacy.id = qpedia_pg_store::slugify(&late_legacy.filename).into();
    db.insert_source(&late_legacy).await?;
    let (status, text) = request(
        &app,
        "POST",
        "/api/v1/sources",
        "multipart/form-data; boundary=test",
        upload_body(&identifiers[0], "Report.pdf", "%PDF-synthetic"),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(serde_json::from_str::<Value>(&text)?["id"], canonical);
    // Existing legacy bindings are retained, with digest mismatch still rejected.
    let existing = db
        .get_source_in(&tenant, &ids.iter().next().unwrap().clone().into())
        .await?
        .unwrap();
    let mut legacy = existing.clone();
    legacy.filename = "legacy_document.txt".into();
    legacy.id = qpedia_pg_store::slugify(&legacy.filename).into();
    db.insert_source(&legacy).await?;
    let body = upload_body(&legacy.filename, "Report.pdf", "%PDF-synthetic");
    let (status, text) = request(
        &app,
        "POST",
        "/api/v1/sources",
        "multipart/form-data; boundary=test",
        body.clone(),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text)?["id"],
        legacy.id.as_str()
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/sources",
            "multipart/form-data; boundary=test",
            body.replace("%PDF-synthetic", "changed bytes")
        )
        .await?
        .0,
        StatusCode::CONFLICT
    );
    Ok(())
}

struct ScopedReader(Tenant, bool);
#[async_trait]
impl ExternalAuthProvider for ScopedReader {
    async fn authenticate(&self, _: &HeaderMap) -> Option<User> {
        Some(User {
            id: format!("service:embedded:read:{}", self.0.as_str()),
            email: None,
            name: None,
            groups: vec![if self.1 { "admin" } else { "outsider" }.into()],
            tenant: self.0.clone(),
        })
    }
}

struct OutageModel;
#[async_trait]
impl LlmProvider for OutageModel {
    fn name(&self) -> &str {
        "synthetic-outage"
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
        let prompt = &req.messages.last().unwrap().content;
        assert!(prompt.contains("Outage evidence"));
        assert!(!prompt.contains("internal fixture"));
        Ok(stream::empty().boxed())
    }
}

#[tokio::test]
async fn embedded_reads_survive_filing_lookup_outage_without_relaxing_page_acl() -> Result<()> {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        return Ok(());
    };
    let db = PgStore::connect(&url).await?;
    let tenant = Tenant::new(format!("outage-review-{}", ulid::Ulid::new()).to_lowercase());
    db.upsert_tenant(&tenant, "Synthetic fixture", None).await?;
    let dir = tempfile::tempdir()?;
    let mut state = filing_state(db.clone(), &tenant, dir.path())?;
    state.ctx.llm = Some(Arc::new(OutageModel));
    state.auth.external = Some(Arc::new(ScopedReader(tenant.clone(), true)));
    let wiki = state.ctx.wiki_store.get(&tenant).await?;
    wiki.commit_bundle(&DiffBundle {
        ingest_id: "fixture".into(),
        summary: "fixture".into(),
        operations: vec![DiffOp::Create {
            path: "summaries/evidence.md".into(),
            content: "---\nsource_ids: [locked]\n---\n# Outage evidence\nA genuine document."
                .into(),
            rationale: "fixture".into(),
        }],
    })
    .await?;
    // Closing only this test's pool gives deterministic connection failures;
    // it does not stop the database service or affect any other test pool.
    db.pool().close().await;
    let app = core_router(DEFAULT_UPLOAD_LIMIT_BYTES).with_state(state.clone());
    for uri in [
        "/api/v1/wiki/list",
        "/api/v1/wiki/pages/summaries/evidence.md",
        "/api/v1/wiki/search?q=outage&limit=1",
    ] {
        let (status, text) = request(&app, "GET", uri, "", String::new()).await?;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains("evidence.md") || text.contains("Outage evidence"));
        assert!(!text.contains("QPEDIA.md"));
    }
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/wiki/pages/index.md",
            "",
            String::new()
        )
        .await?
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, text) = request(
        &app,
        "POST",
        "/api/v1/chat",
        "application/json",
        json!({"message":"outage","max_pages":1}).to_string(),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        text.contains("summaries/evidence.md") && text.contains("event: done"),
        "{text}"
    );
    state.auth.external = Some(Arc::new(ScopedReader(tenant, false)));
    let restricted = core_router(DEFAULT_UPLOAD_LIMIT_BYTES).with_state(state);
    assert_eq!(
        request(
            &restricted,
            "GET",
            "/api/v1/wiki/pages/summaries/evidence.md",
            "",
            String::new()
        )
        .await?
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, text) = request(
        &restricted,
        "GET",
        "/api/v1/wiki/search?q=outage&limit=1",
        "",
        String::new(),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&text)?["hits"], json!([]));
    Ok(())
}

#[tokio::test]
async fn api_folder_search_merges_content_before_truncation_in_both_backends() -> Result<()> {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        return Ok(());
    };
    let db = PgStore::connect(&url).await?;
    let tenant = Tenant::new(format!("ranking-review-{}", ulid::Ulid::new()).to_lowercase());
    db.upsert_tenant(&tenant, "Synthetic fixture", None).await?;
    let dir = tempfile::tempdir()?;
    let mut state = filing_state(db.clone(), &tenant, dir.path())?;
    let app = core_router(DEFAULT_UPLOAD_LIMIT_BYTES).with_state(state.clone());
    let wiki = state.ctx.wiki_store.get(&tenant).await?;
    let mut operations = vec![];
    for (identifier, path, title, body) in [
        (
            "a",
            "summaries/a.md",
            "Travel receipt",
            "Unrelated travel costs.",
        ),
        (
            "z",
            "summaries/z.md",
            "Expert report",
            "The expert report answers the question.",
        ),
    ] {
        let (status, text) = request(
            &app,
            "POST",
            "/api/v1/sources",
            "multipart/form-data; boundary=test",
            upload_body(identifier, &format!("{identifier}.txt"), body),
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{text}");
        let source: Value = serde_json::from_str(&text)?;
        operations.push(DiffOp::Create {
            path: path.into(),
            content: format!(
                "---\nsource_ids: [{}]\n---\n# {title}\n{body}",
                source["id"].as_str().unwrap()
            ),
            rationale: "fixture".into(),
        });
        if identifier == "z" {
            db.upsert_wiki_page(
                &tenant,
                &WikiPageUpsert {
                    page_id: "expert".into(),
                    path: path.into(),
                    kind: "concept".into(),
                    title: title.into(),
                    content: body.into(),
                    tags: vec![],
                    source_ids: vec![source["id"].as_str().unwrap().into()],
                },
                vec![1.; 384],
            )
            .await?;
        }
    }
    wiki.commit_bundle(&DiffBundle {
        ingest_id: "fixture".into(),
        summary: "fixture".into(),
        operations,
    })
    .await?;
    for hybrid in [false, true] {
        if hybrid {
            state.ctx.embedder = Some(Arc::new(Fixture(tenant.clone())));
        }
        let app = core_router(DEFAULT_UPLOAD_LIMIT_BYTES).with_state(state.clone());
        for (query, path) in [
            ("expert report in /Inbox", "summaries/z.md"),
            ("/Inbox/a.txt", "summaries/a.md"),
        ] {
            let uri = format!(
                "/api/v1/wiki/search?{}",
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("q", query)
                    .append_pair("limit", "1")
                    .finish()
            );
            let (status, text) = request(&app, "GET", &uri, "", String::new()).await?;
            assert_eq!(status, StatusCode::OK, "{text}");
            let value: Value = serde_json::from_str(&text)?;
            assert_eq!(value["hits"][0]["path"], path, "{text}");
            assert_eq!(value["mode"], if hybrid { "hybrid" } else { "filesystem" });
        }
    }
    Ok(())
}
