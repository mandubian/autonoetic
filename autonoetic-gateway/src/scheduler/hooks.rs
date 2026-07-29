use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use autonoetic_types::hooks::{HookAction, HookConfig, HookContext, HookEvent};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::scheduler::gateway_store::GatewayStore;

/// Request sent over the spawn channel when an `agent.spawn` hook fires.
/// The receiver in `GatewayExecutionService` calls `spawn_agent_once`.
#[derive(Debug)]
pub struct HookSpawnRequest {
    /// Target agent to spawn (from `params.agent_id`).
    pub agent_id: String,
    /// Rendered message (template substitution applied).
    pub message: String,
    /// Session ID for the new spawn — `hook-spawn-<uuid>`.
    pub session_id: String,
    /// Root session that fired the hook.
    pub root_session_id: String,
}

pub struct HookExecutor {
    hooks: Vec<HookConfig>,
    store: Option<Arc<GatewayStore>>,
    port: u16,
    signal_timeout_secs: u64,
    /// Sender half of the spawn channel. When `None`, `agent.spawn` hooks are
    /// logged as warnings and skipped (pre-wiring state, e.g. in tests).
    spawn_tx: Option<tokio::sync::mpsc::Sender<HookSpawnRequest>>,
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
            spawn_tx: None,
        }
    }

    /// Wire up the spawn channel. Called by `GatewayExecutionService` after
    /// creating both the executor and the channel.
    pub fn set_spawn_tx(&mut self, tx: tokio::sync::mpsc::Sender<HookSpawnRequest>) {
        self.spawn_tx = Some(tx);
    }

    pub fn wants_policy_decision_hooks(&self) -> bool {
        self.hooks
            .iter()
            .any(|h| h.event == HookEvent::PolicyDecision)
    }

    pub fn has_deliver_signal_hook(&self, event: HookEvent) -> bool {
        self.hooks
            .iter()
            .any(|h| h.event == event && h.action == HookAction::DeliverSignal)
    }

    /// Called by `GatewayStore::create_causal_event` after the row is committed.
    /// Observer-only: never affects allow/deny.
    pub fn maybe_dispatch_policy_decision_hook(
        &self,
        event: &autonoetic_types::causal_chain::CausalEventRecord,
    ) {
        if !self.wants_policy_decision_hooks() {
            return;
        }
        if !autonoetic_types::causal_chain::causal_event_notifies_policy_decision(event) {
            return;
        }
        let root = crate::runtime::content_store::root_session_id(&event.session_id);
        let ctx = HookContext::for_policy_decision(&root, event);
        self.dispatch_async(ctx);
    }

    pub fn dispatch(&self, ctx: &HookContext) {
        for hook in &self.hooks {
            if hook.event != ctx.event {
                continue;
            }
            match hook.action {
                HookAction::PublishReport => self.publish_report(ctx, hook),
                HookAction::DeliverSignal => self.deliver_signal(ctx, hook),
                HookAction::AgentSpawn => self.agent_spawn(ctx, hook),
                HookAction::HttpCallback => self.http_callback(ctx, hook),
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
                HookAction::AgentSpawn => {
                    self.agent_spawn(&ctx, hook);
                }
                HookAction::HttpCallback => {
                    // dispatch_async always fires hooks in background tasks;
                    // the hook.async flag is only honored by the blocking
                    // dispatch() path (used in tests / non-tokio callers).
                    let store = self.store.clone();
                    let ctx = ctx.clone();
                    let hook = hook.clone();
                    tokio::spawn(async move {
                        if let Err(e) = Self::http_callback_sync(&store, &ctx, &hook).await {
                            tracing::warn!(
                                target: "hooks",
                                error = %e,
                                "http.callback hook failed"
                            );
                        }
                    });
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
            let sanitized_html = sanitize_html_for_publishing(&html_body);
            let h =
                crate::runtime::content_store::ContentStore::compute_handle(sanitized_html.as_bytes());
            html_handle = Some(h);
            if let Ok(content_store) =
                crate::runtime::content_store::ContentStore::new(&gateway_dir)
            {
                let _ = content_store.write(sanitized_html.as_bytes());
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
                    child_summaries: Vec::new(),
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

    fn http_callback(&self, ctx: &HookContext, hook: &HookConfig) {
        if hook.r#async {
            let store = self.store.clone();
            let ctx = ctx.clone();
            let hook = hook.clone();
            tokio::spawn(async move {
                if let Err(e) = Self::http_callback_sync(&store, &ctx, &hook).await {
                    tracing::warn!(
                        target: "hooks",
                        error = %e,
                        "http.callback hook failed"
                    );
                }
            });
        } else if let Err(e) =
            crate::runtime::tools::block_on_http(Self::http_callback_sync(&self.store, ctx, hook))
        {
            tracing::warn!(
                target: "hooks",
                error = %e,
                "http.callback hook failed"
            );
        }
    }

    async fn http_callback_sync(
        store: &Option<Arc<GatewayStore>>,
        ctx: &HookContext,
        hook: &HookConfig,
    ) -> anyhow::Result<()> {
        let callback_url = required_string_param(hook, "url")?;
        let secret_env = required_string_param(hook, "secret_env")?;
        let parsed_url = validate_callback_url(&callback_url, &hook.callback_allowlist)?;
        // Resolve the hostname *before* connecting and reject if any returned
        // address is internal. This catches the common case of a hostname
        // pointing at RFC-1918/loopback space without requiring a custom
        // connector. Note: this does not fully prevent DNS-rebinding (TOCTOU),
        // but it raises the bar significantly.
        check_resolved_ips_not_internal(&parsed_url).await?;
        let delivery_id = build_http_callback_delivery_id(ctx, &parsed_url, hook);
        let event_name = ctx.event.as_str();
        let action_name = "http.callback";

        if let Some(store) = store {
            if let Some(existing) =
                store.get_hook_delivery(&delivery_id, event_name, action_name)?
            {
                if existing.status == "delivered" {
                    tracing::debug!(
                        target: "hooks",
                        delivery_id = %delivery_id,
                        url = %callback_url,
                        "http.callback already delivered, skipping duplicate dispatch"
                    );
                    return Ok(());
                }
            }
        }

        let payload = build_http_callback_payload(ctx, &delivery_id)?;
        let body = serde_json::to_vec(&payload)?;
        let secret = std::env::var(&secret_env)
            .with_context(|| format!("http.callback secret env '{}' is not set", secret_env))?;
        anyhow::ensure!(
            !secret.trim().is_empty(),
            "http.callback secret env '{}' is empty",
            secret_env
        );
        let signature = compute_hmac_sha256_hex(secret.as_bytes(), &body)?;
        // Disable all redirects: a 30x to an internal address would bypass the
        // allowlist/SSRF check that ran against the original URL.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let mut last_error: Option<String> = None;
        let mut backoff = Duration::from_millis(250);

        for attempt in 1..=3_i64 {
            let now = chrono::Utc::now().to_rfc3339();
            if let Some(store) = store {
                store.upsert_hook_delivery(
                    &delivery_id,
                    event_name,
                    action_name,
                    "pending",
                    attempt,
                    last_error.as_deref(),
                    &now,
                )?;
            }

            match client
                .post(parsed_url.clone())
                .header("content-type", "application/json")
                .header("x-autonoetic-event", event_name)
                .header("x-autonoetic-delivery-id", &delivery_id)
                .header("x-autonoetic-signature", format!("sha256={signature}"))
                .body(body.clone())
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    if let Some(store) = store {
                        store.upsert_hook_delivery(
                            &delivery_id,
                            event_name,
                            action_name,
                            "delivered",
                            attempt,
                            None,
                            &chrono::Utc::now().to_rfc3339(),
                        )?;
                    }
                    tracing::info!(
                        target: "hooks",
                        delivery_id = %delivery_id,
                        event = %event_name,
                        url = %callback_url,
                        attempts = attempt,
                        "http.callback delivered"
                    );
                    return Ok(());
                }
                Ok(response) => {
                    let status = response.status();
                    let body_preview = response.text().await.unwrap_or_default();
                    last_error = Some(format_http_callback_error(status, &body_preview));
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }

            let status = if attempt == 3 { "failed" } else { "pending" };
            if let Some(store) = store {
                store.upsert_hook_delivery(
                    &delivery_id,
                    event_name,
                    action_name,
                    status,
                    attempt,
                    last_error.as_deref(),
                    &chrono::Utc::now().to_rfc3339(),
                )?;
            }

            if attempt < 3 {
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }

        Err(anyhow::anyhow!(
            "http.callback delivery failed after 3 attempts: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        ))
    }

    // ── agent.spawn ──────────────────────────────────────────────────────

    fn agent_spawn(&self, ctx: &HookContext, hook: &HookConfig) {
        if !hook.r#async {
            tracing::warn!(
                target: "hooks",
                "agent.spawn hook requires async: true — skipping synchronous dispatch"
            );
            return;
        }
        self.agent_spawn_async(ctx.clone(), hook.clone());
    }

    fn agent_spawn_async(&self, ctx: HookContext, hook: HookConfig) {
        // Validate required params.
        let agent_id = match hook.params.get("agent_id").and_then(|v| v.as_str()) {
            Some(id) if !id.trim().is_empty() => id.to_string(),
            _ => {
                tracing::warn!(
                    target: "hooks",
                    event = ?ctx.event,
                    "agent.spawn hook is missing required param 'agent_id' — skipping"
                );
                return;
            }
        };

        // ACL: if allowed_agents is non-empty, target must be in the list.
        if !hook.allowed_agents.is_empty() && !hook.allowed_agents.iter().any(|a| a == &agent_id) {
            tracing::warn!(
                target: "hooks",
                agent_id = %agent_id,
                allowed = ?hook.allowed_agents,
                "agent.spawn hook: agent_id not in allowed_agents — ACL blocked"
            );
            return;
        }

        // Render message template.
        let template = hook
            .params
            .get("message_template")
            .and_then(|v| v.as_str())
            .unwrap_or("Hook-triggered spawn from event {{event}}");
        let message = render_template(template, &ctx);

        // Build a unique, traceable session ID.
        let session_id = autonoetic_types::id_format::short_random_id_hex("hook-spawn-", 12);

        let Some(ref tx) = self.spawn_tx else {
            tracing::warn!(
                target: "hooks",
                agent_id = %agent_id,
                session_id = %session_id,
                "agent.spawn hook fired but spawn channel is not wired — skipping"
            );
            return;
        };

        let req = HookSpawnRequest {
            agent_id: agent_id.clone(),
            message,
            session_id: session_id.clone(),
            root_session_id: ctx.root_session_id.clone(),
        };

        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = tx.send(req).await {
                tracing::warn!(
                    target: "hooks",
                    agent_id = %agent_id,
                    session_id = %session_id,
                    error = %e,
                    "agent.spawn hook: failed to send on spawn channel"
                );
            } else {
                tracing::info!(
                    target: "hooks",
                    agent_id = %agent_id,
                    session_id = %session_id,
                    "agent.spawn hook: spawn request queued"
                );
            }
        });
    }
}

fn required_string_param(hook: &HookConfig, key: &str) -> anyhow::Result<String> {
    let value = hook
        .params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("hook param '{}' is required", key))?;
    Ok(value.to_string())
}

fn validate_callback_url(
    url: &str,
    allowlist: &[autonoetic_types::background::GrantTarget],
) -> anyhow::Result<Url> {
    anyhow::ensure!(
        !allowlist.is_empty(),
        "http.callback requires a non-empty callback_allowlist"
    );
    let parsed = Url::parse(url)?;
    let scheme = parsed.scheme();
    anyhow::ensure!(
        scheme == "https" || scheme == "http",
        "http.callback only supports http/https URLs"
    );
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("http.callback URL is missing a host"))?;
    anyhow::ensure!(
        !is_disallowed_callback_host(host),
        "http.callback URL host '{}' is not allowed",
        host
    );

    let port = parsed.port_or_known_default();
    let host_and_port = port.map(|p| format!("{}:{p}", host.to_ascii_lowercase()));
    let allowed = allowlist.iter().any(|target| match target {
        autonoetic_types::background::GrantTarget::Any => true,
        autonoetic_types::background::GrantTarget::UrlPrefix(_) => target.matches(url),
        autonoetic_types::background::GrantTarget::HostAndPort { .. } => host_and_port
            .as_deref()
            .map(|authority| target.matches(authority))
            .unwrap_or(false),
        autonoetic_types::background::GrantTarget::ExactHost(_)
        | autonoetic_types::background::GrantTarget::HostSuffix(_) => target.matches(host),
    });
    anyhow::ensure!(
        allowed,
        "http.callback URL '{}' is not covered by callback_allowlist",
        url
    );
    Ok(parsed)
}

async fn check_resolved_ips_not_internal(url: &Url) -> anyhow::Result<()> {
    let host = match url.host() {
        Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_)) => {
            return Ok(());
        }
        Some(url::Host::Domain(d)) => d.to_string(),
        None => return Ok(()),
    };
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("http.callback: failed to resolve host '{}'", host))?;
    for addr in addrs {
        if is_disallowed_callback_host(&addr.ip().to_string()) {
            anyhow::bail!(
                "http.callback URL resolves to a disallowed address '{}' for host '{}'",
                addr.ip(),
                host
            );
        }
    }
    Ok(())
}

fn is_disallowed_callback_host(host: &str) -> bool {
    if cfg!(test)
        && std::env::var("AUTONOETIC_TEST_ALLOW_LOOPBACK_HTTP_CALLBACK")
            .map(|value| value == "1")
            .unwrap_or(false)
    {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(ip) => {
                let octets = ip.octets();
                let is_documentation = matches!(
                    octets,
                    [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _]
                );
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_broadcast()
                    || ip.is_unspecified()
                    || ip.octets()[0] == 0
                    || is_documentation
            }
            IpAddr::V6(ip) => {
                let segments = ip.segments();
                let is_documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
                ip.is_loopback()
                    || ip.is_unique_local()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.is_unicast_link_local()
                    || is_documentation
            }
        };
    }
    false
}

fn build_http_callback_delivery_id(ctx: &HookContext, url: &Url, hook: &HookConfig) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ctx.event.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(ctx.root_session_id.as_bytes());
    hasher.update(b"\n");
    if let Some(session_id) = &ctx.session_id {
        hasher.update(session_id.as_bytes());
    }
    hasher.update(b"\n");
    if let Some(agent_id) = &ctx.agent_id {
        hasher.update(agent_id.as_bytes());
    }
    hasher.update(b"\n");
    hasher.update(url.as_str().as_bytes());
    let mut field_pairs: Vec<_> = ctx.fields.iter().collect();
    field_pairs.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (key, value) in field_pairs {
        hasher.update(b"\n");
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
    }
    hasher.update(b"\nhook_params:");
    let mut param_pairs: Vec<_> = hook.params.iter().collect();
    param_pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (key, val) in param_pairs {
        hasher.update(b"\n");
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(val.to_string().as_bytes());
    }
    format!("hook-{}", hex::encode(hasher.finalize()))
}

fn build_http_callback_payload(
    ctx: &HookContext,
    delivery_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let mut payload = json!({
        "delivery_id": delivery_id,
        "event": ctx.event.as_str(),
        "root_session_id": ctx.root_session_id,
        "session_id": ctx.session_id,
        "agent_id": ctx.agent_id,
        "fields": ctx.fields,
        "sent_at": chrono::Utc::now().to_rfc3339(),
    });

    if let Some(report) = load_sanitized_session_report(ctx)? {
        payload["report"] = report;
    }

    let redacted = crate::log_redaction::RedactedPayload::from_raw(payload).into_inner();
    Ok(redact_callback_payload_strings(redacted))
}

fn load_sanitized_session_report(ctx: &HookContext) -> anyhow::Result<Option<serde_json::Value>> {
    if ctx.event != HookEvent::SessionClosed {
        return Ok(None);
    }
    let Some(gateway_dir) = &ctx.gateway_dir else {
        return Ok(None);
    };
    let session_id = ctx.session_id.as_deref().unwrap_or(&ctx.root_session_id);
    let path = std::path::Path::new(gateway_dir)
        .join("sessions")
        .join(session_id)
        .join("session_report.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => {
            if session_id != ctx.root_session_id {
                let root_path = std::path::Path::new(gateway_dir)
                    .join("sessions")
                    .join(&ctx.root_session_id)
                    .join("session_report.json");
                match std::fs::read_to_string(&root_path) {
                    Ok(raw) => raw,
                    Err(_) => return Ok(None),
                }
            } else {
                return Ok(None);
            }
        }
    };
    let sanitized = sanitize_report_for_publishing(&raw);
    let parsed = serde_json::from_str(&sanitized).with_context(|| {
        format!(
            "failed to parse sanitized session report {}",
            path.display()
        )
    })?;
    Ok(Some(parsed))
}

fn compute_hmac_sha256_hex(secret: &[u8], body: &[u8]) -> anyhow::Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|e| anyhow::anyhow!("failed to initialize HMAC: {e}"))?;
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn redact_callback_payload_strings(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, redact_callback_payload_strings(value)))
                .collect(),
        ),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(redact_callback_payload_strings)
                .collect(),
        ),
        serde_json::Value::String(text) => {
            serde_json::Value::String(crate::log_redaction::redact_text_for_logs(&text))
        }
        other => other,
    }
}

fn format_http_callback_error(status: reqwest::StatusCode, response_body: &str) -> String {
    let body = crate::log_redaction::redact_text_for_logs(response_body);
    if body.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {} {}", status.as_u16(), truncate_chars(&body, 240))
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod policy_decision_hook_tests {
    use autonoetic_types::causal_chain::{
        causal_event_notifies_policy_decision, default_enforced_rules, CausalEventRecord,
    };

    fn sample_record(status: &str, enforced_rules: Vec<String>) -> CausalEventRecord {
        CausalEventRecord {
            event_id: "e1".into(),
            agent_id: "a".into(),
            session_id: "s".into(),
            turn_id: None,
            event_seq: 1,
            timestamp: "2026-01-01T00:00:00Z".into(),
            category: "tool_invoke".into(),
            action: "completed".into(),
            status: status.into(),
            enforced_rules,
            target: None,
            payload: None,
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        }
    }

    #[test]
    fn emits_denied_even_if_only_baseline_rule() {
        let ev = sample_record("DENIED", default_enforced_rules());
        assert!(causal_event_notifies_policy_decision(&ev));
    }

    #[test]
    fn emits_success_when_non_baseline_rule_present() {
        let mut rules = default_enforced_rules();
        rules.push("R+16".into());
        let ev = sample_record("SUCCESS", rules);
        assert!(causal_event_notifies_policy_decision(&ev));
    }

    #[test]
    fn skips_success_when_only_baseline_rule() {
        let ev = sample_record("SUCCESS", default_enforced_rules());
        assert!(!causal_event_notifies_policy_decision(&ev));
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

fn sanitize_html_for_publishing(html: &str) -> String {
    crate::log_redaction::redact_text_for_logs(html)
}

/// Renders a `message_template` string by replacing `{{key}}` placeholders
/// with values from `HookContext.fields`. The special key `{{event}}` is
/// substituted with the event's string name.
///
/// Unknown placeholders are left as-is.
fn render_template(template: &str, ctx: &HookContext) -> String {
    let mut out = template.to_string();
    // Substitute the event name first.
    out = out.replace("{{event}}", ctx.event.as_str());
    // Then all context fields.
    for (key, value) in &ctx.fields {
        out = out.replace(&format!("{{{{{}}}}}", key), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use autonoetic_types::background::GrantTarget;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use serial_test::serial;
    use tempfile::tempdir;

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        headers: HashMap<String, String>,
        body_raw: String,
        body_json: serde_json::Value,
    }

    #[derive(Clone)]
    struct CaptureState {
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    #[derive(Clone)]
    struct FlakyState {
        attempts: Arc<AtomicUsize>,
    }

    struct ScopedEnv {
        key: &'static str,
        previous: Option<String>,
    }

    impl ScopedEnv {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    async fn capture_request(
        State(state): State<CaptureState>,
        headers: HeaderMap,
        body: String,
    ) -> StatusCode {
        let captured = CapturedRequest {
            headers: headers
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (key.to_string(), value.to_string()))
                })
                .collect(),
            body_json: serde_json::from_str(&body).expect("valid hook payload json"),
            body_raw: body,
        };
        state.requests.lock().unwrap().push(captured);
        StatusCode::OK
    }

    async fn flaky_handler(State(state): State<FlakyState>) -> StatusCode {
        let attempt = state.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt < 3 {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        }
    }

    fn http_callback_hook(url: &str, secret_env: &str) -> HookConfig {
        HookConfig {
            event: HookEvent::SessionClosed,
            action: HookAction::HttpCallback,
            r#async: true,
            params: serde_json::json!({
                "url": url,
                "secret_env": secret_env,
            })
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect(),
            callback_allowlist: vec![GrantTarget::UrlPrefix(url.to_string())],
            allowed_agents: Vec::new(),
        }
    }

    fn session_closed_ctx(gateway_dir: &std::path::Path) -> HookContext {
        HookContext::for_session_closed(
            "root-session-1",
            "root-session-1",
            "coder.default",
            "token=top-secret",
            4,
            Some(gateway_dir),
        )
    }

    fn write_session_report(gateway_dir: &std::path::Path) {
        let session_dir = gateway_dir.join("sessions").join("root-session-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("session_report.json"),
            serde_json::json!({
                "status": "completed",
                "agents": {
                    "coder.default": {
                        "agent_id": "coder.default",
                        "input_preview": "token=should-not-leak",
                        "output_preview": "secret response",
                        "approvals": [{
                            "reason": "token=secret",
                            "resolution_summary": "done"
                        }]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn test_http_callback_delivers_signed_redacted_payload_and_persists_delivery() {
        let _secret = ScopedEnv::set("AUTONOETIC_HOOK_SECRET", "hook-secret");
        let _loopback = ScopedEnv::set("AUTONOETIC_TEST_ALLOW_LOOPBACK_HTTP_CALLBACK", "1");
        let temp = tempdir().unwrap();
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        write_session_report(&gateway_dir);

        let requests = Arc::new(Mutex::new(Vec::<CapturedRequest>::new()));
        let app = Router::new()
            .route("/hooks", post(capture_request))
            .with_state(CaptureState {
                requests: requests.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/hooks");
        let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
        let executor = HookExecutor::new(
            vec![http_callback_hook(&url, "AUTONOETIC_HOOK_SECRET")],
            Some(store.clone()),
            4000,
            60,
        );
        let ctx = session_closed_ctx(&gateway_dir);
        let delivery_id = build_http_callback_delivery_id(
            &ctx,
            &Url::parse(&url).unwrap(),
            &http_callback_hook(&url, "AUTONOETIC_HOOK_SECRET"),
        );

        executor.dispatch_async(ctx);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !requests.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("request should arrive");

        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let request = &captured[0];
        assert_eq!(
            request
                .headers
                .get("x-autonoetic-event")
                .map(String::as_str),
            Some("session.closed")
        );
        assert_eq!(
            request
                .headers
                .get("x-autonoetic-delivery-id")
                .map(String::as_str),
            Some(delivery_id.as_str())
        );
        let expected_sig = format!(
            "sha256={}",
            compute_hmac_sha256_hex(b"hook-secret", request.body_raw.as_bytes()).unwrap()
        );
        assert_eq!(
            request
                .headers
                .get("x-autonoetic-signature")
                .map(String::as_str),
            Some(expected_sig.as_str())
        );
        assert_eq!(request.body_json["delivery_id"], delivery_id);
        assert_eq!(request.body_json["event"], "session.closed");
        // close_reason was set to "token=top-secret" in session_closed_ctx.
        // The canonical redaction (autonoetic-types::redaction) now masks the
        // value in place via ENV_ASSIGN_RE rather than wholesale-redacting the
        // string. Still no secret leak — `top-secret` is gone — but the
        // assignment shape `token=…` is preserved for operator triage.
        assert_eq!(
            request.body_json["fields"]["close_reason"],
            "token=***REDACTED***"
        );
        assert!(
            !request.body_json["fields"]["close_reason"]
                .as_str()
                .unwrap_or_default()
                .contains("top-secret"),
            "secret value must not leak"
        );
        assert!(
            request.body_json["report"]["agents"]["coder.default"]["input_preview"].is_null()
        );
        assert!(
            request.body_json["report"]["agents"]["coder.default"]["output_preview"].is_null()
        );
        assert!(
            request.body_json["report"]["agents"]["coder.default"]["approvals"][0]["reason"]
                .is_null()
        );

        let delivery = store
            .get_hook_delivery(&delivery_id, "session.closed", "http.callback")
            .unwrap()
            .expect("delivery row");
        assert_eq!(delivery.status, "delivered");
        assert_eq!(delivery.attempt_count, 1);
        assert!(delivery.last_error.is_none());

        handle.abort();
    }

    #[tokio::test]
    #[serial]
    async fn test_http_callback_retries_and_records_attempt_count() {
        let _secret = ScopedEnv::set("AUTONOETIC_HOOK_SECRET", "retry-secret");
        let _loopback = ScopedEnv::set("AUTONOETIC_TEST_ALLOW_LOOPBACK_HTTP_CALLBACK", "1");
        let temp = tempdir().unwrap();
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        write_session_report(&gateway_dir);

        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/hooks", post(flaky_handler))
            .with_state(FlakyState {
                attempts: attempts.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{addr}/hooks");
        let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
        let executor = HookExecutor::new(
            vec![http_callback_hook(&url, "AUTONOETIC_HOOK_SECRET")],
            Some(store.clone()),
            4000,
            60,
        );
        let ctx = session_closed_ctx(&gateway_dir);
        let delivery_id = build_http_callback_delivery_id(
            &ctx,
            &Url::parse(&url).unwrap(),
            &http_callback_hook(&url, "AUTONOETIC_HOOK_SECRET"),
        );

        executor.dispatch_async(ctx);

        // Poll the authoritative *store* row for the terminal state, not the
        // handler's `attempts` counter. The counter reaches 3 the instant the
        // 3rd request lands, but the executor records the "delivered" row only
        // *after* that HTTP call returns and is processed — waiting on the
        // counter and then asserting on the row races the store write (the flake
        // this replaces). The row reaching "delivered" is the real completion
        // signal.
        let delivery = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match store.get_hook_delivery(&delivery_id, "session.closed", "http.callback") {
                    Ok(Some(d)) if d.status == "delivered" => return d,
                    // No row yet, or not yet terminal — keep polling.
                    Ok(_) => {}
                    // A store/SQL error is a real bug; fail fast with it rather
                    // than let it masquerade as a generic delivery timeout.
                    Err(e) => panic!("get_hook_delivery failed: {e}"),
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("delivery should reach 'delivered' within the timeout");

        assert_eq!(delivery.status, "delivered");
        assert_eq!(delivery.attempt_count, 3);
        // By the time the row is "delivered" the 3rd attempt has necessarily
        // landed, so the handler counter is a stable 3.
        assert_eq!(attempts.load(Ordering::SeqCst), 3);

        handle.abort();
    }

    #[test]
    #[serial]
    fn test_http_callback_rejects_internal_hosts_without_test_override() {
        // Ensure the loopback-allow override is not set from a concurrent test.
        let _guard = ScopedEnv::set("AUTONOETIC_TEST_ALLOW_LOOPBACK_HTTP_CALLBACK", "0");
        let err = validate_callback_url(
            "http://127.0.0.1:8080/hooks",
            &[GrantTarget::UrlPrefix("http://127.0.0.1:8080/".to_string())],
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not allowed"));
    }
}
