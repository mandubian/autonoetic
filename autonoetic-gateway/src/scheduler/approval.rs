//! Approval resolution for the background scheduler.
//! Handles loading, approving, and rejecting approval requests.
//!
//! The gateway follows a "Lawful Gate / Agent Retry" model: on approval it merely
//! unblocks the workflow and notifies the agent, which retries the tool call
//! with an approval_ref. The gateway never auto-executes tool calls on behalf
//! of the agent.

use crate::execution::{gateway_actor_id, init_gateway_causal_logger};
use crate::tracing::{EventScope, SessionId, TraceSession};
use autonoetic_types::background::{
    ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;

/// Determine the required approval level for a given action based on config.
pub fn resolve_approval_level(config: &GatewayConfig, action: &ScheduledAction) -> ApprovalLevel {
    let level_config = &config.approval_levels;
    let action_kind = action.kind();

    // Check action_overrides first
    if let Some(level_str) = level_config.action_overrides.get(action_kind) {
        return parse_approval_level(level_str);
    }

    // For SandboxExec, check host_overrides against the command
    if let ScheduledAction::SandboxExec { command, .. } = action {
        for (pattern, level_str) in &level_config.host_overrides {
            if pattern.trim().is_empty() {
                tracing::warn!(
                    target: "approval",
                    "Ignoring empty approval_levels.host_overrides pattern"
                );
                continue;
            }
            if command.contains(pattern) {
                return parse_approval_level(level_str);
            }
        }
    }

    // Fall back to default
    level_config
        .default
        .as_deref()
        .map(parse_approval_level)
        .unwrap_or(ApprovalLevel::Operator)
}

fn parse_approval_level(s: &str) -> ApprovalLevel {
    match s {
        "admin" => ApprovalLevel::Admin,
        s if s.starts_with("agent:") => {
            ApprovalLevel::Agent(s.strip_prefix("agent:").unwrap_or(s).to_string())
        }
        _ => ApprovalLevel::Operator,
    }
}

/// Check whether the provided approver level satisfies the required level.
pub fn level_satisfies(provided: &ApprovalLevel, required: &ApprovalLevel) -> bool {
    match (provided, required) {
        // Admin satisfies any level
        (ApprovalLevel::Admin, _) => true,
        // Operator satisfies Operator only
        (ApprovalLevel::Operator, ApprovalLevel::Operator) => true,
        (ApprovalLevel::Operator, _) => false,
        // Agent(x) satisfies Agent(x) exactly
        (ApprovalLevel::Agent(a), ApprovalLevel::Agent(b)) => a == b,
        (ApprovalLevel::Agent(_), _) => false,
    }
}

/// Load approval requests from the gateway store for a specific session.
///
/// Fetches pending approval requests stored directly in the SQLite `GatewayStore`.
/// Returns an empty list if the gateway store is unavailable.
pub fn load_approval_requests(
    _config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
) -> anyhow::Result<Vec<ApprovalRequest>> {
    if let Some(store) = gateway_store {
        store.get_pending_approvals()
    } else {
        // GatewayStore not available - return empty list instead of error
        Ok(Vec::new())
    }
}

/// Pending approvals whose [`ApprovalRequest::session_id`] shares the same root session as
/// `root_session_id` (see [`crate::runtime::content_store::root_session_id`]).
pub fn pending_approval_requests_for_root(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    root_session_id: &str,
) -> anyhow::Result<Vec<ApprovalRequest>> {
    let all = load_approval_requests(config, gateway_store)?;
    Ok(all
        .into_iter()
        .filter(|r| {
            crate::runtime::content_store::root_session_id(&r.session_id) == root_session_id
        })
        .collect())
}

/// Pending [`ScheduledAction::SandboxExec`] approvals for an exact `session_id` (e.g. child
/// delegation path), oldest first. Used to stop repeated `sandbox.exec` calls from minting many
/// `apr-*` rows while an approval is still open.
pub fn pending_sandbox_exec_requests_for_session(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
) -> anyhow::Result<Vec<ApprovalRequest>> {
    if session_id.is_empty() {
        return Ok(Vec::new());
    }
    let mut v: Vec<ApprovalRequest> = load_approval_requests(config, gateway_store)?
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .filter(|r| matches!(r.action, ScheduledAction::SandboxExec { .. }))
        .collect();
    v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(v)
}

/// Pending approvals of any kind for an exact `session_id`, oldest first.
/// Used to stop repeated calls from minting many `apr-*` rows while an approval is still open.
pub fn pending_approval_requests_for_session(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    session_id: &str,
) -> anyhow::Result<Vec<ApprovalRequest>> {
    if session_id.is_empty() {
        return Ok(Vec::new());
    }
    let mut v: Vec<ApprovalRequest> = load_approval_requests(config, gateway_store)?
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .collect();
    v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(v)
}

/// Optional parameters for approving a request with Phase 2 grant options.
#[derive(Default)]
pub struct ApproveOptions {
    pub grant_scope: Option<autonoetic_types::background::GrantScope>,
    pub grant_targets: Vec<autonoetic_types::background::GrantTarget>,
    pub grant_expires_at: Option<String>,
    /// Capability type names the operator explicitly acknowledges as part of
    /// approving a `RevisionPromote` request (R++2). Must match the union of
    /// `added_capabilities + broadened_capabilities` exactly. Empty for any
    /// other action type.
    pub acknowledged_capabilities: Vec<String>,
    /// R++4: Confirmation phrase for destructive approval classes. Must match
    /// the `confirm_phrase` stored on the approval request exactly (case-insensitive).
    pub confirm_phrase: Option<String>,
}

pub fn approve_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
    secrets: Option<Vec<(String, String)>>,
    approver_level: Option<&ApprovalLevel>,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
) -> anyhow::Result<ApprovalDecision> {
    approve_request_with_options(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason,
        secrets,
        approver_level,
        hook_executor,
        ApproveOptions::default(),
    )
}

pub fn approve_request_with_options(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
    secrets: Option<Vec<(String, String)>>,
    approver_level: Option<&ApprovalLevel>,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
    options: ApproveOptions,
) -> anyhow::Result<ApprovalDecision> {
    let store = gateway_store
        .ok_or_else(|| anyhow::anyhow!("GatewayStore is required to approve requests"))?;
    let req = store
        .get_approval(request_id)?
        .ok_or_else(|| anyhow::anyhow!("Approval request not found in store: {}", request_id))?;

    // Level validation is always enforced. Missing level defaults to Operator.
    let provided_level = approver_level.cloned().unwrap_or(ApprovalLevel::Operator);
    if !level_satisfies(&provided_level, &req.approval_level) {
        anyhow::bail!(
            "Insufficient approval level: this request requires {:?} but you have {:?}",
            req.approval_level,
            provided_level
        );
    }

    // R++4: Dwell time enforcement. Reject if the approval was decided too
    // quickly after the request was created (operator must see the prompt
    // for a minimum time before confirming).
    if let Some(min_dwell_ms) = req.min_dwell_ms {
        let multiplier = if config.approval_dwell_multiplier.is_finite()
            && config.approval_dwell_multiplier >= 0.0
        {
            config.approval_dwell_multiplier
        } else {
            1.0
        };
        let effective_dwell = (min_dwell_ms as f64 * multiplier) as i64;
        if effective_dwell > 0 {
            let created = chrono::DateTime::parse_from_rfc3339(&req.created_at).map_err(|e| {
                anyhow::anyhow!(
                    "R++4: Cannot parse created_at '{}' for dwell-time check: {}",
                    req.created_at,
                    e
                )
            })?;
            let elapsed_ms = chrono::Utc::now()
                .signed_duration_since(created.with_timezone(&chrono::Utc))
                .num_milliseconds();
            if elapsed_ms < effective_dwell {
                anyhow::bail!(
                    "R++4: Dwell time not met — this approval class requires {} ms \
                     before confirmation, but only {} ms have elapsed since creation. \
                     Wait and retry.",
                    effective_dwell,
                    elapsed_ms
                );
            }
        }
    }

    // R++4: Confirm phrase enforcement. Destructive approval classes require
    // the operator to type a specific phrase to confirm.
    if let Some(ref required_phrase) = req.confirm_phrase {
        let provided = options.confirm_phrase.as_deref().unwrap_or("");
        if !provided.eq_ignore_ascii_case(required_phrase) {
            anyhow::bail!(
                "R++4: Confirmation phrase required for this approval class. \
                 Expected: '{}'. Provide via --confirm-phrase.",
                required_phrase
            );
        }
    }

    // R++2: a `RevisionPromote` approval can only be approved when the
    // operator names every added/broadened capability via
    // `--acknowledge-capability`. The set must match exactly — silent
    // accretion is the threat we are defending against.
    if let ScheduledAction::RevisionPromote {
        added_capabilities,
        broadened_capabilities,
        agent_id: target_agent_id,
        revision_id: target_revision_id,
        ..
    } = &req.action
    {
        use std::collections::BTreeSet;
        let required: BTreeSet<&str> = added_capabilities
            .iter()
            .chain(broadened_capabilities.iter())
            .map(String::as_str)
            .collect();
        let acknowledged: BTreeSet<&str> = options
            .acknowledged_capabilities
            .iter()
            .map(String::as_str)
            .collect();
        if acknowledged != required {
            let missing: Vec<&str> = required.difference(&acknowledged).copied().collect();
            let extra: Vec<&str> = acknowledged.difference(&required).copied().collect();
            anyhow::bail!(
                "Capability-accretion approval (R++2) for agent '{}' revision '{}' \
                 requires the operator to acknowledge each added/broadened capability \
                 by name via --acknowledge-capability. Required: [{}]. Missing: [{}]. \
                 Unexpected: [{}].",
                target_agent_id,
                target_revision_id,
                required.iter().copied().collect::<Vec<_>>().join(", "),
                missing.join(", "),
                extra.join(", "),
            );
        }
    }

    // If secrets are provided, store them in the vault before approving
    // and create the CredentialRecord so the caller can resume.
    if let ScheduledAction::CredentialPrompt {
        service,
        credential_id,
        secret_fields,
        payload,
        ..
    } = &req.action
    {
        // For CredentialPrompt, secrets are always required
        let secret_pairs = secrets.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "CredentialPrompt approval requires secrets. Provide them via --secret KEY=VALUE or interactively."
            )
        })?;
        if secret_pairs.is_empty() {
            anyhow::bail!("CredentialPrompt approval requires at least one secret. None provided.");
        }

        // Extract setup metadata from payload
        let inject_as = payload.as_ref().and_then(|p| {
            p.get("inject_as")
                .and_then(|v| v.as_str().map(String::from))
        });
        let allowed_hosts: Vec<String> = payload
            .as_ref()
            .and_then(|p| {
                p.get("allowed_hosts")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
            })
            .unwrap_or_default();
        let expires_at = payload.as_ref().and_then(|p| {
            p.get("expires_at")
                .and_then(|v| v.as_str().map(String::from))
        });

        // Store secrets in vault — fail-closed, require VAULT_PATH
        let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
            .ok()
            .map(std::path::PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!("AUTONOETIC_VAULT_PATH must be set for credential prompt approval")
            })?;
        let mut vault = crate::vault::Vault::load_from_file(&vault_path).map_err(|e| {
            anyhow::anyhow!(
                "Failed to load vault from {}: {}. Ensure AUTONOETIC_VAULT_KEY or AUTONOETIC_VAULT_KEY_PATH is set.",
                vault_path.display(),
                e
            )
        })?;

        // Validate that all secret_fields have corresponding values
        let missing: Vec<&str> = secret_fields
            .iter()
            .filter(|f| !secret_pairs.iter().any(|(name, _)| name == &f.name))
            .map(|f| f.name.as_str())
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "Missing required secret fields for credential prompt: {}. Provided: {:?}.",
                missing.join(", "),
                secret_pairs
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        for field in secret_fields {
            if let Some((_, value)) = secret_pairs.iter().find(|(name, _)| name == &field.name) {
                vault.set_secret(&field.name, value.clone());
            }
        }
        vault.persist_to_file(&vault_path)?;

        // Create the CredentialRecord with full metadata
        let cred = autonoetic_types::agent::CredentialRecord {
            credential_id: credential_id.clone(),
            service: service.clone(),
            secret_name: secret_fields
                .first()
                .map(|f| f.name.clone())
                .unwrap_or_default(),
            inject_as,
            created_by_agent: Some(req.agent_id.clone()),
            expires_at,
            shared_with: vec![],
            allowed_hosts,
            refresh_token_secret_name: None,
            refresh_url: None,
            refresh_method: None,
            refresh_headers: None,
            refresh_extract_access_token: None,
            refresh_extract_refresh_token: None,
            refresh_extract_expires_in: None,
            label: None,
        };
        store.upsert_credential(&cred)?;

        tracing::info!(
            target: "approval",
            request_id = %request_id,
            credential_id = %credential_id,
            secrets_stored = secret_pairs.len(),
            "Stored secrets and created credential record for credential prompt"
        );
    }

    // WikiProposal materialization — must happen before the decision is
    // committed so that an I/O failure leaves the request pending (operator
    // can retry) rather than marking it Approved with partial materialization.
    let wiki_materialized = if let ScheduledAction::WikiProposal {
        page_id,
        title,
        content,
        tags,
        content_sha256,
        proposed_by_agent,
        proposed_by_session,
    } = &req.action
    {
        let wiki_dir = crate::execution::gateway_root_dir(config).join("wiki");
        std::fs::create_dir_all(&wiki_dir)?;
        // Write .md file atomically via temp rename
        let md_path = wiki_dir.join(format!("{}.md", page_id));
        let tmp_path = wiki_dir.join(format!("{}.md.tmp", page_id));
        std::fs::write(&tmp_path, content.as_bytes())?;
        if let Err(e) = std::fs::rename(&tmp_path, &md_path) {
            let _ = std::fs::remove_file(&tmp_path);
            anyhow::bail!("Failed to rename wiki page: {}", e);
        }
        // Update index.toml
        let index_path = wiki_dir.join("index.toml");
        let mut index: Vec<toml::Value> = if index_path.exists() {
            let index_content = std::fs::read_to_string(&index_path).map_err(|e| {
                anyhow::anyhow!("Failed to read wiki index '{}': {}", index_path.display(), e)
            })?;
            let parsed: toml::Value = index_content.parse().map_err(|e| {
                anyhow::anyhow!("Failed to parse wiki index '{}': {}", index_path.display(), e)
            })?;
            parsed.get("pages").and_then(|p| p.as_array().cloned()).unwrap_or_default()
        } else {
            Vec::new()
        };
        let entry = toml::Value::Table({
            let mut m = toml::map::Map::new();
            m.insert("id".to_string(), toml::Value::String(page_id.clone()));
            m.insert("title".to_string(), toml::Value::String(title.clone()));
            m.insert("file".to_string(), toml::Value::String(format!("{}.md", page_id)));
            m.insert("tags".to_string(), toml::Value::Array(
                tags.iter().map(|t| toml::Value::String(t.clone())).collect()
            ));
            m
        });
        if let Some(pos) = index.iter().position(|e| {
            e.get("id").and_then(|v| v.as_str()) == Some(page_id.as_str())
        }) {
            index[pos] = entry;
        } else {
            index.push(entry);
        }
        let index_content = toml::Value::Table({
            let mut m = toml::map::Map::new();
            m.insert("pages".to_string(), toml::Value::Array(index));
            m
        });
        let toml_str = index_content.to_string();
        let tmp_index = wiki_dir.join("index.toml.tmp");
        std::fs::write(&tmp_index, &toml_str)?;
        if let Err(e) = std::fs::rename(&tmp_index, &index_path) {
            let _ = std::fs::remove_file(&tmp_index);
            anyhow::bail!("Failed to rename index.toml: {}", e);
        }
        tracing::info!(
            target: "approval",
            page_id = %page_id,
            title = %title,
            "Wiki page promoted via approval"
        );
        Some(serde_json::json!({
            "page_id": page_id,
            "title": title,
            "content_sha256": content_sha256,
            "proposed_by_agent": proposed_by_agent,
            "proposed_by_session": proposed_by_session,
        }))
    } else {
        None
    };

    let decision = decide_request_with_options(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason.clone(),
        ApprovalStatus::Approved,
        options,
    )?;

    if let ScheduledAction::SessionEscalate { payload, .. } = &decision.action {
        if let Some(payload) = payload {
            if payload.get("type").and_then(|v| v.as_str()) == Some("promotion_review") {
                if let Some(esc_id) = payload.get("escalation_id").and_then(|v| v.as_str()) {
                    if let Some(store) = gateway_store {
                        if let Err(e) = store.resolve_escalation(
                            esc_id,
                            autonoetic_types::escalation::EscalationStatus::Approved,
                            decided_by,
                            reason.as_deref(),
                        ) {
                            tracing::warn!(
                                target: "approval",
                                escalation_id = %esc_id,
                                error = %e,
                                "Failed to resolve linked escalation on approval"
                            );
                        }
                    }
                }
            }
        }
    }

    // Emit timeline + causal events after the decision is recorded.
    if let Some(meta) = wiki_materialized {
        if matches!(decision.status, ApprovalStatus::Approved) {
            // Session timeline event
            emit_wiki_timeline_event(gateway_store, &decision, "wiki.promoted", None);

            // Causal event for observability
            let causal_logger = crate::execution::init_gateway_causal_logger(config)?;
            let mut trace_session = crate::tracing::TraceSession::create_with_session_id(
                crate::tracing::SessionId::from_string(decision.session_id.clone()),
                std::sync::Arc::new(causal_logger),
                crate::execution::gateway_actor_id(),
                crate::tracing::EventScope::Session,
            );
            let _ = trace_session.log_completed(
                "wiki.promoted",
                None,
                Some(serde_json::json!({
                    "page_id": meta["page_id"],
                    "title": meta["title"],
                    "content_sha256": meta["content_sha256"],
                    "proposed_by_agent": meta["proposed_by_agent"],
                    "proposed_by_session": meta["proposed_by_session"],
                    "approved_by": decision.decided_by,
                })),
            );
        } else {
            tracing::info!(
                target: "approval",
                "Wiki proposal rejected or cancelled; files written beforehand discarded"
            );
        }
    }

    // Lawful Gate model: notify the waiting session, do not auto-execute.
    if should_resume_waiting_session(&decision) {
        if let Err(e) =
            resume_session_after_approval(config, gateway_store, &decision, hook_executor)
        {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                error = %e,
                "Failed to send session resume notification"
            );
        }
    } else {
        tracing::info!(
            target: "approval",
            request_id = %decision.request_id,
            action = ?decision.action,
            "No waiting session to resume"
        );
    }

    unblock_task_on_approval(config, gateway_store, &decision);

    Ok(decision)
}

pub fn reject_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
) -> anyhow::Result<ApprovalDecision> {
    let decision = decide_request(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason,
        ApprovalStatus::Rejected,
    )?;

    // Workflow-bound tasks surface rejection through task failure + workflow
    // resume. Non-workflow callers still need a direct notification.
    if should_resume_waiting_session(&decision) {
        resume_session_after_approval(config, gateway_store, &decision, hook_executor)?;
    } else {
        tracing::info!(
            target: "approval",
            request_id = %decision.request_id,
            workflow_id = ?decision.workflow_id,
            task_id = ?decision.task_id,
            "Skipping direct rejection resume; workflow-bound task will continue via workflow failure"
        );
    }

    // Unblock the task in the workflow (marks as Failed)
    unblock_task_on_approval(config, gateway_store, &decision);

    if let Some(ref task_id) = decision.task_id {
        let _ = crate::runtime::continuation::delete_continuation(config, task_id);
    }

    // Emit wiki.rejected timeline event for wiki proposals.
    emit_wiki_timeline_event(gateway_store, &decision, "wiki.rejected", Some(&decision.decided_by));

    Ok(decision)
}

pub fn cancel_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    cancelled_by: &str,
    reason: Option<String>,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
) -> anyhow::Result<ApprovalDecision> {
    let decision =
        cancel_approval_request(config, gateway_store, request_id, cancelled_by, reason)?;

    // Notify waiting session of cancellation
    if should_resume_waiting_session(&decision) {
        resume_session_after_approval(config, gateway_store, &decision, hook_executor)?;
    }

    // Unblock workflow task if bound
    unblock_task_on_approval(config, gateway_store, &decision);

    if let Some(ref task_id) = decision.task_id {
        let _ = crate::runtime::continuation::delete_continuation(config, task_id);
    }

    // Emit wiki.rejected timeline event for cancelled wiki proposals.
    emit_wiki_timeline_event(gateway_store, &decision, "wiki.rejected", Some(cancelled_by));

    Ok(decision)
}

/// Withdraw a still-pending approval bound to a workflow task (e.g. task cancelled).
pub fn cancel_pending_approval_for_workflow_task(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    task_id: &str,
    cancelled_by: &str,
    reason: &str,
) -> anyhow::Result<Option<String>> {
    let Some(store) = gateway_store else {
        return Ok(None);
    };
    let Some(request_id) = store.get_pending_approval_request_id_for_task(task_id)? else {
        return Ok(None);
    };
    let cancelled_at = chrono::Utc::now().to_rfc3339();
    match store.cancel_approval(&request_id, cancelled_by, &cancelled_at) {
        Ok(()) => {
            tracing::info!(
                target: "approval",
                request_id = %request_id,
                task_id = %task_id,
                reason = %reason,
                "Cancelled pending approval for workflow task"
            );
            Ok(Some(request_id))
        }
        Err(error) => {
            tracing::warn!(
                target: "approval",
                request_id = %request_id,
                task_id = %task_id,
                error = %error,
                "Failed to cancel pending approval for workflow task"
            );
            Ok(None)
        }
    }
}

fn cancel_approval_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    cancelled_by: &str,
    reason: Option<String>,
) -> anyhow::Result<ApprovalDecision> {
    let request = if let Some(store) = gateway_store {
        store
            .get_approval(request_id)?
            .ok_or_else(|| anyhow::anyhow!("Approval request not found in store: {}", request_id))?
    } else {
        anyhow::bail!("GatewayStore is required to cancel approvals");
    };

    // Idempotency guard
    if let Some(ref current_status) = request.status {
        let status_str = current_status.as_str();
        anyhow::bail!(
            "Approval {} already decided as '{}' (by {})",
            request_id,
            status_str,
            request.decided_by.as_deref().unwrap_or("unknown")
        );
    }

    let decided_at = chrono::Utc::now().to_rfc3339();

    // Persist cancellation
    if let Some(store) = gateway_store {
        store.cancel_approval(request_id, cancelled_by, &decided_at)?;
    }

    let decision = ApprovalDecision {
        request_id: request.request_id,
        agent_id: request.agent_id,
        session_id: request.session_id,
        action: request.action,
        status: ApprovalStatus::Cancelled,
        decided_at,
        decided_by: cancelled_by.to_string(),
        reason,
        root_session_id: request.root_session_id.clone(),
        workflow_id: request.workflow_id.clone(),
        task_id: request.task_id.clone(),
        approval_level: request.approval_level,
    };

    // Clean up reevaluation state
    let agent_dir = config.agents_dir.join(&decision.agent_id);
    let _ = crate::runtime::reevaluation_state::persist_reevaluation_state(&agent_dir, |state| {
        state
            .open_approval_request_ids
            .retain(|existing| existing != &decision.request_id);
        state.pending_scheduled_action = None;
        state.last_outcome = Some("approval_cancelled".to_string());
    });

    let state_path = crate::scheduler::store::background_state_path(config, &decision.agent_id);
    if let Ok(mut background_state) = crate::scheduler::store::load_background_state(
        &state_path,
        &decision.agent_id,
        &crate::scheduler::decision::background_session_id(&decision.agent_id),
    ) {
        background_state.approval_blocked = false;
        background_state
            .pending_approval_request_ids
            .retain(|existing| existing != &decision.request_id);
        background_state
            .processed_approval_request_ids
            .push(decision.request_id.clone());
        let _ = crate::scheduler::store::save_background_state(&state_path, &background_state);
    }

    // Log to gateway causal chain
    let causal_logger = init_gateway_causal_logger(config)?;
    let mut trace_session = TraceSession::create_with_session_id(
        SessionId::from_string(decision.session_id.clone()),
        Arc::new(causal_logger),
        gateway_actor_id(),
        EventScope::Session,
    );
    let _ = trace_session.log_completed(
        "background.approval",
        Some("cancelled"),
        Some(serde_json::json!({
            "agent_id": decision.agent_id,
            "request_id": decision.request_id,
            "cancelled_by": decision.decided_by,
            "action_kind": decision.action.kind()
        })),
    );

    Ok(decision)
}

/// Queue durable session notifications after approval/rejection.
///
/// This function persists approval-resolution signals that are consumed by
/// gateway-owned delivery loops and/or channel clients (for example, the TUI
/// chat client resuming on its own connection).
///
/// Under the "Lawful Gate" model, the gateway never auto-executes tool calls.
/// It merely notifies the agent that approval was granted, and the agent
/// retries the tool call with an approval_ref.
fn resume_session_after_approval(
    _config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    decision: &ApprovalDecision,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
) -> anyhow::Result<()> {
    // Resume for actions that have a suspended session waiting at an ApprovalRequired checkpoint.
    let is_supported_action = matches!(
        &decision.action,
        autonoetic_types::background::ScheduledAction::AgentInstall { .. }
            | autonoetic_types::background::ScheduledAction::SandboxExec { .. }
            | autonoetic_types::background::ScheduledAction::SessionEscalate { .. }
            | autonoetic_types::background::ScheduledAction::SessionContinue { .. }
            | autonoetic_types::background::ScheduledAction::CredentialRequest { .. }
            | autonoetic_types::background::ScheduledAction::CredentialPrompt { .. }
            | autonoetic_types::background::ScheduledAction::WebFetch { .. }
            | autonoetic_types::background::ScheduledAction::WebCall { .. }
            | autonoetic_types::background::ScheduledAction::WebSearch { .. }
            | autonoetic_types::background::ScheduledAction::RevisionPromote { .. }
    );

    if !is_supported_action {
        tracing::debug!(
            target: "approval",
            request_id = %decision.request_id,
            action = ?decision.action.kind(),
            "No auto-resume needed for this action type"
        );
        return Ok(());
    }

    let session_id = &decision.session_id;
    if session_id.is_empty() {
        return Ok(());
    }

    tracing::info!(
        target: "approval",
        request_id = %decision.request_id,
        session_id = %session_id,
        status = ?decision.status,
        "Resuming session after approval resolution"
    );

    // Build a synthetic message that the gateway will route to the waiting agent
    let status_str = decision.status.as_str();

    // Extract agent_id and build status message based on action type
    let (agent_id, status_message) = match &decision.action {
        autonoetic_types::background::ScheduledAction::AgentInstall { agent_id, .. } => {
            let msg = match decision.status {
                ApprovalStatus::Approved => format!(
                    "approval_resumed:install:{}:{}",
                    decision.request_id, agent_id
                ),
                ApprovalStatus::Rejected => format!(
                    "approval_rejected:install:{}:{}",
                    decision.request_id, agent_id
                ),
                ApprovalStatus::Cancelled => format!(
                    "approval_cancelled:install:{}:{}",
                    decision.request_id, agent_id
                ),
            };
            (agent_id.clone(), msg)
        }
        autonoetic_types::background::ScheduledAction::SandboxExec { .. } => {
            let msg = match decision.status {
                ApprovalStatus::Approved => format!(
                    "approval_resumed:sandbox_exec:{}:approved",
                    decision.request_id
                ),
                ApprovalStatus::Rejected => format!(
                    "approval_rejected:sandbox_exec:{}:rejected",
                    decision.request_id
                ),
                ApprovalStatus::Cancelled => format!(
                    "approval_cancelled:sandbox_exec:{}:cancelled",
                    decision.request_id
                ),
            };
            (decision.agent_id.clone(), msg)
        }
        autonoetic_types::background::ScheduledAction::SessionEscalate { .. } => {
            let guidance = decision.reason.as_deref().unwrap_or("");
            let msg = match decision.status {
                ApprovalStatus::Approved => format!(
                    "escalation_resumed:{}:approved{}",
                    decision.request_id,
                    if guidance.is_empty() {
                        String::new()
                    } else {
                        format!(":guidance={}", guidance)
                    }
                ),
                ApprovalStatus::Rejected => {
                    format!("escalation_rejected:{}:rejected", decision.request_id)
                }
                ApprovalStatus::Cancelled => {
                    format!("escalation_cancelled:{}:cancelled", decision.request_id)
                }
            };
            (decision.agent_id.clone(), msg)
        }
        autonoetic_types::background::ScheduledAction::SessionContinue { .. } => {
            let msg = match decision.status {
                ApprovalStatus::Approved => {
                    format!("session_continue_approved:{}:approved", decision.request_id)
                }
                ApprovalStatus::Rejected => {
                    format!("session_continue_rejected:{}:rejected", decision.request_id)
                }
                ApprovalStatus::Cancelled => format!(
                    "session_continue_cancelled:{}:cancelled",
                    decision.request_id
                ),
            };
            (decision.agent_id.clone(), msg)
        }
        autonoetic_types::background::ScheduledAction::CredentialRequest { .. }
        | autonoetic_types::background::ScheduledAction::CredentialPrompt { .. } => {
            let msg = match decision.status {
                ApprovalStatus::Approved => format!(
                    "approval_resumed:credential:{}:approved",
                    decision.request_id
                ),
                ApprovalStatus::Rejected => format!(
                    "approval_rejected:credential:{}:rejected",
                    decision.request_id
                ),
                ApprovalStatus::Cancelled => format!(
                    "approval_cancelled:credential:{}:cancelled",
                    decision.request_id
                ),
            };
            (decision.agent_id.clone(), msg)
        }
        autonoetic_types::background::ScheduledAction::WebFetch { .. }
        | autonoetic_types::background::ScheduledAction::WebCall { .. }
        | autonoetic_types::background::ScheduledAction::WebSearch { .. } => {
            let msg = match decision.status {
                ApprovalStatus::Approved => format!(
                    "approval_resumed:web:{}:approved",
                    decision.request_id
                ),
                ApprovalStatus::Rejected => format!(
                    "approval_rejected:web:{}:rejected",
                    decision.request_id
                ),
                ApprovalStatus::Cancelled => format!(
                    "approval_cancelled:web:{}:cancelled",
                    decision.request_id
                ),
            };
            (decision.agent_id.clone(), msg)
        }
        autonoetic_types::background::ScheduledAction::RevisionPromote { .. } => {
            let msg = match decision.status {
                ApprovalStatus::Approved => format!(
                    "approval_resumed:revision_promote:{}:approved",
                    decision.request_id
                ),
                ApprovalStatus::Rejected => format!(
                    "approval_rejected:revision_promote:{}:rejected",
                    decision.request_id
                ),
                ApprovalStatus::Cancelled => format!(
                    "approval_cancelled:revision_promote:{}:cancelled",
                    decision.request_id
                ),
            };
            (decision.agent_id.clone(), msg)
        }
        _ => (
            "unknown".to_string(),
            format!("approval_{}:unknown:{}", status_str, decision.request_id),
        ),
    };

    // Write approval resolution signal to GatewayStore for scheduler delivery (enables auto-resume).
    let signal = super::signal::Signal::ApprovalResolved {
        request_id: decision.request_id.clone(),
        agent_id: agent_id.clone(),
        status: status_str.to_string(),
        install_completed: false,
        message: status_message.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };

    // Write signal to the child session (the original waiting runtime).
    // write_signal persists the record to GatewayStore for durable scheduler delivery.
    if let Err(e) = super::signal::write_signal(
        gateway_store.as_deref(),
        session_id,
        &decision.request_id,
        &signal,
    ) {
        tracing::warn!(
            target: "approval",
            request_id = %decision.request_id,
            error = %e,
            "Failed to write approval signal to store"
        );
    }

    // Use root_session_id from the task graph to determine the parent,
    // rather than string-parsing the session ID.
    let notify_parent = should_notify_parent_session(decision);
    let parent_session_id = decision.root_session_id.as_deref().unwrap_or(session_id);

    if notify_parent {
        tracing::info!(
            target: "approval",
            parent_session = %parent_session_id,
            "Also notifying parent session of approval resolution"
        );

        // Write signal to parent session too
        let parent_signal = super::signal::Signal::ApprovalResolved {
            request_id: decision.request_id.clone(),
            agent_id: agent_id.clone(),
            status: status_str.to_string(),
            install_completed: false,
            message: status_message.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        // write_signal persists the record to GatewayStore for durable scheduler delivery.
        if let Err(e) = super::signal::write_signal(
            gateway_store.as_deref(),
            parent_session_id,
            &decision.request_id,
            &parent_signal,
        ) {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                parent_session = %parent_session_id,
                error = %e,
                "Failed to write approval signal to parent session store"
            );
        }
    }

    // Delivery ownership is gateway-side and durable:
    // this function only persists signals. Gateway pollers and channel-specific
    // consumers (such as the chat TUI on its own socket) perform delivery + ack.
    let target_session = if notify_parent {
        format!("{},{}", session_id, parent_session_id)
    } else {
        session_id.to_string()
    };
    tracing::info!(
        target: "approval",
        request_id = %decision.request_id,
        target_session = %target_session,
        "Approval notification queued for gateway-owned delivery"
    );

    if let Some(executor) = hook_executor {
        let status_str = decision.status.as_str();
        let root_id = decision
            .root_session_id
            .as_deref()
            .unwrap_or(&decision.session_id);
        let ctx = autonoetic_types::hooks::HookContext::for_approval_resolved(
            root_id,
            &decision.session_id,
            &decision.agent_id,
            &decision.request_id,
            status_str,
        );
        executor.dispatch_async(ctx);
    }

    Ok(())
}

/// Determines whether the parent (root) session should be notified of an
/// approval resolution. Uses the task graph (`root_session_id`) rather than
/// string-parsing the session ID.
fn should_notify_parent_session(decision: &ApprovalDecision) -> bool {
    // If the decision has a root_session_id that differs from the session_id,
    // this is a child session and the root should be notified.
    match &decision.root_session_id {
        Some(root) if root != &decision.session_id => true,
        _ => false,
    }
}

fn should_resume_waiting_session(decision: &ApprovalDecision) -> bool {
    !(decision.workflow_id.is_some() && decision.task_id.is_some())
}

/// On approval resolution, update the blocked task's status and emit workflow events.
fn unblock_task_on_approval(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    decision: &ApprovalDecision,
) {
    let (Some(wf_id), Some(t_id)) = (&decision.workflow_id, &decision.task_id) else {
        return;
    };
    let (new_status, approval_event_type) = match decision.status {
        ApprovalStatus::Approved => (
            autonoetic_types::workflow::TaskRunStatus::Runnable,
            "task.approved",
        ),
        ApprovalStatus::Rejected => (
            autonoetic_types::workflow::TaskRunStatus::Failed,
            "task.rejected",
        ),
        ApprovalStatus::Cancelled => (
            autonoetic_types::workflow::TaskRunStatus::Failed,
            "task.cancelled",
        ),
    };

    // Emit the approval decision event before updating status so chat CLI sees it.
    let _ = super::workflow_store::append_workflow_event(
        config,
        gateway_store,
        &autonoetic_types::workflow::WorkflowEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            workflow_id: wf_id.to_string(),
            task_id: Some(t_id.to_string()),
            event_type: approval_event_type.to_string(),
            agent_id: Some(decision.agent_id.clone()),
            payload: serde_json::json!({
                "request_id": decision.request_id,
                "status": decision.status.as_str(),
            }),
            occurred_at: decision.decided_at.clone(),
        },
    );

    let result_summary = match (new_status, &decision.reason) {
        (autonoetic_types::workflow::TaskRunStatus::Failed, Some(r)) => {
            Some(format!("approval_{}: {}", approval_event_type.strip_prefix("task.").unwrap_or("rejected"), r))
        }
        _ => None,
    };
    if let Err(e) = super::workflow_store::update_task_run_status(
        config,
        gateway_store,
        wf_id,
        t_id,
        new_status,
        result_summary,
        None,
        None,
    ) {
        tracing::warn!(
            target: "approval",
            workflow_id = %wf_id,
            task_id = %t_id,
            error = %e,
            "Failed to unblock task on approval resolution"
        );
        return;
    }

    tracing::info!(
        target: "approval",
        workflow_id = %wf_id,
        task_id = %t_id,
        status = ?decision.status,
        "Task unblocked after approval resolution"
    );

    // Save an "approval_resolved" checkpoint with a structured continuation payload.
    let continuation_payload = serde_json::json!({
        "approval_resolved": true,
        "request_id": decision.request_id,
        "status": if decision.status == ApprovalStatus::Approved { "approved" } else { "rejected" },
        "action_type": match &decision.action {
            autonoetic_types::background::ScheduledAction::SandboxExec { .. } => "sandbox_exec",
            autonoetic_types::background::ScheduledAction::AgentInstall { .. } => "agent_install",
            autonoetic_types::background::ScheduledAction::SessionEscalate { .. } => "session_escalate",
            _ => "unknown",
        },
    });
    if let Err(e) = super::workflow_store::checkpoint_task(
        config,
        gateway_store,
        wf_id,
        t_id,
        "approval_resolved".to_string(),
        continuation_payload,
    ) {
        tracing::warn!(
            target: "approval",
            workflow_id = %wf_id,
            task_id = %t_id,
            error = %e,
            "Failed to save approval_resolved checkpoint"
        );
    }

    // Clear BlockedApproval if no tasks remain in AwaitingApproval.
    if let Ok(tasks) =
        super::workflow_store::list_task_runs_for_workflow(config, gateway_store, wf_id)
    {
        let any_awaiting = tasks
            .iter()
            .any(|t| t.status == autonoetic_types::workflow::TaskRunStatus::AwaitingApproval);
        if !any_awaiting {
            if let Ok(Some(mut wf)) =
                super::workflow_store::load_workflow_run(config, gateway_store, wf_id)
            {
                if wf.status == autonoetic_types::workflow::WorkflowRunStatus::BlockedApproval {
                    wf.status = autonoetic_types::workflow::WorkflowRunStatus::WaitingChildren;
                    wf.updated_at = chrono::Utc::now().to_rfc3339();
                    if let Err(e) =
                        super::workflow_store::save_workflow_run(config, gateway_store, &wf)
                    {
                        tracing::warn!(
                            target: "approval",
                            workflow_id = %wf_id,
                            error = %e,
                            "Failed to clear BlockedApproval status"
                        );
                    }
                }
            }
        }
    }
}

/// Emit a session timeline event (`wiki.rejected`) for wiki proposals.
/// Silently skips non-WikiProposal actions.
fn emit_wiki_timeline_event(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    decision: &ApprovalDecision,
    event_type: &str,
    cancelled_by: Option<&str>,
) {
    let ScheduledAction::WikiProposal { ref page_id, ref title, .. } = decision.action else {
        return;
    };
    let Some(store) = gateway_store else { return };

    let role = crate::runtime::session_timeline::derive_role(&decision.agent_id);
    let principal = autonoetic_types::principal::Principal::agent(decision.agent_id.clone());
    let refs = autonoetic_types::session_timeline::TimelineRefs::default();
    let mut payload = serde_json::json!({
        "page_id": page_id,
        "title": title,
        "decided_by": decision.decided_by,
        "reason": decision.reason,
    });
    if let Some(d) = cancelled_by {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("cancelled_by".into(), serde_json::json!(d));
        }
    }
    let event = crate::runtime::session_timeline::build_timeline_event(
        decision.root_session_id.clone().unwrap_or_else(|| decision.session_id.clone()),
        decision.session_id.clone(),
        None,
        &principal,
        &role,
        event_type,
        None,
        Some(payload),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(target: "session_timeline", error = %e, "{event_type} timeline emit failed");
    }
}

/// Whether an approval's action introduces an external effect or is hard to
/// undo — used by the §O classifier to make a principal's *approval* of such an
/// action BLOCKING (must carry a reason). Non-exhaustive: unknown/new actions
/// are treated as local (DEFERRED), failing toward less friction.
fn action_is_external_or_irreversible(action: &ScheduledAction) -> bool {
    use ScheduledAction::*;
    matches!(
        action,
        AgentInstall { .. }
            | CredentialPrompt { .. }
            | CredentialRequest { .. }
            | WebFetch { .. }
            | WebCall { .. }
            | WebSearch { .. }
            | ProfileShare { .. }
            | LayerMount { .. }
            | RevisionPromote { .. }
    )
}

/// §O motivation tier for a gate decision. `true` = BLOCKING (a motivation is
/// required). A rejection/abort by a principal always blocks (the symmetric
/// mirror of `Ri-0.3`); a principal's approval blocks only when the action is
/// elevated-authority or external/irreversible. Mechanical resolutions (no
/// principal — `gateway`/`system`/`emergency_stop:…`) never block. Reversible
/// operator-level approvals are DEFERRED (not enforced here yet — "block now,
/// refine later").
fn decision_is_blocking(
    request: &ApprovalRequest,
    decided_by: &str,
    status: &ApprovalStatus,
) -> bool {
    if autonoetic_types::principal::decider_principal_kind(decided_by).is_none() {
        return false;
    }
    match status {
        ApprovalStatus::Rejected | ApprovalStatus::Cancelled => true,
        ApprovalStatus::Approved => {
            request.approval_level != ApprovalLevel::Operator
                || action_is_external_or_irreversible(&request.action)
        }
    }
}

/// Enforce the §O decider obligation: refuse a BLOCKING-tier decision with no
/// motivation. Presence-only check (never judges the reason's quality —
/// Lawful Executor). Disabled via `decider_obligations.enabled = false`.
fn enforce_decider_motivation(
    config: &GatewayConfig,
    request: &ApprovalRequest,
    decided_by: &str,
    status: &ApprovalStatus,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    if !config.decider_obligations.enabled {
        return Ok(());
    }
    if decision_is_blocking(request, decided_by, status) {
        let has_reason = reason.map(|r| !r.trim().is_empty()).unwrap_or(false);
        if !has_reason {
            anyhow::bail!(
                "§O decider obligation: recording approval '{}' (level {}) as '{}' requires a \
                 motivation. Provide a non-empty reason and retry.",
                request.request_id,
                request.approval_level.to_config(),
                status.as_str()
            );
        }
    }
    Ok(())
}

fn decide_request(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
    status: ApprovalStatus,
) -> anyhow::Result<ApprovalDecision> {
    decide_request_with_options(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason,
        status,
        ApproveOptions::default(),
    )
}

fn decide_request_with_options(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
    status: ApprovalStatus,
    options: ApproveOptions,
) -> anyhow::Result<ApprovalDecision> {
    let request = if let Some(store) = gateway_store {
        store
            .get_approval(request_id)?
            .ok_or_else(|| anyhow::anyhow!("Approval request not found in store: {}", request_id))?
    } else {
        anyhow::bail!("GatewayStore is required to decide approvals");
    };

    // Idempotency guard: reject duplicate decisions
    if let Some(ref current_status) = request.status {
        let status_str = current_status.as_str();
        anyhow::bail!(
            "Approval {} already decided as '{}' (by {})",
            request_id,
            status_str,
            request.decided_by.as_deref().unwrap_or("unknown")
        );
    }

    // §O symmetric obligation (#359 / #395): a principal decider owes a
    // motivation for a BLOCKING-tier decision (reject/abort, or approval of an
    // elevated-authority / external-irreversible action). Checked before commit.
    enforce_decider_motivation(config, &request, decided_by, &status, reason.as_deref())?;

    let decision = ApprovalDecision {
        request_id: request.request_id,
        agent_id: request.agent_id,
        session_id: request.session_id,
        action: request.action,
        status: status.clone(),
        decided_at: chrono::Utc::now().to_rfc3339(),
        decided_by: decided_by.to_string(),
        reason,
        root_session_id: request.root_session_id.clone(),
        workflow_id: request.workflow_id.clone(),
        task_id: request.task_id.clone(),
        approval_level: request.approval_level,
    };
    // Persist decision in GatewayStore
    if let Some(store) = gateway_store {
        store
            .record_decision(
                &decision.request_id,
                decision.status.as_str(),
                &decision.decided_by,
                &decision.decided_at,
                decision.reason.as_deref(),
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to record approval decision '{}' in store: {}",
                    decision.request_id,
                    e
                )
            })?;
    }

    if matches!(status, ApprovalStatus::Approved) {
        let hosts = decision.action.detected_hosts();

        if let Some(hosts) = hosts {
            if !hosts.is_empty() {
                if let Some(root_sid) = &decision.root_session_id {
                    if let Some(store) = gateway_store {
                        let scope = options
                            .grant_scope
                            .clone()
                            .unwrap_or(autonoetic_types::background::GrantScope::RootSession);
                        let targets = if options.grant_targets.is_empty() {
                            hosts
                                .iter()
                                .map(|h| {
                                    autonoetic_types::background::GrantTarget::ExactHost(h.clone())
                                })
                                .collect()
                        } else {
                            options.grant_targets.clone()
                        };
                        let session_id = decision.session_id.as_str();
                        let computed_expiry = if options.grant_expires_at.is_none()
                            && config.default_grant_ttl_secs > 0
                        {
                            let ttl_secs =
                                i64::try_from(config.default_grant_ttl_secs).unwrap_or(i64::MAX);
                            let base = chrono::DateTime::parse_from_rfc3339(&decision.decided_at)
                                .map(|dt| dt.with_timezone(&chrono::Utc))
                                .unwrap_or_else(|_| chrono::Utc::now());
                            let t = base + chrono::Duration::seconds(ttl_secs);
                            Some(t.to_rfc3339())
                        } else {
                            None
                        };
                        let expires_at = options
                            .grant_expires_at
                            .as_deref()
                            .or(computed_expiry.as_deref());
                        if let Err(e) = store.insert_session_grant(
                            root_sid,
                            session_id,
                            &decision.agent_id,
                            &scope,
                            &targets,
                            &decision.decided_by,
                            &decision.decided_at,
                            Some(&decision.request_id),
                            expires_at,
                        ) {
                            tracing::warn!(
                                target: "approval",
                                request_id = %decision.request_id,
                                error = %e,
                                "Failed to insert session approval grants — session grant auto-approval will not be available for this session"
                            );
                        } else {
                            tracing::info!(
                                target: "approval",
                                request_id = %decision.request_id,
                                agent_id = %decision.agent_id,
                                root_session_id = %root_sid,
                                scope = %scope.as_str(),
                                targets = ?targets,
                                "Inserted session approval grants for approved network action"
                            );
                        }
                    }
                }
            }
        }
    }

    let background_session_id = super::decision::background_session_id;
    let load_background_state = super::store::load_background_state;
    let save_background_state = super::store::save_background_state;

    if matches!(status, ApprovalStatus::Rejected) {
        let agent_dir = config.agents_dir.join(&decision.agent_id);
        crate::runtime::reevaluation_state::persist_reevaluation_state(&agent_dir, |state| {
            state
                .open_approval_request_ids
                .retain(|existing| existing != &decision.request_id);
            state.pending_scheduled_action = None;
            state.last_outcome = Some("approval_rejected".to_string());
        })?;
        let state_path = super::store::background_state_path(config, &decision.agent_id);
        let mut background_state = load_background_state(
            &state_path,
            &decision.agent_id,
            &background_session_id(&decision.agent_id),
        )?;
        background_state.approval_blocked = false;
        background_state
            .pending_approval_request_ids
            .retain(|existing| existing != &decision.request_id);
        background_state
            .processed_approval_request_ids
            .push(decision.request_id.clone());
        save_background_state(&state_path, &background_state)?;
    }

    let causal_logger = init_gateway_causal_logger(config)?;
    let mut trace_session = TraceSession::create_with_session_id(
        SessionId::from_string(decision.session_id.clone()),
        Arc::new(causal_logger),
        gateway_actor_id(),
        EventScope::Session,
    );
    let action = match status {
        ApprovalStatus::Approved => "background.approval",
        ApprovalStatus::Rejected => "background.approval",
        ApprovalStatus::Cancelled => "background.approval",
    };
    let status_str = status.as_str();
    let _ = trace_session.log_completed(
        action,
        Some(status_str),
        Some(serde_json::json!({
            "agent_id": decision.agent_id,
            "request_id": decision.request_id,
            "decided_by": decision.decided_by,
            "action_kind": decision.action.kind()
        })),
    );
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::{should_notify_parent_session, should_resume_waiting_session};
    use crate::scheduler::workflow_store::{ensure_workflow_for_root_session, save_task_run};
    use autonoetic_types::background::{
        ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
    };
    use autonoetic_types::config::GatewayConfig;
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
    use tempfile::tempdir;

    #[test]
    fn load_approval_requests_skips_payload_companion_files() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let mut req = ApprovalRequest {
            request_id: "apr-test1234".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root-session/coder-abc".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 x".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            created_at: "2020-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut req).unwrap();

        let loaded = super::load_approval_requests(&cfg, Some(&store)).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].request_id, "apr-test1234");
    }

    #[test]
    fn decider_obligation_blocks_unmotivated_blocking_decisions() {
        use autonoetic_types::background::ApprovalStatus;

        let mk = |level: ApprovalLevel, action: ScheduledAction| ApprovalRequest {
            request_id: "apr".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "s".to_string(),
            action,
            created_at: "2020-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: level,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        let sandbox = || ScheduledAction::SandboxExec {
            command: "x".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        };
        let install = || ScheduledAction::AgentInstall {
            agent_id: "a".to_string(),
            summary: "s".to_string(),
            requested_by_agent_id: "r".to_string(),
            install_fingerprint: "fp".to_string(),
            payload: None,
        };
        let cfg = GatewayConfig::default(); // decider_obligations.enabled = true
        let local = mk(ApprovalLevel::Operator, sandbox());

        // Operator rejection without a reason → blocked (mirror of Ri-0.3).
        assert!(super::enforce_decider_motivation(&cfg, &local, "operator", &ApprovalStatus::Rejected, None).is_err());
        // …with a reason → allowed.
        assert!(super::enforce_decider_motivation(&cfg, &local, "operator", &ApprovalStatus::Rejected, Some("out of scope")).is_ok());
        // Whitespace-only reason doesn't count.
        assert!(super::enforce_decider_motivation(&cfg, &local, "operator", &ApprovalStatus::Rejected, Some("   ")).is_err());
        // Approving a reversible, operator-level action without a reason → allowed (DEFERRED).
        assert!(super::enforce_decider_motivation(&cfg, &local, "operator", &ApprovalStatus::Approved, None).is_ok());
        // Mechanical decider (no principal) is exempt even on rejection.
        assert!(super::enforce_decider_motivation(&cfg, &local, "gateway", &ApprovalStatus::Rejected, None).is_ok());
        assert!(super::enforce_decider_motivation(&cfg, &local, "emergency_stop:estop-1", &ApprovalStatus::Cancelled, None).is_ok());
        // Approving an external/irreversible action without a reason → blocked.
        let ext = mk(ApprovalLevel::Operator, install());
        assert!(super::enforce_decider_motivation(&cfg, &ext, "operator", &ApprovalStatus::Approved, None).is_err());
        // Approving an elevated-authority gate without a reason → blocked.
        let elevated = mk(ApprovalLevel::Admin, sandbox());
        assert!(super::enforce_decider_motivation(&cfg, &elevated, "operator", &ApprovalStatus::Approved, None).is_err());

        // Disabled config → no enforcement at all.
        let cfg_off = GatewayConfig {
            decider_obligations: autonoetic_types::config::DeciderObligationsConfig { enabled: false },
            ..Default::default()
        };
        assert!(super::enforce_decider_motivation(&cfg_off, &local, "operator", &ApprovalStatus::Rejected, None).is_ok());
    }

    #[test]
    fn pending_approval_requests_for_root_filters_by_session() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let req = |id: &str, sess: &str| ApprovalRequest {
            request_id: id.to_string(),
            agent_id: "a".to_string(),
            session_id: sess.to_string(),
            action: ScheduledAction::SandboxExec {
                command: "c".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: "2020-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store
            .create_approval(&mut req("apr-a", "root-a/coder-1"))
            .unwrap();
        store
            .create_approval(&mut req("apr-b", "root-b/coder-1"))
            .unwrap();

        let for_a =
            super::pending_approval_requests_for_root(&cfg, Some(&store), "root-a").unwrap();
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].request_id, "apr-a");
    }

    #[test]
    fn pending_sandbox_exec_requests_for_session_filters_and_sorts() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let req = |id: &str, created: &str| ApprovalRequest {
            request_id: id.to_string(),
            agent_id: "evaluator.default".to_string(),
            session_id: "sess/evaluator-1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 x".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            approval_level: ApprovalLevel::Operator,
            created_at: created.to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store
            .create_approval(&mut req("apr-second", "2020-01-02T00:00:00Z"))
            .unwrap();
        store
            .create_approval(&mut req("apr-first", "2020-01-01T00:00:00Z"))
            .unwrap();
        // Install-style request same session — must not appear in sandbox-only list
        let mut install = ApprovalRequest {
            request_id: "apr-install".to_string(),
            agent_id: "b".to_string(),
            session_id: "sess/evaluator-1".to_string(),
            action: ScheduledAction::AgentInstall {
                agent_id: "x".to_string(),
                summary: "s".to_string(),
                requested_by_agent_id: "y".to_string(),
                install_fingerprint: "fp".to_string(),
                payload: None,
            },
            created_at: "2019-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut install).unwrap();

        let list = super::pending_sandbox_exec_requests_for_session(
            &cfg,
            Some(&store),
            "sess/evaluator-1",
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].request_id, "apr-first");
        assert_eq!(list[1].request_id, "apr-second");
    }

    #[test]
    fn test_should_notify_parent_session_when_root_differs_from_session() {
        let decision = ApprovalDecision {
            request_id: "apr-1".to_string(),
            agent_id: "specialized_builder.default".to_string(),
            session_id: "demo-session/specialized_builder.default-abcd1234".to_string(),
            action: ScheduledAction::AgentInstall {
                agent_id: "specialist.weather".to_string(),
                summary: "install specialist.weather".to_string(),
                requested_by_agent_id: "specialized_builder.default".to_string(),
                install_fingerprint: "sha256:abc123".to_string(),
                payload: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("demo-session".to_string()),
            approval_level: ApprovalLevel::Operator,
        };
        assert!(should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_notify_parent_session_for_sandbox_exec_in_child() {
        let decision = ApprovalDecision {
            request_id: "apr-2".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("demo-session".to_string()),
            approval_level: ApprovalLevel::Operator,
        };
        assert!(should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_not_notify_parent_session_when_root_is_same_as_session() {
        let decision = ApprovalDecision {
            request_id: "apr-3".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("demo-session".to_string()),
            approval_level: ApprovalLevel::Operator,
        };
        assert!(!should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_not_notify_parent_session_when_no_root() {
        let decision = ApprovalDecision {
            request_id: "apr-4".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            approval_level: ApprovalLevel::Operator,
        };
        assert!(!should_notify_parent_session(&decision));
    }

    #[test]
    fn test_should_not_resume_waiting_session_for_workflow_bound_approval() {
        let decision = ApprovalDecision {
            request_id: "apr-workflow1".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: Some("wf-demo".to_string()),
            task_id: Some("task-demo".to_string()),
            root_session_id: Some("demo-session".to_string()),
            approval_level: ApprovalLevel::Operator,
        };

        assert!(!should_resume_waiting_session(&decision));
    }

    #[test]
    fn test_should_resume_waiting_session_for_non_workflow_approval() {
        let decision = ApprovalDecision {
            request_id: "apr-direct1".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            approval_level: ApprovalLevel::Operator,
        };

        assert!(should_resume_waiting_session(&decision));
    }

    #[test]
    fn workflow_bound_approval_skips_direct_session_notification() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        let agent_dir = agents_dir.join("coder.default");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf =
            ensure_workflow_for_root_session(&cfg, Some(&store), "demo-session", None).unwrap();

        let task = TaskRun {
            task_id: "task-approval".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            parent_session_id: "demo-session".to_string(),
            status: TaskRunStatus::AwaitingApproval,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("Continue after approval".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        let mut request = ApprovalRequest {
            request_id: "apr-write123".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: task.session_id.clone(),
            action: ScheduledAction::WriteFile {
                path: "approved.txt".to_string(),
                content: "approved".to_string(),
                requires_approval: true,
                evidence_ref: None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: None,
            evidence_ref: None,
            workflow_id: Some(wf.workflow_id.clone()),
            task_id: Some(task.task_id.clone()),
            root_session_id: Some("demo-session".to_string()),
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut request).unwrap();

        super::approve_request(
            &cfg,
            Some(&store),
            &request.request_id,
            "operator",
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(
            pending.is_empty(),
            "workflow-bound approvals should continue through workflow re-queue only"
        );
    }

    #[test]
    fn revision_promote_approval_signal_prompts_agent_retry() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir,
            ..Default::default()
        };
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let decision = ApprovalDecision {
            request_id: "apr-promote01".to_string(),
            agent_id: "specialized_builder.default".to_string(),
            session_id: "session-88f313bd/specialized_builder.default-c671e74b".to_string(),
            action: ScheduledAction::RevisionPromote {
                agent_id: "weather-lookup".to_string(),
                revision_id: "ar-weather01".to_string(),
                outgoing_revision_id: String::new(),
                added_capabilities: vec!["NetworkAccess".to_string()],
                broadened_capabilities: vec![],
                payload: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("session-88f313bd".to_string()),
            approval_level: ApprovalLevel::Operator,
        };

        super::resume_session_after_approval(&cfg, Some(store.as_ref()), &decision, None).unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(!pending.is_empty(), "should have created a notification");
        let payload = &pending[0].payload;
        assert_eq!(
            payload.get("message").and_then(|v| v.as_str()),
            Some("approval_resumed:revision_promote:apr-promote01:approved")
        );
    }

    #[test]
    fn sandbox_approval_signal_prompts_agent_retry() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir,
            ..Default::default()
        };
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        let decision = ApprovalDecision {
            request_id: "apr-out1234".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 /tmp/weather.py".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            status: ApprovalStatus::Approved,
            decided_at: chrono::Utc::now().to_rfc3339(),
            decided_by: "operator".to_string(),
            reason: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            approval_level: ApprovalLevel::Operator,
        };

        // SandboxExec is no longer auto-executed; agent retries with approval_ref.
        super::resume_session_after_approval(&cfg, Some(store.as_ref()), &decision, None).unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(!pending.is_empty(), "should have created a notification");
    }

    #[test]
    fn resolve_approval_level_ignores_empty_host_override_pattern() {
        let mut cfg = GatewayConfig::default();
        cfg.approval_levels
            .host_overrides
            .insert("".to_string(), "admin".to_string());
        cfg.approval_levels.default = Some("operator".to_string());
        let action = ScheduledAction::SandboxExec {
            command: "python3 /tmp/run.py".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        };
        let level = super::resolve_approval_level(&cfg, &action);
        assert_eq!(level, ApprovalLevel::Operator);
    }

    #[test]
    fn approve_request_defaults_to_operator_and_enforces_required_level() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let mut request = ApprovalRequest {
            request_id: "apr-admin-needed".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "echo secure".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            created_at: "2020-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Admin,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut request).unwrap();

        // Missing approver_level defaults to Operator and should fail for admin requests.
        let denied = super::approve_request(
            &cfg,
            Some(&store),
            &request.request_id,
            "cli",
            None,
            None,
            None,
            None,
        )
        .expect_err("operator default should not satisfy admin-level request");
        assert!(denied.to_string().contains("Insufficient approval level"));

        // Explicit admin level should pass.
        let admin = ApprovalLevel::Admin;
        let decision = super::approve_request(
            &cfg,
            Some(&store),
            &request.request_id,
            "cli",
            None,
            None,
            Some(&admin),
            None,
        )
        .expect("admin-level approval should succeed");
        assert_eq!(decision.status, ApprovalStatus::Approved);
    }

    #[test]
    fn double_approve_is_rejected() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let mut request = ApprovalRequest {
            request_id: "apr-double".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root/coder-abc".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "echo hi".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
            },
            created_at: "2020-01-01T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut request).unwrap();

        // First approve succeeds
        let result = super::approve_request(
            &cfg,
            Some(&store),
            "apr-double",
            "operator",
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok(), "first approve should succeed");

        // Second approve fails with idempotency error
        let result = super::approve_request(
            &cfg,
            Some(&store),
            "apr-double",
            "operator",
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err(), "second approve should be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("already decided"),
            "error should mention already decided: {}",
            err_msg
        );
    }
}
