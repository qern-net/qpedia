use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use serde_json::Value;
use std::io::{Cursor, Read};
use tower::ServiceExt;

async fn call(router: &Router, request: Request<Body>) -> Vec<u8> {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    bytes.to_vec()
}
async fn get(router: &Router, path: &str) -> Vec<u8> {
    call(router, Request::get(path).body(Body::empty()).unwrap()).await
}
async fn upload(router: &Router, folder: &str, data: &str) -> Value {
    let body = format!("--fixture\r\nContent-Disposition: form-data; name=\"on_conflict\"\r\n\r\nversion\r\n--fixture\r\nContent-Disposition: form-data; name=\"folder_path\"\r\n\r\n{folder}\r\n--fixture\r\nContent-Disposition: form-data; name=\"file\"; filename=\"receipt.txt\"\r\nContent-Type: text/plain\r\n\r\n{data}\r\n--fixture--\r\n");
    serde_json::from_slice(
        &call(
            router,
            Request::post("/api/v1/sources")
                .header("content-type", "multipart/form-data; boundary=fixture")
                .body(Body::from(body))
                .unwrap(),
        )
        .await,
    )
    .unwrap()
}

#[tokio::test]
async fn unchanged_revisions_folder_boundaries_and_zip_originals() {
    let database = std::env::var("FILE_MANAGER_TEST_DATABASE_URL")
        .expect("Set the disposable fixture database URL");
    let data = tempfile::tempdir().unwrap();
    std::env::set_var("QPEDIA_DB_URL", database);
    std::env::set_var("QPEDIA_DATA_DIR", data.path());
    std::env::set_var("QPEDIA_AUTH_MODE", "dev");
    let router = qpedia_api::AppBuilder::from_env()
        .await
        .unwrap()
        .with_routes(qpedia_file_routes::routes())
        .build();
    let folder = format!("/files-{}", std::process::id());
    let nested = format!("{folder}/invoices");
    for path in [&folder, &nested] {
        call(
            &router,
            Request::post("/api/v1/folders")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"path":path,"pinned":true}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    }
    let first = upload(&router, &nested, "original").await;
    let changed = upload(&router, &nested, "revised").await;
    let duplicate = upload(&router, &nested, "revised").await;
    assert_eq!(first["id"], changed["id"]);
    assert_eq!(duplicate["unchanged"], true);
    upload(&router, &format!("{folder}-other"), "sibling").await;
    let id = first["id"].as_str().unwrap();
    let list: Value =
        serde_json::from_slice(&get(&router, &format!("/api/v1/files?folder={nested}")).await)
            .unwrap();
    assert_eq!(list["files"].as_array().unwrap().len(), 1);
    assert_eq!(list["files"][0]["version"], 2);
    let versions: Value =
        serde_json::from_slice(&get(&router, &format!("/api/v1/sources/{id}/versions")).await)
            .unwrap();
    assert_eq!(versions.as_array().unwrap().len(), 2);
    assert_eq!(
        get(&router, &format!("/api/v1/sources/{id}/original?version=1")).await,
        b"original"
    );
    let bytes = get(&router, &format!("/api/v1/files/archive?folder={folder}")).await;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    assert_eq!(zip.len(), 1);
    let mut contents = String::new();
    zip.by_name("invoices/receipt.txt")
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();
    assert_eq!(contents, "revised");
}
