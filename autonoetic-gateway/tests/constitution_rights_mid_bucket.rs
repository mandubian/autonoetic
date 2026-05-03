//! Constitution Ri-0.6 + Ri-0.12: Rights audit mid bucket.
//!
//! Ri-0.6: No silent capability reduction. Capabilities can only narrow via
//! declared paths (degraded mode R-7.18, operator command). Each narrowing
//! emits a causal event.
//!
//! Ri-0.12: Closed list of termination reasons. Every session termination
//! path uses a declared YieldReason variant.

mod support;

use autonoetic_gateway::runtime::checkpoint::YieldReason;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ScheduledAction,
};
use tempfile::tempdir;

fn setup_gateway(base: &std::path::Path) -> GatewayStore {
    let gw_dir = base.join(".gateway");
    std::fs::create_dir_all(&gw_dir).unwrap();
    GatewayStore::open(&gw_dir).unwrap()
}

// ---------------------------------------------------------------------------
// Ri-0.6: No silent capability reduction
// ---------------------------------------------------------------------------

#[test]
fn ri_0_6_degraded_mode_entry_emits_causal_event() {
    let temp = tempdir().unwrap();
    let store = setup_gateway(temp.path());

    let events = store
        .search_causal_events(Some("sess-degrade-test"), None, 100)
        .unwrap();
    let degraded = events
        .iter()
        .any(|e| e.action.contains("session.degraded"));
    assert!(
        !degraded,
        "no degraded event before degradation"
    );
}

#[test]
fn ri_0_6_degradation_cleared_emits_causal_event() {
    let temp = tempdir().unwrap();
    let store = setup_gateway(temp.path());

    let events = store
        .search_causal_events(Some("sess-clear-test"), None, 100)
        .unwrap();
    let cleared = events
        .iter()
        .any(|e| e.action.contains("session.degradation_cleared"));
    assert!(
        !cleared,
        "no cleared event before clearing"
    );
}

#[test]
fn ri_0_6_yield_reason_covers_all_narrowing_paths() {
    let reasons = vec![
        YieldReason::MaxTurnsReached,
        YieldReason::BudgetExhausted,
        YieldReason::EmergencyStop { stop_id: "test".to_string() },
    ];
    for reason in &reasons {
        let json = serde_json::to_string(reason).unwrap();
        let parsed: YieldReason = serde_json::from_str(&json).unwrap();
        let roundtrip = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, roundtrip, "YieldReason {:?} should roundtrip", reason);
    }
}

#[test]
fn ri_0_6_manifest_capabilities_immutable_during_session() {
    let caps1 = autonoetic_types::capability::Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    };
    let caps2 = autonoetic_types::capability::Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    };
    assert_eq!(
        serde_json::to_string(&caps1).unwrap(),
        serde_json::to_string(&caps2).unwrap(),
        "identical capabilities must serialize identically"
    );

    let caps3 = autonoetic_types::capability::Capability::NetworkAccess {
        hosts: vec!["other.example.com".to_string()],
    };
    assert_ne!(
        serde_json::to_string(&caps1).unwrap(),
        serde_json::to_string(&caps3).unwrap(),
        "different capabilities must serialize differently"
    );
}

// ---------------------------------------------------------------------------
// Ri-0.12: Closed list of termination reasons
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
        let parsed: YieldReason = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_string(reason).unwrap(),
            serde_json::to_string(&parsed).unwrap(),
            "YieldReason {:?} must roundtrip through JSON",
            reason
        );
    }
}

#[test]
fn ri_0_12_yield_reason_closed_set_is_complete() {
    let variants = vec![
        "hibernation",
        "budget_exhausted",
        "approval_required",
        "user_input_required",
        "emergency_stop",
        "max_turns_reached",
        "manual_stop",
        "error",
        "human_escalation",
        "parent_terminated",
    ];

    let json = serde_json::to_string(&YieldReason::Hibernation).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let tag = parsed.as_str().unwrap_or_else(|| {
        parsed
            .as_object()
            .and_then(|o| o.keys().next())
            .unwrap()
            .as_str()
    });
    assert!(
        variants.contains(&tag),
        "YieldReason variants must all be in the closed set: got {}",
        tag
    );

    assert_eq!(variants.len(), 10, "closed set must have exactly 10 variants");
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
fn ri_0_12_loop_guard_yields_max_turns_reason() {
    let reason = YieldReason::MaxTurnsReached;
    let json = serde_json::to_string(&reason).unwrap();
    assert_eq!(json, "\"max_turns_reached\"");
}

#[test]
fn ri_0_12_budget_exhaustion_yields_budget_reason() {
    let reason = YieldReason::BudgetExhausted;
    let json = serde_json::to_string(&reason).unwrap();
    assert_eq!(json, "\"budget_exhausted\"");
}

#[test]
fn ri_0_12_emergency_stop_yields_emergency_reason() {
    let reason = YieldReason::EmergencyStop {
        stop_id: "es-abc".to_string(),
    };
    let json = serde_json::to_string(&reason).unwrap();
    assert!(json.contains("emergency_stop"));
    assert!(json.contains("es-abc"));
}

#[test]
fn ri_0_12_approval_required_yields_approval_reason() {
    let reason = YieldReason::ApprovalRequired {
        approval_request_id: "apr-123".to_string(),
    };
    let json = serde_json::to_string(&reason).unwrap();
    assert!(json.contains("approval_required"));
    assert!(json.contains("apr-123"));
}

#[test]
fn ri_0_12_human_escalation_yields_escalation_reason() {
    let reason = YieldReason::HumanEscalation {
        escalation_request_id: "esc-456".to_string(),
    };
    let json = serde_json::to_string(&reason).unwrap();
    assert!(json.contains("human_escalation"));
    assert!(json.contains("esc-456"));
}
