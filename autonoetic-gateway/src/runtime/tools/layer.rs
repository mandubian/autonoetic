//! `layer_list` — the read surface for the layer store.
//!
//! Layers were write-only from an agent's perspective: `sandbox_exec`
//! capture returns `captured_layers` once, and if that response was lost to
//! truncation (or the session restarted) there was no way to recover a
//! layer's full identity — `artifact_build` failed with "does not exist in
//! layer store" and the agent resorted to guessing. This tool lists stored
//! layers (id, digest, name, size) so the id is always recoverable, and
//! complements the digest-prefix recovery in `resolve` /
//! `LayerStore::resolve_ref`.
//!
//! The listing is deliberately global: the layer store is content-addressed
//! (like the `sha256:` content handles), a layer captured by a sibling
//! session is addressable by any agent holding its id, and manifest
//! metadata (ids/digests/sizes/names) carries no secret material. Mount-time
//! scope gates — not read visibility — are the security boundary for layers.

use std::path::Path;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(LayerListTool));
}

pub struct LayerListTool;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;

impl NativeTool for LayerListTool {
    fn name(&self) -> &'static str {
        "layer_list"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List stored dependency layers — recovers the full layer_id/digest before artifact_build (capture returns them once; this is the lookup). Filters: name_contains substring; limit caps the result.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name_contains": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                },
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            name_contains: Option<String>,
            limit: Option<usize>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource(
                "layer_list requires gateway directory to be configured",
                None::<String>,
            )
            .to_error_response());
        };

        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let name_contains = args
            .name_contains
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase);

        let layer_store = crate::layer_store::LayerStore::new(gw_dir, Default::default())?;
        // Fetch an unbounded window, filter, then cap — so a filter does not
        // silently hide matching layers behind an arbitrary first-N cut.
        let all = layer_store.list_layers(usize::MAX)?;
        let total_stored = all.len();
        let mut matched: Vec<_> = all
            .into_iter()
            .filter(|m| {
                if let Some(needle) = &name_contains {
                    if !m.name.to_ascii_lowercase().contains(needle.as_str()) {
                        return false;
                    }
                }
                true
            })
            .collect();
        let total_matched = matched.len();
        matched.truncate(limit);

        let layers: Vec<serde_json::Value> = matched
            .iter()
            .map(|m| {
                json!({
                    "layer_id": m.layer_id,
                    "digest": m.digest,
                    "name": m.name,
                    "file_count": m.file_count,
                    "size_bytes": m.size_bytes,
                    "created_at": m.created_at,
                    "resolved_package_count": m.resolved_packages.len(),
                })
            })
            .collect();

        Ok(json!({
            "ok": true,
            "layers": layers,
            "count": layers.len(),
            "total_matched": total_matched,
            "total_stored": total_stored,
            "truncated": total_matched > layers.len(),
            "repair_hint": "Attach with artifact_build(layers=[{layer_id, name, mount_path, digest}]) — copy layer_id and digest exactly from this output.",
        })
        .to_string())
    }
}
