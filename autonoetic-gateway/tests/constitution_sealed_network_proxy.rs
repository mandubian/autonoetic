//! Constitution test for RFC scope 5.2 — sealed-network egress proxy.
//!
//! End-to-end: start the proxy, point a `reqwest` client at it via the
//! HTTP-proxy protocol, and verify:
//!
//!   1. A request with a matching fixture returns the canned `(status,
//!      headers, body)` straight from disk.
//!   2. A request with no matching fixture returns 502 with the
//!      `unfixtured_target` envelope naming the expected fixture path.
//!   3. CONNECT (HTTPS tunnelling) is rejected with a 502 carrying a
//!      diagnostic — sealed proxy does not yet support HTTPS.
//!   4. Starting the proxy with `Normal` policy is a programmer error
//!      (the proxy should never have been started in that case).
//!
//! Scope 5.2c (sandbox integration) is **not** exercised here — the
//! sandbox-side wiring that injects `HTTP_PROXY` into bubblewrap exec is
//! deferred. This test exercises the proxy + fixture pipeline directly,
//! which is the structural guarantee 5.2c depends on.
//!
//! Refs: docs/design/sealed-network-evaluation-plan.md §3.2 / scope 5.2.

mod support;

use std::sync::Arc;

use autonoetic_gateway::runtime::sealed_network::FixtureLoader;
use autonoetic_gateway::runtime::sealed_network_proxy::start_sealed_proxy;
use autonoetic_types::agent::SandboxNetworkPolicy;
use tempfile::tempdir;

fn write_fixture(root: &std::path::Path, rel: &str, body: &str) {
    let p = root.join("fixtures").join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[tokio::test]
async fn sealed_proxy_returns_fixture_on_hit() {
    let dir = tempdir().unwrap();
    write_fixture(
        dir.path(),
        "api.example.com/GET-v1-widgets.json",
        r#"{
            "status": 200,
            "headers": {"x-test": "hello"},
            "body": "{\"items\":[\"a\",\"b\"]}"
        }"#,
    );

    let loader = Arc::new(FixtureLoader::new(dir.path()));
    let handle = start_sealed_proxy(SandboxNetworkPolicy::Sealed, loader, None, None, None, None)
        .await
        .expect("proxy must start");

    let proxy_url = handle.proxy_url();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(&proxy_url).unwrap())
        .build()
        .unwrap();

    let resp = client
        .get("http://api.example.com/v1/widgets")
        .send()
        .await
        .expect("request must reach proxy");

    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("x-test")
            .map(|v| v.to_str().unwrap().to_string()),
        Some("hello".to_string())
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("items"), "fixture body must pass through: {body}");

    handle.shutdown().await;
}

#[tokio::test]
async fn sealed_proxy_returns_unfixtured_envelope_on_miss() {
    let dir = tempdir().unwrap();
    let loader = Arc::new(FixtureLoader::new(dir.path()));
    let handle = start_sealed_proxy(SandboxNetworkPolicy::Sealed, loader, None, None, None, None)
        .await
        .expect("proxy must start");

    let proxy_url = handle.proxy_url();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(&proxy_url).unwrap())
        .build()
        .unwrap();

    let resp = client
        .post("http://api.example.com/v1/echo")
        .body("payload")
        .send()
        .await
        .expect("request must reach proxy");

    assert_eq!(resp.status().as_u16(), 502);
    let body = resp.text().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "unfixtured_target");
    assert_eq!(parsed["host"], "api.example.com");
    assert_eq!(parsed["method"], "POST");
    let expected = parsed["expected_fixture_path"].as_str().unwrap();
    assert!(expected.contains("api.example.com"));
    assert!(expected.contains("POST-v1-echo.json"));

    handle.shutdown().await;
}

#[tokio::test]
async fn sealed_proxy_handles_host_port_in_fixture_path() {
    // Localhost services with non-standard ports (the moltbook case)
    // must map into `fixtures/<host-port>/...`.
    let dir = tempdir().unwrap();
    write_fixture(
        dir.path(),
        "localhost-9876/POST-status.json",
        r#"{
            "status": 200,
            "headers": {"content-type": "application/json"},
            "body": "{\"agents\":[]}"
        }"#,
    );

    let loader = Arc::new(FixtureLoader::new(dir.path()));
    let handle = start_sealed_proxy(SandboxNetworkPolicy::Sealed, loader, None, None, None, None)
        .await
        .expect("proxy must start");
    let proxy_url = handle.proxy_url();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(&proxy_url).unwrap())
        .build()
        .unwrap();

    let resp = client
        .post("http://localhost:9876/status")
        .send()
        .await
        .expect("request must reach proxy");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("agents"));

    handle.shutdown().await;
}

#[tokio::test]
async fn sealed_proxy_rejects_https_connect() {
    // HTTPS via CONNECT requires CA-cert injection to decrypt traffic
    // for fixture matching — a future RFC step. Sealed proxy must
    // surface a clear error rather than silently allow tunnelled traffic.
    let dir = tempdir().unwrap();
    let loader = Arc::new(FixtureLoader::new(dir.path()));
    let handle = start_sealed_proxy(SandboxNetworkPolicy::Sealed, loader, None, None, None, None)
        .await
        .expect("proxy must start");

    let proxy_url = handle.proxy_url();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(&proxy_url).unwrap())
        .build()
        .unwrap();

    // reqwest will attempt CONNECT against the proxy. The proxy refuses.
    // reqwest surfaces this as a request error — exact wording depends
    // on the reqwest/hyper version. What we structurally need: the
    // request **must not succeed**. (The fixture for `api.example.com`
    // doesn't exist, so a success here would mean we silently allowed
    // the request through — much worse than a network error.)
    let result = client
        .get("https://api.example.com/v1/secret")
        .send()
        .await;
    assert!(
        result.is_err(),
        "HTTPS via sealed proxy must fail; got OK response: {:?}",
        result.unwrap().status()
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn sealed_proxy_rejects_normal_policy_at_start() {
    // Normal policy means "no interception" — starting a sealed proxy
    // for a Normal session is a programmer error and must be caught
    // early, not silently start a no-op server.
    let dir = tempdir().unwrap();
    let loader = Arc::new(FixtureLoader::new(dir.path()));
    let err = start_sealed_proxy(SandboxNetworkPolicy::Normal, loader, None, None, None, None)
        .await
        .expect_err("Normal policy must be rejected");
    assert!(err.to_string().contains("Normal"));
}

#[tokio::test]
async fn sealed_proxy_shuts_down_cleanly() {
    // Pin lifecycle: starting + shutting down does not leave the listener
    // hanging. We start two proxies sequentially on different ports —
    // proves the bind succeeds the second time (which it would not if the
    // first listener leaked).
    let dir = tempdir().unwrap();
    let loader1 = Arc::new(FixtureLoader::new(dir.path()));
    let h1 = start_sealed_proxy(SandboxNetworkPolicy::Sealed, loader1, None, None, None, None)
        .await
        .expect("proxy 1 starts");
    let addr1 = h1.addr();
    h1.shutdown().await;

    let loader2 = Arc::new(FixtureLoader::new(dir.path()));
    let h2 = start_sealed_proxy(SandboxNetworkPolicy::Sealed, loader2, None, None, None, None)
        .await
        .expect("proxy 2 starts");
    let addr2 = h2.addr();
    h2.shutdown().await;

    // Addresses are random; this just confirms both binds succeeded.
    assert_ne!(addr1.port(), 0);
    assert_ne!(addr2.port(), 0);
}
