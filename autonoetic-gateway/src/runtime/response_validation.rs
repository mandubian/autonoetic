//! Response validation gate — validates agent outputs against declared constraints.
//!
//! When enabled, gateway checks SpawnResult against the agent's ResponseContract.
//! Returns violations for each failed check.

use autonoetic_types::agent::ResponseContract;
use regex::RegexBuilder;
use std::collections::HashSet;
use std::path::Path;

use crate::execution::SpawnResult;

/// A single validation violation found during response checking.
#[derive(Debug, Clone)]
pub struct ValidationViolation {
    /// Which rule was violated (e.g. "required_artifacts", "max_artifacts").
    pub rule: String,
    /// Human-readable description of what went wrong.
    pub message: String,
    /// Hint the agent can use to repair the violation.
    pub repair_hint: String,
}

impl std::fmt::Display for ValidationViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.rule, self.message)
    }
}

/// Parse a `ResponseContract` from spawn metadata.
pub fn parse_response_contract(
    metadata: Option<&serde_json::Value>,
) -> anyhow::Result<Option<ResponseContract>> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    let Some(contract_value) = metadata.get("response_contract") else {
        return Ok(None);
    };

    let mut contract: ResponseContract = serde_json::from_value(contract_value.clone())
        .map_err(|e| anyhow::anyhow!("invalid response_contract metadata: {}", e))?;

    contract.normalize();

    for pattern in &contract.prohibited_text_patterns {
        RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| {
                anyhow::anyhow!(
                    "invalid prohibited_text_patterns regex '{}': {}",
                    pattern,
                    e
                )
            })?;
    }

    if contract.is_empty() {
        return Ok(None);
    }
    Ok(Some(contract))
}

/// Validate a `SpawnResult` against a `ResponseContract`.
///
/// Returns an empty vector when all checks pass.
pub fn validate_spawn_response(
    result: &SpawnResult,
    contract: &ResponseContract,
    gateway_dir: Option<&Path>,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    // 1. Required artifacts
    for required in &contract.required_artifacts {
        let found = result.artifacts.iter().any(|a| a.name == *required)
            || result.files.iter().any(|f| f.name == *required);
        if !found {
            violations.push(ValidationViolation {
                rule: "required_artifacts".into(),
                message: format!("required artifact '{}' not produced", required),
                repair_hint: format!(
                    "Create '{}' with content.write then register with artifact.build",
                    required
                ),
            });
        }
    }

    // 2. Max artifacts
    if let Some(max) = contract.max_artifacts {
        if result.artifacts.len() > max {
            violations.push(ValidationViolation {
                rule: "max_artifacts".into(),
                message: format!(
                    "artifact count {} exceeds max_artifacts ({})",
                    result.artifacts.len(),
                    max
                ),
                repair_hint: format!("Reduce artifacts to {} or fewer", max),
            });
        }
    }

    // 3. Max total size of unique named outputs.
    if let Some(max_mb) = contract.max_total_size_mb {
        match compute_total_output_size_bytes(result, gateway_dir) {
            Ok(total_bytes) => {
                let max_bytes = max_mb.saturating_mul(1024 * 1024);
                if total_bytes > max_bytes {
                    violations.push(ValidationViolation {
                        rule: "max_total_size_mb".into(),
                        message: format!(
                            "total output size {} bytes exceeds max_total_size_mb ({} bytes)",
                            total_bytes, max_bytes
                        ),
                        repair_hint: format!(
                            "Reduce output size to {} MiB or fewer by removing or shrinking generated files",
                            max_mb
                        ),
                    });
                }
            }
            Err(e) => {
                violations.push(ValidationViolation {
                    rule: "max_total_size_mb".into(),
                    message: format!("cannot verify output size: {}", e),
                    repair_hint: "Ensure the gateway content store is available and output files are written via content.write".into(),
                });
            }
        }
    }

    // 4. Max reply length
    if let Some(max_chars) = contract.max_reply_length_chars {
        if let Some(ref reply) = result.assistant_reply {
            if reply.len() > max_chars {
                violations.push(ValidationViolation {
                    rule: "max_reply_length_chars".into(),
                    message: format!("reply {} chars exceeds max ({})", reply.len(), max_chars),
                    repair_hint: format!("Shorten reply to {} chars", max_chars),
                });
            }
        }
    }

    // 5. Prohibited text patterns — compile the validated regex and match case-insensitively.
    if let Some(ref reply) = result.assistant_reply {
        for pattern in &contract.prohibited_text_patterns {
            // Patterns were validated at parse_response_contract time; compile is safe.
            let Ok(re) = RegexBuilder::new(pattern).case_insensitive(true).build() else {
                continue; // defensive — should never happen after parse validation
            };
            if re.is_match(reply) {
                violations.push(ValidationViolation {
                    rule: "prohibited_text_pattern".into(),
                    message: format!("reply matches prohibited pattern '{}'", pattern),
                    repair_hint: "Remove or redact the matched text".into(),
                });
            }
        }
    }

    // 6. Output schema (JSON only, lightweight validation)
    if let Some(ref schema) = contract.output_schema {
        let schema_is_constrained =
            schema.get("required").is_some() || schema.get("properties").is_some();
        match result.assistant_reply.as_deref() {
            None if schema_is_constrained => {
                violations.push(ValidationViolation {
                    rule: "output_schema".into(),
                    message: "no reply produced but output_schema requires structured output"
                        .into(),
                    repair_hint: "Return JSON matching the declared schema".into(),
                });
            }
            Some(reply) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(reply) {
                    violations.extend(validate_json_against_schema(&json, schema));
                } else if schema_is_constrained {
                    violations.push(ValidationViolation {
                        rule: "output_schema".into(),
                        message:
                            "reply is not valid JSON but output schema requires structured output"
                                .into(),
                        repair_hint: "Return JSON matching the declared schema".into(),
                    });
                }
            }
            None => {} // schema has no constraints; no reply is acceptable
        }
    }

    violations
}

fn compute_total_output_size_bytes(
    result: &SpawnResult,
    gateway_dir: Option<&Path>,
) -> anyhow::Result<u64> {
    let Some(gateway_dir) = gateway_dir else {
        anyhow::bail!("gateway directory unavailable");
    };

    let store = crate::runtime::content_store::ContentStore::new(gateway_dir)?;
    let mut unique_handles = HashSet::new();
    let mut total_bytes = 0u64;

    for file in &result.files {
        if !unique_handles.insert(file.handle.clone()) {
            continue;
        }

        let blob_path = store.blob_path(&file.handle);
        let metadata = std::fs::metadata(&blob_path).map_err(|e| {
            anyhow::anyhow!(
                "failed reading blob metadata for '{}' ({}) : {}",
                file.name,
                file.handle,
                e
            )
        })?;
        total_bytes = total_bytes.saturating_add(metadata.len());
    }

    Ok(total_bytes)
}

/// Validate that a required promotion.record was called during the session.
///
/// When metadata contains `require_promotion_record: true`, the gateway checks
/// the PromotionStore for a matching record. Two failure modes:
/// 1. No record exists at all → agent forgot to call `promotion.record` → repairable
/// 2. Record exists but pass=false → evaluator/auditor rejected the artifact → terminal
pub fn validate_promotion_record(
    gateway_dir: Option<&Path>,
    promotion_artifact_id: &str,
    promotion_role: &str,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    let Some(gw_dir) = gateway_dir else {
        violations.push(ValidationViolation {
            rule: "promotion_record".into(),
            message: "cannot verify promotion record: gateway directory unavailable".into(),
            repair_hint: "Ensure the gateway directory is configured".into(),
        });
        return violations;
    };

    let store = match crate::runtime::promotion_store::PromotionStore::new(gw_dir) {
        Ok(s) => s,
        Err(e) => {
            violations.push(ValidationViolation {
                rule: "promotion_record".into(),
                message: format!("cannot load promotion store: {}", e),
                repair_hint: "Ensure the gateway promotion store is accessible".into(),
            });
            return violations;
        }
    };

    match store.get_promotion(promotion_artifact_id) {
        None => {
            violations.push(ValidationViolation {
                rule: "promotion_record_missing".into(),
                message: format!(
                    "completed without a matching promotion.record within the session for artifact '{}' (role: {})",
                    promotion_artifact_id, promotion_role
                ),
                repair_hint: format!(
                    "Call promotion.record with artifact_id='{}', role='{}', pass=true (or false if validation failed). Example: promotion.record({{\"artifact_id\": \"{}\", \"role\": \"{}\", \"pass\": true}})",
                    promotion_artifact_id, promotion_role, promotion_artifact_id, promotion_role
                ),
            });
        }
        Some(record) => {
            let passed = match promotion_role {
                "evaluator" => record.evaluator_pass,
                "auditor" => record.auditor_pass,
                _ => {
                    violations.push(ValidationViolation {
                        rule: "promotion_record".into(),
                        message: format!("unknown promotion role '{}'", promotion_role),
                        repair_hint: "Use 'evaluator' or 'auditor'".into(),
                    });
                    return violations;
                }
            };

            if !passed {
                let findings = match promotion_role {
                    "evaluator" => &record.evaluator_findings,
                    "auditor" => &record.auditor_findings,
                    _ => &Vec::<autonoetic_types::promotion::Finding>::new(),
                };
                let findings_summary = if findings.is_empty() {
                    "no findings provided".to_string()
                } else {
                    findings
                        .iter()
                        .map(|f| format!("[{:?}] {}", f.severity, f.description))
                        .collect::<Vec<_>>()
                        .join("; ")
                };
                violations.push(ValidationViolation {
                    rule: "promotion_record_failed".into(),
                    message: format!(
                        "{} recorded pass=false for artifact '{}': {}",
                        promotion_role, promotion_artifact_id, findings_summary
                    ),
                    repair_hint: format!(
                        "The {} rejected the artifact. Fix the issues and re-run validation before installing.",
                        promotion_role
                    ),
                });
            }
        }
    }

    violations
}

/// Validate durable tool-evidence requirements using gateway execution traces.
pub fn validate_session_evidence(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
    contract: &ResponseContract,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    let min_builds = contract.min_artifact_builds.unwrap_or(0);
    if min_builds == 0 {
        return violations;
    }

    let Some(store) = gateway_store else {
        violations.push(ValidationViolation {
            rule: "artifact_build_evidence".into(),
            message: "cannot verify artifact.build evidence: gateway store unavailable".into(),
            repair_hint:
                "Retry with gateway store enabled, and ensure artifact.build is called before finishing"
                    .into(),
        });
        return violations;
    };

    let traces = match store.search_execution_traces(
        Some("artifact.build"),
        Some(true),
        None,
        None,
        None,
        Some(session_id),
        10_000,
    ) {
        Ok(t) => t,
        Err(e) => {
            violations.push(ValidationViolation {
                rule: "artifact_build_evidence".into(),
                message: format!("failed querying execution traces: {}", e),
                repair_hint: "Retry the run and ensure gateway tracing is operational".into(),
            });
            return violations;
        }
    };

    let build_count = traces.len() as u32;
    if build_count < min_builds {
        violations.push(ValidationViolation {
            rule: "artifact_build_evidence".into(),
            message: format!(
                "requires at least {} successful artifact.build call(s), found {}",
                min_builds, build_count
            ),
            repair_hint:
                "Create required files with content.write, then call artifact.build before finishing"
                    .into(),
        });
    }

    violations
}

/// Build a structured repair prompt to inject back into the child agent session.
///
/// Uses clear section headers, reasoning, and examples to help LLM agents understand
/// what failed and how to fix it. Designed for LLM reasoning patterns (prose > JSON).
pub fn build_repair_prompt(
    violations: &[ValidationViolation],
    attempt: usize,
    max_repair_rounds: usize,
) -> String {
    let remaining = max_repair_rounds - attempt + 1;

    // Build violations section with reasoning
    let violations_section: Vec<String> = violations
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let rule_explanation = match v.rule.as_str() {
                "required_artifacts" => "You must produce this file as a deliverable.",
                "max_artifacts" => "You produced too many files; consolidate them.",
                "max_total_size_mb" => "Your output is too large; reduce file sizes.",
                "max_reply_length_chars" => "Your text reply is too long; be concise.",
                "prohibited_text_pattern" => "Your reply contains sensitive data or unsafe content.",
                "output_schema" => "Your output does not match the required JSON schema.",
                "promotion_record_missing" => "You forgot to call promotion.record — this is required for artifact promotion gates.",
                "promotion_record_failed" => "The evaluator or auditor rejected the artifact. This cannot be auto-repaired.",
                _ => "Your output violates a declared constraint.",
            };

            format!(
                "{}. [{}] {}\n   Why: {}\n   Fix: {}",
                i + 1,
                v.rule,
                v.message,
                rule_explanation,
                v.repair_hint
            )
        })
        .collect();

    // Build repair instructions based on violation types
    let repair_examples = build_repair_examples(violations);

    format!(
        "[GATEWAY_VALIDATION] REPAIR REQUIRED — Attempt {}/{}\n\
═══════════════════════════════════════════════════════════════════════\n\n\
WHAT FAILED:\n\
───────────────────────────────────────────────────────────────────────\n\
Your previous output failed validation. These {} constraint(s) must be fixed:\n\n\
{}\n\n\
WHAT TO DO:\n\
───────────────────────────────────────────────────────────────────────\n\
For each violation above:\n\
• Understand why it failed (the \"Why\" explanation)\n\
• Apply the fix (the \"Fix\" hint)\n\
• Use your normal tools: artifact.build(), content.write(), etc.\n\
• Re-run your workflow to regenerate the output\n\n\
EXAMPLES OF CORRECT OUTPUT:\n\
───────────────────────────────────────────────────────────────────────\n\
{}\n\n\
CONSTRAINT SUMMARY:\n\
───────────────────────────────────────────────────────────────────────\n\
✓ Fix ALL {} issue(s) above before finishing\n\
✓ {} repair attempt(s) remaining\n\
✓ After fixes, run your workflow again to produce corrected output\n\n\
Continue repairing your output.",
        attempt,
        max_repair_rounds,
        violations.len(),
        violations_section.join("\n"),
        repair_examples,
        violations.len(),
        remaining
    )
}

/// Generate contextual repair examples based on violation types.
fn build_repair_examples(violations: &[ValidationViolation]) -> String {
    let mut examples = Vec::new();

    for v in violations {
        match v.rule.as_str() {
            "required_artifacts" => {
                examples.push(
                    "Required Artifact:\n  \
                     Use artifact.build({\"name\": \"filename.ext\", ...}) or \n  \
                     content.write(\"path/to/file\", contents) to create the file."
                        .to_string(),
                );
            }
            "max_reply_length_chars" => {
                examples.push(
                    "Reply Length:\n  \
                     Condense your response. Remove verbose explanations and keep only essential info.\n  \
                     Target: 1-2 paragraph summary instead of detailed analysis."
                        .to_string()
                );
            }
            "prohibited_text_pattern" => {
                examples.push(
                    "Sensitive Data:\n  \
                     ❌ BAD:  api_key = \"sk-1234567890abcdef\"\n  \
                     ✓ GOOD: api_key = \"<use_credential_store>\" or \"${SECRET_API_KEY}\""
                        .to_string(),
                );
            }
            "output_schema" => {
                examples.push(
                    "JSON Schema:\n  \
                     ❌ BAD:  \"result completed\"\n  \
                     ✓ GOOD: {\"status\": \"success\", \"result\": \"...\"}"
                        .to_string(),
                );
            }
            "max_artifacts" => {
                examples.push(
                    "Artifact Consolidation:\n  \
                     Combine similar files into fewer artifacts or use subdirectories."
                        .to_string(),
                );
            }
            "promotion_record_missing" => {
                examples.push(
                    "Promotion Record:\n  \
                     Call promotion.record as a tool (not via sandbox.exec):\n  \
                     promotion.record({\"artifact_id\": \"<your_artifact_id>\", \"role\": \"evaluator\", \"pass\": true, \"summary\": \"Tests passed\"})"
                        .to_string()
                );
            }
            _ => {}
        }
    }

    if examples.is_empty() {
        "Generic: Review your output carefully and ensure all violations are resolved.".to_string()
    } else {
        examples.join("\n\n  ")
    }
}

/// Convert violations into a terminal `anyhow::Error` for propagation to the caller.
///
/// When `include_session_context` is true (repair mode was active), the error includes
/// the `session_id` and a "Repair hints" block so the calling agent can understand
/// the failure and take corrective action at a higher level.
pub fn violations_to_final_error(
    violations: &[ValidationViolation],
    session_id: &str,
    include_session_context: bool,
) -> anyhow::Error {
    let summary: String = violations
        .iter()
        .map(|v| {
            format!(
                "[{}] {} (repair_hint: {})",
                v.rule, v.message, v.repair_hint
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    if include_session_context {
        let hints: String = violations
            .iter()
            .map(|v| v.repair_hint.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!(
            "response validation failed: {}. Session: {}. Repair hints: {}",
            summary,
            session_id,
            hints
        )
    } else {
        anyhow::anyhow!("response validation failed: {}", summary)
    }
}

/// Lightweight JSON schema validation (required + type + enum + minLength).
fn validate_json_against_schema(
    json: &serde_json::Value,
    schema: &serde_json::Value,
) -> Vec<ValidationViolation> {
    let mut violations = Vec::new();

    // Required fields
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for req in required {
            if let Some(field) = req.as_str() {
                if json.get(field).is_none() {
                    violations.push(ValidationViolation {
                        rule: "output_schema".into(),
                        message: format!("required field '{}' missing", field),
                        repair_hint: format!("Include '{}' in your JSON reply", field),
                    });
                }
            }
        }
    }

    // Property checks
    if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
        for (key, prop_schema) in properties {
            let Some(value) = json.get(key) else { continue };

            // Type check
            if let Some(type_spec) = prop_schema.get("type").and_then(|v| v.as_str()) {
                let matches = match type_spec {
                    "string" => value.is_string(),
                    "number" => value.is_number(),
                    "integer" => value.is_i64() || value.is_u64(),
                    "boolean" => value.is_boolean(),
                    "object" => value.is_object(),
                    "array" => value.is_array(),
                    "null" => value.is_null(),
                    _ => true,
                };
                if !matches {
                    violations.push(ValidationViolation {
                        rule: "output_schema".into(),
                        message: format!(
                            "field '{}' expected type '{}', got {}",
                            key,
                            type_spec,
                            json_type_name(value)
                        ),
                        repair_hint: format!("Set '{}' to type '{}'", key, type_spec),
                    });
                }
            }

            // Enum check
            if let Some(enum_vals) = prop_schema.get("enum").and_then(|v| v.as_array()) {
                if !enum_vals.contains(value) {
                    violations.push(ValidationViolation {
                        rule: "output_schema".into(),
                        message: format!(
                            "field '{}' value {:?} not in enum {:?}",
                            key, value, enum_vals
                        ),
                        repair_hint: format!("Use one of the allowed values for '{}'", key),
                    });
                }
            }

            // minLength for strings
            if let Some(min_len) = prop_schema.get("minLength").and_then(|v| v.as_u64()) {
                if let Some(s) = value.as_str() {
                    if (s.len() as u64) < min_len {
                        violations.push(ValidationViolation {
                            rule: "output_schema".into(),
                            message: format!(
                                "field '{}' length {} < minLength {}",
                                key,
                                s.len(),
                                min_len
                            ),
                            repair_hint: format!("Ensure '{}' has at least {} chars", key, min_len),
                        });
                    }
                }
            }
        }
    }

    violations
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ArtifactMetadata, ContentFile};

    fn make_result(
        artifacts: Vec<ArtifactMetadata>,
        files: Vec<ContentFile>,
        reply: Option<&str>,
    ) -> SpawnResult {
        SpawnResult {
            agent_id: "test.agent".into(),
            session_id: "sess-1".into(),
            assistant_reply: reply.map(|s| s.to_string()),
            should_signal_background: false,
            artifacts,
            files,
            shared_knowledge: vec![],
            llm_usage: vec![],
            suspended_for_approval: None,
        }
    }

    fn make_artifact(name: &str) -> ArtifactMetadata {
        ArtifactMetadata {
            id: format!("art-{}", name),
            name: name.to_string(),
            description: String::new(),
            files: vec![],
            entry_point: None,
            io: None,
        }
    }

    #[test]
    fn test_required_artifacts_pass() {
        let c = ResponseContract {
            required_artifacts: vec!["report.md".into()],
            ..Default::default()
        };
        let r = make_result(vec![make_artifact("report.md")], vec![], Some("done"));
        assert!(validate_spawn_response(&r, &c, None).is_empty());
    }

    #[test]
    fn test_required_artifacts_fail() {
        let c = ResponseContract {
            required_artifacts: vec!["report.md".into(), "data.json".into()],
            ..Default::default()
        };
        let r = make_result(vec![make_artifact("report.md")], vec![], Some("done"));
        let v = validate_spawn_response(&r, &c, None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "required_artifacts");
    }

    #[test]
    fn test_max_artifacts() {
        let c = ResponseContract {
            max_artifacts: Some(2),
            ..Default::default()
        };
        let r = make_result(
            vec![make_artifact("a"), make_artifact("b"), make_artifact("c")],
            vec![],
            None,
        );
        assert_eq!(validate_spawn_response(&r, &c, None).len(), 1);
    }

    #[test]
    fn test_prohibited_text() {
        let c = ResponseContract {
            prohibited_text_patterns: vec!["API_KEY".into()],
            ..Default::default()
        };
        let r = make_result(vec![], vec![], Some("key is API_KEY=xyz"));
        assert_eq!(validate_spawn_response(&r, &c, None).len(), 1);
    }

    #[test]
    fn test_output_schema_required_fields() {
        let c = ResponseContract {
            output_schema: Some(serde_json::json!({"required": ["status", "summary"]})),
            ..Default::default()
        };
        let r = make_result(vec![], vec![], Some(r#"{"status": "ok"}"#));
        let v = validate_spawn_response(&r, &c, None);
        assert!(v.iter().any(|v| v.message.contains("summary")));
    }

    #[test]
    fn test_output_schema_type_check() {
        let c = ResponseContract {
            output_schema: Some(serde_json::json!({
                "properties": {"count": {"type": "integer"}}
            })),
            ..Default::default()
        };
        let r = make_result(vec![], vec![], Some(r#"{"count": "not_a_number"}"#));
        let v = validate_spawn_response(&r, &c, None);
        assert!(v
            .iter()
            .any(|v| v.message.contains("count") && v.message.contains("integer")));
    }

    #[test]
    fn test_no_contract_passes() {
        let c = ResponseContract::default();
        let r = make_result(vec![], vec![], Some("anything"));
        assert!(validate_spawn_response(&r, &c, None).is_empty());
    }

    #[test]
    fn test_non_json_reply_fails_schema_validation() {
        let c = ResponseContract {
            output_schema: Some(serde_json::json!({"required": ["status"]})),
            ..Default::default()
        };
        let r = make_result(vec![], vec![], Some("plain text reply"));
        let v = validate_spawn_response(&r, &c, None);
        assert!(v.iter().any(|v| v.rule == "output_schema"));
        assert!(v.iter().any(|v| v.message.contains("valid JSON")));
    }

    #[test]
    fn test_missing_reply_fails_schema_validation() {
        let c = ResponseContract {
            output_schema: Some(serde_json::json!({"required": ["status"]})),
            ..Default::default()
        };
        let r = make_result(vec![], vec![], None);
        let v = validate_spawn_response(&r, &c, None);
        assert!(v.iter().any(|v| v.rule == "output_schema"));
        assert!(v.iter().any(|v| v.message.contains("no reply produced")));
    }

    #[test]
    fn test_max_total_size_mb_enforced() {
        let temp = tempfile::tempdir().unwrap();
        let gw = temp.path().join(".gateway");
        std::fs::create_dir_all(&gw).unwrap();
        let store = crate::runtime::content_store::ContentStore::new(&gw).unwrap();
        let handle = store.write(&vec![b'x'; 2 * 1024 * 1024]).unwrap();

        let c = ResponseContract {
            max_total_size_mb: Some(1),
            ..Default::default()
        };
        let r = make_result(
            vec![],
            vec![ContentFile {
                name: "big.bin".into(),
                handle,
                alias: "deadbeef".into(),
            }],
            Some("done"),
        );
        let v = validate_spawn_response(&r, &c, Some(&gw));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule, "max_total_size_mb");
        assert!(v[0].message.contains("exceeds"));
    }

    #[test]
    fn test_text_pattern_regex_matching() {
        // Regex anchor: matches only word boundary, not substring
        let c = ResponseContract {
            prohibited_text_patterns: vec!["\\bsecret\\b".into()],
            ..Default::default()
        };
        let r_match = make_result(vec![], vec![], Some("this is a secret value"));
        let r_no_match = make_result(vec![], vec![], Some("secretive behavior"));
        assert_eq!(validate_spawn_response(&r_match, &c, None).len(), 1);
        assert!(validate_spawn_response(&r_no_match, &c, None).is_empty());
    }

    #[test]
    fn test_text_pattern_case_insensitive() {
        let c = ResponseContract {
            prohibited_text_patterns: vec!["API_KEY".into()],
            ..Default::default()
        };
        let r = make_result(vec![], vec![], Some("the api_key was leaked"));
        let v = validate_spawn_response(&r, &c, None);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn test_parse_response_contract_invalid_regex() {
        let metadata = serde_json::json!({
            "response_contract": {
                "prohibited_text_patterns": ["[invalid regex"]
            }
        });
        assert!(parse_response_contract(Some(&metadata)).is_err());
    }

    #[test]
    fn test_build_repair_prompt_contains_violations() {
        let violations = vec![ValidationViolation {
            rule: "required_artifacts".into(),
            message: "missing 'report.md'".into(),
            repair_hint: "create report.md".into(),
        }];
        let prompt = build_repair_prompt(&violations, 1, 2);
        assert!(prompt.contains("[GATEWAY_VALIDATION]"));
        assert!(prompt.contains("required_artifacts"));
        assert!(prompt.contains("Attempt 1/2"));
        assert!(prompt.contains("create report.md"));
    }

    #[test]
    fn test_violations_to_final_error_without_context() {
        let violations = vec![ValidationViolation {
            rule: "required_artifacts".into(),
            message: "missing 'x.md'".into(),
            repair_hint: "create x.md".into(),
        }];
        let e = violations_to_final_error(&violations, "sess-abc", false);
        let msg = e.to_string();
        assert!(msg.contains("required_artifacts"));
        assert!(msg.contains("repair_hint"));
        assert!(!msg.contains("sess-abc"));
    }

    #[test]
    fn test_violations_to_final_error_with_context() {
        let violations = vec![ValidationViolation {
            rule: "required_artifacts".into(),
            message: "missing 'x.md'".into(),
            repair_hint: "create x.md".into(),
        }];
        let e = violations_to_final_error(&violations, "sess-abc", true);
        let msg = e.to_string();
        assert!(msg.contains("sess-abc"));
        assert!(msg.contains("Repair hints"));
        assert!(msg.contains("create x.md"));
    }

    #[test]
    fn test_promotion_record_missing() {
        let temp = tempfile::tempdir().unwrap();
        let violations = validate_promotion_record(Some(temp.path()), "art_missing", "evaluator");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record_missing");
        assert!(violations[0].message.contains("art_missing"));
        assert!(violations[0].repair_hint.contains("promotion.record"));
    }

    #[test]
    fn test_promotion_record_evaluator_pass() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::runtime::promotion_store::PromotionStore::new(temp.path()).unwrap();
        use autonoetic_types::promotion::PromotionRole;
        store
            .record_promotion(
                "art_good".to_string(),
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                true,
                vec![],
                Some("all good".to_string()),
            )
            .unwrap();

        let violations = validate_promotion_record(Some(temp.path()), "art_good", "evaluator");
        assert!(violations.is_empty());
    }

    #[test]
    fn test_promotion_record_evaluator_fail() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::runtime::promotion_store::PromotionStore::new(temp.path()).unwrap();
        use autonoetic_types::promotion::{Finding, FindingSeverity, PromotionRole};
        store
            .record_promotion(
                "art_bad".to_string(),
                None,
                PromotionRole::Evaluator,
                "evaluator.default",
                false,
                vec![Finding {
                    severity: FindingSeverity::Error,
                    description: "tests failed".to_string(),
                    evidence: None,
                }],
                None,
            )
            .unwrap();

        let violations = validate_promotion_record(Some(temp.path()), "art_bad", "evaluator");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record_failed");
        assert!(violations[0].message.contains("pass=false"));
        assert!(violations[0].message.contains("tests failed"));
    }

    #[test]
    fn test_promotion_record_auditor_fail() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::runtime::promotion_store::PromotionStore::new(temp.path()).unwrap();
        use autonoetic_types::promotion::{Finding, FindingSeverity, PromotionRole};
        store
            .record_promotion(
                "art_audit".to_string(),
                None,
                PromotionRole::Auditor,
                "auditor.default",
                false,
                vec![Finding {
                    severity: FindingSeverity::Critical,
                    description: "security risk".to_string(),
                    evidence: Some("found network access".to_string()),
                }],
                None,
            )
            .unwrap();

        let violations = validate_promotion_record(Some(temp.path()), "art_audit", "auditor");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record_failed");
        assert!(violations[0].message.contains("security risk"));
    }

    #[test]
    fn test_promotion_record_no_gateway_dir() {
        let violations = validate_promotion_record(None, "art_x", "evaluator");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "promotion_record");
    }
}
