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

pub(crate) fn agent_manifest(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
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

pub(crate) fn write_agent_dir(agents_dir: &PathBuf, agent_id: &str, capabilities: &[Capability]) {
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
llm_preset: decider
capabilities:
{caps_yaml}---
# Instructions
"#,
        agent_id = agent_id,
        caps_yaml = caps_yaml
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill).unwrap();
}

pub(crate) fn seed_decider_revision(
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

/// Seat `agent_id` over `scope_root` so the decide path's provenance condition
/// (#1195) is satisfied. Inserted directly rather than through
/// `decider_appointment::appoint` on purpose: these tests exercise the *decide*
/// path (P-2.20 capability, R-10.7 boundary), and going through the appointing
/// validator would couple them to its rules as well. Appointment validation has
/// its own suite in `decider_appointment.rs`.
fn seed_appointment(
    store: &GatewayStore,
    agent_id: &str,
    scope_root: &str,
) -> anyhow::Result<()> {
    use autonoetic_types::background::ApprovalRisk;
    use autonoetic_types::decider_appointment::DeciderAppointment;
    store.insert_decider_appointment(&DeciderAppointment {
        appointment_id: format!("apt-test-{}-{}", agent_id, scope_root).replace('/', "_"),
        decider_agent: agent_id.to_string(),
        decider_revision: "rev-test".to_string(),
        decider_provider: None,
        decider_model: None,
        kinds: vec!["approval".to_string(), "escalation".to_string()],
        scope_root_session: scope_root.to_string(),
        decider_session: None,
        risk_ceiling: ApprovalRisk::High,
        advice_only: true,
        expires_at: None,
        max_gates: None,
        gates_decided: 0,
        appointed_by: "operator".to_string(),
        appointed_at: chrono::Utc::now().to_rfc3339(),
        revoked_at: None,
        revoked_by: None,
        revoked_reason: None,
    })
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
    seed_appointment(&store, "decider.default", "root-session")?;
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
    seed_appointment(&store, "rejecter.default", "root-session")?;
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
    seed_appointment(&store, "escalator.default", "root-session")?;

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

// ---------------------------------------------------------------------------
// #1192: a claimed agent identity that does not resolve must be refused, never
// silently demoted to a human decision.
// ---------------------------------------------------------------------------

/// An `agent:<id>` decider that was never installed is refused, and the gate
/// stays pending. Before #1192 the manifest-load error was logged at debug and
/// the decision committed with P-2.20, R-10.7 and the `agent_decider.*_gate`
/// causal event all skipped.
#[test]
fn unresolvable_agent_decider_is_refused_not_demoted_to_human() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    create_pending_approval(
        &store,
        "apr-ghost",
        "coder.default",
        "root-session/coder-abc",
    )?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-ghost",
        "agent:ghost.default",
        Some("approved by a decider that does not exist".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("other-session".to_string()),
            ..Default::default()
        },
    )
    .expect_err("an unresolvable agent decider must be refused, not treated as an operator");

    assert!(
        err.to_string().contains("P-2.20"),
        "refusal should cite P-2.20: {}",
        err
    );

    // The gate must still be pending — a refused decision commits nothing.
    let after = store
        .get_approval("apr-ghost")?
        .expect("approval should still exist");
    assert!(
        after.decided_by.is_none(),
        "refused decision must not record a decider, got: {:?}",
        after.decided_by
    );
    assert!(
        !matches!(after.status, Some(ApprovalStatus::Approved)),
        "refused decision must not approve the gate, got: {:?}",
        after.status
    );

    Ok(())
}

/// The revocation shape: an agent whose bundle is on disk but has no promoted
/// revision does not resolve (#1136 presence check). Its decisions must start
/// being *refused*, not stop being *checked*.
#[test]
fn agent_decider_without_promoted_revision_is_refused() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    // Bundle exists on disk and even declares GateDecider — but it is not
    // seeded as a promoted revision, so it is not installed.
    write_agent_dir(
        &agents_dir,
        "revoked.default",
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
    seed_decider_session(&store, "other-session", "revoked.default")?;
    create_pending_approval(
        &store,
        "apr-revoked",
        "coder.default",
        "root-session/coder-abc",
    )?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-revoked",
        "agent:revoked.default",
        Some("approved by a revoked decider".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("other-session".to_string()),
            ..Default::default()
        },
    )
    .expect_err("an agent without a promoted revision must be refused");

    assert!(
        err.to_string().contains("P-2.20"),
        "refusal should cite P-2.20: {}",
        err
    );

    let after = store
        .get_approval("apr-revoked")?
        .expect("approval should still exist");
    assert!(
        after.decided_by.is_none(),
        "refused decision must not record a decider, got: {:?}",
        after.decided_by
    );

    Ok(())
}

/// A `decided_by` that never claimed agent identity is unaffected: the human
/// path still bypasses the P-2.20 check entirely.
#[test]
fn operator_decider_is_unaffected_by_the_agent_identity_check() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    create_pending_approval(
        &store,
        "apr-operator",
        "coder.default",
        "root-session/coder-abc",
    )?;

    let decision = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-operator",
        "operator",
        Some("approved by the human operator".to_string()),
        None,
        None,
        None,
        ApproveOptions::default(),
    )?;

    assert_eq!(decision.status, ApprovalStatus::Approved);
    assert_eq!(decision.decided_by, "operator");

    Ok(())
}

// ---------------------------------------------------------------------------
// #1193: R-10.7 is a trust boundary, not a direction. A decider spawned *by*
// the agent whose gate it decides is the same conflict of interest read the
// other way round.
// ---------------------------------------------------------------------------

/// The lead spawns its own approver. `R/lead/nightwatch` rules on a gate raised
/// in `R/lead` — before #1193 this passed, because the spawn-tree check only
/// refused when the gate sat at or *below* the decider.
#[test]
fn decider_spawned_by_the_gate_raiser_is_refused() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "captive.default",
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
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "captive.default")?;

    // The decider is a *descendant* of the session that raised the gate:
    // the lead spawned the agent that is now being asked to judge the lead.
    seed_decider_session(&store, "root-1/lead/nightwatch", "captive.default")?;
    create_pending_approval(&store, "apr-captive", "lead.default", "root-1/lead")?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-captive",
        "agent:captive.default",
        Some("approving my spawner's gate".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("root-1/lead/nightwatch".to_string()),
            ..Default::default()
        },
    )
    .expect_err("a decider spawned by the gate raiser must not rule on its gate");

    assert!(
        err.to_string().contains("R-10.7"),
        "refusal should cite R-10.7: {}",
        err
    );

    let after = store
        .get_approval("apr-captive")?
        .expect("approval should still exist");
    assert!(
        after.decided_by.is_none(),
        "refused decision must not record a decider, got: {:?}",
        after.decided_by
    );

    Ok(())
}

/// The same inversion, reachable only through recorded spawn lineage rather
/// than the hierarchical-ID fast path — non-hierarchical session IDs must not
/// be a way around the check.
#[test]
fn decider_spawned_by_the_gate_raiser_is_refused_via_recorded_lineage() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "captive2.default",
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
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "captive2.default")?;

    // Flat IDs sharing a root: the prefix fast path cannot see the relation,
    // so only the recorded lineage walk catches it.
    seed_decider_session(&store, "root-2/watch-9", "captive2.default")?;
    create_pending_approval(&store, "apr-captive2", "lead.default", "root-2/lead-1")?;
    store.upsert_session_spawn_lineage(
        "root-2/watch-9",
        "root-2/lead-1",
        "root-2",
        1,
        "captive2.default",
        &chrono::Utc::now().to_rfc3339(),
    )?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-captive2",
        "agent:captive2.default",
        Some("approving my spawner's gate".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("root-2/watch-9".to_string()),
            ..Default::default()
        },
    )
    .expect_err("recorded spawn lineage must close the same hole as the ID prefix");

    assert!(
        err.to_string().contains("R-10.7"),
        "refusal should cite R-10.7: {}",
        err
    );

    Ok(())
}

/// The boundary is ancestry, not co-residence: an unrelated decider in a
/// different tree still decides. This pins the scope of #1193 so the fix is not
/// silently widened to "same root session" without a decision.
#[test]
fn unrelated_decider_in_another_tree_still_decides() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "free.default",
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
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "free.default")?;
    seed_decider_session(&store, "other-root/watch-1", "free.default")?;
    seed_appointment(&store, "free.default", "root-3")?;
    create_pending_approval(&store, "apr-free", "coder.default", "root-3/coder-1")?;

    let decision = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-free",
        "agent:free.default",
        Some("approved by an unrelated decider".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("other-root/watch-1".to_string()),
            ..Default::default()
        },
    )?;

    assert_eq!(decision.status, ApprovalStatus::Approved);

    Ok(())
}

/// #1195 (closing #1193's second half): lineage is not provenance. An agent
/// that holds `GateDecider`, is installed, and sits in a completely unrelated
/// session tree still may not rule on a gate nobody seated it for. The
/// capability is an eligibility to be appointed, not a standing licence.
#[test]
fn decider_without_an_appointment_is_refused() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    write_agent_dir(
        &agents_dir,
        "unseated.default",
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
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "unseated.default")?;
    // Clean lineage: a different root entirely.
    seed_decider_session(&store, "other-root/watch-1", "unseated.default")?;
    create_pending_approval(&store, "apr-unseated", "coder.default", "root-9/coder-a")?;

    let err = approve_request_with_options(
        &cfg,
        Some(&store),
        "apr-unseated",
        "agent:unseated.default",
        Some("ruling without a seat".to_string()),
        None,
        None,
        None,
        ApproveOptions {
            decider_session_id: Some("other-root/watch-1".to_string()),
            ..Default::default()
        },
    )
    .expect_err("an unappointed decider must be refused even with clean lineage");

    assert!(
        err.to_string().contains("no active appointment"),
        "refusal should name the missing appointment: {}",
        err
    );

    let after = store
        .get_approval("apr-unseated")?
        .expect("approval should still exist");
    assert!(after.decided_by.is_none(), "the gate must still be pending");

    Ok(())
}
