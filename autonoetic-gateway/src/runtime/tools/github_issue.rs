use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::NativeTool;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(GithubIssueCreateTool));
}

#[derive(Debug, Deserialize)]
struct GithubIssueCreateArgs {
    title: String,
    body: String,
    #[serde(default)]
    labels: Option<String>,
    /// Target repo in "owner/repo" format — required for policy scoping.
    repo: String,
}

pub struct GithubIssueCreateTool;

impl NativeTool for GithubIssueCreateTool {
    fn name(&self) -> &'static str {
        "github.issue.create"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::GithubIssueCreate { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Create a GitHub issue. Requires `gh` CLI to be installed and authenticated. \
                          The code-issue-proposer agent uses this to file scoped code-level issues \
                          from failed sessions."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Issue title"
                    },
                    "body": {
                        "type": "string",
                        "description": "Issue body in Markdown"
                    },
                    "labels": {
                        "type": "string",
                        "description": "Comma-separated labels (optional)"
                    },
                    "repo": {
                        "type": "string",
                        "description": "Target repo in \"owner/repo\" format"
                    }
                },
                "required": ["title", "body", "repo"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: GithubIssueCreateArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let decision = policy.can_create_github_issue(&args.repo);
        if !decision.is_allowed() {
            return Err(anyhow::anyhow!(
                "GithubIssueCreate capability denied for repo '{}': missing required capability",
                args.repo
            ));
        }

        let mut cmd = std::process::Command::new("gh");
        cmd.arg("issue").arg("create");
        cmd.arg("--title").arg(&args.title);
        cmd.arg("--body").arg(&args.body);
        cmd.arg("--repo").arg(&args.repo);

        if let Some(labels) = &args.labels {
            if !labels.trim().is_empty() {
                cmd.arg("--label").arg(labels);
            }
        }

        let output = cmd
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute `gh issue create`: {} — is `gh` installed?", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "`gh issue create` failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(serde_json::json!({
            "ok": true,
            "url": stdout,
        })
        .to_string())
    }
}
