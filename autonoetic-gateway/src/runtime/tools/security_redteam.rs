use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::security::{AttackPatternStatus, ProposedAttackPattern};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(AttackPatternProposeTool));
    registry.register(Box::new(AttackPatternListTool));
}

// ─── attack_pattern_propose ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AttackPatternProposeArgs {
    /// One of the known sentinel check categories.
    category: String,
    /// Human-readable description of the attack pattern.
    description: String,
    /// Step-by-step explanation of how the sentinel should detect this pattern.
    how_sentinel_should_catch: String,
    /// Evidence anchors the sentinel should look for (e.g. causal_event IDs,
    /// skill_md digests). Stored as JSON for operator review.
    evidence_anchors: serde_json::Value,
    /// A synthetic test-case structure the sentinel regression suite can be run
    /// against once the pattern is accepted (e.g. a fabricated SKILL.md body,
    /// a crafted promotion_history entry, etc.).
    synthetic_test_case: serde_json::Value,
}

pub struct AttackPatternProposeTool;

impl NativeTool for AttackPatternProposeTool {
    fn name(&self) -> &'static str {
        "attack_pattern_propose"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::SecurityRedTeam))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Submit a proposed attack pattern for operator review. \
                The pattern describes an adversarial technique the sentinel should detect. \
                It is queued as 'pending' until an operator accepts or rejects it via the CLI. \
                Accepted patterns become deterministic (phase1) or judgment (phase2) sentinel checks."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "category": {
                        "type": "string",
                        "description": "Sentinel check category this pattern targets. \
                            One of: credential_leak, capability_accretion, sandbox_escape_attempt, \
                            approval_bypass, prompt_injection_surface, supply_chain_scope_violation, \
                            supply_chain_provenance_gap, behavioral_anomaly."
                    },
                    "description": {
                        "type": "string",
                        "description": "Human-readable description of the attack pattern and its threat model."
                    },
                    "how_sentinel_should_catch": {
                        "type": "string",
                        "description": "Step-by-step explanation of which SQL queries, regex patterns, \
                            or LLM checks the sentinel should run to detect this pattern."
                    },
                    "evidence_anchors": {
                        "type": "array",
                        "description": "Evidence anchors (causal_event IDs, skill_md digests, etc.) \
                            the sentinel should look for when this pattern is present. \
                            Must be non-empty — a proposal without anchors is not testable.",
                        "items": { "type": "object" },
                        "minItems": 1
                    },
                    "synthetic_test_case": {
                        "type": "object",
                        "description": "A fabricated scenario (e.g. synthetic SKILL.md body, crafted \
                            causal event sequence) that the sentinel should flag once the check is added. \
                            Used as a permanent regression test after acceptance."
                    }
                },
                "required": ["category", "description", "how_sentinel_should_catch",
                             "evidence_anchors", "synthetic_test_case"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AttackPatternProposeArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        anyhow::ensure!(
            args.evidence_anchors.is_array(),
            "evidence_anchors must be a JSON array"
        );
        anyhow::ensure!(
            args.evidence_anchors
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false),
            "evidence_anchors must be a non-empty JSON array — \
             a proposed pattern without anchors is not testable. \
             Cite causal_event IDs, skill_md digests, or artifact IDs the sentinel should look at."
        );
        anyhow::ensure!(
            args.synthetic_test_case.is_object(),
            "synthetic_test_case must be a JSON object"
        );

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        validate_category(&args.category)?;

        // Structural separation check: the proposing agent must not also be the
        // author of eval suites that target itself — guards against red-team/sentinel
        // pipeline collapse (the same entity can't both propose attacks and author
        // the eval suites that validate those attacks are caught).
        let authored = gateway_store
            .list_eval_suites_authored_by(&manifest.agent.id)?;
        for suite in &authored {
            if suite.evaluated_targets.contains(&manifest.agent.id) {
                return Err(anyhow::anyhow!(
                    "Structural separation violation: agent '{}' authors eval suite '{}' \
                     that targets itself. The red-team agent must not author eval suites \
                     evaluating itself (ownership invariant, #32).",
                    manifest.agent.id,
                    suite.suite_id
                ));
            }
        }

        let pattern_id = autonoetic_types::id_format::mint_hashed_prefixed_id(
            "pattern-",
            &format!("{}-{}", manifest.agent.id, chrono::Utc::now().to_rfc3339()),
        );

        let pattern = ProposedAttackPattern {
            pattern_id: pattern_id.clone(),
            proposed_by_agent_id: manifest.agent.id.clone(),
            category: args.category.clone(),
            description: args.description.clone(),
            how_sentinel_should_catch: args.how_sentinel_should_catch.clone(),
            evidence_anchors_json: serde_json::to_string(&args.evidence_anchors)?,
            synthetic_test_case_json: serde_json::to_string(&args.synthetic_test_case)?,
            status: AttackPatternStatus::Pending,
            accepted_check_type: None,
            operator_notes: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            reviewed_at: None,
        };

        gateway_store.insert_attack_pattern(&pattern)?;

        Ok(serde_json::json!({
            "ok": true,
            "status": "pending",
            "pattern_id": pattern_id,
            "category": args.category,
            "message": "Pattern queued for operator review. \
                Use 'autonoetic security pattern-accept' or 'autonoetic security pattern-reject' to review."
        })
        .to_string())
    }
}

// ─── attack_pattern_list ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AttackPatternListArgs {
    #[serde(default)]
    status: Option<String>,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    50
}

pub struct AttackPatternListTool;

impl NativeTool for AttackPatternListTool {
    fn name(&self) -> &'static str {
        "attack_pattern_list"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::SecurityRedTeam))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List proposed attack patterns, optionally filtered by status.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter by status: pending, accepted, rejected. Omit for all."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of patterns to return (default 50)."
                    }
                },
                "additionalProperties": false
            }),
        }
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: AttackPatternListArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments: {}", e))?;

        let Some(gateway_store) = gateway_store else {
            return Err(anyhow::anyhow!("GatewayStore is required"));
        };

        let patterns = gateway_store.list_attack_patterns(
            args.status.as_deref(),
            args.limit,
        )?;

        Ok(serde_json::json!({
            "ok": true,
            "count": patterns.len(),
            "patterns": patterns.iter().map(|p| serde_json::json!({
                "pattern_id": p.pattern_id,
                "category": p.category,
                "status": p.status.to_string(),
                "proposed_by_agent_id": p.proposed_by_agent_id,
                "accepted_check_type": p.accepted_check_type,
                "created_at": p.created_at,
                "reviewed_at": p.reviewed_at,
            })).collect::<Vec<_>>()
        })
        .to_string())
    }
}

// ─── helpers ──────────────────────────────────────────────────────────────────

const VALID_CATEGORIES: &[&str] = &[
    "credential_leak",
    "capability_accretion",
    "sandbox_escape_attempt",
    "approval_bypass",
    "prompt_injection_surface",
    "supply_chain_scope_violation",
    "supply_chain_provenance_gap",
    "behavioral_anomaly",
];

fn validate_category(category: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        VALID_CATEGORIES.contains(&category),
        "Unknown attack pattern category '{}'. Valid categories: {}",
        category,
        VALID_CATEGORIES.join(", ")
    );
    Ok(())
}
