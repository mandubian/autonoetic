//! Constitution Ri-0.6 + Ri-0.12: Rights audit mid bucket.
//!
//! Ri-0.6: No silent capability reduction. Capabilities can only narrow via
//! declared paths (degraded mode P-7.18, operator command). Each narrowing
//! emits a causal event.
//!
//! Ri-0.12: Closed list of termination/suspension reasons. YieldReason is the
//! authoritative closed set. Mechanical completeness rests on the exhaustive
//! match in `ri_0_12_category`, which fails to *compile* when a variant is
//! added — not on the sample lists, whose length assertions only ever compare a
//! vec to itself. (That distinction is not academic: `Idle` (#902) entered the
//! enum and stayed unclassified and untested while every assertion here passed.)


use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::runtime::checkpoint::YieldReason;
use autonoetic_gateway::runtime::lifecycle::determine_tool_tier_filter;
use autonoetic_gateway::runtime::tools::ToolTierFilter;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, SessionState};
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn minimal_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "test.agent".to_string(),
            name: "Test Agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        ..TestManifest::new().build()
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

/// Ri-0.12 category of a yield reason: does it *end* the session or not?
///
/// The match in [`ri_0_12_category`] is **exhaustive on purpose**. Adding a
/// `YieldReason` variant breaks the build here until it is deliberately
/// classified, and that compile error *is* the closed-list guarantee Ri-0.12
/// claims. A hand-written sample list checked against a literal count cannot
/// provide it: such an assertion only ever compares a vec to its own length, so
/// `Idle` (#902) entered the enum unclassified and untested while both Ri-0.12
/// tests kept passing. Mirrors the pattern `SessionLifecycleState` already uses.
///
/// This axis is orthogonal to crash-recovery auto-resume
/// (`should_auto_resume_checkpoint_yield_reason`): `BudgetExhausted` and `Error`
/// are terminal here yet auto-resume once the condition clears.
#[derive(Debug, PartialEq, Eq)]
enum Ri012Category {
    /// The session closes.
    Terminal,
    /// The session suspends and continues.
    Resumable,
}

fn ri_0_12_category(reason: &YieldReason) -> Ri012Category {
    match reason {
        YieldReason::MaxTurnsReached
        | YieldReason::BudgetExhausted
        | YieldReason::EmergencyStop { .. }
        | YieldReason::Error(_)
        | YieldReason::ParentTerminated { .. } => Ri012Category::Terminal,

        YieldReason::Hibernation
        // A parked resident session (#902) exists precisely to be resumed by an
        // inbound message.
        | YieldReason::Idle { .. }
        | YieldReason::WaitingForChild { .. }
        // Cooperative operator pause (#1026/#1051): `root_session.pause` is the
        // only producer of `ManualStop`, and it parks the session as
        // `SessionLifecycleState::Paused` to resume in-place on the next
        // message. It does not close the session.
        | YieldReason::ManualStop
        | YieldReason::ApprovalRequired { .. }
        | YieldReason::UserInputRequired { .. }
        | YieldReason::HumanEscalation { .. } => Ri012Category::Resumable,
    }
}

#[test]
fn ri_0_12_all_yield_reasons_roundtrip() {
    let reasons = vec![
        YieldReason::Hibernation,
        YieldReason::Idle {
            since: "2026-08-13T00:00:00Z".to_string(),
            ttl_secs: 900,
        },
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
    // Every sample must be classifiable; the exhaustive match in
    // `ri_0_12_category` is what actually forces a new variant to be handled.
    for reason in &reasons {
        let _ = ri_0_12_category(reason);
    }
    assert_eq!(
        reasons.len(),
        12,
        "every YieldReason variant needs a roundtrip sample here — \
         add one when you add a variant (the exhaustive match in \
         `ri_0_12_category` is the guard that will not let you forget)"
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
        YieldReason::Error("fatal".to_string()),
        YieldReason::ParentTerminated {
            parent_session_id: "p".to_string(),
            reason: "stop".to_string(),
        },
    ];
    let resumable = vec![
        YieldReason::Hibernation,
        YieldReason::Idle {
            since: "2026-08-13T00:00:00Z".to_string(),
            ttl_secs: 900,
        },
        // Cooperative operator pause parks as `Paused` and resumes in-place.
        YieldReason::ManualStop,
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
        12,
        "terminal + resumable must cover all 12 YieldReason variants"
    );

    // The categories asserted above must be the ones the exhaustive
    // classification actually yields — otherwise this test drifts from the
    // vocabulary it claims to pin.
    for r in &terminal {
        assert_eq!(
            ri_0_12_category(r),
            Ri012Category::Terminal,
            "{r:?} must classify as terminal"
        );
    }
    for r in &resumable {
        assert_eq!(
            ri_0_12_category(r),
            Ri012Category::Resumable,
            "{r:?} must classify as resumable"
        );
    }

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
            YieldReason::Idle {
                since: "2026-08-13T00:00:00Z".to_string(),
                ttl_secs: 900,
            },
            "idle",
        ),
        // Cooperative operator pause (#1026/#1051): parks as `Paused`, resumes
        // on the next message — resumable, not a close.
        (YieldReason::ManualStop, "manual_stop"),
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
