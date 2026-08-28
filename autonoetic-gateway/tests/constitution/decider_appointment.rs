//! P-2.20 / §3.2 / §4.4: seating a decider for a run (#1195, umbrella #1191).
//!
//! The capability says an agent *may* hold the seat; an appointment is the
//! operator act that seats one. These tests pin the refusals that make the act
//! safe — an appointment never widens capabilities, `Critical` is not
//! delegable at all, and phase 1 is advisory-only — plus the coverage and
//! expiry semantics that routing (#1197) will depend on.

use std::path::PathBuf;

use autonoetic_gateway::decider_appointment::{
    active_appointment_for_gate, agent_is_appointed_for_scope, appoint, revoke,
    AppointmentRequest,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::ApprovalRisk;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

// Reuse the bundle/revision fixtures from the sibling suite rather than
// restating them: both suites need "an installed agent with these
// capabilities", and two copies would drift.
use crate::gate_decider::{seed_decider_revision, write_agent_dir};

struct Fixture {
    _temp: tempfile::TempDir,
    cfg: GatewayConfig,
    store: GatewayStore,
}

/// Installs `nightwatch.default` holding `GateDecider { kinds: ["approval"] }`
/// unless different capabilities are given.
fn fixture(agent_id: &str, capabilities: &[Capability]) -> anyhow::Result<Fixture> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    write_agent_dir(&agents_dir, agent_id, capabilities);
    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };
    let store = GatewayStore::open(&gateway_dir)?;
    seed_decider_revision(&agents_dir, &gateway_dir, &store, agent_id)?;
    Ok(Fixture {
        _temp: temp,
        cfg,
        store,
    })
}

fn request(agent: &str, scope: &str) -> AppointmentRequest {
    AppointmentRequest {
        decider_agent: agent.to_string(),
        kinds: vec!["approval".to_string()],
        scope_root_session: scope.to_string(),
        risk_ceiling: ApprovalRisk::High,
        advice_only: true,
        expires_at: None,
        max_gates: None,
        appointed_by: "operator".to_string(),
    }
}

fn approval_decider() -> Vec<Capability> {
    vec![Capability::GateDecider {
        kinds: vec!["approval".to_string()],
    }]
}

// ── An appointment never widens capabilities ────────────────────────────────

#[test]
fn appointing_an_agent_without_gate_decider_is_refused() -> anyhow::Result<()> {
    let f = fixture(
        "plain.default",
        &[Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }],
    )?;
    let err = appoint(&f.cfg, &f.store, request("plain.default", "root-1"))
        .expect_err("an agent without GateDecider cannot be seated");
    assert!(
        err.to_string().contains("GateDecider"),
        "refusal should name the missing capability: {err}"
    );
    Ok(())
}

#[test]
fn appointing_for_a_kind_the_agent_does_not_hold_is_refused() -> anyhow::Result<()> {
    // Holds GateDecider for approvals only; appointed for escalations.
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.kinds = vec!["escalation".to_string()];
    let err = appoint(&f.cfg, &f.store, req)
        .expect_err("appointment must not widen the capability's kinds");
    assert!(
        err.to_string().contains("escalation"),
        "refusal should name the kind not held: {err}"
    );
    Ok(())
}

#[test]
fn appointing_an_uninstalled_agent_is_refused() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let err = appoint(&f.cfg, &f.store, request("ghost.default", "root-1"))
        .expect_err("an appointment names an existing holder, it does not create one");
    assert!(
        err.to_string().contains("does not resolve"),
        "refusal should say the agent is not installed: {err}"
    );
    Ok(())
}

// ── What is not delegable at all ────────────────────────────────────────────

#[test]
fn critical_ceiling_is_refused_at_appointment_time() -> anyhow::Result<()> {
    // Refused when the appointment is made, not merely left above a ceiling a
    // later edit could raise: promotion and secret delivery must not become
    // delegable by one operator gesture.
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.risk_ceiling = ApprovalRisk::Critical;
    let err = appoint(&f.cfg, &f.store, req).expect_err("Critical is not appointable");
    assert!(
        err.to_string().contains("not appointable"),
        "refusal should say Critical is not appointable: {err}"
    );
    Ok(())
}

#[test]
fn binding_appointment_is_refused_in_phase_1() -> anyhow::Result<()> {
    // §4.4: a judgment layer earns binding authority from measured agreement,
    // so `advice_only: false` is refused with the reason rather than silently
    // downgraded to advisory.
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.advice_only = false;
    let err = appoint(&f.cfg, &f.store, req).expect_err("binding mode does not exist yet");
    assert!(
        err.to_string().contains("advisory-only"),
        "refusal should explain the advisory stage: {err}"
    );
    Ok(())
}

#[test]
fn a_scopeless_or_kindless_appointment_is_refused() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;

    let mut no_scope = request("nightwatch.default", "");
    no_scope.scope_root_session = String::new();
    assert!(appoint(&f.cfg, &f.store, no_scope).is_err());

    let mut no_kinds = request("nightwatch.default", "root-1");
    no_kinds.kinds = vec![];
    assert!(appoint(&f.cfg, &f.store, no_kinds).is_err());
    Ok(())
}

// ── The record, and its trace ───────────────────────────────────────────────

#[test]
fn appointment_round_trips_and_lands_on_the_causal_chain() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;

    let stored = f
        .store
        .get_decider_appointment(&a.appointment_id)?
        .expect("appointment should be readable back");
    assert_eq!(stored.decider_agent, "nightwatch.default");
    assert_eq!(stored.scope_root_session, "root-1");
    assert_eq!(stored.risk_ceiling, ApprovalRisk::High);
    assert!(stored.advice_only, "phase 1 is advisory-only");
    assert_eq!(stored.appointed_by, "operator");
    assert!(
        stored.decider_session.is_none(),
        "the peer-root session is created later (#1196); an appointment is a \
         record of authority, not of a running process"
    );

    let events = f
        .store
        .search_causal_events(None, Some("nightwatch.default"), 10)?;
    let appointed = events
        .iter()
        .find(|e| e.action == "decider.appointed")
        .expect("appointing must be on the chain — it is a power act");
    assert!(appointed
        .enforced_rules
        .iter()
        .any(|r| r == "P-2.20"));
    Ok(())
}

#[test]
fn an_appointment_with_no_expiry_is_reported_as_standing() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;
    assert!(
        a.is_standing(),
        "an appointment that never expires is a standing grant and should look like one"
    );
    Ok(())
}

// ── Revocation ──────────────────────────────────────────────────────────────

#[test]
fn revocation_records_attribution_and_is_idempotent() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;

    assert!(revoke(&f.store, &a.appointment_id, "pascal", Some("shift over"))?);
    let after = f
        .store
        .get_decider_appointment(&a.appointment_id)?
        .expect("revocation must not delete the record");
    assert_eq!(after.revoked_by.as_deref(), Some("pascal"));
    assert_eq!(after.revoked_reason.as_deref(), Some("shift over"));
    let first_revoked_at = after.revoked_at.clone();

    // A second revoke is a no-op — it must not rewrite the first one's
    // attribution.
    assert!(!revoke(&f.store, &a.appointment_id, "someone-else", Some("again"))?);
    let again = f
        .store
        .get_decider_appointment(&a.appointment_id)?
        .expect("still there");
    assert_eq!(again.revoked_by.as_deref(), Some("pascal"));
    assert_eq!(again.revoked_at, first_revoked_at);

    let events = f
        .store
        .search_causal_events(None, Some("nightwatch.default"), 10)?;
    assert!(
        events.iter().any(|e| e.action == "decider.revoked"),
        "revocation must be on the chain too"
    );
    Ok(())
}

#[test]
fn revoking_vacates_the_seat_for_provenance_and_routing() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;

    assert!(agent_is_appointed_for_scope(
        &f.store,
        "nightwatch.default",
        "root-1"
    )?);
    assert!(active_appointment_for_gate(&f.store, "root-1", "approval", ApprovalRisk::High)?
        .is_some());

    revoke(&f.store, &a.appointment_id, "operator", None)?;

    assert!(
        !agent_is_appointed_for_scope(&f.store, "nightwatch.default", "root-1")?,
        "a revoked appointment must not satisfy the decide path's provenance condition"
    );
    assert!(
        active_appointment_for_gate(&f.store, "root-1", "approval", ApprovalRisk::High)?.is_none(),
        "a revoked appointment must not route gates"
    );
    Ok(())
}

// ── Coverage and expiry ─────────────────────────────────────────────────────

#[test]
fn ceiling_bounds_which_gates_route() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.risk_ceiling = ApprovalRisk::Standard;
    appoint(&f.cfg, &f.store, req)?;

    assert!(
        active_appointment_for_gate(&f.store, "root-1", "approval", ApprovalRisk::Standard)?
            .is_some()
    );
    // The Night Shift consequence: `sandbox_exec` with detected hosts is High,
    // so a Standard-ceiling night watch decides neither demo gate.
    assert!(
        active_appointment_for_gate(&f.store, "root-1", "approval", ApprovalRisk::High)?.is_none(),
        "a gate above the ceiling parks for the operator"
    );
    Ok(())
}

#[test]
fn kind_bounds_which_gates_route() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;
    assert!(
        active_appointment_for_gate(&f.store, "root-1", "escalation", ApprovalRisk::Standard)?
            .is_none(),
        "an appointment for approvals does not route escalations"
    );
    Ok(())
}

#[test]
fn scope_bounds_which_gates_route() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;
    assert!(
        active_appointment_for_gate(&f.store, "root-2", "approval", ApprovalRisk::Standard)?
            .is_none(),
        "an appointment is run-scoped, not a standing global grant"
    );
    assert!(
        !agent_is_appointed_for_scope(&f.store, "nightwatch.default", "root-2")?,
        "and the same holds for the provenance question"
    );
    Ok(())
}

#[test]
fn a_past_expiry_stops_routing() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.expires_at = Some("2000-01-01T00:00:00Z".to_string());
    appoint(&f.cfg, &f.store, req)?;
    assert!(
        active_appointment_for_gate(&f.store, "root-1", "approval", ApprovalRisk::Standard)?
            .is_none(),
        "an expired appointment must not route"
    );
    Ok(())
}

#[test]
fn a_malformed_expiry_is_refused_rather_than_ignored() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.expires_at = Some("tonight".to_string());
    let err = appoint(&f.cfg, &f.store, req).expect_err("a bad expiry must not become 'never'");
    assert!(
        err.to_string().contains("RFC3339"),
        "refusal should name the expected format: {err}"
    );
    Ok(())
}

#[test]
fn gate_count_expiry_is_independent_of_the_clock() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.max_gates = Some(2);
    let a = appoint(&f.cfg, &f.store, req)?;
    assert!(!a.is_standing(), "a gate-count bound is an expiry");

    f.store.record_decider_gate_decided(&a.appointment_id)?;
    assert!(
        active_appointment_for_gate(&f.store, "root-1", "approval", ApprovalRisk::Standard)?
            .is_some(),
        "one gate of two used — still seated"
    );

    f.store.record_decider_gate_decided(&a.appointment_id)?;
    assert!(
        active_appointment_for_gate(&f.store, "root-1", "approval", ApprovalRisk::Standard)?
            .is_none(),
        "the count is reached, so the seat is vacant even though the clock never ran out"
    );
    Ok(())
}

#[test]
fn appointment_pins_the_revision_it_seated() -> anyhow::Result<()> {
    // An agent id is not a stable thing to have seated: the revision carries
    // the instructions, the capabilities and the model. Calibration evidence
    // (#1198) belongs to the closure that produced the verdicts, so the record
    // says which one it was — a later promotion cannot retroactively change
    // what this appointment seated.
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;
    let pinned = a
        .decider_revision
        .as_deref()
        .expect("the seated revision must be recorded");
    assert!(
        pinned.starts_with("rev_sha256:"),
        "expected a revision id, got {pinned}"
    );

    let stored = f
        .store
        .get_decider_appointment(&a.appointment_id)?
        .expect("readable back");
    assert_eq!(stored.decider_revision.as_deref(), Some(pinned));
    Ok(())
}
