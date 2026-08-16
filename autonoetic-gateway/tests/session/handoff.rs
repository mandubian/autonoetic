//! `session.handoff` (#1088) — operator-only rebind of a live root session
//! to another orchestrator: binding rewrite, residency re-point, successor
//! context envelope, causal event, and gate guards.
//!
//! These tests exercise `perform_handoff` directly rather than through a
//! `JsonRpcRouter`: the constitution runtime is process-global and a binary
//! cannot host two routers built on different tempdir `gateway_dir`s (the
//! same pre-existing constraint that keeps `timeline_jsonrpc` the only
//! router-holding module in this binary). The RPC arm is thin plumbing;
//! `HandoffParams`' wire shape is pinned by the serde round-trip test below.

use autonoetic_gateway::runtime::session_handoff::{
    perform_handoff, HandoffOutcome, HandoffParams,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::SessionAgentBinding;
use std::sync::{Arc, OnceLock};

use crate::support::{seed_agent_revision, TestWorkspace};

struct SharedEnv {
    ws: TestWorkspace,
    store: Arc<GatewayStore>,
}

static SHARED: OnceLock<SharedEnv> = OnceLock::new();

fn shared() -> &'static SharedEnv {
    SHARED.get_or_init(|| {
        // No constitution initialization here: `perform_handoff` re-pins the
        // constitution only when the runtime is already initialized, and a
        // second initializer in this binary would poison the process-global
        // runtime for `timeline_jsonrpc`'s router (different tempdir
        // gateway_dir). Whichever neighbor initializes first is fine.
        let ws = TestWorkspace::new().expect("workspace");
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        let config = ws.gateway_config();

        // Minimal agent dirs (SKILL.md stub) for two orchestrators.
        for id in ["planner.default", "planner.collaborative"] {
            let dir = config.agents_dir.join(id);
            std::fs::create_dir_all(&dir).expect("agent dir");
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: \"{id}\"\ndescription: stub\n---\n# {id}\n"),
            )
            .expect("skill");
            seed_agent_revision(&store, &config, id, &dir).expect("seed revision");
        }

        SharedEnv { ws, store }
    })
}

fn config() -> autonoetic_types::config::GatewayConfig {
    shared().ws.gateway_config()
}

/// Bind a session to `agent_id` the way a first ingest would.
fn bind_session(session_id: &str, agent_id: &str) {
    let env = shared();
    let alias = env
        .store
        .resolve_alias(agent_id)
        .expect("alias")
        .expect("seeded alias");
    let revision = env
        .store
        .get_agent_revision(&alias.revision_id)
        .expect("revision")
        .expect("seeded revision");
    env.store
        .upsert_session_agent_binding(&SessionAgentBinding {
            session_id: session_id.to_string(),
            root_session_id: session_id.to_string(),
            alias_id: Some(alias.alias_id.clone()),
            agent_id: agent_id.to_string(),
            revision_id: revision.revision_id.clone(),
            runtime_lock_hash: revision.runtime_lock_hash.clone(),
            constitution_version: None,
            constitution_digest: None,
            home_node_id: "gateway".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            requested_target: agent_id.to_string(),
        })
        .expect("bind");
}

fn handoff(session_id: &str, target: &str, reason: Option<&str>) -> Result<HandoffOutcome, String> {
    let env = shared();
    perform_handoff(
        &config(),
        &env.store,
        &HandoffParams {
            session_id: session_id.to_string(),
            target_agent_id: target.to_string(),
            reason: reason.map(str::to_string),
            context_note: None,
        },
    )
}

#[test]
fn handoff_rebinds_the_session_and_records_everything() {
    let env = shared();
    let sid = "session-handoff-happy";
    bind_session(sid, "planner.default");

    let outcome = handoff(
        sid,
        "planner.collaborative",
        Some("task needs plan co-editing"),
    )
    .expect("handoff must succeed");
    assert!(outcome.ok);
    assert_eq!(outcome.from_agent_id, "planner.default");
    assert_eq!(outcome.to_agent_id, "planner.collaborative");

    // Binding now points at the successor with its active revision.
    let binding = env
        .store
        .get_session_agent_binding(sid)
        .expect("binding read")
        .expect("binding exists");
    assert_eq!(binding.agent_id, "planner.collaborative");
    let alias = env
        .store
        .resolve_alias("planner.collaborative")
        .expect("alias")
        .expect("alias exists");
    assert_eq!(binding.revision_id, alias.revision_id);

    // Causal event recorded on the root session.
    let events = env
        .store
        .search_causal_events(Some(sid), None, 10)
        .expect("causal search");
    let handoff_event = events
        .iter()
        .find(|e| e.action == "handoff" && e.category == "session")
        .expect("handoff causal event");
    assert_eq!(handoff_event.status, "completed");

    // Successor context envelope seeded (assert via the public load API).
    let ctx = autonoetic_gateway::runtime::session_context::SessionContext::load(
        &config().agents_dir.join("planner.collaborative"),
        sid,
    )
    .expect("successor context seeded");
    assert!(
        ctx.current_topic
            .as_deref()
            .unwrap_or("")
            .contains("Handoff from planner.default"),
        "topic names the transition: {:?}",
        ctx.current_topic
    );
}

#[test]
fn handoff_folds_operator_context_note_into_envelope() {
    let sid = "session-handoff-note";
    bind_session(sid, "planner.default");
    let env = shared();
    let outcome = perform_handoff(
        &config(),
        &env.store,
        &HandoffParams {
            session_id: sid.to_string(),
            target_agent_id: "planner.collaborative".to_string(),
            reason: Some("needs plans".to_string()),
            context_note: Some("coder artifact ready; next step is federation".to_string()),
        },
    )
    .expect("handoff");
    assert!(outcome.ok);
    let ctx = autonoetic_gateway::runtime::session_context::SessionContext::load(
        &config().agents_dir.join("planner.collaborative"),
        sid,
    )
    .expect("successor context");
    assert!(
        ctx.known_facts
            .iter()
            .any(|f| f.label == "handoff_note" && f.value.contains("federation")),
        "operator context note folded in: {:?}",
        ctx.known_facts
    );
    assert!(
        ctx.known_facts.iter().any(|f| f.label == "handoff"),
        "mechanical handoff fact recorded: {:?}",
        ctx.known_facts
    );
}

#[test]
fn handoff_refuses_unbound_session() {
    let err = handoff("session-never-bound", "planner.collaborative", None)
        .expect_err("must fail");
    assert!(err.contains("no agent binding"), "{err}");
}

#[test]
fn handoff_refuses_unknown_target() {
    let sid = "session-handoff-unknown-target";
    bind_session(sid, "planner.default");
    let err = handoff(sid, "nonexistent.agent", None).expect_err("must fail");
    assert!(err.contains("not installed"), "{err}");
    // Binding unchanged.
    let binding = shared()
        .store
        .get_session_agent_binding(sid)
        .expect("binding read")
        .expect("binding exists");
    assert_eq!(binding.agent_id, "planner.default");
}

#[test]
fn handoff_refuses_noop_to_same_agent() {
    let sid = "session-handoff-noop";
    bind_session(sid, "planner.default");
    let err = handoff(sid, "planner.default", None).expect_err("must fail");
    assert!(err.contains("already bound"), "{err}");
}

/// The no-op guard compares the RESOLVED logical agent, not the requested
/// string. A second alias for the bound agent would be the bypass case — but
/// the store enforces one alias per agent (`idx_agent_aliases_agent`
/// UNIQUE(agent_id), every upsert site writes alias_id == agent_id), so the
/// practical protection is against revision drift (same agent id, different
/// revision): still a refused no-op with a "promote the alias" hint, never a
/// pointless rebind of the same logical agent.
#[test]
fn handoff_noop_error_names_the_bound_agent() {
    let sid = "session-handoff-noop";
    bind_session(sid, "planner.default");
    let err = handoff(sid, "planner.default", None).expect_err("must fail");
    assert!(
        err.contains("already bound to agent 'planner.default'"),
        "got: {err}"
    );
}

/// PR review: handoff is a root-session primitive — a child session's binding
/// must be refused so the causal event / room bookkeeping cannot split.
#[test]
fn handoff_refuses_child_session_binding() {
    let env = shared();
    let sid = "session-handoff-child-1";
    // Bind a CHILD session: root differs from the session id (the shape the
    // gateway writes for spawned children — `root/child` ids).
    let alias = env
        .store
        .resolve_alias("planner.default")
        .expect("alias")
        .expect("seeded");
    env.store
        .upsert_session_agent_binding(&SessionAgentBinding {
            session_id: sid.to_string(),
            root_session_id: "session-handoff-child-root".to_string(),
            alias_id: Some(alias.alias_id.clone()),
            agent_id: "planner.default".to_string(),
            revision_id: alias.revision_id.clone(),
            runtime_lock_hash: "sha256:seed-lock".to_string(),
            constitution_version: None,
            constitution_digest: None,
            home_node_id: "gateway".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            requested_target: "planner.default".to_string(),
        })
        .expect("bind child");
    let err = handoff(sid, "planner.collaborative", None).expect_err("must fail");
    assert!(err.contains("child of root"), "{err}");
    let after = env
        .store
        .get_session_agent_binding(sid)
        .expect("binding")
        .expect("bound");
    assert_eq!(after.agent_id, "planner.default");
}

#[test]
fn handoff_updates_parked_residency_agent() {
    let env = shared();
    let sid = "session-handoff-resident";
    bind_session(sid, "planner.default");
    // Park the session under the outgoing agent, as a resident idle would.
    env.store
        .upsert_session_residency(&autonoetic_gateway::scheduler::gateway_store::SessionResidency {
            session_id: sid.to_string(),
            root_session_id: sid.to_string(),
            agent_id: "planner.default".to_string(),
            turn_id: "turn-000001".to_string(),
            since: chrono::Utc::now().to_rfc3339(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(600)).to_rfc3339(),
        })
        .expect("park");

    handoff(sid, "planner.collaborative", None).expect("handoff");

    let residency = env
        .store
        .get_session_residency(sid)
        .expect("residency")
        .expect("still parked");
    assert_eq!(residency.agent_id, "planner.collaborative");
}

/// The RPC arm is thin plumbing around `perform_handoff`; this pins the wire
/// shape so a renamed field cannot drift silently (`session.handoff` params →
/// `HandoffParams`).
#[test]
fn handoff_params_wire_round_trip() {
    let v = serde_json::json!({
        "session_id": "session-x",
        "target_agent_id": "planner.collaborative",
        "reason": "needs plan co-editing",
    });
    let p: HandoffParams = serde_json::from_value(v).expect("parse");
    assert_eq!(p.session_id, "session-x");
    assert_eq!(p.target_agent_id, "planner.collaborative");
    assert_eq!(p.reason.as_deref(), Some("needs plan co-editing"));
    assert_eq!(p.context_note, None);
    // Optional fields stay optional.
    let p: HandoffParams = serde_json::from_value(serde_json::json!({
        "session_id": "s",
        "target_agent_id": "t",
    }))
    .expect("minimal parse");
    assert_eq!(p.reason, None);
}
