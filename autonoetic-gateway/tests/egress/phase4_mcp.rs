//! Phase 4 (#909) slice 3: MCP egress_class + remote tools/call boundary gate.

use std::collections::HashMap;
use std::sync::Arc;

use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::{
    argument_taint_from_prior, mcp_remote_egress_refusal_json, PriorLabeledResult,
};
use autonoetic_gateway::runtime::mcp::McpToolRuntime;
use autonoetic_mcp::{McpServer, McpTransportConfig};
use autonoetic_types::egress::{EgressClass, EgressLabel, Sink};

fn run_ctx(session_id: &str, taint: EgressLabel) -> NativeToolRunContext {
    NativeToolRunContext {
        registry: ActiveExecutionRegistry::new(),
        root_session_id: session_id
            .split('/')
            .next()
            .unwrap_or(session_id)
            .to_string(),
        workflow_id: None,
        task_id: None,
        session_id: session_id.into(),
        agent_id: "coder.default".into(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: None,
        discovered_tools: None,
            annotation_counter: None,
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
        egress_taint: Some(taint),
        egress_query_sink: None,
    }
}

#[test]
fn mcp_server_sse_defaults_to_remote_egress_class() {
    let server = McpServer {
        name: "remote".into(),
        command: String::new(),
        args: vec![],
        transport: McpTransportConfig::Sse {
            url: "https://mcp.example.com/sse".into(),
        },
        egress_class: None,
    };
    assert_eq!(server.resolved_egress_class(), EgressClass::Remote);
    assert!(server.requires_network_egress_gate());
}

#[test]
fn mcp_server_stdio_defaults_to_local_egress_class() {
    let server = McpServer {
        name: "local".into(),
        command: "bash".into(),
        args: vec![],
        transport: McpTransportConfig::Stdio,
        egress_class: None,
    };
    assert_eq!(server.resolved_egress_class(), EgressClass::Local);
    assert!(!server.requires_network_egress_gate());
}

#[test]
fn mcp_remote_refused_under_local_only_session_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
        tmp.path(),
    )?);
    let session_id = "root-mcp/coder";
    let ctx = run_ctx(session_id, EgressLabel::local_only());

    let refusal = mcp_remote_egress_refusal_json(
        "mcp_remote_echo",
        r#"{"text":"hello"}"#,
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "coder.default",
        None,
        &HashMap::new(),
        Some("mcp.example.com"),
    )
    .expect("expected MCP refusal without declassification grant");

    let payload: serde_json::Value = serde_json::from_str(&refusal)?;
    assert_eq!(payload["surface"], "mcp");
    assert_eq!(payload["error_type"], "egress_boundary_refused");

    let events = store.search_causal_events(Some(session_id), None, 10)?;
    assert!(
        events.iter().any(|e| e.action == "egress.boundary_refused"),
        "expected egress.boundary_refused"
    );
    Ok(())
}

#[test]
fn mcp_remote_refused_when_arguments_carry_local_only_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
        tmp.path(),
    )?);
    let session_id = "root-mcp-args/coder";
    let ctx = run_ctx(session_id, EgressLabel::unrestricted());

    let mut prior = HashMap::new();
    prior.insert(
        "tc_prior".to_string(),
        PriorLabeledResult {
            label: EgressLabel::local_only(),
            content_snippet: Some("CANARY-LOCAL-ONLY".into()),
        },
    );
    let args = r#"{"text":"CANARY-LOCAL-ONLY forwarded"}"#;
    let (arg_taint, ids) = argument_taint_from_prior(args, &prior);
    assert!(!arg_taint.allows(Sink::Network));
    assert_eq!(ids, vec!["tc_prior".to_string()]);

    let refusal = mcp_remote_egress_refusal_json(
        "mcp_remote_echo",
        args,
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "coder.default",
        None,
        &prior,
        Some("mcp.example.com"),
    )
    .expect("expected refusal when args inherit local_only taint");

    let payload: serde_json::Value = serde_json::from_str(&refusal)?;
    assert_eq!(payload["surface"], "mcp");
    assert_eq!(payload["parent_envelope_ids"], serde_json::json!(["tc_prior"]));
    Ok(())
}

#[test]
fn unknown_mcp_tool_requires_network_gate_fail_closed() {
    let runtime = McpToolRuntime::empty();
    assert!(runtime.tool_requires_network_egress_gate("mcp_unknown_tool"));
}

#[test]
fn mcp_remote_allowed_after_host_scoped_network_declass_with_clean_args() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(
        tmp.path(),
    )?);
    let session_id = "root-mcp-declass/coder";
    let root = "root-mcp-declass";
    let ctx = run_ctx(session_id, EgressLabel::local_only());
    store.insert_egress_declassification_grant(
        root,
        session_id,
        "coder.default",
        &autonoetic_gateway::runtime::egress_labeler::session_host_network_declass_target(
            root,
            "mcp.example.com",
        ),
        Sink::Network,
        &autonoetic_types::background::GrantScope::RootSession,
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        None,
        None,
    )?;

    // Host-scoped grant covers the declassified server host…
    let refusal = mcp_remote_egress_refusal_json(
        "mcp_remote_echo",
        r#"{"text":"hello"}"#,
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "coder.default",
        None,
        &HashMap::new(),
        Some("mcp.example.com"),
    );
    assert!(
        refusal.is_none(),
        "declassified server host with clean args should allow remote MCP"
    );

    // …but not a different MCP server host.
    let other = mcp_remote_egress_refusal_json(
        "mcp_other_echo",
        r#"{"text":"hello"}"#,
        Some(&ctx),
        Some(&store),
        Some(session_id),
        "coder.default",
        None,
        &HashMap::new(),
        Some("other-mcp.example.com"),
    );
    assert!(
        other.is_some(),
        "host-scoped grant must not widen to other MCP server hosts"
    );
    Ok(())
}
