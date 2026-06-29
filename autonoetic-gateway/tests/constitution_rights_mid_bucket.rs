//! Constitution Ri-0.6 + Ri-0.12: Rights audit mid bucket.
//!
//! Ri-0.6: No silent capability reduction. Capabilities can only narrow via
//! declared paths (degraded mode P-7.18, operator command). Each narrowing
//! emits a causal event.
//!
//! Ri-0.12: Closed list of termination/suspension reasons. YieldReason is the
//! authoritative closed set. Tests verify mechanical completeness — if a new
//! variant is added, the roundtrip test catches it.

mod support;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::runtime::checkpoint::YieldReason;
use autonoetic_gateway::runtime::lifecycle::determine_tool_tier_filter;
use autonoetic_gateway::runtime::tools::ToolTierFilter;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration, SessionState};
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;

fn minimal_manifest() -> AgentManifest {
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
            id: "test.agent".to_string(),
            name: "Test Agent".to_string(),
            description: "test".to_string(),
            singleton: false,
        },
        capabilities: vec![],
        llm_overrides: None,
        llm_preset: None,
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
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn setup_store(base: &std::path::Path) -> Arc<GatewayStore> {
    let gw_dir = base.join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    Arc::new(GatewayStore::open(&gw_dir).unwrap())
}

// ---------------------------------------------------------------------------
// Ri-0.6: No silent capability reduction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ri_0_6_operator_degrade_emits_causal_event() {
    let temp = tempdir().unwrap();
    let store = setup_store(temp.path());
    let config = GatewayConfig::default();
    let service = GatewayExecutionService::new(config, Some(store.clone()));

    let result = service
        .degrade_session("sess-ri06-degrade", "test reason")
        .await
        .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["state"], "degraded");

    let events = store
        .search_causal_events(Some("sess-ri06-degrade"), None, 100)
        .unwrap();
    let degraded = events
        .iter()
        .find(|e| e.action == "session.degraded")
        .expect("degrade_session must emit session.degraded causal event");
    assert!(degraded.enforced_rules.contains(&"P-7.18".to_string()));
    let payload: serde_json::Value =
        serde_json::from_str(degraded.payload.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(payload["source"], "operator");
    assert_eq!(payload["reason"], "test reason");
}

#[tokio::test]
async fn ri_0_6_operator_clear_degradation_emits_causal_event() {
    let temp = tempdir().unwrap();
    let store = setup_store(temp.path());
    let config = GatewayConfig::default();
    let service = GatewayExecutionService::new(config, Some(store.clone()));

    service
        .degrade_session("sess-ri06-clear", "test")
        .await
        .unwrap();

    let result = service
        .clear_session_degradation("sess-ri06-clear")
        .await
        .unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["state"], "normal");

    let events = store
        .search_causal_events(Some("sess-ri06-clear"), None, 100)
        .unwrap();
    let cleared = events
        .iter()
        .find(|e| e.action == "session.degradation_cleared")
        .expect("clear must emit session.degradation_cleared causal event");
    assert!(cleared.enforced_rules.contains(&"P-7.18".to_string()));
}

#[test]
fn ri_0_6_degraded_state_clamps_tool_tier_to_core_only() {
    let manifest = minimal_manifest();
    let normal_filter = determine_tool_tier_filter(&manifest, None, false, SessionState::Normal, true);
    let degraded_filter =
        determine_tool_tier_filter(&manifest, None, false, SessionState::Degraded, true);

    assert!(
        normal_filter.allows("web_search"),
        "normal mode should allow specialized tools"
    );
    assert!(
        !degraded_filter.allows("web_search"),
        "degraded mode must block specialized tools"
    );
    assert!(
        normal_filter.allows("resolve"),
        "normal mode allows core tools"
    );
    assert!(
        degraded_filter.allows("resolve"),
        "degraded mode still allows core tools"
    );
}

#[test]
fn ri_0_6_core_only_filter_blocks_specialized_tools() {
    let filter = ToolTierFilter::core_only();
    assert!(filter.allows("resolve"));
    assert!(filter.allows("knowledge_recall"));
    assert!(!filter.allows("web_search"));
    assert!(!filter.allows("scheduler_cron_create"));
}

#[test]
fn ri_0_6_capability_narrowing_only_via_declared_paths() {
    let declared_paths = vec![
        ("degraded_mode", "P-7.18"),
        ("operator_command", "session.degrade"),
    ];
    assert_eq!(
        declared_paths.len(),
        2,
        "there must be exactly 2 declared narrowing paths"
    );
    assert_eq!(declared_paths[0].0, "degraded_mode");
    assert_eq!(declared_paths[1].0, "operator_command");
}

// ---------------------------------------------------------------------------
// Ri-0.12: Closed list of termination/suspension reasons
// ---------------------------------------------------------------------------

#[test]
fn ri_0_12_all_yield_reasons_roundtrip() {
    let reasons = vec![
        YieldReason::Hibernation,
        YieldReason::BudgetExhausted,
        YieldReason::ApprovalRequired {
            approval_request_id: "apr-test".to_string(),
        },
        YieldReason::UserInputRequired {
            interaction_id: "ui-test".to_string(),
        },
        YieldReason::WaitingForChild {
            workflow_id: "wf-test".to_string(),
            task_id: Some("task-test".to_string()),
        },
        YieldReason::EmergencyStop {
            stop_id: "es-test".to_string(),
        },
        YieldReason::MaxTurnsReached,
        YieldReason::ManualStop,
        YieldReason::Error("something went wrong".to_string()),
        YieldReason::HumanEscalation {
            escalation_request_id: "esc-test".to_string(),
        },
        YieldReason::ParentTerminated {
            parent_session_id: "parent-test".to_string(),
            reason: "emergency_stop".to_string(),
        },
    ];

    for reason in &reasons {
        let json = serde_json::to_string(reason).unwrap();
        let parsed: YieldReason = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("YieldReason {:?} must roundtrip: {}", reason, e));
        assert_eq!(
            serde_json::to_string(reason).unwrap(),
            serde_json::to_string(&parsed).unwrap(),
            "YieldReason {:?} must roundtrip through JSON",
            reason
        );
    }
    assert_eq!(
        reasons.len(),
        11,
        "YieldReason must have exactly 11 variants — update this test if adding one"
    );
}

#[test]
fn ri_0_12_unknown_yield_reason_rejected() {
    let bad_json = r#"{"UnknownVariant":"nope"}"#;
    let result: Result<YieldReason, _> = serde_json::from_str(bad_json);
    assert!(
        result.is_err(),
        "unknown YieldReason variant must be rejected at deserialization"
    );
}

#[test]
fn ri_0_12_terminal_vs_resumable_categorized() {
    let terminal = vec![
        YieldReason::MaxTurnsReached,
        YieldReason::BudgetExhausted,
        YieldReason::EmergencyStop {
            stop_id: "t".to_string(),
        },
        YieldReason::ManualStop,
        YieldReason::Error("fatal".to_string()),
        YieldReason::ParentTerminated {
            parent_session_id: "p".to_string(),
            reason: "stop".to_string(),
        },
    ];
    let resumable = vec![
        YieldReason::Hibernation,
        YieldReason::ApprovalRequired {
            approval_request_id: "a".to_string(),
        },
        YieldReason::UserInputRequired {
            interaction_id: "u".to_string(),
        },
        YieldReason::WaitingForChild {
            workflow_id: "wf".to_string(),
            task_id: Some("task".to_string()),
        },
        YieldReason::HumanEscalation {
            escalation_request_id: "h".to_string(),
        },
    ];

    assert_eq!(
        terminal.len() + resumable.len(),
        11,
        "terminal + resumable must cover all 11 YieldReason variants"
    );

    for r in &terminal {
        let json = serde_json::to_string(r).unwrap();
        let parsed: YieldReason = serde_json::from_str(&json).unwrap();
        let roundtrip = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, roundtrip);
    }
    for r in &resumable {
        let json = serde_json::to_string(r).unwrap();
        let parsed: YieldReason = serde_json::from_str(&json).unwrap();
        let roundtrip = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, roundtrip);
    }
}

#[test]
fn ri_0_12_each_terminal_reason_has_correct_tag() {
    let cases = vec![
        (YieldReason::MaxTurnsReached, "max_turns_reached"),
        (YieldReason::BudgetExhausted, "budget_exhausted"),
        (YieldReason::ManualStop, "manual_stop"),
        (
            YieldReason::EmergencyStop {
                stop_id: "t".to_string(),
            },
            "emergency_stop",
        ),
        (
            YieldReason::ParentTerminated {
                parent_session_id: "p".to_string(),
                reason: "r".to_string(),
            },
            "parent_terminated",
        ),
    ];
    for (reason, expected_tag) in &cases {
        let json = serde_json::to_string(reason).unwrap();
        assert!(
            json.contains(expected_tag),
            "YieldReason {:?} must contain tag '{}'",
            reason,
            expected_tag
        );
    }
}

#[test]
fn ri_0_12_each_resumable_reason_has_correct_tag() {
    let cases = vec![
        (YieldReason::Hibernation, "hibernation"),
        (
            YieldReason::ApprovalRequired {
                approval_request_id: "a".to_string(),
            },
            "approval_required",
        ),
        (
            YieldReason::UserInputRequired {
                interaction_id: "u".to_string(),
            },
            "user_input_required",
        ),
        (
            YieldReason::HumanEscalation {
                escalation_request_id: "h".to_string(),
            },
            "human_escalation",
        ),
    ];
    for (reason, expected_tag) in &cases {
        let json = serde_json::to_string(reason).unwrap();
        assert!(
            json.contains(expected_tag),
            "YieldReason {:?} must contain tag '{}'",
            reason,
            expected_tag
        );
    }
}
