use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::egress_stored::{
    filter_or_indicate_for_sink, query_sink_or_remote, resolve_stored_label, FilteredStoredContent,
};
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::egress::{EgressConfig, IndicationVerbosity, Sink};
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ExecutionSearchTool));
}

fn filter_opt_field(
    value: Option<&str>,
    label: &autonoetic_types::egress::EgressLabel,
    sink: Sink,
    kind: &str,
) -> Option<String> {
    let Some(raw) = value else {
        return None;
    };
    match filter_or_indicate_for_sink(
        raw,
        label,
        sink,
        Some(kind),
        IndicationVerbosity::Descriptive,
    ) {
        FilteredStoredContent::Allowed(c) => Some(c),
        FilteredStoredContent::Withheld { indication } => Some(indication),
    }
}

/// Is `requested` the caller's root session or a session nested under it?
///
/// The store's `session_branch` filter has the same shape ("exact match or
/// `id/<suffix>`"), so a request that passes this check can only ever widen to
/// sessions inside the caller's own root.
fn session_within_root(requested: &str, root: &str) -> bool {
    requested == root || requested.starts_with(&format!("{root}/"))
}

/// The caller's root session — the ownership boundary `execution_search` scopes
/// to (#1062).
///
/// Prefers the run context (authoritative, set by the executor); falls back to
/// deriving the root from the current session id. `None` means the caller's
/// identity could not be established at all, which the tool treats as a refusal
/// rather than a licence to search every session in the store.
fn caller_root_session(
    run_context: Option<&NativeToolRunContext>,
    session_id: Option<&str>,
) -> Option<String> {
    run_context
        .map(|rc| rc.root_session_id.clone())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            session_id
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| crate::runtime::content_store::root_session_id(s).to_string())
        })
}

pub struct ExecutionSearchTool;

impl NativeTool for ExecutionSearchTool {
    fn name(&self) -> &'static str {
        "execution_search"
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search raw execution traces for tool-level debugging within sessions. Query by tool name, success status, error type, command pattern, or agent ID. Returns execution metadata including exit codes, duration, and error info. For cross-session discovery of high-level session summaries, use observability_search instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "Filter by tool name (e.g., 'sandbox.exec'). Optional."
                    },
                    "success": {
                        "type": "boolean",
                        "description": "Filter by success (true), failure (false), or both (null). Optional."
                    },
                    "error_type": {
                        "type": "string",
                        "enum": ["compilation", "runtime", "permission", "timeout", "validation", "resource", "conflict", "quota_exceeded", "not_found", "sandbox_unavailable"],
                        "description": "Filter by error type. Optional."
                    },
                    "command_pattern": {
                        "type": "string",
                        "description": "Filter by command pattern (SQL LIKE). Optional."
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "Filter by agent ID. Optional."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Narrow to this session id and its nested sessions (exact match or id/<suffix>). Optional; defaults to your own root session. Must be your root session or one nested under it — other roots are not searchable."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 10)."
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
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            tool_name: Option<String>,
            #[serde(default)]
            success: Option<bool>,
            #[serde(default)]
            error_type: Option<String>,
            #[serde(default)]
            command_pattern: Option<String>,
            #[serde(default)]
            agent_id: Option<String>,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            limit: Option<i64>,
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(ToolError::resource("execution_search requires GatewayStore to be configured", None::<String>).to_error_response());
        };

        let limit = args.limit.unwrap_or(10).min(100) as i64;

        // Ownership scope (#1062). `session_id` used to be a voluntary filter:
        // omitting it searched every session in the store — cross-root,
        // cross-operator, historical — and returned other sessions' raw
        // `stdout` verbatim. The caller's root session is the trust domain
        // (#1061), so it is the default and the ceiling: an explicit
        // `session_id` may only narrow within it.
        //
        // No establishable caller ⇒ refuse. This tool is always-available
        // (`is_available` returns `true` for every manifest, and it is on the
        // clarification/degraded allowlists), so an unscoped fallback here
        // would be reachable from every session in the system.
        let Some(caller_root) = caller_root_session(run_context, session_id) else {
            return Ok(ToolError::permission(
                "execution_search requires a session context: the caller's root session is the search scope",
            )
            .to_error_response());
        };
        let scope = match args.session_id.as_deref() {
            None => caller_root.clone(),
            Some(requested) if session_within_root(requested, &caller_root) => {
                requested.to_string()
            }
            Some(requested) => {
                return Ok(ToolError::permission(format!(
                    "execution_search is scoped to your root session '{caller_root}'; \
                     '{requested}' is outside it"
                ))
                .to_error_response());
            }
        };

        let cfg: EgressConfig = config.map(|c| c.egress.clone()).unwrap_or_default();
        let sink = query_sink_or_remote(run_context.and_then(|rc| rc.egress_query_sink));

        let traces = store.search_execution_traces(
            args.tool_name.as_deref(),
            args.success,
            args.error_type.as_deref(),
            args.command_pattern.as_deref(),
            args.agent_id.as_deref(),
            Some(scope.as_str()),
            limit,
        )?;

        let items: Vec<serde_json::Value> = traces
            .into_iter()
            .map(|mut t| {
                // RFC data-envelopes §6: stored-content query surfaces are gated by
                // egress label × query sink — not by ViewerClass redaction. The
                // filtered fields below are what the caller's sink may see.
                // `error_summary` is derived from tool output (stderr first line,
                // error message) so it is gated like the content fields. The
                // indication kind is the tool name, matching the other
                // stored-content surfaces (`knowledge_recall`, `wiki_get`).
                let label = resolve_stored_label(t.egress_label.as_ref(), &cfg);
                t.command = filter_opt_field(t.command.as_deref(), &label, sink, "execution_search");
                t.stdout = filter_opt_field(t.stdout.as_deref(), &label, sink, "execution_search");
                t.stderr = filter_opt_field(t.stderr.as_deref(), &label, sink, "execution_search");
                t.error_summary = filter_opt_field(t.error_summary.as_deref(), &label, sink, "execution_search");
                t.egress_label = Some(label);
                serde_json::json!({
                    "trace_id": t.trace_id,
                    "agent_id": t.agent_id,
                    "session_id": t.session_id,
                    "turn_id": t.turn_id,
                    "timestamp": t.timestamp,
                    "tool_name": t.tool_name,
                    "command": t.command,
                    "exit_code": t.exit_code,
                    "stdout": t.stdout,
                    "stderr": t.stderr,
                    "duration_ms": t.duration_ms,
                    "success": t.success == 1,
                    "error_type": t.error_type,
                    "error_summary": t.error_summary,
                    "approval_required": t.approval_required == Some(1),
                    "approval_request_id": t.approval_request_id,
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "results": items,
            "count": items.len(),
            // Echo what was actually searched: with the scope defaulted rather
            // than supplied, an empty result set otherwise reads as "no such
            // trace" when it means "not in your root session".
            "session_scope": scope,
        }))
        .map_err(Into::into)
    }
}
