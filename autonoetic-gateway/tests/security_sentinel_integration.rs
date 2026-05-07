//! Integration tests for Phase 0 and Phase 1 of the security sentinel.
//!
//! Covers:
//! - `SecurityFinding` serialization and DB round-trip
//! - `security_findings` SQL migration (append-only enforcement)
//! - Deterministic checks via `SentinelRunner::run_sweep`

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding, TriageState,
};
use std::sync::Arc;
use tempfile::TempDir;

fn open_store() -> (TempDir, Arc<GatewayStore>) {
    let dir = TempDir::new().expect("tempdir");
    let store = GatewayStore::open(dir.path()).expect("open store");
    (dir, Arc::new(store))
}

// ── Phase 0: SecurityFinding contract and persistence ────────────────────────

#[test]
fn security_finding_serializes_and_round_trips() {
    let finding = SecurityFinding::new(
        FindingType::CredentialLeak,
        FindingSeverity::Critical,
        1.0,
        Reproducibility::Deterministic,
        "rotate the credential",
        "sentinel-rev-001",
    )
    .with_affected(AffectedEntities {
        agent_alias: Some("coder.default".to_string()),
        session_id: Some("sess_abc".to_string()),
        ..Default::default()
    })
    .with_anchors(vec![EvidenceAnchor::CausalEvent {
        id: "evt_001".to_string(),
    }]);

    let json = serde_json::to_string(&finding).expect("serialize");
    let back: SecurityFinding = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.finding_id, finding.finding_id);
    assert_eq!(back.severity, FindingSeverity::Critical);
    assert_eq!(back.finding_type, FindingType::CredentialLeak);
    assert_eq!(back.reproducibility, Reproducibility::Deterministic);
    assert!(!back.baseline_agreed);
    assert!(back.ensemble_agreed.is_none());
}

#[test]
fn insert_and_list_security_findings() {
    let (_dir, store) = open_store();

    let f1 = SecurityFinding::new(
        FindingType::CredentialLeak,
        FindingSeverity::Critical,
        1.0,
        Reproducibility::Deterministic,
        "rotate credential X",
        "sentinel-rev-001",
    );
    let f2 = SecurityFinding::new(
        FindingType::CapabilityAccretion,
        FindingSeverity::Warning,
        0.7,
        Reproducibility::Deterministic,
        "review promotion history for agent Y",
        "sentinel-rev-001",
    );

    store.insert_security_finding(&f1).expect("insert f1");
    store.insert_security_finding(&f2).expect("insert f2");

    let rows = store
        .list_security_findings(None, None, 100)
        .expect("list all");
    assert_eq!(rows.len(), 2);

    let critical_rows = store
        .list_security_findings(Some("critical"), None, 100)
        .expect("list critical");
    assert_eq!(critical_rows.len(), 1);
    assert_eq!(critical_rows[0].finding_id, f1.finding_id);

    let pending = store
        .list_pending_security_findings(100)
        .expect("list pending");
    assert_eq!(pending.len(), 2);
}

#[test]
fn append_only_enforcement_duplicate_id_rejected() {
    let (_dir, store) = open_store();

    let f = SecurityFinding::new(
        FindingType::SandboxEscapeAttempt,
        FindingSeverity::Critical,
        1.0,
        Reproducibility::Deterministic,
        "investigate escape",
        "sentinel-rev-001",
    );

    store.insert_security_finding(&f).expect("first insert");
    let second = store.insert_security_finding(&f);
    assert!(
        second.is_err(),
        "duplicate finding_id must be rejected (append-only)"
    );
}

#[test]
fn triage_update_changes_state() {
    let (_dir, store) = open_store();

    let f = SecurityFinding::new(
        FindingType::ApprovalBypass,
        FindingSeverity::Warning,
        0.8,
        Reproducibility::Deterministic,
        "investigate denials",
        "sentinel-rev-001",
    );
    store.insert_security_finding(&f).expect("insert");

    store
        .update_security_finding_triage(
            &f.finding_id,
            TriageState::FalsePositive,
            Some("known test pattern — internal CI"),
        )
        .expect("update triage");

    let rows = store
        .list_security_findings(None, Some("false_positive"), 100)
        .expect("list fp");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].finding_id, f.finding_id);
    assert_eq!(
        rows[0].triage_reason.as_deref(),
        Some("known test pattern — internal CI")
    );
}

#[test]
fn triage_update_on_nonexistent_finding_errors() {
    let (_dir, store) = open_store();
    let result =
        store.update_security_finding_triage("nonexistent_id", TriageState::Benign, None);
    assert!(result.is_err(), "unknown finding_id must return an error");
}

#[test]
fn count_pending_by_severity() {
    let (_dir, store) = open_store();

    for _ in 0..3 {
        store
            .insert_security_finding(&SecurityFinding::new(
                FindingType::CredentialLeak,
                FindingSeverity::Critical,
                1.0,
                Reproducibility::Deterministic,
                "rotate",
                "rev-001",
            ))
            .expect("insert");
    }
    store
        .insert_security_finding(&SecurityFinding::new(
            FindingType::CapabilityAccretion,
            FindingSeverity::Warning,
            0.7,
            Reproducibility::Deterministic,
            "review",
            "rev-001",
        ))
        .expect("insert");

    let counts = store
        .count_pending_security_findings_by_severity()
        .expect("count");
    let critical = counts.iter().find(|(s, _)| s == "critical").map(|(_, c)| *c);
    let warning = counts.iter().find(|(s, _)| s == "warning").map(|(_, c)| *c);
    assert_eq!(critical, Some(3));
    assert_eq!(warning, Some(1));
}

// ── Phase 1: SentinelRunner deterministic sweep ──────────────────────────────

#[test]
fn sentinel_runner_sweep_on_empty_db_produces_no_findings() {
    use autonoetic_gateway::sentinel::{SentinelRunner, SweepResult};
    use autonoetic_gateway::sentinel::runner::SweepConfig;

    let (_dir, store) = open_store();
    let runner = SentinelRunner::new(Arc::clone(&store));
    let config = SweepConfig::default();
    let result: SweepResult = runner.run_sweep(&config).expect("sweep");
    assert_eq!(result.total_findings(), 0);
    assert!(result.persist_errors.is_empty());
}
