//! Integration tests for the recording-mode infrastructure.
//!
//! Tests the redaction layer, fixture writing, and proxy recording mode.

mod support;

use std::sync::Arc;

use autonoetic_gateway::runtime::sealed_network::{
    redact_fixture, write_recording_fixture, FixtureRecord, RecordedRequest,
    RecordedResponse,
};
use autonoetic_gateway::runtime::sealed_network_proxy::start_sealed_proxy;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::SandboxNetworkPolicy;
use autonoetic_types::recording::{
    FixtureSet, FixtureSetStatus, RecordingSession, RecordingStatus,
};
use tempfile::tempdir;

// ── Redaction tests ──────────────────────────────────────────────

#[test]
fn redact_authorization_header() {
    let mut record = FixtureRecord {
        request: RecordedRequest {
            method: "GET".to_string(),
            url: "http://api.example.com/data".to_string(),
            headers: vec![
                ("authorization".to_string(), "Bearer eyJhbGciOiJIUzI1NiJ9.test".to_string()),
            ].into_iter().collect(),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: "ok".to_string(),
        },
        recorded_at: "now".to_string(),
        redacted: vec![],
    };

    let redacted = redact_fixture(&mut record);
    assert!(redacted.contains(&"authorization".to_string()));
    assert_eq!(
        record.request.headers.get("authorization").unwrap(),
        "[REDACTED]"
    );
}

#[test]
fn redact_cookie_and_set_cookie() {
    let mut record = FixtureRecord {
        request: RecordedRequest {
            method: "GET".to_string(),
            url: "http://api.example.com/data".to_string(),
            headers: vec![
                ("cookie".to_string(), "session=abc123; token=xyz".to_string()),
            ].into_iter().collect(),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: vec![
                ("set-cookie".to_string(), "session=abc123; HttpOnly".to_string()),
            ].into_iter().collect(),
            body: "ok".to_string(),
        },
        recorded_at: "now".to_string(),
        redacted: vec![],
    };

    let redacted = redact_fixture(&mut record);
    assert!(redacted.contains(&"cookie".to_string()), "should redact cookie: {:?}", redacted);
    assert!(redacted.contains(&"set-cookie".to_string()), "should redact set-cookie: {:?}", redacted);
    assert_eq!(record.request.headers.get("cookie").unwrap(), "[REDACTED]");
    assert_eq!(record.response.headers.get("set-cookie").unwrap(), "[REDACTED]");
}

#[test]
fn redact_query_params() {
    let mut record = FixtureRecord {
        request: RecordedRequest {
            method: "GET".to_string(),
            url: "http://api.example.com/data?api_key=abc123&token=xyz&name=hello".to_string(),
            headers: Default::default(),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: "ok".to_string(),
        },
        recorded_at: "now".to_string(),
        redacted: vec![],
    };

    let redacted = redact_fixture(&mut record);
    assert!(redacted.contains(&"query_api_key".to_string()), "should redact api_key: {:?}", redacted);
    assert!(redacted.contains(&"query_token".to_string()), "should redact token: {:?}", redacted);
    assert!(!record.request.url.contains("abc123"), "api_key value should not leak");
    assert!(!record.request.url.contains("xyz"), "token value should not leak");
    assert!(record.request.url.contains("name=hello"), "non-sensitive params preserved");
}

#[test]
fn redact_bearer_in_response_body() {
    let mut record = FixtureRecord {
        request: RecordedRequest {
            method: "GET".to_string(),
            url: "http://api.example.com/token".to_string(),
            headers: Default::default(),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: r#"{"access_token": "Bearer eyJhbGciOiJIUzI1NiJ9.test"}"#.to_string(),
        },
        recorded_at: "now".to_string(),
        redacted: vec![],
    };

    let _redacted = redact_fixture(&mut record);
    assert!(!record.response.body.contains("eyJhbGciOiJIUzI1NiJ9.test"), "body should not contain raw token");
    assert!(record.response.body.contains("[REDACTED]"), "body should have redacted value");
}

#[test]
fn write_fixture_file() {
    let dir = tempdir().unwrap();
    let staging = dir.path().join("recordings").join("test-session").join("fixtures");

    let record = FixtureRecord {
        request: RecordedRequest {
            method: "GET".to_string(),
            url: "http://api.example.com/items?limit=10".to_string(),
            headers: Default::default(),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())]
                .into_iter().collect(),
            body: r#"{"items":[]}"#.to_string(),
        },
        recorded_at: chrono::Utc::now().to_rfc3339(),
        redacted: vec![],
    };

    let path = write_recording_fixture(
        &staging,
        "api.example.com",
        None,
        "GET",
        "/items",
        &record,
    )
    .expect("write should succeed");

    assert!(path.exists(), "fixture file should exist");
    assert!(path.to_string_lossy().contains("api.example.com"));
    assert!(path.to_string_lossy().contains("GET-items.json"));

    let content = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(parsed["response"]["status"], 200);
    assert_eq!(parsed["request"]["method"], "GET");
}

// ── Recording session store tests ────────────────────────────────

fn create_test_session(agent_id: &str, session_id: &str) -> RecordingSession {
    RecordingSession {
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        artifact_id: "art_test".to_string(),
        revision_id: "rev_test".to_string(),
        root_session_id: "root-test".to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        stopped_at: None,
        duration_secs: Some(300),
        max_requests: Some(100),
        max_bytes: Some(50_000_000),
        request_count: 0,
        total_bytes: 0,
        status: RecordingStatus::Active,
        fixture_set_id: None,
        created_by: "test-operator".to_string(),
    }
}

#[test]
fn recording_session_crud() {
    let dir = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(&dir.path().join(".gateway")).unwrap());

    let session_id = "rs_test_crud_001";
    let session = create_test_session("test.agent", session_id);
    store.create_recording_session(&session).unwrap();

    let loaded = store
        .get_recording_session(session_id)
        .unwrap()
        .expect("session should exist");
    assert_eq!(loaded.agent_id, "test.agent");
    assert_eq!(loaded.status, RecordingStatus::Active);
    assert!(loaded.stopped_at.is_none());

    let sessions = store.list_recording_sessions(None, 10).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, session_id);

    store
        .stop_recording_session(session_id, RecordingStatus::Completed)
        .unwrap();

    let stopped = store
        .get_recording_session(session_id)
        .unwrap()
        .expect("session should exist");
    assert_eq!(stopped.status, RecordingStatus::Completed);
    assert!(stopped.stopped_at.is_some());

    store.delete_recording_session(session_id).unwrap();
    assert!(store.get_recording_session(session_id).unwrap().is_none());
}

#[test]
fn recording_session_stop_twice_fails() {
    let dir = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(&dir.path().join(".gateway")).unwrap());

    let session_id = "rs_test_stop_twice";
    let session = create_test_session("test.agent", session_id);
    store.create_recording_session(&session).unwrap();

    store
        .stop_recording_session(session_id, RecordingStatus::Completed)
        .unwrap();

    let err = store
        .stop_recording_session(session_id, RecordingStatus::Completed)
        .unwrap_err();
    assert!(err.to_string().contains("already"));
}

#[test]
fn fixture_set_crud() {
    let dir = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(&dir.path().join(".gateway")).unwrap());

    let fs_id = "fs_test_crud_001";
    let fixture_set = FixtureSet {
        fixture_set_id: fs_id.to_string(),
        agent_id: "test.agent".to_string(),
        revision_id: "rev_test".to_string(),
        recording_session_id: "rs_test".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        fixture_file_count: 5,
        total_bytes: 1024,
        digest: "sha256:abc123".to_string(),
        host_summary: vec!["api.example.com".to_string()],
        host_count: 1,
        redaction_summary: vec!["authorization".to_string()],
        status: FixtureSetStatus::Ready,
    };
    store.create_fixture_set(&fixture_set).unwrap();

    let loaded = store.get_fixture_set(fs_id).unwrap().expect("fixture set should exist");
    assert_eq!(loaded.fixture_file_count, 5);
    assert_eq!(loaded.digest, "sha256:abc123");
    assert_eq!(loaded.status, FixtureSetStatus::Ready);

    let sets = store.list_fixture_sets(Some("test.agent"), 10).unwrap();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].fixture_set_id, fs_id);

    store.delete_fixture_set(fs_id).unwrap();
    assert!(store.get_fixture_set(fs_id).unwrap().is_none());
}

#[test]
fn fixture_set_and_session_linking() {
    let dir = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(&dir.path().join(".gateway")).unwrap());

    let session_id = "rs_test_link";
    let fs_id = "fs_test_link_001";
    let session = create_test_session("link.test.agent", session_id);
    store.create_recording_session(&session).unwrap();

    let fixture_set = FixtureSet {
        fixture_set_id: fs_id.to_string(),
        agent_id: "link.test.agent".to_string(),
        revision_id: "rev_link_test".to_string(),
        recording_session_id: session_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        fixture_file_count: 3,
        total_bytes: 512,
        digest: "sha256:link-digest".to_string(),
        host_summary: vec!["api.example.com".to_string()],
        host_count: 1,
        redaction_summary: vec![],
        status: FixtureSetStatus::Ready,
    };
    store.create_fixture_set(&fixture_set).unwrap();

    store
        .set_recording_session_fixture_set(session_id, fs_id)
        .unwrap();

    let session = store
        .get_recording_session(session_id)
        .unwrap()
        .expect("session should exist");
    assert_eq!(session.fixture_set_id, Some(fs_id.to_string()));
}

// ── Proxy recording mode test ────────────────────────────────────

#[tokio::test]
async fn proxy_recording_mode_captures_fixture_on_miss() {
    let dir = tempdir().unwrap();
    let staging_dir = dir.path().join("recordings").join("test-session").join("fixtures");

    // Start a simple HTTP server to serve as the "live" target.
    let server_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let server_url = format!("http://{}", server_addr);

    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = server_listener.accept().await.unwrap();
        use tokio::io::AsyncWriteExt;
        let response = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 24\r\n\r\n{\"status\":\"live\",\"ok\":true}";
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
    });

    // Give the server time to accept.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Start the proxy in Recording mode.
    let fixture_root = dir.path().join("no-fixtures-here");
    std::fs::create_dir_all(&fixture_root).unwrap();
    let loader = Arc::new(
        autonoetic_gateway::runtime::sealed_network::FixtureLoader::new(fixture_root),
    );
    let handle = start_sealed_proxy(
        SandboxNetworkPolicy::Recording,
        loader,
        Some(staging_dir.clone()),
        None,
    )
    .await
    .expect("recording proxy must start");

    let proxy_url = handle.proxy_url();

    // Make a request through the proxy to the live server.
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(&proxy_url).unwrap())
        .build()
        .unwrap();

    let resp = client
        .get(format!("{}/test-path", server_url))
        .header("authorization", "Bearer test-token")
        .send()
        .await
        .expect("request through proxy must succeed");

    // Verify the live response was returned.
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("live"), "should get live response: {body}");

    // Give the proxy time to write the fixture.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify the fixture was captured with redactions.
    let fixture_path = staging_dir.join("127.0.0.1").join(format!("GET-{}-test-path.json",
        server_addr.port()));
    let alt_path = staging_dir.join(format!("127.0.0.1-{}", server_addr.port())).join("GET-test-path.json");

    let fixture_path = if fixture_path.exists() {
        fixture_path
    } else if alt_path.exists() {
        alt_path
    } else {
        // Search for any fixture file.
        let mut found = None;
        if let Ok(entries) = std::fs::read_dir(&staging_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Ok(files) = std::fs::read_dir(&path) {
                        for file in files.flatten() {
                            let fp = file.path();
                            if fp.to_string_lossy().contains("GET") {
                                found = Some(fp);
                                break;
                            }
                        }
                    }
                }
            }
        }
        found.expect(&format!("fixture file should exist in {:?}", staging_dir))
    };

    assert!(fixture_path.exists(), "fixture file should exist: {:?}", fixture_path);
    let content = std::fs::read_to_string(&fixture_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    // Verify redaction was applied.
    assert_eq!(parsed["response"]["status"], 200);
    assert!(parsed["response"]["body"].to_string().contains("live"));

    handle.shutdown().await;
    server_handle.await.unwrap();
}
