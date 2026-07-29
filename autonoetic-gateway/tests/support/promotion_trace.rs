//! Helpers for seeding execution traces in promotion gate tests (#580).

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::causal_chain::ExecutionTraceRecord;

pub fn execution_trace(
    session_id: &str,
    trace_id: &str,
    exit_code: i32,
) -> ExecutionTraceRecord {
    ExecutionTraceRecord {
        trace_id: trace_id.to_string(),
        event_id: None,
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "artifact_exec".to_string(),
        command: Some("pytest".to_string()),
        exit_code: Some(exit_code),
        stdout: Some(if exit_code == 0 {
            "ok".to_string()
        } else {
            "fail".to_string()
        }),
        stderr: None,
        duration_ms: 1,
        success: if exit_code == 0 { 1 } else { 0 },
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
        egress_label: None,
    }
}

pub fn seed_execution_trace(
    store: &GatewayStore,
    session_id: &str,
    trace_id: &str,
    exit_code: i32,
) {
    store
        .create_execution_trace(&execution_trace(session_id, trace_id, exit_code))
        .expect("create_execution_trace");
}

pub fn seed_success_trace(store: &GatewayStore, session_id: &str, trace_id: &str) {
    seed_execution_trace(store, session_id, trace_id, 0);
}

pub const DEFAULT_TRACE_ID: &str = "trace-promotion-test-001";

pub fn seed_smoke_test_task(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
    agent_id: &str,
    revision_id: &str,
) -> (String, String) {
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
    let workflow = autonoetic_gateway::scheduler::workflow_store::ensure_workflow_for_root_session(
        config,
        Some(store),
        "root-smoke-promotion-test",
        None,
    )
    .expect("workflow");
    let mut run = workflow.clone();
    run.status = WorkflowRunStatus::Active;
    autonoetic_gateway::scheduler::workflow_store::save_workflow_run(config, Some(store), &run)
        .expect("save workflow");
    let task_id = format!("smoke-{revision_id}");
    let task = TaskRun {
        task_id: task_id.clone(),
        workflow_id: workflow.workflow_id.clone(),
        agent_id: agent_id.to_string(),
        session_id: format!("{}/{}-smoke", workflow.root_session_id, agent_id),
        parent_session_id: workflow.root_session_id.clone(),
        status: TaskRunStatus::Succeeded,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("agent-factory.default".to_string()),
        result_summary: None,
        join_group: None,
        message: None,
        metadata: Some(serde_json::json!({
            "_autonoetic_spawn_revision_id": revision_id,
        })),
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    };
    autonoetic_gateway::scheduler::workflow_store::save_task_run(config, Some(store), &task)
        .expect("save task");
    (workflow.workflow_id, task_id)
}

pub fn build_promotion_record_args(
    gw_store: &GatewayStore,
    artifact_id: &str,
    role: &str,
    pass: bool,
    session_id: &str,
) -> serde_json::Value {
    let mut args = serde_json::json!({
        "artifact_id": artifact_id,
        "role": role,
        "findings": [],
        "summary": format!("{role} check — pass={pass}"),
    });
    if autonoetic_gateway::runtime::promotion_evidence::role_requires_execution_trace_str(role) {
        let trace_id = format!(
            "trace-{artifact_id}-{role}-{}",
            session_id.replace(['/', ' '], "-")
        );
        seed_execution_trace(gw_store, session_id, &trace_id, if pass { 0 } else { 1 });
        args["execution_trace_id"] = serde_json::json!(trace_id);
    } else {
        args["pass"] = serde_json::json!(pass);
    }
    args
}

pub fn seed_promotion_store_execution_role(
    store: &autonoetic_gateway::runtime::promotion_store::PromotionStore,
    gw_store: &GatewayStore,
    artifact_id: &str,
    role: autonoetic_types::promotion::PromotionRole,
    agent_id: &str,
    pass: bool,
    session_id: &str,
    content_digest: Option<&str>,
) {
    let trace_id =
        if autonoetic_gateway::runtime::promotion_evidence::role_requires_execution_trace(&role) {
            let trace_id = format!(
                "trace-{artifact_id}-{}-{}",
                role.as_str(),
                session_id.replace(['/', ' '], "-")
            );
            seed_execution_trace(gw_store, session_id, &trace_id, if pass { 0 } else { 1 });
            Some(trace_id)
        } else {
            None
        };
    store
        .record_promotion(
            artifact_id.to_string(),
            None,
            content_digest.map(|s| s.to_string()),
            role,
            agent_id,
            pass,
            vec![],
            None,
            trace_id,
        )
        .expect("record_promotion");
}
