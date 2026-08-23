use crate::agent::repository::AgentRepository;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use crate::scheduler::cron_parser;
use autonoetic_types::agent::{AgentManifest, ExecutionMode};
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SchedulerCronCreateTool));
    registry.register(Box::new(SchedulerCronListTool));
    registry.register(Box::new(SchedulerCronPauseTool));
    registry.register(Box::new(SchedulerCronResumeTool));
    registry.register(Box::new(SchedulerCronCancelTool));
}

#[derive(Deserialize)]
struct CreateArgs {
    message: String,
    schedule_expr: String,
    target_agent_id: Option<String>,
    metadata: Option<serde_json::Value>,
}

pub struct SchedulerCronCreateTool;

impl NativeTool for SchedulerCronCreateTool {
    fn name(&self) -> &'static str {
        "scheduler_cron_create"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Create a new scheduled (cron) job that will trigger a workflow task at specified intervals. The job is always owned by the calling agent. The target agent is resolved and pinned to a revision at creation time. All schedules are evaluated in UTC.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The message/prompt to send to the target agent when the job triggers"
                    },
                    "schedule_expr": {
                        "type": "string",
                        "description": "Cron expression (5 fields) or natural-language schedule phrase (e.g., 'every 10 seconds', 'every 5 minutes', 'every day at 09:00', 'every monday at 14:30')"
                    },
                    "target_agent_id": {
                        "type": "string",
                        "description": "The agent ID to trigger when the job fires. Defaults to the calling agent_id if not specified. The agent is resolved and pinned to its current revision at creation time."
                    },
                    "metadata": {
                        "type": "object",
                        "description": "Optional metadata to attach to the scheduled job"
                    }
                },
                "required": ["message", "schedule_expr"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        let policy = PolicyEngine::new(manifest.clone());
        policy.can_schedule("scheduler_cron_create").is_allowed()
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: CreateArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let decision = policy.can_schedule("scheduler_cron_create");
        if !decision.is_allowed() {
            return Ok(ToolError::permission(
                "Missing SchedulerAccess capability for scheduler.cron.create",
            )
            .with_enforced_rules(
                decision
                    .enforced_rules
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )
            .to_error_response());
        }

        let cron = cron_parser::parse_schedule(&args.schedule_expr)
            .map_err(|e| anyhow::anyhow!("Invalid schedule expression: {}", e))?;

        let cfg = config.cloned().unwrap_or_default();
        let min_interval = cfg.scheduled_jobs.min_interval_secs;

        let now = chrono::Utc::now();
        let next1 = cron_parser::next_occurrence(&cron, now)
            .ok_or_else(|| anyhow::anyhow!("No future occurrence found for schedule"))?;
        let next2 = cron_parser::next_occurrence(&cron, next1)
            .ok_or_else(|| anyhow::anyhow!("Cannot compute second occurrence for schedule"))?;
        let interval_secs = (next2 - next1).num_seconds() as u64;
        if interval_secs < min_interval {
            return Ok(ToolError::validation(
                format!(
                    "Schedule interval ({}s) is below the minimum allowed ({}s)",
                    interval_secs, min_interval
                ),
                Some("Use a less frequent schedule.".to_string()),
            )
            .to_error_response());
        }

        let job_count = store
            .list_scheduled_jobs_for_root(session_id.unwrap_or("default"))?
            .len();
        if job_count >= cfg.scheduled_jobs.max_per_root {
            return Ok(ToolError::quota_exceeded(
                format!(
                    "Maximum scheduled jobs per root ({}) reached",
                    cfg.scheduled_jobs.max_per_root
                ),
                Some("Cancel existing jobs before creating new ones.".to_string()),
            )
            .to_error_response());
        }

        let target = args
            .target_agent_id
            .clone()
            .unwrap_or_else(|| manifest.agent.id.clone());

        // Fast-path guardrail before target resolution:
        // if we can already determine the target is reasoning-mode, reject sub-10s
        // schedules with a clear error even when alias resolution would fail later.
        if interval_secs < 10 {
            let target_is_script_hint = if target == manifest.agent.id {
                Some(matches!(manifest.execution_mode, ExecutionMode::Script))
            } else {
                let repo = AgentRepository::from_config(&cfg);
                // Derive from config when the engine passed no gateway_dir.
                // `unwrap_or_default()` would yield an empty path, and since this
                // check is now fail-closed (#1136) that would reject a legitimately
                // promoted script agent rather than merely losing a fallback.
                let gateway_dir = gateway_dir
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| crate::execution::gateway_root_dir(&cfg));
                // Promoted revision only (#1136). `None` here means "can't
                // tell yet" and defers to the definitive check below after
                // alias resolution; it must never mean "ask the ungated
                // agents_dir copy", which would let an unvetted manifest
                // answer the guardrail.
                repo.get_sync_from_store(&target, &gateway_dir, Some(store.as_ref()))
                    .ok()
                    .map(|loaded| matches!(loaded.manifest.execution_mode, ExecutionMode::Script))
            };
            if target_is_script_hint == Some(false) {
                return Ok(ToolError::validation(
                    "Sub-10s schedules are only allowed for script-mode agents (execution_mode=script)",
                    Some("Use >=10s intervals for reasoning agents.".to_string()),
                ).to_error_response());
            }
        }

        // Resolve target to a pinned revision ref at creation time.
        let agent_ref =
            match crate::runtime::tools::resolve_target_to_agent_ref(&target, store.as_ref()) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolError::not_found(
                        format!("target agent '{}'", target),
                        Some(format!("Ensure the agent exists and is promoted. {}", e)),
                    )
                    .to_error_response());
                }
            };

        // Guardrail: sub-10s schedules are only allowed for script-mode targets.
        if interval_secs < 10 {
            let target_is_script = if agent_ref.agent_id == manifest.agent.id {
                matches!(manifest.execution_mode, ExecutionMode::Script)
            } else {
                let repo = AgentRepository::from_config(&cfg);
                // Derive from config when the engine passed no gateway_dir.
                // `unwrap_or_default()` would yield an empty path, and since this
                // check is now fail-closed (#1136) that would reject a legitimately
                // promoted script agent rather than merely losing a fallback.
                let gateway_dir = gateway_dir
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| crate::execution::gateway_root_dir(&cfg));
                // Promoted revision only — a failed lookup means "not
                // promoted", which fails closed into the `not_found` branch
                // below rather than consulting the ungated ingest dir (#1136).
                let loaded =
                    repo.get_sync_from_store(&agent_ref.agent_id, &gateway_dir, Some(store.as_ref()));
                match loaded {
                    Ok(loaded) => matches!(loaded.manifest.execution_mode, ExecutionMode::Script),
                    Err(_) => {
                        return Ok(ToolError::not_found(
                            format!("script-mode target agent '{}'", agent_ref.agent_id),
                            Some(
                                "Sub-10s schedules require an existing script-mode target agent."
                                    .to_string(),
                            ),
                        )
                        .to_error_response());
                    }
                }
            };

            if !target_is_script {
                return Ok(ToolError::validation(
                    "Sub-10s schedules are only allowed for script-mode agents (execution_mode=script)",
                    Some("Use >=10s intervals for reasoning agents.".to_string()),
                ).to_error_response());
            }
        }

        let now = chrono::Utc::now();
        let next_run = cron_parser::next_occurrence(&cron, now)
            .ok_or_else(|| anyhow::anyhow!("No future occurrence found for schedule"))?;

        let metadata_json = args
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()?;

        let job = ScheduledJob {
            job_id: format!("sj-{}", uuid::Uuid::new_v4()),
            owner_agent_id: manifest.agent.id.clone(),
            root_session_id: session_id.unwrap_or("default").to_string(),
            target_agent_id: agent_ref.agent_id.clone(),
            target_revision_id: agent_ref.revision_id.clone(),
            message: args.message.clone(),
            metadata_json,
            cron_expr: cron.to_string(),
            timezone: "UTC".to_string(),
            next_run_at: next_run.to_rfc3339(),
            last_run_at: None,
            status: ScheduledJobStatus::Active,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            last_error: None,
            generation: 0,
        };

        store.create_scheduled_job(&job)?;

        let response = serde_json::json!({
            "ok": true,
            "job_id": job.job_id,
            "normalized_cron_expr": job.cron_expr,
            "timezone": job.timezone,
            "next_run_at": job.next_run_at,
            "status": "active"
        });

        serde_json::to_string(&response).map_err(Into::into)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

#[derive(Deserialize)]
struct ListArgs {
    status: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub struct SchedulerCronListTool;

impl NativeTool for SchedulerCronListTool {
    fn name(&self) -> &'static str {
        "scheduler_cron_list"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "List scheduled cron jobs owned by the calling agent.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["active", "paused", "cancelled"],
                        "description": "Filter by job status"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 100)"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Pagination offset"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        let policy = PolicyEngine::new(manifest.clone());
        policy.can_schedule("scheduler_cron_list").is_allowed()
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: ListArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let decision = policy.can_schedule("scheduler_cron_list");
        if !decision.is_allowed() {
            return Ok(ToolError::permission(
                "Missing SchedulerAccess capability for scheduler.cron.list",
            )
            .with_enforced_rules(
                decision
                    .enforced_rules
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )
            .to_error_response());
        }

        let jobs =
            store.list_scheduled_jobs_for_owner(&manifest.agent.id, args.limit, args.offset)?;

        let job_summaries: Vec<serde_json::Value> = jobs
            .into_iter()
            .filter(|j| {
                args.status
                    .as_ref()
                    .map_or(true, |s| j.status.to_string() == *s)
            })
            .map(|j| {
                serde_json::json!({
                    "job_id": j.job_id,
                    "owner_agent_id": j.owner_agent_id,
                    "target_agent_id": j.target_agent_id,
                    "cron_expr": j.cron_expr,
                    "timezone": j.timezone,
                    "next_run_at": j.next_run_at,
                    "last_run_at": j.last_run_at,
                    "status": j.status.to_string(),
                    "created_at": j.created_at,
                })
            })
            .collect();

        let response = serde_json::json!({
            "ok": true,
            "jobs": job_summaries,
            "count": job_summaries.len()
        });

        serde_json::to_string(&response).map_err(Into::into)
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

#[derive(Deserialize)]
struct PauseArgs {
    job_id: String,
}

pub struct SchedulerCronPauseTool;

impl NativeTool for SchedulerCronPauseTool {
    fn name(&self) -> &'static str {
        "scheduler_cron_pause"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Pause a scheduled cron job. Paused jobs will not trigger until resumed."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job ID to pause"
                    }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        let policy = PolicyEngine::new(manifest.clone());
        policy.can_schedule("scheduler_cron_pause").is_allowed()
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: PauseArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let decision = policy.can_schedule("scheduler_cron_pause");
        if !decision.is_allowed() {
            return Ok(ToolError::permission(
                "Missing SchedulerAccess capability for scheduler.cron.pause",
            )
            .with_enforced_rules(
                decision
                    .enforced_rules
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )
            .to_error_response());
        }

        let job = store.get_scheduled_job(&args.job_id)?;
        match job {
            Some(j) => {
                if j.owner_agent_id != manifest.agent.id {
                    return Ok(ToolError::permission(
                        "Not authorized to pause this job (ownership mismatch)",
                    )
                    .to_error_response());
                }
                let paused = store.pause_scheduled_job(&args.job_id)?;
                Ok(serde_json::to_string(&serde_json::json!({
                    "ok": paused,
                    "job_id": args.job_id,
                    "status": if paused { "paused" } else { "no_change" }
                }))?)
            }
            None => Ok(ToolError::not_found(
                format!("scheduled job '{}'", args.job_id),
                Some("Use scheduler.cron.list to see your active jobs.".to_string()),
            )
            .to_error_response()),
        }
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

#[derive(Deserialize)]
struct ResumeArgs {
    job_id: String,
}

pub struct SchedulerCronResumeTool;

impl NativeTool for SchedulerCronResumeTool {
    fn name(&self) -> &'static str {
        "scheduler_cron_resume"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Resume a paused scheduled cron job.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job ID to resume"
                    }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        let policy = PolicyEngine::new(manifest.clone());
        policy.can_schedule("scheduler_cron_resume").is_allowed()
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: ResumeArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let decision = policy.can_schedule("scheduler_cron_resume");
        if !decision.is_allowed() {
            return Ok(ToolError::permission(
                "Missing SchedulerAccess capability for scheduler.cron.resume",
            )
            .with_enforced_rules(
                decision
                    .enforced_rules
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )
            .to_error_response());
        }

        let job = store.get_scheduled_job(&args.job_id)?;
        match job {
            Some(j) => {
                if j.owner_agent_id != manifest.agent.id {
                    return Ok(ToolError::permission(
                        "Not authorized to resume this job (ownership mismatch)",
                    )
                    .to_error_response());
                }
                let resumed = store.resume_scheduled_job(&args.job_id)?;
                Ok(serde_json::to_string(&serde_json::json!({
                    "ok": resumed,
                    "job_id": args.job_id,
                    "status": if resumed { "active" } else { "no_change" }
                }))?)
            }
            None => Ok(ToolError::not_found(
                format!("scheduled job '{}'", args.job_id),
                Some("Use scheduler.cron.list to see your active jobs.".to_string()),
            )
            .to_error_response()),
        }
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}

#[derive(Deserialize)]
struct CancelArgs {
    job_id: String,
}

pub struct SchedulerCronCancelTool;

impl NativeTool for SchedulerCronCancelTool {
    fn name(&self) -> &'static str {
        "scheduler_cron_cancel"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Cancel a scheduled cron job permanently. Cancelled jobs cannot be resumed."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "The job ID to cancel"
                    }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        let policy = PolicyEngine::new(manifest.clone());
        policy.can_schedule("scheduler_cron_cancel").is_allowed()
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: CancelArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(store) = gateway_store else {
            return Ok(
                ToolError::resource("Gateway store not available", None::<String>)
                    .to_error_response(),
            );
        };

        let decision = policy.can_schedule("scheduler_cron_cancel");
        if !decision.is_allowed() {
            return Ok(ToolError::permission(
                "Missing SchedulerAccess capability for scheduler.cron.cancel",
            )
            .with_enforced_rules(
                decision
                    .enforced_rules
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            )
            .to_error_response());
        }

        let job = store.get_scheduled_job(&args.job_id)?;
        match job {
            Some(j) => {
                if j.owner_agent_id != manifest.agent.id {
                    return Ok(ToolError::permission(
                        "Not authorized to cancel this job (ownership mismatch)",
                    )
                    .to_error_response());
                }
                let cancelled = store.cancel_scheduled_job(&args.job_id)?;
                if cancelled {
                    if let Some(cfg) = config {
                        if let Err(e) =
                            crate::scheduler::workflow_store::append_scheduled_job_cancelled_workflow_event(
                                cfg,
                                store.as_ref(),
                                &j.root_session_id,
                                &j.job_id,
                                &j.owner_agent_id,
                                &j.target_agent_id,
                                &j.cron_expr,
                                "scheduler.cron.cancel",
                            )
                        {
                            tracing::warn!(
                                target: "scheduler",
                                error = %e,
                                job_id = %args.job_id,
                                "Failed to append scheduled_job.cancelled workflow event"
                            );
                        }
                    }
                }
                Ok(serde_json::to_string(&serde_json::json!({
                    "ok": cancelled,
                    "job_id": args.job_id,
                    "status": if cancelled { "cancelled" } else { "no_change" }
                }))?)
            }
            None => Ok(ToolError::not_found(
                format!("scheduled job '{}'", args.job_id),
                Some("Use scheduler.cron.list to see your active jobs.".to_string()),
            )
            .to_error_response()),
        }
    }

    fn extract_metadata(&self, _arguments_json: &str) -> ToolMetadata {
        ToolMetadata::default()
    }
}
