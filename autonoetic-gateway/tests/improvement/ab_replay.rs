
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::improvement::AbReplayTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::agent_revision::{AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::PrincipalKind;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;
use crate::support::manifest_builder::TestManifest;

fn test_manifest(capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "improvement-orchestrator".to_string(),
            name: "improvement-orchestrator".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        ..TestManifest::new().build()
    }
}

fn seed_revision(
    store: &GatewayStore,
    agent_id: &str,
    revision_id: &str,
) -> anyhow::Result<()> {
    let rec = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:seed-{}", revision_id),
        runtime_lock_hash: "sha256:seed-lock".to_string(),
        manifest_hash: "sha256:seed-manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "improvement_ab_replay_test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rec)?;
    Ok(())
}

fn seed_alias(store: &GatewayStore, agent_id: &str, revision_id: &str) -> anyhow::Result<()> {
    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "improvement_ab_replay_test".to_string(),
        reason: Some("test seed".to_string()),
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&alias)?;
    Ok(())
}

/// Full revision IDs in AgentRef.parse() format.
const REV_A_ID: &str = "rev_sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REV_B_ID: &str = "rev_sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const TARGET_AGENT: &str = "test-agent";

fn open_store(tmp: &TempDir) -> Arc<GatewayStore> {
    Arc::new(GatewayStore::open(tmp.path()).unwrap())
}

fn setup_env(tmp: &TempDir) -> (Arc<GatewayStore>, GatewayConfig) {
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = open_store(tmp);

    // Seed two revisions of the same test agent
    seed_revision(&store, TARGET_AGENT, REV_A_ID).unwrap();
    seed_revision(&store, TARGET_AGENT, REV_B_ID).unwrap();
    seed_alias(&store, TARGET_AGENT, REV_B_ID).unwrap();

    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: tmp.path().join("agents"),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.agents_dir).unwrap();

    (store, config)
}

fn execute_tool(
    store: Arc<GatewayStore>,
    config: GatewayConfig,
    args: serde_json::Value,
) -> serde_json::Value {
    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let result = AbReplayTool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            None,
            &args.to_string(),
            None,
            None,
            Some(&config),
            Some(store),
            None,
        )
        .unwrap();
    serde_json::from_str(&result).unwrap()
}

// ─── 1. Queued path ───────────────────────────────────────────────────────

#[test]
fn test_ab_replay_queues_eval_runs() {
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool(store, config, args);
    assert_eq!(v["ok"], true, "expected ok=true, got: {v}");
    assert_eq!(v["status"], "queued", "expected queued, got: {v}");
    assert!(v["suite_id"].as_str().unwrap().starts_with("suite-ab-"));
    let ids = v["queued_eval_run_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 2, "expected 2 queued runs, got: {v}");
    assert!(v["message"].as_str().unwrap().contains("Queued"));
}

// ─── 2. Cost ceiling exceeded ─────────────────────────────────────────────

#[test]
fn test_ab_replay_cost_ceiling_exceeded() {
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);

    // 20 tasks × 2 runs × $0.05 = $2.00 > $1.00 ceiling
    let tasks: Vec<serde_json::Value> = (0..20)
        .map(|i| {
            json!({
                "message": format!("task {}", i),
                "case_id": format!("t{}", i),
            })
        })
        .collect();

    let args = json!({
        "task_specs": tasks,
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool(store, config, args);
    assert_eq!(v["ok"], false, "expected ok=false, got: {v}");
    assert_eq!(v["status"], "cost_exceeded", "expected cost_exceeded, got: {v}");
    assert!(v["estimated_cost_usd"].as_f64().unwrap() > 1.0);
    assert_eq!(v["max_budget_usd"], 1.0);
    assert_eq!(v["case_count"], 20);
}

// ─── 3. Missing Evaluation capability ─────────────────────────────────────

#[test]
fn test_ab_replay_requires_evaluation_capability() {
    let manifest = test_manifest(vec![]);
    assert!(!AbReplayTool.is_available(&manifest));

    let eval_manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    assert!(AbReplayTool.is_available(&eval_manifest));
}

// ─── 4. Missing gateway store returns error ───────────────────────────────

#[test]
fn test_ab_replay_requires_gateway_store() {
    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let args = json!({
        "task_specs": [{"message": "hello"}],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let result = AbReplayTool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        None,
        &args.to_string(),
        None,
        None,
        None,
        None,
        None,
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("GatewayStore"), "expected GatewayStore error, got: {err}");
}

// ─── 5. Revisions must belong to same agent ───────────────────────────────

#[test]
fn test_ab_replay_revisions_must_be_same_agent() {
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);

    // Seed a third revision for a DIFFERENT agent
    let other_rev = "rev_sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    seed_revision(&store, "other-agent", other_rev).unwrap();

    let args = json!({
        "task_specs": [{"message": "hello"}],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", "other-agent", other_rev),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let result = AbReplayTool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        None,
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store),
        None,
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("same logical agent"),
        "expected same-agent error, got: {err}"
    );
}

// ─── 6. Holdout covers all tasks → error ──────────────────────────────────

#[test]
fn test_ab_replay_holdout_covers_all_fails() {
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);

    // 1 task, holdout_ratio=1.0 → 0 train tasks
    let args = json!({
        "task_specs": [{"message": "hello"}],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 1.0,
    });

    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let result = AbReplayTool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        None,
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store),
        None,
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("holdout_ratio") || err.contains("no training"),
        "expected holdout error, got: {err}"
    );
}

// ─── 7. Re-invoke while pending returns queued (no duplicate enqueue) ─────

#[test]
fn test_ab_replay_reinvoke_while_pending_returns_queued() {
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);

    let args = json!({
        "task_specs": [{"message": "hello", "case_id": "t1"}],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    // First call: queues the runs
    let v1 = execute_tool(store.clone(), config.clone(), args.clone());
    assert_eq!(v1["status"], "queued");
    let suite_id = v1["suite_id"].as_str().unwrap().to_string();
    let queued_ids = v1["queued_eval_run_ids"].as_array().unwrap().clone();

    // Second call with the same suite_id: runs are still pending → returns queued without re-enqueuing
    let mut args2 = args.clone();
    args2["suite_id"] = json!(suite_id);
    let v2 = execute_tool(store.clone(), config, args2);
    assert_eq!(v2["status"], "queued");
    let second_ids = v2["queued_eval_run_ids"].as_array().unwrap();

    // Should return the same IDs (not new ones)
    assert_eq!(second_ids.len(), queued_ids.len());
    for id in &queued_ids {
        assert!(
            second_ids.iter().any(|i| i == id),
            "expected pending ID {:?} to appear in {:?}",
            id,
            second_ids
        );
    }
}

// ─── 8. End-to-end: completed comparison with divergent revisions ─────

#[test]
fn test_ab_replay_completed_comparison_with_divergent_revisions() {
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);

    // Create a pre-existing eval suite with 2 cases
    let suite_id = "suite-e2e-test".to_string();
    let spec_json = json!({
        "cases": [
            {"case_id": "t1", "message": "do task one", "assertions": {"reply_max_chars": 100}},
            {"case_id": "t2", "message": "do task two", "assertions": {"reply_max_chars": 100}},
        ]
    });
    let suite = autonoetic_types::evaluation::EvalSuiteRecord {
        suite_id: suite_id.clone(),
        name: "e2e-test".to_string(),
        description: "E2E test suite".to_string(),
        spec_json,
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        origin_node_id: "gateway".to_string(),
        evaluated_targets: vec![TARGET_AGENT.to_string()],
        author_agent_id: None,
        based_on_suite_id: None,
    };
    store.insert_eval_suite(&suite).unwrap();

    // Baseline run: passes all cases
    let baseline_run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: "eval-baseline-e2e".to_string(),
        suite_id: suite_id.clone(),
        subject_agent_id: TARGET_AGENT.to_string(),
        subject_revision_id: REV_A_ID.to_string(),
        baseline_revision_id: None,
        status: autonoetic_types::evaluation::EvalRunStatus::Passed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: None,
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({"passed": 2, "failed": 0}),
        report_handle: None,
        origin_node_id: "test".to_string(),
    };
    store.insert_eval_run(&baseline_run).unwrap();

    // Candidate run: fails t2
    let candidate_run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: "eval-candidate-e2e".to_string(),
        suite_id: suite_id.clone(),
        subject_agent_id: TARGET_AGENT.to_string(),
        subject_revision_id: REV_B_ID.to_string(),
        baseline_revision_id: Some(REV_A_ID.to_string()),
        status: autonoetic_types::evaluation::EvalRunStatus::Failed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: None,
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({"passed": 1, "failed": 1}),
        report_handle: None,
        origin_node_id: "test".to_string(),
    };
    store.insert_eval_run(&candidate_run).unwrap();

    // Case results: baseline passes t1, t2; candidate passes t1, fails t2
    for (run_id, case_id, status) in &[
        ("eval-baseline-e2e", "t1", "passed"),
        ("eval-baseline-e2e", "t2", "passed"),
        ("eval-candidate-e2e", "t1", "passed"),
        ("eval-candidate-e2e", "t2", "failed"),
    ] {
        store
            .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
                eval_run_id: run_id.to_string(),
                case_id: case_id.to_string(),
                status: status.to_string(),
                score: if *status == "passed" { Some(1.0) } else { Some(0.0) },
                session_id: None,
                notes: None,
                output_json: json!({}),
            })
            .unwrap();
    }

    // Call ab_replay with the existing suite_id
    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "suite_id": suite_id,
        "holdout_ratio": 0.0,
    });

    let v = execute_tool(store, config, args);
    assert_eq!(v["ok"], true, "expected ok=true, got: {v}");
    assert_eq!(v["status"], "completed", "expected completed, got: {v}");

    // Summary assertions
    let summary = &v["summary"];
    assert_eq!(summary["baseline_passed"], 2, "baseline should pass all");
    assert_eq!(summary["baseline_total"], 2);
    assert_eq!(summary["candidate_passed"], 1, "candidate should pass 1");
    assert_eq!(summary["candidate_total"], 2);
    assert_eq!(summary["delta_passed"], -1);
    assert_eq!(summary["regression_count"], 1);
    assert_eq!(summary["improvement_count"], 0);

    // Regression is t2: base=passed, cand=failed
    let regressions = v["regressions"].as_array().unwrap();
    assert_eq!(regressions.len(), 1);
    assert_eq!(regressions[0], "t2");

    // No improvements
    let improvements = v["improvements"].as_array().unwrap();
    assert!(improvements.is_empty());

    // Holdout is empty (holdout_ratio=0)
    let holdout = &v["holdout"];
    assert_eq!(holdout["total_held_out"], 0);

    // Stats may be None (no session outcomes) — acceptable
    // Verify run IDs are present
    assert_eq!(v["baseline_eval_run_id"], "eval-baseline-e2e");
    assert_eq!(v["candidate_eval_run_id"], "eval-candidate-e2e");
}

// ─── 9. Prompt-only guardrail (P4) ─────────────────────────────────────────
//
// The `improve.restrict_to_prompt_only` flag (defaults to true) is enforced
// at A/B replay time when a `gateway_dir` is provided. It loads the two
// revisions' on-disk SKILL.md files and refuses the comparison if their
// declared `capabilities` or `allowed_tool_tiers` differ.

/// Write a minimal SKILL.md for a revision under
/// `gateway_dir/revisions/agents/<agent>/<rev>/SKILL.md`, with the
/// caller-supplied capability list in the frontmatter. Used by the
/// surface-drift integration tests below.
fn write_revision_skill_md(
    gateway_dir: &Path,
    agent_id: &str,
    revision_id: &str,
    capabilities_yaml: &str,
) {
    let dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&dir).unwrap();
    let skill_md = format!(
        r#"---
name: "{agent}"
description: "test agent"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "{agent}"
      name: "{agent}"
      description: "test agent"
    capabilities:
{caps}
---

# {agent}

This is the test prompt body. Self-improvement loops are free to edit me.
"#,
        agent = agent_id,
        caps = capabilities_yaml,
    );
    std::fs::write(dir.join("SKILL.md"), skill_md).unwrap();
}

fn execute_tool_with_gateway_dir(
    store: Arc<GatewayStore>,
    config: GatewayConfig,
    gateway_dir: &Path,
    args: serde_json::Value,
) -> serde_json::Value {
    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let result = AbReplayTool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir),
            &args.to_string(),
            None,
            None,
            Some(&config),
            Some(store),
            None,
        )
        .unwrap();
    serde_json::from_str(&result).unwrap()
}

#[test]
fn test_ab_replay_prompt_only_guard_rejects_capability_widening() {
    // Tool convention (per schema): revision_a = baseline, revision_b
    // = candidate. The candidate (B) adds CodeExecution on top of
    // baseline (A); the gate must reject.
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");

    // Baseline = REV_A: minimal Evaluation capability only.
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_A_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]"#,
    );
    // Candidate = REV_B: adds CodeExecution. This is a capability
    // *kind* addition (the easy case for any drift checker).
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_B_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "CodeExecution"
        patterns: ["*"]"#,
    );

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_eq!(v["ok"], false, "guardrail should reject");
    assert_eq!(v["status"], "surface_drift_rejected");
    assert_eq!(v["guardrail"], "improve.restrict_to_prompt_only");
    // policy_applied is set on rejects too (PR #269 audit-trail
    // uniformity). For the "not opted in" case, the classification is
    // `prompt_only_violation`.
    assert_eq!(v["policy_applied"], "prompt_only_violation");
    assert_eq!(v["classification"], "prompt_only_violation");
    let reason = v["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("CodeExecution"),
        "rejection reason should name the added kind: got {}",
        reason
    );
}

#[test]
fn test_ab_replay_prompt_only_guard_rejects_parameter_widening() {
    // The crucial case Copilot's PR #267 review caught: same
    // capability *kind* in both revisions, but the candidate widened
    // the parameters. A naive kind-only comparator would let this
    // through; `compute_capability_delta` catches it as "broadened".
    //
    // Baseline (A): ReadAccess scoped to "self.*".
    // Candidate (B): ReadAccess scoped to "*" (all sessions).
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");

    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_A_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "ReadAccess"
        scopes: ["self.*"]"#,
    );
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_B_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "ReadAccess"
        scopes: ["*"]"#,
    );

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_eq!(
        v["status"], "surface_drift_rejected",
        "parameter widening must trip the gate (this is the bug Copilot caught on PR #267); got {:?}",
        v
    );
    let reason = v["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("broadened") && reason.contains("ReadAccess"),
        "rejection reason should name the broadened kind: got {}",
        reason
    );
}

#[test]
fn test_ab_replay_prompt_only_guard_allows_identical_surface() {
    // Both revisions declare the same Evaluation capability. The gate
    // is on, but the surfaces match → the comparison proceeds (and
    // returns "queued" since no eval runs exist yet — that's the
    // normal pre-A/B state, not a rejection).
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");

    let identical_caps = r#"      - type: "Evaluation"
        patterns: ["*"]"#;
    // Baseline = REV_A, candidate = REV_B.
    write_revision_skill_md(&gateway_dir, TARGET_AGENT, REV_A_ID, identical_caps);
    write_revision_skill_md(&gateway_dir, TARGET_AGENT, REV_B_ID, identical_caps);

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    // Should NOT be the surface_drift_rejected status — anything else
    // (queued, completed, etc.) means the gate let it through.
    assert_ne!(
        v["status"], "surface_drift_rejected",
        "gate must allow identical-surface comparisons; got {:?}",
        v
    );
}

#[test]
fn test_ab_replay_prompt_only_guard_can_be_disabled() {
    // With restrict_to_prompt_only = false, the gate is skipped even
    // when surfaces differ. Pins the escape hatch for P5+ work that
    // needs to A/B-test capability changes. Baseline = REV_A,
    // candidate = REV_B.
    let tmp = TempDir::new().unwrap();
    let (store, mut config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");
    config.improve.restrict_to_prompt_only = false;

    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_A_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]"#,
    );
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_B_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "CodeExecution"
        patterns: ["*"]"#,
    );

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_ne!(
        v["status"], "surface_drift_rejected",
        "gate must NOT fire when restrict_to_prompt_only is false; got {:?}",
        v
    );
}

// ─── 10. Capability-change guardrail (P5) ──────────────────────────────────
//
// P5 lifts P4's prompt-only restriction selectively: when an operator opts in
// via `improve.allow_capability_changes = true`, the gate permits comparisons
// whose candidate has a low-blast-radius capability delta, with the holdout
// ratio coerced up to `capability_change_min_holdout` (default 0.5). High-blast
// changes (sandbox / network / code-exec / credential / scheduler / revision)
// remain rejected regardless.

#[test]
fn test_ab_replay_p5_allows_low_blast_capability_change_with_strict_holdout() {
    // Baseline: minimal Evaluation. Candidate: adds AgentMessage
    // (low-blast — agent-to-agent communication, not a sandbox/network
    // privilege). With allow_capability_changes=true, the gate should
    // permit the comparison and coerce holdout up to 0.5.
    let tmp = TempDir::new().unwrap();
    let (store, mut config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");
    config.improve.allow_capability_changes = true;

    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_A_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]"#,
    );
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_B_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "AgentMessage"
        patterns: ["*"]"#,
    );

    let args = json!({
        "task_specs": (0..8).map(|i| json!({
            "message": format!("do task {}", i),
            "case_id": format!("t{}", i),
        })).collect::<Vec<_>>(),
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.1, // intentionally below the 0.5 minimum
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_ne!(
        v["status"], "surface_drift_rejected",
        "low-blast capability change with opt-in must not reject; got {:?}",
        v
    );
    assert_eq!(
        v["policy_applied"], "capability_change_with_strict_holdout",
        "expected policy_applied=capability_change_with_strict_holdout; got {:?}",
        v["policy_applied"]
    );
    // Holdout was coerced from 0.1 up to the configured minimum.
    let coerced = v["holdout_coerced_from"].as_f64();
    assert!(
        coerced.is_some() && (coerced.unwrap() - 0.1).abs() < 1e-9,
        "expected holdout_coerced_from=0.1; got {:?}",
        v["holdout_coerced_from"]
    );
}

#[test]
fn test_ab_replay_p5_rejects_high_blast_added_kind() {
    // Adding CodeExecution to a candidate is high-blast-radius (it's
    // in the default high_blast_radius_capability_kinds list). Even
    // with allow_capability_changes=true, the gate must reject.
    let tmp = TempDir::new().unwrap();
    let (store, mut config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");
    config.improve.allow_capability_changes = true;

    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_A_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]"#,
    );
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_B_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "CodeExecution"
        patterns: ["*"]"#,
    );

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.5,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_eq!(v["status"], "surface_drift_rejected", "got {:?}", v);
    assert_eq!(v["classification"], "high_blast_radius");
    // policy_applied now mirrors classification on rejects (PR #269
    // review fix — uniform audit field across all responses).
    assert_eq!(v["policy_applied"], "high_blast_radius");
    let reason = v["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("CodeExecution"),
        "rejection reason should cite CodeExecution: got {}",
        reason
    );
}

#[test]
fn test_ab_replay_p5_rejects_high_blast_broadened_kind() {
    // Broadening an existing high-blast capability is also rejected.
    // Baseline has NetworkAccess scoped to one host; candidate widens
    // to all hosts. NetworkAccess is in the default high-blast list,
    // so this must reject regardless of the opt-in flag.
    let tmp = TempDir::new().unwrap();
    let (store, mut config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");
    config.improve.allow_capability_changes = true;

    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_A_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "NetworkAccess"
        hosts: ["api.example.com"]"#,
    );
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_B_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "NetworkAccess"
        hosts: ["*"]"#,
    );

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.5,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_eq!(v["status"], "surface_drift_rejected", "got {:?}", v);
    assert_eq!(v["classification"], "high_blast_radius");
    // policy_applied now mirrors classification on rejects (PR #269
    // review fix — uniform audit field across all responses).
    assert_eq!(v["policy_applied"], "high_blast_radius");
    let reason = v["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("NetworkAccess"),
        "rejection reason should cite NetworkAccess: got {}",
        reason
    );
}

#[test]
fn test_ab_replay_p5_opt_in_unaffected_by_no_delta() {
    // When there's no delta, the policy is `Allow` and the gate is
    // silent regardless of allow_capability_changes. Verifies the
    // happy path's `policy_applied` reads `no_delta`.
    let tmp = TempDir::new().unwrap();
    let (store, mut config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");
    config.improve.allow_capability_changes = true;

    let identical_caps = r#"      - type: "Evaluation"
        patterns: ["*"]"#;
    write_revision_skill_md(&gateway_dir, TARGET_AGENT, REV_A_ID, identical_caps);
    write_revision_skill_md(&gateway_dir, TARGET_AGENT, REV_B_ID, identical_caps);

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.3,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_ne!(v["status"], "surface_drift_rejected", "got {:?}", v);
    assert_eq!(v["policy_applied"], "no_delta");
    // Holdout NOT coerced — caller's 0.3 is fine when there's no delta.
    assert!(
        v["holdout_coerced_from"].is_null(),
        "no coercion expected when there's no delta; got {:?}",
        v["holdout_coerced_from"]
    );
}

#[test]
fn test_ab_replay_p5_low_blast_with_high_enough_holdout_is_not_coerced() {
    // When the caller already provides a holdout ≥ the configured
    // minimum, no coercion happens. The capability change still goes
    // through the strict-holdout policy, just with the caller's value.
    let tmp = TempDir::new().unwrap();
    let (store, mut config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");
    config.improve.allow_capability_changes = true;

    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_A_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]"#,
    );
    write_revision_skill_md(
        &gateway_dir,
        TARGET_AGENT,
        REV_B_ID,
        r#"      - type: "Evaluation"
        patterns: ["*"]
      - type: "AgentMessage"
        patterns: ["*"]"#,
    );

    let args = json!({
        "task_specs": (0..8).map(|i| json!({
            "message": format!("do task {}", i),
            "case_id": format!("t{}", i),
        })).collect::<Vec<_>>(),
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.6, // already above the 0.5 min
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_eq!(v["policy_applied"], "capability_change_with_strict_holdout");
    assert!(
        v["holdout_coerced_from"].is_null(),
        "caller's holdout was already above the min — no coercion; got {:?}",
        v["holdout_coerced_from"]
    );
}

// ─── 11. Audit-trail uniformity (PR #269 review) ──────────────────────────
//
// `policy_applied` must reflect what actually happened across all six
// surface-policy states, not just the no-delta default. These tests
// pin the two "gate skipped" branches (`gate_disabled` and
// `not_evaluated`) that earlier silently reported `no_delta`.

#[test]
fn test_ab_replay_policy_applied_gate_disabled_when_restrict_off() {
    // Even with a real on-disk capability widening, `policy_applied`
    // is `gate_disabled` (not `no_delta`) when the master switch is
    // off — the policy was never consulted.
    let tmp = TempDir::new().unwrap();
    let (store, mut config) = setup_env(&tmp);
    let gateway_dir = tmp.path().join(".gateway");
    config.improve.restrict_to_prompt_only = false;

    let identical_caps = r#"      - type: "Evaluation"
        patterns: ["*"]"#;
    write_revision_skill_md(&gateway_dir, TARGET_AGENT, REV_A_ID, identical_caps);
    write_revision_skill_md(&gateway_dir, TARGET_AGENT, REV_B_ID, identical_caps);

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool_with_gateway_dir(store, config, &gateway_dir, args);
    assert_eq!(
        v["policy_applied"], "gate_disabled",
        "expected gate_disabled when restrict_to_prompt_only=false; got {:?}",
        v["policy_applied"]
    );
}

#[test]
fn test_ab_replay_policy_applied_not_evaluated_when_gateway_dir_absent() {
    // When the gate is on but gateway_dir is None, the policy can't
    // be evaluated. The response must say so explicitly, not pretend
    // there was no delta.
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);
    // Note: existing execute_tool helper passes `None` for gateway_dir.
    // The gate's master switch (restrict_to_prompt_only) defaults to
    // true, so this exercises the not_evaluated branch.

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "holdout_ratio": 0.0,
    });

    let v = execute_tool(store, config, args);
    assert_eq!(
        v["policy_applied"], "not_evaluated",
        "expected not_evaluated when gateway_dir is None; got {:?}",
        v["policy_applied"]
    );
}
