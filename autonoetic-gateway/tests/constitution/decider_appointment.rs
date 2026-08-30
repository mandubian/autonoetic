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
    viewer_class_for_gate, AppointmentRequest,
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
    // A fixed preset, as an operator's config would define it. `appoint`
    // resolves it to record what was seated, and refuses routing presets.
    let mut llm_presets = std::collections::HashMap::new();
    llm_presets.insert(
        "decider".to_string(),
        autonoetic_types::config::LlmPreset {
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-20250514".to_string()),
            temperature: Some(0.0),
            fallback_provider: None,
            fallback_model: None,
            chat_only: None,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            thinking: None,
            tier: None,
            cost: None,
            latency: None,
            routing: None,
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        llm_presets,
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
    // #1196: the peer-root session exists as soon as the seat does, and is
    // **top-level** — that is what puts it outside the run for R-10.7, budget,
    // emergency stop, session grants and content-visibility push, with no
    // hand-written exclusion for any of them.
    let session = stored
        .decider_session
        .as_deref()
        .expect("the peer-root session is created with the appointment");
    assert!(
        !session.contains('/'),
        "a decider session must be top-level, not a child of anything: {session}"
    );
    assert!(
        !session.starts_with(&stored.scope_root_session),
        "and must not share a root with the run it decides for: {session}"
    );

    // The model is pinned alongside the revision (#1232 down payment).
    assert_eq!(stored.decider_provider.as_deref(), Some("anthropic"));
    assert_eq!(stored.decider_model.as_deref(), Some("claude-opus-4-20250514"));

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
    assert!(
        a.decider_revision.starts_with("rev_sha256:"),
        "expected a revision id, got {}",
        a.decider_revision
    );

    let stored = f
        .store
        .get_decider_appointment(&a.appointment_id)?
        .expect("readable back");
    assert_eq!(stored.decider_revision, a.decider_revision);
    Ok(())
}

// ── Read parity attaches to the seat, not the identity (#1194) ─────────────

#[test]
fn a_seated_decider_reads_at_decider_class() -> anyhow::Result<()> {
    use autonoetic_types::disclosure::ViewerClass;
    let f = fixture("nightwatch.default", &approval_decider())?;
    appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;

    assert_eq!(
        viewer_class_for_gate(&f.store, "nightwatch.default", "root-1/coder-a"),
        ViewerClass::Decider,
        "a seated decider must see the command it is asked to judge"
    );
    Ok(())
}

#[test]
fn the_same_agent_stays_at_agent_class_outside_its_scope() -> anyhow::Result<()> {
    use autonoetic_types::disclosure::ViewerClass;
    let f = fixture("nightwatch.default", &approval_decider())?;
    appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;

    // Same identity, different run. Nothing was granted to the agent, so
    // nothing follows it across scopes — this is what "office before occupant"
    // means mechanically.
    assert_eq!(
        viewer_class_for_gate(&f.store, "nightwatch.default", "root-2/coder-a"),
        ViewerClass::Agent
    );
    Ok(())
}

#[test]
fn an_unseated_agent_reads_at_agent_class() -> anyhow::Result<()> {
    use autonoetic_types::disclosure::ViewerClass;
    let f = fixture("nightwatch.default", &approval_decider())?;
    assert_eq!(
        viewer_class_for_gate(&f.store, "nightwatch.default", "root-1/coder-a"),
        ViewerClass::Agent,
        "holding GateDecider is not itself a read right"
    );
    Ok(())
}

#[test]
fn revocation_removes_the_read_right_it_never_granted_to_the_agent() -> anyhow::Result<()> {
    use autonoetic_types::disclosure::ViewerClass;
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;
    assert_eq!(
        viewer_class_for_gate(&f.store, "nightwatch.default", "root-1/coder-a"),
        ViewerClass::Decider
    );

    revoke(&f.store, &a.appointment_id, "operator", None)?;

    assert_eq!(
        viewer_class_for_gate(&f.store, "nightwatch.default", "root-1/coder-a"),
        ViewerClass::Agent,
        "revocation is real precisely because the right was the seat's, not the agent's"
    );
    Ok(())
}

#[test]
fn an_expired_appointment_stops_conferring_read_parity() -> anyhow::Result<()> {
    use autonoetic_types::disclosure::ViewerClass;
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.expires_at = Some("2000-01-01T00:00:00Z".to_string());
    appoint(&f.cfg, &f.store, req)?;
    assert_eq!(
        viewer_class_for_gate(&f.store, "nightwatch.default", "root-1/coder-a"),
        ViewerClass::Agent,
        "an expired seat confers nothing"
    );
    Ok(())
}

// ── #1196: the peer-root session, and what it buys ─────────────────────────

/// The decider's session shares no root with the run, which is the whole
/// mechanism. `root_session_id` is the first path segment, so budget rollup,
/// emergency stop, session approval grants and content-visibility propagation
/// are all keyed on a root the run does not share — none of them needs an
/// exclusion written for the decider.
#[test]
fn the_decider_session_shares_no_root_with_the_run() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;
    let session = a.decider_session.as_deref().expect("session created");

    // Same derivation the gateway uses everywhere else.
    let decider_root = session.split('/').next().unwrap_or(session);
    assert_eq!(decider_root, session, "the decider session *is* its own root");
    assert_ne!(
        decider_root, "root-1",
        "sharing a root would put the decider inside the run's budget, stop \
         scope, grants and content visibility"
    );
    Ok(())
}

/// R-10.7 authenticates the decider against recorded session ownership, so the
/// seat is useless unless the gateway registered that ownership. The principal
/// is the appointing operator, so the chain reads as delegation, not spawn.
#[test]
fn the_decider_session_is_owned_by_the_agent_and_principaled_to_the_operator() -> anyhow::Result<()>
{
    let f = fixture("nightwatch.default", &approval_decider())?;
    let mut req = request("nightwatch.default", "root-1");
    req.appointed_by = "pascal".to_string();
    let a = appoint(&f.cfg, &f.store, req)?;
    let session = a.decider_session.as_deref().expect("session created");

    assert_eq!(
        f.store.session_owner_agent(session)?.as_deref(),
        Some("nightwatch.default"),
        "R-10.7 authenticates against this; without it the seat cannot be used"
    );

    assert_eq!(
        f.store.session_principal(session)?.as_deref(),
        Some("pascal"),
        "the appointing operator is the principal — the chain must read as \
         delegation, not as an agent spawning itself a judge"
    );
    Ok(())
}

/// Two appointments never collide on one session, so revoking one seat cannot
/// disturb another.
#[test]
fn each_appointment_gets_its_own_session() -> anyhow::Result<()> {
    let f = fixture("nightwatch.default", &approval_decider())?;
    let a = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))?;
    let b = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-2"))?;
    assert_ne!(a.decider_session, b.decider_session);
    Ok(())
}

/// A routing preset picks a model per call, so the same seat would be served by
/// different models on different gates — and the agreement rate that is meant
/// to justify binding authority would be an average over an unknown mixture.
#[test]
fn a_routing_preset_cannot_be_seated() -> anyhow::Result<()> {
    let mut f = fixture("nightwatch.default", &approval_decider())?;
    // A routing preset that *does* resolve — otherwise the refusal would come
    // from resolution failing, and the routing check would go untested.
    let fixed = f
        .cfg
        .llm_presets
        .get("decider")
        .cloned()
        .expect("fixture defines a fixed decider preset");
    f.cfg.llm_presets.insert("opus_fixed".to_string(), fixed);
    f.cfg.llm_presets.insert(
        "decider".to_string(),
        autonoetic_types::config::LlmPreset {
            provider: None,
            model: None,
            temperature: None,
            fallback_provider: None,
            fallback_model: None,
            chat_only: None,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            thinking: None,
            tier: None,
            cost: None,
            latency: None,
            routing: Some(autonoetic_types::config::RoutingPresetConfig {
                models: vec!["opus_fixed".to_string()],
                ..Default::default()
            }),
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    let err = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))
        .expect_err("a routing preset must not be seatable");
    assert!(
        err.to_string().contains("fixed preset"),
        "refusal should explain why routing is disqualifying: {err}"
    );
    Ok(())
}

/// An agent whose preset does not resolve could not decide anything, so the
/// refusal belongs at appointment time rather than at 3am.
#[test]
fn an_unresolvable_preset_is_refused_at_appointment_time() -> anyhow::Result<()> {
    let mut f = fixture("nightwatch.default", &approval_decider())?;
    f.cfg.llm_presets.clear();
    let err = appoint(&f.cfg, &f.store, request("nightwatch.default", "root-1"))
        .expect_err("an unresolvable preset must be refused");
    assert!(
        err.to_string().contains("llm_presets"),
        "refusal should point at the fix: {err}"
    );
    Ok(())
}

/// An existing database that has already applied v82 must gain the model-pin
/// columns, not just a freshly created one.
///
/// This is the case the first version of #1196 got wrong: the columns were
/// added by editing v82's `CREATE TABLE`, which only ever runs on a database
/// that has not applied v82 yet. A released schema version cannot be edited in
/// place — every deployment that had run it would keep the old table while the
/// new code inserted columns that were not there.
#[test]
fn a_database_created_before_the_model_pin_is_upgraded_not_broken() -> anyhow::Result<()> {
    use rusqlite::Connection;

    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    // Stand up the v82-era table shape by hand, and claim v83 so the migrator
    // treats this as a database that predates the pin.
    {
        let db = gateway_dir.join("gateway.db");
        let conn = Connection::open(&db)?;
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at TEXT NOT NULL);
             CREATE TABLE decider_appointments (
                appointment_id TEXT PRIMARY KEY,
                decider_agent TEXT NOT NULL,
                decider_revision TEXT NOT NULL,
                kinds TEXT NOT NULL,
                scope_root_session TEXT NOT NULL,
                decider_session TEXT,
                risk_ceiling TEXT NOT NULL,
                advice_only INTEGER NOT NULL DEFAULT 1,
                expires_at TEXT,
                max_gates INTEGER,
                gates_decided INTEGER NOT NULL DEFAULT 0,
                appointed_by TEXT NOT NULL,
                appointed_at TEXT NOT NULL,
                revoked_at TEXT,
                revoked_by TEXT,
                revoked_reason TEXT);
             INSERT INTO schema_migrations (version, name, applied_at)
                VALUES (83, 'pre-existing', '2026-01-01T00:00:00Z');",
        )?;
    }

    // Opening the store runs the migrator over that database.
    let store = GatewayStore::open(&gateway_dir)?;

    // The round trip is the real assertion: insert and read back a row naming
    // the new columns. A missing column fails here, which is exactly how the
    // unmigrated deployment would have failed at runtime.
    use autonoetic_types::decider_appointment::DeciderAppointment;
    store.insert_decider_appointment(&DeciderAppointment {
        appointment_id: "apt-upgrade".to_string(),
        decider_agent: "nightwatch.default".to_string(),
        decider_revision: "rev-x".to_string(),
        decider_provider: Some("anthropic".to_string()),
        decider_model: Some("claude-opus-4-20250514".to_string()),
        kinds: vec!["approval".to_string()],
        scope_root_session: "root-1".to_string(),
        decider_session: Some("decider-x".to_string()),
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
    })?;

    let back = store
        .get_decider_appointment("apt-upgrade")?
        .expect("readable back after upgrade");
    assert_eq!(back.decider_model.as_deref(), Some("claude-opus-4-20250514"));
    assert_eq!(back.decider_provider.as_deref(), Some("anthropic"));
    Ok(())
}
