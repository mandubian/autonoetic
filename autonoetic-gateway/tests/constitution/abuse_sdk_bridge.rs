//! Constitution R+10: Sandbox→gateway SDK-bridge rate and payload-size limits.
//!
//! R+10 ensures that a sandboxed process cannot flood the gateway or balloon
//! the content store through unbounded SDK bridge calls. Per-session rate
//! limit (default 100/sec) and per-call payload size cap (default 1 MiB).
//! Violations return structured errors and log `sdk_bridge_abuse` to the
//! causal chain.


use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use autonoetic_gateway::log_redaction::RedactedPayload;
use autonoetic_gateway::sandbox::{
    SdkBridgeRateLimiter, SDK_BRIDGE_MAX_PAYLOAD_BYTES, SDK_BRIDGE_RATE_LIMIT_PER_SEC,
};
use tempfile::tempdir;

fn make_sdk_request(method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
    .to_string()
}

fn parse_response(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw.trim()).unwrap()
}

#[test]
fn r10_rate_limiter_allows_under_limit() {
    let limiter = SdkBridgeRateLimiter::new(10, 1024);
    let t = 1000;
    for _ in 0..10 {
        assert!(limiter.check_rate_at(t), "calls under limit should succeed");
    }
}

#[test]
fn r10_rate_limiter_blocks_over_limit() {
    let limiter = SdkBridgeRateLimiter::new(5, 1024);
    let t = 2000;
    for _ in 0..5 {
        assert!(limiter.check_rate_at(t), "calls under limit should succeed");
    }
    assert!(
        !limiter.check_rate_at(t),
        "call over limit should be blocked"
    );
}

#[test]
fn r10_rate_limiter_default_config() {
    assert_eq!(
        SDK_BRIDGE_RATE_LIMIT_PER_SEC, 100,
        "default rate limit should be 100/sec"
    );
    assert_eq!(
        SDK_BRIDGE_MAX_PAYLOAD_BYTES, 1_048_576,
        "default max payload should be 1 MiB"
    );
}

#[test]
fn r10_oversized_payload_returns_error() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("test-agent");
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();

    let socket_path = temp.path().join("test-r10-oversize.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).unwrap();
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let agent_dir_clone = agent_dir.clone();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let handle = thread::spawn(move || {
        let rate_limiter = SdkBridgeRateLimiter::new(100, 256);
        while !stop_clone.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ =
                        handle_sdk_in_test(stream, &agent_dir_clone, &gateway_dir, &rate_limiter);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut client = UnixStream::connect(&socket_path).unwrap();
    let big_content = "x".repeat(512);
    let request = make_sdk_request(
        "memory_write",
        serde_json::json!({"path": "test.txt", "content": big_content}),
    );
    writeln!(client, "{}", request).unwrap();
    client.flush().unwrap();

    let mut response = String::new();
    BufReader::new(&client).read_line(&mut response).unwrap();

    stop.store(true, Ordering::SeqCst);
    let _ = UnixStream::connect(&socket_path);
    let _ = handle.join();

    let parsed = parse_response(&response);
    let error = parsed.get("error").expect("should have error");
    assert_eq!(
        error["code"], -32001,
        "oversized payload should return code -32001"
    );
    assert_eq!(
        error["message"], "payload_too_large",
        "error message should be payload_too_large"
    );
    assert_eq!(
        error["data"]["error_type"], "sdk_bridge_abuse",
        "error type should be sdk_bridge_abuse"
    );

    let log_path = agent_dir.join("history").join("causal_chain.jsonl");
    let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log_content.contains("payload_too_large"),
        "should log sdk_bridge_abuse with payload_too_large to causal chain"
    );
}

#[test]
fn r10_rate_limited_returns_error() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("test-agent");
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();

    let socket_path = temp.path().join("test-r10-rate.sock");
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).unwrap();
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let agent_dir_clone = agent_dir.clone();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let handle = thread::spawn(move || {
        let rate_limiter = SdkBridgeRateLimiter::new(3, 1_048_576);
        while !stop_clone.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ =
                        handle_sdk_in_test(stream, &agent_dir_clone, &gateway_dir, &rate_limiter);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let mut last_response = String::new();
    for i in 0..5 {
        let mut client = UnixStream::connect(&socket_path).unwrap();
        let request = make_sdk_request("memory_list_keys", serde_json::json!({}));
        writeln!(client, "{}", request).unwrap();
        client.flush().unwrap();
        let mut response = String::new();
        BufReader::new(&client).read_line(&mut response).unwrap();
        last_response = response;
        drop(client);
    }

    stop.store(true, Ordering::SeqCst);
    let _ = UnixStream::connect(&socket_path);
    let _ = handle.join();

    let parsed = parse_response(&last_response);
    let error = parsed
        .get("error")
        .expect("later calls should be rate-limited");
    assert_eq!(
        error["code"], -32002,
        "rate-limited call should return code -32002"
    );
    assert_eq!(
        error["message"], "rate_limited",
        "error message should be rate_limited"
    );
    assert_eq!(
        error["data"]["error_type"], "sdk_bridge_abuse",
        "error type should be sdk_bridge_abuse"
    );

    let log_path = agent_dir.join("history").join("causal_chain.jsonl");
    let log_content = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log_content.contains("rate_limited"),
        "should log sdk_bridge_abuse with rate_limited to causal chain"
    );
}

fn handle_sdk_in_test(
    mut stream: std::os::unix::net::UnixStream,
    agent_dir: &Path,
    gateway_dir: &Path,
    rate_limiter: &SdkBridgeRateLimiter,
) -> anyhow::Result<()> {
    use autonoetic_gateway::sandbox::{gateway_dir_from_agent_dir, validate_sdk_relative_path};
    use std::fs;
    use std::io::{BufReader, Write};

    let gw_dir = if gateway_dir.exists() {
        gateway_dir.to_path_buf()
    } else {
        gateway_dir_from_agent_dir(agent_dir)?
    };

    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader.read_line(&mut line)?;
    }
    if line.trim().is_empty() {
        return Ok(());
    }

    if line.len() > rate_limiter.max_payload_bytes() {
        let log_path = agent_dir.join("history").join("causal_chain.jsonl");
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let logger = autonoetic_gateway::causal_chain::CausalLogger::new(&log_path)?;
        logger.log(
            "test-agent",
            "sdk-bridge",
            None,
            1,
            "abuse",
            "payload_too_large",
            autonoetic_types::causal_chain::EntryStatus::Denied,
            Some(RedactedPayload::from_raw(serde_json::json!({
                "detail": format!("{} bytes exceeds {} limit", line.len(), rate_limiter.max_payload_bytes()),
                "violation": "payload_too_large",
            }))),
        )?;
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": -32001,
                "message": "payload_too_large",
                "data": {"error_type": "sdk_bridge_abuse", "max_bytes": rate_limiter.max_payload_bytes()}
            }
        });
        let payload = serde_json::to_string(&error_resp)? + "\n";
        stream.write_all(payload.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    let request: serde_json::Value = serde_json::from_str(&line)?;
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = request
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = request
        .get("params")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    if !rate_limiter.check_rate() {
        let log_path = agent_dir.join("history").join("causal_chain.jsonl");
        if let Some(parent) = log_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let logger = autonoetic_gateway::causal_chain::CausalLogger::new(&log_path)?;
        logger.log(
            "test-agent",
            "sdk-bridge",
            None,
            1,
            "abuse",
            "rate_limited",
            autonoetic_types::causal_chain::EntryStatus::Denied,
            Some(RedactedPayload::from_raw(serde_json::json!({
                "detail": format!("sdk bridge call '{}' exceeded rate limit of {}/sec", method, rate_limiter.rate_limit()),
                "violation": "rate_limited",
            }))),
        )?;
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32002,
                "message": "rate_limited",
                "data": {"error_type": "sdk_bridge_abuse", "rate_limit_per_sec": rate_limiter.rate_limit()}
            }
        });
        let payload = serde_json::to_string(&error_resp)? + "\n";
        stream.write_all(payload.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    let result = match method.as_str() {
        "memory_list_keys" => serde_json::json!({"keys": []}),
        "memory_write" => {
            let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
            validate_sdk_relative_path(path)?;
            let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let target = agent_dir.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, content)?;
            serde_json::json!({"ok": true})
        }
        _ => serde_json::json!({"ok": true}),
    };

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    let payload = serde_json::to_string(&response)? + "\n";
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    Ok(())
}
