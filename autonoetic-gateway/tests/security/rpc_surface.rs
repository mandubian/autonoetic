//! `security.*` RPC service layer (#1119 tranche 2) — the logic behind the
//! JSON-RPC methods `autonoetic security status|findings|triage|patterns|
//! pattern-accept|pattern-reject` now calls, so the CLI stops reading
//! gateway.db directly.
//!
//! Service-level like `tests/session/outcome_rpc.rs`: a second concurrent
//! in-process router initialization races global startup paths (see that
//! file's docstring); the router arms are thin param-decode + delegation.

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::security::{
    AttackPatternStatus, FindingSeverity, FindingType, ProposedAttackPattern, Reproducibility,
    SecurityFinding, TriageState,
};
use std::sync::Arc;

fn service() -> &'static GatewayExecutionService {
    static SERVICE: std::sync::OnceLock<GatewayExecutionService> = std::sync::OnceLock::new();
    SERVICE.get_or_init(|| {
        let ws = tempfile::tempdir().expect("tempdir");
        let config = autonoetic_types::config::GatewayConfig {
            runtime_dir: ws.path().join("agents").join(".gateway"),
            agents_dir: ws.path().join("agents"),
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        // Leak: service + store must outlive the tests sharing this OnceLock.
        std::mem::forget(ws);
        GatewayExecutionService::new(config, Some(store))
    })
}

const REV: &str = "sentinel-rev-rpc";

fn finding(severity: FindingSeverity) -> SecurityFinding {
    SecurityFinding::new(
        FindingType::CredentialLeak,
        severity,
        0.9,
        Reproducibility::Deterministic,
        "rpc-surface test finding",
        REV,
    )
}

fn pattern(id: &str) -> ProposedAttackPattern {
    ProposedAttackPattern {
        pattern_id: id.to_string(),
        proposed_by_agent_id: "auditor.default".to_string(),
        category: "credential_leak".to_string(),
        description: "rpc test pattern".to_string(),
        how_sentinel_should_catch: "scan vault reads".to_string(),
        evidence_anchors_json: "[]".to_string(),
        synthetic_test_case_json: "{}".to_string(),
        status: AttackPatternStatus::Pending,
        accepted_check_type: None,
        operator_notes: None,
        created_at: "2026-08-23T00:00:00+00:00".to_string(),
        reviewed_at: None,
    }
}

#[tokio::test]
async fn security_status_counts_by_severity_and_triage() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let critical = store.insert_security_finding(&finding(FindingSeverity::Critical));
    assert!(critical.is_ok(), "seed failed: {:?}", critical.err());

    let status = svc.security_status().expect("status");
    let sev = status["pending_by_severity"]
        .as_array()
        .expect("severity array")
        .iter()
        .find(|e| e["severity"] == "critical")
        .and_then(|e| e["count"].as_i64())
        .unwrap_or(0);
    assert!(sev >= 1, "expected >=1 pending critical, got {status}");
    assert!(
        status["by_triage_state"].as_array().is_some(),
        "triage counts missing"
    );
}

#[tokio::test]
async fn findings_filter_by_severity_and_roundtrip_fields() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let f = finding(FindingSeverity::Info);
    store.insert_security_finding(&f).expect("insert");

    let rows = svc
        .security_findings(Some("info"), None, Some("pending"), 100)
        .expect("findings");
    let row = rows
        .iter()
        .find(|r| r["finding_id"] == f.finding_id)
        .expect("seeded finding present");
    assert_eq!(row["confidence"].as_f64(), Some(0.9));
    assert_eq!(row["triage_state"].as_str(), Some("pending"));
}

#[tokio::test]
async fn triage_sets_state_then_bulk_marks_the_rest() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let f1 = finding(FindingSeverity::Warning);
    let f2 = finding(FindingSeverity::Warning);
    store.insert_security_finding(&f1).expect("insert f1");
    store.insert_security_finding(&f2).expect("insert f2");

    svc.security_triage_finding(
        &f1.finding_id,
        TriageState::TruePositive,
        Some("confirmed manually"),
    )
    .expect("single triage");

    let bulk = svc
        .security_triage_bulk(TriageState::Benign, "noise", Some("warning"), None)
        .expect("bulk");
    assert!(
        bulk["matched"].as_u64().unwrap_or(0) >= 1,
        "bulk should match pending warnings: {bulk}"
    );
    assert!(bulk["failures"].as_array().map(|a| a.is_empty()).unwrap_or(false));

    // f1 was already true_positive → untouched by the pending-only bulk.
    let rows = svc.security_findings(None, None, None, 100).expect("rows");
    let r1 = rows
        .iter()
        .find(|r| r["finding_id"] == f1.finding_id)
        .expect("f1 present");
    assert_eq!(r1["triage_state"].as_str(), Some("true_positive"));
    assert_eq!(r1["triage_reason"].as_str(), Some("confirmed manually"));
}

#[tokio::test]
async fn patterns_list_and_review_accept_requires_check_type_path() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let p = pattern("pattern-rpc-accept");
    store.insert_attack_pattern(&p).expect("insert pattern");

    let listed = svc.security_patterns(Some("pending"), 100).expect("patterns");
    assert!(
        listed.iter().any(|e| e["pattern_id"] == "pattern-rpc-accept"),
        "pending pattern not listed: {listed:?}"
    );

    svc.security_review_pattern(
        &p.pattern_id,
        AttackPatternStatus::Accepted,
        Some("phase1"),
        Some("looks solid"),
    )
    .expect("review");

    let stored = store.get_attack_pattern(&p.pattern_id).expect("get").expect("present");
    assert_eq!(stored.status, AttackPatternStatus::Accepted);
    assert_eq!(stored.accepted_check_type.as_deref(), Some("phase1"));
    assert_eq!(stored.operator_notes.as_deref(), Some("looks solid"));
}
