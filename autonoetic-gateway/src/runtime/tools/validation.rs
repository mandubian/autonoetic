use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::{ValidationClass, ValidationWaiver};
use autonoetic_types::tool_error::ToolError;
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
    // Agent-waivable without further approval. CorrectnessCheck is NOT
    // agent-waivable outright — it requires explicit operator approval —
    // but it stays in the *waivable* set so the more precise
    // `correctness_waiver_requires_operator` denial (with its actionable
    // hint) fires instead of the generic non_waivable_validation (#1144).
    matches!(
        class,
        ValidationClass::QualityCheck | ValidationClass::PackagingCheck
    )
}

/// Returns true if the waiver class requires an operator approval before it can
/// be recorded by an agent.
fn requires_operator_waiver_approval(class: ValidationClass) -> bool {
    matches!(class, ValidationClass::CorrectnessCheck)
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let Some(config) = config else {
            return Ok(ToolError::execution("Gateway config not available", Some("Ensure the gateway configuration is loaded and valid.")).with_code("gateway_config_unavailable").to_error_response());
        };

        let session_id_val = session_id.ok_or_else(|| anyhow::anyhow!("session_id required"))?;
        let root_session_id = session_id_val.split('/').next().unwrap_or(session_id_val);

        if !args.artifact_id.starts_with("art_") {
            return Ok(ToolError::validation(format!("Invalid artifact_id '{}': must be a canonical artifact ID (art_*). Use artifact_inspect or resolve to look up art_* from a ref.", args.artifact_id), Some("Use a canonical art_* ID. Run artifact_inspect or resolve on your ref first.")).with_code("invalid_artifact_id").to_error_response());
        }

        let validation_class = match parse_class(&args.validation_class) {
            Some(c) => c,
            None => {
                return Ok(ToolError::validation(format!("Invalid validation_class '{}'. Waivable classes: quality_check, packaging_check; correctness_check requires operator approval.", args.validation_class), Some("Use one of: quality_check, packaging_check; correctness_check requires operator approval.")).with_code("invalid_validation_class").to_error_response());
            }
        };

        // correctness_check waivers (e.g. unit tests) require explicit operator
        // approval because waiving a correctness gate is a high-trust decision.
        // Agents cannot self-approve them. Checked BEFORE is_waivable so the
        // precise operator-required denial fires, not the generic non-waivable
        // rejection (#1144).
        if requires_operator_waiver_approval(validation_class) {
            return Ok(ToolError::validation(
                format!("Waiving '{}' validation for '{}' requires operator approval. An operator must run `autonoetic gateway validation waive --artifact-id {} --validation-id {} --validation-class {} --reason '<rationale>'`.", args.validation_class, args.validation_id, args.artifact_id, args.validation_id, args.validation_class),
                Some("Correctness checks (unit tests, lint, typecheck) cannot be waived by agents. Request the operator to waive this validation explicitly."),
            ).with_code("correctness_waiver_requires_operator").to_error_response());
        }

        if !is_waivable(validation_class) {
            return Ok(ToolError::validation(format!("{} validations cannot be waived — they are mechanically enforced", args.validation_class), Some("Only waivable classes can be waived. Check the list of waivable classes.")).with_code("non_waivable_validation").to_error_response());
        }

        if args.reason.trim().is_empty() {
            return Ok(ToolError::validation("A non-empty reason is required for all waivers", Some("Provide a non-empty reason string in the request.")).with_code("empty_waiver_reason").to_error_response());
        }

        let workflow_id = match crate::scheduler::workflow_store::ensure_workflow_for_root_session(
            config,
            Some(&store),
            root_session_id,
            Some(&manifest.agent.id),
        ) {
            Ok(w) => w.workflow_id,
            Err(e) => {
                return Ok(ToolError::execution(format!("Failed to ensure workflow for root session: {}", e), Some("Check the workflow subsystem and retry.")).with_code("workflow_ensure_failed").to_error_response());
            }
        };

        let waiver = ValidationWaiver {
            waiver_id: new_waiver_id(),
            workflow_id,
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
            return Ok(ToolError::execution("Gateway store not available", Some("Ensure the gateway database is initialized and accessible.")).with_code("gateway_store_unavailable").to_error_response());
        };

        let waivers = if let Some(artifact_id) = &args.artifact_id {
            store.list_waivers_for_artifact(artifact_id)?
        } else if let Some(workflow_id) = &args.workflow_id {
            store.list_waivers_for_workflow(workflow_id)?
        } else {
            return Ok(ToolError::validation("Provide artifact_id or workflow_id", Some("Include either artifact_id or workflow_id in the request.")).with_code("missing_artifact_or_workflow").to_error_response());
        };

        let summary: Vec<serde_json::Value> = waivers.iter().map(|w| {
            serde_json::json!({
                "waiver_id": w.waiver_id,
                "workflow_id": w.workflow_id,
                "artifact_id": w.artifact_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_class_recognizes_all_validation_classes() {
        assert_eq!(parse_class("correctness_check"), Some(ValidationClass::CorrectnessCheck));
        assert_eq!(parse_class("quality_check"), Some(ValidationClass::QualityCheck));
        assert_eq!(parse_class("packaging_check"), Some(ValidationClass::PackagingCheck));
        assert_eq!(parse_class("mechanical_safety"), Some(ValidationClass::MechanicalSafety));
        assert_eq!(parse_class("security_review"), Some(ValidationClass::SecurityReview));
        assert_eq!(parse_class("bogus"), None);
    }

    #[test]
    fn quality_and_packaging_are_agent_waivable() {
        assert!(is_waivable(ValidationClass::QualityCheck));
        assert!(is_waivable(ValidationClass::PackagingCheck));
    }

    #[test]
    fn correctness_safety_and_security_are_not_agent_waivable() {
        assert!(!is_waivable(ValidationClass::CorrectnessCheck));
        assert!(!is_waivable(ValidationClass::MechanicalSafety));
        assert!(!is_waivable(ValidationClass::SecurityReview));
    }

    #[test]
    fn correctness_check_requires_operator_waiver_approval() {
        assert!(requires_operator_waiver_approval(ValidationClass::CorrectnessCheck));
        assert!(!requires_operator_waiver_approval(ValidationClass::QualityCheck));
        assert!(!requires_operator_waiver_approval(ValidationClass::PackagingCheck));
        assert!(!requires_operator_waiver_approval(ValidationClass::MechanicalSafety));
        assert!(!requires_operator_waiver_approval(ValidationClass::SecurityReview));
    }
}
