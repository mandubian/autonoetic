use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::memory::Tier2Memory;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::approval::approve_request;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use tempfile::tempdir;

fn test_manifest(capabilities: Vec<Capability>) -> AgentManifest {
    test_manifest_with_id("test-agent", capabilities)
}

fn test_manifest_with_id(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "test".to_string(),
        },
        capabilities,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn spawn_redirect_http_server(
    location: &str,
    final_status: &str,
    final_content_type: &str,
    final_body: String,
) -> (String, thread::JoinHandle<()>) {
    let location = location.to_string();
    let final_status = final_status.to_string();
    let final_content_type = final_content_type.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should expose local addr");
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buf = [0_u8; 2048];
            let _ = stream.read(&mut request_buf);
            let request = String::from_utf8_lossy(&request_buf);
            let response = if request.contains("GET /redirect") {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else {
                format!(
                    "HTTP/1.1 {final_status}\r\nContent-Type: {final_content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{final_body}",
                    final_body.len()
                )
            };
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), handle)
}

fn spawn_one_shot_http_server(
    status: &str,
    content_type: &str,
    body: String,
) -> (String, thread::JoinHandle<()>) {
    let status = status.to_string();
    let content_type = content_type.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should expose local addr");
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buf = [0_u8; 2048];
            let _ = stream.read(&mut request_buf);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), handle)
}

fn spawn_counting_http_server(
    status: &str,
    content_type: &str,
    body: String,
    expected_requests: usize,
) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let status = status.to_string();
    let content_type = content_type.to_string();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&hits);
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should expose local addr");
    let handle = thread::spawn(move || {
        for _ in 0..expected_requests {
            if let Ok((mut stream, _)) = listener.accept() {
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let mut request_buf = [0_u8; 2048];
                let _ = stream.read(&mut request_buf);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        }
    });
    (format!("http://{}", addr), hits, handle)
}

fn write_remote_access_any(agent_dir: &std::path::Path) {
    let skill = r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: "required"
      targets:
        - kind: "any"
      enabled_languages: []
      python_imports: []
      js_imports: []
      rust_imports: []
      go_imports: []
      function_calls: []
      shell_commands: []
      package_manager_commands: []
---
"#;
    std::fs::write(agent_dir.join("SKILL.md"), skill).expect("skill should write");
}

#[test]
fn test_native_tool_registry_availability() {
    let registry = default_registry();
    let manifest_none = test_manifest(vec![]);
    assert!(
        registry.available_definitions(&manifest_none).len() >= 9,
        "baseline tool set should include always-available tools"
    );
    let manifest_shell = test_manifest(vec![Capability::CodeExecution {
        patterns: vec!["*".into()],
        commands: vec![],
    }]);
    let defs = registry.available_definitions(&manifest_shell);
    assert!(defs.len() >= 10);
    assert!(defs.iter().any(|d| d.name == "sandbox_exec"));

    let manifest_all = test_manifest(vec![
        Capability::CodeExecution {
            patterns: vec![],
            commands: vec![],
        },
        Capability::ReadAccess { scopes: vec![] },
        Capability::WriteAccess { scopes: vec![] },
    ]);
    let defs_all = registry.available_definitions(&manifest_all);
    assert!(defs_all.len() >= 22);

    let manifest_spawn = test_manifest(vec![Capability::AgentSpawn {
        max_children: 4,
        max_spawn_depth: 0,
    }]);
    let defs_spawn = registry.available_definitions(&manifest_spawn);
    assert!(defs_spawn.len() >= 8);
    assert!(defs_spawn.iter().any(|d| d.name == "agent_spawn"));
    assert!(defs_spawn.iter().any(|d| d.name == "agent_discover"));
    assert!(defs_spawn.iter().any(|d| d.name == "workflow_wait"));

    let manifest_revision = test_manifest(vec![Capability::AgentRevision {
        patterns: vec!["*".to_string()],
    }]);
    let defs_revision = registry.available_definitions(&manifest_revision);
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent_revision_create"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent_revision_create_from_intent"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent_revision_list"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent_revision_inspect"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent_revision_promote"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent_revision_rollback"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent_revision_diff"));

    let manifest_net = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["*".to_string()],
    }]);
    let defs_net = registry.available_definitions(&manifest_net);
    assert!(defs_net.len() >= 10);
    assert!(defs_net.iter().any(|d| d.name == "web_search"));
    assert!(defs_net.iter().any(|d| d.name == "web_fetch"));
}

#[test]
fn test_workflow_wait_missing_task_returns_immediately_in_blocking_mode() {
    let manifest = test_manifest(vec![Capability::AgentSpawn {
        max_children: 4,
        max_spawn_depth: 0,
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let caller_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&caller_dir).expect("caller dir should create");

    let args = serde_json::json!({
        "workflow_id": "wf-missing",
        "task_ids": ["task-missing"],
        "timeout_secs": 30,
        "poll_interval_secs": 30
    });

    let started = std::time::Instant::now();
    let result = registry
        .execute(
            "workflow_wait",
            &manifest,
            &policy,
            &caller_dir,
            None,
            &args.to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("workflow.wait should succeed");

    let elapsed = started.elapsed();
    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("workflow.wait result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(
        parsed.get("join_satisfied"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(parsed.get("any_not_found"), Some(&serde_json::json!(true)));
    assert_eq!(parsed.get("waited_secs"), Some(&serde_json::json!(0)));
    assert_eq!(
        parsed.get("message").and_then(|v| v.as_str()),
        Some("One or more tasks were not found. Verify task_ids and workflow_id.")
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "blocking workflow.wait should fail fast for missing tasks"
    );
}

#[test]
fn test_web_fetch_tool_roundtrip_local_server() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());
    let (base_url, handle) = spawn_one_shot_http_server(
        "200 OK",
        "text/plain; charset=utf-8",
        "hello web fetch".to_string(),
    );

    let args = serde_json::json!({
        "url": format!("{}/doc", base_url),
        "timeout_secs": 10,
        "max_chars": 4096
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "web_fetch",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("web.fetch should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("web.fetch result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert!(parsed
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("hello web fetch"));

    handle.join().expect("server thread should join");
}

#[test]
fn test_web_fetch_follows_same_host_redirect() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let (final_base, final_handle) = spawn_one_shot_http_server(
        "200 OK",
        "text/plain; charset=utf-8",
        "after redirect".to_string(),
    );
    let final_url = format!("{final_base}/final");
    let (redirect_base, redirect_handle) =
        spawn_redirect_http_server(&final_url, "200 OK", "text/plain", String::new());

    let args = serde_json::json!({
        "url": format!("{redirect_base}/redirect"),
        "timeout_secs": 10,
        "max_chars": 4096
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "web_fetch",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("web.fetch should follow same-host redirect");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("web.fetch result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(parsed.get("redirect_hops"), Some(&serde_json::json!(1)));
    assert_eq!(parsed.get("final_url"), Some(&serde_json::json!(final_url)));
    assert!(parsed
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("after redirect"));

    redirect_handle.join().expect("redirect server should join");
    final_handle.join().expect("final server should join");
}

#[test]
fn test_web_fetch_cross_domain_redirect_requires_approval() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");
    let agent_dir = agents_dir.join(&manifest.agent.id);
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    write_remote_access_any(&agent_dir);

    let gateway_store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store should open"));
    let mut config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    config.approval_dwell_multiplier = 0.0;

    let (redirect_base, redirect_handle) = spawn_redirect_http_server(
        "http://example.com/cross-domain-target",
        "200 OK",
        "text/plain",
        String::new(),
    );

    let args = serde_json::json!({
        "url": format!("{redirect_base}/redirect"),
        "timeout_secs": 10
    });

    let result = registry
        .execute(
            "web_fetch",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            Some("root-test"),
            None,
            Some(&config),
            Some(gateway_store),
            None,
        )
        .expect("web.fetch should return approval payload");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(false)));
    assert_eq!(
        parsed.get("redirect_cross_domain"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        parsed.get("redirect_url"),
        Some(&serde_json::json!("http://example.com/cross-domain-target"))
    );
    assert_eq!(
        parsed.get("approval_required"),
        Some(&serde_json::json!(true))
    );

    redirect_handle.join().expect("redirect server should join");
}

#[test]
fn test_web_fetch_cross_domain_redirect_follows_when_target_pre_approved() {
    // Cross-domain redirect (127.0.0.1 → localhost) is followed transparently
    // when the target host is already in the agent's allowed hosts.
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let (final_base, final_handle) = spawn_one_shot_http_server(
        "200 OK",
        "text/plain; charset=utf-8",
        "cross-domain redirect followed".to_string(),
    );
    let final_url = format!("{final_base}/final");
    let (redirect_base, redirect_handle) =
        spawn_redirect_http_server(&final_url, "200 OK", "text/plain", String::new());

    let args = serde_json::json!({
        "url": format!("{redirect_base}/redirect"),
        "timeout_secs": 10,
        "max_chars": 4096
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "web_fetch",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("web.fetch should follow cross-domain redirect when target is pre-approved");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("web.fetch result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(parsed.get("redirect_hops"), Some(&serde_json::json!(1)));
    assert!(parsed
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("cross-domain redirect followed"));

    redirect_handle.join().expect("redirect server should join");
    final_handle.join().expect("final server should join");
}

#[test]
fn test_web_fetch_tool_denied_by_netconnect_policy() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["example.com".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let args = serde_json::json!({
        "url": "http://127.0.0.1:65535/forbidden"
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "web_fetch",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect_err("web.fetch should be denied");
    assert!(err.to_string().contains("NetworkAccess"));
}

#[test]
fn test_web_fetch_denied_by_netconnect_mints_approval_and_grant_allows_retry() {
    let manifest = test_manifest(vec![Capability::NetworkAccess { hosts: vec![] }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");
    let agent_dir = agents_dir.join(&manifest.agent.id);
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    write_remote_access_any(&agent_dir);

    let gateway_store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store should open"));
    let mut config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    config.approval_dwell_multiplier = 0.0;

    // First call: host is not allowed by NetworkAccess → approval required.
    let first_args = serde_json::json!({
        "url": "http://127.0.0.1:65535/needs-approval"
    });
    let first = registry
        .execute(
            "web_fetch",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &first_args.to_string(),
            Some("root-test"),
            None,
            Some(&config),
            Some(Arc::clone(&gateway_store)),
            None,
        )
        .expect("web.fetch should return approval-required payload");

    let parsed: serde_json::Value = serde_json::from_str(&first).expect("json should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(false)));
    assert_eq!(
        parsed.get("approval_required"),
        Some(&serde_json::json!(true))
    );
    let request_id = parsed
        .get("request_id")
        .and_then(|v| v.as_str())
        .expect("request_id present")
        .to_string();
    assert!(
        request_id.starts_with("apr-"),
        "request_id must look like an approval id: {}",
        request_id
    );

    let row = gateway_store
        .get_approval(&request_id)
        .expect("get_approval should succeed")
        .expect("approval row should exist");
    match row.action {
        ScheduledAction::WebFetch {
            url,
            detected_hosts,
            ..
        } => {
            assert_eq!(url, "http://127.0.0.1:65535/needs-approval");
            let hosts = detected_hosts.expect("detected_hosts should be set");
            assert!(hosts.contains(&"127.0.0.1".to_string()));
        }
        other => panic!("unexpected approval action: {:?}", other),
    }

    // Approve: should insert a session grant for 127.0.0.1
    approve_request(
        &config,
        Some(gateway_store.as_ref()),
        &request_id,
        "tester",
        Some("ok".to_string()),
        None,
        None,
        None,
    )
    .expect("approval should succeed");

    assert!(
        gateway_store.session_grants_cover_targets("root-test", &[String::from("127.0.0.1")]),
        "approval should create a session grant for 127.0.0.1"
    );

    // Retry (new URL, same host): should now succeed because the session grant covers 127.0.0.1.
    let (base_url, handle) = spawn_one_shot_http_server(
        "200 OK",
        "text/plain; charset=utf-8",
        "hello after approval".to_string(),
    );
    let retry_args = serde_json::json!({
        "url": format!("{}/doc", base_url),
        "timeout_secs": 10,
        "max_chars": 4096
    });
    let retry = registry
        .execute(
            "web_fetch",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &retry_args.to_string(),
            Some("root-test"),
            None,
            Some(&config),
            Some(gateway_store),
            None,
        )
        .expect("web.fetch should succeed after approval grant");

    let retry_parsed: serde_json::Value =
        serde_json::from_str(&retry).expect("retry response should decode");
    assert_eq!(retry_parsed.get("ok"), Some(&serde_json::json!(true)));
    assert!(retry_parsed
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("hello after approval"));

    handle.join().expect("server thread should join");
}

#[test]
fn test_web_search_tool_denied_by_netconnect_policy() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["example.com".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let args = serde_json::json!({
        "query": "rust",
        "engine_url": "http://127.0.0.1:65535/search"
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect_err("web.search should be denied");
    assert!(err.to_string().contains("NetworkAccess"));
}

#[test]
fn test_web_call_denied_by_netconnect_mints_approval_when_store_config_present() {
    let manifest = test_manifest(vec![Capability::NetworkAccess { hosts: vec![] }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");
    let agent_dir = agents_dir.join(&manifest.agent.id);
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    write_remote_access_any(&agent_dir);

    let gateway_store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store should open"));
    let mut config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    config.approval_dwell_multiplier = 0.0;

    let args = serde_json::json!({
        "url": "http://127.0.0.1:65535/forbidden",
        "method": "GET"
    });
    let result = registry
        .execute(
            "web_call",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            Some("root-test"),
            None,
            Some(&config),
            Some(Arc::clone(&gateway_store)),
            None,
        )
        .expect("web.call should return approval-required payload");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json should decode");
    assert_eq!(parsed.get("approval_required"), Some(&serde_json::json!(true)));
    let request_id = parsed["request_id"].as_str().expect("request_id").to_string();
    let row = gateway_store
        .get_approval(&request_id)
        .expect("get_approval should succeed")
        .expect("approval row should exist");
    assert!(
        matches!(row.action, ScheduledAction::WebCall { .. }),
        "expected WebCall action, got: {:?}",
        row.action
    );
}

#[test]
fn test_web_search_denied_by_netconnect_mints_approval_when_store_config_present() {
    let manifest = test_manifest(vec![Capability::NetworkAccess { hosts: vec![] }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");
    let agent_dir = agents_dir.join(&manifest.agent.id);
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    write_remote_access_any(&agent_dir);

    let gateway_store = Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store should open"));
    let mut config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    config.approval_dwell_multiplier = 0.0;

    let args = serde_json::json!({
        "query": "rust",
        "provider": "duckduckgo",
        "engine_url": "http://127.0.0.1:65535/search"
    });
    let result = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            Some("root-test"),
            None,
            Some(&config),
            Some(Arc::clone(&gateway_store)),
            None,
        )
        .expect("web.search should return approval-required payload");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json should decode");
    assert_eq!(parsed.get("approval_required"), Some(&serde_json::json!(true)));
    let request_id = parsed["request_id"].as_str().expect("request_id").to_string();
    let row = gateway_store
        .get_approval(&request_id)
        .expect("get_approval should succeed")
        .expect("approval row should exist");
    assert!(
        matches!(row.action, ScheduledAction::WebSearch { .. }),
        "expected WebSearch action, got: {:?}",
        row.action
    );
}

#[test]
fn test_web_search_tool_roundtrip_local_engine() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());
    let body = serde_json::json!({
        "Results": [],
        "RelatedTopics": [
            {
                "Text": "Rust language homepage",
                "FirstURL": "https://www.rust-lang.org/"
            },
            {
                "Name": "Docs",
                "Topics": [
                    {
                        "Text": "The Rust book",
                        "FirstURL": "https://doc.rust-lang.org/book/"
                    }
                ]
            }
        ]
    })
    .to_string();
    let (engine_url, handle) = spawn_one_shot_http_server("200 OK", "application/json", body);

    let args = serde_json::json!({
        "query": "rust language",
        "provider": "duckduckgo",
        "engine_url": engine_url,
        "max_results": 5
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("web.search should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("web.search result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert!(
        parsed
            .get("result_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            >= 2
    );

    handle.join().expect("server thread should join");
}

#[test]
fn test_web_search_google_requires_api_key_env() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let args = serde_json::json!({
        "query": "rust",
        "provider": "google",
        "engine_url": "http://127.0.0.1:65535/search",
        "google_engine_id": "cx-test",
        "google_api_key_env": "AUTONOETIC_TEST_GOOGLE_KEY_MISSING"
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect_err("google search without key should fail");
    assert!(err.to_string().contains("requires API key env"));
}

#[test]
fn test_web_search_google_roundtrip_local_engine() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());
    let body = serde_json::json!({
        "searchInformation": {
            "totalResults": "123"
        },
        "items": [
            {
                "title": "Rust language",
                "link": "https://www.rust-lang.org/",
                "snippet": "Rust empowers everyone."
            },
            {
                "title": "The Rust Book",
                "link": "https://doc.rust-lang.org/book/",
                "snippet": "Learn Rust."
            }
        ]
    })
    .to_string();
    let (engine_url, handle) = spawn_one_shot_http_server("200 OK", "application/json", body);

    let key_env = "AUTONOETIC_TEST_GOOGLE_KEY_OK";
    let cx_env = "AUTONOETIC_TEST_GOOGLE_CX_OK";
    let prior_key = std::env::var(key_env).ok();
    let prior_cx = std::env::var(cx_env).ok();
    std::env::set_var(key_env, "test-api-key");
    std::env::set_var(cx_env, "test-cx-id");

    let args = serde_json::json!({
        "query": "rust language",
        "provider": "google",
        "engine_url": engine_url,
        "google_api_key_env": key_env,
        "google_engine_id_env": cx_env
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("google web.search should succeed");

    match prior_key {
        Some(value) => std::env::set_var(key_env, value),
        None => std::env::remove_var(key_env),
    }
    match prior_cx {
        Some(value) => std::env::set_var(cx_env, value),
        None => std::env::remove_var(cx_env),
    }
    handle.join().expect("server thread should join");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("web.search result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(parsed.get("provider"), Some(&serde_json::json!("google")));
    assert_eq!(parsed.get("total_results"), Some(&serde_json::json!(123)));
    assert_eq!(parsed.get("result_count"), Some(&serde_json::json!(2)));
}

#[test]
fn test_web_search_google_legacy_cx_env_alias_roundtrip() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let body = serde_json::json!({
        "searchInformation": {
            "totalResults": "7"
        },
        "items": [
            {
                "title": "Example result",
                "link": "https://example.com/",
                "snippet": "example"
            }
        ]
    })
    .to_string();
    let (engine_url, handle) = spawn_one_shot_http_server("200 OK", "application/json", body);

    let key_env = "GOOGLE_SEARCH_API_KEY";
    let cx_env = "GOOGLE_SEARCH_CX";
    let prior_key = std::env::var(key_env).ok();
    let prior_cx = std::env::var(cx_env).ok();
    std::env::set_var(key_env, "legacy-test-api-key");
    std::env::set_var(cx_env, "legacy-test-cx");

    let args = serde_json::json!({
        "query": "legacy cx alias",
        "provider": "google",
        "engine_url": engine_url
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("google web.search should accept GOOGLE_SEARCH_CX legacy alias");

    match prior_key {
        Some(value) => std::env::set_var(key_env, value),
        None => std::env::remove_var(key_env),
    }
    match prior_cx {
        Some(value) => std::env::set_var(cx_env, value),
        None => std::env::remove_var(cx_env),
    }
    handle.join().expect("server thread should join");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("web.search result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(parsed.get("provider"), Some(&serde_json::json!("google")));
    assert_eq!(parsed.get("result_count"), Some(&serde_json::json!(1)));
}

#[test]
fn test_web_search_auto_falls_back_to_duckduckgo_when_google_fails() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let google_body = serde_json::json!({
        "error": { "message": "quota exceeded" }
    })
    .to_string();
    let (google_engine_url, google_handle) =
        spawn_one_shot_http_server("500 Internal Server Error", "application/json", google_body);

    let ddg_body = serde_json::json!({
        "Results": [],
        "RelatedTopics": [
            {
                "Text": "Rust official site",
                "FirstURL": "https://www.rust-lang.org/"
            }
        ]
    })
    .to_string();
    let (duckduckgo_engine_url, ddg_handle) =
        spawn_one_shot_http_server("200 OK", "application/json", ddg_body);

    let key_env = "AUTONOETIC_TEST_GOOGLE_KEY_AUTO";
    let cx_env = "AUTONOETIC_TEST_GOOGLE_CX_AUTO";
    let prior_key = std::env::var(key_env).ok();
    let prior_cx = std::env::var(cx_env).ok();
    std::env::set_var(key_env, "test-api-key");
    std::env::set_var(cx_env, "test-cx-id");

    let args = serde_json::json!({
        "query": "rust language",
        "provider": "auto",
        "google_engine_url": google_engine_url,
        "duckduckgo_engine_url": duckduckgo_engine_url,
        "google_api_key_env": key_env,
        "google_engine_id_env": cx_env
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("auto provider should fall back to duckduckgo");

    match prior_key {
        Some(value) => std::env::set_var(key_env, value),
        None => std::env::remove_var(key_env),
    }
    match prior_cx {
        Some(value) => std::env::set_var(cx_env, value),
        None => std::env::remove_var(cx_env),
    }

    google_handle
        .join()
        .expect("google server thread should join");
    ddg_handle.join().expect("ddg server thread should join");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("web.search result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(
        parsed.get("requested_provider"),
        Some(&serde_json::json!("auto"))
    );
    assert_eq!(
        parsed.get("provider"),
        Some(&serde_json::json!("duckduckgo"))
    );
    let attempted = parsed
        .get("attempted_providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(attempted.contains(&serde_json::json!("google")));
    assert!(attempted.contains(&serde_json::json!("duckduckgo")));
    assert!(parsed
        .get("fallback_reason")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("google provider failed"));
}

#[test]
fn test_web_search_cache_hits_without_second_network_call() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    write_remote_access_any(temp.path());

    let body = serde_json::json!({
        "Results": [],
        "RelatedTopics": [
            {
                "Text": "Rust language homepage",
                "FirstURL": "https://www.rust-lang.org/"
            }
        ]
    })
    .to_string();
    let (engine_url, hits, handle) =
        spawn_counting_http_server("200 OK", "application/json", body, 1);

    let unique_query = format!(
        "rust cache {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos()
    );
    let args = serde_json::json!({
        "query": unique_query,
        "provider": "duckduckgo",
        "duckduckgo_engine_url": engine_url,
        "cache_ttl_secs": 300
    });

    let registry = default_registry();
    let first = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("first web.search call should succeed");
    let second = registry
        .execute(
            "web_search",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("second web.search call should succeed");

    let first_parsed: serde_json::Value =
        serde_json::from_str(&first).expect("first response should decode");
    let second_parsed: serde_json::Value =
        serde_json::from_str(&second).expect("second response should decode");
    assert_eq!(
        first_parsed.get("cache_hit"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(
        second_parsed.get("cache_hit"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    handle.join().expect("server thread should join");
}

#[test]
fn test_scheduler_cron_create_rejects_sub10s_for_reasoning_target() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

    let caller_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&caller_dir).expect("caller dir should create");

    let target_dir = agents_dir.join("reasoner.default");
    std::fs::create_dir_all(&target_dir).expect("target dir should create");
    std::fs::write(
        target_dir.join("SKILL.md"),
        "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"reasoner.default\"\n  name: \"reasoner.default\"\n  description: \"Reasoning target\"\nexecution_mode: reasoning\nllm_config:\n  provider: \"openai\"\n  model: \"gpt-4o-mini\"\n---\n# Instructions\nReasoning test agent.\n",
    )
    .expect("target skill should write");

    let manifest = test_manifest(vec![Capability::SchedulerAccess {
        patterns: vec!["*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store =
        std::sync::Arc::new(GatewayStore::open(&gateway_dir).expect("gateway store should open"));

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };

    let args = serde_json::json!({
        "message": "tick",
        "schedule_expr": "every 5 seconds",
        "target_agent_id": "reasoner.default"
    });

    let result = registry
        .execute(
            "scheduler_cron_create",
            &manifest,
            &policy,
            &caller_dir,
            None,
            &args.to_string(),
            Some("root-test"),
            None,
            Some(&config),
            Some(gateway_store),
            None,
        )
        .expect("tool call should succeed with structured rejection");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("scheduler response should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(false)));
    assert!(parsed
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("Sub-10s schedules are only allowed for script-mode agents"));
    assert_eq!(
        parsed.get("error_type"),
        Some(&serde_json::json!("validation"))
    );
}

#[test]
fn test_agent_spawn_tool_validates_non_empty_message() {
    let manifest = test_manifest(vec![Capability::AgentSpawn {
        max_children: 2,
        max_spawn_depth: 0,
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&parent_dir).expect("parent dir should create");

    let args = serde_json::json!({
        "agent_id": "researcher.default",
        "message": ""
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "agent_spawn",
            &manifest,
            &policy,
            &parent_dir,
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            Some("session-1"),
            None,
            None,
            None,
            None,
        )
        .expect_err("empty message should be rejected");
    assert!(err.to_string().contains("message must not be empty"));
}

#[test]
fn test_agent_spawn_tool_accepts_metadata_argument() {
    let manifest = test_manifest(vec![Capability::AgentSpawn {
        max_children: 2,
        max_spawn_depth: 0,
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&parent_dir).expect("parent dir should create");

    let args = serde_json::json!({
        "agent_id": "researcher.default",
        "message": "",
        "metadata": {
            "delegated_role": "researcher",
            "expected_outputs": ["summary.md", "sources.json"]
        }
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "agent_spawn",
            &manifest,
            &policy,
            &parent_dir,
            None,
            &serde_json::to_string(&args).expect("json should encode"),
            Some("session-1"),
            None,
            None,
            None,
            None,
        )
        .expect_err("empty message should be rejected even with metadata");
    assert!(err.to_string().contains("message must not be empty"));
}

#[test]
fn test_agent_spawn_coalesces_duplicate_durable_operation() {
    let manifest = test_manifest_with_id(
        "planner.default",
        vec![Capability::AgentSpawn {
            max_children: 2,
            max_spawn_depth: 0,
        }],
    );
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    let child_dir = agents_dir.join("builder.default");
    std::fs::create_dir_all(&parent_dir).expect("parent dir should create");
    std::fs::create_dir_all(&child_dir).expect("child dir should create");

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let gateway_store = Arc::new(
        GatewayStore::open(&gateway_dir).expect("gateway store should open"),
    );

    let args = serde_json::json!({
        "agent_id": "builder.default",
        "message": "Build the durable artifact and keep the workflow state authoritative.",
        "metadata": {
            "stage_kind": "durable_build",
            "artifact_ref": "ar.test-build-01"
        }
    });

    let registry = default_registry();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();
    let first = registry
        .execute(
            "agent_spawn",
            &manifest,
            &policy,
            &parent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).expect("json should encode"),
            Some("root-single-flight"),
            None,
            Some(&config),
            Some(gateway_store.clone()),
            None,
        )
        .expect("first durable spawn should queue");
    let first_json: serde_json::Value =
        serde_json::from_str(&first).expect("first response should decode");
    assert_eq!(first_json.get("status"), Some(&serde_json::json!("queued")));
    let first_task_id = first_json
        .get("task_id")
        .and_then(|value| value.as_str())
        .expect("queued response should include task_id")
        .to_string();

    let second = registry
        .execute(
            "agent_spawn",
            &manifest,
            &policy,
            &parent_dir,
            Some(&gateway_dir),
            &serde_json::to_string(&args).expect("json should encode"),
            Some("root-single-flight"),
            None,
            Some(&config),
            Some(gateway_store),
            None,
        )
        .expect("duplicate durable spawn should coalesce");
    let second_json: serde_json::Value =
        serde_json::from_str(&second).expect("coalesced response should decode");
    assert_eq!(second_json.get("status"), Some(&serde_json::json!("coalesced")));
    assert_eq!(
        second_json.get("existing_task_id"),
        Some(&serde_json::json!(first_task_id))
    );
    assert_eq!(
        second_json.get("retry_advice"),
        Some(&serde_json::json!("wait"))
    );
}

#[test]
fn test_skill_normalize_writes_autonoetic_skill_under_skills_scope() {
    let manifest = test_manifest(vec![Capability::WriteAccess {
        scopes: vec!["skills/*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agent_dir = temp.path().join("agents").join("planner.default");
    std::fs::create_dir_all(&agent_dir).expect("agent workspace should create");

    let md = r#"## Example API

Use this to register:

POST /v1/register
{
  "username": "alice"
}
"#;

    let args = serde_json::json!({
        "intent": "normalize third-party doc for credential_setup",
        "content": md,
        "service": "examplesvc",
        "source_url": "https://api.example.com/docs"
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "skill_normalize",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("skill_normalize should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("skill_normalize result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    let rel = parsed
        .get("skill_path")
        .and_then(|v| v.as_str())
        .expect("skill_path");
    assert_eq!(rel, "skills/examplesvc/SKILL.md");
    assert_eq!(parsed.get("steps_count").and_then(|v| v.as_u64()), Some(1_u64));

    let written = std::fs::read_to_string(agent_dir.join(rel)).expect("normalized file should exist");
    assert!(written.starts_with("---\n"));
    assert!(written.contains("autonoetic:"));
    assert!(written.contains("/v1/register"));
}

#[test]
fn test_skill_normalize_auto_registers_discovery_record() {
    let manifest = test_manifest_with_id(
        "planner.default",
        vec![Capability::WriteAccess {
            scopes: vec!["skills/*".to_string()],
        }],
    );
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agent_dir = temp.path().join("agents").join("planner.default");
    std::fs::create_dir_all(&agent_dir).expect("agent workspace should create");

    let md = r#"## Example API

Use this to register:

POST /v1/register
{
  "username": "alice"
}
"#;

    let args = serde_json::json!({
        "intent": "normalize and register discovery",
        "content": md,
        "service": "examplesvc",
        "source_url": "https://api.example.com/docs"
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "skill_normalize",
            &manifest,
            &policy,
            &agent_dir,
            Some(temp.path()),
            &args.to_string(),
            Some("sess-1"),
            Some("turn-1"),
            None,
            None,
            None,
        )
        .expect("skill_normalize should succeed");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("skill_normalize result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(
        parsed.get("discovery_record_registered"),
        Some(&serde_json::json!(true))
    );

    let mem = Tier2Memory::open_sqlite(temp.path(), "planner.default")
        .expect("memory should open on gateway dir");
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should initialize");
    let recalled = rt
        .block_on(mem.recall("registration:examplesvc"))
        .expect("discovery record should be present");
    assert_eq!(recalled.scope, "skills");
    assert!(matches!(
        recalled.visibility,
        autonoetic_types::memory::MemoryVisibility::Global
    ));
    assert!(recalled.content.contains("\"service\":\"examplesvc\""));
    assert!(recalled.content.contains("\"skill_path\":\"skills/examplesvc/SKILL.md\""));
}

#[test]
fn test_skill_normalize_moltbook_fixture_generates_expected_steps() {
    let manifest = test_manifest(vec![Capability::WriteAccess {
        scopes: vec!["skills/*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();
    let agent_dir = temp.path().join("agents").join("planner.default");
    std::fs::create_dir_all(&agent_dir).expect("agent workspace should create");

    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("bin")
        .join("mock_moltbook_skill.md");
    let fixture_markdown = std::fs::read_to_string(&fixture_path).expect("fixture should read");

    let args = serde_json::json!({
        "intent": "normalize moltbook fixture for credential_setup",
        "content": fixture_markdown,
        "service": "moltbook",
        "source_url": "http://127.0.0.1:8787/skill.md"
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "skill_normalize",
            &manifest,
            &policy,
            &agent_dir,
            Some(gateway_dir),
            &args.to_string(),
            Some("session-1"),
            None,
            None,
            None,
            None,
        )
        .expect("skill_normalize should succeed for fixture");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("skill_normalize result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    assert_eq!(
        parsed.get("skill_path").and_then(|v| v.as_str()),
        Some("skills/moltbook/SKILL.md")
    );
    assert_eq!(parsed.get("steps_count").and_then(|v| v.as_u64()), Some(6_u64));
    assert_eq!(
        parsed
            .get("session_content")
            .and_then(|v| v.get("normalized_name"))
            .and_then(|v| v.as_str()),
        Some("skill.moltbook.md")
    );
    assert!(
        parsed
            .get("session_content")
            .and_then(|v| v.get("normalized_ref"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .starts_with("cnt_")
    );

    let written = std::fs::read_to_string(agent_dir.join("skills/moltbook/SKILL.md"))
        .expect("normalized fixture file should exist");
    assert!(written.contains("autonoetic:"));
    assert!(written.contains("service: moltbook"));
    assert!(written.contains("inject_as: MOLTBOOK_SECRET"));
    assert!(written.contains("/api/register-agent"));
    assert!(written.contains("/api/human-claim"));
    assert!(written.contains("/api/verify-human-claim"));
    assert!(written.contains("/api/setup-heartbeat"));
    assert!(written.contains("/api/post-to-feed"));
    assert!(written.contains("/status"));

    let store = ContentStore::new(gateway_dir).expect("content store should open");
    let registered = store
        .read_by_name_or_handle("session-1", "skill.moltbook.md")
        .expect("normalized content should be registered in session content");
    let registered =
        String::from_utf8(registered).expect("registered content should be utf-8");
    assert_eq!(registered, written);
}

#[test]
fn test_skill_normalize_sole_url_in_content_fetches_and_normalizes() {
    let md = "## Example API\n\nPOST /v1/ping\n{\"x\":1}\n";
    let (base_url, handle) = spawn_one_shot_http_server("200 OK", "text/markdown", md.to_string());
    let fetch_url = format!("{}/skill.md", base_url.trim_end_matches('/'));

    let manifest = test_manifest(vec![
        Capability::WriteAccess {
            scopes: vec!["skills/*".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agent_dir = temp.path().join("agents").join("planner.default");
    std::fs::create_dir_all(&agent_dir).expect("agent workspace should create");

    let args = serde_json::json!({
        "intent": "normalize from URL mistakenly passed as content",
        "content": fetch_url,
        "service": "urlskill"
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "skill_normalize",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("skill_normalize should succeed after fetch");

    let _ = handle.join();

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("skill_normalize result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(true)));
    let rel = parsed
        .get("skill_path")
        .and_then(|v| v.as_str())
        .expect("skill_path");
    assert_eq!(rel, "skills/urlskill/SKILL.md");
    let written = std::fs::read_to_string(agent_dir.join(rel)).expect("normalized file should exist");
    assert!(written.contains("autonoetic:"));
    assert!(written.contains("/v1/ping"));
}

#[test]
fn test_skill_normalize_sole_url_requires_network_access() {
    let manifest = test_manifest(vec![Capability::WriteAccess {
        scopes: vec!["skills/*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agent_dir = temp.path().join("agents").join("planner.default");
    std::fs::create_dir_all(&agent_dir).expect("agent workspace should create");

    let args = serde_json::json!({
        "intent": "try URL-only content without NetworkAccess",
        "content": "http://127.0.0.1:65530/missing.md",
        "service": "noop"
    });

    let registry = default_registry();
    let result = registry
        .execute(
            "skill_normalize",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("skill_normalize should return JSON error");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("skill_normalize result should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(false)));
    let err = parsed
        .get("error")
        .and_then(|v| v.as_str())
        .expect("error string");
    assert!(
        err.contains("NetworkAccess"),
        "expected policy hint, got: {err}"
    );
}

#[test]
fn test_skill_normalize_denied_without_skills_write_capability() {
    let manifest = test_manifest(vec![Capability::WriteAccess {
        scopes: vec!["self.*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agent_dir = temp.path();

    let registry = default_registry();
    assert!(
        !registry.available_definitions(&manifest).iter().any(|d| d.name == "skill_normalize"),
        "skill_normalize should not appear without skills/* write scope"
    );

    let args = serde_json::json!({
        "intent": "should not run",
        "content": "POST /x",
        "service": "noop"
    });

    let result = registry
        .execute(
            "skill_normalize",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("skill_normalize should return a structured permission error");
    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("permission response should decode");
    assert_eq!(parsed.get("ok"), Some(&serde_json::json!(false)));
    assert_eq!(parsed.get("error_type"), Some(&serde_json::json!("permission")));
    assert!(parsed["message"]
        .as_str()
        .unwrap_or_default()
        .contains("skill_normalize"));
}

#[test]
fn test_skill_normalize_requires_non_empty_intent() {
    let manifest = test_manifest(vec![Capability::WriteAccess {
        scopes: vec!["skills/*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
    let agent_dir = temp.path();

    let args = serde_json::json!({
        "intent": "",
        "content": "POST /ping",
        "service": "svc"
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "skill_normalize",
            &manifest,
            &policy,
            &agent_dir,
            None,
            &args.to_string(),
            None,
            None,
            None,
            None,
            None,
        )
        .expect_err("skill_normalize must reject blank intent");
    assert!(
        err.to_string().contains("intent"),
        "expected intent validation error; got {}",
        err
    );
}

#[test]
fn test_credential_setup_and_skill_normalize_exposed_for_planner_like_caps() {
    let registry = default_registry();
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["*".to_string()],
        },
        Capability::WriteAccess {
            scopes: vec!["skills/*".to_string()],
        },
    ]);
    let defs = registry.available_definitions(&manifest);
    assert!(defs.iter().any(|d| d.name == "credential_setup"));
    assert!(defs.iter().any(|d| d.name == "skill_normalize"));
}
