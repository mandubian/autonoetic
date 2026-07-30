//! Artifact egress labels — RFC data-envelopes §4.5 artifact birth point. #980.
//!
//! Artifacts were the one stored-content surface with no label: `memories`,
//! `execution_traces`, and `agent_messages` all got one, `artifact_store` did
//! not. Because artifacts are how agents pass content (handles, not inline
//! blobs) *and* they are content-addressed and persistent, an unlabeled artifact
//! walked straight through the §5.5 compartment firebreak:
//!
//! ```text
//! session A (tainted local_only) → writes artifact X → X carries no label
//! session B (never tainted)      → reads X           → unrestricted → remote
//! ```
//!
//! P-15.2 gates on *session* taint, so A was fenced; B was never tainted, so
//! there was nothing to gate on.
//!
//! Two halves under test. **Write:** the builder session's taint is recorded
//! against the artifact id, intersecting on content-addressed reuse.
//! **Read:** any tool call naming that artifact has the stored label intersected
//! into its result, so the label survives the round trip — including into a
//! session that never touched the original source.

use std::sync::Arc;

use autonoetic_gateway::runtime::egress_labeler::{
    artifact_ids_in_arguments, artifact_taint_from_store,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressLabel, Sink};

// ---------------------------------------------------------------------------
// Write side — the label plane, and its monotonicity under dedup
// ---------------------------------------------------------------------------

/// A tainted builder's artifact carries the taint.
#[test]
fn build_records_the_builder_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    let merged = store.restrict_artifact_egress_label("art_deadbeef", &EgressLabel::local_only())?;
    assert_eq!(merged, EgressLabel::local_only());
    assert_eq!(
        store.get_artifact_egress_label("art_deadbeef")?,
        Some(EgressLabel::local_only())
    );
    Ok(())
}

/// The dedup case that makes a manifest field awkward and a sidecar right:
/// identical bytes built by a clean session return the *same* artifact id, and
/// the label must not relax. Content-addressed identity means these are the same
/// bytes, so if any producer was tainted, the bytes are tainted.
#[test]
fn a_cleaner_rebuild_cannot_relax_an_artifact_label() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    store.restrict_artifact_egress_label("art_cafe0001", &EgressLabel::local_only())?;
    let merged =
        store.restrict_artifact_egress_label("art_cafe0001", &EgressLabel::unrestricted())?;
    assert_eq!(
        merged,
        EgressLabel::local_only(),
        "a clean rebuild of identical content must not widen the label"
    );
    Ok(())
}

/// A stricter rebuild tightens it, and the result is the intersection.
#[test]
fn a_stricter_rebuild_tightens_the_label() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    store.restrict_artifact_egress_label("art_cafe0002", &EgressLabel::no_remote_model())?;
    let merged =
        store.restrict_artifact_egress_label("art_cafe0002", &EgressLabel::local_only())?;
    assert_eq!(merged, EgressLabel::local_only());
    assert!(!merged.allows(Sink::Network));
    Ok(())
}

/// Absence ⇒ unrestricted holds in this table too: a clean build stores no row,
/// so an unconfigured deployment pays nothing.
#[test]
fn a_clean_build_stores_no_row() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    let merged =
        store.restrict_artifact_egress_label("art_cafe0003", &EgressLabel::unrestricted())?;
    assert!(merged.is_unrestricted());
    assert_eq!(store.get_artifact_egress_label("art_cafe0003")?, None);
    Ok(())
}

/// Labels do not bleed between artifacts.
#[test]
fn artifact_labels_are_per_artifact() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    store.restrict_artifact_egress_label("art_aaaa1111", &EgressLabel::local_only())?;
    assert_eq!(store.get_artifact_egress_label("art_bbbb2222")?, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// Read side — id extraction
// ---------------------------------------------------------------------------

/// Ids are `art_` + exactly 8 hex chars, so scanning the arguments is
/// tool-schema-independent — one hook covers inspect, read_file, exec, prepare.
#[test]
fn artifact_ids_are_found_in_any_argument_shape() {
    assert_eq!(
        artifact_ids_in_arguments(r#"{"artifact_id":"art_deadbeef"}"#),
        vec!["art_deadbeef"]
    );
    assert_eq!(
        artifact_ids_in_arguments(r#"{"inputs":["art_00112233","art_44556677"]}"#),
        vec!["art_00112233", "art_44556677"]
    );
    // Deduped and sorted, so the audit record is stable.
    assert_eq!(
        artifact_ids_in_arguments(r#"{"a":"art_ffff0000","b":"art_ffff0000"}"#),
        vec!["art_ffff0000"]
    );
    // Embedded in prose (an exec command line) still counts.
    assert_eq!(
        artifact_ids_in_arguments(r#"{"command":"python3 /mnt/art_abcdef01/run.py"}"#),
        vec!["art_abcdef01"]
    );
}

/// Non-ids are rejected: a longer alphanumeric run is a different token, and
/// non-hex is not an id at all. A false positive would resolve to no stored
/// label and be harmless, but precision keeps the audit record honest.
#[test]
fn non_ids_are_not_matched() {
    assert!(artifact_ids_in_arguments(r#"{"x":"art_notahexid"}"#).is_empty());
    assert!(artifact_ids_in_arguments(r#"{"x":"art_deadbeefcafe"}"#).is_empty());
    assert!(artifact_ids_in_arguments(r#"{"x":"art_dead"}"#).is_empty());
    assert!(artifact_ids_in_arguments(r#"{"x":"artifact"}"#).is_empty());
    // Truncated at the very end of the buffer must not panic or match.
    assert!(artifact_ids_in_arguments("art_dead").is_empty());
    assert!(artifact_ids_in_arguments("art_").is_empty());
}

/// Non-ASCII bytes after the prefix must not panic. `arguments_json` is arbitrary
/// agent-supplied JSON, and the 8 bytes after `art_` can be the middle of a
/// multi-byte codepoint — `art_€€€` puts byte offset 12 inside the third `€`.
/// Slicing the `&str` there panics ("byte index 12 is not a char boundary"), which
/// in this path would crash egress labeling itself: a denial of service on the
/// enforcement plane, reachable from any tool call. Byte-level validation, not str
/// slicing.
#[test]
fn non_ascii_after_the_prefix_does_not_panic() {
    assert!(artifact_ids_in_arguments("art_€€€").is_empty());
    assert!(artifact_ids_in_arguments(r#"{"x":"art_€€€"}"#).is_empty());
    // Multi-byte immediately after the prefix, and part-way through the id span.
    assert!(artifact_ids_in_arguments("art_🎉").is_empty());
    assert!(artifact_ids_in_arguments("art_dead€eef").is_empty());
    // A valid id is still found when a multi-byte char sits nearby.
    assert_eq!(
        artifact_ids_in_arguments(r#"{"note":"€ art_deadbeef €"}"#),
        vec!["art_deadbeef"]
    );
}

// ---------------------------------------------------------------------------
// Read side — the round trip
// ---------------------------------------------------------------------------

/// The leak this closes, expressed as the round trip: a session that never
/// touched the private source reads the artifact and the label comes back with
/// it. `prior_labels` only reaches across the current turn, so nothing else in
/// the plane would have carried it.
#[test]
fn a_labeled_artifact_taints_a_later_read() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    store.restrict_artifact_egress_label("art_11223344", &EgressLabel::local_only())?;

    let (label, applied) = artifact_taint_from_store(
        r#"{"artifact_id":"art_11223344","file":"inbox.txt"}"#,
        store.as_ref(),
    );
    assert_eq!(label, EgressLabel::local_only());
    assert!(!label.allows(Sink::RemoteModel));
    assert_eq!(applied, vec!["art_11223344"]);
    Ok(())
}

/// An unlabeled artifact contributes nothing — clean artifacts stay remote-eligible.
#[test]
fn an_unlabeled_artifact_contributes_nothing() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    let (label, applied) =
        artifact_taint_from_store(r#"{"artifact_id":"art_99887766"}"#, store.as_ref());
    assert!(label.is_unrestricted());
    assert!(applied.is_empty());
    Ok(())
}

/// Several artifacts in one call intersect, so a bundle assembled from a clean
/// and a tainted artifact is bound by the tainted one.
#[test]
fn multiple_artifacts_intersect() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    store.restrict_artifact_egress_label("art_aaaa0001", &EgressLabel::no_remote_model())?;
    store.restrict_artifact_egress_label("art_bbbb0002", &EgressLabel::local_only())?;

    let (label, applied) = artifact_taint_from_store(
        r#"{"inputs":["art_aaaa0001","art_bbbb0002","art_cccc0003"]}"#,
        store.as_ref(),
    );
    assert_eq!(label, EgressLabel::local_only());
    // The unlabeled third artifact contributes nothing and is not recorded.
    assert_eq!(applied, vec!["art_aaaa0001", "art_bbbb0002"]);
    Ok(())
}

/// End to end across the compartment boundary: session A is tainted and builds
/// the artifact; session B never touches the source, is clean, and still cannot
/// send the artifact's content to a remote sink.
#[test]
fn the_compartment_laundering_path_is_closed() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);

    // Session A: tainted, builds the artifact.
    let a_taint = EgressLabel::local_only();
    store.restrict_session_egress_taint("root-a", &a_taint)?;
    store.restrict_artifact_egress_label("art_beef0001", &a_taint)?;

    // Session B: genuinely clean — no session taint of its own.
    assert_eq!(store.get_session_egress_taint("root-b")?, None);

    // B reads the artifact. Before #980 this produced an unrestricted result.
    let (label, applied) =
        artifact_taint_from_store(r#"{"artifact_id":"art_beef0001"}"#, store.as_ref());
    assert_eq!(applied, vec!["art_beef0001"]);
    assert!(
        !label.allows(Sink::RemoteModel),
        "the artifact's label must reach a session that never touched the source"
    );
    assert!(!label.allows(Sink::Network));
    Ok(())
}
