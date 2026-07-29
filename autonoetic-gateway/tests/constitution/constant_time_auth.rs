//! Constitution R+15 — Constant-time shared-secret comparison.


use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::server::http::create_router;
use autonoetic_gateway::server::http::HttpState;
use std::sync::Arc;
use tokio::sync::Mutex;

const TEST_SECRET: &str = "test-secret-for-constant-time";

fn auth_bearer(token: &str) -> (String, String) {
    ("Authorization".to_string(), format!("Bearer {}", token))
}

#[tokio::test]
async fn http_rejects_wrong_token_with_403() {
    let dir = tempfile::tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = ContentStore::new(&gateway_dir).unwrap();
    let state = HttpState {
        store: Arc::new(Mutex::new(store)),
        shared_secret: TEST_SECRET.to_string(),
        max_body_size: 10 * 1024 * 1024,
        router: None,
    };
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let client = reqwest::Client::new();
    let (auth_name, auth_value) = auth_bearer("wrong-token");

    let body = serde_json::json!({
        "session_id": "s",
        "name": "n",
        "content": "c"
    });

    let resp = client
        .post(&format!("http://{}/api/content/write", addr))
        .header(&auth_name, &auth_value)
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        403,
        "wrong token should be rejected with 403"
    );

    handle.abort();
}

#[tokio::test]
async fn http_accepts_correct_token() {
    let dir = tempfile::tempdir().unwrap();
    let gateway_dir = dir.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let store = ContentStore::new(&gateway_dir).unwrap();
    let state = HttpState {
        store: Arc::new(Mutex::new(store)),
        shared_secret: TEST_SECRET.to_string(),
        max_body_size: 10 * 1024 * 1024,
        router: None,
    };
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let client = reqwest::Client::new();
    let (auth_name, auth_value) = auth_bearer(TEST_SECRET);

    let body = serde_json::json!({
        "session_id": "s",
        "name": "n",
        "content": "c"
    });

    let resp = client
        .post(&format!("http://{}/api/content/write", addr))
        .header(&auth_name, &auth_value)
        .json(&body)
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "correct token should be accepted"
    );

    handle.abort();
}
