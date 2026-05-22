use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::improvement_cycle::{
    CycleOutcome, ImprovementCycleRecord, ImprovementLevel,
};

fn seed_cycle(store: &GatewayStore, agent_id: &str, level: ImprovementLevel, outcome: CycleOutcome) {
    store.insert_improvement_cycle(&ImprovementCycleRecord {
        cycle_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        level,
        outcome,
        regression_detected: matches!(outcome, CycleOutcome::Regression),
        operator_decision: "approved".to_string(),
        session_id: None,
        revision_before: Some("rev-before".to_string()),
        revision_after: Some("rev-after".to_string()),
        blast_radius_score: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        closed_at: Some(chrono::Utc::now().to_rfc3339()),
    }).unwrap();
}

#[test]
fn test_insert_and_get_cycle() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();

    let cycle = ImprovementCycleRecord {
        cycle_id: "cycle-001".to_string(),
        agent_id: "planner.default".to_string(),
        level: ImprovementLevel::L1,
        outcome: CycleOutcome::Success,
        regression_detected: false,
        operator_decision: "approved".to_string(),
        session_id: Some("sess-1".to_string()),
        revision_before: Some("rev-old".to_string()),
        revision_after: Some("rev-new".to_string()),
        blast_radius_score: Some(0.1),
        created_at: chrono::Utc::now().to_rfc3339(),
        closed_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    store.insert_improvement_cycle(&cycle).unwrap();

    let got = store.get_improvement_cycle("cycle-001").unwrap().unwrap();
    assert_eq!(got.cycle_id, "cycle-001");
    assert_eq!(got.agent_id, "planner.default");
    assert_eq!(got.level, ImprovementLevel::L1);
    assert_eq!(got.outcome, CycleOutcome::Success);
    assert!(!got.regression_detected);
    assert_eq!(got.blast_radius_score.unwrap(), 0.1);
}

#[test]
fn test_count_successful_cycles() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();

    for _ in 0..5 {
        seed_cycle(&store, "agent-a", ImprovementLevel::L1, CycleOutcome::Success);
    }
    seed_cycle(&store, "agent-a", ImprovementLevel::L1, CycleOutcome::Regression);
    seed_cycle(&store, "agent-b", ImprovementLevel::L1, CycleOutcome::Success);

    assert_eq!(store.count_successful_cycles("agent-a", &ImprovementLevel::L1).unwrap(), 5);
    assert_eq!(store.count_successful_cycles("agent-b", &ImprovementLevel::L1).unwrap(), 1);
    assert_eq!(store.count_successful_cycles("agent-c", &ImprovementLevel::L1).unwrap(), 0);
}

#[test]
fn test_l2_unlock_after_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();

    assert!(!store.check_automation_level_eligibility("agent-a", &ImprovementLevel::L2, 10, 20).unwrap());

    for _ in 0..9 {
        seed_cycle(&store, "agent-a", ImprovementLevel::L1, CycleOutcome::Success);
    }
    assert!(!store.check_automation_level_eligibility("agent-a", &ImprovementLevel::L2, 10, 20).unwrap());

    seed_cycle(&store, "agent-a", ImprovementLevel::L1, CycleOutcome::Success);
    assert!(store.check_automation_level_eligibility("agent-a", &ImprovementLevel::L2, 10, 20).unwrap());
}

#[test]
fn test_l3_unlock_after_l2_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();

    assert!(!store.check_automation_level_eligibility("agent-a", &ImprovementLevel::L3, 10, 20).unwrap());

    for _ in 0..19 {
        seed_cycle(&store, "agent-a", ImprovementLevel::L2, CycleOutcome::Success);
    }
    assert!(!store.check_automation_level_eligibility("agent-a", &ImprovementLevel::L3, 10, 20).unwrap());

    seed_cycle(&store, "agent-a", ImprovementLevel::L2, CycleOutcome::Success);
    assert!(store.check_automation_level_eligibility("agent-a", &ImprovementLevel::L3, 10, 20).unwrap());
}

#[test]
fn test_regression_does_not_count_as_success() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();

    for _ in 0..15 {
        seed_cycle(&store, "agent-a", ImprovementLevel::L1, CycleOutcome::Regression);
    }
    assert_eq!(store.count_successful_cycles("agent-a", &ImprovementLevel::L1).unwrap(), 0);
    assert!(!store.check_automation_level_eligibility("agent-a", &ImprovementLevel::L2, 10, 20).unwrap());
}

#[test]
fn test_close_cycle() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();

    let cycle = ImprovementCycleRecord {
        cycle_id: "cycle-open".to_string(),
        agent_id: "agent-a".to_string(),
        level: ImprovementLevel::L1,
        outcome: CycleOutcome::Cancelled,
        regression_detected: false,
        operator_decision: String::new(),
        session_id: None,
        revision_before: None,
        revision_after: None,
        blast_radius_score: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        closed_at: None,
    };
    store.insert_improvement_cycle(&cycle).unwrap();

    store.close_improvement_cycle("cycle-open", &CycleOutcome::Rejected, false, "operator_declined").unwrap();

    let got = store.get_improvement_cycle("cycle-open").unwrap().unwrap();
    assert_eq!(got.outcome, CycleOutcome::Rejected);
    assert_eq!(got.operator_decision, "operator_declined");
    assert!(got.closed_at.is_some());
}

#[test]
fn test_list_cycles_for_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(tmp.path()).unwrap();

    for _ in 0..3 {
        seed_cycle(&store, "agent-a", ImprovementLevel::L1, CycleOutcome::Success);
    }
    seed_cycle(&store, "agent-a", ImprovementLevel::L2, CycleOutcome::Success);
    seed_cycle(&store, "agent-b", ImprovementLevel::L1, CycleOutcome::Success);

    let all_a = store.list_improvement_cycles_for_agent("agent-a", None, 100).unwrap();
    assert_eq!(all_a.len(), 4);

    let l1_a = store.list_improvement_cycles_for_agent("agent-a", Some(&ImprovementLevel::L1), 100).unwrap();
    assert_eq!(l1_a.len(), 3);

    let l2_a = store.list_improvement_cycles_for_agent("agent-a", Some(&ImprovementLevel::L2), 100).unwrap();
    assert_eq!(l2_a.len(), 1);
}
