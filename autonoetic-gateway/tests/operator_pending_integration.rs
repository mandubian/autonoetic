//! #722 Stage 1 — the unified `operator.pending` view collects and normalizes
//! pending decisions across all four backing stores (approvals,
//! user_interactions, escalations, plan_frames) for one root session.

use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ScheduledAction, UserInteraction, UserInteractionKind,
    UserInteractionStatus,
};
use autonoetic_types::escalation::{EscalationMessage, EscalationStatus, EscalationType};
use autonoetic_types::plan_frame::{PlanFrame, PlanStatus};

use autonoetic_gateway::runtime::operator_pending::{collect_pending_for_root, PendingKind};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

use chrono::{TimeZone, Utc};

const ROOT: &str = "root-abc";

fn store() -> (tempfile::TempDir, GatewayStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(dir.path()).unwrap();
    (dir, store)
}

fn seed_approval(store: &GatewayStore, created_at: &str) {
    let mut app = ApprovalRequest {
        request_id: "apr-1".to_string(),
        agent_id: "researcher.default".to_string(),
        session_id: ROOT.to_string(),
        action: ScheduledAction::WebFetch {
            url: "https://archive.example.org/data".to_string(),
            timeout_secs: None,
            max_chars: None,
            detected_hosts: Some(vec!["archive.example.org".to_string()]),
            payload: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: created_at.to_string(),
        reason: None,
        evidence_ref: None,
        workflow_id: None,
        task_id: None,
        root_session_id: Some(ROOT.to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
    };
    store.create_approval(&mut app).unwrap();
}

fn seed_interaction(store: &GatewayStore, created_at: &str) {
    let interaction = UserInteraction {
        interaction_id: "ui-1".to_string(),
        session_id: ROOT.to_string(),
        root_session_id: ROOT.to_string(),
        workflow_id: None,
        task_id: None,
        agent_id: "planner.default".to_string(),
        turn_id: "turn-1".to_string(),
        kind: UserInteractionKind::Clarification,
        question: "Which region should I target?".to_string(),
        context: None,
        options: vec![],
        allow_freeform: true,
        status: UserInteractionStatus::Pending,
        answer_option_id: None,
        answer_text: None,
        answered_by: None,
        created_at: created_at.to_string(),
        answered_at: None,
        expires_at: None,
        checkpoint_turn_id: None,
    };
    store.create_user_interaction(&interaction).unwrap();
}

fn seed_escalation(store: &GatewayStore, created_at: &str) {
    let esc = EscalationMessage {
        escalation_id: "esc_1".to_string(),
        artifact_id: "art_1".to_string(),
        artifact_digest: None,
        agent_id: "coder.default".to_string(),
        revision_id: "rev-9".to_string(),
        role_verdicts: vec![],
        planner_synthesis: "Promotion review: recommend approve.".to_string(),
        created_at: created_at.to_string(),
        resolved_at: None,
        root_session_id: ROOT.to_string(),
        status: EscalationStatus::Pending,
        decided_by: None,
        decision_reason: None,
        code_excerpts: None,
        escalation_type: EscalationType::default(),
    };
    store.create_escalation(&esc).unwrap();
}

fn seed_plan(store: &GatewayStore, created_at: &str) {
    let plan = PlanFrame {
        plan_id: "plan-1".to_string(),
        version: 1,
        parent_version: None,
        workflow_id: "wf-1".to_string(),
        root_session_id: ROOT.to_string(),
        title: "Ship the migration".to_string(),
        objective: "Migrate the data model".to_string(),
        status: PlanStatus::AwaitingApproval,
        steps: vec![],
        validation_policy: Default::default(),
        capability_envelope: vec![],
        approved_by: None,
        approved_at: None,
        created_by_agent_id: "planner.default".to_string(),
        reason: None,
        created_at: created_at.to_string(),
    };
    store.save_plan_frame(&plan).unwrap();
}

#[test]
fn collects_and_normalizes_all_four_sources() {
    let (_dir, store) = store();
    // Staggered timestamps so oldest-first ordering is deterministic.
    seed_approval(&store, "2026-07-02T10:00:00Z"); // oldest
    seed_interaction(&store, "2026-07-02T10:05:00Z");
    seed_escalation(&store, "2026-07-02T10:10:00Z");
    seed_plan(&store, "2026-07-02T10:15:00Z"); // newest

    let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();
    let pending = collect_pending_for_root(&store, ROOT, now).unwrap();

    assert_eq!(pending.len(), 4, "one entry per source");

    // Oldest-first: approval → interaction → escalation → plan.
    let kinds: Vec<PendingKind> = pending.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        vec![
            PendingKind::Approval,
            PendingKind::Interaction,
            PendingKind::Escalation,
            PendingKind::Plan,
        ]
    );

    // Ages are monotonically decreasing (oldest has the largest age).
    let ages: Vec<i64> = pending.iter().map(|p| p.age_secs.unwrap()).collect();
    assert!(ages.windows(2).all(|w| w[0] >= w[1]), "oldest-first: {ages:?}");
    assert_eq!(ages[0], 2 * 3600, "approval waited two hours");

    // Each carries the correct answer verb.
    let approval = &pending[0];
    assert_eq!(approval.answer.method, "approvals.approve");
    assert_eq!(approval.answer.params["request_id"], "apr-1");
    // Empty reason falls back to the action label from the serde type tag.
    assert_eq!(approval.summary, "web_fetch — approval required");

    let interaction = &pending[1];
    assert_eq!(interaction.answer.method, "interaction.answer");
    assert_eq!(interaction.answer.params["interaction_id"], "ui-1");
    assert_eq!(interaction.summary, "Which region should I target?");

    let escalation = &pending[2];
    assert_eq!(escalation.answer.method, "admin.escalation_resolve");
    assert_eq!(escalation.answer.params["escalation_id"], "esc_1");
    assert_eq!(escalation.summary, "Promotion review: recommend approve.");

    let plan = &pending[3];
    assert_eq!(plan.answer.method, "planframes.approve");
    assert_eq!(plan.answer.params["plan_id"], "plan-1");
    assert_eq!(plan.summary, "Ship the migration");
    assert_eq!(plan.workflow_id.as_deref(), Some("wf-1"));
}

#[test]
fn escalation_fallback_uses_type_not_hardcoded_promotion() {
    let (_dir, store) = store();
    let esc = EscalationMessage {
        escalation_id: "esc_2".to_string(),
        artifact_id: "art_2".to_string(),
        artifact_digest: None,
        agent_id: "coder.default".to_string(),
        revision_id: "rev-9".to_string(),
        role_verdicts: vec![],
        planner_synthesis: "   ".to_string(), // empty after trim
        created_at: "2026-07-02T10:00:00Z".to_string(),
        resolved_at: None,
        root_session_id: ROOT.to_string(),
        status: EscalationStatus::Pending,
        decided_by: None,
        decision_reason: None,
        code_excerpts: None,
        escalation_type: EscalationType::SealedEvalInquiry,
    };
    store.create_escalation(&esc).unwrap();

    let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();
    let pending = collect_pending_for_root(&store, ROOT, now).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0].summary.contains("sealed eval inquiry"),
        "empty synthesis should fall back to the escalation type, got: {}",
        pending[0].summary
    );
    assert!(
        !pending[0].summary.contains("promotion review"),
        "fallback must not hard-code promotion review for non-promotion escalations"
    );
}

#[test]
fn scopes_to_the_requested_root() {
    let (_dir, store) = store();
    seed_approval(&store, "2026-07-02T10:00:00Z");
    let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();

    // A different root sees nothing.
    let other = collect_pending_for_root(&store, "root-other", now).unwrap();
    assert!(other.is_empty(), "approval belongs to a different root");

    // The owning root sees it.
    let mine = collect_pending_for_root(&store, ROOT, now).unwrap();
    assert_eq!(mine.len(), 1);
}

#[test]
fn resolved_items_drop_out_of_the_queue() {
    let (_dir, store) = store();
    seed_approval(&store, "2026-07-02T10:00:00Z");
    store
        .record_decision("apr-1", "approved", "operator", "2026-07-02T11:00:00Z", None)
        .unwrap();

    let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();
    let pending = collect_pending_for_root(&store, ROOT, now).unwrap();
    assert!(pending.is_empty(), "an approved approval is no longer pending");
}
