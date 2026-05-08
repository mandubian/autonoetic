//! Integration tests: Phase 7 — red-team agent and adversarial co-evolution.
//!
//! Tests cover:
//!   1. attack_pattern_propose succeeds for a valid SecurityRedTeam agent
//!   2. attack_pattern_propose rejects unknown category
//!   3. attack_pattern_propose rejects agent without SecurityRedTeam capability
//!   4. Structural separation: propose rejects agent that authors eval suites targeting itself
//!   5. attack_pattern_list filters by status
//!   6. Store: review_attack_pattern (accept + reject)
//!   7. Store: review_attack_pattern on unknown ID returns error
//!   8. AttackPatternStatus Display round-trips

mod support;

use autonoetic_gateway::runtime::tools::{
    AttackPatternListTool, AttackPatternProposeTool, NativeTool,
};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::capability::Capability;
use autonoetic_types::security::AttackPatternStatus;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

// ─── helpers ──────────────────────────────────────────────────────────────────

fn redteam_manifest(agent_id: &str) -> autonoetic_types::agent::AgentManifest {
    let caps = serde_json::to_string(&vec![
        Capability::SecurityRedTeam,
        Capability::ReadAccess { scopes: vec!["*".into()] },
    ])
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
  name: "Red Team"
  description: "Red team agent."
capabilities: {caps}
llm_config:
  provider: "openai"
  model: "test-model"
---
# Red Team
"#
    );
    let (m, _) = autonoetic_gateway::runtime::parser::SkillParser::parse(&yaml).unwrap();
    m
}

fn eval_curator_manifest(agent_id: &str) -> autonoetic_types::agent::AgentManifest {
    let caps = serde_json::to_string(&vec![Capability::Evaluation {
        patterns: vec!["*".into()],
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
  name: "Curator"
  description: "Eval curator."
capabilities: {caps}
llm_config:
  provider: "openai"
  model: "test-model"
---
# Curator
"#
    );
    let (m, _) = autonoetic_gateway::runtime::parser::SkillParser::parse(&yaml).unwrap();
    m
}

fn open_store(tmp: &TempDir) -> Arc<GatewayStore> {
    Arc::new(GatewayStore::open(tmp.path()).unwrap())
}

fn valid_propose_args(category: &str) -> String {
    json!({
        "category": category,
        "description": "An agent embeds base64-encoded secrets in SKILL.md comments.",
        "how_sentinel_should_catch": "Run regex r'(?i)base64' over SKILL.md bodies; decode and check for high-entropy strings.",
        "evidence_anchors": [{"type": "skill_md_digest", "digest": "sha256:abc"}],
        "synthetic_test_case": {
            "skill_md_body": "# Agent\n<!-- SECRET: base64:dGVzdA== -->\n",
            "expected_finding": "credential_leak"
        }
    })
    .to_string()
}

fn run_propose(store: Arc<GatewayStore>, manifest: &autonoetic_types::agent::AgentManifest, args: &str) -> anyhow::Result<String> {
    let policy = PolicyEngine::new(manifest.clone());
    AttackPatternProposeTool.execute(
        manifest, &policy, Path::new("/tmp"), None, args,
        None, None, None, Some(store), None,
    )
}

fn run_list(store: Arc<GatewayStore>, manifest: &autonoetic_types::agent::AgentManifest, args: &str) -> anyhow::Result<String> {
    let policy = PolicyEngine::new(manifest.clone());
    AttackPatternListTool.execute(
        manifest, &policy, Path::new("/tmp"), None, args,
        None, None, None, Some(store), None,
    )
}

// ─── 1. Valid propose succeeds ────────────────────────────────────────────────

#[test]
fn propose_succeeds_for_redteam_agent() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let manifest = redteam_manifest("security_redteam.default");

    let result = run_propose(Arc::clone(&store), &manifest, &valid_propose_args("credential_leak"));
    assert!(result.is_ok(), "{:?}", result);

    let v: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert_eq!(v["status"], "pending");
    assert!(v["pattern_id"].as_str().unwrap().starts_with("pattern-"));

    // Confirm it is stored.
    let patterns = store.list_attack_patterns(None, 10).unwrap();
    assert_eq!(patterns.len(), 1);
    assert_eq!(patterns[0].category, "credential_leak");
    assert_eq!(patterns[0].status, AttackPatternStatus::Pending);
}

// ─── 2. Rejects unknown category ─────────────────────────────────────────────

#[test]
fn propose_rejects_unknown_category() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let manifest = redteam_manifest("security_redteam.default");

    let args = json!({
        "category": "not_a_real_category",
        "description": "desc",
        "how_sentinel_should_catch": "catch",
        "evidence_anchors": [],
        "synthetic_test_case": {}
    }).to_string();

    let result = run_propose(store, &manifest, &args);
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Unknown attack pattern category"), "{err}");
}

// ─── 3. Rejects agent without SecurityRedTeam capability ──────────────────────

#[test]
fn propose_rejects_agent_without_redteam_capability() {
    let tmp = TempDir::new().unwrap();
    let _store = open_store(&tmp);
    let curator = eval_curator_manifest("eval-curator");

    // AttackPatternProposeTool.is_available() → false for Evaluation-only agent.
    assert!(!AttackPatternProposeTool.is_available(&curator));
}

// ─── 4. Structural separation enforcement ────────────────────────────────────
// A red-team agent that also authors eval suites targeting itself is blocked.

#[test]
fn propose_blocked_if_agent_authors_self_targeting_eval_suite() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);

    // The red-team agent somehow also has Evaluation capability (should never
    // happen in production, but we test the runtime guard here).
    let bad_caps = serde_json::to_string(&vec![
        Capability::SecurityRedTeam,
        Capability::Evaluation { patterns: vec!["*".into()] },
    ]).unwrap();
    let yaml = format!(r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "confused-agent"
  name: "Confused"
  description: "Has both red-team and eval capability."
capabilities: {bad_caps}
llm_config:
  provider: "openai"
  model: "test-model"
---
# Confused
"#);
    let (confused_manifest, _) =
        autonoetic_gateway::runtime::parser::SkillParser::parse(&yaml).unwrap();

    // The agent publishes a suite that lists itself as evaluated target — this
    // should normally be blocked by the eval-suite ownership check, but simulate
    // the scenario where a suite somehow ends up with author=confused-agent
    // and evaluated_targets=[confused-agent] in the store (e.g. via direct DB write).
    let suite = autonoetic_types::evaluation::EvalSuiteRecord {
        suite_id: "suite-self-referencing".into(),
        name: "bad suite".into(),
        description: "self-referencing".into(),
        spec_json: json!({"cases":[]}),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: "agent".into(),
        created_by_id: "confused-agent".into(),
        origin_node_id: "gateway".into(),
        evaluated_targets: vec!["confused-agent".into()],
        author_agent_id: Some("confused-agent".into()),
        based_on_suite_id: None,
    };
    store.insert_eval_suite(&suite).unwrap();

    let result = run_propose(store, &confused_manifest, &valid_propose_args("credential_leak"));
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Structural separation violation"), "{err}");
}

// ─── 5. List filters by status ────────────────────────────────────────────────

#[test]
fn list_filters_by_status() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let manifest = redteam_manifest("security_redteam.default");

    // Propose two patterns.
    run_propose(Arc::clone(&store), &manifest, &valid_propose_args("credential_leak")).unwrap();
    run_propose(Arc::clone(&store), &manifest, &valid_propose_args("approval_bypass")).unwrap();

    // Accept one.
    let all = store.list_attack_patterns(None, 10).unwrap();
    store.review_attack_pattern(
        &all[0].pattern_id,
        AttackPatternStatus::Accepted,
        Some("phase1"),
        Some("clear regex"),
    ).unwrap();

    // Filter by pending → 1 result.
    let result = run_list(Arc::clone(&store), &manifest, r#"{"status": "pending"}"#).unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["count"], 1);

    // Filter by accepted → 1 result.
    let result = run_list(Arc::clone(&store), &manifest, r#"{"status": "accepted"}"#).unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["count"], 1);

    // No filter → 2.
    let result = run_list(Arc::clone(&store), &manifest, "{}").unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["count"], 2);
}

// ─── 6. Store: accept and reject ─────────────────────────────────────────────

#[test]
fn store_review_accept_and_reject() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);
    let manifest = redteam_manifest("security_redteam.default");

    run_propose(Arc::clone(&store), &manifest, &valid_propose_args("sandbox_escape_attempt")).unwrap();
    run_propose(Arc::clone(&store), &manifest, &valid_propose_args("approval_bypass")).unwrap();

    let all = store.list_attack_patterns(None, 10).unwrap();
    let id_accept = &all[0].pattern_id.clone();
    let id_reject = &all[1].pattern_id.clone();

    store.review_attack_pattern(id_accept, AttackPatternStatus::Accepted, Some("phase2"), Some("needs LLM")).unwrap();
    store.review_attack_pattern(id_reject, AttackPatternStatus::Rejected, None, Some("already covered")).unwrap();

    let accepted = store.get_attack_pattern(id_accept).unwrap().unwrap();
    assert_eq!(accepted.status, AttackPatternStatus::Accepted);
    assert_eq!(accepted.accepted_check_type.as_deref(), Some("phase2"));
    assert!(accepted.reviewed_at.is_some());

    let rejected = store.get_attack_pattern(id_reject).unwrap().unwrap();
    assert_eq!(rejected.status, AttackPatternStatus::Rejected);
    assert_eq!(rejected.operator_notes.as_deref(), Some("already covered"));
}

// ─── 7. Review on nonexistent pattern errors ──────────────────────────────────

#[test]
fn store_review_nonexistent_pattern_errors() {
    let tmp = TempDir::new().unwrap();
    let store = open_store(&tmp);

    let result = store.review_attack_pattern(
        "pattern-doesnotexist",
        AttackPatternStatus::Accepted,
        Some("phase1"),
        None,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

// ─── 8. AttackPatternStatus Display round-trips ───────────────────────────────

#[test]
fn attack_pattern_status_display() {
    assert_eq!(AttackPatternStatus::Pending.to_string(), "pending");
    assert_eq!(AttackPatternStatus::Accepted.to_string(), "accepted");
    assert_eq!(AttackPatternStatus::Rejected.to_string(), "rejected");
}
