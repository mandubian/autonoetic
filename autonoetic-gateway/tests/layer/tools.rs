//! Integration tests: layer identity recovery surfaces.
//!
//! Covers the read doors added after the live incident where a truncated
//! `sandbox_exec` capture left the agent holding fragments (`layer_56:`,
//! `sha256:f6`) with no way to recover the full identity:
//! - `resolve(ref="layer_*")` returns the layer manifest; digest prefixes work
//! - `resolve(ref="sha256:…")` falls back to the layer store on content miss
//! - `artifact_build(layers=…)` accepts a wrong-but-prefix id/digest pair and
//!   pins the CANONICAL identity into the bundle
//! - `layer_list` lists stored layers with id + digest

use autonoetic_gateway::artifact_store::ArtifactStore;
use autonoetic_gateway::layer_store::{LayerLimits, LayerStore};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

fn read_write_manifest() -> AgentManifest {
    autonoetic_types::agent::AgentManifest {
        capabilities: vec![
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
            Capability::WriteAccess {
                scopes: vec!["*".to_string()],
            },
        ],
        ..crate::support::manifest_builder::TestManifest::new().build()
    }
}

struct Fixture {
    _td: tempfile::TempDir,
    gw_dir: std::path::PathBuf,
    layer: autonoetic_types::layer::CapturedLayer,
    layer_store: LayerStore,
}

fn setup() -> Fixture {
    let td = tempdir().unwrap();
    let gw_dir = td.path().join(".gateway");
    fs::create_dir_all(&gw_dir).unwrap();

    let layer_store = LayerStore::new(&gw_dir, LayerLimits::default()).unwrap();
    let src = td.path().join("deps");
    fs::create_dir_all(src.join("requests-2.31.0.dist-info")).unwrap();
    fs::write(src.join("requests-2.31.0.dist-info/METADATA"), b"name: requests").unwrap();
    let layer = layer_store
        .create_from_dir(&src, "python-deps", "/opt/venv", None)
        .unwrap();

    Fixture {
        _td: td,
        gw_dir,
        layer,
        layer_store,
    }
}

fn run_tool(
    fx: &Fixture,
    tool: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let manifest = read_write_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_store = Arc::new(GatewayStore::open(&fx.gw_dir).unwrap());
    let config = GatewayConfig::default();
    let agent_dir = fx._td.path().join("agent");
    fs::create_dir_all(&agent_dir).unwrap();
    let result = registry.execute(
        tool,
        &manifest,
        &policy,
        &agent_dir,
        Some(&fx.gw_dir),
        &arguments.to_string(),
        Some("session-layer-tools"),
        None,
        Some(&config),
        Some(gateway_store),
        None,
    );
    assert!(result.is_ok(), "{tool} errored: {:?}", result.err());
    let raw = result.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("{tool} returned non-JSON: {e}\n{raw}"));
    parsed
}

#[test]
fn resolve_layer_ref_returns_manifest() {
    let fx = setup();
    let resp = run_tool(
        &fx,
        "resolve",
        serde_json::json!({ "ref": fx.layer.layer_id }),
    );

    assert_eq!(resp["ok"], true);
    assert_eq!(resp["kind"], "layer");
    assert_eq!(resp["layer_id"], fx.layer.layer_id.as_str());
    assert_eq!(resp["digest"], fx.layer.digest.as_str());
    assert_eq!(resp["exists"], true);
}

#[test]
fn resolve_layer_digest_prefix_recovers_identity() {
    let fx = setup();
    let hex = fx.layer.digest.strip_prefix("sha256:").unwrap();

    // Prefix ≥ 6 hex chars resolves to the one matching layer.
    let resp = run_tool(
        &fx,
        "resolve",
        serde_json::json!({ "ref": format!("sha256:{}", &hex[..10]) }),
    );
    assert_eq!(resp["ok"], true, "prefix resolve failed: {resp}");
    assert_eq!(resp["kind"], "layer");
    assert_eq!(resp["layer_id"], fx.layer.layer_id.as_str());

    // The id+digest concatenation an LLM produces when merging the short id
    // with the digest head (the live hallucination shape,
    // `layer_56:987f5225fbc9…`): the id's 5-char suffix is replaced by a
    // longer digest prefix.
    let base = fx.layer.layer_id.split(':').next().unwrap();
    let resp = run_tool(
        &fx,
        "resolve",
        serde_json::json!({ "ref": format!("{}:{}", base, &hex[..32]) }),
    );
    assert_eq!(resp["ok"], true, "merged-id resolve failed: {resp}");
    assert_eq!(resp["layer_id"], fx.layer.layer_id.as_str());

    // A 2-char fragment (the live incident shape) fails loudly, with a
    // recovery hint — never a silent pick.
    let resp = run_tool(&fx, "resolve", serde_json::json!({ "ref": "sha256:f6" }));
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "layer_not_found");
    assert!(resp["message"].as_str().unwrap().contains("too short"));
}

#[test]
fn resolve_sha256_content_miss_falls_back_to_layer_store() {
    let fx = setup();
    // The full layer digest is not registered content, so the content store
    // misses; the layer fallback must answer before not-found.
    let resp = run_tool(&fx, "resolve", serde_json::json!({ "ref": fx.layer.digest }));
    assert_eq!(resp["ok"], true, "digest fallback failed: {resp}");
    assert_eq!(resp["kind"], "layer");
    assert_eq!(resp["layer_id"], fx.layer.layer_id.as_str());
}

#[test]
fn artifact_build_canonicalizes_prefix_layer_identity() {
    let fx = setup();

    // Session content so the build has a payload.
    let content_store = ContentStore::new(&fx.gw_dir).unwrap();
    let handle = content_store.write(b"print('hello')").unwrap();
    content_store
        .register_name("session-layer-tools", "main.py", &handle)
        .unwrap();

    // The agent holds a WRONG layer_id (empty suffix — the beheaded
    // capture) but the full digest: the digest resolves the layer and the
    // bundle must pin the canonical id.
    let resp = run_tool(
        &fx,
        "artifact_build",
        serde_json::json!({
            "inputs": ["main.py"],
            "layers": [{
                "layer_id": "layer_56:",
                "name": "python-deps",
                "mount_path": "/opt/venv",
                "digest": fx.layer.digest,
            }]
        }),
    );
    assert_eq!(resp["ok"], true, "build with beheaded id failed: {resp}");

    let artifact_store = ArtifactStore::new(&fx.gw_dir).unwrap();
    let canonical_id = resp["artifact_id"].as_str().unwrap();
    let bundle = artifact_store.inspect(canonical_id).unwrap();
    assert_eq!(bundle.layers.len(), 1);
    assert_eq!(bundle.layers[0].layer_id, fx.layer.layer_id);
    assert_eq!(bundle.layers[0].digest, fx.layer.digest);
}

#[test]
fn artifact_build_rejects_divergent_digest() {
    let fx = setup();

    let content_store = ContentStore::new(&fx.gw_dir).unwrap();
    let handle = content_store.write(b"print('hello')").unwrap();
    content_store
        .register_name("session-layer-tools", "main.py", &handle)
        .unwrap();

    // A full 64-hex digest that does NOT match the stored layer is a wrong
    // pairing, not a prefix — must be refused.
    let wrong = format!("sha256:{}", "0".repeat(64));
    let resp = run_tool(
        &fx,
        "artifact_build",
        serde_json::json!({
            "inputs": ["main.py"],
            "layers": [{
                "layer_id": fx.layer.layer_id,
                "name": "python-deps",
                "mount_path": "/opt/venv",
                "digest": wrong,
            }]
        }),
    );
    assert_eq!(resp["ok"], false, "divergent digest must fail: {resp}");
    assert!(resp["message"].as_str().unwrap().contains("digest mismatch"));
}

#[test]
fn layer_list_lists_and_filters() {
    let fx = setup();

    let resp = run_tool(&fx, "layer_list", serde_json::json!({}));
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["total_stored"], 1);
    let layers = resp["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0]["layer_id"], fx.layer.layer_id.as_str());
    assert_eq!(layers[0]["digest"], fx.layer.digest.as_str());
    assert_eq!(layers[0]["name"], "python-deps");

    // Name filter hit (case-insensitive).
    let resp = run_tool(
        &fx,
        "layer_list",
        serde_json::json!({ "name_contains": "PYTHON" }),
    );
    assert_eq!(resp["count"], 1, "name filter is case-insensitive: {resp}");

    // Name filter miss.
    let resp = run_tool(
        &fx,
        "layer_list",
        serde_json::json!({ "name_contains": "node" }),
    );
    assert_eq!(resp["count"], 0);
    assert_eq!(resp["truncated"], false);
}
