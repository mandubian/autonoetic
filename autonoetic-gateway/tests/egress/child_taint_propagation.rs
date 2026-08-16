//! Downward taint propagation: parent → locally spawned child (RFC §5.5). #982.
//!
//! Taint already flowed *up* (a surfaced child's taint intersects into the
//! parent's tool result), *laterally* (`ecosystem.send_message` stamps the
//! sender's taint onto the payload), and *inward from federation* (an OFP
//! inbound session is seeded from the peer's wire label). The downward direction
//! was the one left open: a child spawned out of a tainted parent started clean,
//! so the delegation instruction — derived from the parent's private context —
//! arrived as an *unlabeled* first user turn and was shipped to whatever
//! provider the child's own routing picked.
//!
//! The chain under test, which is the tool-call plumbing's semantics without the
//! plumbing (same integration boundary as the other egress suites):
//!
//! 1. `resolve_session_egress_taint` reads the parent's **live** taint from the
//!    run context — the stored row is only written at finalize, so mid-turn it is
//!    stale or absent, and reading the store alone would propagate nothing.
//! 2. that taint is stamped into spawn metadata under
//!    [`PARENT_TAINT_METADATA_KEY`] (by `agent_spawn`, gateway-side);
//! 3. `resolve_ingest_turn_label` intersects it into the child's first-turn label;
//! 4. `restrict_session_egress_taint` folds it into the child's session taint.

use std::sync::Arc;

use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_labeler::{
    plan_taint_following_route, resolve_ingest_turn_label, resolve_session_egress_taint,
    strip_ingest_label_keys, PresetCandidate, INGEST_LABEL_METADATA_KEYS,
    PARENT_TAINT_METADATA_KEY,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressClass, EgressLabel, NamedEgressLabel, Sink};

fn cand(name: &str, class: EgressClass) -> PresetCandidate {
    PresetCandidate {
        name: name.to_string(),
        egress_class: Some(class),
    }
}

fn run_ctx(session_id: &str, taint: Option<EgressLabel>) -> NativeToolRunContext {
    NativeToolRunContext {
        registry: ActiveExecutionRegistry::new(),
        root_session_id: session_id
            .split('/')
            .next()
            .unwrap_or(session_id)
            .to_string(),
        workflow_id: None,
        task_id: None,
        session_id: session_id.into(),
        agent_id: "parent.agent".into(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: None,
        discovered_tools: None,
            annotation_counter: None,
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
        egress_taint: taint,
        egress_query_sink: None,
    }
}

/// What `agent_spawn` stamps: the parent's taint, restrictive ones only.
fn spawn_metadata_for(parent_taint: Option<EgressLabel>) -> serde_json::Value {
    let mut meta = serde_json::json!({ "_autonoetic_spawn_message": "summarize this" });
    if let Some(t) = parent_taint.filter(|t| !t.is_unrestricted()) {
        meta[PARENT_TAINT_METADATA_KEY] = serde_json::to_value(&t).unwrap();
    }
    meta
}

/// Reserved keys are stripped from agent-authored metadata. `agent_spawn`
/// forwards model-supplied `metadata` into the child's ingest, where these keys
/// are read as *declarations* — so an agent could otherwise forge an operator
/// mark, a peer wire label, or a parent taint. Intersection bounds a forgery to
/// over-restriction rather than a leak, but label resolution must not be a
/// function of model output at all (I-14).
#[test]
fn agent_authored_metadata_cannot_smuggle_label_keys() {
    let mut meta = serde_json::json!({
        "operator_egress_label": serde_json::to_value(EgressLabel::local_only()).unwrap(),
        "ofp_inbound_egress_label": serde_json::to_value(EgressLabel::local_only()).unwrap(),
        "parent_egress_taint": serde_json::to_value(EgressLabel::local_only()).unwrap(),
        "keep_me": "an ordinary metadata value",
    });
    let stripped = strip_ingest_label_keys(&mut meta);

    assert_eq!(stripped.len(), 3, "every reserved key must be reported");
    for key in INGEST_LABEL_METADATA_KEYS {
        assert!(meta.get(key).is_none(), "{key} must be stripped");
    }
    assert_eq!(meta["keep_me"], "an ordinary metadata value");
    assert_eq!(
        resolve_ingest_turn_label(None, false, Some(&meta)),
        None,
        "a stripped payload declares nothing"
    );
}

/// The case an overwrite-only fix would miss: when the parent is **clean** the
/// gateway stamps nothing, so a forged key would survive untouched. Stripping
/// first is what closes it.
#[test]
fn a_forged_key_does_not_survive_a_clean_parent_spawn() {
    let mut meta = serde_json::json!({
        PARENT_TAINT_METADATA_KEY: serde_json::to_value(EgressLabel::local_only()).unwrap(),
    });
    // Gateway-side order: strip what the agent wrote, then stamp what was
    // actually resolved — which for a clean parent is nothing.
    strip_ingest_label_keys(&mut meta);
    let clean_parent: Option<EgressLabel> = None;
    if let Some(t) = clean_parent.filter(|t: &EgressLabel| !t.is_unrestricted()) {
        meta[PARENT_TAINT_METADATA_KEY] = serde_json::to_value(&t).unwrap();
    }
    assert!(meta.get(PARENT_TAINT_METADATA_KEY).is_none());
    assert_eq!(resolve_ingest_turn_label(None, false, Some(&meta)), None);
}

/// Non-object metadata is tolerated rather than panicking — `agent_spawn` wraps
/// a non-object under `_original_metadata`, and the stripper must be safe to call
/// on whatever shape arrives.
#[test]
fn stripping_is_safe_on_non_object_metadata() {
    let mut meta = serde_json::json!("just a string");
    assert!(strip_ingest_label_keys(&mut meta).is_empty());
    assert_eq!(meta, serde_json::json!("just a string"));
}

/// The whole point: a tainted parent's child is born tainted, and the label
/// reaches both the child's first turn and the child's session taint.
#[test]
fn tainted_parent_spawns_a_tainted_child() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let parent = "root-mail";
    let child = "root-mail/summarizer-abc123";

    // Parent is mid-turn: its taint is live in the run context and NOT yet in
    // the store (the row lands at finalize).
    let ctx = run_ctx(parent, Some(EgressLabel::local_only()));
    assert_eq!(store.get_session_egress_taint(parent)?, None);

    let parent_taint = resolve_session_egress_taint(Some(&ctx), Some(&store), Some(parent))?
        .expect("live parent taint");
    assert_eq!(parent_taint, EgressLabel::local_only());

    // Spawn stamps it; the child's ingest resolves it.
    let meta = spawn_metadata_for(Some(parent_taint));
    let child_label =
        resolve_ingest_turn_label(None, false, Some(&meta)).expect("child's first turn is labeled");
    assert_eq!(child_label, EgressLabel::local_only());
    assert!(!child_label.allows(Sink::RemoteModel));

    // And the child's session taint follows, so its own routing is constrained
    // even for turns after the first.
    let merged = store.restrict_session_egress_taint(child, &child_label)?;
    assert_eq!(merged, EgressLabel::local_only());
    assert_eq!(
        store.get_session_egress_taint(child)?,
        Some(EgressLabel::local_only())
    );
    Ok(())
}

/// The regression this closes: without the stamp, the child's first turn carries
/// no label at all and its session stays clean. Pinned so a refactor that drops
/// the stamp fails here rather than silently leaking.
#[test]
fn without_the_stamp_the_child_would_start_clean() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let child = "root-mail/summarizer-def456";

    let meta = serde_json::json!({ "_autonoetic_spawn_message": "summarize this" });
    assert_eq!(
        resolve_ingest_turn_label(None, false, Some(&meta)),
        None,
        "no stamp ⇒ no label — this is the hole #982 closes"
    );
    assert_eq!(store.get_session_egress_taint(child)?, None);
    Ok(())
}

/// A clean parent stamps nothing, so an unconfigured deployment pays no cost and
/// clean delegation chains stay remote-eligible (no over-tainting).
#[test]
fn clean_parent_spawns_a_clean_child() {
    let meta = spawn_metadata_for(None);
    assert!(meta.get(PARENT_TAINT_METADATA_KEY).is_none());
    assert_eq!(resolve_ingest_turn_label(None, false, Some(&meta)), None);

    // Explicitly unrestricted is also "nothing to carry".
    let meta = spawn_metadata_for(Some(EgressLabel::unrestricted()));
    assert!(meta.get(PARENT_TAINT_METADATA_KEY).is_none());
    assert_eq!(resolve_ingest_turn_label(None, false, Some(&meta)), None);
}

/// The live taint is preferred over the stored row. If the store were consulted
/// first, a parent that became tainted during the current turn would propagate
/// its *previous* (or absent) taint — the stale-read version of the same leak.
#[test]
fn live_parent_taint_wins_over_a_staler_stored_row() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let parent = "root-stale";

    // Stored row from an earlier turn is laxer than what the parent now holds.
    store.restrict_session_egress_taint(parent, &EgressLabel::no_remote_model())?;
    let ctx = run_ctx(parent, Some(EgressLabel::local_only()));

    let resolved = resolve_session_egress_taint(Some(&ctx), Some(&store), Some(parent))?
        .expect("resolved taint");
    assert_eq!(
        resolved,
        EgressLabel::local_only(),
        "the run context's live taint must win"
    );
    Ok(())
}

/// With no run context (e.g. a spawn dispatched outside a live parent turn) the
/// stored row is the fallback rather than nothing.
#[test]
fn stored_row_is_the_fallback_without_a_run_context() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let parent = "root-fallback";

    store.restrict_session_egress_taint(parent, &EgressLabel::local_only())?;
    let resolved = resolve_session_egress_taint(None, Some(&store), Some(parent))?
        .expect("stored taint used as fallback");
    assert_eq!(resolved, EgressLabel::local_only());
    Ok(())
}

/// A malformed stamp fails closed. Dropping it would turn a serialization bug
/// into a silent leak, which is the one outcome §2.2 forbids.
#[test]
fn malformed_parent_taint_stamp_fails_closed() {
    let meta = serde_json::json!({ PARENT_TAINT_METADATA_KEY: "not-a-label" });
    let label = resolve_ingest_turn_label(None, false, Some(&meta)).expect("fail-closed label");
    assert_eq!(label, EgressLabel::local_only());
}

/// Parent taint and the child's own session policy both apply — the child cannot
/// be laxer than either. Also covers the reverse: a parent taint cannot widen a
/// stricter room default.
#[test]
fn parent_taint_intersects_with_the_child_session_policy() {
    // Parent laxer than the room default → the default holds.
    let meta = spawn_metadata_for(Some(EgressLabel::no_remote_model()));
    let label = resolve_ingest_turn_label(Some(NamedEgressLabel::LocalOnly), false, Some(&meta))
        .expect("restricted");
    assert_eq!(label, EgressLabel::local_only());

    // Parent stricter than the room default → the parent holds.
    let meta = spawn_metadata_for(Some(EgressLabel::local_only()));
    let label = resolve_ingest_turn_label(Some(NamedEgressLabel::NoRemoteModel), false, Some(&meta))
        .expect("restricted");
    assert_eq!(label, EgressLabel::local_only());
}

/// The round trip, and the reason any of this matters: a child born tainted has
/// its provider *selection* constrained (so the private instruction is never
/// offered to a remote model even though the child's own bundle is
/// remote-capable), and its taint intersects back into the parent on return, so
/// the parent does not launder the result by surfacing it.
#[test]
fn a_tainted_child_routes_local_and_its_taint_returns_to_the_parent() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;
    let parent = "root-round";
    let child = "root-round/summarizer-ccc789";

    // Born tainted from the parent (the change under test).
    let ctx = run_ctx(parent, Some(EgressLabel::local_only()));
    let parent_taint =
        resolve_session_egress_taint(Some(&ctx), Some(&store), Some(parent))?.expect("parent taint");
    let meta = spawn_metadata_for(Some(parent_taint.clone()));
    let child_label = resolve_ingest_turn_label(None, false, Some(&meta)).expect("child labeled");
    store.restrict_session_egress_taint(child, &child_label)?;

    // Routing: the child's own primary is remote, but the taint makes it
    // ineligible and reroutes to the local preset.
    let presets = vec![
        cand("sonnet", EgressClass::Remote),
        cand("ollama", EgressClass::Local),
    ];
    let plan = plan_taint_following_route(
        &child_label,
        Some(EgressClass::Remote),
        &presets,
        None,
    );
    assert!(
        !plan.primary_eligible,
        "a remote primary must not be eligible for a tainted child"
    );
    assert_eq!(
        plan.reroute_to.as_ref().map(|c| c.name.as_str()),
        Some("ollama")
    );

    // Return: the parent intersects the child's taint, so surfacing the child's
    // result cannot widen anything.
    let parent_after = parent_taint.restrict(
        &store
            .get_session_egress_taint(child)?
            .expect("child taint stored"),
    );
    assert_eq!(parent_after, EgressLabel::local_only());
    Ok(())
}

/// Grandchildren: taint keeps flowing down a delegation chain, because each hop
/// stamps from the taint that hop actually holds.
#[test]
fn taint_flows_down_a_multi_level_delegation_chain() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let child = "root-chain/mid-aaa";
    let grandchild = "root-chain/mid-aaa/leaf-bbb";

    // Child was born tainted (hop 1, proven above).
    store.restrict_session_egress_taint(child, &EgressLabel::local_only())?;

    // Hop 2: the child spawns, with no live context of its own — the stored row
    // carries it.
    let hop2 = resolve_session_egress_taint(None, Some(&store), Some(child))?.expect("child taint");
    let meta = spawn_metadata_for(Some(hop2));
    let leaf_label = resolve_ingest_turn_label(None, false, Some(&meta)).expect("leaf labeled");
    assert_eq!(leaf_label, EgressLabel::local_only());

    store.restrict_session_egress_taint(grandchild, &leaf_label)?;
    assert_eq!(
        store.get_session_egress_taint(grandchild)?,
        Some(EgressLabel::local_only())
    );
    Ok(())
}
