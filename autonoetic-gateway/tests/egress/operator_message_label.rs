//! Operator-declared taint on an incoming user-role turn (RFC §4.5 "User/operator
//! message", §5.4 rung 3). #981.
//!
//! Covers the two halves of ingest labeling:
//! - [`resolve_ingest_turn_label`] — the pure intersection of every declaration
//!   about the turn (session-policy default, the operator's per-message mark, a
//!   peer's inbound federation label), fail-closed on a malformed value or an
//!   unreadable policy;
//! - `GatewayStore::restrict_session_egress_taint` — folding that label into the
//!   session's accumulated taint *without ever widening it*, which is what
//!   distinguishes an incremental ingest contribution from the finalize-path
//!   replace.

use autonoetic_gateway::runtime::egress_labeler::resolve_ingest_turn_label;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressLabel, NamedEgressLabel, Sink};

fn meta(key: &str, label: &EgressLabel) -> serde_json::Value {
    serde_json::json!({ key: serde_json::to_value(label).unwrap() })
}

// ---------------------------------------------------------------------------
// resolve_ingest_turn_label — the declaration intersection
// ---------------------------------------------------------------------------

/// Nothing declared ⇒ nothing restricts the turn. Absence is the unrestricted
/// encoding, so an unconfigured deployment stays inert.
#[test]
fn no_declaration_leaves_the_turn_unlabeled() {
    assert_eq!(resolve_ingest_turn_label(None, false, None), None);
    let empty = serde_json::json!({});
    assert_eq!(resolve_ingest_turn_label(None, false, Some(&empty)), None);
}

/// An explicitly `unrestricted` declaration is still no restriction — it must
/// not mint a label row that says "everything is allowed".
#[test]
fn explicitly_unrestricted_declarations_produce_no_label() {
    assert_eq!(
        resolve_ingest_turn_label(Some(NamedEgressLabel::Unrestricted), false, None),
        None
    );
    let m = meta("operator_egress_label", &EgressLabel::unrestricted());
    assert_eq!(resolve_ingest_turn_label(None, false, Some(&m)), None);
}

/// The operator marks one message private — rung 3, the headline case.
#[test]
fn operator_per_message_mark_labels_the_turn() {
    let m = meta("operator_egress_label", &EgressLabel::local_only());
    let label = resolve_ingest_turn_label(None, false, Some(&m)).expect("restricted");
    assert_eq!(label, EgressLabel::local_only());
    assert!(!label.allows(Sink::RemoteModel));
}

/// The session policy's `default_label` applies to operator input with no
/// explicit mark — the room-wide declaration ("this room is private").
#[test]
fn session_policy_default_labels_an_unmarked_turn() {
    let label =
        resolve_ingest_turn_label(Some(NamedEgressLabel::LocalOnly), false, None).expect("restricted");
    assert_eq!(label, EgressLabel::local_only());
}

/// Mark and default intersect, so the more restrictive of the two wins in both
/// orders — the operator can tighten a room default per message, and a room
/// default tightens a laxer mark.
#[test]
fn mark_and_default_intersect_most_restrictive_wins() {
    // Default is the laxer one; the per-message mark tightens it.
    let m = meta("operator_egress_label", &EgressLabel::local_only());
    let label = resolve_ingest_turn_label(Some(NamedEgressLabel::NoRemoteModel), false, Some(&m))
        .expect("restricted");
    assert_eq!(label, EgressLabel::local_only());

    // Mark is the laxer one; the room default still holds it down.
    let m = meta("operator_egress_label", &EgressLabel::no_remote_model());
    let label = resolve_ingest_turn_label(Some(NamedEgressLabel::LocalOnly), false, Some(&m))
        .expect("restricted");
    assert_eq!(label, EgressLabel::local_only());
}

/// A mark cannot widen a restrictive room default. This is the property that
/// makes the metadata channel safe from any caller: intersection only shrinks,
/// so an `unrestricted` mark is inert rather than a bypass (I-14).
#[test]
fn an_unrestricted_mark_cannot_widen_a_restrictive_default() {
    let m = meta("operator_egress_label", &EgressLabel::unrestricted());
    let label = resolve_ingest_turn_label(Some(NamedEgressLabel::LocalOnly), false, Some(&m))
        .expect("default must survive an unrestricted mark");
    assert_eq!(label, EgressLabel::local_only());
}

/// Operator mark and federation label both apply — a peer's inbound turn that
/// the operator also marked is bound by both.
#[test]
fn operator_and_federation_labels_both_apply() {
    let m = serde_json::json!({
        "operator_egress_label": serde_json::to_value(EgressLabel::no_remote_model()).unwrap(),
        "ofp_inbound_egress_label": serde_json::to_value(EgressLabel::local_only()).unwrap(),
    });
    let label = resolve_ingest_turn_label(None, false, Some(&m)).expect("restricted");
    assert_eq!(label, EgressLabel::local_only());
}

/// A malformed label value fails closed. Dropping it would silently discard the
/// restriction the caller was trying to express (§2.2).
#[test]
fn malformed_label_value_fails_closed() {
    let m = serde_json::json!({ "operator_egress_label": "not-a-label" });
    let label = resolve_ingest_turn_label(None, false, Some(&m)).expect("fail-closed label");
    assert_eq!(label, EgressLabel::local_only());
    assert!(!label.allows(Sink::RemoteModel));
}

/// An unreadable session policy fails closed too — the policy that could not be
/// read might have been restrictive.
#[test]
fn unreadable_policy_fails_closed() {
    let label = resolve_ingest_turn_label(None, true, None).expect("fail-closed label");
    assert_eq!(label, EgressLabel::local_only());
}

// ---------------------------------------------------------------------------
// restrict_session_egress_taint — monotonic accumulation
// ---------------------------------------------------------------------------

/// First contribution to a clean session records the label as-is.
#[test]
fn restrict_taint_seeds_a_clean_session() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    let merged = store.restrict_session_egress_taint("sess-a", &EgressLabel::no_remote_model())?;
    assert_eq!(merged, EgressLabel::no_remote_model());
    assert_eq!(
        store.get_session_egress_taint("sess-a")?,
        Some(EgressLabel::no_remote_model())
    );
    Ok(())
}

/// The core monotonicity guarantee (§2.4): a laxer later contribution cannot
/// widen an accumulated taint. `set_session_egress_taint` would have clobbered
/// it — which is exactly why the ingest path must not use that.
#[test]
fn restrict_taint_never_widens_an_existing_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    store.restrict_session_egress_taint("sess-b", &EgressLabel::local_only())?;
    let merged = store.restrict_session_egress_taint("sess-b", &EgressLabel::unrestricted())?;
    assert_eq!(
        merged,
        EgressLabel::local_only(),
        "an unrestricted contribution must not widen local_only"
    );
    assert_eq!(
        store.get_session_egress_taint("sess-b")?,
        Some(EgressLabel::local_only())
    );

    // Contrast: the finalize-path replace *does* discard the accumulated taint,
    // so the two are not interchangeable. Pinning this keeps a future refactor
    // from "simplifying" the ingest path back onto `set_`. (The cleared state is
    // absence, not a stored `unrestricted` row — see the normalization test.)
    store.set_session_egress_taint("sess-b", &EgressLabel::unrestricted())?;
    assert_eq!(store.get_session_egress_taint("sess-b")?, None);
    Ok(())
}

/// "Absence ⇒ unrestricted" is enforced in the store, not by caller-side
/// guards: an `unrestricted` label clears the row instead of storing one. A
/// stored `unrestricted` row would read as a taint at a glance while permitting
/// everything, and would be wrong for any consumer treating `None` as the only
/// clean state. This is what keeps an unguarded caller from creating them.
#[test]
fn unrestricted_never_gets_stored_as_a_row() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    // Directly, onto a clean session.
    store.set_session_egress_taint("sess-f", &EgressLabel::unrestricted())?;
    assert_eq!(store.get_session_egress_taint("sess-f")?, None);

    // Over an existing restrictive taint — clears it rather than storing
    // "everything allowed" on top.
    store.restrict_session_egress_taint("sess-f", &EgressLabel::local_only())?;
    assert_eq!(
        store.get_session_egress_taint("sess-f")?,
        Some(EgressLabel::local_only())
    );
    store.set_session_egress_taint("sess-f", &EgressLabel::unrestricted())?;
    assert_eq!(store.get_session_egress_taint("sess-f")?, None);

    // And via the restrict path, which delegates to the same normalization.
    let merged = store.restrict_session_egress_taint("sess-f", &EgressLabel::unrestricted())?;
    assert!(merged.is_unrestricted());
    assert_eq!(store.get_session_egress_taint("sess-f")?, None);
    Ok(())
}

/// A stricter later contribution does tighten it, and the result is the
/// intersection rather than either input.
#[test]
fn restrict_taint_tightens_and_intersects() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    store.restrict_session_egress_taint("sess-c", &EgressLabel::no_remote_model())?;
    let merged = store.restrict_session_egress_taint("sess-c", &EgressLabel::local_only())?;
    assert_eq!(merged, EgressLabel::local_only());
    assert!(!merged.allows(Sink::RemoteModel));
    assert!(!merged.allows(Sink::Network));
    Ok(())
}

/// An unrestricted contribution to a clean session writes no row: absence ⇒
/// unrestricted, so storing one would be noise the taint reader has to ignore.
#[test]
fn restrict_taint_writes_no_row_for_an_unrestricted_result() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    let merged = store.restrict_session_egress_taint("sess-d", &EgressLabel::unrestricted())?;
    assert!(merged.is_unrestricted());
    assert_eq!(store.get_session_egress_taint("sess-d")?, None);
    Ok(())
}

/// Sessions do not bleed into each other.
#[test]
fn restrict_taint_is_per_session() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    store.restrict_session_egress_taint("sess-e", &EgressLabel::local_only())?;
    assert_eq!(store.get_session_egress_taint("sess-other")?, None);
    Ok(())
}
