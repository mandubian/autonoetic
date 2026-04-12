use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ObservabilitySearchTool));
    registry.register(Box::new(ObservabilityReadTool));
}

pub struct ObservabilitySearchTool;

impl NativeTool for ObservabilitySearchTool {
    fn name(&self) -> &'static str {
        "observability.search"
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
            description: "Discover observability resources across sessions. Searches published session reports by text. Returns matching reports with URIs and summaries. Unlike execution.search (which searches raw tool execution traces within a session), observability.search finds high-level session summaries and reports across sessions. Use this to discover what happened in other sessions, then use observability.read to drill into details.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search text matched against published report summaries, agent IDs, and metadata."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default: 20, max: 100).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": ["query"],
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
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            query: String,
            #[serde(default = "default_limit")]
            limit: i64,
        }

        fn default_limit() -> i64 {
            20
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            anyhow::bail!("observability.search requires GatewayStore to be configured");
        };

        let limit = args.limit.clamp(1, 100);
        let reports = store.search_published_reports(&args.query, limit)?;

        let items: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                let root = &r.root_session_id;
                serde_json::json!({
                    "uri": format!("autonoetic://observability/roots/{}/report", root),
                    "resource_type": "report",
                    "root_session_id": root,
                    "title": r.title,
                    "status": r.status,
                    "agent_count": r.agent_count,
                    "error_count": r.error_count,
                    "approval_count": r.approval_count,
                    "started_at": r.started_at,
                    "ended_at": r.ended_at,
                    "links": {
                        "self": format!("autonoetic://observability/roots/{}/report", root),
                        "overview": format!("autonoetic://observability/roots/{}/report/overview", root),
                    }
                })
            })
            .collect();

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "query": args.query,
            "results": items,
            "count": items.len(),
        }))
        .map_err(Into::into)
    }
}

pub struct ObservabilityReadTool;

impl NativeTool for ObservabilityReadTool {
    fn name(&self) -> &'static str {
        "observability.read"
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
            description: "Read an observability resource by URI. Fetches published session report metadata and sub-resource summaries. The view parameter controls depth: 'metadata' returns structure only, 'summary' returns compact body (default), 'full' returns complete detail. Published reports are sanitized — input/output previews, tool details, and approval reasons are stripped before storage. Unlike execution.search (raw tool traces), this returns high-level session reports suitable for cross-session learning.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "uri": {
                        "type": "string",
                        "description": "Observability URI, e.g. 'autonoetic://observability/roots/<root>/report' or a sub-resource path."
                    },
                    "view": {
                        "type": "string",
                        "enum": ["metadata", "summary", "full"],
                        "description": "Depth of response: 'metadata' (structure/links only), 'summary' (compact body, default), 'full' (complete detail)."
                    },
                },
                "required": ["uri"],
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
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            uri: String,
            #[serde(default = "default_view")]
            view: String,
        }

        fn default_view() -> String {
            "summary".to_string()
        }

        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            anyhow::bail!("observability.read requires GatewayStore to be configured");
        };

        let root_session_id = match parse_root_from_uri(&args.uri) {
            Some(root) => root,
            None => anyhow::bail!(
                "Invalid observability URI. Expected format: autonoetic://observability/roots/<root>/..."
            ),
        };

        let sub_path = parse_sub_path(&args.uri);

        let Some(report) = store.find_published_report(&root_session_id)? else {
            return serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": format!("No published report found for root session '{}'", root_session_id),
            }))
            .map_err(Into::into);
        };

        match sub_path.as_deref() {
            None | Some("report") | Some("report/") => {
                build_report_response(&root_session_id, &report, &args.view)
            }
            Some("report/overview") | Some("report/overview/") => {
                build_overview_response(&root_session_id, &report)
            }
            Some("report/agents") | Some("report/agents/") => {
                build_agents_list_response(&root_session_id, &report)
            }
            _ => serde_json::to_string(&serde_json::json!({
                "ok": false,
                "error": format!("Unknown observability sub-path: {}", args.uri),
                "hint": "Available: /report, /report/overview, /report/agents",
            }))
            .map_err(Into::into),
        }
    }
}

fn parse_root_from_uri(uri: &str) -> Option<String> {
    let stripped = uri.strip_prefix("autonoetic://observability/roots/")?;
    let slash = stripped.find('/').unwrap_or(stripped.len());
    Some(stripped[..slash].to_string())
}

fn parse_sub_path(uri: &str) -> Option<String> {
    let stripped = uri.strip_prefix("autonoetic://observability/roots/")?;
    let slash = stripped.find('/')?;
    Some(stripped[slash + 1..].to_string())
}

fn build_report_response(
    root: &str,
    report: &autonoetic_types::causal_chain::PublishedSessionReportRecord,
    view: &str,
) -> anyhow::Result<String> {
    let base = serde_json::json!({
        "ok": true,
        "canonical_uri": format!("autonoetic://observability/roots/{}/report", root),
        "resource_type": "report",
        "root_session_id": root,
        "title": report.title,
        "status": report.status,
        "links": {
            "self": format!("autonoetic://observability/roots/{}/report", root),
            "overview": format!("autonoetic://observability/roots/{}/report/overview", root),
            "agents": format!("autonoetic://observability/roots/{}/report/agents", root),
        },
    });

    match view {
        "metadata" => serde_json::to_string(&base).map_err(Into::into),
        "full" | _ => {
            let mut full = base;
            full["body"] = serde_json::json!({
                "root_session_id": report.root_session_id,
                "title": report.title,
                "status": report.status,
                "started_at": report.started_at,
                "ended_at": report.ended_at,
                "agent_count": report.agent_count,
                "error_count": report.error_count,
                "approval_count": report.approval_count,
                "generated_at": report.generated_at,
                "report_version": report.report_version,
            });
            serde_json::to_string(&full).map_err(Into::into)
        }
    }
}

fn build_overview_response(
    root: &str,
    report: &autonoetic_types::causal_chain::PublishedSessionReportRecord,
) -> anyhow::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "ok": true,
        "canonical_uri": format!("autonoetic://observability/roots/{}/report/overview", root),
        "resource_type": "report_overview",
        "body": {
            "root_session_id": root,
            "title": report.title,
            "status": report.status,
            "agent_count": report.agent_count,
            "error_count": report.error_count,
            "approval_count": report.approval_count,
            "started_at": report.started_at,
            "ended_at": report.ended_at,
        },
        "links": {
            "self": format!("autonoetic://observability/roots/{}/report/overview", root),
            "parent": format!("autonoetic://observability/roots/{}/report", root),
        }
    }))
    .map_err(Into::into)
}

fn build_agents_list_response(
    root: &str,
    report: &autonoetic_types::causal_chain::PublishedSessionReportRecord,
) -> anyhow::Result<String> {
    serde_json::to_string(&serde_json::json!({
        "ok": true,
        "canonical_uri": format!("autonoetic://observability/roots/{}/report/agents", root),
        "resource_type": "report_agents_collection",
        "agent_count": report.agent_count,
        "links": {
            "self": format!("autonoetic://observability/roots/{}/report/agents", root),
            "parent": format!("autonoetic://observability/roots/{}/report", root),
        }
    }))
    .map_err(Into::into)
}
