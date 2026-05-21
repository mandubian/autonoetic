use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::improvement::AbReplayTool;
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::{AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

fn test_manifest(capabilities: Vec<Capability>) -> AgentManifest {
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
            id: "improvement-orchestrator".to_string(),
            name: "improvement-orchestrator".to_string(),
            description: "test".to_string(),
        },
        capabilities,
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
        agentskills_import: None,
        compression: None,
        sandbox_network: Default::default(),
    }
}

fn open_store(tmp: &TempDir) -> Arc<GatewayStore> {
    Arc::new(GatewayStore::open(tmp.path()).unwrap())
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
        created_by_type: "test".to_string(),
        created_by_id: "improvement_ab_replay_test".to_string(),
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
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
        updated_by_type: "test".to_string(),
        updated_by_id: "improvement_ab_replay_test".to_string(),
        reason: Some("test seed".to_string()),
    };
    store.upsert_agent_alias(&alias)?;
    Ok(())
}

/// Full revision IDs in AgentRef.parse() format.
const REV_A_ID: &str = "rev_sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REV_B_ID: &str = "rev_sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const TARGET_AGENT: &str = "test-agent";

fn setup_env(tmp: &TempDir) -> (Arc<GatewayStore>, GatewayConfig) {
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = open_store(&tmp);

    // Seed two revisions of the same test agent
    seed_revision(&store, TARGET_AGENT, REV_A_ID).unwrap();
    seed_revision(&store, TARGET_AGENT, REV_B_ID).unwrap();
    seed_alias(&store, TARGET_AGENT, REV_B_ID).unwrap();

    let config = GatewayConfig {
        agents_dir: tmp.path().join("agents"),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.agents_dir).unwrap();

    (store, config)
}

// ─── 1. Queued path ───────────────────────────────────────────────────────

#[test]
fn test_ab_replay_queues_eval_runs() {
    let tmp = TempDir::new().unwrap();
    let (store, config) = setup_env(&tmp);

    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let args = json!({
        "task_specs": [
            {"message": "do task one", "case_id": "t1"},
            {"message": "do task two", "case_id": "t2"},
        ],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "replays_per_variant": 1,
        "holdout_ratio": 0.0,
    });

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

    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
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

    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    // 20 tasks × 1 replay × 2 variants × $0.05 = $2.00 > $1.00 ceiling
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
        "replays_per_variant": 1,
        "holdout_ratio": 0.0,
    });

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

    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["ok"], false, "expected ok=false, got: {v}");
    assert_eq!(v["status"], "cost_exceeded", "expected cost_exceeded, got: {v}");
    assert!(v["estimated_cost_usd"].as_f64().unwrap() > 1.0);
    assert_eq!(v["max_budget_usd"], 1.0);
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
        "replays_per_variant": 1,
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

    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    let args = json!({
        "task_specs": [{"message": "hello"}],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", "other-agent", other_rev),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "replays_per_variant": 1,
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

    let manifest = test_manifest(vec![Capability::Evaluation {
        patterns: vec!["*".into()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());

    // 1 task, holdout_ratio=1.0 → 0 train tasks
    let args = json!({
        "task_specs": [{"message": "hello"}],
        "agent_id": TARGET_AGENT,
        "revision_a": format!("{}@{}", TARGET_AGENT, REV_A_ID),
        "revision_b": format!("{}@{}", TARGET_AGENT, REV_B_ID),
        "replays_per_variant": 1,
        "holdout_ratio": 1.0,
    });

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
