use std::path::Path;
use std::sync::Arc;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::tools::{NativeTool, NativeToolRunContext};

pub struct AgentInspectTool;

impl NativeTool for AgentInspectTool {
    fn name(&self) -> &'static str {
        "agent_inspect"
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
            description: "Inspect an installed agent's metadata, capabilities, and optionally its source code. Resolves the agent's current active revision. Source code is only returned for locally-trusted agents.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent ID to inspect (e.g., 'daily-trading-signal', 'coder.default')"
                    },
                    "include_source": {
                        "type": "boolean",
                        "description": "Include full source file contents. Only returned for locally-trusted agents. Default: false."
                    },
                    "include_layers": {
                        "type": "boolean",
                        "description": "Include dependency layer metadata from the artifact bundle. Default: false."
                    }
                },
                "required": ["agent_id"],
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
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("agent_id is required"))?;

        crate::runtime::tools::validate_agent_id(agent_id)?;

        let include_source = args
            .get("include_source")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_layers = args
            .get("include_layers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let store = gateway_store
            .ok_or_else(|| anyhow::anyhow!("GatewayStore is required"))?;
        let gateway_dir = gateway_dir
            .ok_or_else(|| anyhow::anyhow!("gateway_dir is required"))?;

        let alias = store
            .resolve_alias(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' is not installed (no alias found)", agent_id))?;

        let rev = store
            .get_agent_revision(&alias.revision_id)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Revision '{}' for agent '{}' not found",
                    alias.revision_id,
                    agent_id
                )
            })?;

        let is_local = rev.trust_domain == "local";

        let revision_dir = gateway_dir
            .join("revisions")
            .join("agents")
            .join(agent_id)
            .join(&rev.revision_id);

        if !revision_dir.exists() {
            return Err(anyhow::anyhow!(
                "Revision directory for agent '{}' does not exist on disk",
                agent_id
            ));
        }

        let file_map = collect_revision_files(&revision_dir)?;

        let skill_content = file_map
            .get("SKILL.md")
            .map(|bytes| String::from_utf8_lossy(bytes).to_string());

        let parsed_manifest = skill_content
            .as_deref()
            .and_then(|s| crate::runtime::parser::SkillParser::parse(s).ok());

        let (skill_meta, file_list) = {
            let mut files: Vec<String> = file_map.keys().cloned().collect();
            files.sort();

            let meta = if let Some((ref m, _)) = parsed_manifest {
                serde_json::json!({
                    "agent": {
                        "id": m.agent.id,
                        "name": m.agent.name,
                        "description": m.agent.description,
                    },
                    "capabilities": m.capabilities.iter().map(|c| serde_json::to_value(c).unwrap_or_default()).collect::<Vec<_>>(),
                    "execution_mode": serde_json::to_value(&m.execution_mode).unwrap_or_default(),
                    "script_entry": m.script_entry,
                })
            } else {
                serde_json::json!(null)
            };

            (meta, files)
        };

        let mut out = serde_json::json!({
            "ok": true,
            "agent_id": agent_id,
            "alias": {
                "revision_id": alias.revision_id,
                "short_ref": format!("{}@rev_{}", agent_id, rev.short_id),
                "updated_at": alias.updated_at,
            },
            "revision": {
                "revision_id": rev.revision_id,
                "status": format!("{:?}", rev.status),
                "created_at": rev.created_at,
                "created_by_type": rev.created_by_type,
                "created_by_id": rev.created_by_id,
                "trust_domain": rev.trust_domain,
                "source_kind": rev.source_kind,
                "base_revision_id": rev.base_revision_id,
                "artifact_id": rev.artifact_id,
            },
            "skill": skill_meta,
            "files": file_list,
        });

        if include_source && is_local {
            let source: std::collections::BTreeMap<String, String> = file_map
                .iter()
                .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).to_string()))
                .collect();
            out.as_object_mut().map(|o| {
                o.insert("source".to_string(), serde_json::to_value(&source).unwrap());
            });
        } else if include_source && !is_local {
            out.as_object_mut().map(|o| {
                o.insert(
                    "source".to_string(),
                    serde_json::json!({
                        "omitted": true,
                        "reason": format!("Agent trust domain is '{}' — source code is restricted to local agents only", rev.trust_domain),
                    }),
                );
            });
        }

        if include_layers {
            if let Some(ref art_id) = rev.artifact_id {
                let artifact_store =
                    crate::artifact_store::ArtifactStore::new(gateway_dir)?;
                match artifact_store.inspect(art_id) {
                    Ok(bundle) => {
                        let layers: Vec<serde_json::Value> = bundle
                            .layers
                            .iter()
                            .map(|l| {
                                serde_json::json!({
                                    "layer_id": l.layer_id,
                                    "name": l.name,
                                    "mount_path": l.mount_path,
                                    "digest": l.digest,
                                })
                            })
                            .collect();
                        out.as_object_mut().map(|o| {
                            o.insert("layers".to_string(), serde_json::json!(layers));
                        });
                    }
                    Err(e) => {
                        out.as_object_mut().map(|o| {
                            o.insert(
                                "layers".to_string(),
                                serde_json::json!({
                                    "error": format!("Could not load artifact layers: {}", e),
                                }),
                            );
                        });
                    }
                }
            } else {
                out.as_object_mut().map(|o| {
                    o.insert("layers".to_string(), serde_json::json!([]));
                });
            }
        }

        serde_json::to_string(&out).map_err(Into::into)
    }
}

fn collect_revision_files(root: &Path) -> anyhow::Result<std::collections::BTreeMap<String, Vec<u8>>> {
    fn walk(
        base: &Path,
        current: &Path,
        out: &mut std::collections::BTreeMap<String, Vec<u8>>,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out)?;
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let rel = path
                .strip_prefix(base)
                .map_err(|e| anyhow::anyhow!("Failed to compute relative path: {}", e))?;
            let rel = rel.to_string_lossy().replace('\\', "/");
            let bytes = std::fs::read(&path)?;
            out.insert(rel, bytes);
        }
        Ok(())
    }

    let mut files = std::collections::BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(AgentInspectTool));
}
