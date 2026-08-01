//! `egress.sources` operator RPC (#977) — the live tool catalog + MCP server
//! list + path families the room Tab-completes `/taint` sources from, plus
//! the restrictive label spellings. Read-only and pure; the room caches it.

use crate::rpc_env::{env, rpc_as};
use autonoetic_gateway::router::JsonRpcResponse;

async fn sources(params: serde_json::Value) -> serde_json::Value {
    let resp = rpc_as("sources-test", "egress.sources", params).await;
    assert!(
        resp.error.is_none(),
        "egress.sources returned error: {:?}",
        resp.error
    );
    resp.result.expect("egress.sources result")
}

#[tokio::test]
async fn egress_sources_lists_tools_families_and_labels() {
    let _env = env();
    let resp = sources(serde_json::json!({})).await;

    let tools = resp["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t == "sandbox_exec"),
        "registered tools expected, got: {tools:?}"
    );
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.as_str())
        .collect();
    assert!(
        names.windows(2).all(|w| w[0] <= w[1]),
        "tools must be sorted for cycling completion"
    );

    let path_families = resp["path_families"]
        .as_array()
        .expect("path_families array");
    for f in ["fs.read", "content.read", "sandbox.exec", "artifact.exec"] {
        assert!(
            path_families.iter().any(|p| p == f),
            "path family {f} missing: {path_families:?}"
        );
    }

    let mcp_servers = resp["mcp_servers"].as_array().expect("mcp_servers array");
    assert!(mcp_servers.is_empty(), "no AUTONOETIC_MCP_REGISTRY_PATH in test env");

    // Only restrictive spellings are offered — `/taint unrestricted` is a
    // widening usage error and must never complete.
    assert_eq!(
        resp["labels"],
        serde_json::json!(["local_only", "no_remote_model"])
    );
}

#[tokio::test]
async fn egress_sources_rejects_params() {
    let _env = env();
    let resp: JsonRpcResponse =
        rpc_as("sources-test", "egress.sources", serde_json::json!({ "x": 1 })).await;
    assert_eq!(
        resp.error.as_ref().map(|e| e.code),
        Some(-32602),
        "unexpected params must be rejected: {:?}",
        resp.error
    );
}
