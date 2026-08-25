//! #378 — `user_profile_request` routes through GateService: pending dedup
//! per user (P-2.3), root-scoped identical-action join, unified decider card.

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;

fn manifest() -> AgentManifest {
    let mut m = support_manifest();
    m.capabilities = vec![Capability::UserProfileAccess {
        scopes: vec!["*".to_string()],
    }];
    m
}

fn support_manifest() -> AgentManifest {
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};
    AgentManifest {
        remote_access: None,
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
            mounts: Vec::new(),
        },
        agent: AgentIdentity {
            id: "profile.tester".to_string(),
            name: "profile.tester".to_string(),
            description: "user_profile gate tests".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        ..Default::default()
    }
}

fn request(
    registry: &NativeToolRegistry,
    manifest: &AgentManifest,
    store: &Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
    args: serde_json::Value,
) -> String {
    let policy = PolicyEngine::new(manifest.clone());
    registry
        .execute(
            "user_profile_share",
            manifest,
            &policy,
            &std::path::PathBuf::from("/tmp/unused"),
            None,
            &args.to_string(),
            Some("sess-profile-gate"),
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect("tool returns a structured result")
}

/// First request mints a gate; a second request for the SAME user is
/// deduplicated onto the pending gate instead of minting another approval row.
#[test]
fn profile_share_request_dedups_pending_gate_per_user() {
    let tmp = tempfile::tempdir().unwrap();
    let gateway_dir = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
    );

    let mut registry = NativeToolRegistry::new();
    autonoetic_gateway::runtime::tools::user_profile::register_tools(&mut registry);
    let manifest = manifest();

    let first = serde_json::from_str::<serde_json::Value>(&request(
        &registry,
        &manifest,
        &store,
        serde_json::json!({"user_id": "user-1", "scope": "restricted"}),
    ))
    .unwrap();
    assert_eq!(first["ok"], serde_json::json!(false));
    assert_eq!(first["approval_required"], serde_json::json!(true));
    assert_eq!(first["suspended"], serde_json::json!(true));
    assert_eq!(first.get("deduplicated"), Some(&serde_json::Value::Bool(false)));
    let gate_id = first["approval_request_id"].as_str().unwrap().to_string();

    // Same user again → deduplicated onto the pending gate.
    let second = serde_json::from_str::<serde_json::Value>(&request(
        &registry,
        &manifest,
        &store,
        serde_json::json!({"user_id": "user-1", "scope": "restricted", "reason": "again"}),
    ))
    .unwrap();
    // AlreadyPending semantics match the session-escalate migration:
    // the request itself succeeded, but no new gate was minted.
    assert_eq!(second["ok"], serde_json::json!(true));
    assert_eq!(second["approval_required"], serde_json::json!(true));
    assert_eq!(second.get("deduplicated"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(
        second["approval_request_id"].as_str().unwrap(),
        gate_id,
        "second request must reference the SAME pending gate"
    );

    // A DIFFERENT user still mints its own gate.
    let third = serde_json::from_str::<serde_json::Value>(&request(
        &registry,
        &manifest,
        &store,
        serde_json::json!({"user_id": "user-2", "scope": "restricted"}),
    ))
    .unwrap();
    assert_eq!(third.get("deduplicated"), Some(&serde_json::Value::Bool(false)));
    assert_ne!(
        third["approval_request_id"].as_str().unwrap(),
        gate_id,
        "a different user gets its own pending gate"
    );

    // Exactly two rows total — one per user, no duplicates.
    let config = autonoetic_types::config::GatewayConfig {
        runtime_dir: tmp.path().join(".gateway"),
        ..Default::default()
    };
    let all =
        autonoetic_gateway::scheduler::load_approval_requests(&config, Some(store.as_ref()))
            .unwrap();
    assert_eq!(all.len(), 2, "one gate per user, no duplicates: {all:?}");
}
