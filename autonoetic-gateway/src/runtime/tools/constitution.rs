//! `constitution_read` — Ri-0.10 (issue #95).
//!
//! Every agent has the right to read the full text of the constitution it is
//! operating under, addressed by digest. No capability gate: reading the law
//! is a right, not a privilege.
//!
//! Section selector accepts:
//!   - `Ri-X.Y` (rights), `R-X.Y` (numbered rules)
//!   - `R+N`, `R++N`, `R+++N` (pending / structural / constitutional)
//!   - `§N` or `section N` for whole numbered sections (`§0`..`§14`)
//!   - omitted / empty → entire document.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::OnceLock;

/// The full constitution text, embedded at compile time so the digest is a
/// property of the gateway binary itself.
const CONSTITUTION_TEXT: &str = include_str!("../../../../docs/gateway-constitution.md");

/// Constitution version is the gateway crate version. When the constitution
/// changes the crate's patch version (or higher) is expected to bump.
const CONSTITUTION_VERSION: &str = env!("CARGO_PKG_VERSION");

fn constitution_digest() -> &'static str {
    static DIGEST: OnceLock<String> = OnceLock::new();
    DIGEST.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(CONSTITUTION_TEXT.as_bytes());
        hex::encode(hasher.finalize())
    })
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ConstitutionReadTool));
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
                document version and is stable for a given gateway binary."
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
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
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
            (CONSTITUTION_TEXT.to_string(), None)
        } else {
            match extract_section(CONSTITUTION_TEXT, selector) {
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
            "digest": constitution_digest(),
            "version": CONSTITUTION_VERSION,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_hex_sha256() {
        let d = constitution_digest();
        assert_eq!(d.len(), 64, "sha256 hex digest is 64 chars");
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        // Calling twice returns the same cached value.
        assert_eq!(d, constitution_digest());
    }

    #[test]
    fn extract_right_ri_0_10() {
        let extract = extract_section(CONSTITUTION_TEXT, "Ri-0.10")
            .expect("Ri-0.10 must exist in the constitution");
        assert!(extract.contains("Ri-0.10"));
        assert!(extract.contains("constitution"));
    }

    #[test]
    fn extract_section_zero() {
        let extract = extract_section(CONSTITUTION_TEXT, "§0")
            .expect("section 0 (Rights) must exist");
        assert!(extract.starts_with("## 0. "));
        // Should include the Ri-0.10 row but stop before the next `## ` section.
        assert!(extract.contains("Ri-0.10"));
        assert!(!extract.contains("\n## 1. "));
    }

    #[test]
    fn extract_unknown_returns_none() {
        assert!(extract_section(CONSTITUTION_TEXT, "Ri-9.99").is_none());
        assert!(extract_section(CONSTITUTION_TEXT, "§999").is_none());
    }

    #[test]
    fn extract_pending_rule() {
        // R+++3 is in the constitution as a pending constitutional rule.
        let extract = extract_section(CONSTITUTION_TEXT, "R+++3")
            .expect("R+++3 must exist");
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
