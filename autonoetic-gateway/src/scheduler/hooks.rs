use std::sync::Arc;

use autonoetic_types::hooks::{HookAction, HookConfig, HookContext, HookEvent};

use crate::scheduler::gateway_store::GatewayStore;

pub struct HookExecutor {
    hooks: Vec<HookConfig>,
    store: Option<Arc<GatewayStore>>,
    port: u16,
    signal_timeout_secs: u64,
}

impl HookExecutor {
    pub fn new(
        hooks: Vec<HookConfig>,
        store: Option<Arc<GatewayStore>>,
        port: u16,
        signal_timeout_secs: u64,
    ) -> Self {
        Self {
            hooks,
            store,
            port,
            signal_timeout_secs,
        }
    }

    pub fn dispatch(&self, ctx: &HookContext) {
        for hook in &self.hooks {
            if hook.event != ctx.event {
                continue;
            }
            match hook.action {
                HookAction::PublishReport => self.publish_report(ctx, hook),
                HookAction::DeliverSignal => self.deliver_signal(ctx, hook),
                HookAction::AgentSpawn | HookAction::HttpCallback => {
                    tracing::warn!(
                        target: "hooks",
                        action = ?hook.action,
                        "hook action not yet implemented"
                    );
                }
            }
        }
    }

    pub fn dispatch_async(&self, ctx: HookContext) {
        for hook in &self.hooks {
            if hook.event != ctx.event {
                continue;
            }
            match hook.action {
                HookAction::PublishReport => {
                    let hook = hook.clone();
                    let ctx = ctx.clone();
                    let store = self.store.clone();
                    tokio::spawn(async move {
                        if let Err(e) = Self::publish_report_sync(&store, &ctx, &hook) {
                            tracing::warn!(
                                target: "hooks",
                                error = %e,
                                "publish_report hook failed"
                            );
                        }
                    });
                }
                HookAction::DeliverSignal => {
                    let store = self.store.clone();
                    let ctx = ctx.clone();
                    let hook = hook.clone();
                    let port = self.port;
                    let timeout_secs = self.signal_timeout_secs;
                    tokio::spawn(async move {
                        if let Err(e) =
                            Self::deliver_signal_sync(&store, &ctx, &hook, port, timeout_secs).await
                        {
                            tracing::warn!(
                                target: "hooks",
                                error = %e,
                                "deliver_signal hook failed"
                            );
                        }
                    });
                }
                HookAction::AgentSpawn | HookAction::HttpCallback => {
                    tracing::warn!(
                    target: "hooks",
                    action = ?hook.action,
                    "hook action not yet implemented"
                    );
                }
            }
        }
    }

    fn publish_report(&self, ctx: &HookContext, hook: &HookConfig) {
        if hook.r#async {
            let store = self.store.clone();
            let ctx = ctx.clone();
            let hook = hook.clone();
            tokio::spawn(async move {
                if let Err(e) = Self::publish_report_sync(&store, &ctx, &hook) {
                    tracing::warn!(
                        target: "hooks",
                        error = %e,
                        "publish_report hook failed"
                    );
                }
            });
        } else if let Err(e) = Self::publish_report_sync(&self.store, ctx, hook) {
            tracing::warn!(
                target: "hooks",
                error = %e,
                "publish_report hook failed"
            );
        }
    }

    fn publish_report_sync(
        store: &Option<Arc<GatewayStore>>,
        ctx: &HookContext,
        _hook: &HookConfig,
    ) -> anyhow::Result<()> {
        let Some(store) = store else {
            tracing::debug!(target: "hooks", "no gateway store, skipping publish_report");
            return Ok(());
        };
        let session_id = ctx.session_id.as_deref().unwrap_or(&ctx.root_session_id);

        let gateway_dir = match &ctx.gateway_dir {
            Some(d) => std::path::PathBuf::from(d),
            None => {
                tracing::debug!(target: "hooks", "no gateway_dir in context, skipping publish_report");
                return Ok(());
            }
        };

        let session_dir = gateway_dir.join("sessions").join(session_id);

        let report_json_path = session_dir.join("session_report.json");

        let report_body = match std::fs::read_to_string(&report_json_path) {
            Ok(body) => body,
            Err(e) => {
                tracing::debug!(
                    target: "hooks",
                    path = %report_json_path.display(),
                    error = %e,
                    "session_report.json not found, skipping publish"
                );
                return Ok(());
            }
        };

        let parsed: serde_json::Value = serde_json::from_str(&report_body).unwrap_or_default();
        let title = format!(
            "Session report: {}",
            ctx.session_id.as_deref().unwrap_or(&ctx.root_session_id)
        );
        let status = parsed
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let started_at = parsed
            .get("started_at")
            .and_then(|v| v.as_str())
            .map(String::from);
        let ended_at = parsed
            .get("ended_at")
            .and_then(|v| v.as_str())
            .map(String::from);
        let agent_count = parsed
            .get("agents")
            .map(|v| v.as_object().map(|o| o.len()).unwrap_or(0))
            .unwrap_or(0) as i32;
        let error_count = parsed
            .get("agents")
            .and_then(|agents| {
                agents
                    .as_object()
                    .map(|map| {
                        map.values()
                            .filter_map(|a| a.get("error_count").and_then(|e| e.as_i64()))
                            .sum::<i64>()
                    })
                    .or(Some(0))
            })
            .unwrap_or(0) as i32;
        let approval_count = parsed
            .get("agents")
            .and_then(|agents| {
                agents
                    .as_object()
                    .map(|map| {
                        map.values()
                            .filter_map(|a| a.get("approval_count").and_then(|e| e.as_i64()))
                            .sum::<i64>()
                    })
                    .or(Some(0))
            })
            .unwrap_or(0) as i32;

        let mut search_parts = vec![title.clone()];
        if let Some(reason) = ctx.fields.get("close_reason") {
            search_parts.push(reason.clone());
        }
        for agent_val in parsed
            .get("agents")
            .and_then(|a| a.as_object())
            .into_iter()
            .flat_map(|m| m.values())
        {
            if let Some(s) = agent_val.get("agent_id").and_then(|v| v.as_str()) {
                search_parts.push(s.to_string());
            }
        }
        let search_text = search_parts.join(" ");
        let search_text = crate::log_redaction::redact_text_for_logs(&search_text);
        let title = crate::log_redaction::redact_text_for_logs(&title);

        let sanitized_body = sanitize_report_for_publishing(&report_body);

        let report_handle =
            crate::runtime::content_store::ContentStore::compute_handle(sanitized_body.as_bytes());

        if let Ok(content_store) = crate::runtime::content_store::ContentStore::new(&gateway_dir) {
            if let Err(e) = content_store.write(sanitized_body.as_bytes()) {
                tracing::warn!(
                    target: "hooks",
                    error = %e,
                    "Failed to write report to content store"
                );
            }
        }

        let mut html_handle: Option<String> = None;
        let html_path = session_dir.join("session_report_final.html");
        if let Ok(html_body) = std::fs::read_to_string(&html_path) {
            let h =
                crate::runtime::content_store::ContentStore::compute_handle(html_body.as_bytes());
            html_handle = Some(h);
            if let Ok(content_store) =
                crate::runtime::content_store::ContentStore::new(&gateway_dir)
            {
                let _ = content_store.write(html_body.as_bytes());
            }
        }

        store.upsert_published_session_report(
            &autonoetic_types::causal_chain::PublishedSessionReportRecord {
                root_session_id: ctx.root_session_id.clone(),
                report_handle,
                overview_handle: None,
                html_handle,
                narrative_handle: None,
                title,
                status: status.to_string(),
                started_at,
                ended_at,
                agent_count,
                error_count,
                approval_count,
                search_text,
                generated_at: chrono::Utc::now().to_rfc3339(),
                report_version: 1,
            },
        )?;

        tracing::info!(
            target: "hooks",
            root_session_id = %ctx.root_session_id,
            "published session report to catalog"
        );
        Ok(())
    }

    fn deliver_signal(&self, ctx: &HookContext, hook: &HookConfig) {
        if hook.r#async {
            let store = self.store.clone();
            let ctx = ctx.clone();
            let hook = hook.clone();
            let port = self.port;
            let timeout_secs = self.signal_timeout_secs;
            tokio::spawn(async move {
                if let Err(e) =
                    Self::deliver_signal_sync(&store, &ctx, &hook, port, timeout_secs).await
                {
                    tracing::warn!(
                        target: "hooks",
                        error = %e,
                        "deliver_signal hook failed"
                    );
                }
            });
        } else {
            tracing::warn!(
                target: "hooks",
                "sync deliver_signal not supported, use async: true"
            );
        }
    }

    async fn deliver_signal_sync(
        store: &Option<Arc<GatewayStore>>,
        ctx: &HookContext,
        hook: &HookConfig,
        port: u16,
        timeout_secs: u64,
    ) -> anyhow::Result<()> {
        let Some(store) = store else {
            return Ok(());
        };
        let target_session = ctx.session_id.as_deref().unwrap_or(&ctx.root_session_id);
        let request_id = ctx.fields.get("request_id").cloned().unwrap_or_default();

        let signal = match ctx.event {
            HookEvent::ApprovalResolved => {
                let decision = ctx
                    .fields
                    .get("decision")
                    .map(String::as_str)
                    .unwrap_or("approved");
                crate::scheduler::signal::Signal::ApprovalResolved {
                    request_id: request_id.clone(),
                    agent_id: ctx.agent_id.clone().unwrap_or_default(),
                    status: if decision == "approved" || decision == "granted" {
                        "approved".to_string()
                    } else {
                        "denied".to_string()
                    },
                    install_completed: false,
                    message: format!("Approval {} {}", request_id, decision),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                }
            }
            HookEvent::WorkflowJoinSatisfied => {
                let task_ids: Vec<String> = ctx
                    .fields
                    .get("task_ids")
                    .map(|s| s.split(',').map(String::from).collect())
                    .unwrap_or_default();
                let workflow_id = ctx.fields.get("workflow_id").cloned().unwrap_or_default();
                crate::scheduler::signal::Signal::WorkflowJoinSatisfied {
                    workflow_id,
                    join_task_ids: task_ids,
                    message: hook
                        .params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("workflow join satisfied")
                        .to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                }
            }
            _ => {
                tracing::debug!(target: "hooks", event = ?ctx.event, "deliver_signal not applicable");
                return Ok(());
            }
        };

        crate::scheduler::signal::write_signal(
            Some(store.as_ref()),
            target_session,
            &request_id,
            &signal,
        )?;

        let pending = crate::scheduler::signal::PendingSignal {
            request_id: request_id.clone(),
            signal,
            filename: format!("{}.json", &request_id),
        };
        crate::scheduler::signal::deliver_signal(&pending, target_session, port, timeout_secs)
            .await?;

        Ok(())
    }
}

fn sanitize_report_for_publishing(report_json: &str) -> String {
    let mut parsed: serde_json::Value = match serde_json::from_str(report_json) {
        Ok(v) => v,
        Err(_) => return report_json.to_string(),
    };

    if let Some(agents) = parsed.get_mut("agents").and_then(|a| a.as_object_mut()) {
        for agent_val in agents.values_mut() {
            let agent = match agent_val.as_object_mut() {
                Some(a) => a,
                None => continue,
            };
            agent.remove("input_preview");
            agent.remove("output_preview");

            if let Some(errors) = agent.get_mut("errors").and_then(|e| e.as_array_mut()) {
                for err in errors.iter_mut() {
                    if let Some(obj) = err.as_object_mut() {
                        obj.remove("summary");
                    }
                }
            }
            if let Some(approvals) = agent.get_mut("approvals").and_then(|a| a.as_array_mut()) {
                for appr in approvals.iter_mut() {
                    if let Some(obj) = appr.as_object_mut() {
                        obj.remove("reason");
                        obj.remove("resolution_summary");
                    }
                }
            }
        }
    }

    if let Some(timeline) = parsed.get_mut("timeline").and_then(|t| t.as_array_mut()) {
        for event in timeline.iter_mut() {
            if let Some(obj) = event.as_object_mut() {
                obj.remove("details");
                obj.remove("payload_ref");
            }
        }
    }

    serde_json::to_string(&parsed).unwrap_or_else(|_| report_json.to_string())
}
