//! Integration tests for the capsule export/import pipeline.

use autonoetic_gateway::capsule::{export, import, ExportRequest, ImportRequest};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::capsule::CapsuleMode;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::PrincipalKind;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use std::sync::Arc;
use tempfile::tempdir;

struct Fixture {
    _temp: tempfile::TempDir,
    config: GatewayConfig,
    store: Arc<GatewayStore>,
    gateway_dir: std::path::PathBuf,
}

fn seed_revision_files(gateway_dir: &std::path::Path, agent_id: &str, revision_id: &str) {
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(revision_id);
    std::fs::create_dir_all(&rev_dir).unwrap();
    std::fs::write(
        rev_dir.join("SKILL.md"),
        "# test agent\nSecret: OPENAI_API_KEY=sk-abc123\n",
    )
    .unwrap();
    std::fs::write(rev_dir.join("runtime.lock"), "agents: []\n").unwrap();
}

fn make_fixture(agent_id: &str, revision_id: &str) -> Fixture {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    seed_revision_files(&gateway_dir, agent_id, revision_id);

    let rev = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:{}", "x".repeat(64)),
        runtime_lock_hash: format!("sha256:{}", "y".repeat(64)),
        manifest_hash: format!("sha256:{}", "z".repeat(64)),
        created_at: "2026-05-28T00:00:00Z".to_string(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "test".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "artifact".to_string(),
        source_ref: Some("art_test".to_string()),
        origin_node_id: "node-A".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::Value::Null,
        short_id: "abcd1234".to_string(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rev).unwrap();
    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),
        updated_at: "2026-05-28T00:00:00Z".to_string(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "test".to_string(),
        reason: None,
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&alias).unwrap();

    let mut config = GatewayConfig {
        agents_dir,
        ..Default::default()
    };
    // Seed trusted signer for the gateway key so signed import verifies.
    let key =
        autonoetic_gateway::runtime::crypto::GatewayIdentityKey::load_or_generate(&gateway_dir)
            .unwrap();
    config.capsule.trusted_signers.insert(
        format!("gateway:{}", key.fingerprint()),
        B64.encode(key.public_key_bytes()),
    );

    Fixture {
        _temp: temp,
        config,
        store,
        gateway_dir,
    }
}

#[test]
fn export_then_import_creates_revision_with_capsule_import_source_kind() {
    let f = make_fixture("demo.agent", "rev_sha256:cap-rt-001");
    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("agent.capsule.tar.zst");

    let outcome = export(
        ExportRequest {
            agent_id: "demo.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: None,
            sign: Some(true),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
        },
        autonoetic_gateway::capsule::ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");
    assert_eq!(outcome.mode, "thin");
    assert!(outcome.signed);
    assert!(outcome.size_bytes > 0);
    assert!(outcome.capsule_path.exists());

    // Import into a fresh gateway.
    let f2 = {
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
        let mut config = GatewayConfig {
            agents_dir,
            ..Default::default()
        };
        config
            .capsule
            .trusted_signers
            .extend(f.config.capsule.trusted_signers.clone());
        Fixture {
            _temp: temp,
            config,
            store,
            gateway_dir,
        }
    };

    let imp = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: true,
            dry_run: false,
            activate: true,
            trust_domain_override: None,
            memory_conflict_policy: Default::default(),
        },
        autonoetic_gateway::capsule::ImportContext {
            gateway_dir: &f2.gateway_dir,
            gateway_config: &f2.config,
            gateway_store: &f2.store,
        },
    )
    .expect("import");
    assert!(imp.created_revision);
    assert_eq!(imp.agent_id, "demo.agent");
    assert_eq!(imp.signature_status, "Verified");

    // The new gateway should now have the revision with source_kind = capsule_import.
    let rev = f2
        .store
        .get_agent_revision(&imp.revision_id)
        .unwrap()
        .expect("revision persisted");
    assert_eq!(rev.source_kind, "capsule_import");
    assert_eq!(rev.source_ref.as_deref(), Some(outcome.capsule_id.as_str()));

    // Alias bound to the new revision (because --activate).
    let alias = f2.store.get_agent_alias("demo.agent").unwrap().unwrap();
    assert_eq!(alias.revision_id, imp.revision_id);

    // The exported SKILL.md must NOT contain the raw secret value.
    let rev_dir = f2
        .gateway_dir
        .join("revisions")
        .join("agents")
        .join("demo.agent")
        .join(&imp.revision_id);
    let skill = std::fs::read_to_string(rev_dir.join("SKILL.md")).unwrap();
    assert!(
        !skill.contains("sk-abc123"),
        "scrubbing failed; SKILL.md still contains secret: {}",
        skill
    );
}

#[test]
fn import_dry_run_does_not_persist_revision() {
    let f = make_fixture("demo.agent", "rev_sha256:cap-dr-002");
    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("agent.capsule.tar.zst");

    let outcome = export(
        ExportRequest {
            agent_id: "demo.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: None,
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
        },
        autonoetic_gateway::capsule::ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");
    let _ = outcome;

    let f2 = {
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
        let config = GatewayConfig {
            agents_dir,
            ..Default::default()
        };
        Fixture {
            _temp: temp,
            config,
            store,
            gateway_dir,
        }
    };

    let imp = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: true,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: Default::default(),
        },
        autonoetic_gateway::capsule::ImportContext {
            gateway_dir: &f2.gateway_dir,
            gateway_config: &f2.config,
            gateway_store: &f2.store,
        },
    )
    .expect("import");
    assert!(imp.dry_run);
    assert!(!imp.created_revision);
    assert!(f2.store.get_agent_revision(&imp.revision_id).unwrap().is_none());
}

#[test]
fn tampered_archive_fails_verify_signature() {
    let f = make_fixture("demo.agent", "rev_sha256:cap-tamper-003");
    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("agent.capsule.tar.zst");

    export(
        ExportRequest {
            agent_id: "demo.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: None,
            sign: Some(true),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
        },
        autonoetic_gateway::capsule::ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    // Tamper: rewrite the manifest inside the archive.
    let work = tempdir().unwrap();
    autonoetic_gateway::capsule::archive::unpack(&archive, work.path(), 1024 * 1024).unwrap();
    let manifest_path = work.path().join("capsule.json");
    let mut m: autonoetic_types::capsule::CapsuleManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    m.agent_id = "evil.agent".to_string();
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
    autonoetic_gateway::capsule::archive::pack(work.path(), &archive).unwrap();

    let f2 = {
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
        let mut config = GatewayConfig {
            agents_dir,
            ..Default::default()
        };
        config
            .capsule
            .trusted_signers
            .extend(f.config.capsule.trusted_signers.clone());
        Fixture {
            _temp: temp,
            config,
            store,
            gateway_dir,
        }
    };

    let err = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: true,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: Default::default(),
        },
        autonoetic_gateway::capsule::ImportContext {
            gateway_dir: &f2.gateway_dir,
            gateway_config: &f2.config,
            gateway_store: &f2.store,
        },
    )
    .expect_err("tampered capsule must fail verify");
    let msg = err.to_string();
    assert!(
        msg.contains("Mismatch") || msg.contains("signature"),
        "unexpected error: {msg}"
    );
}

#[test]
fn import_refuses_archive_exceeding_size_cap() {
    let f = make_fixture("demo.agent", "rev_sha256:cap-size-004");
    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("agent.capsule.tar.zst");
    export(
        ExportRequest {
            agent_id: "demo.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: None,
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
        },
        autonoetic_gateway::capsule::ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    let mut config = f.config.clone();
    config.capsule.max_capsule_size_bytes = 16;
    let err = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: true,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: Default::default(),
        },
        autonoetic_gateway::capsule::ImportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &config,
            gateway_store: &f.store,
        },
    )
    .expect_err("oversize archive must be refused");
    assert!(
        err.to_string().contains("exceeds configured max"),
        "{}",
        err
    );
}

#[test]
fn second_import_is_dedup_noop_for_existing_revision() {
    let f = make_fixture("demo.agent", "rev_sha256:cap-dedup-005");
    let out_dir = tempdir().unwrap();
    let archive = out_dir.path().join("agent.capsule.tar.zst");
    export(
        ExportRequest {
            agent_id: "demo.agent".to_string(),
            revision_id: None,
            mode: CapsuleMode::Thin,
            include_memory: None,
            sign: Some(false),
            output_path: Some(archive.clone()),
            session_id: None,
            root_session_id: None,
        },
        autonoetic_gateway::capsule::ExportContext {
            gateway_dir: &f.gateway_dir,
            gateway_config: &f.config,
            gateway_store: &f.store,
        },
    )
    .expect("export");

    let f2 = {
        let temp = tempdir().unwrap();
        let agents_dir = temp.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
        let config = GatewayConfig {
            agents_dir,
            ..Default::default()
        };
        Fixture {
            _temp: temp,
            config,
            store,
            gateway_dir,
        }
    };

    let first = import(
        ImportRequest {
            archive_path: archive.clone(),
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: Default::default(),
        },
        autonoetic_gateway::capsule::ImportContext {
            gateway_dir: &f2.gateway_dir,
            gateway_config: &f2.config,
            gateway_store: &f2.store,
        },
    )
    .expect("first import");
    assert!(first.created_revision);

    let second = import(
        ImportRequest {
            archive_path: archive,
            verify_signature: false,
            dry_run: false,
            activate: false,
            trust_domain_override: None,
            memory_conflict_policy: Default::default(),
        },
        autonoetic_gateway::capsule::ImportContext {
            gateway_dir: &f2.gateway_dir,
            gateway_config: &f2.config,
            gateway_store: &f2.store,
        },
    )
    .expect("second import");
    assert!(!second.created_revision, "second import must not duplicate");
    assert!(
        second.dedup_savings_bytes >= first.dedup_savings_bytes,
        "second import should report at least as much dedup as the first"
    );
}
