//! Tool Call Processor for Agent Execution.
//!
//! Handles tool execution, disclosure tracking, and secret store integration.
//! Returns structured error responses for recoverable failures instead of aborting.

use crate::llm::ToolCall;
use crate::runtime::disclosure::DisclosureState;
use crate::runtime::failure_classification::{decorate_tool_error, normalize_tool_result_json};
use crate::runtime::mcp::McpToolRuntime;
use crate::runtime::session_tracer::SessionTracer;
use crate::runtime::store::SecretStoreRuntime;
use crate::runtime::tools::NativeToolRegistry;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::disclosure::DisclosureClass;
use autonoetic_types::tool_error::{ToolError, ToolErrorType};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct ToolCallProcessor<'a> {
    mcp_runtime: &'a mut McpToolRuntime,
    registry: &'a NativeToolRegistry,
    manifest: &'a AgentManifest,
    disclosure_state: &'a mut DisclosureState,
    secret_store: Option<&'a mut SecretStoreRuntime>,
    /// Cached per-processor to avoid cloning the manifest for every tool call.
    policy: crate::policy::PolicyEngine,
    session_id: Option<String>,
    turn_id: Option<String>,
    config: Option<&'a GatewayConfig>,
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    run_context: Option<crate::runtime::active_execution_registry::NativeToolRunContext>,
    session_state: autonoetic_types::agent::SessionState,
}

pub fn is_degraded_mode_tool_blocked(
    session_state: autonoetic_types::agent::SessionState,
    tool_name: &str,
) -> bool {
    if session_state != autonoetic_types::agent::SessionState::Degraded {
        return false;
    }
    matches!(tool_name, "sandbox_exec" | "artifact_exec")
}

fn strip_gemma_token_artifacts(s: &str) -> String {
    let re = regex::Regex::new(r"<\|[^>]*\|>").unwrap();
    re.replace_all(s, |caps: &regex::Captures| -> String {
        let token = &caps[0];
        match token {
            "<|\"|>" => "\"".to_string(),
            "<|'|>" => "'".to_string(),
            "<|_|>" => "_".to_string(),
            _ => token.to_string(),
        }
    })
    .to_string()
}

impl<'a> ToolCallProcessor<'a> {
    fn canonical_tool_name(name: &str) -> &str {
        match name {
            "spawn" => "agent_spawn",
            "message" => "agent_message",
            "search" => "web_search",
            "fetch" => "web_fetch",
            _ => name,
        }
    }

    pub fn new(
        mcp_runtime: &'a mut McpToolRuntime,
        registry: &'a NativeToolRegistry,
        manifest: &'a AgentManifest,
        disclosure_state: &'a mut DisclosureState,
        secret_store: Option<&'a mut SecretStoreRuntime>,
        config: Option<&'a GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> Self {
        Self {
            mcp_runtime,
            registry,
            manifest,
            disclosure_state,
            secret_store,
            policy: crate::policy::PolicyEngine::new(manifest.clone()),
            session_id: None,
            turn_id: None,
            config,
            gateway_store,
            run_context,
            session_state: autonoetic_types::agent::SessionState::Normal,
        }
    }

    pub fn with_session_context(
        mut self,
        session_id: Option<String>,
        turn_id: Option<String>,
    ) -> Self {
        self.session_id = session_id;
        self.turn_id = turn_id;
        self
    }

    pub fn with_session_state(mut self, state: autonoetic_types::agent::SessionState) -> Self {
        self.session_state = state;
        self
    }

    fn is_degraded_blocked_tool(&self, tool_name: &str) -> bool {
        is_degraded_mode_tool_blocked(self.session_state, tool_name)
    }

    /// Processes tool calls and returns `(had_any_success, results)`.
    /// `had_any_success` is `true` if at least one call completed successfully;
    /// the execution loop uses this to decide whether to reset the loop-guard counter.
    /// Recoverable errors are returned as structured error JSON in the result.
    /// Only fatal errors cause the entire operation to fail.
    pub async fn process_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
        tracer: &mut SessionTracer,
    ) -> anyhow::Result<(bool, Vec<(String, String, String)>)> {
        let mut results = Vec::with_capacity(tool_calls.len());
        let mut had_any_success = false;

        for tc in tool_calls {
            let started_at = Instant::now();
            let approval_ref = extract_approval_ref_from_args(&tc.arguments);
            let tool_name = Self::canonical_tool_name(&tc.name).to_string();

            if self.is_degraded_blocked_tool(&tool_name) {
                let tool_error = ToolError::permission(format!(
                    "session_degraded: tool '{}' blocked in degraded mode (P-7.18). \
                     CodeExecution is refused until operator clears degradation.",
                    tool_name
                ));
                let error_json = tool_error.to_json_string();
                let failure_event_id = self.log_tool_failure(tracer, tc, &tool_error)?;
                self.record_execution_trace(
                    tc,
                    &error_json,
                    started_at.elapsed(),
                    Some(&tool_error),
                    Some(failure_event_id),
                )?;
                results.push((tc.id.clone(), tc.name.clone(), error_json));
                continue;
            }

            let intent = match validate_tool_intent(&tool_name, &tc.arguments) {
                Ok(intent) => intent,
                Err(tool_error) => {
                    let error_json = tool_error.to_json_string();
                    let failure_event_id = self.log_tool_failure(tracer, tc, &tool_error)?;
                    self.record_execution_trace(
                        tc,
                        &error_json,
                        started_at.elapsed(),
                        Some(&tool_error),
                        Some(failure_event_id),
                    )?;
                    results.push((tc.id.clone(), tc.name.clone(), error_json));
                    continue;
                }
            };
            let canonical_tool = Self::canonical_tool_name(&tc.name);
            tracer.log_tool_requested(canonical_tool, &tc.arguments, intent.as_deref())?;

            // Execute tool call, handling errors appropriately
            let result = match self.execute_tool_call(tc, agent_dir, gateway_dir).await {
                Ok(res) => {
                    let res = normalize_tool_result_json(&res);
                    let event_id = tracer.log_tool_completed_with_approval(
                        canonical_tool,
                        &res,
                        Some(&tc.arguments),
                        approval_ref.as_deref(),
                    )?;
                    self.record_execution_trace(
                        tc,
                        &res,
                        started_at.elapsed(),
                        None,
                        Some(event_id.clone()),
                    )?;
                    self.record_operator_activity(tc, &res, Some(event_id));
                    self.log_memory_tool_event(tracer, &tc.name, &res);
                    had_any_success = true;
                    res
                }
                Err(e) => {
                    let tool_error = decorate_tool_error(e.into());
                    let error_json = tool_error.to_json_string();
                    let failure_event_id = self.log_tool_failure(tracer, tc, &tool_error)?;
                    if !tool_error.is_recoverable() {
                        self.record_execution_trace(
                            tc,
                            &error_json,
                            started_at.elapsed(),
                            Some(&tool_error),
                            Some(failure_event_id),
                        )?;
                        return Err(anyhow::anyhow!(
                            "Fatal tool error in {}: {}",
                            tc.name,
                            tool_error.message
                        ));
                    }
                    let event_id = tracer.log_tool_completed_with_approval(
                        canonical_tool,
                        &error_json,
                        Some(&tc.arguments),
                        approval_ref.as_deref(),
                    )?;
                    self.record_execution_trace(
                        tc,
                        &error_json,
                        started_at.elapsed(),
                        Some(&tool_error),
                        Some(event_id.clone()),
                    )?;
                    self.record_operator_activity(tc, &error_json, Some(event_id));
                    error_json
                }
            };

            results.push((tc.id.clone(), tc.name.clone(), result));

            if tool_result_requires_approval(&results.last().expect("just pushed result").2) {
                break;
            }
            if tool_result_requires_escalation(&results.last().expect("just pushed result").2) {
                break;
            }
        }

        Ok((had_any_success, results))
    }

    fn record_execution_trace(
        &self,
        tc: &ToolCall,
        result_json: &str,
        duration: Duration,
        tool_error: Option<&ToolError>,
        event_id: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(store) = &self.gateway_store else {
            return Ok(());
        };
        let session_id = self
            .session_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("session_id missing while recording execution trace"))?;

        let canonical_tool_name = Self::canonical_tool_name(&tc.name).to_string();
        let parsed_result = serde_json::from_str::<serde_json::Value>(result_json).ok();
        let success = infer_trace_success(parsed_result.as_ref(), tool_error);
        let error_type = infer_trace_error_type(parsed_result.as_ref(), tool_error);
        let error_summary = infer_trace_error_summary(parsed_result.as_ref(), tool_error);
        let command = infer_trace_command(&canonical_tool_name, &tc.arguments);
        let exit_code = parsed_result
            .as_ref()
            .and_then(|v| v.get("exit_code"))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);
        let stdout = parsed_result
            .as_ref()
            .and_then(|v| v.get("stdout"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let stderr = parsed_result
            .as_ref()
            .and_then(|v| v.get("stderr"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let approval_required = parsed_result
            .as_ref()
            .and_then(|v| v.get("approval_required"))
            .and_then(|v| v.as_bool())
            .map(|required| if required { 1 } else { 0 });
        let approval_request_id = parsed_result
            .as_ref()
            .and_then(|v| v.get("request_id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let trace = ExecutionTraceRecord {
            trace_id: uuid::Uuid::new_v4().to_string(),
            event_id,
            agent_id: self.manifest.agent.id.clone(),
            session_id: session_id.clone(),
            turn_id: self.turn_id.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: canonical_tool_name,
            command,
            exit_code,
            stdout,
            stderr,
            duration_ms: duration.as_millis().min(i64::MAX as u128) as i64,
            success: if success { 1 } else { 0 },
            error_type,
            error_summary,
            approval_required,
            approval_request_id,
            arguments: Some(tc.arguments.clone()),
            result: Some(result_json.to_string()),
        };
        store.create_execution_trace(&trace)
    }

    fn record_operator_activity(
        &self,
        tc: &ToolCall,
        result_json: &str,
        causal_event_id: Option<String>,
    ) {
        let Some(store) = &self.gateway_store else {
            return;
        };
        let Some(session_id) = &self.session_id else {
            return;
        };
        let canonical_tool_name = Self::canonical_tool_name(&tc.name).to_string();
        let Some(draft) = crate::runtime::operator_activity::classify_tool_activity(
            &canonical_tool_name,
            &tc.arguments,
            result_json,
        ) else {
            return;
        };

        let root_session_id = self
            .run_context
            .as_ref()
            .map(|c| c.root_session_id.clone())
            .unwrap_or_else(|| {
                crate::runtime::content_store::root_session_id(session_id).to_string()
            });
        let workflow_id = self.run_context.as_ref().and_then(|c| c.workflow_id.clone());
        let task_id = self.run_context.as_ref().and_then(|c| c.task_id.clone());

        let record = draft.into_record(
            root_session_id,
            session_id.clone(),
            self.manifest.agent.id.clone(),
            workflow_id,
            task_id,
            self.turn_id.clone(),
            Some(canonical_tool_name),
            causal_event_id,
            None,
        );
        let rate_limit_per_min = self
            .config
            .map(|c| c.operator_activity.rate_limit_per_min)
            .unwrap_or_else(|| {
                autonoetic_types::config::OperatorActivityConfig::default().rate_limit_per_min
            });
        match store.insert_operator_activity_throttled(&record, rate_limit_per_min) {
            Ok(crate::scheduler::gateway_store::OperatorActivityInsert::Dropped) => {
                tracing::debug!(
                    target: "operator_activity",
                    session_id = %session_id,
                    root_session_id = %record.root_session_id,
                    rate_limit_per_min,
                    "Operator activity dropped by per-root rate limit"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    target: "operator_activity",
                    error = %e,
                    session_id = %session_id,
                    "Failed to persist operator activity"
                );
            }
        }
    }

    async fn execute_tool_call(
        &mut self,
        tc: &ToolCall,
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
    ) -> anyhow::Result<String> {
        let tool_name = Self::canonical_tool_name(&tc.name);
        let policy = &self.policy;

        let sanitized_args = strip_gemma_token_artifacts(&tc.arguments);

        let mut result = if self.mcp_runtime.has_tool(tool_name) {
            let tool_policy = policy.can_invoke_tool(tool_name);
            if !tool_policy.is_allowed() {
                return Err(anyhow::Error::from(
                    autonoetic_types::tool_error::tagged::Tagged::permission_with_rules(
                        anyhow::anyhow!(
                            "Tool '{}' is not allowed by ToolInvoke capability",
                            tool_name
                        ),
                        tool_policy.into_rule_ids(),
                    ),
                ));
            }
            self.mcp_runtime
                .call_tool(tool_name, &sanitized_args)
                .await?
        } else if self.registry.has_tool(tool_name) {
            // #289 session-scoped read cache: for pure read tools, return a
            // memoized result instead of re-executing (and re-injecting the
            // same content into the transcript). Caching wraps only the raw
            // `registry.execute` output; disclosure + secret redaction below
            // still run on every hit, so this is transparent to those
            // invariants. The cache is consulted only when we have both a
            // session id and a gateway store to hold the per-session cache.
            let cache_ctx = match (self.session_id.as_deref(), self.gateway_store.as_ref()) {
                (Some(sid), Some(store)) => Some((sid.to_string(), store.clone())),
                _ => None,
            };

            if let Some((sid, store)) = cache_ctx.as_ref() {
                if let Some(hit) = store
                    .session_read_cache
                    .get(sid, tool_name, &sanitized_args)
                {
                    self.emit_cache_hit_event(store, tool_name);
                    hit
                } else {
                    let r = self.registry.execute(
                        tool_name,
                        self.manifest,
                        &policy,
                        agent_dir,
                        gateway_dir,
                        &sanitized_args,
                        self.session_id.as_deref(),
                        self.turn_id.as_deref(),
                        self.config,
                        self.gateway_store.clone(),
                        self.run_context.as_ref(),
                    )?;
                    self.maybe_cache_or_invalidate(store, sid, tool_name, &sanitized_args, &r);
                    r
                }
            } else {
                self.registry.execute(
                    tool_name,
                    self.manifest,
                    &policy,
                    agent_dir,
                    gateway_dir,
                    &sanitized_args,
                    self.session_id.as_deref(),
                    self.turn_id.as_deref(),
                    self.config,
                    self.gateway_store.clone(),
                    self.run_context.as_ref(),
                )?
            }
        } else {
            let agent_like_hint = if tc.name.contains('.') {
                " This looks like an agent ID, not a tool name. Use agent_spawn with {\"agent_id\": \"...\", \"message\": \"...\"}."
            } else {
                ""
            };
            return Err(anyhow::Error::from(
                 autonoetic_types::tool_error::tagged::Tagged::resource(anyhow::anyhow!(
                     "Unknown tool '{}'. Verify the tool name against the available tools list and retry with the correct name.{}",
                     tc.name,
                     agent_like_hint
                 )),
             ));
        };

        let tc_meta = self.registry.extract_metadata(tool_name, &tc.arguments);
        self.disclosure_state
            .register_result(tool_name, tc_meta.path.as_deref(), &result);

        if let Some(store) = &mut self.secret_store {
            let (new_result, extracted_secrets) = store.apply_and_redact(&result)?;
            result = new_result;
            for s in extracted_secrets {
                self.disclosure_state
                    .register_explicit_taint(&s, DisclosureClass::Restricted);
            }
        }

        Ok(result)
    }

    /// After a successful `registry.execute` for a tool with cache
    /// relevance: store cacheable read results, and invalidate the
    /// affected cache-tag class when the tool was a mutator (#289).
    fn maybe_cache_or_invalidate(
        &self,
        store: &std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
        session_id: &str,
        tool_name: &str,
        arguments_json: &str,
        result: &str,
    ) {
        use crate::runtime::session_read_cache::{invalidation_tag_for, read_cache_policy};

        // Only memoize / invalidate on a successful result — a failed read
        // must not be cached, and a failed mutation must not invalidate.
        if !crate::runtime::tool_dispatch::tool_result_counts_as_progress(result) {
            return;
        }

        if read_cache_policy(tool_name, arguments_json).is_some() {
            store
                .session_read_cache
                .put(session_id, tool_name, arguments_json, result);
        }

        if let Some(tag) = invalidation_tag_for(tool_name) {
            store.session_read_cache.invalidate_tag_all_sessions(tag);
        }
    }

    /// Emit a `tool_call.cache_hit` causal event so the audit chain still
    /// records the logical tool call when it was served from the #289
    /// read cache. Best-effort: a failed write is logged, not fatal.
    fn emit_cache_hit_event(
        &self,
        store: &std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
        tool_name: &str,
    ) {
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("cachehit-{}", uuid::Uuid::new_v4()),
            agent_id: self.manifest.agent.id.clone(),
            session_id: self.session_id.clone().unwrap_or_default(),
            turn_id: self.turn_id.clone(),
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "tool_call".to_string(),
            action: "cache_hit".to_string(),
            status: "SUCCESS".to_string(),
            enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
            target: Some(tool_name.to_string()),
            payload: None,
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        };
        if let Err(e) = store.create_causal_event(&event) {
            tracing::warn!(
                target: "session_read_cache",
                error = %e,
                tool = %tool_name,
                "failed to emit tool_call.cache_hit causal event"
            );
        }
    }

    fn log_tool_failure(
        &self,
        tracer: &mut SessionTracer,
        tc: &ToolCall,
        error: &ToolError,
    ) -> anyhow::Result<String> {
        let payload = serde_json::json!({
            "tool_name": tc.name,
            "tool_id": tc.id,
            "error_type": error.error_type,
            "message": error.message,
            "reason": error.message,
            "repair_hint": error.repair_hint,
            "enforced_rules": error.enforced_rules,
            "recoverable": error.is_recoverable(),
        });

        tracer.log_event(
            "tool",
            "failure",
            autonoetic_types::causal_chain::EntryStatus::Error,
            Some(payload),
        )
    }

    fn log_memory_tool_event(&self, tracer: &mut SessionTracer, tool_name: &str, result: &str) {
        let action = match tool_name {
            "memory_remember" => "remember",
            "memory_recall" => "recall",
            "memory_search" => "search",
            _ => return,
        };

        let parsed = match serde_json::from_str::<serde_json::Value>(result) {
            Ok(value) => value,
            Err(_) => return,
        };

        let payload = serde_json::json!({
            "tool_name": tool_name,
            "memory_id": parsed.get("memory_id").and_then(|v| v.as_str()),
            "scope": parsed.get("scope").and_then(|v| v.as_str()),
            "count": parsed.get("count").and_then(|v| v.as_u64()),
            "source_ref": parsed.get("source_ref").and_then(|v| v.as_str()),
            "visibility": parsed.get("visibility").cloned(),
        });

        let _ = tracer.log_event(
            "memory",
            action,
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(payload),
        );
    }
}

fn tool_result_requires_approval(result: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|parsed| parsed.get("approval_required").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

fn validate_tool_intent(
    tool_name: &str,
    arguments_json: &str,
) -> Result<Option<String>, ToolError> {
    let sanitized_args = strip_gemma_token_artifacts(arguments_json);
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&sanitized_args) else {
        return Ok(None);
    };

    let intent_value = parsed.get("intent");
    let requires_intent = crate::runtime::tools::tool_requires_intent(tool_name);

    let Some(intent_value) = intent_value else {
        if requires_intent {
            return Err(intent_required_error());
        }
        return Ok(None);
    };

    let Some(intent) = intent_value.as_str() else {
        return Err(ToolError::validation(
            "intent must be a string no longer than 500 characters",
            Some(
                "Set the top-level 'intent' field to a short natural-language reason for the call.",
            ),
        ));
    };

    if intent.trim().is_empty() {
        if requires_intent {
            return Err(intent_required_error());
        }
        return Err(ToolError::validation(
            "intent must not be empty when provided",
            Some("Either omit 'intent' for non-privileged tools or provide a short 1-2 sentence reason."),
        ));
    }

    if intent.chars().count() > 500 {
        return Err(ToolError::validation(
            "intent must be at most 500 characters",
            Some("Shorten the top-level 'intent' field to 1-2 concise sentences."),
        ));
    }

    Ok(Some(intent.to_string()))
}

fn intent_required_error() -> ToolError {
    ToolError::validation(
        "intent_required: privileged tool calls must include a non-empty top-level 'intent' field",
        Some("Add 'intent' as a short 1-2 sentence reason for invoking this privileged tool, then retry."),
    )
}

fn tool_result_requires_escalation(result: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|parsed| parsed.get("escalation_required").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

fn infer_trace_success(
    parsed_result: Option<&serde_json::Value>,
    tool_error: Option<&ToolError>,
) -> bool {
    if tool_error.is_some() {
        return false;
    }
    let Some(parsed) = parsed_result else {
        return true;
    };
    // Prefer the explicit command outcome when present. For exec tools
    // (`artifact_exec`) `ok` now means "the tool ran to completion" while
    // `command_succeeded` is the exit-0 signal — so the recorded trace
    // `success` (and operator-facing digests/overviews derived from it) must
    // reflect the command result, not merely that the sandbox executed.
    // (RFC: unit-test-runner-divergence-loop)
    if let Some(cs) = parsed.get("command_succeeded").and_then(|v| v.as_bool()) {
        return cs;
    }
    if let Some(ok) = parsed.get("ok").and_then(|v| v.as_bool()) {
        return ok;
    }
    if let Some(approval_required) = parsed.get("approval_required").and_then(|v| v.as_bool()) {
        return !approval_required;
    }
    if let Some(exit_code) = parsed.get("exit_code").and_then(|v| v.as_i64()) {
        return exit_code == 0;
    }
    true
}

fn normalize_error_type(raw: &str) -> String {
    match raw {
        "execution" | "fatal" => "runtime".to_string(),
        "quota" => "quota_exceeded".to_string(),
        _ => raw.to_string(),
    }
}

fn infer_trace_error_type(
    parsed_result: Option<&serde_json::Value>,
    tool_error: Option<&ToolError>,
) -> Option<String> {
    if let Some(err) = tool_error {
        let mapped = match err.error_type {
            ToolErrorType::Validation => "validation",
            ToolErrorType::Permission => "permission",
            ToolErrorType::Resource => "resource",
            ToolErrorType::Conflict => "conflict",
            ToolErrorType::QuotaExceeded => "quota_exceeded",
            ToolErrorType::NotFound => "not_found",
            ToolErrorType::Timeout => "timeout",
            ToolErrorType::SandboxUnavailable => "sandbox_unavailable",
            ToolErrorType::Execution | ToolErrorType::Fatal => "runtime",
        };
        return Some(mapped.to_string());
    }
    parsed_result
        .and_then(|v| v.get("error_type"))
        .and_then(|v| v.as_str())
        .map(normalize_error_type)
}

fn infer_trace_error_summary(
    parsed_result: Option<&serde_json::Value>,
    tool_error: Option<&ToolError>,
) -> Option<String> {
    if let Some(err) = tool_error {
        return Some(err.message.clone());
    }
    let Some(parsed) = parsed_result else {
        return None;
    };
    if let Some(summary) = parsed.get("error_summary").and_then(|v| v.as_str()) {
        return Some(summary.to_string());
    }
    if let Some(message) = parsed.get("message").and_then(|v| v.as_str()) {
        return Some(message.to_string());
    }
    if let Some(stderr) = parsed.get("stderr").and_then(|v| v.as_str()) {
        let first_line = stderr.lines().next().unwrap_or(stderr).trim();
        if !first_line.is_empty() {
            return Some(first_line.to_string());
        }
    }
    parsed
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .map(|code| format!("exit_code={code}"))
}

fn infer_trace_command(tool_name: &str, arguments_json: &str) -> Option<String> {
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments_json) {
        if let Some(command) = args.get("command").and_then(|v| v.as_str()) {
            return Some(command.to_string());
        }
        if let Some(command) = args.get("cmd").and_then(|v| v.as_str()) {
            return Some(command.to_string());
        }
        if tool_name == "sandbox_exec" {
            if let Some(script) = args.get("script").and_then(|v| v.as_str()) {
                return Some(script.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolDefinition;
    use crate::policy::PolicyEngine;
    use crate::runtime::tools::default_registry;
    use crate::runtime::tools::{NativeTool, NativeToolRegistry};
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};
    use autonoetic_types::capability::Capability;
    use autonoetic_types::tool_error::{tagged, ToolErrorType};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "test-agent".to_string(),
                name: "test-agent".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![
                Capability::ReadAccess {
                    scopes: vec!["*".to_string()],
                },
                Capability::WriteAccess {
                    scopes: vec!["*".to_string()],
                },
            ],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        }
    }

    #[tokio::test]
    async fn test_recoverable_error_returns_structured_json() {
        let temp = tempdir().unwrap();
        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let registry = default_registry();
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            None,
            None,
        );

        let tool_calls = vec![ToolCall {
            id: "tc1".to_string(),
            name: "knowledge_store".to_string(),
            arguments: r#"{"id":"","content":"hello"}"#.to_string(),
        }];

        let (_, result) = processor
            .process_tool_calls(
                &tool_calls,
                temp.path(),
                None,
                &mut SessionTracer::test_tracer(),
            )
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let (_, _, tool_result) = &result[0];

        // Should be a structured error JSON, not a panic
        let parsed: serde_json::Value = serde_json::from_str(tool_result).unwrap();
        assert_eq!(parsed.get("ok").unwrap(), false);
        // The error could be "resource" (unknown tool) or "validation" depending on tool availability
        assert!(
            parsed.get("error_type").unwrap().as_str().unwrap() == "resource"
                || parsed.get("error_type").unwrap().as_str().unwrap() == "validation"
        );
        assert!(
            parsed
                .get("message")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("must not be empty")
                || parsed
                    .get("message")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains("not found")
        );
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_recoverable_resource_error() {
        let temp = tempdir().unwrap();
        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let registry = default_registry();
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            None,
            None,
        );

        // Unknown tool is now recoverable so the agent can self-repair by
        // retrying with the correct tool name in the next turn.
        let tool_calls = vec![ToolCall {
            id: "tc1".to_string(),
            name: "unknown.tool".to_string(),
            arguments: "{}".to_string(),
        }];

        let (had_success, results) = processor
            .process_tool_calls(
                &tool_calls,
                temp.path(),
                None,
                &mut SessionTracer::test_tracer(),
            )
            .await
            .expect("unknown tool must not abort session");

        assert!(!had_success, "unknown tool must not count as success");
        assert_eq!(results.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&results[0].2).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error_type"], "resource");
    }

    #[tokio::test]
    async fn test_multiple_tool_calls_with_mixed_results() {
        let temp = tempdir().unwrap();
        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let registry = default_registry();
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            None,
            None,
        );

        // First call fails (validation), second would succeed if we had gateway_dir
        let tool_calls = vec![
            ToolCall {
                id: "tc1".to_string(),
                name: "knowledge_store".to_string(),
                arguments: r#"{"id":"","content":"hello"}"#.to_string(),
            },
            ToolCall {
                id: "tc2".to_string(),
                name: "knowledge_recall".to_string(),
                arguments: r#"{"id":"some-id"}"#.to_string(),
            },
        ];

        let (_, result) = processor
            .process_tool_calls(
                &tool_calls,
                temp.path(),
                None,
                &mut SessionTracer::test_tracer(),
            )
            .await
            .unwrap();

        // Both calls should complete (first with validation error, second with resource error for missing gateway)
        assert_eq!(result.len(), 2);

        // First is validation error for empty id or resource/execution error if tool not available
        let parsed1: serde_json::Value = serde_json::from_str(&result[0].2).unwrap();
        assert_eq!(parsed1.get("ok").unwrap(), false);
        let error_type1 = parsed1.get("error_type").unwrap().as_str().unwrap();
        assert!(
            error_type1 == "resource" || error_type1 == "validation" || error_type1 == "execution",
            "error_type1 was: {}",
            error_type1
        );

        // Second is execution/resource error for missing gateway_dir
        let parsed2: serde_json::Value = serde_json::from_str(&result[1].2).unwrap();
        assert_eq!(parsed2.get("ok").unwrap(), false);
        let error_type2 = parsed2.get("error_type").unwrap().as_str().unwrap();
        assert!(
            error_type2 == "resource" || error_type2 == "execution",
            "error_type2 was: {}",
            error_type2
        );
    }

    struct ApprovalRequiredTool;

    impl NativeTool for ApprovalRequiredTool {
        fn name(&self) -> &'static str {
            "test.approval"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "Returns approval required".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            }
        }

        fn is_available(&self, _manifest: &AgentManifest) -> bool {
            true
        }

        fn execute(
            &self,
            _manifest: &AgentManifest,
            _policy: &PolicyEngine,
            _agent_dir: &Path,
            _gateway_dir: Option<&Path>,
            _arguments_json: &str,
            _session_id: Option<&str>,
            _turn_id: Option<&str>,
            _config: Option<&autonoetic_types::config::GatewayConfig>,
            _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
            _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            Ok(serde_json::json!({
                "ok": false,
                "approval_required": true,
                "request_id": "apr-test1234"
            })
            .to_string())
        }
    }

    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    impl NativeTool for CountingTool {
        fn name(&self) -> &'static str {
            "test.count"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "Counts executions".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            }
        }

        fn is_available(&self, _manifest: &AgentManifest) -> bool {
            true
        }

        fn execute(
            &self,
            _manifest: &AgentManifest,
            _policy: &PolicyEngine,
            _agent_dir: &Path,
            _gateway_dir: Option<&Path>,
            _arguments_json: &str,
            _session_id: Option<&str>,
            _turn_id: Option<&str>,
            _config: Option<&autonoetic_types::config::GatewayConfig>,
            _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
            _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "ok": true }).to_string())
        }
    }

    struct TraceSuccessTool;

    impl NativeTool for TraceSuccessTool {
        fn name(&self) -> &'static str {
            "test.trace.success"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "Returns sandbox-style successful payload".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }),
            }
        }

        fn is_available(&self, _manifest: &AgentManifest) -> bool {
            true
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
            _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            let parsed: serde_json::Value = serde_json::from_str(arguments_json)?;
            let command = parsed
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            Ok(serde_json::json!({
                "ok": true,
                "exit_code": 0,
                "stdout": format!("ran: {command}"),
                "stderr": "",
            })
            .to_string())
        }
    }

    struct TraceFailureTool;

    impl NativeTool for TraceFailureTool {
        fn name(&self) -> &'static str {
            "test.trace.failure"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "Returns tagged execution failure".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }),
            }
        }

        fn is_available(&self, _manifest: &AgentManifest) -> bool {
            true
        }

        fn execute(
            &self,
            _manifest: &AgentManifest,
            _policy: &PolicyEngine,
            _agent_dir: &Path,
            _gateway_dir: Option<&Path>,
            _arguments_json: &str,
            _session_id: Option<&str>,
            _turn_id: Option<&str>,
            _config: Option<&autonoetic_types::config::GatewayConfig>,
            _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
            _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            Err(anyhow::Error::from(tagged::Tagged::execution(
                anyhow::anyhow!("command crashed"),
            )))
        }
    }

    #[tokio::test]
    async fn test_approval_required_stops_remaining_tool_calls() {
        let temp = tempdir().unwrap();
        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let mut registry = NativeToolRegistry::new();
        registry.register(Box::new(ApprovalRequiredTool));
        let counting_calls = Arc::new(AtomicUsize::new(0));
        registry.register(Box::new(CountingTool {
            calls: Arc::clone(&counting_calls),
        }));
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            None,
            None,
        );

        let tool_calls = vec![
            ToolCall {
                id: "tc1".to_string(),
                name: "test.approval".to_string(),
                arguments: "{}".to_string(),
            },
            ToolCall {
                id: "tc2".to_string(),
                name: "test.count".to_string(),
                arguments: "{}".to_string(),
            },
        ];

        let (had_success, results) = processor
            .process_tool_calls(
                &tool_calls,
                temp.path(),
                None,
                &mut SessionTracer::test_tracer(),
            )
            .await
            .unwrap();

        assert!(
            had_success,
            "approval-required tool result should still count as progress"
        );
        assert_eq!(results.len(), 1, "remaining tool calls should be skipped");
        assert_eq!(counting_calls.load(Ordering::SeqCst), 0);
        let parsed: serde_json::Value = serde_json::from_str(&results[0].2).unwrap();
        assert_eq!(parsed["approval_required"], true);
        assert_eq!(parsed["failure_class"], "approval_pending");
        assert_eq!(parsed["retry_advice"], "wait");
        assert_eq!(parsed["requires_external_event"], true);
        assert_eq!(parsed["requires_human"], true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_in_session_repair_loop_recovery_from_structured_error() {
        let temp = tempdir().unwrap();
        let gw_dir = temp.path().join("gateway");
        std::fs::create_dir_all(&gw_dir).unwrap();

        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let registry = default_registry();
        let mut disclosure_state = DisclosureState::default();

        let gateway_store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gw_dir).unwrap(),
        );
        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            Some(gateway_store),
            None,
        )
        .with_session_context(
            Some("repair-loop-session".to_string()),
            Some("turn-000001".to_string()),
        );

        // First turn: malformed tool call - empty id triggers validation error
        let tool_calls_turn1 = vec![ToolCall {
            id: "tc1".to_string(),
            name: "knowledge_store".to_string(),
            arguments: r#"{"id":"","content":"hello"}"#.to_string(),
        }];

        let (had_success_turn1, result_turn1) = processor
            .process_tool_calls(
                &tool_calls_turn1,
                temp.path(),
                Some(gw_dir.as_path()),
                &mut SessionTracer::test_tracer(),
            )
            .await
            .unwrap();

        assert_eq!(result_turn1.len(), 1);

        assert!(
            !had_success_turn1,
            "failed tool call must not count as success"
        );
        // Parse the error response - could be resource (unknown tool) or validation (empty id)
        let parsed_error: serde_json::Value = serde_json::from_str(&result_turn1[0].2).unwrap();
        assert_eq!(parsed_error.get("ok").unwrap(), false);
        let error_type = parsed_error.get("error_type").unwrap().as_str().unwrap();
        assert!(error_type == "validation" || error_type == "resource");
        if error_type == "validation" {
            assert!(parsed_error.get("repair_hint").is_some());
            // Extract the repair hint for the agent to use
            let repair_hint = parsed_error.get("repair_hint").unwrap().as_str().unwrap();
            assert!(repair_hint.contains("id") || repair_hint.contains("field"));
        }

        // Second turn: agent reads error, corrects the tool call with valid id
        let tool_calls_turn2 = vec![ToolCall {
            id: "tc2".to_string(),
            name: "knowledge_store".to_string(),
            arguments: r#"{"id":"valid-id-123","content":"hello world"}"#.to_string(),
        }];

        let (had_success_turn2, result_turn2) = processor
            .process_tool_calls(
                &tool_calls_turn2,
                temp.path(),
                Some(gw_dir.as_path()),
                &mut SessionTracer::test_tracer(),
            )
            .await
            .unwrap();

        assert_eq!(result_turn2.len(), 1);
        assert!(
            had_success_turn2,
            "successful tool call must set had_any_success"
        );

        // This time it should succeed
        let parsed_success: serde_json::Value = serde_json::from_str(&result_turn2[0].2).unwrap();
        assert_eq!(parsed_success.get("ok").unwrap(), true);
        // knowledge.store returns "id" field, not "memory_id"
        assert!(parsed_success.get("id").is_some() || parsed_success.get("memory_id").is_some());
    }

    #[tokio::test]
    async fn test_process_tool_calls_writes_execution_traces() {
        let temp = tempdir().unwrap();
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let mut registry = NativeToolRegistry::new();
        registry.register(Box::new(TraceSuccessTool));
        registry.register(Box::new(TraceFailureTool));
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .with_session_context(
            Some("trace-session".to_string()),
            Some("turn-000001".to_string()),
        );

        let tool_calls = vec![
            ToolCall {
                id: "tc1".to_string(),
                name: "test.trace.success".to_string(),
                arguments: r#"{"command":"echo hi"}"#.to_string(),
            },
            ToolCall {
                id: "tc2".to_string(),
                name: "test.trace.failure".to_string(),
                arguments: r#"{"command":"false"}"#.to_string(),
            },
        ];

        let (_had_success, _results) = processor
            .process_tool_calls(
                &tool_calls,
                temp.path(),
                Some(gateway_dir.as_path()),
                &mut SessionTracer::test_tracer(),
            )
            .await
            .unwrap();

        let traces = store
            .search_execution_traces(
                None,
                None,
                None,
                None,
                Some("test-agent"),
                Some("trace-session"),
                10,
            )
            .unwrap();
        assert_eq!(traces.len(), 2);
        for trace in &traces {
            assert_eq!(trace.session_id, "trace-session");
            assert_eq!(trace.turn_id.as_deref(), Some("turn-000001"));
        }

        let fail = traces
            .iter()
            .find(|t| t.tool_name == "test.trace.failure")
            .expect("failure trace should exist");
        assert_eq!(fail.success, 0);
        assert_eq!(fail.error_type.as_deref(), Some("runtime"));
        assert_eq!(fail.command.as_deref(), Some("false"));

        let success = traces
            .iter()
            .find(|t| t.tool_name == "test.trace.success")
            .expect("success trace should exist");
        assert_eq!(success.success, 1);
        assert_eq!(success.command.as_deref(), Some("echo hi"));
        assert_eq!(success.exit_code, Some(0));
        assert!(success
            .stdout
            .as_deref()
            .unwrap_or_default()
            .contains("echo hi"));
    }

    #[test]
    fn test_canonical_tool_name_aliases() {
        assert_eq!(
            ToolCallProcessor::canonical_tool_name("spawn"),
            "agent_spawn"
        );
        assert_eq!(
            ToolCallProcessor::canonical_tool_name("message"),
            "agent_message"
        );
        assert_eq!(
            ToolCallProcessor::canonical_tool_name("search"),
            "web_search"
        );
        assert_eq!(ToolCallProcessor::canonical_tool_name("fetch"), "web_fetch");
        assert_eq!(
            ToolCallProcessor::canonical_tool_name("agent_spawn"),
            "agent_spawn"
        );
        assert_eq!(
            ToolCallProcessor::canonical_tool_name("web_search"),
            "web_search"
        );
    }

    #[test]
    fn test_tagged_error_explicit_classification() {
        // Test that tagged::Tagged provides explicit classification
        let tagged = tagged::Tagged::validation(anyhow::anyhow!("some error"));
        let tool_error: ToolError = tagged.into();
        assert_eq!(tool_error.error_type, ToolErrorType::Validation);
        assert!(tool_error.is_recoverable());

        let tagged = tagged::Tagged::fatal(anyhow::anyhow!("corrupted state"));
        let tool_error: ToolError = tagged.into();
        assert_eq!(tool_error.error_type, ToolErrorType::Fatal);
        assert!(!tool_error.is_recoverable());

        let tagged = tagged::Tagged::permission(anyhow::anyhow!("access denied"));
        let tool_error: ToolError = tagged.into();
        assert_eq!(tool_error.error_type, ToolErrorType::Permission);
        assert!(tool_error.is_recoverable());

        let tagged = tagged::Tagged::resource(anyhow::anyhow!("file not found"));
        let tool_error: ToolError = tagged.into();
        assert_eq!(tool_error.error_type, ToolErrorType::Resource);
        assert!(tool_error.is_recoverable());

        let tagged = tagged::Tagged::execution(anyhow::anyhow!("unexpected result"));
        let tool_error: ToolError = tagged.into();
        assert_eq!(tool_error.error_type, ToolErrorType::Execution);
        assert!(tool_error.is_recoverable());
    }

    fn make_degraded_test_processor() -> ToolCallProcessor<'static> {
        let mut mcp_runtime = Box::leak(Box::new(crate::runtime::mcp::McpToolRuntime::empty()));
        let manifest = Box::leak(Box::new(test_manifest()));
        let registry = Box::leak(Box::new(default_registry()));
        let ds = Box::leak(Box::new(
            crate::runtime::disclosure::DisclosureState::default(),
        ));
        ToolCallProcessor::new(mcp_runtime, registry, manifest, ds, None, None, None, None)
    }

    #[test]
    fn degraded_mode_blocks_sandbox_exec() {
        let mut proc = make_degraded_test_processor();
        proc.session_state = autonoetic_types::agent::SessionState::Degraded;
        assert!(proc.is_degraded_blocked_tool("sandbox_exec"));
        assert!(proc.is_degraded_blocked_tool("artifact_exec"));
    }

    #[test]
    fn normal_mode_allows_sandbox_exec() {
        let proc = make_degraded_test_processor();
        assert!(!proc.is_degraded_blocked_tool("sandbox_exec"));
        assert!(!proc.is_degraded_blocked_tool("artifact_exec"));
    }

    #[test]
    fn degraded_mode_does_not_block_other_core_tools() {
        let mut proc = make_degraded_test_processor();
        proc.session_state = autonoetic_types::agent::SessionState::Degraded;
        assert!(!proc.is_degraded_blocked_tool("content_write"));
        assert!(!proc.is_degraded_blocked_tool("knowledge_store"));
        assert!(!proc.is_degraded_blocked_tool("artifact_build"));
    }

    // ── #289 session read-cache wiring ──────────────────────────────────

    /// A fake `resolve` that returns `{"ok":true,...}` and counts
    /// real executions, so a cache hit is observable as "counter did not
    /// increment".
    struct FakeResolve {
        calls: Arc<AtomicUsize>,
    }
    impl NativeTool for FakeResolve {
        fn name(&self) -> &'static str {
            "resolve"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "fake".to_string(),
                input_schema: serde_json::json!({"type":"object"}),
            }
        }
        fn is_available(&self, _m: &AgentManifest) -> bool {
            true
        }
        fn execute(
            &self,
            _m: &AgentManifest,
            _p: &PolicyEngine,
            _ad: &Path,
            _gd: Option<&Path>,
            _args: &str,
            _sid: Option<&str>,
            _tid: Option<&str>,
            _cfg: Option<&autonoetic_types::config::GatewayConfig>,
            _gs: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
            _rc: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "ok": true, "content": format!("bytes-{n}") }).to_string())
        }
    }

    /// A fake `agent_inspect` whose result counts executions.
    struct FakeAgentInspect {
        calls: Arc<AtomicUsize>,
    }
    impl NativeTool for FakeAgentInspect {
        fn name(&self) -> &'static str {
            "agent_inspect"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "fake".to_string(),
                input_schema: serde_json::json!({"type":"object"}),
            }
        }
        fn is_available(&self, _m: &AgentManifest) -> bool {
            true
        }
        fn execute(
            &self,
            _m: &AgentManifest,
            _p: &PolicyEngine,
            _ad: &Path,
            _gd: Option<&Path>,
            _args: &str,
            _sid: Option<&str>,
            _tid: Option<&str>,
            _cfg: Option<&autonoetic_types::config::GatewayConfig>,
            _gs: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
            _rc: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "ok": true, "exists": false }).to_string())
        }
    }

    /// A fake `agent_revision_promote` mutator (returns ok; the processor
    /// invalidates the AgentExistence cache class on success).
    struct FakePromote;
    impl NativeTool for FakePromote {
        fn name(&self) -> &'static str {
            "agent_revision_promote"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: "fake".to_string(),
                input_schema: serde_json::json!({"type":"object"}),
            }
        }
        fn is_available(&self, _m: &AgentManifest) -> bool {
            true
        }
        fn execute(
            &self,
            _m: &AgentManifest,
            _p: &PolicyEngine,
            _ad: &Path,
            _gd: Option<&Path>,
            _args: &str,
            _sid: Option<&str>,
            _tid: Option<&str>,
            _cfg: Option<&autonoetic_types::config::GatewayConfig>,
            _gs: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
            _rc: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
        ) -> anyhow::Result<String> {
            Ok(serde_json::json!({ "ok": true }).to_string())
        }
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_cache_serves_repeat_resolve_without_re_executing() {
        let temp = tempdir().unwrap();
        let gw_dir = temp.path().join("gateway");
        std::fs::create_dir_all(&gw_dir).unwrap();
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gw_dir).unwrap(),
        );

        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let mut registry = NativeToolRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        registry.register(Box::new(FakeResolve {
            calls: Arc::clone(&calls),
        }));
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .with_session_context(Some("cache-sess".to_string()), Some("t1".to_string()));

        let args = r#"{"name":"f"}"#;
        // First call executes for real.
        let (_ok1, r1) = processor
            .process_tool_calls(&[call("tc1", "resolve", args)], temp.path(), None, &mut SessionTracer::test_tracer())
            .await
            .unwrap();
        // Second identical call must be served from cache (no re-exec).
        let (_ok2, r2) = processor
            .process_tool_calls(&[call("tc2", "resolve", args)], temp.path(), None, &mut SessionTracer::test_tracer())
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "second read must hit cache, not re-execute");
        assert_eq!(r1[0].2, r2[0].2, "cached result must be byte-identical");

        // Audit: a tool_call.cache_hit causal event was recorded.
        let events = store.search_causal_events(Some("cache-sess"), None, 50).unwrap();
        assert!(
            events.iter().any(|e| e.category == "tool_call" && e.action == "cache_hit"),
            "expected a tool_call.cache_hit causal event; got {:?}",
            events.iter().map(|e| format!("{}.{}", e.category, e.action)).collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mutator_invalidates_agent_inspect_cache() {
        let temp = tempdir().unwrap();
        let gw_dir = temp.path().join("gateway");
        std::fs::create_dir_all(&gw_dir).unwrap();
        let store = Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gw_dir).unwrap(),
        );

        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let mut registry = NativeToolRegistry::new();
        let exists_calls = Arc::new(AtomicUsize::new(0));
        registry.register(Box::new(FakeAgentInspect {
            calls: Arc::clone(&exists_calls),
        }));
        registry.register(Box::new(FakePromote));
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .with_session_context(Some("inv-sess".to_string()), Some("t1".to_string()));

        let ea = r#"{"agent_id":"x"}"#;
        // inspect #1 (real), inspect #2 (cache hit) → 1 execution.
        processor.process_tool_calls(&[call("a1", "agent_inspect", ea)], temp.path(), None, &mut SessionTracer::test_tracer()).await.unwrap();
        processor.process_tool_calls(&[call("a2", "agent_inspect", ea)], temp.path(), None, &mut SessionTracer::test_tracer()).await.unwrap();
        assert_eq!(exists_calls.load(Ordering::SeqCst), 1, "second agent_inspect should be cached");

        // A promote invalidates the AgentExistence class. `agent_revision_*`
        // is intent-gated, so a real intent must be supplied for the tool to
        // actually execute (and thus invalidate) — a missing intent would be
        // rejected pre-execution and must NOT invalidate.
        processor.process_tool_calls(&[call("p1", "agent_revision_promote", r#"{"intent":"promote x for invalidation test"}"#)], temp.path(), None, &mut SessionTracer::test_tracer()).await.unwrap();

        // inspect #3 must re-execute (cache invalidated) → 2 executions.
        processor.process_tool_calls(&[call("a3", "agent_inspect", ea)], temp.path(), None, &mut SessionTracer::test_tracer()).await.unwrap();
        assert_eq!(exists_calls.load(Ordering::SeqCst), 2, "agent_inspect after promote must re-execute");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_cache_noop_without_gateway_store() {
        // No gateway store → no cache; the tool executes every time.
        let temp = tempdir().unwrap();
        let manifest = test_manifest();
        let mut mcp_runtime = crate::runtime::mcp::McpToolRuntime::empty();
        let mut registry = NativeToolRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        registry.register(Box::new(FakeResolve {
            calls: Arc::clone(&calls),
        }));
        let mut disclosure_state = DisclosureState::default();

        let mut processor = ToolCallProcessor::new(
            &mut mcp_runtime,
            &registry,
            &manifest,
            &mut disclosure_state,
            None,
            None,
            None,
            None,
        )
        .with_session_context(Some("no-store".to_string()), Some("t1".to_string()));

        let args = r#"{"name":"f"}"#;
        processor.process_tool_calls(&[call("c1", "resolve", args)], temp.path(), None, &mut SessionTracer::test_tracer()).await.unwrap();
        processor.process_tool_calls(&[call("c2", "resolve", args)], temp.path(), None, &mut SessionTracer::test_tracer()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "no cache without a gateway store");
    }
}

fn extract_approval_ref_from_args(arguments_json: &str) -> Option<String> {
    let Ok(v) = serde_json::from_str::<Value>(arguments_json) else {
        return None;
    };
    v.get("approval_ref")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            v.get("promotion_gate")
                .and_then(|g| g.get("install_approval_ref"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}
