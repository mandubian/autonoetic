use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
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
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
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
    assert!(defs.iter().any(|d| d.name == "sandbox.exec"));

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

    let manifest_spawn = test_manifest(vec![Capability::AgentSpawn { max_children: 4 }]);
    let defs_spawn = registry.available_definitions(&manifest_spawn);
    assert!(defs_spawn.len() >= 8);
    assert!(defs_spawn.iter().any(|d| d.name == "agent.spawn"));
    assert!(defs_spawn.iter().any(|d| d.name == "agent.exists"));
    assert!(defs_spawn.iter().any(|d| d.name == "agent.discover"));
    assert!(defs_spawn.iter().any(|d| d.name == "workflow.wait"));

    let manifest_revision = test_manifest(vec![Capability::AgentRevision {
        patterns: vec!["*".to_string()],
    }]);
    let defs_revision = registry.available_definitions(&manifest_revision);
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent.revision.create"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent.revision.create_from_intent"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent.revision.list"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent.revision.inspect"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent.revision.promote"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent.revision.rollback"));
    assert!(defs_revision
        .iter()
        .any(|d| d.name == "agent.revision.diff"));

    let manifest_net = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["*".to_string()],
    }]);
    let defs_net = registry.available_definitions(&manifest_net);
    assert!(defs_net.len() >= 10);
    assert!(defs_net.iter().any(|d| d.name == "web.search"));
    assert!(defs_net.iter().any(|d| d.name == "web.fetch"));
}

#[test]
fn test_workflow_wait_missing_task_returns_immediately_in_blocking_mode() {
    let manifest = test_manifest(vec![Capability::AgentSpawn { max_children: 4 }]);
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
            "workflow.wait",
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
            "web.fetch",
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
fn test_web_fetch_tool_denied_by_netconnect_policy() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["example.com".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");

    let args = serde_json::json!({
        "url": "http://127.0.0.1:65535/forbidden"
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "web.fetch",
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
fn test_web_search_tool_denied_by_netconnect_policy() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["example.com".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");

    let args = serde_json::json!({
        "query": "rust",
        "engine_url": "http://127.0.0.1:65535/search"
    });

    let registry = default_registry();
    let err = registry
        .execute(
            "web.search",
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
fn test_web_search_tool_roundtrip_local_engine() {
    let manifest = test_manifest(vec![Capability::NetworkAccess {
        hosts: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let temp = tempdir().expect("tempdir should create");
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
            "web.search",
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
            "web.search",
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
            "web.search",
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
            "web.search",
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
            "web.search",
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
            "web.search",
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
            "web.search",
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
            "scheduler.cron.create",
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
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("Sub-10s schedules are only allowed for script-mode agents"));
}

#[test]
fn test_agent_spawn_tool_validates_non_empty_message() {
    let manifest = test_manifest(vec![Capability::AgentSpawn { max_children: 2 }]);
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
            "agent.spawn",
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
    let manifest = test_manifest(vec![Capability::AgentSpawn { max_children: 2 }]);
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
            "agent.spawn",
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
