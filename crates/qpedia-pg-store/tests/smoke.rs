//! End-to-end smoke test against a real Postgres + pgvector.
//!
//! Gated on `QPEDIA_DB_URL`. If unset (e.g. a developer running
//! `cargo test` with no DB handy) the test prints a `skip:` note and
//! returns. CI sets the var to a pgvector service container and the
//! body runs.
//!
//! What this catches:
//!
//!   * any migration that fails to apply against a clean v17 + pgvector;
//!   * column-name / type drift in every PgStore method called below;
//!   * RLS policies that are too strict (writes that should succeed
//!     would fail with `permission denied`);
//!   * the `vector(384)` ↔ `Vec<f32>` round trip for `wiki_pages`;
//!   * tsvector + GIN + HNSW indexes co-existing through `hybrid_search`.
//!
//! Each run uses a tenant slug derived from `SystemTime::now()` so
//! parallel CI shards don't collide. CI's service container is
//! ephemeral so no cleanup is attempted — sufficient for a fresh-DB
//! invariant.

use qpedia_core::{
    acl::Acl,
    source::{Source, SourceStatus},
    tenant::Tenant,
    SourceId,
};
use qpedia_pg_store::{unique_source_slug, PgStore, WikiPageUpsert};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn unique_tenant() -> Tenant {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Tenant::new(format!("ci-{nanos:032}"))
}

#[tokio::test]
async fn smoke_full_lifecycle() {
    let Ok(url) = std::env::var("QPEDIA_DB_URL") else {
        eprintln!("skip: QPEDIA_DB_URL not set (CI: pgvector service container expected)");
        return;
    };

    let db = PgStore::connect(&url)
        .await
        .expect("connect + run all migrations");
    let tenant = unique_tenant();

    // ── Tenants ───────────────────────────────────────────────────────
    db.upsert_tenant(
        &tenant,
        "CI Tenant",
        Some(&format!("{}.example.com", tenant.as_str())),
    )
    .await
    .expect("upsert tenant");
    let row = db
        .get_tenant(&tenant)
        .await
        .expect("get tenant")
        .expect("tenant row should exist");
    assert_eq!(row.display_name, "CI Tenant");

    // ── Folders ──────────────────────────────────────────────────────
    db.create_folder(&tenant, "/Finance Reports", true, "ci")
        .await
        .expect("create folder");
    let folders = db.list_folders(&tenant).await.expect("list folders");
    assert!(
        folders
            .iter()
            .any(|f| f.path == "/finance-reports" && f.pinned),
        "expected slugified pinned folder; got {folders:?}"
    );
    assert!(db.is_folder_pinned(&tenant, "/finance-reports").await.unwrap());
    db.set_folder_pinned(&tenant, "/finance-reports", false, "ci")
        .await
        .expect("unpin");
    assert!(!db.is_folder_pinned(&tenant, "/finance-reports").await.unwrap());
    db.delete_folder(&tenant, "/finance-reports")
        .await
        .expect("delete folder");

    // ── Folder ACLs (closest-ancestor resolution) ────────────────────
    db.set_folder_acl(
        &tenant,
        "/finance",
        &Acl::from_iter(["finance-team".to_string()]),
        "ci",
    )
    .await
    .expect("set acl");
    let resolved = db
        .resolve_folder_acl(&tenant, "/finance/q4")
        .await
        .expect("resolve acl")
        .expect("ancestor acl should apply");
    assert!(resolved.0.contains("finance-team"));
    db.delete_folder_acl(&tenant, "/finance")
        .await
        .expect("delete acl");

    // ── Sources (slug minting + CRUD) ────────────────────────────────
    let slug = unique_source_slug(&db, &tenant, "Q4 Report.pdf")
        .await
        .expect("mint slug");
    assert_eq!(slug, "q4-report-pdf");
    let sid = SourceId::from(slug);
    let src = Source {
        id: sid.clone(),
        tenant: tenant.clone(),
        folder_path: "/finance".into(),
        filename: "Q4 Report.pdf".into(),
        mime: "application/pdf".into(),
        sha256: "deadbeefcafebabe".repeat(4),
        size_bytes: 12_345,
        acl: Acl::from_iter(["finance-team".to_string()]),
        status: SourceStatus::Pending,
        language: None,
        created_at: chrono::Utc::now(),
        ingested_at: None,
        classification: None,
    };
    db.insert_source(&src).await.expect("insert source");

    let got = db
        .get_source_in(&tenant, &sid)
        .await
        .expect("get source")
        .expect("source row should exist");
    assert_eq!(got.filename, "Q4 Report.pdf");
    assert_eq!(got.status, SourceStatus::Pending);

    db.update_status(&tenant, &sid, SourceStatus::Extracted)
        .await
        .expect("update status");
    db.update_language(&tenant, &sid, "en")
        .await
        .expect("update language");
    db.update_classification(
        &tenant,
        &sid,
        &json!({"doc_type": "report", "language": "en", "sensitivity": "low", "hints": ["q4"]}),
    )
    .await
    .expect("update classification");

    let refreshed = db
        .get_source_in(&tenant, &sid)
        .await
        .expect("get refreshed")
        .expect("source row should still exist");
    assert_eq!(refreshed.status, SourceStatus::Extracted);
    assert_eq!(refreshed.language.as_deref(), Some("en"));
    assert_eq!(
        refreshed
            .classification
            .as_ref()
            .and_then(|c| c.get("doc_type"))
            .and_then(|v| v.as_str()),
        Some("report")
    );

    // Replace-in-place (Band 2.1): same slug, fresh bytes.
    db.replace_source_blob(
        &tenant,
        &sid,
        "Q4 Report v2.pdf",
        "application/pdf",
        &"abcd".repeat(16),
        67_890,
    )
    .await
    .expect("replace blob");
    let replaced = db
        .get_source_in(&tenant, &sid)
        .await
        .expect("get replaced")
        .expect("source row should still exist");
    assert_eq!(replaced.filename, "Q4 Report v2.pdf");
    assert_eq!(replaced.size_bytes, 67_890);
    assert_eq!(replaced.status, SourceStatus::Pending);
    assert!(replaced.classification.is_none());

    db.delete_source(&tenant, &sid)
        .await
        .expect("delete source");
    assert!(
        db.get_source_in(&tenant, &sid).await.unwrap().is_none(),
        "source row should be gone after delete"
    );

    // ── Audit ────────────────────────────────────────────────────────
    db.write_audit(
        &tenant,
        "ci",
        "smoke.test",
        Some("target"),
        Some(&json!({"k": "v"})),
    )
    .await
    .expect("write audit");

    // ── OAuth grants (SSO-aligned connector credentials) ─────────────
    let grant_id = db
        .upsert_oauth_grant(
            &tenant,
            "google",
            "drive.readonly",
            "", // org-level
            Some("access-xyz"),
            "refresh-abc",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "ci",
        )
        .await
        .expect("upsert oauth grant");
    let grant = db
        .get_oauth_grant(&tenant, grant_id)
        .await
        .expect("get grant")
        .expect("grant should exist");
    assert_eq!(grant.provider, "google");
    assert_eq!(grant.refresh_token, "refresh-abc");
    // Upsert again (re-auth) — same logical key, refreshed tokens, same row.
    let grant_id2 = db
        .upsert_oauth_grant(
            &tenant,
            "google",
            "drive.readonly",
            "",
            Some("access-new"),
            "refresh-new",
            Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            "ci",
        )
        .await
        .expect("re-upsert oauth grant");
    assert_eq!(grant_id, grant_id2, "re-auth should update the same row");
    let found = db
        .find_oauth_grant(&tenant, "google", "drive.readonly", "")
        .await
        .expect("find grant")
        .expect("grant should be found by key");
    assert_eq!(found.refresh_token, "refresh-new");
    db.update_oauth_access_token(
        &tenant,
        grant_id,
        "access-rotated",
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .expect("rotate access token");
    db.delete_oauth_grant(&tenant, grant_id)
        .await
        .expect("delete grant");
    assert!(
        db.get_oauth_grant(&tenant, grant_id).await.unwrap().is_none(),
        "grant should be gone after delete"
    );

    // ── Workspaces: membership, org creation, invites (Band 4.1) ─────
    let user_a = format!("firebase:{}", &tenant.as_str()[3..]); // arbitrary stable id
    // Owner membership in this (acting as individual) workspace.
    db.ensure_membership(&tenant, &user_a, Some("a@x.com"), "owner")
        .await
        .expect("ensure membership");
    assert_eq!(
        db.membership_role(&tenant, &user_a).await.unwrap().as_deref(),
        Some("owner")
    );
    let my_ws = db.list_user_workspaces(&user_a).await.expect("list workspaces");
    assert!(my_ws.iter().any(|w| w.tenant.as_str() == tenant.as_str()));

    // Create an org workspace; owner is user_a.
    let org = Tenant::new(format!("org-{}", &tenant.as_str()[3..]));
    db.create_org_workspace(&org, "Acme", &user_a, Some("a@x.com"))
        .await
        .expect("create org");
    assert_eq!(
        db.membership_role(&org, &user_a).await.unwrap().as_deref(),
        Some("owner")
    );
    assert_eq!(
        db.list_user_workspaces(&user_a).await.unwrap().len(),
        2,
        "user_a now belongs to individual + org"
    );

    // Invite user_b into the org; accept it.
    let token = format!("tok-{}", &tenant.as_str()[3..]);
    db.create_invite(&org, "b@x.com", "member", &token, &user_a, 3600)
        .await
        .expect("create invite");
    assert_eq!(db.list_invites(&org).await.unwrap().len(), 1);
    let preview = db.get_invite_by_token(&token).await.unwrap().expect("invite exists");
    assert_eq!(preview.email, "b@x.com");

    let user_b = format!("firebase:b-{}", &tenant.as_str()[3..]);
    let joined = db
        .accept_invite(&token, &user_b, Some("b@x.com"))
        .await
        .expect("accept invite");
    assert_eq!(joined.as_str(), org.as_str());
    assert_eq!(
        db.membership_role(&org, &user_b).await.unwrap().as_deref(),
        Some("member")
    );
    // Re-accepting fails (already accepted).
    assert!(db.accept_invite(&token, &user_b, Some("b@x.com")).await.is_err());
    // Pending list is now empty.
    assert_eq!(db.list_invites(&org).await.unwrap().len(), 0);

    // org now has 2 members; can't strand it ownerless.
    assert_eq!(db.list_members(&org).await.unwrap().len(), 2);
    assert!(
        db.remove_member(&org, &user_a).await.is_err(),
        "removing the last owner must fail"
    );
    db.remove_member(&org, &user_b).await.expect("remove member b");
    assert_eq!(db.list_members(&org).await.unwrap().len(), 1);

    // ── Wiki: pgvector + tsvector round trip ─────────────────────────
    let page = WikiPageUpsert {
        page_id: "ci-page".into(),
        path: "concepts/ci-smoke.md".into(),
        kind: "concept".into(),
        title: "CI Smoke".into(),
        content:
            "This is a deterministic smoke-test page covering ingest, embed, and hybrid search."
                .into(),
        tags: vec!["test".into(), "smoke".into()],
        source_ids: vec![],
    };
    // 384-dim, all 0.1 — matches the bge-small-en-v1.5 shape.
    let vec_384: Vec<f32> = vec![0.1; 384];
    db.upsert_wiki_page(&tenant, &page, vec_384.clone())
        .await
        .expect("upsert wiki_pages");

    let hits = db
        .hybrid_search(&tenant, "smoke", vec_384, 0.7, 5)
        .await
        .expect("hybrid_search");
    assert!(
        hits.iter().any(|h| h.path == "concepts/ci-smoke.md"),
        "hybrid_search should surface the page we just upserted; got {hits:?}"
    );

    db.delete_wiki_page(&tenant, "concepts/ci-smoke.md")
        .await
        .expect("delete wiki page");
}
