use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{ValidationClass, ValidationWaiver};
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ValidationWaiveTool));
    registry.register(Box::new(ValidationWaiversTool));
}

fn has_workbench_access(manifest: &AgentManifest) -> bool {
    manifest.capabilities.iter().any(|c| {
        matches!(c, Capability::PlanFrameAccess { .. })
    })
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_waiver_id() -> String {
    let bytes = uuid::Uuid::new_v4();
    format!("vw-{}", hex::encode(&bytes.as_bytes()[..6]))
}

fn parse_class(s: &str) -> Option<ValidationClass> {
    match s {
        "correctness_check" => Some(ValidationClass::CorrectnessCheck),
        "quality_check" => Some(ValidationClass::QualityCheck),
        "packaging_check" => Some(ValidationClass::PackagingCheck),
        "mechanical_safety" => Some(ValidationClass::MechanicalSafety),
        "security_review" => Some(ValidationClass::SecurityReview),
        _ => None,
    }
}

fn is_waivable(class: ValidationClass) -> bool {
    !matches!(
        class,
        ValidationClass::MechanicalSafety | ValidationClass::SecurityReview
    )
}

pub struct ValidationWaiveTool;

impl NativeTool for ValidationWaiveTool {
    fn name(&self) -> &'static str {
        "validation_waive"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Record a validation waiver for an artifact. The waiver is durable and visible in promotion records and traces. Mechanical safety gates and security reviews cannot be waived.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "The artifact ID (art_*) to waive validation for"
                    },
                    "validation_id": {
                        "type": "string",
                        "description": "Identifier for the validation being waived (e.g., 'unit_tests', 'style_review')"
                    },
                    "validation_class": {
                        "type": "string",
                        "enum": ["correctness_check", "quality_check", "packaging_check"],
                        "description": "Class of validation being waived. Mechanical safety and security reviews cannot be waived."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Human-readable reason for the waiver"
                    }
                },
                "required": ["artifact_id", "validation_id", "validation_class", "reason"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            artifact_id: String,
            validation_id: String,
            validation_class: String,
            reason: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let Some(config) = config else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway config not available"
            }))?);
        };

        let session_id_val = session_id.ok_or_else(|| anyhow::anyhow!("session_id required"))?;
        let root_session_id = session_id_val.split('/').next().unwrap_or(session_id_val);

        let validation_class = match parse_class(&args.validation_class) {
            Some(c) => c,
            None => {
                return Ok(serde_json::to_string(&serde_json::json!({
                    "ok": false,
                    "error": format!("Invalid validation_class '{}'. Waivable classes: correctness_check, quality_check, packaging_check", args.validation_class)
                }))?);
            }
        };

        if !is_waivable(validation_class) {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": format!("{} validations cannot be waived — they are mechanically enforced", args.validation_class)
            }))?);
        }

        if args.reason.trim().is_empty() {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "A non-empty reason is required for all waivers"
            }))?);
        }

        let workflow_id = crate::scheduler::workflow_store::resolve_workflow_id_for_root_session(
            config, root_session_id,
        ).ok().flatten().unwrap_or_default();

        let waiver = ValidationWaiver {
            waiver_id: new_waiver_id(),
            workflow_id: workflow_id.clone(),
            artifact_id: args.artifact_id.clone(),
            validation_id: args.validation_id.clone(),
            validation_class,
            waived_by: manifest.agent.id.clone(),
            reason: args.reason.clone(),
            created_at: now_rfc3339(),
        };

        store.save_validation_waiver(&waiver)?;

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "waiver_id": waiver.waiver_id,
            "artifact_id": waiver.artifact_id,
            "validation_id": waiver.validation_id,
            "validation_class": args.validation_class,
            "reason": waiver.reason,
            "waived_by": waiver.waived_by,
            "message": "Validation waived. This waiver is recorded and visible in promotion records."
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

pub struct ValidationWaiversTool;

impl NativeTool for ValidationWaiversTool {
    fn name(&self) -> &'static str {
        "validation_waivers"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List validation waivers for an artifact or workflow. Shows all recorded waivers with reasons.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_id": {
                        "type": "string",
                        "description": "List waivers for this artifact ID"
                    },
                    "workflow_id": {
                        "type": "string",
                        "description": "List waivers for this workflow ID"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        has_workbench_access(manifest)
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            artifact_id: Option<String>,
            workflow_id: Option<String>,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Gateway store not available"
            }))?);
        };

        let waivers = if let Some(artifact_id) = &args.artifact_id {
            store.list_waivers_for_artifact(artifact_id)?
        } else if let Some(workflow_id) = &args.workflow_id {
            store.list_waivers_for_workflow(workflow_id)?
        } else {
            return Ok(serde_json::to_string(&serde_json::json!({
                "ok": false, "error": "Provide artifact_id or workflow_id"
            }))?);
        };

        let summary: Vec<serde_json::Value> = waivers.iter().map(|w| {
            serde_json::json!({
                "waiver_id": w.waiver_id,
                "validation_id": w.validation_id,
                "validation_class": w.validation_class.as_str(),
                "waived_by": w.waived_by,
                "reason": w.reason,
                "created_at": w.created_at,
            })
        }).collect();

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "waivers": summary,
            "count": waivers.len(),
        }))?)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}
