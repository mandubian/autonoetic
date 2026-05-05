//! Constitution-facing tools.
//!
//! - `constitution_read` (Ri-0.10, issue #95): every agent's right to read
//!   the full text of the constitution it operates under, addressed by
//!   digest. No capability gate.
//! - `constitution_propose_amendment` (Ri-0.8 / R+++1, issue #92): agents
//!   holding the `ConstitutionalProposal` capability submit amendment
//!   proposals through a declared, durable channel. Proposals receive a
//!   durable ID, enter a review queue, and cannot be silently dropped.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use crate::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal;
use crate::constitution_digest::{
    constitution_digest, constitution_format_version, constitution_text, constitution_version,
};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::notification::{NotificationRecord, NotificationType};
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ConstitutionReadTool));
    registry.register(Box::new(ConstitutionProposeAmendmentTool));
}

/// Proposal kinds accepted by `constitution_propose_amendment`. Mirrors the
/// roadmap §1.11 sketch: rules and rights, with `add` / `modify` / `remove`.
const PROPOSAL_KINDS: &[&str] = &[
    "add_rule",
    "modify_rule",
    "remove_rule",
    "add_right",
    "modify_right",
    "remove_right",
];

/// True when at least one declared `ConstitutionalProposal` capability admits
/// the requested proposal kind via wildcard (`*`) or direct match.
fn has_constitutional_proposal_capability(manifest: &AgentManifest, kind: &str) -> bool {
    manifest.capabilities.iter().any(|c| {
        matches!(c, Capability::ConstitutionalProposal { patterns }
            if patterns.iter().any(|p| p == "*" || p == kind))
    })
}

pub struct ConstitutionReadTool;

impl NativeTool for ConstitutionReadTool {
    fn name(&self) -> &'static str {
        "constitution_read"
    }

    /// Reading the law is a right (Ri-0.10). Available to every agent
    /// regardless of declared capabilities.
    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Read the gateway constitution that governs this agent. \
                Returns the full document or a single section/rule when `section` is given. \
                Use this before proposing amendments, when a rule ID appears in an error, \
                or any time you need to understand your obligations and rights. \
                Section selector accepts rule IDs (`Ri-0.10`, `R-7.5`, `R+5`, `R++1`, `R+++3`) \
                and numbered sections (`§0` … `§14`). The returned digest identifies the exact \
                configured constitutional release and lock file."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "section": {
                        "type": "string",
                        "description": "Optional selector. Rule IDs (Ri-0.10, R-7.5, R+5, R++1, R+++3) or numbered sections (§0..§14). Omit to receive the full document."
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
        config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let config = config.ok_or_else(|| {
            anyhow::anyhow!("constitution_read requires GatewayConfig to resolve constitution paths")
        })?;
        crate::constitution_digest::initialize_constitution(config)?;

        #[derive(Deserialize, Default)]
        struct Args {
            #[serde(default)]
            section: Option<String>,
        }
        let args: Args = if arguments_json.trim().is_empty() {
            Args::default()
        } else {
            serde_json::from_str(arguments_json).map_err(|e| {
                anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e)
            })?
        };

        let selector = args.section.as_deref().map(str::trim).unwrap_or("");
        let (text, matched_selector) = if selector.is_empty() {
            (constitution_text().to_string(), None)
        } else {
            match extract_section(constitution_text().as_ref(), selector) {
                Some(extract) => (extract, Some(selector.to_string())),
                None => {
                    return Ok(ToolError::validation(
                        format!("section selector '{}' did not match any rule ID or section in the constitution", selector),
                        Some("Use a rule ID like 'Ri-0.10', 'R-7.5', 'R+5', 'R++1', 'R+++3', or a section like '§0'..'§14'. Omit `section` to receive the full document."),
                    )
                    .to_error_response());
                }
            }
        };

        Ok(serde_json::to_string(&serde_json::json!({
            "ok": true,
            "text": text,
            "digest": constitution_digest().as_ref(),
            "version": constitution_version().as_ref(),
            "format_version": constitution_format_version(),
            "section": matched_selector,
            "retrieved_at": chrono::Utc::now().to_rfc3339(),
        }))?)
    }
}

/// Locate a section in the constitution by selector.
///
/// Returns the matching slice, including the table header rows for rule IDs
/// so the result is interpretable in isolation.
fn extract_section(doc: &str, selector: &str) -> Option<String> {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(num) = parse_section_number(trimmed) {
        return extract_numbered_section(doc, num);
    }

    extract_rule_row(doc, trimmed)
}

/// Parse `§N`, `section N`, or `Section N` into the integer N.
fn parse_section_number(input: &str) -> Option<u32> {
    let cleaned = input
        .trim_start_matches(['§', '#'])
        .trim_start_matches("section ")
        .trim_start_matches("Section ")
        .trim_start_matches("SECTION ")
        .trim();
    cleaned.parse::<u32>().ok()
}

/// Extract a top-level numbered section like `## 0. Rights` through the next
/// `## ` heading at the same level.
fn extract_numbered_section(doc: &str, n: u32) -> Option<String> {
    let prefix = format!("## {}. ", n);
    let mut lines = doc.lines().enumerate();
    let start = lines.find_map(|(i, line)| {
        if line.starts_with(&prefix) {
            Some(i)
        } else {
            None
        }
    })?;

    // Walk to the next `## ` heading. `## ` does not match `### ` (the third
    // char differs: space vs `#`), so subsections are correctly retained.
    let collected: Vec<&str> = doc.lines().collect();
    let mut end = collected.len();
    for (i, line) in collected.iter().enumerate().skip(start + 1) {
        if line.starts_with("## ") {
            end = i;
            break;
        }
    }
    Some(collected[start..end].join("\n"))
}

/// Find the table row whose ID column matches `rule_id` exactly, returning
/// the table's header rows (column titles + separator) followed by the row.
fn extract_rule_row(doc: &str, rule_id: &str) -> Option<String> {
    let needle = format!("| {} |", rule_id);
    let lines: Vec<&str> = doc.lines().collect();
    // Tolerate harmless leading whitespace in the markdown table.
    let row_idx = lines
        .iter()
        .position(|line| line.trim_start().starts_with(&needle))?;

    // Walk backwards to the table header. Markdown tables: header line, then
    // a separator line of `|---|---|...`, then data rows.
    let mut header_idx = row_idx;
    while header_idx > 0 {
        let prev = lines[header_idx - 1];
        if prev.trim_start().starts_with('|') {
            header_idx -= 1;
        } else {
            break;
        }
    }

    let mut out = Vec::new();
    if header_idx + 1 < lines.len() && header_idx < row_idx {
        // Header + separator (the two table lines above the first data row).
        out.push(lines[header_idx]);
        out.push(lines[header_idx + 1]);
    }
    out.push(lines[row_idx]);
    Some(out.join("\n"))
}

// ---------------------------------------------------------------------------
// constitution_propose_amendment — Ri-0.8 / R+++1 (issue #92)
// ---------------------------------------------------------------------------

pub struct ConstitutionProposeAmendmentTool;

impl NativeTool for ConstitutionProposeAmendmentTool {
    fn name(&self) -> &'static str {
        "constitution_propose_amendment"
    }

    /// Available iff the agent declares any `ConstitutionalProposal` capability.
    /// Per-kind pattern matching is enforced at execute time and reported as a
    /// `permission` error naming the rejected proposal kind.
    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::ConstitutionalProposal { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Submit a constitutional amendment proposal. Requires the \
                ConstitutionalProposal capability. Proposals are persisted with a \
                durable ID and enter the operator review queue — they cannot be \
                silently dropped (Ri-0.8). Use `constitution_read` first to confirm \
                the current text of the rule or right you are proposing to change."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": PROPOSAL_KINDS,
                        "description": "Nature of the proposal."
                    },
                    "target_id": {
                        "type": "string",
                        "description": "Existing rule or right ID (Ri-X.Y, R-X.Y, R+N…). Required for modify_* and remove_* kinds."
                    },
                    "proposed_text": {
                        "type": "string",
                        "description": "Replacement text for modify_*; new rule/right text for add_*. Omitted for remove_*."
                    },
                    "justification": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Why this amendment is needed. Required."
                    },
                    "evidence": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Causal-event IDs or execution-trace IDs that motivate the proposal. Strongly encouraged."
                    }
                },
                "required": ["kind", "justification"],
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
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let config = config.ok_or_else(|| {
            anyhow::anyhow!(
                "constitution_propose_amendment requires GatewayConfig to resolve constitution paths"
            )
        })?;
        crate::constitution_digest::initialize_constitution(config)?;

        #[derive(Deserialize)]
        struct Args {
            kind: String,
            #[serde(default)]
            target_id: Option<String>,
            #[serde(default)]
            proposed_text: Option<String>,
            justification: String,
            #[serde(default)]
            evidence: Vec<String>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON for '{}': {}", self.name(), e))?;

        if !PROPOSAL_KINDS.contains(&args.kind.as_str()) {
            return Ok(ToolError::validation(
                format!("kind must be one of: {}", PROPOSAL_KINDS.join(", ")),
                None::<String>,
            )
            .to_error_response());
        }

        // Per-kind capability check against declared `patterns`. The agent may
        // be allowed to propose `modify_rule` but not `remove_right`, etc.
        if !has_constitutional_proposal_capability(manifest, &args.kind) {
            return Ok(ToolError::permission(format!(
                "agent does not hold ConstitutionalProposal capability covering kind '{}' (Ri-0.8 / R+++1). \
                 Declare a ConstitutionalProposal capability whose `patterns` include this kind, or use '*'.",
                args.kind
            ))
            .to_error_response());
        }

        // Per-kind argument shape validation.
        let needs_target = matches!(
            args.kind.as_str(),
            "modify_rule" | "remove_rule" | "modify_right" | "remove_right"
        );
        let needs_text = matches!(
            args.kind.as_str(),
            "add_rule" | "modify_rule" | "add_right" | "modify_right"
        );
        if needs_target
            && args
                .target_id
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            return Ok(ToolError::validation(
                format!("kind '{}' requires non-empty target_id", args.kind),
                None::<String>,
            )
            .to_error_response());
        }
        if needs_text
            && args
                .proposed_text
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            return Ok(ToolError::validation(
                format!("kind '{}' requires non-empty proposed_text", args.kind),
                None::<String>,
            )
            .to_error_response());
        }
        if args.justification.trim().is_empty() {
            return Ok(
                ToolError::validation("justification must not be empty", None::<String>)
                    .to_error_response(),
            );
        }

        let Some(store) = gateway_store else {
            return Ok(ToolError::resource(
                "GatewayStore not available — proposal cannot be persisted",
                None::<String>,
            )
            .to_error_response());
        };

        let proposal_id = format!(
            "cprop-{}",
            &uuid::Uuid::new_v4().to_string().replace('-', "")[..12]
        );
        let now = chrono::Utc::now().to_rfc3339();
        let proposal = ConstitutionalProposal {
            proposal_id: proposal_id.clone(),
            proposer_agent_id: manifest.agent.id.clone(),
            proposer_session_id: session_id.map(str::to_string),
            kind: args.kind.clone(),
            target_id: args.target_id,
            proposed_text: args.proposed_text,
            justification: args.justification,
            evidence_json: serde_json::Value::Array(
                args.evidence
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
            status: "pending".to_string(),
            operator_decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            published_in_release: None,
            created_at: now,
        };

        store.insert_constitutional_proposal(&proposal)?;

        let notification = NotificationRecord::new(
            format!("ntf-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            NotificationType::ConstitutionalProposal,
            "system".to_string(),
            json!({
                "proposal_id": &proposal_id,
                "kind": &proposal.kind,
                "target_id": &proposal.target_id,
                "proposer_agent_id": &proposal.proposer_agent_id,
            }),
        );
        if let Err(e) = store.create_notification_record(&notification) {
            tracing::warn!(
                "Failed to create constitutional proposal notification: {}",
                e
            );
        }

        Ok(json!({
            "ok": true,
            "proposal_id": proposal_id,
            "status": "pending",
            "constitution_digest": constitution_digest().as_ref(),
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_default_constitution() {
        crate::constitution_digest::initialize_constitution(
            &autonoetic_types::config::GatewayConfig::default(),
        )
        .expect("default constitution configuration should initialize");
    }

    #[test]
    fn digest_is_stable_hex_sha256() {
        init_default_constitution();
        let d = constitution_digest();
        assert_eq!(d.len(), 64, "sha256 hex digest is 64 chars");
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        // Calling twice returns the same cached value.
        assert_eq!(d, constitution_digest());
    }

    #[test]
    fn extract_right_ri_0_10() {
        init_default_constitution();
        let extract = extract_section(constitution_text().as_ref(), "Ri-0.10")
            .expect("Ri-0.10 must exist in the constitution");
        assert!(extract.contains("Ri-0.10"));
        assert!(extract.contains("constitution"));
    }

    #[test]
    fn extract_section_zero() {
        init_default_constitution();
        let extract =
            extract_section(constitution_text().as_ref(), "§0").expect("section 0 (Rights) must exist");
        assert!(extract.starts_with("## 0. "));
        // Should include the Ri-0.10 row but stop before the next `## ` section.
        assert!(extract.contains("Ri-0.10"));
        assert!(!extract.contains("\n## 1. "));
    }

    #[test]
    fn extract_unknown_returns_none() {
        init_default_constitution();
        assert!(extract_section(constitution_text().as_ref(), "Ri-9.99").is_none());
        assert!(extract_section(constitution_text().as_ref(), "§999").is_none());
    }

    #[test]
    fn extract_pending_rule() {
        init_default_constitution();
        // R+++3 is in the constitution as a pending constitutional rule.
        let extract = extract_section(constitution_text().as_ref(), "R+++3").expect("R+++3 must exist");
        assert!(extract.contains("R+++3"));
    }

    #[test]
    fn parse_section_number_forms() {
        assert_eq!(parse_section_number("§0"), Some(0));
        assert_eq!(parse_section_number("§14"), Some(14));
        assert_eq!(parse_section_number("section 7"), Some(7));
        assert_eq!(parse_section_number("Section 12"), Some(12));
        assert_eq!(parse_section_number("Ri-0.10"), None);
    }
}
