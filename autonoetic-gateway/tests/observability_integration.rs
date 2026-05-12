use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::hooks::HookExecutor;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::PublishedSessionReportRecord;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::hooks::{HookAction, HookConfig, HookContext, HookEvent};
use std::sync::Arc;
use tempfile::tempdir;

fn test_manifest() -> AgentManifest {
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
            id: "coder.default".to_string(),
            name: "coder".to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
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

#[test]
fn test_published_report_catalog_roundtrip() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let record = PublishedSessionReportRecord {
        root_session_id: "demo-session-1".to_string(),
        report_handle: "sha256:abc123".to_string(),
        overview_handle: None,
        html_handle: Some("sha256:html456".to_string()),
        narrative_handle: None,
        title: "Session report: demo-session-1".to_string(),
        status: "completed".to_string(),
        started_at: Some("2026-04-12T10:00:00Z".to_string()),
        ended_at: Some("2026-04-12T10:30:00Z".to_string()),
        agent_count: 2,
        error_count: 1,
        approval_count: 1,
        search_text: "demo-session-1 coder.default researcher.default weather api".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        report_version: 1,
    };

    store.upsert_published_session_report(&record)?;

    let found = store.find_published_report("demo-session-1")?;
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.root_session_id, "demo-session-1");
    assert_eq!(found.report_handle, "sha256:abc123");
    assert_eq!(found.html_handle, Some("sha256:html456".to_string()));
    assert_eq!(found.agent_count, 2);
    assert_eq!(found.error_count, 1);

    let missing = store.find_published_report("nonexistent")?;
    assert!(missing.is_none());

    Ok(())
}

#[test]
fn test_published_report_search() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    for i in 1..=3 {
        store.upsert_published_session_report(&PublishedSessionReportRecord {
            root_session_id: format!("session-{}", i),
            report_handle: format!("sha256:handle{}", i),
            overview_handle: None,
            html_handle: None,
            narrative_handle: None,
            title: format!("Session report: session-{}", i),
            status: "completed".to_string(),
            started_at: None,
            ended_at: None,
            agent_count: 1,
            error_count: 0,
            approval_count: 0,
            search_text: format!("session-{} weather api python", i),
            generated_at: chrono::Utc::now().to_rfc3339(),
            report_version: 1,
        })?;
    }

    let results = store.search_published_reports("weather", 10)?;
    assert_eq!(results.len(), 3, "FTS or LIKE should match all 3 reports");

    let results = store.search_published_reports("session-2", 10)?;
    assert!(results.len() >= 1, "Should find session-2");

    Ok(())
}

#[test]
fn test_observability_search_tool() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let config = GatewayConfig::default();

    store.upsert_published_session_report(&PublishedSessionReportRecord {
        root_session_id: "root-sess-1".to_string(),
        report_handle: "sha256:deadbeef".to_string(),
        overview_handle: None,
        html_handle: None,
        narrative_handle: None,
        title: "Session report: root-sess-1".to_string(),
        status: "completed".to_string(),
        started_at: Some("2026-04-12T10:00:00Z".to_string()),
        ended_at: Some("2026-04-12T10:30:00Z".to_string()),
        agent_count: 3,
        error_count: 0,
        approval_count: 2,
        search_text: "root-sess-1 coder.default weather".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        report_version: 1,
    })?;

    let registry = default_registry();
    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    let args = serde_json::json!({"query": "weather"});
    let result = registry.execute(
        "observability_search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(parsed["ok"].as_bool().unwrap());
    let results = parsed["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["root_session_id"].as_str(), Some("root-sess-1"));
    assert_eq!(results[0]["agent_count"].as_i64(), Some(3));
    assert!(results[0]["links"]["self"]
        .as_str()
        .unwrap()
        .contains("root-sess-1"));

    Ok(())
}

#[test]
fn test_observability_read_tool() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let config = GatewayConfig::default();

    store.upsert_published_session_report(&PublishedSessionReportRecord {
        root_session_id: "root-read-test".to_string(),
        report_handle: "sha256:cafe".to_string(),
        overview_handle: None,
        html_handle: None,
        narrative_handle: None,
        title: "Session report: root-read-test".to_string(),
        status: "completed".to_string(),
        started_at: Some("2026-04-12T08:00:00Z".to_string()),
        ended_at: Some("2026-04-12T09:00:00Z".to_string()),
        agent_count: 1,
        error_count: 2,
        approval_count: 0,
        search_text: "root-read-test debugger".to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        report_version: 1,
    })?;

    let registry = default_registry();
    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    let args = serde_json::json!({
        "uri": "autonoetic://observability/roots/root-read-test/report",
        "view": "full"
    });
    let result = registry.execute(
        "observability_read",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(parsed["ok"].as_bool().unwrap());
    assert_eq!(parsed["resource_type"].as_str(), Some("report"));
    assert_eq!(parsed["body"]["error_count"].as_i64(), Some(2));
    assert!(parsed["links"]["self"]
        .as_str()
        .unwrap()
        .contains("root-read-test"));

    let args_overview = serde_json::json!({
        "uri": "autonoetic://observability/roots/root-read-test/report/overview"
    });
    let result2 = registry.execute(
        "observability_read",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args_overview.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let parsed2: serde_json::Value = serde_json::from_str(&result2)?;
    assert!(parsed2["ok"].as_bool().unwrap());
    assert_eq!(parsed2["resource_type"].as_str(), Some("report_overview"));
    assert_eq!(parsed2["body"]["agent_count"].as_i64(), Some(1));

    let args_missing = serde_json::json!({
        "uri": "autonoetic://observability/roots/nonexistent/report"
    });
    let result3 = registry.execute(
        "observability_read",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args_missing.to_string(),
        None,
        None,
        Some(&config),
        Some(store),
        None,
    )?;
    let parsed3: serde_json::Value = serde_json::from_str(&result3)?;
    assert!(!parsed3["ok"].as_bool().unwrap());

    Ok(())
}

#[test]
fn test_hook_dispatch_publishes_report() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let config = GatewayConfig::default();

    let session_id = "hook-test-session";
    let session_dir = gateway_dir.join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir)?;

    let report_json = serde_json::json!({
        "status": "completed",
        "started_at": "2026-04-12T10:00:00Z",
        "ended_at": "2026-04-12T10:05:00Z",
        "agents": {
            "coder.default": {
                "agent_id": "coder.default",
                "error_count": 0,
                "approval_count": 1,
            }
        }
    });
    std::fs::write(
        session_dir.join("session_report.json"),
        serde_json::to_string(&report_json)?,
    )?;

    let hooks = vec![HookConfig {
        event: HookEvent::SessionClosed,
        action: HookAction::PublishReport,
        r#async: false,
        params: Default::default(),
        callback_allowlist: Vec::new(),
        allowed_agents: Vec::new(),
    }];
    let executor = HookExecutor::new(hooks, Some(store.clone()), 4000, 60);

    let ctx = HookContext::for_session_closed(
        session_id,
        session_id,
        "coder.default",
        "jsonrpc_spawn_complete",
        5,
        Some(&gateway_dir),
    );
    executor.dispatch(&ctx);

    let found = store.find_published_report(session_id)?;
    assert!(found.is_some());
    let report = found.unwrap();
    assert_eq!(report.root_session_id, session_id);
    assert!(report.report_handle.starts_with("sha256:"));
    assert_eq!(report.agent_count, 1);
    assert_eq!(report.approval_count, 1);
    assert_eq!(report.error_count, 0);

    Ok(())
}

#[test]
fn test_observability_search_after_publish() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let config = GatewayConfig::default();

    let session_id = "e2e-search-session";
    let session_dir = gateway_dir.join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir)?;

    let report_json = serde_json::json!({
        "status": "completed",
        "started_at": "2026-04-12T11:00:00Z",
        "agents": {
            "researcher.default": {
                "agent_id": "researcher.default",
                "error_count": 0,
                "approval_count": 0,
            }
        }
    });
    std::fs::write(
        session_dir.join("session_report.json"),
        serde_json::to_string(&report_json)?,
    )?;

    let hooks = vec![HookConfig {
        event: HookEvent::SessionClosed,
        action: HookAction::PublishReport,
        r#async: false,
        params: Default::default(),
        callback_allowlist: Vec::new(),
        allowed_agents: Vec::new(),
    }];
    let executor = HookExecutor::new(hooks, Some(store.clone()), 4000, 60);

    let ctx = HookContext::for_session_closed(
        session_id,
        session_id,
        "researcher.default",
        "jsonrpc_spawn_complete",
        3,
        Some(&gateway_dir),
    );
    executor.dispatch(&ctx);

    let registry = default_registry();
    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    let args = serde_json::json!({"query": "researcher"});
    let result = registry.execute(
        "observability_search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(parsed["ok"].as_bool().unwrap());
    let results = parsed["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["root_session_id"].as_str(), Some(session_id));

    let read_args = serde_json::json!({
        "uri": format!("autonoetic://observability/roots/{}/report", session_id)
    });
    let read_result = registry.execute(
        "observability_read",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &read_args.to_string(),
        None,
        None,
        Some(&config),
        Some(store),
        None,
    )?;
    let read_parsed: serde_json::Value = serde_json::from_str(&read_result)?;
    assert!(read_parsed["ok"].as_bool().unwrap());
    assert_eq!(read_parsed["body"]["status"].as_str(), Some("completed"));

    Ok(())
}
