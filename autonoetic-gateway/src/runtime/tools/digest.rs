use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(DigestAnnotateTool));
}

pub struct DigestAnnotateTool;

impl NativeTool for DigestAnnotateTool {
    fn name(&self) -> &'static str {
        "digest_annotate"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Add ONE reasoning, decision, observation, or lesson line to the live session digest. Annotations are audit notes — they are not work and do not advance the session. Record an event once; do not re-annotate the same event with reworded content, and never annotate in place of acting or ending your turn.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "type": {
                        "type": "string",
                        "enum": ["reasoning", "decision", "observation", "lesson"],
                        "description": "Category of annotation"
                    },
                    "content": {
                        "type": "string",
                        "description": "Text to record in the digest"
                    }
                },
                "required": ["type", "content"],
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
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(rename = "type")]
            annotation_type: String,
            content: String,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;
        let allowed = ["reasoning", "decision", "observation", "lesson"];
        if !allowed.contains(&args.annotation_type.as_str()) {
            return Ok(ToolError::validation(
                format!("type must be one of: {}", allowed.join(", ")),
                None::<String>,
            )
            .to_error_response());
        }
        let mut annotations_total: Option<u32> = None;
        if let Some(ctx) = run_context {
            if let Some(w) = &ctx.live_digest {
                if let Ok(mut g) = w.lock() {
                    g.record_annotation(&args.annotation_type, &args.content)?;
                }
            }
            // #1092: echo the running total so redundancy is visible
            // in-context. The counter is session-scoped; the hint escalates
            // at 3+ (one or two annotations are legitimate audit notes).
            if let Some(counter) = &ctx.annotation_counter {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                annotations_total = Some(n);
            }
            if let Some(w) = &ctx.live_report {
                if let Ok(mut g) = w.lock() {
                    let _ = g.record_annotation(&args.annotation_type, &args.content, _turn_id);
                }
            }
            if let Some(store) = _gateway_store.as_ref() {
                let note_role = crate::runtime::session_timeline::derive_role(&ctx.agent_id);
                let note_altitude = crate::runtime::session_timeline::altitude_for(
                    "digest_annotate",
                    &note_role,
                );
                let note_principal =
                    autonoetic_types::principal::Principal::agent(ctx.agent_id.clone());
                let _ = store.create_live_digest_event(
                    &crate::scheduler::gateway_store::LiveDigestEventRecord {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        root_session_id: ctx.root_session_id.clone(),
                        source_session_id: ctx.session_id.clone(),
                        turn_id: _turn_id.map(|s| s.to_string()),
                        source_agent_id: Some(ctx.agent_id.clone()),
                        source_node_id: crate::execution::gateway_actor_id(),
                        event_type: "digest_annotate".to_string(),
                        payload: Some(
                            serde_json::json!({
                                "type": args.annotation_type,
                                "content": crate::log_redaction::redact_text_for_logs(&args.content),
                            })
                            .to_string(),
                        ),
                        created_at: chrono::Utc::now().to_rfc3339(),
                        principal_kind: Some(note_principal.kind_to_storage()),
                        principal_id: Some(note_principal.id.clone()),
                        role: Some(note_role.to_storage()),
                        altitude: Some(note_altitude.as_str().to_string()),
                        refs_json: None,
                    },
                );
            }
        }
        let content_head: String = args.content.trim().chars().take(60).collect();
        let mut out = serde_json::json!({
            "ok": true,
            "recorded": true,
            "type": args.annotation_type,
            "content_head": content_head,
        });
        if let Some(n) = annotations_total {
            out["annotations_this_session"] = serde_json::json!(n);
            if n >= 3 {
                out["hint"] = serde_json::json!(
                    "This session already carries several annotations. Annotations are \
                     audit notes, not work: if the event is already recorded, do not \
                     re-annotate it with reworded content. If you are waiting on a child \
                     or an operator decision, END YOUR TURN — the gateway resumes you \
                     when the result lands. Otherwise take the next real action."
                );
            }
        }
        Ok(serde_json::to_string(&out)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1092: the result echoes what was recorded (type + content head) so
    /// redundancy is visible in-context. Without a run context (unit path)
    /// there is no counter — the echo stays, the count/hint do not.
    #[test]
    fn echo_includes_type_and_content_head_without_context() {
        let out = DigestAnnotateTool
            .execute(
                &Default::default(),
                &crate::policy::PolicyEngine::new(Default::default()),
                std::path::Path::new("."),
                None,
                r#"{"type":"decision","content":"build started"}"#,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        let v: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["recorded"], serde_json::json!(true));
        assert_eq!(v["type"], serde_json::json!("decision"));
        assert_eq!(v["content_head"], serde_json::json!("build started"));
        assert!(v.get("annotations_this_session").is_none());
        assert!(v.get("hint").is_none());
    }

    #[test]
    fn invalid_type_is_rejected() {
        let out = DigestAnnotateTool
            .execute(
                &Default::default(),
                &crate::policy::PolicyEngine::new(Default::default()),
                std::path::Path::new("."),
                None,
                r#"{"type":"bogus","content":"x"}"#,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        let v: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], serde_json::json!(false));
    }
}
