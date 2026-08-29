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
    ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, GrantScope, GrantTarget,
    ScheduledAction,
};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::plan_frame::PlanStatus;
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
#[derive(Default, Clone)]
pub struct ApproveOptions {
    pub grant_scope: Option<autonoetic_types::background::GrantScope>,
    pub grant_targets: Vec<autonoetic_types::background::GrantTarget>,
    pub grant_expires_at: Option<String>,
    /// When `Some(false)`, skip session-grant materialization even if the
    /// approved action carries `detected_hosts`. This turns the approval into
    /// a one-shot: only this single invocation is authorized, and subsequent
    /// calls to the same hosts will re-trigger the gate. When `None` or
    /// `Some(true)` (the default), grants are created normally.
    pub create_grant: Option<bool>,
    /// Capability type names the operator explicitly acknowledges as part of
    /// approving a `RevisionPromote` request (R++2). Must match the union of
    /// `added_capabilities + broadened_capabilities` exactly. Empty for any
    /// other action type.
    pub acknowledged_capabilities: Vec<String>,
    /// R++4: Confirmation phrase for destructive approval classes. Must match
    /// the `confirm_phrase` stored on the approval request exactly (case-insensitive).
    pub confirm_phrase: Option<String>,
    /// Session ID of the agent-decider, when the decider is an agent. Used for
    /// R-10.7 spawn-tree trust-boundary enforcement.
    pub decider_session_id: Option<String>,
}

/// Pre-computed metadata from the decision entry point that `apply_decision`
/// needs to perform its side-effects. Created by each entry point before
/// calling `apply_decision`.
pub struct DecisionContext<'a> {
    /// Wiki materialization metadata — present if the action was WikiProposal
    /// and was Approved (materialization happens before the decision so files
    /// are always written first).
    pub wiki_materialized_meta: Option<serde_json::Value>,
    /// Hook executor for async hook dispatch after signal write.
    pub hook_executor: Option<&'a crate::scheduler::hooks::HookExecutor>,
}

/// Single fan-out for all post-decision side-effects.
///
/// Every approval decision entry point (approve/reject/cancel/withdraw/
/// cancel-for-task) MUST call this function after persisting the decision.
///
/// # Policy decisions (explicit, not accidental)
///
/// **Agent-initiated withdrawal**: updates `reevaluation_state` and
/// `background_state` identically to operator cancellation. The §O decider
/// obligation does NOT apply to agent withdrawals — the agent is not a
/// principal decider under O-1.
///
/// **Cancellation** always updates `reevaluation_state` and
/// `background_state`, regardless of source (operator, scheduler task,
/// agent).
///
/// **Escalation resolution**: resolved on approve AND reject (not on
/// cancel/withdraw) because an unresolved escalation pollutes
/// `pending_escalation_ids`.
///
/// **Wiki timeline**: emitted as `wiki.decision` with a
/// `decision: "approved"|"rejected"|"withdrawn"|"cancelled"` field,
/// replacing the previous `wiki.promoted` / `wiki.rejected` /
/// `wiki.withdrawn` split.
///
/// **Causal events**: every decision emits a `background.approval` causal
/// event for auditability.
///
/// **Side-effect order** (within this function):
/// 1. Insert session grants (if Approved)
/// 2. Resolve linked escalation (approve + reject only)
/// 3. Update reevaluation_state + background_state (reject/cancel/withdraw)
/// 4. Write resume signal to GatewayStore for scheduler delivery
/// 5. Emit normalized wiki timeline event
/// 6. Emit causal event
/// 7. Unblock workflow task (if workflow-bound)
/// Expiry for a materialized declassification grant: an explicit
/// `options.grant_expires_at` wins; otherwise `default_grant_ttl_secs` from
/// config (0 = no expiry). Shared by the explicit `EgressDeclassify` path and
/// the implicit host-scoped network-approval path so both honor the same TTL.
///
/// Fail-closed on unrepresentable TTLs: a config value that overflows
/// chrono's range expires the grant immediately rather than panicking in
/// approval handling or silently becoming "no expiry".
fn declass_grant_expiry(
    config: &GatewayConfig,
    options: &ApproveOptions,
    decided_at: &str,
) -> Option<String> {
    if let Some(exp) = options.grant_expires_at.as_deref() {
        return Some(exp.to_string());
    }
    if config.default_grant_ttl_secs > 0 {
        let ttl_secs = i64::try_from(config.default_grant_ttl_secs).unwrap_or(i64::MAX);
        let base = chrono::DateTime::parse_from_rfc3339(decided_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        let expiry =
            chrono::Duration::try_seconds(ttl_secs).and_then(|d| base.checked_add_signed(d));
        return match expiry {
            Some(t) => Some(t.to_rfc3339()),
            None => {
                tracing::warn!(
                    target: "approval",
                    default_grant_ttl_secs = config.default_grant_ttl_secs,
                    "declassification grant TTL unrepresentable — expiring immediately (fail-closed)"
                );
                Some(decided_at.to_string())
            }
        };
    }
    None
}

pub fn apply_decision(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    decision: &ApprovalDecision,
    options: &ApproveOptions,
    context: &DecisionContext,
) -> anyhow::Result<()> {
    // Reap the orphaned checkpoint for rejected/cancelled approvals (#607):
    // the suspended turn is dead, so its signed checkpoint file would leak on
    // disk otherwise. (Approved approvals keep their checkpoint — it is
    // consumed on resume.)
    if matches!(
        decision.status,
        ApprovalStatus::Rejected | ApprovalStatus::Cancelled
    ) {
        if let Err(e) = crate::runtime::checkpoint::delete_approval_bound_checkpoint(
            config,
            &decision.session_id,
            &decision.request_id,
        ) {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                session_id = %decision.session_id,
                error = %e,
                "Failed to reap orphan checkpoint after reject/cancel"
            );
        }

        // #1213: the same reasoning that reaps the checkpoint applies to the
        // stored action. `action_payload` is kept raw while a gate is live
        // because it is the scheduler's execution input; a rejected or
        // cancelled gate will never be executed, so the raw command — which may
        // carry a credential the agent inlined — has nothing left to be raw
        // for. Replaced with its operator-class projection, which keeps the
        // shape a human reviewing history reads and drops the values.
        if let Some(store) = gateway_store {
            match store.scrub_dead_approval_payload(&decision.request_id) {
                Ok(true) => tracing::debug!(
                    target: "approval",
                    request_id = %decision.request_id,
                    "Scrubbed secrets from a dead approval's stored action"
                ),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    target: "approval",
                    request_id = %decision.request_id,
                    error = %e,
                    "Failed to scrub a dead approval's stored action"
                ),
            }
        }
    }

    let Some(store) = gateway_store else {
        return Ok(());
    };

    // ── 1. Session grants (Approved only) ──────────────────────────────
    // Operator can opt out of grant creation via `create_grant: Some(false)`
    // to approve just this one invocation without pre-authorizing the hosts
    // for the rest of the session.
    let create_grant = options.create_grant.unwrap_or(true);
    if decision.status == ApprovalStatus::Approved && create_grant {
        let hosts = decision.action.detected_hosts();
        if let Some(hosts) = hosts {
            if !hosts.is_empty() {
                if let Some(root_sid) = &decision.root_session_id {
                    let scope = options
                        .grant_scope
                        .clone()
                        .unwrap_or(GrantScope::RootSession);
                    let targets = if options.grant_targets.is_empty() {
                        hosts
                            .iter()
                            .map(|h| GrantTarget::ExactHost(h.clone()))
                            .collect()
                    } else {
                        options.grant_targets.clone()
                    };
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
                        &decision.session_id,
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
                            "Failed to insert session approval grants"
                        );
                    } else {
                        emit_host_contract_drift_events(
                            store,
                            &decision.agent_id,
                            &decision.session_id,
                            &hosts,
                        );
                    }
                }
            }
        }
    }

    // ── 1c. Promotion attempt ledger reset (issue #720) ────────────────
    if decision.status == ApprovalStatus::Approved {
        if let ScheduledAction::RevisionPromote { agent_id, revision_id, .. } = &decision.action {
            if let Ok(Some(rev)) = store.get_agent_revision(revision_id) {
                if let Err(e) = store.reset_promotion_attempts(agent_id, &rev.content_digest) {
                    tracing::warn!(
                        target: "approval",
                        request_id = %decision.request_id,
                        agent_id = %agent_id,
                        revision_id = %revision_id,
                        error = %e,
                        "Failed to reset promotion attempt ledger after approval"
                    );
                }
            }
        }
    }

    // ── 1e. Network egress under taint = declassification (RFC §8) ─────
    // Sandbox share_net and gateway web tools widen to Sink::Network — but the
    // operator approved egress to *specific hosts*, so materialize **host-scoped**
    // grants (`session:<root>:host:<host>`) + egress.declassified, never a silent
    // session-wide widen (#909 slices 2 / 2b; host-scoping follow-up). Session-wide
    // Network declassification remains possible via the explicit EgressDeclassify
    // action (§1d) only.
    if decision.status == ApprovalStatus::Approved {
        let network_action = matches!(
            decision.action,
            ScheduledAction::SandboxExec { .. }
                | ScheduledAction::WebFetch { .. }
                | ScheduledAction::WebSearch { .. }
                | ScheduledAction::WebCall { .. }
        );
        if network_action {
            if let Some(root_sid) = &decision.root_session_id {
                let taint = store
                    .get_session_egress_taint(&decision.session_id)
                    .ok()
                    .flatten()
                    .or_else(|| store.get_session_egress_taint(root_sid).ok().flatten());
                let network_excluded = taint
                    .as_ref()
                    .map(|t| !t.allows(autonoetic_types::egress::Sink::Network))
                    .unwrap_or(false);
                if network_excluded {
                    let hosts = decision.action.detected_hosts().unwrap_or_default();
                    if hosts.is_empty() {
                        // Fail-closed: an approval without concrete hosts widens
                        // nothing. The retried call refuses again; the repair hint
                        // points at the explicit EgressDeclassify path.
                        tracing::warn!(
                            target: "approval",
                            request_id = %decision.request_id,
                            "Tainted network action approved without detected_hosts — no declassification grant materialized"
                        );
                    }
                    let scope = options
                        .grant_scope
                        .clone()
                        .unwrap_or(GrantScope::RootSession);
                    let expires_at = declass_grant_expiry(config, options, &decision.decided_at);
                    let tool_label = match &decision.action {
                        ScheduledAction::SandboxExec { .. } => "sandbox share_net",
                        ScheduledAction::WebFetch { .. } => "web_fetch",
                        ScheduledAction::WebSearch { .. } => "web_search",
                        ScheduledAction::WebCall { .. } => "web_call",
                        _ => "network egress",
                    };
                    for host in &hosts {
                        let target =
                            crate::runtime::egress_labeler::session_host_network_declass_target(
                                root_sid, host,
                            );
                        let declass_reason = format!(
                            "{tool_label} network egress to {host} under session egress taint (RFC §8)"
                        );
                        match store.insert_egress_declassification_grant(
                            root_sid,
                            &decision.session_id,
                            &decision.agent_id,
                            &target,
                            autonoetic_types::egress::Sink::Network,
                            &scope,
                            &decision.decided_by,
                            &decision.decided_at,
                            Some(&decision.request_id),
                            expires_at.as_deref(),
                        ) {
                            Err(e) => {
                                tracing::warn!(
                                    target: "approval",
                                    request_id = %decision.request_id,
                                    error = %e,
                                    "Failed to insert host-scoped Network declassification grant for tainted network action"
                                );
                            }
                            Ok(()) => {
                                crate::runtime::egress_labeler::emit_declassified(
                                    store,
                                    &decision.session_id,
                                    &decision.agent_id,
                                    &target,
                                    autonoetic_types::egress::Sink::Network,
                                    scope.clone(),
                                    Some(&decision.request_id),
                                    &declass_reason,
                                    expires_at.as_deref(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // ── 1b. PlanFrame side-effects ─────────────────────────────────────
    if let ScheduledAction::PlanFrame {
        plan_id,
        version,
        envelope,
    } = &decision.action
    {
        use autonoetic_types::session_timeline::TimelineRefs;
        let plan_status = match decision.status {
            ApprovalStatus::Approved => PlanStatus::Approved,
            _ => PlanStatus::Cancelled,
        };
        if let Err(e) = store.update_plan_frame_status(
            plan_id,
            *version,
            plan_status,
            Some(&decision.decided_by),
            Some(&decision.decided_at),
        ) {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                error = %e,
                "Failed to update plan frame status"
            );
        }

        if decision.status == ApprovalStatus::Approved {
            // Materialize the declared envelope as session approval grants.
            if let Some(root_sid) = &decision.root_session_id {
                let _ = crate::runtime::session_envelope::materialize_envelope(
                    store,
                    root_sid,
                    envelope,
                    &decision.decided_by,
                    &decision.request_id,
                );

                // Propose/lock the session envelope (declared or discovered).
                if let Ok(Some(plan)) = store.load_plan_frame_revision(plan_id, *version) {
                    let _ = crate::runtime::session_envelope::propose_plan_envelope_on_approval(
                        store,
                        &plan,
                        &decision.decided_by,
                    );
                }
            }

            // Canonical plan-approval timeline event.
            let (principal, role) =
                crate::runtime::session_timeline::decider_seat(&decision.decided_by);
            let refs = TimelineRefs {
                plan_id: Some(plan_id.clone()),
                approval_request_id: Some(decision.request_id.clone()),
                ..Default::default()
            };
            let event = crate::runtime::session_timeline::build_timeline_event(
                decision
                    .root_session_id
                    .clone()
                    .unwrap_or_else(|| decision.session_id.clone()),
                decision.session_id.clone(),
                None,
                &principal,
                &role,
                "plan.approved",
                None,
                Some(serde_json::json!({
                    "plan_id": plan_id,
                    "version": version,
                    "approved_by": decision.decided_by,
                })),
                refs,
            );
            if let Err(e) = store.create_live_digest_event(&event) {
                tracing::debug!(
                    target: "session_timeline",
                    error = %e,
                    "plan.approved timeline emit failed"
                );
            }
        }
    }

    // ── 1d. Egress declassification grant materialization (RFC §8) ───
    if let ScheduledAction::EgressDeclassify {
        target,
        allowed_sink,
        reason,
        ..
    } = &decision.action
    {
        if decision.status == ApprovalStatus::Approved {
            if let Some(root_sid) = &decision.root_session_id {
                let scope = options
                    .grant_scope
                    .clone()
                    .unwrap_or(GrantScope::RootSession);
                let expires_at = declass_grant_expiry(config, options, &decision.decided_at);
                match store.insert_egress_declassification_grant(
                    root_sid,
                    &decision.session_id,
                    &decision.agent_id,
                    target,
                    *allowed_sink,
                    &scope,
                    &decision.decided_by,
                    &decision.decided_at,
                    Some(&decision.request_id),
                    expires_at.as_deref(),
                ) {
                    Err(e) => {
                        tracing::warn!(
                            target: "approval",
                            request_id = %decision.request_id,
                            error = %e,
                            "Failed to insert egress declassification grant"
                        );
                    }
                    Ok(()) => {
                        crate::runtime::egress_labeler::emit_declassified(
                            store,
                            &decision.session_id,
                            &decision.agent_id,
                            target,
                            *allowed_sink,
                            scope,
                            Some(&decision.request_id),
                            reason,
                            expires_at.as_deref(),
                        );
                        // Workspace target (#1001): releasing the workspace is a
                        // one-shot — the approved grant deletes the durable
                        // label. A later exec that resolves restricted re-narrows
                        // it, which is the ratchet the issue names: clearing
                        // goes through operator approval only, never through a
                        // later session's actions.
                        if let autonoetic_types::egress::EgressDeclassificationTarget::Workspace(
                            agent_id,
                        ) = target
                        {
                            if let Err(e) = store.delete_workspace_egress_label(agent_id) {
                                tracing::warn!(
                                    target: "approval",
                                    request_id = %decision.request_id,
                                    error = %e,
                                    agent_id = %agent_id,
                                    "failed to clear workspace egress label after \
                                     approved declassification"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // ── 2. Linked escalation resolution (approve + reject) ──────────────
    if matches!(
        decision.status,
        ApprovalStatus::Approved | ApprovalStatus::Rejected
    ) {
        // Collect the escalation_id (if any) linked to this approval. Two
        // shapes carry one:
        //   - legacy SessionEscalate{PromotionReview}: escalation_id lives in
        //     the action payload, gated by `is_promotion_review`.
        //   - merged RevisionPromote with federation_context (#738 single
        //     decision): escalation_id lives in the payload too. This path
        //     must also resolve its linked escalation projection or the
        //     unseeded→seeded lifecycle (escalate-before-install) breaks: the
        //     operator approves the merged approval, but the projection stays
        //     Pending and the later real-revision promote cannot find an
        //     Approved escalation under the artifact-scoped fallbacks.
        let linked_escalation_id: Option<&str> = match &decision.action {
            ScheduledAction::SessionEscalate { kind, payload, .. } => {
                // Backward-compat: approvals created before EscalationKind existed
                // deserialize with the default kind (GuidanceRequest) but carry the
                // legacy `payload.type == "promotion_review"` marker. Honor that too
                // so in-flight approvals still resolve their linked escalation and
                // don't reintroduce the orphaned-row hazard (#724 Part B review).
                let is_promotion_review = *kind
                    == autonoetic_types::background::EscalationKind::PromotionReview
                    || payload
                        .as_ref()
                        .and_then(|p| p.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("promotion_review");
                if is_promotion_review {
                    payload
                        .as_ref()
                        .and_then(|p| p.get("escalation_id"))
                        .and_then(|v| v.as_str())
                } else {
                    None
                }
            }
            ScheduledAction::RevisionPromote {
                federation_context: Some(_),
                payload,
                ..
            } => payload
                .as_ref()
                .and_then(|p| p.get("escalation_id"))
                .and_then(|v| v.as_str()),
            _ => None,
        };
        // #1094: an escalation may also be linked purely via its projection's
        // `approval_request_id` — the bare-promote-first ordering, where the
        // merged path reused an existing (pending) `RevisionPromote` ask
        // instead of minting a second card. The operator's decision on that
        // approval must resolve the linked projection exactly like the
        // payload-linked case above, or the escalation stays Pending forever
        // and the later promote can never find an Approved escalation.
        let mut linked_escalation_ids: Vec<String> = Vec::new();
        if let Some(payload_linked) = linked_escalation_id {
            linked_escalation_ids.push(payload_linked.to_string());
        }
        for id in store.find_escalation_ids_by_approval_request(&decision.request_id)? {
            if !linked_escalation_ids.contains(&id) {
                linked_escalation_ids.push(id);
            }
        }
        for esc_id in linked_escalation_ids {
            let esc_status = if decision.status == ApprovalStatus::Approved {
                autonoetic_types::escalation::EscalationStatus::Approved
            } else {
                autonoetic_types::escalation::EscalationStatus::Rejected
            };
            if let Err(e) = store.resolve_escalation(
                &esc_id,
                esc_status,
                &decision.decided_by,
                decision.reason.as_deref(),
            ) {
                // Already-resolved projections (e.g. a merged approval that
                // carries BOTH the payload link and the projection link) are
                // the expected double-link case — not an error worth a warning.
                let already_resolved = store
                    .get_escalation(&esc_id)
                    .ok()
                    .flatten()
                    .map(|esc| {
                        !matches!(
                            esc.status,
                            autonoetic_types::escalation::EscalationStatus::Pending
                                | autonoetic_types::escalation::EscalationStatus::Stale
                        )
                    })
                    .unwrap_or(false);
                if !already_resolved {
                    tracing::warn!(
                        target: "approval",
                        escalation_id = %esc_id,
                        error = %e,
                        "Failed to resolve linked escalation"
                    );
                }
            }
        }
    }

    // ── 3. Reevaluation state + background state (reject/cancel/withdraw) ──
    if !matches!(decision.status, ApprovalStatus::Approved) {
        let agent_dir = config.agents_dir.join(&decision.agent_id);
        let is_agent_withdrawal = decision.decided_by.starts_with("agent:");
        crate::runtime::reevaluation_state::persist_reevaluation_state(&agent_dir, |state| {
            state
                .open_approval_request_ids
                .retain(|existing| existing != &decision.request_id);
            state.pending_scheduled_action = None;
            let outcome_label = match decision.status {
                ApprovalStatus::Rejected => "approval_rejected",
                ApprovalStatus::Cancelled if is_agent_withdrawal => "approval_withdrawn",
                ApprovalStatus::Cancelled => "approval_cancelled",
                _ => "approval_cancelled",
            };
            state.last_outcome = Some(outcome_label.to_string());
        })?;

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
    }

    // ── 4. Write resume signal to GatewayStore ─────────────────────────
    notify_session_of_decision(store, decision, context.hook_executor);

    // ── 5. Wiki timeline event (keeps existing event names for consumers) ──
    emit_wiki_timeline(store, decision);

    // ── 6. Causal event (best-effort — must not block step 7) ──────────
    if let Ok(causal_logger) = init_gateway_causal_logger(config) {
        let mut trace_session = TraceSession::create_with_session_id(
            SessionId::from_string(decision.session_id.clone()),
            Arc::new(causal_logger),
            gateway_actor_id(),
            EventScope::Session,
        );
        let _ = trace_session.log_completed(
            "background.approval",
            Some(decision.status.as_str()),
            Some(serde_json::json!({
                "agent_id": decision.agent_id,
                "request_id": decision.request_id,
                "decided_by": decision.decided_by,
                "action_kind": decision.action.kind()
            })),
        );
    }

    // ── 7. Unblock workflow task ──────────────────────────────────────
    unblock_task_on_approval(config, Some(store), decision);

    // ── 7b. #723 fan-in: resume sibling waiters that joined this approval ──
    fan_in_approval_waiters(config, store, decision);

    Ok(())
}

/// #723: when an approval resolves, apply the same status transition to every
/// sibling task that joined it as a waiter (root-scoped, identical-action
/// dedup), then clear the waiter rows. Runs independently of the primary
/// approval's own task binding.
fn fan_in_approval_waiters(
    config: &GatewayConfig,
    store: &crate::scheduler::gateway_store::GatewayStore,
    decision: &ApprovalDecision,
) {
    use autonoetic_types::workflow::TaskRunStatus;

    let waiters = match store.list_approval_waiters(&decision.request_id) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                error = %e,
                "failed to list approval waiters for fan-in (#723)"
            );
            return;
        }
    };
    if waiters.is_empty() {
        return;
    }

    // The task transition to apply to each waiter. `Stale` carries no task
    // transition here (it is not an operator decision), but we still fall
    // through to clear the ledger below so rows never leak.
    let new_status = match decision.status {
        ApprovalStatus::Approved => Some(TaskRunStatus::Runnable),
        ApprovalStatus::Rejected | ApprovalStatus::Cancelled => Some(TaskRunStatus::Failed),
        ApprovalStatus::Stale => None,
    };

    if let Some(new_status) = new_status {
        // Preserve the Cancelled-vs-Rejected distinction in both the resume
        // payload and the failure summary, matching unblock_task_on_approval.
        let continuation_payload = serde_json::json!({
            "approval_resolved": true,
            "request_id": decision.request_id,
            "status": decision.status.as_str(),
            "joined_waiter": true,
        });
        let summary_prefix = match decision.status {
            ApprovalStatus::Cancelled => "approval_cancelled",
            _ => "approval_rejected",
        };
        for w in &waiters {
            let (Some(w_wf), Some(w_task)) = (w.workflow_id.as_deref(), w.task_id.as_deref())
            else {
                continue;
            };
            // Idempotency: don't overwrite a waiter task that already reached a
            // terminal state (e.g. its own timeout marked it Failed).
            // `Stale` is intentionally NOT terminal here — it is resumable
            // via late approval (see #722 Stage 2 / P-2.11).
            if let Ok(Some(existing)) =
                super::workflow_store::load_task_run(config, Some(store), w_wf, w_task)
            {
                if existing.status.is_terminal() {
                    continue;
                }
            }
            let summary = match (new_status, &decision.reason) {
                (TaskRunStatus::Failed, Some(r)) => Some(format!("{}: {}", summary_prefix, r)),
                _ => None,
            };
            if let Err(e) = super::workflow_store::update_task_run_status(
                config,
                Some(store),
                w_wf,
                w_task,
                new_status,
                summary,
                None,
                None,
            ) {
                tracing::warn!(
                    target: "approval",
                    request_id = %decision.request_id,
                    workflow_id = %w_wf,
                    task_id = %w_task,
                    error = %e,
                    "failed to fan-in approval waiter (#723)"
                );
                continue;
            }
            let _ = super::workflow_store::checkpoint_task(
                config,
                Some(store),
                w_wf,
                w_task,
                "approval_resolved".to_string(),
                continuation_payload.clone(),
            );
        }
    }

    // Always clear the ledger — including on Stale — so waiter rows never leak.
    if let Err(e) = store.clear_approval_waiters(&decision.request_id) {
        tracing::debug!(
            target: "approval",
            request_id = %decision.request_id,
            error = %e,
            "failed to clear approval waiters after fan-in (#723)"
        );
    }
}

/// Write approval resolution signal to the GatewayStore for scheduler delivery.
/// Under the Lawful Gate model, this merely notifies the waiting session —
/// the agent retries with an `approval_ref`.
fn notify_session_of_decision(
    store: &crate::scheduler::gateway_store::GatewayStore,
    decision: &ApprovalDecision,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
) {
    let resume = should_resume_waiting_session(decision);
    if !resume {
        tracing::info!(
            target: "approval",
            request_id = %decision.request_id,
            workflow_id = ?decision.workflow_id,
            task_id = ?decision.task_id,
            "Skipping direct session notification; workflow-bound task will continue via task dispatch"
        );
        return;
    }

    let session_id = &decision.session_id;
    if session_id.is_empty() {
        return;
    }

    let status_str = decision.status.as_str();
    let signal = super::signal::Signal::ApprovalResolved {
        request_id: decision.request_id.clone(),
        agent_id: decision.agent_id.clone(),
        status: status_str.to_string(),
        install_completed: false,
        message: format!("approval_{}:{}", status_str, decision.request_id),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };

    if let Err(e) =
        super::signal::write_signal(Some(store), session_id, &decision.request_id, &signal)
    {
        tracing::warn!(
            target: "approval",
            request_id = %decision.request_id,
            error = %e,
            "Failed to write approval signal"
        );
    }

    // Notify parent session if this is a child session
    if should_notify_parent_session(decision) {
        let parent_sid = decision.root_session_id.as_deref().unwrap_or(session_id);
        let parent_signal = super::signal::Signal::ApprovalResolved {
            request_id: decision.request_id.clone(),
            agent_id: decision.agent_id.clone(),
            status: status_str.to_string(),
            install_completed: false,
            message: format!("approval_{}:{}", status_str, decision.request_id),
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = super::signal::write_signal(
            Some(store),
            parent_sid,
            &decision.request_id,
            &parent_signal,
        ) {
            tracing::warn!(
                target: "approval",
                request_id = %decision.request_id,
                error = %e,
                "Failed to write approval signal to parent session"
            );
        }
    }

    if let Some(executor) = hook_executor {
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
}

/// Emit a wiki timeline event for WikiProposal actions. Keeps the existing
/// event names (`wiki.promoted` / `wiki.rejected` / `wiki.withdrawn`) that
/// consumers in `session_timeline.rs` and `cli/room/render.rs` already handle.
fn emit_wiki_timeline(
    store: &crate::scheduler::gateway_store::GatewayStore,
    decision: &ApprovalDecision,
) {
    let ScheduledAction::WikiProposal {
        ref page_id,
        ref title,
        ..
    } = decision.action
    else {
        return;
    };

    let is_agent_withdrawal = decision.decided_by.starts_with("agent:");
    let event_type = match decision.status {
        ApprovalStatus::Approved => "wiki.promoted",
        ApprovalStatus::Rejected => "wiki.rejected",
        ApprovalStatus::Cancelled if is_agent_withdrawal => "wiki.withdrawn",
        ApprovalStatus::Cancelled => "wiki.rejected",
        ApprovalStatus::Stale => return,
    };
    let role = crate::runtime::session_timeline::derive_role(&decision.agent_id);
    let principal = autonoetic_types::principal::Principal::agent(decision.agent_id.clone());
    let refs = autonoetic_types::session_timeline::TimelineRefs::default();
    let mut payload = serde_json::json!({
        "page_id": page_id,
        "title": title,
        "decided_by": decision.decided_by,
        "reason": decision.reason,
    });
    if is_agent_withdrawal {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "cancelled_by".into(),
                serde_json::json!(decision.decided_by),
            );
        }
    }
    let event = crate::runtime::session_timeline::build_timeline_event(
        decision
            .root_session_id
            .clone()
            .unwrap_or_else(|| decision.session_id.clone()),
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
        tracing::debug!(target: "session_timeline", error = %e, "{} timeline emit failed", event_type);
    }
}

/// One decision per request: fail if this request already carries a *terminal*
/// one.
///
/// Decidable states are **no status** (pending — `ApprovalStatus` has no
/// `Pending` variant, pendingness is the absence of a status) and
/// [`ApprovalStatus::Stale`].
///
/// `Stale` must pass. An expired approval is suspended awaiting the operator,
/// not concluded: `execution.rs` re-suspends the session "until operator
/// resolves", and the store's `record_decision` deliberately accepts it —
/// `WHERE request_id = ?6 AND status IN ('pending', 'stale')`, with an error
/// message that names both. Rejecting `Stale` here contradicted both and left
/// no way to resolve one: nothing anywhere resets `stale` back to `pending`, so
/// the request could be neither approved nor rejected and its session
/// re-suspended forever. (Raised in the #1047 review; the pre-existing guards
/// this helper replaced all had the same hole.)
///
/// Deliberately an error rather than a silent success for terminal states. A
/// second decision on the same `request_id` means a caller double-submitted, a
/// waiter serviced a record another waiter had already resolved, or a retry fired
/// after the first attempt actually succeeded — all worth surfacing, and none of
/// them safe to report as a fresh approval. Every caller must run this *before*
/// any side effect, since returning the error afterwards does not undo the writes.
fn ensure_decidable(request: &autonoetic_types::background::ApprovalRequest) -> anyhow::Result<()> {
    match request.status {
        None | Some(ApprovalStatus::Stale) => Ok(()),
        Some(ref terminal) => anyhow::bail!(
            "Approval {} already decided as '{}' (by {})",
            request.request_id,
            terminal.as_str(),
            request.decided_by.as_deref().unwrap_or("unknown")
        ),
    }
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

    // Idempotency, checked *before* any side effect (#1047).
    //
    // `decide_request_with_options` performs the same check, but it runs at the
    // end of this function — after the vault write, the credential upsert and
    // wiki materialization. A duplicate approval therefore re-ran every one of
    // those and only then failed, which is how one operator response produced
    // five "Stored secrets and created credential record" entries 3 seconds
    // apart for the same `request_id`. The downstream check stays as the
    // backstop for `reject`/`cancel`; this one is what makes the effects
    // unreachable.
    ensure_decidable(&req)?;

    // Same defect, second instance: the §O motivation obligation was also only
    // enforced inside `decide_request_with_options`, so an approval submitted
    // without a reason banked the vault write and credential upsert and *then*
    // refused — inviting exactly the retry loop that produced the duplicates.
    // `enforce_decider_motivation` is pure, so pre-flighting it costs nothing.
    //
    // The refusal must still be recorded here. Contract-health tallies
    // `decider_obligation.refused` (see `emit_decider_obligation_event`), and the
    // downstream call emitted it on this path; returning early without emitting
    // would drop the O-1 refusal from the ledger. The *satisfied* emission stays
    // downstream, so a successful decision is still recorded exactly once.
    if let Err(e) = enforce_decider_motivation(
        config,
        &req,
        decided_by,
        &ApprovalStatus::Approved,
        reason.as_deref(),
    ) {
        emit_decider_obligation_event(
            Some(store),
            &req,
            decided_by,
            &ApprovalStatus::Approved,
            "refused",
        );
        return Err(e);
    }

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

        // Store secrets in vault — fail-closed, require VAULT_PATH.
        // Fall back to the config's agents_dir when the env var is unset
        // (the normal case for approvals arriving via the TUI; credential_setup
        // resolves the vault path at tool-execution time but does not set the
        // env var, so the approval handler needs the fallback).
        let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
            .ok()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| crate::vault::default_vault_path(&config.agents_dir));
        // Ensure the vault key is available (credential_setup already called
        // this, but the approval handler may run in a context where the env var
        // was cleared or unreachable — the call is idempotent/nop if already set).
        let _ = crate::vault::ensure_default_key(&config.agents_dir);
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

        // Multi-field prompts also store a combined flat JSON object under the
        // credential id so spawn-time env injection delivers every field (the
        // record points at the combined object; single-field prompts keep the
        // raw-value contract under the field name).
        let collected: Vec<(String, String)> = secret_fields
            .iter()
            .filter_map(|f| {
                secret_pairs
                    .iter()
                    .find(|(name, _)| name == &f.name)
                    .map(|(_, v)| (f.name.clone(), v.clone()))
            })
            .collect();
        let record_secret_name =
            crate::runtime::tools::credential::store_collected_secret_values(
                &mut vault,
                credential_id,
                &collected,
            );
        vault.persist_to_file(&vault_path)?;

        // Extract the setup label from the payload so dedup stays scoped to
        // the (service, label) pair the caller declared.
        let label = payload
            .as_ref()
            .and_then(|p| p.get("label"))
            .and_then(|v| v.as_str().map(String::from));

        // Default `inject_as` to the service-derived env var when the flow did
        // not pass one, matching the credential_setup completion path.
        let inject_as = inject_as
            .or_else(|| Some(autonoetic_types::runtime_lock::inject_as_for_service(service)));

        // Create the CredentialRecord with full metadata
        let cred = autonoetic_types::agent::CredentialRecord {
            credential_id: credential_id.clone(),
            service: service.clone(),
            secret_name: record_secret_name,
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
            label,
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
                anyhow::anyhow!(
                    "Failed to read wiki index '{}': {}",
                    index_path.display(),
                    e
                )
            })?;
            let parsed: toml::Value = index_content.parse().map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse wiki index '{}': {}",
                    index_path.display(),
                    e
                )
            })?;
            parsed
                .get("pages")
                .and_then(|p| p.as_array().cloned())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let entry = toml::Value::Table({
            let mut m = toml::map::Map::new();
            m.insert("id".to_string(), toml::Value::String(page_id.clone()));
            m.insert("title".to_string(), toml::Value::String(title.clone()));
            m.insert(
                "file".to_string(),
                toml::Value::String(format!("{}.md", page_id)),
            );
            m.insert(
                "tags".to_string(),
                toml::Value::Array(
                    tags.iter()
                        .map(|t| toml::Value::String(t.clone()))
                        .collect(),
                ),
            );
            m
        });
        if let Some(pos) = index
            .iter()
            .position(|e| e.get("id").and_then(|v| v.as_str()) == Some(page_id.as_str()))
        {
            index[pos] = entry;
        } else {
            index.push(entry);
        }
        let index_content = toml::Value::Table({
            let mut m = toml::map::Map::new();
            m.insert("pages".to_string(), toml::Value::Array(index));
            m
        });
        let toml_str = toml::to_string(&index_content)?;
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
        options.clone(),
    )?;

    let context = DecisionContext {
        wiki_materialized_meta: wiki_materialized.clone(),
        hook_executor,
    };
    apply_decision(config, gateway_store, &decision, &options, &context)?;

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
    reject_request_with_options(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason,
        hook_executor,
        ApproveOptions::default(),
    )
}

pub fn reject_request_with_options(
    config: &GatewayConfig,
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request_id: &str,
    decided_by: &str,
    reason: Option<String>,
    hook_executor: Option<&crate::scheduler::hooks::HookExecutor>,
    options: ApproveOptions,
) -> anyhow::Result<ApprovalDecision> {
    let decision = decide_request_with_options(
        config,
        gateway_store,
        request_id,
        decided_by,
        reason,
        ApprovalStatus::Rejected,
        options.clone(),
    )?;

    let context = DecisionContext {
        wiki_materialized_meta: None,
        hook_executor,
    };
    apply_decision(config, gateway_store, &decision, &options, &context)?;

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

    let context = DecisionContext {
        wiki_materialized_meta: None,
        hook_executor,
    };
    apply_decision(
        config,
        gateway_store,
        &decision,
        &ApproveOptions::default(),
        &context,
    )?;

    Ok(decision)
}

/// Withdraw a still-pending approval bound to a workflow task (e.g. task cancelled).
pub fn cancel_pending_approval_for_workflow_task(
    config: &GatewayConfig,
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
    let decision = cancel_approval_request(
        config,
        gateway_store,
        &request_id,
        cancelled_by,
        Some(reason.to_string()),
    )?;

    let context = DecisionContext {
        wiki_materialized_meta: None,
        hook_executor: None,
    };
    apply_decision(
        config,
        gateway_store,
        &decision,
        &ApproveOptions::default(),
        &context,
    )?;

    Ok(Some(request_id))
}

pub(crate) fn cancel_approval_request(
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

    // Idempotency guard — already ahead of this function's writes.
    ensure_decidable(&request)?;

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

    Ok(decision)
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

pub(crate) fn should_resume_waiting_session(decision: &ApprovalDecision) -> bool {
    if matches!(decision.action, ScheduledAction::PlanFrame { .. }) {
        return false;
    }
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

    // Idempotency guard: if the task is already in a terminal state (e.g.
    // cancel_pending_approval_for_workflow_task is called after the task was
    // already marked Cancelled), don't overwrite it.
    if let Ok(Some(existing)) =
        super::workflow_store::load_task_run(config, gateway_store, wf_id, t_id)
    {
        use autonoetic_types::workflow::TaskRunStatus;
        if existing.status.is_terminal() {
            tracing::debug!(
                target: "approval",
                workflow_id = %wf_id,
                task_id = %t_id,
                current_status = ?existing.status,
                "Task already in terminal state, skipping unblock"
            );
            return;
        }
    }

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
        ApprovalStatus::Stale => return,
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
        (autonoetic_types::workflow::TaskRunStatus::Failed, Some(r)) => Some(format!(
            "approval_{}: {}",
            approval_event_type
                .strip_prefix("task.")
                .unwrap_or("rejected"),
            r
        )),
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
        ApprovalStatus::Stale => false,
    }
}

/// Enforce the §O decider obligation: refuse a BLOCKING-tier decision with no
/// motivation. Presence-only check (never judges the reason's quality —
/// Lawful Executor). Disabled via `decider_obligations.enabled = false`.
/// Outcome of the §O (O-1) decider-motivation check. `Refused` is conveyed as
/// the `Err` of [`enforce_decider_motivation`]; the `Ok` variants distinguish a
/// BLOCKING decision that satisfied its duty from one the obligation doesn't
/// apply to (so the caller only emits a `satisfied` event for the former).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeciderObligationOutcome {
    /// Not a BLOCKING-tier decision (or obligations disabled) — no duty owed.
    NotApplicable,
    /// BLOCKING decision that carried the required motivation.
    Satisfied,
}

fn enforce_decider_motivation(
    config: &GatewayConfig,
    request: &ApprovalRequest,
    decided_by: &str,
    status: &ApprovalStatus,
    reason: Option<&str>,
) -> anyhow::Result<DeciderObligationOutcome> {
    if !config.decider_obligations.enabled {
        return Ok(DeciderObligationOutcome::NotApplicable);
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
        return Ok(DeciderObligationOutcome::Satisfied);
    }
    Ok(DeciderObligationOutcome::NotApplicable)
}

/// Best-effort drift signal when an approved grant host is outside the
/// gateway-persisted install-time host contract (NULL contract = unconstrained).
fn emit_host_contract_drift_events(
    store: &crate::scheduler::gateway_store::GatewayStore,
    agent_id: &str,
    session_id: &str,
    hosts: &[String],
) {
    let revision = resolve_revision_for_host_contract(store, agent_id, session_id);
    let Some(revision) = revision else {
        return;
    };
    let contract = revision.detected_network_hosts.as_deref();
    for host in hosts {
        if !crate::runtime::network_host_contract::host_outside_revision_contract(contract, host) {
            continue;
        }
        let now = chrono::Utc::now();
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: uuid::Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: now.timestamp_millis().max(0) as u64,
            timestamp: now.to_rfc3339(),
            category: "host_contract".to_string(),
            action: "host_outside_revision_contract".to_string(),
            status: "warning".to_string(),
            enforced_rules: vec![],
            target: Some(host.clone()),
            payload: Some(
                serde_json::json!({
                    "host": host,
                    "revision_id": revision.revision_id,
                    "detected_network_hosts": revision.detected_network_hosts,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some(format!(
                "Approved grant host `{host}` is outside revision install-time detected_network_hosts contract"
            )),
        };
        let _ = store.create_causal_event(&event);
    }
}

fn resolve_revision_for_host_contract(
    store: &crate::scheduler::gateway_store::GatewayStore,
    agent_id: &str,
    session_id: &str,
) -> Option<autonoetic_types::agent_revision::AgentRevisionRecord> {
    if let Ok(Some(binding)) = store.get_session_agent_binding(session_id) {
        if let Ok(Some(revision)) = store.get_agent_revision(&binding.revision_id) {
            return Some(revision);
        }
    }
    let alias = store.resolve_alias(agent_id).ok().flatten()?;
    store.get_agent_revision(&alias.revision_id).ok().flatten()
}

/// Emit an `O-1`-tagged causal event recording the §O motivation-obligation
/// outcome (`decider_obligation.refused` / `…satisfied`), so contract-health
/// (`enforcement_register::contract_health`) attributes it to clause O-1.
/// Best-effort: a store/emit failure must not change the decision outcome.
fn emit_decider_obligation_event(
    gateway_store: Option<&crate::scheduler::gateway_store::GatewayStore>,
    request: &ApprovalRequest,
    decided_by: &str,
    status: &ApprovalStatus,
    action: &str,
) {
    let Some(store) = gateway_store else {
        return;
    };
    let now = chrono::Utc::now();
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: request.agent_id.clone(),
        session_id: request.session_id.clone(),
        turn_id: None,
        event_seq: now.timestamp_millis().max(0) as u64,
        timestamp: now.to_rfc3339(),
        category: "decider_obligation".to_string(),
        action: action.to_string(),
        status: if action == "refused" {
            "error"
        } else {
            "success"
        }
        .to_string(),
        enforced_rules: vec!["O-1".to_string()],
        target: Some(request.request_id.clone()),
        payload: Some(
            serde_json::json!({
                "request_id": request.request_id,
                "approval_level": request.approval_level.to_config(),
                "status": status.as_str(),
                "decided_by": decided_by,
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: Some(format!("§O (O-1) decider motivation {action}")),
    };
    let _ = store.create_causal_event(&event);
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

    // Idempotency guard: reject duplicate decisions. Callers that perform side
    // effects before deciding must check this themselves first — see
    // `ensure_decidable`.
    ensure_decidable(&request)?;

    // P-2.20 / R-10.7: agent-decider capability and spawn-tree boundary check.
    // Only a `decided_by` that never claimed to be an agent takes the human
    // path — `parse_agent_decider_id` returning `None`. Once the caller has
    // claimed `agent:<id>`, the claim is load-bearing and must be verified or
    // refused; it is never downgraded to a human decision (#1192).
    if let Some(store) = gateway_store {
        if let Some(agent_id) = parse_agent_decider_id(decided_by) {
            let gateway_dir = crate::execution::gateway_root_dir(config);
            let repo = crate::agent::repository::AgentRepository::from_config(config);
            match repo.get_sync_from_store(agent_id, &gateway_dir, Some(store)) {
                Ok(loaded) => {
                    let policy = crate::policy::PolicyEngine::new(loaded.manifest.clone());
                    let kind_label =
                        if matches!(request.action, ScheduledAction::SessionEscalate { .. }) {
                            "escalation"
                        } else {
                            "approval"
                        };
                    if !policy.can_decide_gate(kind_label).is_allowed() {
                        anyhow::bail!(
                            "Agent '{}' lacks GateDecider capability for {} gates (P-2.20)",
                            agent_id,
                            kind_label
                        );
                    }

                    // R-10.7: authenticate the caller-supplied decider session
                    // against the recorded owner, then ensure it is not in the
                    // spawn tree of the gate's session.
                    let decider_sid = options.decider_session_id.as_deref().unwrap_or("");
                    crate::runtime::human_gate::verify_decider_session_binding(
                        decider_sid,
                        &loaded.manifest.agent.id,
                        &request.session_id,
                        store,
                    )?;

                    // Emit P-2.20 causal event for the verified agent-decider decision.
                    let event = autonoetic_types::causal_chain::CausalEventRecord {
                        event_id: uuid::Uuid::new_v4().to_string(),
                        agent_id: loaded.manifest.agent.id.clone(),
                        session_id: request.session_id.clone(),
                        turn_id: None,
                        event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        category: "background.approval".to_string(),
                        action: format!("agent_decider.{}_gate", kind_label),
                        status: status.as_str().to_string(),
                        enforced_rules: vec!["P-2.20".to_string()],
                        target: Some(request.request_id.clone()),
                        payload: Some(
                            serde_json::json!({
                                "request_id": request.request_id,
                                "decided_by": decided_by,
                                "agent_id": loaded.manifest.agent.id,
                                "gate_kind": kind_label,
                            })
                            .to_string(),
                        ),
                        payload_ref: None,
                        evidence_ref: None,
                        reason: reason.clone(),
                    };
                    let _ = store.create_causal_event(&event);
                }
                Err(e) => {
                    // #1192: previously this logged at debug and fell through,
                    // committing the decision with P-2.20, R-10.7 and the
                    // `agent_decider.*_gate` causal event all skipped — so the
                    // ruling did not even surface as an agent decision in
                    // contract health. Enforcement must not be conditional on a
                    // lookup succeeding: an unresolvable agent stops being
                    // *checked* rather than starting to be *refused*, which is
                    // the wrong failure direction for a capability gate.
                    tracing::warn!(
                        target: "approval",
                        decided_by = %decided_by,
                        error = %e,
                        "Refusing agent-decider decision: claimed agent does not resolve to a loaded manifest"
                    );
                    anyhow::bail!(
                        "Decider '{}' claims agent identity '{}', which does not resolve to a \
                         loaded agent manifest ({}); the GateDecider capability cannot be \
                         verified, so the decision is refused (P-2.20)",
                        decided_by,
                        agent_id,
                        e
                    );
                }
            }
        }
    }

    // §O symmetric obligation (#359 / #395): a principal decider owes a
    // motivation for a BLOCKING-tier decision (reject/abort, or approval of an
    // elevated-authority / external-irreversible action). Checked before commit.
    // Either outcome is recorded as an O-1-tagged causal event (#399) so
    // contract-health can attribute it.
    match enforce_decider_motivation(config, &request, decided_by, &status, reason.as_deref()) {
        Ok(DeciderObligationOutcome::Satisfied) => {
            emit_decider_obligation_event(
                gateway_store,
                &request,
                decided_by,
                &status,
                "satisfied",
            );
        }
        Ok(DeciderObligationOutcome::NotApplicable) => {}
        Err(e) => {
            emit_decider_obligation_event(gateway_store, &request, decided_by, &status, "refused");
            return Err(e);
        }
    }

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

    Ok(decision)
}

/// Parse a `decided_by` string into an agent ID when the decider is an agent.
///
/// Only the canonical `agent:<agent_id>` form is recognized; anything else is
/// treated as a human operator. The resolved `agent_id` is then used to load a
/// manifest for the `GateDecider` capability check (P-2.20).
fn parse_agent_decider_id(decided_by: &str) -> Option<&str> {
    let trimmed = decided_by.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(id) = trimmed.strip_prefix("agent:") {
        let id = id.trim();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        approve_request_with_options, should_notify_parent_session, should_resume_waiting_session,
        ApproveOptions,
    };
    use crate::scheduler::workflow_store::{
        ensure_workflow_for_root_session, load_task_run, save_task_run,
    };
    use autonoetic_types::background::{
        ApprovalDecision, ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
    };
    use autonoetic_types::config::GatewayConfig;
    use autonoetic_types::notification::NotificationType;
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
    use tempfile::tempdir;

    /// #1047: a duplicate approval must not re-run the side effects.
    ///
    /// The guard already existed, but in `decide_request_with_options`, which runs
    /// *after* the vault write, the credential upsert and wiki materialization. So
    /// the second call did all of that and only then failed — which is how one
    /// operator response produced five "Stored secrets and created credential
    /// record" entries 3 seconds apart for the same `request_id`.
    ///
    /// Asserting the error alone would not catch a regression, because the error
    /// was already there. This asserts *ordering*: the second call supplies a
    /// different secret value, so if the effects still ran before the guard the
    /// vault would hold `"second"`.
    #[test]
    #[serial_test::serial] // mutates AUTONOETIC_VAULT_KEY* (process-global)
    fn duplicate_credential_approval_does_not_rewrite_the_vault() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        // `ensure_default_key` early-returns when a key var is already set and
        // otherwise exports a path into its own tempdir — so in parallel these
        // tests encrypt with each other's (already-deleted) keys. Pin an explicit
        // key, per the `vault::tests` convention.
        std::env::remove_var("AUTONOETIC_VAULT_KEY_PATH");
        std::env::set_var("AUTONOETIC_VAULT_KEY", "1".repeat(64));

        let mut request = ApprovalRequest {
            request_id: "apr-dup1047".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root-session".to_string(),
            action: ScheduledAction::CredentialPrompt {
                service: "gmail".to_string(),
                credential_id: "cred_gmail_1047".to_string(),
                message: "Gmail app password".to_string(),
                secret_fields: vec![autonoetic_types::agent::SecretFieldSpec {
                    name: "GMAIL_TOKEN".to_string(),
                    label: "Token".to_string(),
                    masked: true,
                }],
                payload: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
            expires_at: None,
        };
        store.create_approval(&mut request).unwrap();

        // Credential prompts carry an R++4 confirmation phrase, derived on
        // create; both calls supply it so the phrase gate is not what refuses
        // the duplicate.
        let phrase = store
            .get_approval(&request.request_id)
            .unwrap()
            .unwrap()
            .confirm_phrase
            .expect("credential prompt should carry a confirm phrase");

        let approve = |value: &str| {
            approve_request_with_options(
                &cfg,
                Some(&store),
                &request.request_id,
                "operator",
                Some("operator registered the gmail credential".to_string()),
                Some(vec![("GMAIL_TOKEN".to_string(), value.to_string())]),
                None,
                None,
                ApproveOptions {
                    confirm_phrase: Some(phrase.clone()),
                    ..Default::default()
                },
            )
        };

        approve("first").expect("first approval succeeds");

        let err = approve("second").expect_err("second approval must be refused");
        assert!(
            err.to_string().contains("already decided"),
            "unexpected error: {err}"
        );

        // The discriminating assertion: had the effects run before the guard,
        // this would now be "second".
        let vault_path = crate::vault::default_vault_path(&agents_dir);
        let vault = crate::vault::Vault::load_from_file(&vault_path).unwrap();
        use secrecy::ExposeSecret;
        assert_eq!(
            vault.get_secret("GMAIL_TOKEN").map(|s| s.expose_secret()),
            Some("first"),
            "a refused duplicate must not overwrite the stored secret"
        );
        std::env::remove_var("AUTONOETIC_VAULT_KEY");

        // And exactly one credential record, still from the first decision.
        let creds = store.list_all_credentials().unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].credential_id, "cred_gmail_1047");
    }

    /// The same ordering defect, second instance (#1047): the §O motivation
    /// obligation was enforced only in `decide_request_with_options`, so an
    /// approval submitted without a reason wrote the secret to the vault, upserted
    /// the credential, and *then* refused — telling the caller to "retry", which
    /// re-ran the writes. That is a plausible reading of the five duplicates in
    /// the field log: a validation failure downstream of the effects, retried.
    #[test]
    #[serial_test::serial] // mutates AUTONOETIC_VAULT_KEY* (process-global)
    fn approval_missing_motivation_does_not_write_the_vault() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        assert!(
            cfg.decider_obligations.enabled,
            "test presumes §O obligations are on by default"
        );
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        // `ensure_default_key` early-returns when a key var is already set and
        // otherwise exports a path into its own tempdir — so in parallel these
        // tests encrypt with each other's (already-deleted) keys. Pin an explicit
        // key, per the `vault::tests` convention.
        std::env::remove_var("AUTONOETIC_VAULT_KEY_PATH");
        std::env::set_var("AUTONOETIC_VAULT_KEY", "1".repeat(64));

        let mut request = ApprovalRequest {
            request_id: "apr-nomotive".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root-session".to_string(),
            action: ScheduledAction::CredentialPrompt {
                service: "gmail".to_string(),
                credential_id: "cred_gmail_nomotive".to_string(),
                message: "Gmail app password".to_string(),
                secret_fields: vec![autonoetic_types::agent::SecretFieldSpec {
                    name: "NOMOTIVE_TOKEN".to_string(),
                    label: "Token".to_string(),
                    masked: true,
                }],
                payload: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
            expires_at: None,
        };
        store.create_approval(&mut request).unwrap();
        let phrase = store
            .get_approval(&request.request_id)
            .unwrap()
            .unwrap()
            .confirm_phrase
            .expect("credential prompt should carry a confirm phrase");

        let err = approve_request_with_options(
            &cfg,
            Some(&store),
            &request.request_id,
            "operator",
            None, // no motivation
            Some(vec![("NOMOTIVE_TOKEN".to_string(), "leaked".to_string())]),
            None,
            None,
            ApproveOptions {
                confirm_phrase: Some(phrase),
                ..Default::default()
            },
        )
        .expect_err("a motivation-less approval must be refused");
        assert!(
            err.to_string().contains("decider obligation"),
            "unexpected error: {err}"
        );

        let vault_path = crate::vault::default_vault_path(&agents_dir);
        // No vault file at all, or one without the secret — either proves the
        // refusal happened before the write.
        if let Ok(vault) = crate::vault::Vault::load_from_file(&vault_path) {
            assert!(
                vault.get_secret("NOMOTIVE_TOKEN").is_none(),
                "refused approval must not have persisted the secret"
            );
        }
        assert!(
            store
                .get_credential("cred_gmail_nomotive")
                .unwrap()
                .is_none(),
            "refused approval must not have created the credential record"
        );

        // The refusal must still reach the ledger. Contract-health tallies
        // `decider_obligation.refused` by its O-1 rule id, so pre-flighting the
        // check must not swallow the event the downstream call used to emit
        // (raised in the #1047 review).
        let events = store.search_causal_events(None, None, 50).unwrap();
        let refused = events
            .iter()
            .find(|e| e.category == "decider_obligation" && e.action == "refused")
            .expect("O-1 refusal must be recorded even though we refused early");
        assert_eq!(refused.target.as_deref(), Some("apr-nomotive"));
        assert!(
            refused.enforced_rules.iter().any(|r| r == "O-1"),
            "refusal must carry its rule id for contract-health attribution"
        );
        std::env::remove_var("AUTONOETIC_VAULT_KEY");
    }

    /// A `Stale` approval must stay resolvable (#1047 review).
    ///
    /// Expiry suspends an approval awaiting the operator; it does not conclude
    /// it. `flag_expired_standalone_approvals` says so — "the approvals are NOT
    /// cancelled — they remain resolvable if the operator chooses to act" — and
    /// `record_decision` accepts `status IN ('pending', 'stale')`. The guard used
    /// to reject any non-`None` status, so a stale request could be neither
    /// approved nor rejected, and since nothing anywhere resets `stale` back to
    /// `pending`, its session re-suspended "until operator resolves" forever.
    #[test]
    fn stale_approval_remains_resolvable_by_the_operator() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();

        let mut request = ApprovalRequest {
            request_id: "apr-stale47".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "root-session".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "python3 x".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
                intent: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
            // Standalone and already expired, so the sweep flags it stale.
            expires_at: Some("2020-01-02T00:00:00Z".to_string()),
        };
        store.create_approval(&mut request).unwrap();

        let flagged = store.flag_expired_standalone_approvals().unwrap();
        assert_eq!(flagged, vec!["apr-stale47".to_string()]);
        assert_eq!(
            store
                .get_approval("apr-stale47")
                .unwrap()
                .unwrap()
                .status
                .unwrap(),
            ApprovalStatus::Stale
        );

        let decision = super::approve_request(
            &cfg,
            Some(&store),
            "apr-stale47",
            "operator",
            Some("operator resolved the expired request".to_string()),
            None,
            None,
            None,
        )
        .expect("an operator must be able to resolve a stale approval");
        assert_eq!(decision.status, ApprovalStatus::Approved);

        // And it is terminal afterwards: the duplicate guard still applies.
        let err = super::approve_request(
            &cfg,
            Some(&store),
            "apr-stale47",
            "operator",
            Some("again".to_string()),
            None,
            None,
            None,
        )
        .expect_err("resolving it once makes it terminal");
        assert!(
            err.to_string().contains("already decided as 'approved'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_approval_requests_skips_payload_companion_files() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
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
                intent: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };
        let sandbox = || ScheduledAction::SandboxExec {
            command: "x".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
            intent: None,
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
        assert!(super::enforce_decider_motivation(
            &cfg,
            &local,
            "operator",
            &ApprovalStatus::Rejected,
            None
        )
        .is_err());
        // …with a reason → allowed.
        assert!(super::enforce_decider_motivation(
            &cfg,
            &local,
            "operator",
            &ApprovalStatus::Rejected,
            Some("out of scope")
        )
        .is_ok());
        // Whitespace-only reason doesn't count.
        assert!(super::enforce_decider_motivation(
            &cfg,
            &local,
            "operator",
            &ApprovalStatus::Rejected,
            Some("   ")
        )
        .is_err());
        // Approving a reversible, operator-level action without a reason → allowed (DEFERRED).
        assert!(super::enforce_decider_motivation(
            &cfg,
            &local,
            "operator",
            &ApprovalStatus::Approved,
            None
        )
        .is_ok());
        // Mechanical decider (no principal) is exempt even on rejection.
        assert!(super::enforce_decider_motivation(
            &cfg,
            &local,
            "gateway",
            &ApprovalStatus::Rejected,
            None
        )
        .is_ok());
        assert!(super::enforce_decider_motivation(
            &cfg,
            &local,
            "emergency_stop:estop-1",
            &ApprovalStatus::Cancelled,
            None
        )
        .is_ok());
        // Approving an external/irreversible action without a reason → blocked.
        let ext = mk(ApprovalLevel::Operator, install());
        assert!(super::enforce_decider_motivation(
            &cfg,
            &ext,
            "operator",
            &ApprovalStatus::Approved,
            None
        )
        .is_err());
        // Approving an elevated-authority gate without a reason → blocked.
        let elevated = mk(ApprovalLevel::Admin, sandbox());
        assert!(super::enforce_decider_motivation(
            &cfg,
            &elevated,
            "operator",
            &ApprovalStatus::Approved,
            None
        )
        .is_err());

        // Disabled config → no enforcement at all.
        let cfg_off = GatewayConfig {
            decider_obligations: autonoetic_types::config::DeciderObligationsConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(super::enforce_decider_motivation(
            &cfg_off,
            &local,
            "operator",
            &ApprovalStatus::Rejected,
            None
        )
        .is_ok());
    }

    #[test]
    fn decider_obligation_emits_tagged_o1_event() {
        use crate::enforcement_register::contract_health;
        use crate::scheduler::gateway_store::GatewayStore;

        let tmp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(tmp.path()).unwrap();

        let request = ApprovalRequest {
            request_id: "apr-o1".to_string(),
            agent_id: "coder.default".to_string(),
            session_id: "sess-o1".to_string(),
            action: ScheduledAction::SandboxExec {
                command: "x".to_string(),
                dependencies: None,
                requires_approval: true,
                evidence_ref: None,
                detected_hosts: None,
                intent: None,
            },
            created_at: "2026-06-18T00:00:00Z".to_string(),
            reason: None,
            evidence_ref: None,
            workflow_id: None,
            task_id: None,
            root_session_id: Some("sess-o1".to_string()),
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };

        super::emit_decider_obligation_event(
            Some(&store),
            &request,
            "operator",
            &ApprovalStatus::Rejected,
            "refused",
        );

        let events = store
            .search_causal_events(Some("sess-o1"), None, 10)
            .unwrap();
        let ev = events
            .iter()
            .find(|e| e.category == "decider_obligation")
            .expect("a decider_obligation event was emitted");
        assert_eq!(ev.action, "refused");
        assert_eq!(ev.enforced_rules, vec!["O-1".to_string()]);

        // …and contract-health attributes it to clause O-1 (not unattributed).
        let health = contract_health(ev.enforced_rules.iter());
        assert_eq!(health.unattributed, 0);
        assert!(health.by_clause.contains(&("O-1".to_string(), 1)));
    }

    #[test]
    fn pending_approval_requests_for_root_filters_by_session() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
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
                intent: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
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
                intent: None,
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
                intent: None,
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
                intent: None,
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
                intent: None,
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
                intent: None,
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
            runtime_dir: gateway_dir.clone(),
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
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
            !pending
                .iter()
                .any(|n| n.notification_type == NotificationType::ApprovalResolved),
            "workflow-bound approvals should continue through workflow re-queue only"
        );
    }

    #[test]
    fn revision_promote_workflow_bound_approval_unblocks_task_without_direct_notify() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        let agent_dir = agents_dir.join("specialized_builder.default");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
            agents_dir: agents_dir.clone(),
            ..Default::default()
        };
        let store = crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap();
        let wf =
            ensure_workflow_for_root_session(&cfg, Some(&store), "demo-session", None).unwrap();

        let task = TaskRun {
            task_id: "task-promote".to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "specialized_builder.default".to_string(),
            session_id: "demo-session/specialized_builder.default-abc".to_string(),
            parent_session_id: "demo-session".to_string(),
            status: TaskRunStatus::AwaitingApproval,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("Promote after approval".to_string()),
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(&cfg, Some(&store), &task).unwrap();

        let mut request = ApprovalRequest {
            request_id: "apr-promote-wf".to_string(),
            agent_id: "specialized_builder.default".to_string(),
            session_id: task.session_id.clone(),
            action: ScheduledAction::RevisionPromote {
                agent_id: "weather-lookup".to_string(),
                revision_id: "rev_sha256:test".to_string(),
                outgoing_revision_id: String::new(),
                added_capabilities: vec!["NetworkAccess".to_string()],
                broadened_capabilities: vec![],
                payload: None,
                federation_context: None,
            },
            created_at: (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339(),
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };
        store.create_approval(&mut request).unwrap();

        super::approve_request_with_options(
            &cfg,
            Some(&store),
            &request.request_id,
            "operator",
            Some("operator approved for test".to_string()),
            None,
            None,
            None,
            ApproveOptions {
                confirm_phrase: Some("promote weather-lookup rev_sha256:test".to_string()),
                acknowledged_capabilities: vec!["NetworkAccess".to_string()],
                ..Default::default()
            },
        )
        .unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(
            !pending
                .iter()
                .any(|n| n.notification_type == NotificationType::ApprovalResolved),
            "revision_promote workflow-bound approvals should not direct-notify the waiting session"
        );

        let loaded = load_task_run(&cfg, Some(&store), &wf.workflow_id, &task.task_id)
            .unwrap()
            .expect("task run should exist after approval");
        assert_eq!(loaded.status, TaskRunStatus::Runnable);
    }

    #[test]
    fn revision_promote_approval_signal_prompts_agent_retry() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
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
                federation_context: None,
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

        super::apply_decision(
            &cfg,
            Some(store.as_ref()),
            &decision,
            &Default::default(),
            &super::DecisionContext {
                wiki_materialized_meta: None,
                hook_executor: None,
            },
        )
        .unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(!pending.is_empty(), "should have created a notification");
        let payload = &pending[0].payload;
        assert_eq!(
            payload.get("message").and_then(|v| v.as_str()),
            Some("approval_approved:apr-promote01")
        );
    }

    #[test]
    fn sandbox_approval_signal_prompts_agent_retry() {
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
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
                intent: None,
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
        super::apply_decision(
            &cfg,
            Some(store.as_ref()),
            &decision,
            &Default::default(),
            &super::DecisionContext {
                wiki_materialized_meta: None,
                hook_executor: None,
            },
        )
        .unwrap();

        let pending = store.list_pending_notifications().unwrap();
        assert!(!pending.is_empty(), "should have created a notification");
    }

    #[test]
    fn approved_workspace_declassification_clears_the_workspace_label() {
        // #1001: releasing a workspace goes through the normal EgressDeclassify
        // approval → grant machinery; the materialized grant deletes the
        // durable label (the only widening path there is).
        let dir = tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        let gateway_dir = agents_dir.join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let cfg = GatewayConfig {
            runtime_dir: gateway_dir.clone(),
            agents_dir,
            ..Default::default()
        };
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap(),
        );

        use autonoetic_types::background::ApprovalStatus;
        use autonoetic_types::egress::{
            EgressDeclassificationTarget, EgressLabel, Sink,
        };

        store
            .restrict_workspace_egress_label("coder.abc", &EgressLabel::local_only())
            .unwrap();
        assert!(store.get_workspace_egress_label("coder.abc").unwrap().is_some());

        let decision = ApprovalDecision {
            request_id: "apr-ws1".to_string(),
            agent_id: "operator".to_string(),
            session_id: "demo-session/coder.default-6738ac56".to_string(),
            action: ScheduledAction::EgressDeclassify {
                target: EgressDeclassificationTarget::Workspace("coder.abc".to_string()),
                allowed_sink: Sink::LocalModel,
                reason: "operator releases the workspace".to_string(),
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

        super::apply_decision(
            &cfg,
            Some(store.as_ref()),
            &decision,
            &Default::default(),
            &super::DecisionContext {
                wiki_materialized_meta: None,
                hook_executor: None,
            },
        )
        .unwrap();

        assert!(
            store.get_workspace_egress_label("coder.abc").unwrap().is_none(),
            "an approved workspace declassification must clear the durable label"
        );
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
            intent: None,
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
            runtime_dir: gateway_dir.clone(),
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
                intent: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
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
            runtime_dir: gateway_dir.clone(),
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
                intent: None,
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
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
