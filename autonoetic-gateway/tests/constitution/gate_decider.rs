//! P-2.20 / P-2.21 / R-10.7: agent-as-decider runtime.
//!
//! Verifies that agents with `GateDecider` capability can resolve approval and
//! escalation gates, that uncertain agent-deciders escalate rather than reject
//! (P-2.21), and that an agent may not decide a gate created by itself or a
//! descendant session (R-10.7).


use std::path::PathBuf;
use std::sync::Arc;

use autonoetic_gateway::runtime::human_gate::GateService;
use autonoetic_gateway::scheduler::approval::{
    approve_request_with_options, reject_request_with_options, ApproveOptions,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn runtime_declaration() -> RuntimeDeclaration {
    RuntimeDeclaration {
        mounts: Vec::new(),
        engine: "autonoetic".to_string(),
        gateway_version: "0.1.0".to_string(),
        sdk_version: "0.1.0".to_string(),
        runtime_type: "stateful".to_string(),
        sandbox: "bubblewrap".to_string(),
        runtime_lock: "runtime.lock".to_string(),
    }
}

fn agent_manifest(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        runtime: runtime_declaration(),
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: format!("test agent {}", agent_id),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        ..TestManifest::new().build()
    }
}

fn write_agent_dir(agents_dir: &PathBuf, agent_id: &str, capabilities: &[Capability]) {
    let agent_dir = agents_dir.join(agent_id);
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();

    let caps_yaml = serde_yaml::to_string(&capabilities).unwrap();
    let skill = format!(
        r#"---
version: "1.0"
runtime:
  engine: autonoetic
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: stateful
  sandbox: bubblewrap
  runtime_lock: runtime.lock
agent:
  id: {agent_id}
  name: {agent_id}
  description: test agent {agent_id}
capabilities:
{caps_yaml}---
# Instructions
"#,
        agent_id = agent_id,
        caps_yaml = caps_yaml
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill).unwrap();
}

fn seed_decider_revision(
    agents_dir: &PathBuf,
    gateway_dir: &PathBuf,
    store: &GatewayStore,
    agent_id: &str,
) -> anyhow::Result<()> {
    let revision_id = format!("rev_sha256:{}", "0".repeat(64));
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(&revision_id);
    std::fs::create_dir_all(&rev_dir)?;

    let agent_dir = agents_dir.join(agent_id);
    std::fs::copy(agent_dir.join("SKILL.md"), rev_dir.join("SKILL.md"))?;
    std::fs::copy(agent_dir.join("runtime.lock"), rev_dir.join("runtime.lock"))?;

    use autonoetic_types::agent_revision::AgentAliasRecord;
    store.upsert_agent_alias(&AgentAliasRecord::new(
        agent_id.to_string(),
        agent_id.to_string(),
        revision_id,
        chrono::Utc::now().to_rfc3339(),
        "operator".to_string(),
        "test".to_string(),
        Some("test".to_string()),
    ))?;
    Ok(())
}

/// Record a session as owned by `agent_id` so the R-10.7 binding check can
/// authenticate the caller-supplied `decider_session_id`.
fn seed_decider_session(
    store: &GatewayStore,
    session_id: &str,
    agent_id: &str,
) -> anyhow::Result<()> {
    use autonoetic_types::causal_chain::SessionTranscriptRecord;
    let root = session_id.split('/').next().unwrap_or(session_id);
    store.upsert_session_transcript(&SessionTranscriptRecord {
        transcript_id: format!("tr-{}", session_id),
        session_id: session_id.to_string(),
        root_session_id: root.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: None,
        user_id: None,
        started_at: chrono::Utc::now().to_rfc3339(),
        ended_at: None,
        status: "active".to_string(),
        turn_count: 1,
        transcript_handle: None,
        excerpt: None,
        origin_node_id: None,
    })?;
    Ok(())
}

fn sandbox_action() -> ScheduledAction {
    ScheduledAction::SandboxExec {
        command: "echo test".to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: Some(vec!["example.com".to_string()]),
        intent: None,
    }
}

fn create_pending_approval(
    store: &GatewayStore,
    request_id: &str,
    agent_id: &str,
    session_id: &str,
) -> anyhow::Result<()> {
    let mut request = ApprovalRequest {
        request_id: request_id.to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(
            session_id
                .split('/')
                .next()
                .unwrap_or(session_id)
                .to_string(),
        ),
        workflow_id: None,
        task_id: None,
        action: sandbox_action(),
        created_at: (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339(),
        status: None,
        decided_at: None,
        decided_by: None,
        reason: Some("test gate".to_string()),
        evidence_ref: None,
        decision_reason: None,
        approval_level: ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,

        expires_at: None,
    };
    store.create_approval(&mut request)?;
    Ok(())
}

#[test]
fn agent_with_gate_decider_can_approve_other_agents_gate() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "decider.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "decider.default")?;
    seed_decider_session(&store, "other-session", "decider.default")?;
    create_pending_approval(
        &store,
        "apr-decider",
        "coder.default",
        "root-session/coder-abc",
    )?;

    let decision = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-decider",
        "agent:decider.default",
        Some("approved by agent decider".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("other-session".to_string()),
            ..Default::default()
        },
    )?;

    assert_eq!(decision.status, ApprovalStatus::Approved);
    assert_eq!(decision.decided_by, "agent:decider.default");

    // P-2.20 causal event should be recorded.
    let events = store.search_causal_events(None, Some("decider.default"), 10)?;
    let p220 = events
        .iter()
        .find(|e| e.enforced_rules.iter().any(|r| r == "P-2.20"));
    assert!(
        p220.is_some(),
        "agent-decider approval must emit P-2.20 causal event"
    );

    Ok(())
}

#[test]
fn agent_without_gate_decider_cannot_approve() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    // Agent lacks GateDecider entirely.
    write_agent_dir(
        &agents_dir,
        "regular.default",
        &[Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
    );

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "regular.default")?;
    create_pending_approval(
        &store,
        "apr-regular",
        "coder.default",
        "root-session/coder-abc",
    )?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-regular",
        "agent:regular.default",
        Some("trying to approve".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("other-session".to_string()),
            ..Default::default()
        },
    )
    .expect_err("agent without GateDecider should not be allowed to approve");

    assert!(
        err.to_string().contains("P-2.20"),
        "error should cite P-2.20: {}",
        err
    );

    Ok(())
}

#[test]
fn agent_decider_cannot_decide_own_spawn_tree_gate() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "selfdecider.default",
        &[Capability::GateDecider {
            kinds: vec!["*".to_string()],
        }],
    );

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "selfdecider.default")?;
    // The decider session must be bound to the decider agent for the R-10.7
    // ownership check; it is a parent of the gate's session, so the spawn-tree
    // violation still triggers.
    seed_decider_session(&store, "root-session", "selfdecider.default")?;

    // Gate created in a descendant session of the would-be decider.
    create_pending_approval(
        &store,
        "apr-self",
        "selfdecider.default",
        "root-session/selfdecider-abc",
    )?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-self",
        "agent:selfdecider.default",
        Some("trying to self-decide".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("root-session".to_string()),
            ..Default::default()
        },
    )
    .expect_err("agent-decider should not decide a descendant's gate");

    assert!(
        err.to_string().contains("R-10.7"),
        "error should cite R-10.7: {}",
        err
    );

    Ok(())
}

#[test]
fn agent_decider_cannot_spoof_session_id_to_bypass_r_10_7() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "spoofer.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "spoofer.default")?;

    // "legit-session" is owned by a *different* agent.
    seed_decider_session(&store, "legit-session", "other-agent.default")?;

    // Gate sits in the spoofer's own spawn tree, which would be blocked by the
    // real decider session — but the spoofer tries to bypass R-10.7 by claiming
    // "legit-session" (owned by another agent) as its decider session.
    create_pending_approval(
        &store,
        "apr-spoof",
        "spoofer.default",
        "root-session/spoofer-abc",
    )?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-spoof",
        "agent:spoofer.default",
        Some("approving".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("legit-session".to_string()),
            ..Default::default()
        },
    )
    .expect_err("spoofed session ID must not bypass R-10.7 binding");

    assert!(
        err.to_string().contains("R-10.7"),
        "spoofed decider session should be rejected by binding check: {}",
        err
    );

    Ok(())
}

#[test]
fn agent_decider_without_session_id_is_rejected_r_10_7() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "noid.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "noid.default")?;
    create_pending_approval(
        &store,
        "apr-noid",
        "coder.default",
        "root-session/coder-abc",
    )?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-noid",
        "agent:noid.default",
        Some("approving".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: None,
            ..Default::default()
        },
    )
    .expect_err("missing decider_session_id must fail closed (R-10.7)");

    assert!(
        err.to_string().contains("R-10.7"),
        "missing decider_session_id should be rejected: {}",
        err
    );

    Ok(())
}

#[test]
fn agent_decider_can_reject_with_capability() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "rejecter.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "rejecter.default")?;
    seed_decider_session(&store, "other-session", "rejecter.default")?;
    create_pending_approval(
        &store,
        "apr-reject",
        "coder.default",
        "root-session/coder-abc",
    )?;

    // Rejection is a blocking decision, so decider motivation is required.
    let decision = reject_request_with_options(
        &cfg,
        Some(&store),
        "apr-reject",
        "agent:rejecter.default",
        Some("out of scope".to_string()),
        None,
        ApproveOptions {
            decider_session_id: Some("other-session".to_string()),
            ..Default::default()
        },
    )?;

    assert_eq!(decision.status, ApprovalStatus::Rejected);
    assert_eq!(decision.decided_by, "agent:rejecter.default");

    Ok(())
}

#[test]
fn agent_decider_escalates_to_human_when_uncertain() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "escalator.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string(), "escalation".to_string()],
        }],
    );

    let _cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "escalator.default")?;
    seed_decider_session(&store, "other-session", "escalator.default")?;

    // Create an approval gate.
    create_pending_approval(
        &store,
        "apr-escalate",
        "coder.default",
        "root-session/coder-xyz",
    )?;

    let manifest = agent_manifest(
        "escalator.default",
        vec![Capability::GateDecider {
            kinds: vec!["approval".to_string(), "escalation".to_string()],
        }],
    );

    let svc = GateService::new(store.clone());
    let result = svc.escalate_to_human(
        "apr-escalate",
        "insufficient context to decide safely",
        &manifest,
        Some("other-session"),
    )?;

    assert!(
        result.enforced_rules().contains(&"P-2.21"),
        "agent-decider escalation must carry P-2.21, got {:?}",
        result.enforced_rules()
    );

    // The original gate should still be pending.
    let original = store.get_approval("apr-escalate")?.unwrap();
    assert!(
        original.status.is_none(),
        "original gate must remain pending after escalation"
    );

    // The escalation gate should reference the original.
    if let autonoetic_gateway::runtime::human_gate::GateResult::Suspended { gate_id, .. } = result {
        let escalation = store.get_approval(&gate_id)?.unwrap();
        if let ScheduledAction::SessionEscalate { payload, .. } = escalation.action {
            let payload = payload.expect("escalation payload missing");
            assert_eq!(
                payload.get("original_gate_id").and_then(|v| v.as_str()),
                Some("apr-escalate")
            );
        } else {
            panic!("escalation gate should be SessionEscalate");
        }
    } else {
        panic!("escalate_to_human should return Suspended");
    }

    Ok(())
}

#[test]
fn escalation_without_gate_decider_capability_fails() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    // Agent has GateDecider for approvals only, not escalations.
    write_agent_dir(
        &agents_dir,
        "partial.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );

    let _cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "partial.default")?;
    create_pending_approval(
        &store,
        "apr-partial",
        "coder.default",
        "root-session/coder-xyz",
    )?;

    let manifest = agent_manifest(
        "partial.default",
        vec![Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );

    let svc = GateService::new(store.clone());
    let err = svc
        .escalate_to_human(
            "apr-partial",
            "I cannot decide this",
            &manifest,
            Some("other-session"),
        )
        .expect_err("agent without escalation GateDecider should not escalate");

    assert!(
        err.to_string().contains("P-2.20"),
        "error should cite P-2.20: {}",
        err
    );

    Ok(())
}
