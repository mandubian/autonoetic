//! `workflow_state` reuse guards must be sourced from promotion-store verdicts,
//! not merely artifact presence (issue #765).
//!
//! A federation role can complete and leave an artifact behind without ever
//! calling `promotion.record`. The old artifact-presence guards would declare the
//! gate satisfied and emit a `federation_complete` resume hint, even though the
//! required verdict is missing. This test verifies that:
//! - `has_static_evaluator_result` is `false` when no static_evaluator verdict is recorded.
//! - `has_auditor_result` is `true` when the auditor verdict is recorded.
//! - `resume_hint` is NOT `federation_complete` while a required verdict is missing.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::promotion_store::PromotionStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, save_task_run, save_workflow_run,
};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::artifact::{ArtifactKind, ArtifactRefRecord, ArtifactRefScopeType};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::promotion::{Finding, PromotionRole};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn read_access_manifest() -> AgentManifest {
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
            id: "planner.default".to_string(),
            name: "planner.default".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::ReadAccess {
            scopes: vec!["workflow".to_string()],
        }],
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        }
}

fn build_agent_bundle_artifact(base_dir: &Path) -> (String, std::path::PathBuf) {
    let gateway_dir = base_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store =
        autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir).unwrap();
    let session_id = "root-test";

    let skill_md = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "test.agent"
  name: "Test Agent"
  description: "Test agent"
capabilities: []
execution_mode: script
script_entry: main.py
---
# Test Agent
"#;

    let runtime_lock = r#"gateway:
  artifact: autonoetic-gateway
  version: "0.1.0"
  sha256: unmanaged
  signature: null
sdk:
  version: "0.1.0"
sandbox:
  backend: bubblewrap
dependencies: []
artifacts: []
layers: []
"#;

    let main_py = "print('hello')";

    for (path, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", main_py.as_bytes()),
    ] {
        let handle = content_store.write(content).unwrap();
        content_store.register_name(session_id, path, &handle).unwrap();
    }

    let bundle = artifact_store
        .build_with_kind(
            &[
                "SKILL.md".to_string(),
                "runtime.lock".to_string(),
                "main.py".to_string(),
            ],
            Some(&["main.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();
    (bundle.artifact_id, gateway_dir)
}

fn make_task(
    workflow_id: &str,
    root_session: &str,
    task_id: &str,
    agent_id: &str,
    child_session_suffix: &str,
    status: TaskRunStatus,
) -> TaskRun {
    TaskRun {
        task_id: task_id.to_string(),
        workflow_id: workflow_id.to_string(),
        agent_id: agent_id.to_string(),
        session_id: format!("{}/{}", root_session, child_session_suffix),
        parent_session_id: root_session.to_string(),
        status,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        source_agent_id: Some("planner.default".to_string()),
        result_summary: Some("done".to_string()),
        join_group: None,
        message: None,
        metadata: None,
        retry_count: 0,
        last_failure_class: None,
        retry_policy: None,
        side_effect_state: None,
        dedupe_key: None,
    }
}

#[test]
fn workflow_state_reuses_promotion_store_not_artifact_presence() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let planner_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&planner_dir)?;

    let (artifact_id, gateway_dir) = build_agent_bundle_artifact(temp.path());
    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let gw_store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let content_store = ContentStore::new(&gateway_dir)?;
    let artifact_store = autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir)?;
    let bundle = artifact_store.inspect(&artifact_id)?;

    let root_session = "root-issue-765";

    // Create the workflow.
    let run = ensure_workflow_for_root_session(
        &config,
        Some(gw_store.as_ref()),
        root_session,
        Some("planner.default"),
    )?;
    let workflow_id = run.workflow_id;

    // Register a short artifact_ref in workflow scope so the coder's implicit
    // artifact can be resolved back to the canonical artifact_id.
    let artifact_ref = "ar.8e6cc98b2607";
    gw_store.create_artifact_ref(&ArtifactRefRecord {
        ref_id: artifact_ref.to_string(),
        scope_type: ArtifactRefScopeType::Workflow,
        scope_id: workflow_id.clone(),
        artifact_id: artifact_id.clone(),
        artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
        artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
        created_by_agent_id: "coder.default".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        expires_at: None,
        revoked_at: None,
    })?;

    // Coder task succeeded and produced an artifact.
    let coder_task_id = "task-coder";
    let coder_task = make_task(
        &workflow_id,
        root_session,
        coder_task_id,
        "coder.default",
        "coder-child",
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(gw_store.as_ref()), &coder_task)?;

    // Create the coder's implicit artifact, referencing the built artifact.
    let implicit_data = serde_json::json!({
        "implicit_artifact_id": format!("impl_{}", coder_task_id),
        "artifact_type": "implicit",
        "task_id": coder_task_id,
        "agent_id": "coder.default",
        "session_id": format!("{}/coder-child", root_session),
        "parent_session": root_session,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "summary": "Coder completed",
        "content": {
            "named_outputs": [],
            "artifacts": [
                {
                    "artifact_ref": artifact_ref,
                    "artifact_canonical_digest": bundle.artifact_canonical_digest,
                    "kind": "AgentBundle",
                    "entrypoints": ["main.py"],
                    "file_count": bundle.files.len(),
                    "created_at": bundle.created_at,
                }
            ]
        }
    });
    let handle = content_store.write(&serde_json::to_vec(&implicit_data)?)?;
    content_store.register_name(
        root_session,
        &format!("impl_{}", coder_task_id),
        &handle,
    )?;

    // Auditor task succeeded and *did* record a verdict.
    let auditor_task = make_task(
        &workflow_id,
        root_session,
        "task-auditor",
        "auditor.default",
        "auditor-child",
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(gw_store.as_ref()), &auditor_task)?;

    // Static evaluator task succeeded but did *not* record a verdict.
    let static_eval_task = make_task(
        &workflow_id,
        root_session,
        "task-static-eval",
        "static_evaluator.default",
        "static-eval-child",
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(gw_store.as_ref()), &static_eval_task)?;

    // Record only the auditor verdict.
    let promotion_store = PromotionStore::new(&gateway_dir)?;
    promotion_store.record_promotion(
        artifact_id.clone(),
        Some(bundle.artifact_manifest_digest),
        Some(bundle.artifact_canonical_digest),
        PromotionRole::Auditor,
        "auditor.default",
        true,
        vec![Finding {
            severity: autonoetic_types::promotion::FindingSeverity::Info,
            description: "audit passed".to_string(),
            evidence: None,
        }],
        Some("audit passed".to_string()),
        None,
    )?;

    // Force workflow status to a non-terminal state so workflow_state can be queried.
    let mut workflow = autonoetic_gateway::scheduler::workflow_store::load_workflow_run(
        &config,
        Some(gw_store.as_ref()),
        &workflow_id,
    )?
    .expect("workflow should exist");
    workflow.status = WorkflowRunStatus::WaitingChildren;
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(&config, Some(gw_store.as_ref()), &workflow)?;

    // Now invoke workflow_state.
    let manifest = read_access_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let args = serde_json::json!({
        "workflow_id": workflow_id,
    });

    let result = registry.execute(
        "workflow_state",
        &manifest,
        &policy,
        &planner_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session),
        Some("turn-765"),
        Some(&config),
        Some(gw_store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    let guards = parsed
        .get("reuse_guards")
        .expect("reuse_guards should be present");

    assert_eq!(
        guards.get("has_auditor_result").and_then(|v| v.as_bool()),
        Some(true),
        "auditor verdict is recorded, so has_auditor_result should be true"
    );
    assert_eq!(
        guards.get("has_static_evaluator_result").and_then(|v| v.as_bool()),
        Some(false),
        "static_evaluator artifact exists but no verdict recorded, so guard should be false"
    );
    assert_eq!(
        guards.get("has_unit_test_runner_result").and_then(|v| v.as_bool()),
        Some(false),
        "no unit_test_runner verdict recorded"
    );
    assert_eq!(
        guards.get("has_evaluator_result").and_then(|v| v.as_bool()),
        Some(false),
        "no evaluator verdict recorded"
    );
    assert_eq!(
        guards.get("has_sealed_evaluator_result").and_then(|v| v.as_bool()),
        Some(false),
        "no sealed_evaluator verdict recorded"
    );
    assert_eq!(
        guards.get("primary_artifact_id").and_then(|v| v.as_str()),
        Some(artifact_id.as_str()),
        "primary_artifact_id should resolve to the coder's artifact"
    );

    let resume_hint = parsed
        .get("resume_hint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        !resume_hint.starts_with("federation_complete"),
        "workflow should not declare federation_complete while static_evaluator verdict is missing; got: {}",
        resume_hint
    );

    Ok(())
}

#[test]
fn workflow_state_declares_federation_complete_when_all_verdicts_recorded() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let planner_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&planner_dir)?;

    let (artifact_id, gateway_dir) = build_agent_bundle_artifact(temp.path());
    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let gw_store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let content_store = ContentStore::new(&gateway_dir)?;
    let artifact_store = autonoetic_gateway::artifact_store::ArtifactStore::new(&gateway_dir)?;
    let bundle = artifact_store.inspect(&artifact_id)?;

    let root_session = "root-issue-765-complete";

    let run = ensure_workflow_for_root_session(
        &config,
        Some(gw_store.as_ref()),
        root_session,
        Some("planner.default"),
    )?;
    let workflow_id = run.workflow_id;

    let artifact_ref = "ar.8e6cc98b2607";
    gw_store.create_artifact_ref(&ArtifactRefRecord {
        ref_id: artifact_ref.to_string(),
        scope_type: ArtifactRefScopeType::Workflow,
        scope_id: workflow_id.clone(),
        artifact_id: artifact_id.clone(),
        artifact_manifest_digest: bundle.artifact_manifest_digest.clone(),
        artifact_canonical_digest: bundle.artifact_canonical_digest.clone(),
        created_by_agent_id: "coder.default".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        expires_at: None,
        revoked_at: None,
    })?;

    let coder_task_id = "task-coder";
    let coder_task = make_task(
        &workflow_id,
        root_session,
        coder_task_id,
        "coder.default",
        "coder-child",
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(gw_store.as_ref()), &coder_task)?;

    let implicit_data = serde_json::json!({
        "implicit_artifact_id": format!("impl_{}", coder_task_id),
        "artifact_type": "implicit",
        "task_id": coder_task_id,
        "agent_id": "coder.default",
        "session_id": format!("{}/coder-child", root_session),
        "parent_session": root_session,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "summary": "Coder completed",
        "content": {
            "named_outputs": [],
            "artifacts": [
                {
                    "artifact_ref": artifact_ref,
                    "artifact_canonical_digest": bundle.artifact_canonical_digest,
                    "kind": "AgentBundle",
                    "entrypoints": ["main.py"],
                    "file_count": bundle.files.len(),
                    "created_at": bundle.created_at,
                }
            ]
        }
    });
    let handle = content_store.write(&serde_json::to_vec(&implicit_data)?)?;
    content_store.register_name(
        root_session,
        &format!("impl_{}", coder_task_id),
        &handle,
    )?;

    let auditor_task = make_task(
        &workflow_id,
        root_session,
        "task-auditor",
        "auditor.default",
        "auditor-child",
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(gw_store.as_ref()), &auditor_task)?;

    let static_eval_task = make_task(
        &workflow_id,
        root_session,
        "task-static-eval",
        "static_evaluator.default",
        "static-eval-child",
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(gw_store.as_ref()), &static_eval_task)?;

    let promotion_store = PromotionStore::new(&gateway_dir)?;
    for (role, agent_id) in [
        (PromotionRole::Auditor, "auditor.default"),
        (PromotionRole::StaticEvaluator, "static_evaluator.default"),
    ] {
        promotion_store.record_promotion(
            artifact_id.clone(),
            Some(bundle.artifact_manifest_digest.clone()),
            Some(bundle.artifact_canonical_digest.clone()),
            role,
            agent_id,
            true,
            vec![Finding {
                severity: autonoetic_types::promotion::FindingSeverity::Info,
                description: "passed".to_string(),
                evidence: None,
            }],
            Some("passed".to_string()),
            None,
        )?;
    }

    let mut workflow = autonoetic_gateway::scheduler::workflow_store::load_workflow_run(
        &config,
        Some(gw_store.as_ref()),
        &workflow_id,
    )?
    .expect("workflow should exist");
    workflow.status = WorkflowRunStatus::WaitingChildren;
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(&config, Some(gw_store.as_ref()), &workflow)?;

    let manifest = read_access_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let args = serde_json::json!({
        "workflow_id": workflow_id,
    });

    let result = registry.execute(
        "workflow_state",
        &manifest,
        &policy,
        &planner_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session),
        Some("turn-765-complete"),
        Some(&config),
        Some(gw_store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    let guards = parsed
        .get("reuse_guards")
        .expect("reuse_guards should be present");

    assert_eq!(
        guards.get("has_auditor_result").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        guards.get("has_static_evaluator_result").and_then(|v| v.as_bool()),
        Some(true)
    );

    let resume_hint = parsed
        .get("resume_hint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        resume_hint.starts_with("federation_complete"),
        "workflow should declare federation_complete when both auditor and static_evaluator verdicts are recorded; got: {}",
        resume_hint
    );

    Ok(())
}

#[test]
fn workflow_state_decision_predicate_strict_when_promotion_record_missing() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let planner_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&planner_dir)?;

    let (_artifact_id, gateway_dir) = build_agent_bundle_artifact(temp.path());
    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    let gw_store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let root_session = "root-issue-765-fallback";

    let run = ensure_workflow_for_root_session(
        &config,
        Some(gw_store.as_ref()),
        root_session,
        Some("planner.default"),
    )?;
    let workflow_id = run.workflow_id;

    // Static evaluator succeeded, but no builder candidate exists and no coder
    // task produced an implicit artifact, so the primary artifact cannot be
    // resolved and no promotion record is available. The reuse_guard falls back
    // to artifact presence, but the federation completion decision must remain
    // strict and require a recorded verdict.
    let static_eval_task = make_task(
        &workflow_id,
        root_session,
        "task-static-eval",
        "static_evaluator.default",
        "static-eval-child",
        TaskRunStatus::Succeeded,
    );
    save_task_run(&config, Some(gw_store.as_ref()), &static_eval_task)?;

    let mut workflow = autonoetic_gateway::scheduler::workflow_store::load_workflow_run(
        &config,
        Some(gw_store.as_ref()),
        &workflow_id,
    )?
    .expect("workflow should exist");
    workflow.status = WorkflowRunStatus::WaitingChildren;
    workflow.updated_at = chrono::Utc::now().to_rfc3339();
    save_workflow_run(&config, Some(gw_store.as_ref()), &workflow)?;

    let manifest = read_access_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let args = serde_json::json!({
        "workflow_id": workflow_id,
    });

    let result = registry.execute(
        "workflow_state",
        &manifest,
        &policy,
        &planner_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session),
        Some("turn-765-fallback"),
        Some(&config),
        Some(gw_store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    let guards = parsed
        .get("reuse_guards")
        .expect("reuse_guards should be present");

    assert_eq!(
        guards.get("has_static_evaluator_result").and_then(|v| v.as_bool()),
        Some(true),
        "when no promotion record is resolvable, the guard falls back to artifact presence"
    );
    assert!(
        guards.get("primary_artifact_id").is_none()
            || guards.get("primary_artifact_id").and_then(|v| v.as_str()).is_none(),
        "primary_artifact_id should be unresolved in this scenario"
    );

    let resume_hint = parsed
        .get("resume_hint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        !resume_hint.starts_with("federation_complete"),
        "federation_complete must require a recorded verdict, even when the artifact is present; got: {}",
        resume_hint
    );

    Ok(())
}
