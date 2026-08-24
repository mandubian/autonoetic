//! Integration tests: Phase 3 — Agent Revisions, Evaluation, and Federation.
//!
//! Tests cover:
//!   1. Eval suite publish validation (case_id uniqueness, assertion grammar, non-vacuous cases)
//!   2. Eval run with agent_ref resolution and revision-agent validation
//!   3. Eval runner assertion engine (all 5 assertion types)
//!   4. Revision materialization and loading from revision directories
//!   5. Evaluation capability scope enforcement
//!   6. End-to-end tool execution for eval.suite.publish and eval.run
//!   7. Revision directory materialization on agent.revision.create


use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::{
    validate_suite_spec, AgentRevisionDiffTool, AgentRevisionPromoteTool, EvalCompareTool,
    EvalReportTool, EvalRunTool, EvalSuiteCaseSpec, EvalSuitePublishTool, EvalSuiteSpec,
    NativeTool,
};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::capability::Capability;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

fn manifest_with_capabilities(caps: Vec<Capability>) -> AgentManifest {
    let yaml = format!(
        r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "test-agent"
  name: "Test Agent"
  description: "Test."
capabilities: {}
llm_config:
  provider: "openai"
  model: "test-model"
---
# Test Agent
"#,
        serde_json::to_string(&json!(caps)).unwrap()
    );
    let (manifest, _instructions) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&yaml).unwrap();
    manifest
}

// ─────────────────────────────────────────────────────────────────────
// 1. Eval suite publish validation
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_validate_suite_spec_rejects_empty_cases() {
    use autonoetic_gateway::runtime::tools::{validate_suite_spec, EvalSuiteSpec};

    let spec = EvalSuiteSpec { cases: vec![] };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("at least one case"));
}

// ─────────────────────────────────────────────────────────────────────
// 7. Eval runner assertion + report generation tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_evaluate_assertions_all_types_combined() {
    use autonoetic_gateway::scheduler::eval_runner::{evaluate_assertions, EvalAssertions};

    // All assertions pass
    let assertions = EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: Some(vec!["task".into(), "complete".into()]),
        reply_contains_none: Some(vec!["error".into()]),
        reply_max_chars: Some(1000),
        artifacts_min: Some(1),
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };
    assert!(evaluate_assertions(&assertions, "task complete", 2));
    assert!(!evaluate_assertions(&assertions, "Task", 2)); // missing "complete"
    assert!(!evaluate_assertions(&assertions, "task complete error", 2)); // contains "error"
    assert!(!evaluate_assertions(&assertions, "task complete", 0)); // artifacts_min failed

    // Artifacts max constraint
    let assertions_max = EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: None,
        reply_contains_none: None,
        reply_max_chars: None,
        artifacts_min: None,
        artifacts_max: Some(3),
        session_events_min: None,
        session_events_max: None,
    };
    assert!(evaluate_assertions(&assertions_max, "", 2));
    assert!(!evaluate_assertions(&assertions_max, "", 5));

    // Reply max chars
    let assertions_chars = EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: None,
        reply_contains_none: None,
        reply_max_chars: Some(10),
        artifacts_min: None,
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };
    assert!(evaluate_assertions(&assertions_chars, "Short", 0));
    assert!(!evaluate_assertions(
        &assertions_chars,
        "This is way too long",
        0
    ));
}

#[test]
fn test_eval_runner_report_generation_from_store() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    // Insert a completed eval run with report_handle
    let eval_run_id = "eval-runner-report-test";
    let run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: eval_run_id.to_string(),
        suite_id: "suite-runner-test".to_string(),
        subject_agent_id: "test-agent".to_string(),
        subject_revision_id: "rev_test".to_string(),
        baseline_revision_id: None,
        status: autonoetic_types::evaluation::EvalRunStatus::Failed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({
            "suite_name": "Runner Test Suite",
            "case_count": 3,
            "passed": 2,
            "failed": 1,
        }),
        report_handle: Some("sha256:generated_report_abc123".to_string()),
        origin_node_id: "gateway".to_string(),
    };
    store.insert_eval_run(&run).unwrap();

    // Insert mixed case results (2 passed, 1 failed)
    store
        .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
            eval_run_id: eval_run_id.to_string(),
            case_id: "case_pass_1".into(),
            status: "passed".into(),
            score: Some(1.0),
            session_id: Some("session-1".into()),
            notes: None,
            output_json: json!({ "reply_length": 50 }),
        })
        .unwrap();

    store
        .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
            eval_run_id: eval_run_id.to_string(),
            case_id: "case_fail".into(),
            status: "failed".into(),
            score: None,
            session_id: Some("session-2".into()),
            notes: Some("reply_max_chars failed".into()),
            output_json: json!({ "reply_length": 5000 }),
        })
        .unwrap();

    store
        .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
            eval_run_id: eval_run_id.to_string(),
            case_id: "case_pass_2".into(),
            status: "passed".into(),
            score: Some(0.8),
            session_id: Some("session-3".into()),
            notes: None,
            output_json: json!({ "reply_length": 100 }),
        })
        .unwrap();

    // Verify the run and all case results are retrievable
    let retrieved_run = store.get_eval_run(eval_run_id).unwrap().unwrap();
    assert_eq!(
        retrieved_run.status,
        autonoetic_types::evaluation::EvalRunStatus::Failed
    );
    assert_eq!(
        retrieved_run.report_handle,
        Some("sha256:generated_report_abc123".to_string())
    );

    let case_results = store.list_eval_case_results(eval_run_id).unwrap();
    assert_eq!(case_results.len(), 3);

    let passed: Vec<_> = case_results
        .iter()
        .filter(|c| c.status == "passed")
        .collect();
    let failed: Vec<_> = case_results
        .iter()
        .filter(|c| c.status == "failed")
        .collect();
    assert_eq!(passed.len(), 2);
    assert_eq!(failed.len(), 1);

    // Verify the failed case has notes
    let failed_case = &failed[0];
    assert!(failed_case
        .notes
        .as_ref()
        .unwrap()
        .contains("reply_max_chars"));
}

// ─────────────────────────────────────────────────────────────────────
// 6. Core happy path integration tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_eval_run_persists_with_real_revision() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    // Create a real revision in the store
    let revision_id = "rev_sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let rev = autonoetic_types::agent_revision::AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: "test-agent".to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: "sha256:test".to_string(),
        runtime_lock_hash: "none".to_string(),
        manifest_hash: "sha256:test".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "test".to_string(),
        trust_domain: "test".to_string(),
        status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
        metadata_json: json!({}),
        short_id: "test1234".to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();

    // Create a suite
    let suite_id = "suite-test123";
    let suite = autonoetic_types::evaluation::EvalSuiteRecord {
        suite_id: suite_id.to_string(),
        name: "Test Suite".to_string(),
        description: "A test suite".to_string(),
        spec_json: json!({
            "cases": [
                {
                    "case_id": "case_a",
                    "message": "Hello",
                    "assertions": { "reply_max_chars": 200 }
                }
            ]
        }),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        origin_node_id: "test".to_string(),
        evaluated_targets: vec![],
        author_agent_id: None,
        based_on_suite_id: None,
    };
    store.insert_eval_suite(&suite).unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let agents_dir = tmp.path().join("agents");
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        ..Default::default()
    };

    let tool = EvalRunTool;
    let args = json!({
        "agent_ref": format!("test-agent@{}", revision_id),
        "suite_id": suite_id,
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    let response: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["status"], "queued");
    let eval_run_id = response["eval_run_id"].as_str().unwrap();

    // Verify the run was persisted
    let run = store.get_eval_run(eval_run_id).unwrap().unwrap();
    assert_eq!(run.subject_agent_id, "test-agent");
    assert_eq!(run.subject_revision_id, revision_id);
    assert_eq!(
        run.status,
        autonoetic_types::evaluation::EvalRunStatus::Queued
    );
    assert!(run.report_handle.is_none());
}

#[test]
fn test_eval_report_returns_persisted_data() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    // Insert a completed eval run
    let eval_run_id = "eval-report-test";
    let run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: eval_run_id.to_string(),
        suite_id: "suite-xyz".to_string(),
        subject_agent_id: "test-agent".to_string(),
        subject_revision_id: "rev_test".to_string(),
        baseline_revision_id: None,
        status: autonoetic_types::evaluation::EvalRunStatus::Passed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({
            "suite_name": "Test Suite",
            "case_count": 2,
            "passed": 2,
            "failed": 0,
        }),
        report_handle: Some("sha256:report123".to_string()),
        origin_node_id: "test".to_string(),
    };
    store.insert_eval_run(&run).unwrap();

    // Insert case results
    let case_result = autonoetic_types::evaluation::EvalCaseResultRecord {
        eval_run_id: eval_run_id.to_string(),
        case_id: "case_a".to_string(),
        status: "passed".to_string(),
        score: Some(1.0),
        session_id: Some("session-1".to_string()),
        notes: None,
        output_json: json!({ "reply_length": 42 }),
    };
    store.insert_eval_case_result(&case_result).unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let tool = EvalReportTool;
    let args = json!({
        "eval_run_id": eval_run_id,
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    );

    let response: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["run"]["eval_run_id"], eval_run_id);
    assert_eq!(response["run"]["status"], "Passed");
    assert_eq!(response["run"]["report_handle"], "sha256:report123");
    assert_eq!(response["case_count"], 1);
    assert_eq!(
        response["case_results"].as_array().unwrap()[0]["case_id"],
        "case_a"
    );
    assert_eq!(
        response["case_results"].as_array().unwrap()[0]["status"],
        "passed"
    );
}

#[test]
fn test_agent_revision_diff_reports_modified_files() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let rev_a = "rev_sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let rev_b = "rev_sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    for (revision_id, content_digest, runtime_lock_hash, manifest_hash) in [
        (rev_a, "sha256:a", "sha256:lock-a", "sha256:manifest-a"),
        (rev_b, "sha256:b", "sha256:lock-b", "sha256:manifest-b"),
    ] {
        let rec = autonoetic_types::agent_revision::AgentRevisionRecord {
            revision_id: revision_id.to_string(),
            agent_id: "test-agent".to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: content_digest.to_string(),
            runtime_lock_hash: runtime_lock_hash.to_string(),
            manifest_hash: manifest_hash.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: PrincipalKind::Human.tag().to_string(),
            created_by_id: "test".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "test".to_string(),
            trust_domain: "local".to_string(),
            status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            metadata_json: json!({}),
            short_id: "testshort".to_string(),
        detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store.insert_agent_revision(&rec).unwrap();
    }

    let rev_dir_a = gateway_dir
        .join("revisions")
        .join("agents")
        .join("test-agent")
        .join(rev_a);
    let rev_dir_b = gateway_dir
        .join("revisions")
        .join("agents")
        .join("test-agent")
        .join(rev_b);
    std::fs::create_dir_all(&rev_dir_a).unwrap();
    std::fs::create_dir_all(&rev_dir_b).unwrap();
    std::fs::write(rev_dir_a.join("SKILL.md"), "one").unwrap();
    std::fs::write(rev_dir_b.join("SKILL.md"), "two").unwrap();
    std::fs::write(rev_dir_a.join("runtime.lock"), "{}").unwrap();
    std::fs::write(rev_dir_b.join("runtime.lock"), "{}").unwrap();
    std::fs::write(rev_dir_a.join("file.txt"), "alpha").unwrap();
    std::fs::write(rev_dir_b.join("file.txt"), "beta").unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["test-agent*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = AgentRevisionDiffTool;
    let args = json!({
        "from_ref": format!("test-agent@{}", rev_a),
        "to_ref": format!("test-agent@{}", rev_b),
    });

    let out = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["changed"], true);
    assert!(parsed["summary"]["modified"].as_u64().unwrap() >= 1);
}

#[test]
fn test_eval_compare_builds_completed_comparison_report() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let baseline_rev =
        "rev_sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let candidate_rev =
        "rev_sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    for revision_id in [baseline_rev, candidate_rev] {
        let rec = autonoetic_types::agent_revision::AgentRevisionRecord {
            revision_id: revision_id.to_string(),
            agent_id: "test-agent".to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: format!("sha256:{}", &revision_id[11..19]),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: PrincipalKind::Human.tag().to_string(),
            created_by_id: "test".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "test".to_string(),
            trust_domain: "local".to_string(),
            status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            metadata_json: json!({}),
            short_id: "cmp".to_string(),
        detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store.insert_agent_revision(&rec).unwrap();
    }

    let suite = autonoetic_types::evaluation::EvalSuiteRecord {
        suite_id: "suite-compare".to_string(),
        name: "Compare Suite".to_string(),
        description: "desc".to_string(),
        spec_json: json!({"cases":[{"case_id":"c1","message":"m","assertions":{"reply_max_chars":10}}]}),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        origin_node_id: "test".to_string(),
        evaluated_targets: vec![],
        author_agent_id: None,
        based_on_suite_id: None,
    };
    store.insert_eval_suite(&suite).unwrap();

    let baseline_run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: "eval-baseline".to_string(),
        suite_id: suite.suite_id.clone(),
        subject_agent_id: "test-agent".to_string(),
        subject_revision_id: baseline_rev.to_string(),
        baseline_revision_id: None,
        status: autonoetic_types::evaluation::EvalRunStatus::Passed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: None,
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({"passed":1,"failed":0}),
        report_handle: Some("sha256:base-report".to_string()),
        origin_node_id: "test".to_string(),
    };
    let candidate_run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: "eval-candidate".to_string(),
        suite_id: suite.suite_id.clone(),
        subject_agent_id: "test-agent".to_string(),
        subject_revision_id: candidate_rev.to_string(),
        baseline_revision_id: Some(baseline_rev.to_string()),
        status: autonoetic_types::evaluation::EvalRunStatus::Failed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: None,
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({"passed":0,"failed":1}),
        report_handle: Some("sha256:candidate-report".to_string()),
        origin_node_id: "test".to_string(),
    };
    store.insert_eval_run(&baseline_run).unwrap();
    store.insert_eval_run(&candidate_run).unwrap();

    store
        .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
            eval_run_id: baseline_run.eval_run_id.clone(),
            case_id: "c1".to_string(),
            status: "passed".to_string(),
            score: Some(1.0),
            session_id: None,
            notes: None,
            output_json: json!({}),
        })
        .unwrap();
    store
        .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
            eval_run_id: candidate_run.eval_run_id.clone(),
            case_id: "c1".to_string(),
            status: "failed".to_string(),
            score: Some(0.0),
            session_id: None,
            notes: Some("regression".to_string()),
            output_json: json!({}),
        })
        .unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["suite-*".into(), "test-agent*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: tmp.path().join("agents").join(".gateway"),
        agents_dir: tmp.path().join("agents"),
        ..Default::default()
    };

    let tool = EvalCompareTool;
    let args = json!({
        "suite_id": "suite-compare",
        "baseline_ref": format!("test-agent@{}", baseline_rev),
        "candidate_ref": format!("test-agent@{}", candidate_rev),
    });
    let out = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["status"], "completed");
    assert_eq!(parsed["summary"]["regression_count"], 1);
}

#[test]
fn test_eval_compare_with_session_outcomes_produces_stats() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let baseline_rev = "rev_sha256:ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc0";
    let candidate_rev = "rev_sha256:ddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd1";
    for revision_id in [baseline_rev, candidate_rev] {
        let rec = autonoetic_types::agent_revision::AgentRevisionRecord {
            revision_id: revision_id.to_string(),
            agent_id: "test-agent".to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: format!("sha256:{}", &revision_id[11..19]),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: PrincipalKind::Human.tag().to_string(),
            created_by_id: "test".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "test".to_string(),
            trust_domain: "local".to_string(),
            status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
            metadata_json: json!({}),
            short_id: "cmp".to_string(),
        detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store.insert_agent_revision(&rec).unwrap();
    }

    let suite = autonoetic_types::evaluation::EvalSuiteRecord {
        suite_id: "suite-stats".to_string(),
        name: "Stats Suite".to_string(),
        description: "desc".to_string(),
        spec_json: json!({"cases":[
            {"case_id":"c1","message":"m","assertions":{"reply_max_chars":10}},
            {"case_id":"c2","message":"m","assertions":{"reply_max_chars":10}},
            {"case_id":"c3","message":"m","assertions":{"reply_max_chars":10}},
            {"case_id":"c4","message":"m","assertions":{"reply_max_chars":10}},
            {"case_id":"c5","message":"m","assertions":{"reply_max_chars":10}},
        ]}),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        origin_node_id: "test".to_string(),
        evaluated_targets: vec![],
        author_agent_id: None,
        based_on_suite_id: None,
    };
    store.insert_eval_suite(&suite).unwrap();

    let baseline_run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: "eval-stats-baseline".to_string(),
        suite_id: suite.suite_id.clone(),
        subject_agent_id: "test-agent".to_string(),
        subject_revision_id: baseline_rev.to_string(),
        baseline_revision_id: None,
        status: autonoetic_types::evaluation::EvalRunStatus::Passed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: None,
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({"passed":5,"failed":0}),
        report_handle: Some("sha256:base-report".to_string()),
        origin_node_id: "test".to_string(),
    };
    let candidate_run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: "eval-stats-candidate".to_string(),
        suite_id: suite.suite_id.clone(),
        subject_agent_id: "test-agent".to_string(),
        subject_revision_id: candidate_rev.to_string(),
        baseline_revision_id: Some(baseline_rev.to_string()),
        status: autonoetic_types::evaluation::EvalRunStatus::Passed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: None,
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({"passed":5,"failed":0}),
        report_handle: Some("sha256:candidate-report".to_string()),
        origin_node_id: "test".to_string(),
    };
    store.insert_eval_run(&baseline_run).unwrap();
    store.insert_eval_run(&candidate_run).unwrap();

    // Create 5 case results per run, each linked to a session outcome.
    // Baseline: all passed, slightly higher cost/tokens/turns.
    // Candidate: all passed, cheaper on every axis = B should be preferred.
    let session_ids: Vec<String> = (0..5).map(|i| format!("session-b-{}", i)).collect();
    for (i, sid) in session_ids.iter().enumerate() {
        store
            .upsert_session_outcome_metrics(
                sid,
                "root-session",
                "eval-agent",
                None,
                10 + i as u64,
                1000 + (i as u64 * 10),
                0.10 + (i as f64 * 0.01),
                60.0 + i as f64,
            )
            .unwrap();
        // Set completion via grader (different agent to pass ownership check)
        store
            .set_session_outcome_grade(
                sid,
                "outcome-grader",
                autonoetic_types::session_outcome::Completion::Achieved,
                None,
            )
            .unwrap();
        store
            .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
                eval_run_id: baseline_run.eval_run_id.clone(),
                case_id: format!("c{}", i + 1),
                status: "passed".to_string(),
                score: Some(1.0),
                session_id: Some(sid.clone()),
                notes: None,
                output_json: json!({}),
            })
            .unwrap();
    }

    let candidate_sessions: Vec<String> =
        (0..5).map(|i| format!("session-c-{}", i)).collect();
    for (i, sid) in candidate_sessions.iter().enumerate() {
        // Candidate is cheaper: lower cost, fewer tokens, fewer turns
        store
            .upsert_session_outcome_metrics(
                sid,
                "root-session",
                "eval-agent",
                None,
                8 + i as u64,
                800 + (i as u64 * 5),
                0.07 + (i as f64 * 0.005),
                45.0 + i as f64,
            )
            .unwrap();
        store
            .set_session_outcome_grade(
                sid,
                "outcome-grader",
                autonoetic_types::session_outcome::Completion::Achieved,
                None,
            )
            .unwrap();
        store
            .insert_eval_case_result(&autonoetic_types::evaluation::EvalCaseResultRecord {
                eval_run_id: candidate_run.eval_run_id.clone(),
                case_id: format!("c{}", i + 1),
                status: "passed".to_string(),
                score: Some(1.0),
                session_id: Some(sid.clone()),
                notes: None,
                output_json: json!({}),
            })
            .unwrap();
    }

    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["suite-*".into(), "test-agent*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: tmp.path().join("agents").join(".gateway"),
        agents_dir: tmp.path().join("agents"),
        ..Default::default()
    };

    let tool = EvalCompareTool;
    let args = json!({
        "suite_id": "suite-stats",
        "baseline_ref": format!("test-agent@{}", baseline_rev),
        "candidate_ref": format!("test-agent@{}", candidate_rev),
    });
    let out = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            Some(&config),
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["status"], "completed");

    // The stats field should exist and contain a recommendation
    let stats = parsed["stats"].as_object();
    assert!(
        stats.is_some(),
        "expected 'stats' field in eval_compare output, got: {}",
        out
    );
    let stats = stats.unwrap();
    let recommendation = stats
        .get("recommendation")
        .and_then(|r| r.as_str())
        .unwrap_or_else(|| {
            panic!(
                "expected 'recommendation' in stats — if an error occurred the test data \
                 should be fixed, not silently accepted. got: {:?}",
                stats
            )
        });
    assert_eq!(recommendation, "prefer_b");
}

#[test]
fn test_load_from_revision_dir_succeeds_with_materialized_revision() {
    let tmp = TempDir::new().unwrap();
    let agents_dir = tmp.path().join("agents");
    let gateway_dir = tmp.path().join(".gateway");
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join("test-agent")
        .join("rev_test123");
    std::fs::create_dir_all(&rev_dir).unwrap();

    // Materialize a revision directory with SKILL.md
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
  id: "test-agent"
  name: "Test Agent"
  description: "Test."
capabilities: []
llm_config:
  provider: "openai"
  model: "test-model"
---
# Test Agent
You are a test agent.
"#;
    std::fs::write(rev_dir.join("SKILL.md"), skill_md).unwrap();
    std::fs::write(rev_dir.join("runtime.lock"), "dependencies: []\n").unwrap();

    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        ..Default::default()
    };
    let repo = autonoetic_gateway::agent::repository::AgentRepository::from_config(&config);

    let loaded = repo
        .load_from_revision_dir(&gateway_dir, "test-agent", "rev_test123")
        .unwrap();
    assert_eq!(loaded.manifest.agent.id, "test-agent");
    assert_eq!(loaded.manifest.agent.name, "Test Agent");
}

// ─────────────────────────────────────────────────────────────────────
// 5. Full integration tests with GatewayStore
// ─────────────────────────────────────────────────────────────────────

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use std::sync::Arc;

#[test]
fn test_eval_suite_publish_with_gateway_store() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let tool = EvalSuitePublishTool;
    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["test-suite*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let args = json!({
        "name": "test-suite-integration",
        "description": "Integration test suite",
        "spec": {
            "cases": [
                {
                    "case_id": "case_a",
                    "message": "Hello world",
                    "assertions": { "reply_max_chars": 200 }
                }
            ]
        }
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    );

    let response: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["status"], "published");
    assert!(response["suite_id"].as_str().unwrap().starts_with("suite-"));
    assert_eq!(response["case_count"], 1);
}

#[test]
fn test_eval_run_creates_queued_record() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let tool = EvalRunTool;
    let args = json!({
        "agent_ref": "test-agent@rev_abc1234567890123456789012345678901234567890123456789012345678",
        "suite_id": "suite-xyz",
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    );

    assert!(
        result.is_err(),
        "Should fail because revision doesn't exist"
    );
}

#[test]
fn test_eval_run_validates_revision_belongs_to_agent() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    // Insert a revision that belongs to "coder", not "planner"
    let revision_id = "rev_sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let rev = autonoetic_types::agent_revision::AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: "coder".to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: "sha256:test".to_string(),
        runtime_lock_hash: "none".to_string(),
        manifest_hash: "sha256:test".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "test".to_string(),
        trust_domain: "test".to_string(),
        status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
        metadata_json: json!({}),
        short_id: "test1234".to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let agents_dir = tmp.path().join("agents");
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        ..Default::default()
    };

    let tool = EvalRunTool;
    // Request eval.run with "planner" but the revision belongs to "coder"
    let args = json!({
        "agent_ref": format!("planner@{}", revision_id),
        "suite_id": "suite-xyz",
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    );

    assert!(
        result.is_err(),
        "Should fail because revision belongs to different agent"
    );
    let err_msg = result.unwrap_err().to_string();
    // The error should be either "not found" (revision doesn't exist for planner)
    // or "belongs to agent" (revision exists but belongs to different agent)
    assert!(
        err_msg.contains("belongs to agent")
            || err_msg.contains("not found")
            || err_msg.contains("Revision"),
        "Error should mention agent mismatch or revision issue, got: {}",
        err_msg
    );
}

#[test]
fn test_eval_report_returns_not_found_for_missing_run() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let tool = EvalReportTool;
    let args = json!({
        "eval_run_id": "eval-nonexistent",
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    );

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[test]
fn test_promote_rejects_required_eval_run_for_different_revision() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let rev_target = autonoetic_types::agent_revision::AgentRevisionRecord {
        revision_id: "rev_sha256:1111111111111111111111111111111111111111111111111111111111111111"
            .to_string(),
        agent_id: "planner.default".to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: "sha256:target".to_string(),
        runtime_lock_hash: "none".to_string(),
        manifest_hash: "sha256:target".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
        metadata_json: json!({}),
        short_id: "target111".to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    let rev_other = autonoetic_types::agent_revision::AgentRevisionRecord {
        revision_id: "rev_sha256:2222222222222222222222222222222222222222222222222222222222222222"
            .to_string(),
        agent_id: "planner.default".to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: "sha256:other".to_string(),
        runtime_lock_hash: "none".to_string(),
        manifest_hash: "sha256:other".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: autonoetic_types::agent_revision::AgentRevisionStatus::Candidate,
        metadata_json: json!({}),
        short_id: "other222".to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev_target).unwrap();
    store.insert_agent_revision(&rev_other).unwrap();

    // Materialize SKILL.md for the target revision so the promotion gate can read capabilities.
    let rev_dir = gateway_dir
        .join("revisions/agents/planner.default")
        .join(&rev_target.revision_id);
    std::fs::create_dir_all(&rev_dir).unwrap();
    std::fs::write(
        rev_dir.join("SKILL.md"),
        "---\nversion: \"1.0\"\nagent:\n  id: planner.default\n  name: planner\n  description: test\ncapabilities: []\n---\n# Test\n",
    ).unwrap();

    let eval_run = autonoetic_types::evaluation::EvalRunRecord {
        eval_run_id: "eval-mismatch".to_string(),
        suite_id: "suite-mismatch".to_string(),
        subject_agent_id: "planner.default".to_string(),
        subject_revision_id: rev_other.revision_id.clone(),
        baseline_revision_id: None,
        status: autonoetic_types::evaluation::EvalRunStatus::Passed,
        queued_at: chrono::Utc::now().to_rfc3339(),
        started_at: Some(chrono::Utc::now().to_rfc3339()),
        completed_at: Some(chrono::Utc::now().to_rfc3339()),
        summary_json: json!({"passed": 1, "failed": 0}),
        report_handle: Some("sha256:report".to_string()),
        origin_node_id: "gateway".to_string(),
    };
    store.insert_eval_run(&eval_run).unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["planner.default*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = AgentRevisionPromoteTool;
    let args = json!({
        "agent_id": "planner.default",
        "revision_id": rev_target.revision_id,
        "required_eval_run_id": eval_run.eval_run_id,
    });

    let err = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect_err("promotion should fail when required eval run points to another revision");
    assert!(
        err.to_string().contains("was for revision"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_revision_directory_materialization_on_create() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateTool;
    let args = json!({
        "agent_id": "test-agent",
        "artifact_id": "nonexistent-artifact",
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    );

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("not found") || err_msg.contains("Artifact"));
}

#[test]
fn test_revision_create_from_intent_requires_reasoning_llm_config() {
    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateFromIntentTool;
    let args = json!({
        "agent_id": "intent-agent",
        "artifact_id": "art_nonexistent",
        "instructions": "# Intent Agent\nBuild from intent.",
        "description": "Intent-driven install test",
        "capabilities": [],
        "execution_mode": "reasoning"
    });

    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    );

    let response = result.expect("tool should return validation envelope");
    assert!(
        response.contains("llm_preset or llm_config is required"),
        "reasoning mode should enforce inference declaration, got: {response}"
    );
}

#[test]
fn test_revision_create_from_intent_materializes_canonical_skill_and_lock() {
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_types::artifact::ArtifactKind;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let session_id = "intent-session";

    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();
    let main_py = b"print('hello from intent')";
    let main_handle = content_store.write(main_py).unwrap();
    content_store
        .register_name(session_id, "main.py", &main_handle)
        .unwrap();

    let bundle = artifact_store
        .build_with_kind(
            &["main.py".to_string()],
            Some(&["main.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["intent.agent*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateFromIntentTool;
    let args = json!({
        "agent_id": "intent.agent",
        "artifact_id": bundle.artifact_id,
        "instructions": "# Intent Agent\n\nUse deterministic install intent.",
        "description": "Intent install agent",
        "capabilities": [{"type": "ReadAccess", "scopes": ["self.*"]}],
        "execution_mode": "reasoning",
        "llm_config": {
            "provider": "openai",
            "model": "gpt-4o",
            "temperature": 0.1,
            "fallback_provider": null,
            "fallback_model": null,
            "chat_only": false
        }
    });

    let out = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ok"], true);
    let revision_id = parsed["revision_id"].as_str().unwrap();

    let revision_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join("intent.agent")
        .join(revision_id);
    assert!(revision_dir.join("main.py").exists());
    assert!(revision_dir.join("SKILL.md").exists());
    assert!(revision_dir.join("runtime.lock").exists());

    let skill_text = std::fs::read_to_string(revision_dir.join("SKILL.md")).unwrap();
    let (parsed_manifest, body) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&skill_text).unwrap();
    assert_eq!(parsed_manifest.agent.id, "intent.agent");
    assert!(body.contains("Use deterministic install intent."));

    let lock_text = std::fs::read_to_string(revision_dir.join("runtime.lock")).unwrap();
    let lock: autonoetic_types::runtime_lock::RuntimeLock =
        serde_yaml::from_str(&lock_text).unwrap();
    assert_eq!(
        lock.gateway.artifact,
        "marketplace://gateway/autonoetic-gateway"
    );
    assert_eq!(lock.dependencies.len(), 0);
}

#[test]
fn test_revision_create_from_intent_script_mode_without_io_accepts_rejected() {
    // Script agents are schema-native: `io.accepts` is a hard birth requirement.
    // A script that parses stdin deterministically (json.loads) with no declared
    // input contract is advertised to callers as `message_format: "free_text"`,
    // so it receives raw prose and crashes at runtime — a failure the promotion
    // gates cannot see. Reject at creation instead.
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_types::artifact::ArtifactKind;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let session_id = "intent-script-session";

    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();
    // A minimal script as the bundled entrypoint.
    let script =
        b"#!/usr/bin/env python3\nimport os\nprint(os.environ.get('AUTONOETIC_INPUT',''))\n";
    let handle = content_store.write(script).unwrap();
    content_store
        .register_name(session_id, "scripts/echo.py", &handle)
        .unwrap();
    let bundle = artifact_store
        .build_with_kind(
            &["scripts/echo.py".to_string()],
            Some(&["scripts/echo.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["script.plain*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateFromIntentTool;
    let args = json!({
        "agent_id": "script.plain",
        "artifact_id": bundle.artifact_id,
        "instructions": "# Plain script agent\n\nEchoes AUTONOETIC_INPUT.",
        "description": "Script agent with no declared io",
        "capabilities": [],
        "execution_mode": "script",
        "script_entry": "scripts/echo.py",
        // io intentionally omitted
    });

    let out = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["ok"], false,
        "script agent without io.accepts must be rejected: {parsed:?}"
    );
    let message = parsed["message"].as_str().unwrap_or("");
    assert!(
        message.contains("io.accepts"),
        "rejection must name the missing contract: {message}"
    );
}

#[test]
fn test_revision_create_from_intent_script_mode_with_io_materializes_contract() {
    // Positive counterpart: a script agent declaring io.accepts / io.returns is
    // created and the canonical SKILL.md carries the declared contract.
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_types::artifact::ArtifactKind;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let session_id = "intent-script-io-session";

    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();
    let script =
        b"#!/usr/bin/env python3\nimport os\nprint(os.environ.get('AUTONOETIC_INPUT',''))\n";
    let handle = content_store.write(script).unwrap();
    content_store
        .register_name(session_id, "scripts/echo.py", &handle)
        .unwrap();
    let bundle = artifact_store
        .build_with_kind(
            &["scripts/echo.py".to_string()],
            Some(&["scripts/echo.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["script.io*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateFromIntentTool;
    let args = json!({
        "agent_id": "script.io",
        "artifact_id": bundle.artifact_id,
        "instructions": "# IO script agent\n\nEchoes AUTONOETIC_INPUT.",
        "description": "Script agent with declared io contract",
        "capabilities": [],
        "execution_mode": "script",
        "script_entry": "scripts/echo.py",
        "io": {
            "accepts": {"type": "object", "required": ["task"], "properties": {"task": {"type": "string"}}},
            "returns": {"type": "object", "required": ["status"], "properties": {"status": {"type": "string"}}}
        }
    });

    let out = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["ok"], true,
        "create_from_intent should succeed: {parsed:?}"
    );
    let revision_id = parsed["revision_id"].as_str().unwrap();

    let revision_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join("script.io")
        .join(revision_id);
    let skill_text = std::fs::read_to_string(revision_dir.join("SKILL.md")).unwrap();
    let (parsed_manifest, _body) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&skill_text).unwrap();

    let io = parsed_manifest
        .io
        .as_ref()
        .expect("script agent with declared io must install with the contract");
    assert!(
        io.accepts.as_ref().and_then(|a| a.get("required")).is_some(),
        "io.accepts must round-trip into the canonical SKILL.md: {:?}",
        io.accepts
    );
    assert!(
        io.returns.as_ref().and_then(|r| r.get("required")).is_some(),
        "io.returns must round-trip into the canonical SKILL.md: {:?}",
        io.returns
    );
    assert!(matches!(
        parsed_manifest.execution_mode,
        autonoetic_types::agent::ExecutionMode::Script
    ));
}

#[test]
fn test_revision_create_accepts_agentskills_compatible_skill_bundle() {
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_types::artifact::ArtifactKind;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let session_id = "compat-session";

    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();

    let skill_md = r#"---
name: "compat-agent"
description: "Compatibility test agent"
license: "MIT"
compatibility: "agentskills.io"
allowed-tools:
  - Read
  - WebFetch
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
      id: "compat-agent"
      name: "Compat Agent"
      description: "Compatibility test agent"
    llm_config:
      provider: "openrouter"
      model: "google/gemini-3-flash-preview"
---
# Compat Agent

Use web fetch when needed.
"#;
    let runtime_lock = r#"gateway:
  artifact: "marketplace://gateway/autonoetic-gateway"
  version: "0.1.0"
  sha256: "replace-me"
sdk:
  version: "0.1.0"
sandbox:
  backend: "bubblewrap"
dependencies: []
artifacts: []
layers: []
"#;

    for (name, bytes) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", b"print('compat workflow')".as_ref()),
    ] {
        let handle = content_store.write(bytes).unwrap();
        content_store
            .register_name(session_id, name, &handle)
            .unwrap();
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

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["compat-agent".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateTool;
    let args = json!({
        "agent_id": "compat-agent",
        "artifact_id": bundle.artifact_id,
        "summary": "compatibility install test"
    });

    let out = tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &args.to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(parsed["ok"], true);
    let revision_id = parsed["revision_id"].as_str().unwrap();

    let revision_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join("compat-agent")
        .join(revision_id);
    assert!(revision_dir.join("SKILL.md").exists());
    assert!(revision_dir.join("runtime.lock").exists());
    assert!(revision_dir.join("main.py").exists());

    let skill_text = std::fs::read_to_string(revision_dir.join("SKILL.md")).unwrap();
    let (parsed_manifest, _body) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&skill_text).unwrap();
    assert_eq!(parsed_manifest.agent.id, "compat-agent");
    let import = parsed_manifest
        .agentskills_import
        .clone()
        .expect("agentskills import metadata should be preserved");
    assert_eq!(import.license.as_deref(), Some("MIT"));
    assert!(import.allowed_tools.iter().any(|t| t == "WebFetch"));
    assert!(
        parsed_manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. })),
        "allowed-tools WebFetch should infer NetworkAccess"
    );
}

#[tokio::test]
async fn test_revision_create_from_intent_live_openrouter_smoke_if_key_available(
) -> anyhow::Result<()> {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("Skipping live smoke test: OPENROUTER_API_KEY not set");
        return Ok(());
    }

    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::llm::Message;
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
    use autonoetic_types::artifact::ArtifactKind;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let session_id = "intent-live-openrouter";

    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();
    let handle = content_store
        .write(b"print('runtime artifact for reasoning mode')".as_ref())
        .unwrap();
    content_store
        .register_name(session_id, "main.py", &handle)
        .unwrap();
    let bundle = artifact_store
        .build_with_kind(
            &["main.py".to_string()],
            Some(&["main.py".to_string()]),
            None,
            ArtifactKind::AgentBundle,
            session_id,
        )
        .unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["live.intent.agent*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateFromIntentTool;
    let args = json!({
        "agent_id": "live.intent.agent",
        "artifact_id": bundle.artifact_id,
        "instructions": "# Live Intent Agent\n\nYou are concise. Reply with only the final number.",
        "description": "Live OpenRouter smoke agent",
        "capabilities": [{"type": "ReadAccess", "scopes": ["self.*"]}],
        "execution_mode": "reasoning",
        "llm_config": {
            "provider": "openrouter",
            "model": "google/gemini-3-flash-preview",
            "temperature": 0.0,
            "fallback_provider": null,
            "fallback_model": null,
            "chat_only": false
        }
    });

    let out = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        Some(gateway_dir.as_path()),
        &args.to_string(),
        None,
        None,
        None,
        Some(store.clone()),
        None,
    )?;
    let parsed: serde_json::Value = serde_json::from_str(&out)?;
    let revision_id = parsed["revision_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("revision_id missing in response"))?;

    let revision_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join("live.intent.agent")
        .join(revision_id);
    let skill_text = std::fs::read_to_string(revision_dir.join("SKILL.md"))?;
    let (skill_manifest, instructions) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&skill_text)?;

    let llm_cfg = skill_manifest
        .llm_config
        .clone()
        .ok_or_else(|| anyhow::anyhow!("llm_config missing from canonical skill"))?;
    let driver = autonoetic_gateway::llm::build_driver(llm_cfg, reqwest::Client::new())?;

    let agent_dir = tmp.path().join("live-agent-runtime");
    std::fs::create_dir_all(&agent_dir)?;

    let prompt = "What is 2 + 2? Reply with only the number.";
    let mut history = vec![Message::user(prompt.to_string())];
    let mut executor = AgentExecutor::new(
        skill_manifest,
        instructions,
        driver,
        agent_dir,
        autonoetic_gateway::runtime::tools::default_registry(),
        None,
    )
    .with_initial_user_message(prompt.to_string())
    .with_session_id("live-openrouter-smoke-session".to_string());

    let result = executor.execute_with_history(&mut history).await?;
    let reply = match result {
        TurnOutcome::Completed(Some(text)) => text,
        TurnOutcome::Completed(None) => String::new(),
        other => anyhow::bail!("unexpected turn outcome for live smoke: {:?}", other),
    };
    assert!(
        reply.contains('4'),
        "Expected live reply to include 4, got: {}",
        reply
    );

    Ok(())
}

#[test]
fn test_revision_create_rejects_non_agent_bundle_kind() {
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::runtime::content_store::ContentStore;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let session_id = "test-session";

    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();

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
  id: "kind.check"
  name: "Kind Check"
  description: "Kind check"
---
# Kind Check
"#;
    let runtime_lock = r#"gateway:
  artifact: "gateway"
  version: "0.1.0"
  sha256: "sha256:gateway"
sdk:
  version: "0.1.0"
sandbox:
  backend: "bubblewrap"
dependencies: []
artifacts: []
layers: []
"#;
    for (name, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", b"print('hello')".as_ref()),
    ] {
        let handle = content_store.write(content).unwrap();
        content_store
            .register_name(session_id, name, &handle)
            .unwrap();
    }
    // Intentionally build with default kind ("binary") and NO entrypoints.
    let bundle = artifact_store
        .build(
            &["SKILL.md".to_string(), "runtime.lock".to_string()],
            None,
            None,
            session_id,
        )
        .unwrap();

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["kind.check*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let create_tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateTool;
    let err = create_tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &json!({
                "agent_id": "kind.check",
                "artifact_id": bundle.artifact_id,
            })
            .to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect_err(
            "revision creation must require artifact kind agent_bundle or binary with entrypoint",
        );
    assert!(
        err.to_string().contains("requires kind 'agent_bundle'")
            || err.to_string().contains("entrypoint")
    );
}

#[test]
fn test_candidate_revision_runs_without_alias_via_explicit_agent_ref() {
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_types::artifact::ArtifactKind;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let session_id = "test-session";

    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();

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
  id: "candidate.noalias"
  name: "Candidate No Alias"
  description: "Candidate without alias"
---
# Candidate No Alias
"#;
    let runtime_lock = r#"gateway:
  artifact: "gateway"
  version: "0.1.0"
  sha256: "sha256:gateway"
sdk:
  version: "0.1.0"
sandbox:
  backend: "bubblewrap"
dependencies: []
artifacts: []
layers: []
"#;

    for (name, content) in [
        ("SKILL.md", skill_md.as_bytes()),
        ("runtime.lock", runtime_lock.as_bytes()),
        ("main.py", b"print('hello')".as_ref()),
    ] {
        let handle = content_store.write(content).unwrap();
        content_store
            .register_name(session_id, name, &handle)
            .unwrap();
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

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["candidate.noalias*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let create_tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateTool;
    let created_json = create_tool
        .execute(
            &manifest,
            &policy,
            Path::new("/tmp"),
            Some(gateway_dir.as_path()),
            &json!({
                "agent_id": "candidate.noalias",
                "artifact_id": bundle.artifact_id,
            })
            .to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .unwrap();
    let created: serde_json::Value = serde_json::from_str(&created_json).unwrap();
    let revision_id = created["revision_id"].as_str().unwrap().to_string();
    let agent_ref = format!("candidate.noalias@{}", revision_id);

    assert!(
        store.resolve_alias("candidate.noalias").unwrap().is_none(),
        "candidate should be runnable without alias"
    );

    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: tmp.path().join("agents").join(".gateway"),
        agents_dir: tmp.path().join("agents"),
        ..Default::default()
    };
    let repo = autonoetic_gateway::agent::repository::AgentRepository::from_config(&config);
    let (resolved_ref, _, binding) = repo
        .resolve_and_pin_session(
            "session-candidate-noalias",
            "session-candidate-noalias",
            &agent_ref,
            Some(store.as_ref()),
            "host:test",
        )
        .unwrap();
    assert_eq!(resolved_ref.revision_id, revision_id);
    assert_eq!(binding.alias_id, None);
}

#[test]
fn test_changing_pinned_layer_mounts_changes_revision_identity() {
    use autonoetic_gateway::artifact_store::ArtifactStore;
    use autonoetic_gateway::layer_store::{LayerLimits, LayerStore};
    use autonoetic_gateway::runtime::content_store::ContentStore;
    use autonoetic_types::artifact::ArtifactKind;
    use autonoetic_types::layer::ArtifactLayer;

    let tmp = TempDir::new().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    let content_store = ContentStore::new(&gateway_dir).unwrap();
    let artifact_store = ArtifactStore::new(&gateway_dir).unwrap();
    let layer_store = LayerStore::new(&gateway_dir, LayerLimits::default()).unwrap();

    let layer_src = tmp.path().join("layer-src");
    std::fs::create_dir_all(layer_src.join("pkg")).unwrap();
    std::fs::write(layer_src.join("pkg/__init__.py"), b"# package").unwrap();
    let captured = layer_store
        .create_from_dir(&layer_src, "deps", "/opt/deps", None)
        .unwrap();

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
  id: "layer.identity"
  name: "Layer Identity"
  description: "Layer identity test"
---
# Layer Identity
"#;
    let main_py = b"print('same app')";
    let runtime_lock_a = format!(
        "gateway:\n  artifact: \"gateway\"\n  version: \"0.1.0\"\n  sha256: \"sha256:gateway\"\nsdk:\n  version: \"0.1.0\"\nsandbox:\n  backend: \"bubblewrap\"\ndependencies: []\nartifacts: []\nlayers:\n  - layer_id: \"{}\"\n    digest: \"{}\"\n    mount_path: \"/opt/deps-a\"\n",
        captured.layer_id, captured.digest
    );
    let runtime_lock_b = format!(
        "gateway:\n  artifact: \"gateway\"\n  version: \"0.1.0\"\n  sha256: \"sha256:gateway\"\nsdk:\n  version: \"0.1.0\"\nsandbox:\n  backend: \"bubblewrap\"\ndependencies: []\nartifacts: []\nlayers:\n  - layer_id: \"{}\"\n    digest: \"{}\"\n    mount_path: \"/opt/deps-b\"\n",
        captured.layer_id, captured.digest
    );

    let build_bundle = |session_id: &str, runtime_lock_content: &str, mount_path: &str| -> String {
        let entries = vec![
            ("SKILL.md".to_string(), skill_md.as_bytes().to_vec()),
            (
                "runtime.lock".to_string(),
                runtime_lock_content.as_bytes().to_vec(),
            ),
            ("main.py".to_string(), main_py.to_vec()),
        ];
        for (name, bytes) in entries {
            let handle = content_store.write(&bytes).unwrap();
            content_store
                .register_name(session_id, &name, &handle)
                .unwrap();
        }
        let layers = vec![ArtifactLayer {
            layer_id: captured.layer_id.clone(),
            name: captured.name.clone(),
            mount_path: mount_path.to_string(),
            digest: captured.digest.clone(),
        }];
        artifact_store
            .build_with_kind(
                &[
                    "SKILL.md".to_string(),
                    "runtime.lock".to_string(),
                    "main.py".to_string(),
                ],
                Some(&["main.py".to_string()]),
                Some(&layers),
                ArtifactKind::AgentBundle,
                session_id,
            )
            .unwrap()
            .artifact_id
    };

    assert_ne!(runtime_lock_a, runtime_lock_b);
    let artifact_a = build_bundle("test-session-a", &runtime_lock_a, "/opt/deps-a");
    let artifact_b = build_bundle("test-session-b", &runtime_lock_b, "/opt/deps-b");
    assert_ne!(
        artifact_a, artifact_b,
        "artifacts should differ when runtime.lock differs"
    );

    let manifest = manifest_with_capabilities(vec![Capability::AgentRevision {
        patterns: vec!["layer.identity*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let create_tool = autonoetic_gateway::runtime::tools::AgentRevisionCreateTool;

    let created_a: serde_json::Value = serde_json::from_str(
        &create_tool
            .execute(
                &manifest,
                &policy,
                Path::new("/tmp"),
                Some(gateway_dir.as_path()),
                &json!({"agent_id": "layer.identity", "artifact_id": artifact_a}).to_string(),
                None,
                None,
                None,
                Some(store.clone()),
                None,
            )
            .unwrap(),
    )
    .unwrap();
    let created_b: serde_json::Value = serde_json::from_str(
        &create_tool
            .execute(
                &manifest,
                &policy,
                Path::new("/tmp"),
                Some(gateway_dir.as_path()),
                &json!({"agent_id": "layer.identity", "artifact_id": artifact_b}).to_string(),
                None,
                None,
                None,
                Some(store.clone()),
                None,
            )
            .unwrap(),
    )
    .unwrap();
    assert_ne!(created_a["status"], json!("already_exists"));
    assert_ne!(created_b["status"], json!("already_exists"));

    assert_ne!(
        created_a["revision_id"].as_str().unwrap(),
        created_b["revision_id"].as_str().unwrap(),
        "changing pinned layer mounts must change revision identity"
    );
}

#[test]
fn test_load_from_revision_dir_fails_when_missing() {
    let tmp = TempDir::new().unwrap();
    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        ..Default::default()
    };
    let repo = autonoetic_gateway::agent::repository::AgentRepository::from_config(&config);

    let gateway_dir = tmp.path().join(".gateway");
    let result = repo.load_from_revision_dir(&gateway_dir, "nonexistent-agent", "rev_abc123");
    assert!(result.is_err());
}

#[test]
fn test_validate_suite_spec_rejects_duplicate_case_ids() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![
            EvalSuiteCaseSpec {
                case_id: "case_a".into(),
                message: "Hello".into(),
                assertions: json!({ "reply_max_chars": 100 }),
            },
            EvalSuiteCaseSpec {
                case_id: "case_a".into(),
                message: "World".into(),
                assertions: json!({ "reply_max_chars": 200 }),
            },
        ],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Duplicate case_id"));
}

#[test]
fn test_validate_suite_spec_rejects_empty_case_id() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "  ".into(),
            message: "Hello".into(),
            assertions: json!({ "reply_max_chars": 100 }),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("must not be empty"));
}

#[test]
fn test_validate_suite_spec_rejects_empty_message() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "  ".into(),
            assertions: json!({ "reply_max_chars": 100 }),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("message must not be empty"));
}

#[test]
fn test_validate_suite_spec_rejects_non_object_assertions() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "Hello".into(),
            assertions: json!(["reply_max_chars"]),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("must be an object"));
}

#[test]
fn test_validate_suite_spec_rejects_unknown_assertion_type() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "Hello".into(),
            assertions: json!({ "unknown_assertion": true }),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("unknown assertion type"));
}

#[test]
fn test_validate_suite_spec_rejects_vacuous_case_no_assertions() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "Hello".into(),
            assertions: json!({}),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("must have at least one assertion"));
}

#[test]
fn test_validate_suite_spec_rejects_empty_contains_all() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "Hello".into(),
            assertions: json!({ "reply_contains_all": [] }),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("at least one substring"));
}

#[test]
fn test_validate_suite_spec_rejects_empty_contains_any() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "Hello".into(),
            assertions: json!({ "reply_contains_any": [] }),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("at least one substring"));
}

#[test]
fn test_validate_suite_spec_accepts_reply_contains_any() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "Hello".into(),
            assertions: json!({ "reply_contains_any": ["propose_amendment", "delegate"] }),
        }],
    };
    validate_suite_spec(&spec).unwrap();
}

#[test]
fn test_validate_suite_spec_accepts_valid_suite() {
    use autonoetic_gateway::runtime::tools::{
        validate_suite_spec, EvalSuiteCaseSpec, EvalSuiteSpec,
    };

    let spec = EvalSuiteSpec {
        cases: vec![
            EvalSuiteCaseSpec {
                case_id: "case_a".into(),
                message: "Summarize this task".into(),
                assertions: json!({ "reply_max_chars": 200 }),
            },
            EvalSuiteCaseSpec {
                case_id: "case_b".into(),
                message: "Route to specialist".into(),
                assertions: json!({
                    "reply_contains_all": ["task"],
                    "reply_max_chars": 500
                }),
            },
        ],
    };
    let result = validate_suite_spec(&spec);
    assert!(
        result.is_ok(),
        "Valid suite should pass validation: {:?}",
        result.err()
    );
}

// ─────────────────────────────────────────────────────────────────────
// 2. Evaluation capability scope enforcement
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_can_evaluate_suite_publish_matches_suite_name_prefix() {
    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["suite-basic*".into()],
    }]);
    let policy = PolicyEngine::new(manifest);

    assert!(policy
        .can_evaluate_suite_publish("suite-basic-planner")
        .is_allowed());
    assert!(policy
        .can_evaluate_suite_publish("suite-basic-")
        .is_allowed());
    assert!(!policy
        .can_evaluate_suite_publish("suite-advanced")
        .is_allowed());
    assert!(!policy
        .can_evaluate_suite_publish("other-suite")
        .is_allowed());
}

#[test]
fn test_can_evaluate_suite_publish_wildcard() {
    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest);

    assert!(policy
        .can_evaluate_suite_publish("any-suite-name")
        .is_allowed());
    assert!(policy.can_evaluate_suite_publish("suite-x").is_allowed());
}

#[test]
fn test_can_evaluate_suite_matches_suite_or_agent() {
    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["planner*".into()],
    }]);
    let policy = PolicyEngine::new(manifest);

    assert!(policy
        .can_evaluate_suite("planner-suite", "other-agent")
        .is_allowed());
    assert!(policy
        .can_evaluate_suite("other-suite", "planner-agent")
        .is_allowed());
    assert!(!policy
        .can_evaluate_suite("other-suite", "other-agent")
        .is_allowed());
}

#[test]
fn test_can_evaluate_suite_empty_subject_agent_still_matches_suite() {
    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["suite-*".into()],
    }]);
    let policy = PolicyEngine::new(manifest);

    assert!(policy.can_evaluate_suite("suite-abc", "").is_allowed());
    assert!(!policy.can_evaluate_suite("other-abc", "").is_allowed());
}

// ─────────────────────────────────────────────────────────────────────
// 3. Eval runner assertion engine
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_assertion_reply_contains_all() {
    let assertions = autonoetic_gateway::scheduler::eval_runner::EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: Some(vec!["task".into(), "summary".into()]),
        reply_contains_none: None,
        reply_max_chars: None,
        artifacts_min: None,
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };

    assert!(
        autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(
            &assertions,
            "Here is the task summary",
            0
        )
    );

    assert!(
        !autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(
            &assertions,
            "Here is the task",
            0
        )
    );
}

#[test]
fn test_assertion_reply_contains_none() {
    let assertions = autonoetic_gateway::scheduler::eval_runner::EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: None,
        reply_contains_none: Some(vec!["error".into(), "failed".into()]),
        reply_max_chars: None,
        artifacts_min: None,
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };

    assert!(
        autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(
            &assertions,
            "Success! All tests passed.",
            0
        )
    );

    assert!(
        !autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(
            &assertions,
            "The operation failed with an error",
            0
        )
    );
}

#[test]
fn test_assertion_reply_max_chars() {
    let assertions = autonoetic_gateway::scheduler::eval_runner::EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: None,
        reply_contains_none: None,
        reply_max_chars: Some(10),
        artifacts_min: None,
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };

    assert!(
        autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(
            &assertions,
            "Short text",
            0
        )
    );

    assert!(
        !autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(
            &assertions,
            "This is a much longer text",
            0
        )
    );
}

#[test]
fn test_assertion_artifacts_min() {
    let assertions = autonoetic_gateway::scheduler::eval_runner::EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: None,
        reply_contains_none: None,
        reply_max_chars: None,
        artifacts_min: Some(2),
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };

    assert!(autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(&assertions, "", 3));

    assert!(!autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(&assertions, "", 1));
}

#[test]
fn test_assertion_artifacts_max() {
    let assertions = autonoetic_gateway::scheduler::eval_runner::EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: None,
        reply_contains_none: None,
        reply_max_chars: None,
        artifacts_min: None,
        artifacts_max: Some(5),
        session_events_min: None,
        session_events_max: None,
    };

    assert!(autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(&assertions, "", 3));

    assert!(!autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(&assertions, "", 10));
}

#[test]
fn test_assertion_combined_all_pass() {
    let assertions = autonoetic_gateway::scheduler::eval_runner::EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: Some(vec!["OK".into()]),
        reply_contains_none: Some(vec!["error".into()]),
        reply_max_chars: Some(100),
        artifacts_min: Some(1),
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };

    assert!(
        autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(&assertions, "OK, done", 2)
    );
}

#[test]
fn test_assertion_combined_partial_fail() {
    let assertions = autonoetic_gateway::scheduler::eval_runner::EvalAssertions {
        reply_contains_any: None,
        reply_contains_all: Some(vec!["OK".into()]),
        reply_contains_none: Some(vec!["error".into()]),
        reply_max_chars: Some(10),
        artifacts_min: None,
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };

    assert!(
        !autonoetic_gateway::scheduler::eval_runner::evaluate_assertions(
            &assertions,
            "OK, this is a very long response that exceeds the limit",
            0
        )
    );
}

// ─────────────────────────────────────────────────────────────────────
// 3b. Gateway-state (session event) assertions — #772 E.1
// ─────────────────────────────────────────────────────────────────────

fn causal_event(category: &str, action: &str) -> autonoetic_types::causal_chain::CausalEventRecord {
    autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("ev-{}-{}", category, action),
        agent_id: "subject.default".to_string(),
        session_id: "eval-run-1-deadbeef".to_string(),
        turn_id: None,
        event_seq: 0,
        timestamp: "2026-07-16T00:00:00Z".to_string(),
        category: category.to_string(),
        action: action.to_string(),
        status: "ok".to_string(),
        enforced_rules: vec![],
        target: None,
        payload: None,
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    }
}

#[test]
fn test_assertion_session_events_min_and_max() {
    use autonoetic_gateway::scheduler::eval_runner::{
        evaluate_session_event_assertions, EvalAssertions, SessionEventAssertion,
    };

    let events = vec![
        causal_event("anomaly_flag", "filed"),
        causal_event("agent", "spawned"),
        causal_event("agent", "spawned"),
    ];

    let no_session_assertions = EvalAssertions {
        reply_contains_all: None,
        reply_contains_any: None,
        reply_contains_none: None,
        reply_max_chars: None,
        artifacts_min: None,
        artifacts_max: None,
        session_events_min: None,
        session_events_max: None,
    };
    assert!(evaluate_session_event_assertions(&no_session_assertions, &events).is_empty());

    // min satisfied.
    let min_ok = EvalAssertions {
        session_events_min: Some(vec![SessionEventAssertion {
            category: "anomaly_flag".into(),
            action: Some("filed".into()),
            count: 1,
        }]),
        ..no_session_assertions.clone()
    };
    assert!(evaluate_session_event_assertions(&min_ok, &events).is_empty());

    // min violated: 1 filed event < required 2.
    let min_missing = EvalAssertions {
        session_events_min: Some(vec![SessionEventAssertion {
            category: "anomaly_flag".into(),
            action: Some("filed".into()),
            count: 2,
        }]),
        ..no_session_assertions.clone()
    };
    let failures = evaluate_session_event_assertions(&min_missing, &events);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0].contains("anomaly_flag.filed: 1 < 2"),
        "got: {}",
        failures[0]
    );

    // action: None matches any action within the category.
    let category_wide = EvalAssertions {
        session_events_min: Some(vec![SessionEventAssertion {
            category: "agent".into(),
            action: None,
            count: 2,
        }]),
        ..no_session_assertions.clone()
    };
    assert!(evaluate_session_event_assertions(&category_wide, &events).is_empty());

    // max with count 0 forbids the event entirely.
    let forbidden_hit = EvalAssertions {
        session_events_max: Some(vec![SessionEventAssertion {
            category: "anomaly_flag".into(),
            action: Some("filed".into()),
            count: 0,
        }]),
        ..no_session_assertions.clone()
    };
    let failures = evaluate_session_event_assertions(&forbidden_hit, &events);
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0].contains("anomaly_flag.filed: 1 > 0"),
        "got: {}",
        failures[0]
    );

    // Forbidden event genuinely absent → passes.
    let forbidden_absent = EvalAssertions {
        session_events_max: Some(vec![SessionEventAssertion {
            category: "workflow_wait".into(),
            action: None,
            count: 0,
        }]),
        ..no_session_assertions
    };
    assert!(evaluate_session_event_assertions(&forbidden_absent, &events).is_empty());
}

#[test]
fn test_validate_suite_spec_accepts_session_event_assertions() {
    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "planted-anomaly".into(),
            message: "Review this child evaluator output and report anything unexpected.".into(),
            assertions: json!({
                "session_events_min": [{"category": "anomaly_flag", "action": "filed", "count": 1}],
                "session_events_max": [{"category": "workflow_wait", "count": 0}]
            }),
        }],
    };
    assert!(validate_suite_spec(&spec).is_ok());
}

#[test]
fn test_validate_suite_spec_rejects_bad_session_event_assertions() {
    let spec_with = |assertions: serde_json::Value| EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "c1".into(),
            message: "m".into(),
            assertions,
        }],
    };

    // A min of 0 is vacuous — rejected.
    let r = validate_suite_spec(&spec_with(json!({
        "session_events_min": [{"category": "anomaly_flag", "count": 0}]
    })));
    assert!(r.unwrap_err().to_string().contains(">= 1"));

    // Empty array — rejected.
    let r = validate_suite_spec(&spec_with(json!({ "session_events_min": [] })));
    assert!(r.unwrap_err().to_string().contains("at least one entry"));

    // Missing category — rejected.
    let r = validate_suite_spec(&spec_with(json!({
        "session_events_max": [{"count": 1}]
    })));
    assert!(r.unwrap_err().to_string().contains("category"));

    // Not an array — rejected.
    let r = validate_suite_spec(&spec_with(json!({
        "session_events_max": {"category": "x", "count": 1}
    })));
    assert!(r.unwrap_err().to_string().contains("must be an array"));
}

// ─────────────────────────────────────────────────────────────────────
// 4. Tool execution integration tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_eval_suite_publish_tool_execution() {
    let tool = EvalSuitePublishTool;
    let manifest = manifest_with_capabilities(vec![Capability::Evaluation {
        patterns: vec!["test-suite*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let args = json!({
        "name": "test-suite-basic",
        "description": "Basic test suite",
        "spec": {
            "cases": [
                {
                    "case_id": "case_a",
                    "message": "Hello world",
                    "assertions": { "reply_max_chars": 200 }
                }
            ]
        }
    });

    let args_json = args.to_string();
    let result = tool.execute(
        &manifest,
        &policy,
        Path::new("/tmp"),
        None,
        &args_json,
        None,
        None,
        None,
        None,
        None,
    );

    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("GatewayStore is required"));
}

#[test]
fn test_eval_suite_publish_rejects_without_capability() {
    let manifest = manifest_with_capabilities(vec![]);
    let _policy = PolicyEngine::new(manifest.clone());

    let tool = EvalSuitePublishTool;
    assert!(!tool.is_available(&manifest));
}

#[test]
fn test_eval_run_tool_requires_capability() {
    let manifest = manifest_with_capabilities(vec![]);

    let tool = EvalRunTool;
    assert!(!tool.is_available(&manifest));
}

#[test]
fn test_eval_report_tool_requires_capability() {
    let manifest = manifest_with_capabilities(vec![]);

    let tool = EvalReportTool;
    assert!(!tool.is_available(&manifest));
}

#[test]
fn test_eval_tools_available_with_evaluation_capability() {
    let caps = vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }];
    let manifest = manifest_with_capabilities(caps);

    assert!(EvalSuitePublishTool.is_available(&manifest));
    assert!(EvalRunTool.is_available(&manifest));
    assert!(EvalReportTool.is_available(&manifest));
}

#[test]
fn test_validate_suite_spec_accepts_multiple_assertions_per_case() {
    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "multi_assert".into(),
            message: "Do something".into(),
            assertions: json!({
                "reply_contains_all": ["task"],
                "reply_max_chars": 500,
                "artifacts_min": 1
            }),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(
        result.is_ok(),
        "Multiple assertions per case should be valid: {:?}",
        result.err()
    );
}

#[test]
fn test_validate_suite_spec_rejects_empty_reply_contains_none() {
    let spec = EvalSuiteSpec {
        cases: vec![EvalSuiteCaseSpec {
            case_id: "case_a".into(),
            message: "Hello".into(),
            assertions: json!({ "reply_contains_none": [] }),
        }],
    };
    let result = validate_suite_spec(&spec);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("at least one substring"));
}
