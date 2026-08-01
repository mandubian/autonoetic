//! `grants.list` operator RPC (#976) — the child-session taint surface.
//!
//! `grants.list` used to return `current_taint` for the **root** session only,
//! so a tainted child — the very session RFC §5.5 expects to compartment the
//! taint — was invisible to the operator. This covers the new `child_taints`
//! field: it lists every restrictive child-session taint under the root tree,
//! the root's own taint stays in `current_taint`, and unrelated roots / clean
//! children do not leak in. Metadata-only (session id + label + timestamp),
//! matching the rest of the response.

use crate::rpc_env::{env, rpc_as};
use autonoetic_gateway::router::JsonRpcResponse;
use autonoetic_types::egress::EgressLabel;

async fn rpc(method: &str, params: serde_json::Value) -> JsonRpcResponse {
    rpc_as("grants-list", method, params).await
}

async fn grants_list(params: serde_json::Value) -> serde_json::Value {
    let resp = rpc("grants.list", params).await;
    assert!(
        resp.error.is_none(),
        "grants.list returned error: {:?}",
        resp.error
    );
    resp.result.expect("grants.list result")
}

/// `child_taints` lists the root's tainted children, excludes the root itself
/// (it is `current_taint`), and never reaches into an unrelated root tree.
/// A child with no restrictive row stays absent (absence ⇒ unrestricted).
#[tokio::test]
async fn grants_list_surfaces_child_session_taints() {
    let root = "gl-root";
    let child_tainted = "gl-root/coder.abc";
    // A child session id that is never given a taint row — its absence from the
    // result is half the point (clean children stay invisible by design).
    let _child_clean = "gl-root/researcher.xyz";
    let other_root_child = "gl-other/coder.zzz";

    let env = env();
    env.store
        .set_session_egress_taint(root, &EgressLabel::local_only())
        .unwrap();
    // Tainted child — the firebreak the operator must see.
    env.store
        .set_session_egress_taint(child_tainted, &EgressLabel::no_remote_model())
        .unwrap();
    // Clean child: no row ⇒ unrestricted ⇒ nothing to surface.
    // (`_child_clean` above is exactly this — a session id with no taint row.)
    // Unrelated root's child — must NOT appear under `root`.
    env.store
        .set_session_egress_taint(other_root_child, &EgressLabel::local_only())
        .unwrap();

    let resp = grants_list(serde_json::json!({ "root_session_id": root })).await;

    // Root's own taint stays in `current_taint` (not duplicated into children).
    assert_eq!(
        resp["current_taint"],
        serde_json::to_value(EgressLabel::local_only()).unwrap()
    );

    // Only the tainted child of THIS root, in metadata-only shape.
    let children = resp["child_taints"].as_array().expect("child_taints array");
    let child_sessions: Vec<&str> = children
        .iter()
        .map(|r| r["session_id"].as_str().expect("session_id"))
        .collect();
    assert_eq!(
        child_sessions,
        vec![child_tainted],
        "only the tainted child appears — not the root, not the clean child, \
         not an unrelated root's child"
    );

    // Metadata-only: session id + label + timestamp, nothing else.
    let only_child = &children[0];
    assert_eq!(only_child["session_id"], child_tainted);
    assert_eq!(
        only_child["label"],
        serde_json::to_value(EgressLabel::no_remote_model()).unwrap()
    );
    assert!(
        only_child["updated_at"].as_str().is_some(),
        "updated_at is a timestamp string"
    );
    // No content column exists on a taint row — assert the known key set.
    let keys: Vec<&str> = only_child
        .as_object()
        .expect("child taint row is an object")
        .keys()
        .map(|k| k.as_str())
        .collect();
    let mut mut_keys = keys.clone();
    mut_keys.sort();
    assert_eq!(mut_keys, vec!["label", "session_id", "updated_at"]);
}

/// The root's own taint never lands in `child_taints` — it has its own field.
/// A root with a taint but no tainted children yields an empty list.
#[tokio::test]
async fn grants_list_child_taints_excludes_the_root_itself() {
    let root = "gl-solo";
    let env = env();
    env.store
        .set_session_egress_taint(root, &EgressLabel::local_only())
        .unwrap();

    let resp = grants_list(serde_json::json!({ "root_session_id": root })).await;
    assert_eq!(
        resp["current_taint"],
        serde_json::to_value(EgressLabel::local_only()).unwrap()
    );
    assert_eq!(
        resp["child_taints"].as_array().map(|a| a.len()),
        Some(0),
        "the root's own taint is current_taint, not a child"
    );
}

/// An untainted room has no `current_taint` and no child taints — absence is
/// the unrestricted state everywhere in this plane.
#[tokio::test]
async fn grants_list_clean_room_has_no_taint_fields() {
    let root = "gl-clean";
    let resp = grants_list(serde_json::json!({ "root_session_id": root })).await;
    assert!(resp["current_taint"].is_null());
    assert_eq!(resp["child_taints"].as_array().map(|a| a.len()), Some(0));
    // The grant arrays are always present regardless of taint state.
    assert!(resp["session_approval_grants"].is_array());
    assert!(resp["egress_declassification_grants"].is_array());
}

/// Grandchildren are in the same root tree (`root/child/grandchild`), so a
/// tainted grandchild surfaces too — the firebreak is per-session, not per
/// generation.
#[tokio::test]
async fn grants_list_surfaces_tainted_grandchildren() {
    let root = "gl-gen";
    let child = "gl-gen/mid";
    let grandchild = "gl-gen/mid/leaf";
    let env = env();
    env.store
        .set_session_egress_taint(child, &EgressLabel::local_only())
        .unwrap();
    env.store
        .set_session_egress_taint(grandchild, &EgressLabel::no_remote_model())
        .unwrap();

    let resp = grants_list(serde_json::json!({ "root_session_id": root })).await;
    let mut sessions: Vec<&str> = resp["child_taints"]
        .as_array()
        .expect("child_taints")
        .iter()
        .map(|r| r["session_id"].as_str().expect("session_id"))
        .collect();
    sessions.sort();
    let mut expected = vec![child, grandchild];
    expected.sort();
    assert_eq!(sessions, expected);
}
