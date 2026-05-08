//! Integration tests: eval suite ownership model (issue #32).
//!
//! Tests cover:
//!   1. Publish rejects self-referencing evaluated_targets
//!   2. Publish allows different agent as target
//!   3. Update rejects self-referencing evaluated_targets
//!   4. Update requires valid based_on_suite_id
//!   5. Lineage recorded correctly across publish → update chain
//!   6. Store query: list suites authored by a given agent
//!   7. Store query: list suites targeting a given agent
//!   8. Backward-compat: old suites without ownership columns decode correctly

mod support;

use autonoetic_gateway::runtime::tools::{
    EvalSuitePublishTool, EvalSuiteUpdateTool, NativeTool,
};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

// ─── helpers ──────────────────────────────────────────────────────────────────

fn manifest_with_id(agent_id: &str) -> AgentManifest {
    use autonoetic_types::capability::Capability;
    let caps = serde_json::to_string(&vec![Capability::Evaluation {
        patterns: vec!["*".to_string()],
    }])
    .unwrap();
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
  id: "{agent_id}"
  name: "Test Agent"
  description: "Test."
capabilities: {caps}
llm_config:
  provider: "openai"
  model: "test-model"
---
# Test Agent
"#
    );
    let (manifest, _) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&yaml).unwrap();
    manifest
}

fn open_store(tmp: &TempDir) -> Arc<GatewayStore> {
    Arc::new(GatewayStore::open(tmp.path()).unwrap())
}

fn policy_for(manifest: &AgentManifest) -> PolicyEngine {
    PolicyEngine::new(manifest.clone())
}

fn one_case_args(name: &str, targets: &[&str]) -> String {
    json!({
        "name": name,
        "description": "desc",
        "spec": {
            "cases": [{
                "case_id": "c1",
                "message": "hello",
                "assertions": { "reply_contains_all": ["ok"] }
            }]
        },
        "evaluated_targets": targets,
    })
    .to_string()
}

fn run_publish(store: Arc<GatewayStore>, manifest: &AgentManifest, args: &str) -> anyhow::Result<String> {
    let policy = policy_for(manifest);
    EvalSuitePublishTool.execute(
        manifest,
        &policy,
        Path::new("/tmp"),
        None,
        args,
        None,
        None,
        None,
        Some(store),
        None,
    )
}

fn run_update(store: Arc<GatewayStore>, manifest: &AgentManifest, args: &str) -> anyhow::Result<String> {
    let policy = policy_for(manifest);
    EvalSuiteUpdateTool.execute(
        manifest,
        &policy,
        Path::new("/tmp"),
        None,
        args,
        None,
        None,
        None,
        Some(store),
        None,
    )
}

// ─── 1. Publish rejects self-referencing targets ───────────────────────────

#[test]
fn publish_rejects_self_as_evaluated_target() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let manifest = manifest_with_id("coder-agent");

    let args = one_case_args("coder-suite", &["coder-agent"]);
    let result = run_publish(store, &manifest, &args);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Ownership violation"), "expected ownership error, got: {err}");
    assert!(err.contains("coder-agent"), "error should name the agent: {err}");
}

// ─── 2. Publish allows different agent as target ───────────────────────────

#[test]
fn publish_allows_different_agent_as_target() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let manifest = manifest_with_id("eval-curator");

    let args = one_case_args("coder-suite", &["coder-agent"]);
    let result = run_publish(store, &manifest, &args);
    assert!(result.is_ok(), "expected success, got: {:?}", result);

    let v: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(v["status"], "published");
    assert_eq!(v["evaluated_targets"], json!(["coder-agent"]));
}

// ─── 3. Publish with empty targets is allowed ─────────────────────────────

#[test]
fn publish_with_no_targets_is_allowed() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let manifest = manifest_with_id("eval-curator");

    let args = one_case_args("generic-suite", &[]);
    let result = run_publish(store, &manifest, &args);
    assert!(result.is_ok(), "{:?}", result);
}

// ─── 4. Update rejects self-referencing targets ───────────────────────────

#[test]
fn update_rejects_self_as_evaluated_target() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let curator = manifest_with_id("eval-curator");

    // Publish a valid v1.
    let publish_args = one_case_args("coder-suite", &["coder-agent"]);
    let published: serde_json::Value =
        serde_json::from_str(&run_publish(Arc::clone(&store), &curator, &publish_args).unwrap()).unwrap();
    let v1_id = published["suite_id"].as_str().unwrap().to_string();

    // Now a different (bad) agent tries to update and list itself as target.
    let bad_manifest = manifest_with_id("coder-agent");
    let update_args = json!({
        "based_on_suite_id": v1_id,
        "name": "coder-suite-v2",
        "description": "updated",
        "spec": {
            "cases": [{"case_id": "c1", "message": "hi", "assertions": {"reply_contains_all": ["ok"]}}]
        },
        "evaluated_targets": ["coder-agent"],
    })
    .to_string();

    let result = run_update(store, &bad_manifest, &update_args);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Ownership violation"), "expected ownership error, got: {err}");
}

// ─── 5. Update requires valid based_on_suite_id ───────────────────────────

#[test]
fn update_rejects_missing_based_on_suite_id() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let curator = manifest_with_id("eval-curator");

    let update_args = json!({
        "based_on_suite_id": "suite-doesnotexist",
        "name": "updated",
        "description": "desc",
        "spec": {
            "cases": [{"case_id": "c1", "message": "hi", "assertions": {"reply_contains_all": ["ok"]}}]
        },
        "evaluated_targets": ["coder-agent"],
    })
    .to_string();

    let result = run_update(store, &curator, &update_args);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not found"), "expected not-found error, got: {err}");
}

// ─── 6. Lineage recorded across publish → update ──────────────────────────

#[test]
fn update_records_lineage_link() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let curator = manifest_with_id("eval-curator");

    // Publish v1.
    let v1_args = one_case_args("coder-suite", &["coder-agent"]);
    let v1: serde_json::Value =
        serde_json::from_str(&run_publish(Arc::clone(&store), &curator, &v1_args).unwrap()).unwrap();
    let v1_id = v1["suite_id"].as_str().unwrap().to_string();

    // Update to v2.
    let v2_args = json!({
        "based_on_suite_id": v1_id,
        "name": "coder-suite-v2",
        "description": "improved",
        "spec": {
            "cases": [
                {"case_id": "c1", "message": "hello", "assertions": {"reply_contains_all": ["ok"]}},
                {"case_id": "c2", "message": "code it", "assertions": {"artifacts_min": 1}}
            ]
        },
        "evaluated_targets": ["coder-agent"],
    })
    .to_string();
    let v2: serde_json::Value =
        serde_json::from_str(&run_update(Arc::clone(&store), &curator, &v2_args).unwrap()).unwrap();
    let v2_id = v2["suite_id"].as_str().unwrap().to_string();
    assert_eq!(v2["based_on_suite_id"], json!(v1_id));

    // Check DB: v2 has based_on_suite_id pointing to v1.
    let fetched = store.get_eval_suite(&v2_id).unwrap().unwrap();
    assert_eq!(fetched.based_on_suite_id.as_deref(), Some(v1_id.as_str()));
    assert_eq!(fetched.evaluated_targets, vec!["coder-agent"]);
    assert_eq!(fetched.author_agent_id.as_deref(), Some("eval-curator"));
}

// ─── 7. Store: list suites authored by agent ──────────────────────────────

#[test]
fn store_list_suites_authored_by() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let curator = manifest_with_id("eval-curator");
    let other = manifest_with_id("other-agent");

    run_publish(Arc::clone(&store), &curator, &one_case_args("suite-a", &["target-a"])).unwrap();
    run_publish(Arc::clone(&store), &curator, &one_case_args("suite-b", &["target-b"])).unwrap();
    run_publish(Arc::clone(&store), &other, &one_case_args("suite-c", &["target-c"])).unwrap();

    let by_curator = store.list_eval_suites_authored_by("eval-curator").unwrap();
    assert_eq!(by_curator.len(), 2);
    assert!(by_curator.iter().all(|s| s.author_agent_id.as_deref() == Some("eval-curator")));

    let by_other = store.list_eval_suites_authored_by("other-agent").unwrap();
    assert_eq!(by_other.len(), 1);
}

// ─── 8. Store: list suites targeting a given agent ────────────────────────

#[test]
fn store_list_suites_targeting_agent() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let curator = manifest_with_id("eval-curator");

    run_publish(Arc::clone(&store), &curator, &one_case_args("s1", &["coder-agent", "researcher-agent"])).unwrap();
    run_publish(Arc::clone(&store), &curator, &one_case_args("s2", &["coder-agent"])).unwrap();
    run_publish(Arc::clone(&store), &curator, &one_case_args("s3", &["architect-agent"])).unwrap();

    let for_coder = store.list_eval_suites_targeting_agent("coder-agent").unwrap();
    assert_eq!(for_coder.len(), 2, "expected 2 suites targeting coder-agent");

    let for_arch = store.list_eval_suites_targeting_agent("architect-agent").unwrap();
    assert_eq!(for_arch.len(), 1);

    let for_unknown = store.list_eval_suites_targeting_agent("unknown-agent").unwrap();
    assert!(for_unknown.is_empty());
}

// ─── 9. author_agent_id persisted and readable ────────────────────────────

#[test]
fn author_agent_id_is_set_by_gateway_not_caller() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let curator = manifest_with_id("eval-curator");

    let args = one_case_args("my-suite", &["coder-agent"]);
    let result: serde_json::Value =
        serde_json::from_str(&run_publish(Arc::clone(&store), &curator, &args).unwrap()).unwrap();
    let suite_id = result["suite_id"].as_str().unwrap();

    let suite = store.get_eval_suite(suite_id).unwrap().unwrap();
    // author_agent_id is set from manifest.agent.id by the gateway, not from the args payload.
    assert_eq!(suite.author_agent_id.as_deref(), Some("eval-curator"));
    assert_eq!(suite.based_on_suite_id, None);
}
