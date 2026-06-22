//! Unified HumanGate abstraction.
//!
//! Provides a single `GateService` entry point for all tool suspension needs:
//! approvals (`GateKind::Approval`), user clarifications (`GateKind::UserInput`),
//! and escalations (`GateKind::Escalation`).  The pipeline centralises the
//! "check → dedup → gate → suspend" pattern that was previously reimplemented
//! independently in 15+ tool files.
//!
//! See <docs/design/human-gate-unification-plan.md> for the full design.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
    UserInteraction, UserInteractionKind, UserInteractionOption, UserInteractionStatus,
};

use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::approved_exec_cache::ApprovedExecCacheBackfill;
use crate::runtime::content_store;
use crate::runtime::tools::{build_approval_details, extract_host};
use crate::runtime::failure_classification::normalize_tool_result_json;
use crate::scheduler::gateway_store::GatewayStore;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// What kind of gate a tool is requesting.
#[derive(Debug, Clone)]
pub enum GateKind {
    /// Operator must approve/reject a gated action.
    Approval {
        action: ScheduledAction,
        targets: Vec<String>,
        match_strategy: MatchStrategy,
    },
    /// Agent explicitly asks the user a question.
    UserInput {
        question: String,
        kind: String,
        options: Option<Vec<UserInteractionOption>>,
        allow_freeform: bool,
        context: Option<String>,
    },
    /// Operator escalation (guidance needed).
    Escalation {
        reason: String,
    },
    /// Agent proposes a wiki page addition/update.
    WikiProposal {
        page_id: String,
        title: String,
        content: String,
        tags: Vec<String>,
        is_edit: bool,
        proposed_by_agent: String,
        proposed_by_session: String,
    },
}

/// How strictly an `approval_ref` must match the current request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    /// Host extracted from URL must match (credential, web).
    HostLevel,
    /// Exact `ScheduledAction` field equality (current web behaviour).
    ExactPayload,
    /// Command string from approved action replaces current (sandbox).
    SubstituteCommand,
}

/// What a tool provides to the gate.
pub struct GateRequest<'a> {
    pub kind: GateKind,
    pub manifest: &'a AgentManifest,
    pub session_id: Option<&'a str>,
    pub run_context: Option<&'a NativeToolRunContext>,
    pub config: Option<&'a autonoetic_types::config::GatewayConfig>,
    pub reason: String,
    pub summary: String,
    pub approval_ref: Option<&'a str>,
    /// Tool-specific cache hit (e.g. `ApprovedExecCache`).  When `true` the
    /// gate short-circuits to `GateResult::Cleared`.
    pub pre_validated: bool,
    /// Optional exec-cache backfill data.  When the gate clears without a
    /// cache hit, the entry is recorded so future identical executions skip
    /// approval.
    pub cache_backfill: Option<ApprovedExecCacheBackfill>,
    /// Current turn ID (used for UserInput checkpoint tracking).
    pub turn_id: Option<&'a str>,
}

/// Unified result returned by `GateService::check`.
///
/// Every variant carries `enforced_rules` for R+++3 compliance: the
/// constitutional rule IDs that drove the decision.
#[derive(Debug)]
pub enum GateResult {
    /// Proceed — already approved / answered / granted.
    Cleared {
        source: ClearanceSource,
        enforced_rules: Vec<&'static str>,
    },
    /// A matching gate is already pending — reuse the existing gate ID.
    AlreadyPending {
        gate_id: String,
        enforced_rules: Vec<&'static str>,
    },
    /// New gate created; session should suspend.
    Suspended {
        gate_id: String,
        response_json: String,
        enforced_rules: Vec<&'static str>,
    },
    /// Policy allows without gating (no action required).
    PolicyAllowed,
}

/// Why the gate was cleared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearanceSource {
    ApprovalRef(String),
    SessionGrant,
    PreapprovedPolicy,
    CachedApproval,
    AnsweredInteraction(String),
}

impl GateResult {
    pub fn is_cleared(&self) -> bool {
        matches!(self, GateResult::Cleared { .. } | GateResult::PolicyAllowed)
    }

    /// Build the JSON string that the tool should return to the agent.
    /// Returns `None` for `Cleared` / `PolicyAllowed` — the tool should proceed
    /// normally in those cases.
    pub fn suspension_response(&self) -> Option<&str> {
        match self {
            GateResult::Suspended { response_json, .. } => Some(response_json),
            GateResult::AlreadyPending { .. } => {
                // The tool should return an approval_already_pending-style
                // response.  Consumers match on the variant directly.
                None
            }
            GateResult::Cleared { .. } | GateResult::PolicyAllowed => None,
        }
    }

    /// Constitutional rule IDs that drove this gate decision (R+++3).
    pub fn enforced_rules(&self) -> &[&'static str] {
        match self {
            GateResult::Cleared { enforced_rules, .. } => enforced_rules,
            GateResult::AlreadyPending { enforced_rules, .. } => enforced_rules,
            GateResult::Suspended { enforced_rules, .. } => enforced_rules,
            GateResult::PolicyAllowed => &[],
        }
    }
}

// ---------------------------------------------------------------------------
// Enrichment message (gate_messages)
// ---------------------------------------------------------------------------

/// A single message in a gate's enrichment thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateMessage {
    pub id: i64,
    pub gate_id: String,
    pub sender: String,
    pub content: String,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// GateService
// ---------------------------------------------------------------------------

/// Decider-agnostic gate service.  Today the decider is always a human
/// operator, but the interface is designed to support future deciders
/// (autonomous agents, policy engines, webhooks).
pub struct GateService {
    store: Arc<GatewayStore>,
}

impl GateService {
    pub fn new(store: Arc<GatewayStore>) -> Self {
        Self { store }
    }

    // -----------------------------------------------------------------------
    // Main entry point
    // -----------------------------------------------------------------------

    /// Run the unified gate pipeline.
    ///
    /// Returns `GateResult::Cleared` when the tool may proceed,
    /// `GateResult::AlreadyPending` when a matching gate already exists,
    /// or `GateResult::Suspended` when a new gate was created and the tool
    /// must suspend execution.
    pub fn check(&self, req: GateRequest<'_>) -> Result<GateResult> {
        match &req.kind {
            GateKind::Approval {
                action,
                targets,
                match_strategy,
            } => self.check_approval(&req, action, targets, *match_strategy),
            GateKind::UserInput { .. } => self.check_user_input(&req),
            GateKind::Escalation { .. } => self.check_escalation(&req),
            GateKind::WikiProposal { .. } => self.check_wiki_proposal(&req),
        }
    }

    // -----------------------------------------------------------------------
    // Approval pipeline
    // -----------------------------------------------------------------------

    fn check_approval(
        &self,
        req: &GateRequest<'_>,
        action: &ScheduledAction,
        targets: &[String],
        match_strategy: MatchStrategy,
    ) -> Result<GateResult> {
        // 1. Pre-validated bypass (e.g. ApprovedExecCache hit).
        if req.pre_validated {
            return Ok(GateResult::Cleared {
                source: ClearanceSource::CachedApproval,
                enforced_rules: vec!["P-2.6"],
            });
        }

        // 2. approval_ref validation.
        if let Some(ref_id) = req.approval_ref {
            match self.validate_approval_ref(ref_id, req, action, targets, match_strategy)? {
                Some(result) => {
                    self.maybe_backfill_exec_cache(req, &result)?;
                    return Ok(result);
                }
                None => {}
            }
        }

        // 3. Session grant coverage.
        if !targets.is_empty() {
            if let Some(sid) = req.session_id {
                if !sid.is_empty() {
                    let root_sid = content_store::root_session_id(sid);
                    if self.store.session_grants_cover_targets(root_sid, targets) {
                        let result = GateResult::Cleared {
                            source: ClearanceSource::SessionGrant,
                            enforced_rules: vec!["P-2.4"],
                        };
                        self.maybe_backfill_exec_cache(req, &result)?;
                        return Ok(result);
                    }
                }
            }
        }

        // 4. Pending dedup — avoid minting duplicate approval rows for the
        //    same session + action kind + targets.
        if let Some(sid) = req.session_id {
            if !sid.is_empty() {
                if let Some(pending_id) = self.find_pending_for_targets(sid, action, targets)? {
                    return Ok(GateResult::AlreadyPending {
                        gate_id: pending_id,
                        enforced_rules: vec!["P-2.3"],
                    });
                }
            }
        }

        // 5. Create new approval row.
        let gate_id = self.create_approval_row(req, action)?;

        // 5b. Seed enrichment thread with reason + targets.
        {
            let targets_str = if targets.is_empty() {
                String::new()
            } else {
                format!(" (targets: {})", targets.join(", "))
            };
            let seed = format!("{}{}", req.reason, targets_str);
            if !seed.trim().is_empty() {
                let _ = self.add_gate_message(&gate_id, "system", &seed);
            }
        }

        // 6. Build suspension JSON.
        let response_json = self.build_approval_suspension_json(&gate_id, req, action)?;

        Ok(GateResult::Suspended {
            gate_id,
            response_json,
            enforced_rules: vec!["P-2.1", "P-2.2", "P-2.18"],
        })
    }

    // -----------------------------------------------------------------------
    // UserInput pipeline
    // -----------------------------------------------------------------------

    fn check_user_input(&self, req: &GateRequest<'_>) -> Result<GateResult> {
        let GateKind::UserInput {
            ref question,
            ref kind,
            ref options,
            ref allow_freeform,
            ref context,
        } = req.kind
        else {
            unreachable!()
        };

        let sid = req.session_id.unwrap_or("");
        anyhow::ensure!(
            !sid.is_empty(),
            "GateKind::UserInput requires a session_id"
        );

        // Dedup: if a pending interaction already exists for this session, reuse it.
        if let Some(pending_id) = self.find_pending_user_input(sid)? {
            return Ok(GateResult::AlreadyPending {
                gate_id: pending_id,
                enforced_rules: vec!["P-2.3", "P-2.18"],
            });
        }

        let interaction_id = format!("ui-{}", &uuid::Uuid::new_v4().to_string()[..8]);

        let (root_session_id, workflow_id, task_id) = resolve_execution_context(req);

        let interaction = UserInteraction {
            interaction_id: interaction_id.clone(),
            session_id: sid.to_string(),
            root_session_id: root_session_id.unwrap_or_default(),
            agent_id: req.manifest.agent.id.clone(),
            turn_id: req.turn_id.unwrap_or("unknown").to_string(),
            kind: parse_interaction_kind(kind),
            question: question.clone(),
            context: context.clone(),
            options: options.clone().unwrap_or_default(),
            allow_freeform: *allow_freeform,
            status: UserInteractionStatus::Pending,
            answer_option_id: None,
            answer_text: None,
            answered_by: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            answered_at: None,
            expires_at: None,
            workflow_id,
            task_id,
            checkpoint_turn_id: req.turn_id.map(|t| t.to_string()),
        };

        self.store.create_user_interaction(&interaction)?;

        let _ = self.add_gate_message(&interaction_id, "system", &format!("Agent asks: {}", question));

        let response_json = serde_json::json!({
            "ok": true,
            "interaction_required": true,
            "interaction_id": interaction_id,
            "status": "awaiting_user"
        })
        .to_string();

        Ok(GateResult::Suspended {
            gate_id: interaction_id,
            response_json,
            enforced_rules: vec!["P-2.13", "P-2.18"],
        })
    }

    // -----------------------------------------------------------------------
    // Escalation pipeline
    // -----------------------------------------------------------------------

    fn check_escalation(&self, req: &GateRequest<'_>) -> Result<GateResult> {
        let GateKind::Escalation { ref reason } = req.kind else {
            unreachable!()
        };

        let sid = req.session_id.unwrap_or("");

        // Dedup: if a pending SessionEscalate already exists for this session, reuse it.
        if !sid.is_empty() {
            let escalate_action = ScheduledAction::SessionEscalate {
                session_id: sid.to_string(),
                root_session_id: content_store::root_session_id(sid).to_string(),
                requested_by_agent_id: req.manifest.agent.id.clone(),
                reason: reason.clone(),
                context: String::new(),
                urgency: "normal".to_string(),
                suggested_actions: Vec::new(),
                payload: None,
            };
            if let Some(pending_id) = self.find_pending_for_targets(sid, &escalate_action, &[])? {
                return Ok(GateResult::AlreadyPending {
                    gate_id: pending_id,
                    enforced_rules: vec!["P-2.3", "P-2.18"],
                });
            }
        }

        let root_sid = if sid.is_empty() {
            String::new()
        } else {
            content_store::root_session_id(sid).to_string()
        };
        let action = ScheduledAction::SessionEscalate {
            session_id: sid.to_string(),
            root_session_id: root_sid,
            requested_by_agent_id: req.manifest.agent.id.clone(),
            reason: reason.clone(),
            context: String::new(),
            urgency: "normal".to_string(),
            suggested_actions: Vec::new(),
            payload: None,
        };

        let gate_id = self.create_approval_row(req, &action)?;

        let _ = self.add_gate_message(&gate_id, "system", &format!("Escalation: {}", reason));

        let response_json = serde_json::json!({
            "ok": false,
            "escalation_required": true,
            "suspended": true,
            "request_id": gate_id,
            "reason": reason
        })
        .to_string();

        Ok(GateResult::Suspended {
            gate_id,
            response_json,
            enforced_rules: vec!["P-2.18"],
        })
    }

    // -----------------------------------------------------------------------
    // WikiProposal pipeline
    // -----------------------------------------------------------------------

    fn jaccard<T: std::hash::Hash + Eq>(a: &HashSet<T>, b: &HashSet<T>) -> f64 {
        if a.is_empty() && b.is_empty() {
            return 1.0;
        }
        let intersection = a.intersection(b).count() as f64;
        let union = a.union(b).count() as f64;
        if union == 0.0 {
            0.0
        } else {
            intersection / union
        }
    }

    /// Inline Jaccard duplicate detection for wiki proposals. Replaces the
    /// deleted scheduler/approval_similarity.rs helper, which was only kept
    /// alive for this advisory warning.
    fn find_similar_wiki_proposals(
        new_action: &ScheduledAction,
        candidates: &[ApprovalRequest],
        limit: usize,
        threshold: f64,
    ) -> Vec<(String, f64)> {
        let ScheduledAction::WikiProposal {
            content: new_content,
            title: new_title,
            tags: new_tags,
            ..
        } = new_action
        else {
            return Vec::new();
        };
        let new_content_set: HashSet<&str> = new_content.split_whitespace().collect();
        let new_title_set: HashSet<&str> = new_title.split_whitespace().collect();
        let new_tags_set: HashSet<&str> = new_tags.iter().map(|s| s.as_str()).collect();

        let mut scored: Vec<(String, f64)> = candidates
            .iter()
            .filter_map(|c| {
                if let ScheduledAction::WikiProposal {
                    content,
                    title,
                    tags,
                    ..
                } = &c.action
                {
                    let content_sim = Self::jaccard(
                        &new_content_set,
                        &content.split_whitespace().collect::<HashSet<&str>>(),
                    );
                    let title_sim = Self::jaccard(
                        &new_title_set,
                        &title.split_whitespace().collect::<HashSet<&str>>(),
                    );
                    let tags_sim = if new_tags_set.is_empty() && tags.is_empty() {
                        1.0
                    } else {
                        Self::jaccard(
                            &new_tags_set,
                            &tags.iter().map(|s| s.as_str()).collect::<HashSet<&str>>(),
                        )
                    };
                    let score = 0.5 * content_sim + 0.3 * title_sim + 0.2 * tags_sim;
                    if score >= threshold {
                        Some((c.request_id.clone(), score))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);
        scored
    }

    fn check_wiki_proposal(&self, req: &GateRequest<'_>) -> Result<GateResult> {
        let GateKind::WikiProposal {
            page_id,
            title,
            content,
            tags,
            is_edit,
            proposed_by_agent,
            proposed_by_session,
        } = &req.kind
        else {
            unreachable!()
        };

        // 1. Capability check.
        let has_cap = req.manifest.capabilities.iter().any(|c| {
            matches!(c, autonoetic_types::capability::Capability::WikiContribute)
        });
        if !has_cap {
            anyhow::bail!("Missing capability: WikiContribute");
        }

        // 2. Pending dedup.
        if let Some(sid) = req.session_id {
            if !sid.is_empty() {
                if let Some(pending_id) = self.find_pending_wiki_proposal(sid, page_id)? {
                    return Ok(GateResult::AlreadyPending {
                        gate_id: pending_id,
                        enforced_rules: vec!["P-2.3"],
                    });
                }
            }
        }

        // 3. Compute content SHA-256.
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let content_sha256 = Some(format!("{:x}", hasher.finalize()));

        // 4. Create approval row + timeline event.
        let action = ScheduledAction::WikiProposal {
            page_id: page_id.clone(),
            title: title.clone(),
            content: content.clone(),
            tags: tags.clone(),
            content_sha256,
            proposed_by_agent: proposed_by_agent.clone(),
            proposed_by_session: Some(proposed_by_session.clone()),
        };
        let gate_id = self.create_approval_row(req, &action)?;

        // 4b. Advisory quality heuristics + duplicate detection.
        let mut quality_warnings: Vec<String> = Vec::new();
        if let Some(cfg) = req.config {
            if cfg.wiki_proposal.quality_heuristics_enabled {
                if content.chars().count() < cfg.wiki_proposal.min_content_length {
                    quality_warnings.push(format!(
                        "Short content ({} chars, recommended minimum: {})",
                        content.chars().count(),
                        cfg.wiki_proposal.min_content_length
                    ));
                }
                let heading_count = content.lines().filter(|l| l.trim_start().starts_with('#') && l.trim_start().chars().nth(1).map_or(true, |c| c == ' ' || c == '#')).count();
                if heading_count < cfg.wiki_proposal.min_headings {
                    quality_warnings.push(format!(
                        "Few markdown headings ({} found, recommended minimum: {})",
                        heading_count, cfg.wiki_proposal.min_headings
                    ));
                }
            }
            if cfg.wiki_proposal.duplicate_detection_enabled {
                if let Ok(recent) = self.store.get_pending_approvals() {
                    let similar = Self::find_similar_wiki_proposals(
                        &action,
                        &recent,
                        3,
                        cfg.wiki_proposal.duplicate_threshold,
                    );
                    for (request_id, score) in &similar {
                        quality_warnings.push(format!(
                            "Similar to existing proposal {} (score {:.0}%)",
                            request_id,
                            score * 100.0
                        ));
                    }
                }
            }
        }

        // 5. Surface on timeline.
        {
            let role = crate::runtime::session_timeline::derive_role(&req.manifest.agent.id);
            let principal =
                autonoetic_types::principal::Principal::agent(req.manifest.agent.id.clone());
            let refs = autonoetic_types::session_timeline::TimelineRefs {
                approval_request_id: Some(gate_id.clone()),
                ..Default::default()
            };
            let event = crate::runtime::session_timeline::build_timeline_event(
                req.session_id.unwrap_or("unknown").to_string(),
                req.session_id.unwrap_or("unknown").to_string(),
                req.turn_id.map(str::to_string),
                &principal,
                &role,
                "wiki.proposed",
                None,
                Some(serde_json::json!({
                    "page_id": page_id,
                    "title": title,
                    "is_edit": is_edit,
                    "tags": tags,
                    "proposed_by_agent": proposed_by_agent,
                    "gate_id": gate_id,
                })),
                refs,
            );
            if let Err(e) = self.store.create_live_digest_event(&event) {
                tracing::debug!(target: "session_timeline", error = %e, "wiki.proposed timeline emit failed");
            }
        }

        // 6. Seed enrichment message.
        let edit_label = if *is_edit { "edit" } else { "new" };
        let seed = format!(
            "Wiki proposal ({}) — page_id: {}, title: {}, proposed by: {}",
            edit_label, page_id, title, proposed_by_agent
        );
        let _ = self.add_gate_message(&gate_id, "system", &seed);

        // 6b. Advisory quality warnings for operator.
        if !quality_warnings.is_empty() {
            let warning_text = format!("⚠ Quality advisory (advisory only, does not block):\n{}",
                quality_warnings.iter()
                    .map(|w| format!("  • {w}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let _ = self.add_gate_message(&gate_id, "system", &warning_text);
        }

        // 6. Build suspension JSON (tool will NOT suspend — it returns ok:true).
        let response_json = serde_json::json!({
            "ok": true,
            "id": page_id,
            "gate_id": gate_id,
            "is_edit": is_edit,
            "status": "pending",
            "proposed_at": chrono::Utc::now().to_rfc3339(),
        })
        .to_string();

        Ok(GateResult::Suspended {
            gate_id,
            response_json,
            enforced_rules: vec!["P-2.1"],
        })
    }

    // -----------------------------------------------------------------------
    // Helpers — approval_ref validation
    // -----------------------------------------------------------------------

    fn validate_approval_ref(
        &self,
        ref_id: &str,
        req: &GateRequest<'_>,
        action: &ScheduledAction,
        targets: &[String],
        match_strategy: MatchStrategy,
    ) -> Result<Option<GateResult>> {
        let approval = match self.store.get_approval(ref_id)? {
            Some(a) => a,
            None => return Ok(None),
        };

        // Must be approved.
        match approval.status {
            Some(ApprovalStatus::Approved) => {}
            _ => return Ok(None),
        }

        // Agent must match.
        if approval.agent_id != req.manifest.agent.id {
            return Ok(None);
        }

        // Root session must match.
        let sid = match req.session_id {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };
        let current_root = content_store::root_session_id(sid).to_string();
        let approved_root = approval
            .root_session_id
            .as_deref()
            .unwrap_or("")
            .to_string();
        if current_root != approved_root {
            return Ok(None);
        }

        // Match strategy determines whether the approval covers this request.
        let covered = match match_strategy {
            MatchStrategy::HostLevel => self.host_level_covers(&approval, targets),
            MatchStrategy::ExactPayload => self.exact_payload_covers(&approval, action),
            MatchStrategy::SubstituteCommand => {
                // Sandbox-specific: approved action's detected hosts must cover
                // the current targets.
                self.substitute_command_covers(&approval, targets)
            }
        };

        if covered {
            let rules = match match_strategy {
                MatchStrategy::HostLevel => vec!["P-2.4", "P-2.6"],
                MatchStrategy::ExactPayload => vec!["P-2.6"],
                MatchStrategy::SubstituteCommand => vec!["P-2.4", "P-2.6"],
            };
            Ok(Some(GateResult::Cleared {
                source: ClearanceSource::ApprovalRef(ref_id.to_string()),
                enforced_rules: rules,
            }))
        } else {
            Ok(None)
        }
    }

    /// Check if the approved action's hosts cover the requested targets.
    fn host_level_covers(&self, approval: &ApprovalRequest, targets: &[String]) -> bool {
        let approved_hosts = approval.action.detected_hosts().unwrap_or_default();
        if approved_hosts.is_empty() || targets.is_empty() {
            return false;
        }
        targets.iter().all(|t| {
            let t_host = extract_host_from_target(t);
            approved_hosts.iter().any(|h| {
                let a_host = extract_host_from_target(h);
                a_host == t_host
            })
        })
    }

    /// Check if the approved action exactly matches the current action.
    fn exact_payload_covers(&self, approval: &ApprovalRequest, action: &ScheduledAction) -> bool {
        // Same action kind + same serialised payload.
        approval.action.kind() == action.kind()
            && serde_json::to_string(&approval.action).unwrap_or_default()
                == serde_json::to_string(action).unwrap_or_default()
    }

    /// Check if the approved action's detected hosts cover the targets
    /// (sandbox SubstituteCommand strategy).
    fn substitute_command_covers(
        &self,
        approval: &ApprovalRequest,
        targets: &[String],
    ) -> bool {
        let approved_hosts = approval.action.detected_hosts().unwrap_or_default();
        if approved_hosts.is_empty() || targets.is_empty() {
            return false;
        }
        use std::collections::BTreeSet;
        let required: BTreeSet<String> = targets.iter().cloned().collect();
        let granted: BTreeSet<String> = approved_hosts.into_iter().collect();
        required.is_subset(&granted)
    }

    // -----------------------------------------------------------------------
    // Helpers — pending dedup
    // -----------------------------------------------------------------------

    /// Find an existing pending approval for the same session + action kind
    /// whose detected hosts overlap with the requested targets.
    fn find_pending_for_targets(
        &self,
        session_id: &str,
        action: &ScheduledAction,
        targets: &[String],
    ) -> Result<Option<String>> {
        let pending = crate::scheduler::approval::pending_approval_requests_for_session(
            &autonoetic_types::config::GatewayConfig::default(),
            Some(&self.store),
            session_id,
        )?;

        for req in &pending {
            if req.action.kind() != action.kind() {
                continue;
            }
            // If no targets specified, any pending of the same kind counts.
            if targets.is_empty() {
                return Ok(Some(req.request_id.clone()));
            }
            // Check host overlap.
            let req_hosts = req.action.detected_hosts().unwrap_or_default();
            if !req_hosts.is_empty() {
                let overlap = targets.iter().any(|t| {
                    let t_host = extract_host_from_target(t);
                    req_hosts.iter().any(|h| extract_host_from_target(h) == t_host)
                });
                if overlap {
                    return Ok(Some(req.request_id.clone()));
                }
            }
        }
        Ok(None)
    }

    /// Find an existing pending wiki proposal for the same session + page_id.
    fn find_pending_wiki_proposal(
        &self,
        session_id: &str,
        page_id: &str,
    ) -> Result<Option<String>> {
        let pending = crate::scheduler::approval::pending_approval_requests_for_session(
            &autonoetic_types::config::GatewayConfig::default(),
            Some(&self.store),
            session_id,
        )?;
        for req in &pending {
            if let ScheduledAction::WikiProposal {
                page_id: ref pid, ..
            } = &req.action
            {
                if pid == page_id {
                    return Ok(Some(req.request_id.clone()));
                }
            }
        }
        Ok(None)
    }

    /// Find an existing pending user interaction for the same session.
    fn find_pending_user_input(&self, session_id: &str) -> Result<Option<String>> {
        let pending = self.store.get_pending_interactions_for_session(session_id)?;
        if let Some(first) = pending.first() {
            return Ok(Some(first.interaction_id.clone()));
        }
        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Helpers — approval row creation
    // -----------------------------------------------------------------------

    /// Backfill the approved-exec cache when a gate clears without a cache hit.
    /// This is the single place the gate layer triggers cache backfill; the
    /// actual write implementation lives in `approved_exec_cache.rs`.
    fn maybe_backfill_exec_cache(
        &self,
        req: &GateRequest<'_>,
        result: &GateResult,
    ) -> Result<()> {
        let source = match result {
            GateResult::Cleared { source, .. } => match source {
                ClearanceSource::CachedApproval => return Ok(()),
                _ => source,
            },
            _ => return Ok(()),
        };

        let backfill = match req.cache_backfill.as_ref() {
            Some(b) => b,
            None => return Ok(()),
        };

        // Clone only when necessary: if clearance came from an approval_ref, we
        // need to override the approval_request_id field. For other clearance
        // sources the backfill is used as-is.
        let cloned = match source {
            ClearanceSource::ApprovalRef(id) => {
                let mut b = backfill.clone();
                b.approval_request_id = id.clone();
                Some(b)
            }
            _ => None,
        };
        let backfill_to_record = cloned.as_ref().unwrap_or(backfill);

        if let Err(e) = backfill_to_record.record_if_missing() {
            tracing::warn!(
                target: "human_gate",
                error = %e,
                source = ?source,
                "Failed to backfill approved exec cache"
            );
        }

        Ok(())
    }

    fn create_approval_row(
        &self,
        req: &GateRequest<'_>,
        action: &ScheduledAction,
    ) -> Result<String> {
        let sid = req.session_id.unwrap_or("");
        let (root_session_id, workflow_id, task_id) = resolve_execution_context(req);

        let request_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let mut approval_req = ApprovalRequest {
            request_id: request_id.clone(),
            agent_id: req.manifest.agent.id.clone(),
            session_id: sid.to_string(),
            root_session_id,
            workflow_id,
            task_id,
            action: action.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: None,
            decided_at: None,
            decided_by: None,
            reason: if req.reason.is_empty() {
                None
            } else {
                Some(req.reason.clone())
            },
            evidence_ref: None,
            decision_reason: None,
            approval_level: req
                .config
                .map(|cfg| {
                    crate::scheduler::approval::resolve_approval_level(cfg, action)
                })
                .unwrap_or(ApprovalLevel::Operator),
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };

        self.store.create_approval(&mut approval_req)?;

        Ok(request_id)
    }

    // -----------------------------------------------------------------------
    // Helpers — suspension JSON
    // -----------------------------------------------------------------------

    fn build_approval_suspension_json(
        &self,
        gate_id: &str,
        req: &GateRequest<'_>,
        action: &ScheduledAction,
    ) -> Result<String> {
        let kind_label = action.kind();
        let subject = serde_json::json!({
            "action": kind_label,
            "targets": match action.detected_hosts() {
                Some(hosts) => hosts,
                None => Vec::<String>::new(),
            },
        });

        let approval_details = build_approval_details(
            &ApprovalRequest {
                request_id: gate_id.to_string(),
                agent_id: req.manifest.agent.id.clone(),
                session_id: req.session_id.unwrap_or("").to_string(),
                root_session_id: None,
                workflow_id: None,
                task_id: None,
                action: action.clone(),
                created_at: String::new(),
                status: None,
                decided_at: None,
                decided_by: None,
                reason: if req.reason.is_empty() {
                    None
                } else {
                    Some(req.reason.clone())
                },
                evidence_ref: None,
                decision_reason: None,
                approval_level: ApprovalLevel::Operator,
                min_dwell_ms: None,
                confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
            },
            kind_label,
            req.summary.clone(),
            "approval_ref",
            subject,
        );

        Ok(normalize_tool_result_json(
            &serde_json::json!({
                "ok": false,
                "approval_required": true,
                "suspended": true,
                "request_id": gate_id,
                "approval": approval_details
            })
            .to_string(),
        ))
    }

    // -----------------------------------------------------------------------
    // Gate messages (enrichment thread)
    // -----------------------------------------------------------------------

    /// Add a message to a gate's enrichment thread.
    ///
    /// Content is redacted before storage (P-2.19, P-4.13 parity).
    pub fn add_gate_message(
        &self,
        gate_id: &str,
        sender: &str,
        content: &str,
    ) -> Result<i64> {
        let redacted = crate::log_redaction::redact_text_for_logs(content);
        self.store.add_gate_message(gate_id, sender, &redacted)
    }

    /// Retrieve all messages for a gate's enrichment thread.
    pub fn get_gate_messages(&self, gate_id: &str) -> Result<Vec<GateMessage>> {
        self.store.get_gate_messages(gate_id)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve (root_session_id, workflow_id, task_id) from a GateRequest.
fn resolve_execution_context(req: &GateRequest<'_>) -> (Option<String>, Option<String>, Option<String>) {
    let sid = req.session_id.unwrap_or("");
    if let Some(rc) = req.run_context {
        let root_session_id = if rc.root_session_id.is_empty() {
            None
        } else {
            Some(rc.root_session_id.clone())
        };
        (root_session_id, rc.workflow_id.clone(), rc.task_id.clone())
    } else if !sid.is_empty() {
        let root_sid = content_store::root_session_id(sid).to_string();
        (Some(root_sid), None, None)
    } else {
        (None, None, None)
    }
}

/// Parse an interaction kind string into the enum, defaulting to `Clarification`.
fn parse_interaction_kind(kind: &str) -> UserInteractionKind {
    match kind.to_lowercase().as_str() {
        "clarification" => UserInteractionKind::Clarification,
        "decision" => UserInteractionKind::Decision,
        "proposal" => UserInteractionKind::Proposal,
        "confirmation" => UserInteractionKind::Confirmation,
        "divergence_sentinel" => UserInteractionKind::DivergenceSentinel,
        _ => UserInteractionKind::Clarification,
    }
}

/// Extract the host portion from a target string (URL or bare hostname).
/// Returns the input trimmed if no URL structure is detected.
fn extract_host_from_target(target: &str) -> String {
    if target.contains("://") {
        extract_host(target).unwrap_or_else(|_| target.to_string())
    } else {
        target.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{
        AgentIdentity, AgentManifest, RuntimeDeclaration,
    };
    use autonoetic_types::capability::Capability;

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
                description: "test agent".to_string(),
            },
            capabilities: vec![],
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

    fn make_credential_request_action(url: &str) -> ScheduledAction {
        ScheduledAction::CredentialRequest {
            credential_id: format!("cred_{}", url::Url::parse(url).map(|u| u.host_str().unwrap_or("x").to_string()).unwrap_or_else(|_| "test".to_string())),
            url: url.to_string(),
            method: Some("GET".to_string()),
            headers: None,
            body: None,
            inject_secret_as: None,
            payload: None,
        }
    }

    // -----------------------------------------------------------------------
    // Basic type tests
    // -----------------------------------------------------------------------

    #[test]
    fn gate_result_is_cleared() {
        assert!(GateResult::Cleared {
            source: ClearanceSource::SessionGrant,
            enforced_rules: vec!["P-2.4"],
        }
        .is_cleared());
        assert!(GateResult::PolicyAllowed.is_cleared());
        assert!(!GateResult::AlreadyPending {
            gate_id: "apr-test".to_string(),
            enforced_rules: vec!["P-2.3"],
        }
        .is_cleared());
        assert!(!GateResult::Suspended {
            gate_id: "apr-test".to_string(),
            response_json: "{}".to_string(),
            enforced_rules: vec!["P-2.1"],
        }
        .is_cleared());
    }

    #[test]
    fn extract_host_from_target_bare_host() {
        assert_eq!(extract_host_from_target("localhost"), "localhost");
        assert_eq!(extract_host_from_target("api.example.com"), "api.example.com");
    }

    #[test]
    fn extract_host_from_target_url() {
        assert_eq!(
            extract_host_from_target("http://localhost:8080/path"),
            "localhost"
        );
        assert_eq!(
            extract_host_from_target("https://api.example.com/v1/endpoint"),
            "api.example.com"
        );
    }

    #[test]
    fn match_strategy_host_level_covers_basic() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path()).unwrap()));

        let approval = ApprovalRequest {
            request_id: "apr-test".to_string(),
            agent_id: "test-agent".to_string(),
            session_id: "ses-123".to_string(),
            root_session_id: Some("root-123".to_string()),
            workflow_id: None,
            task_id: None,
            action: ScheduledAction::WebFetch {
                url: "http://localhost:8080/api".to_string(),
                timeout_secs: None,
                max_chars: None,
                detected_hosts: Some(vec!["localhost".to_string()]),
                payload: None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            status: Some(ApprovalStatus::Approved),
            decided_at: None,
            decided_by: None,
            reason: None,
            evidence_ref: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };

        assert!(svc.host_level_covers(&approval, &["localhost".to_string()]));
        assert!(!svc.host_level_covers(&approval, &["remote.example.com".to_string()]));
    }

    #[test]
    fn exact_payload_covers_same_action() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path()).unwrap()));

        let action = make_credential_request_action("http://localhost:8080/api");
        let approval = ApprovalRequest {
            request_id: "apr-test".to_string(),
            agent_id: "test-agent".to_string(),
            session_id: "ses-123".to_string(),
            root_session_id: Some("root-123".to_string()),
            workflow_id: None,
            task_id: None,
            action: action.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: Some(ApprovalStatus::Approved),
            decided_at: None,
            decided_by: None,
            reason: None,
            evidence_ref: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };

        assert!(svc.exact_payload_covers(&approval, &action));
    }

    // -----------------------------------------------------------------------
    // Full pipeline tests
    // -----------------------------------------------------------------------

    #[test]
    fn approval_pre_validated_bypass() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path()).unwrap()));
        let manifest = test_manifest();

        let req = GateRequest {
            kind: GateKind::Approval {
                action: make_credential_request_action("http://localhost:8080/api"),
                targets: vec!["localhost".to_string()],
                match_strategy: MatchStrategy::HostLevel,
            },
            manifest: &manifest,
            session_id: Some("ses-123"),
            run_context: None,
            config: None,
            reason: "test".to_string(),
            summary: "test summary".to_string(),
            approval_ref: None,
            pre_validated: true,
            cache_backfill: None,
            turn_id: None,
        };

        let result = svc.check(req)?;
        assert!(result.is_cleared());
        match &result {
            GateResult::Cleared { source: ClearanceSource::CachedApproval, .. } => {}
            other => panic!("expected CachedApproval, got {:?}", other),
        }
        assert!(result.enforced_rules().contains(&"P-2.6"));
        Ok(())
    }

    #[test]
    fn approval_suspends_when_no_bypass() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path()).unwrap()));
        let manifest = test_manifest();

        let req = GateRequest {
            kind: GateKind::Approval {
                action: make_credential_request_action("http://localhost:8080/api"),
                targets: vec!["localhost".to_string()],
                match_strategy: MatchStrategy::HostLevel,
            },
            manifest: &manifest,
            session_id: Some("ses-123"),
            run_context: None,
            config: None,
            reason: "network access required".to_string(),
            summary: "Fetch API from localhost".to_string(),
            approval_ref: None,
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };

        let result = svc.check(req)?;
        assert!(result.enforced_rules().contains(&"P-2.1"));
        assert!(result.enforced_rules().contains(&"P-2.2"));
        assert!(result.enforced_rules().contains(&"P-2.18"));
        match result {
            GateResult::Suspended { gate_id, response_json, .. } => {
                assert!(gate_id.starts_with("apr-"));
                let json: serde_json::Value = serde_json::from_str(&response_json)?;
                assert_eq!(json["ok"], false);
                assert_eq!(json["approval_required"], true);
                assert_eq!(json["suspended"], true);
                assert_eq!(json["request_id"], gate_id);
                assert_eq!(json["failure_class"], "approval_pending");
                assert_eq!(json["retry_advice"], "wait");
                assert_eq!(json["requires_external_event"], true);
                assert_eq!(json["requires_human"], true);
            }
            other => panic!("expected Suspended, got {:?}", other),
        }
        Ok(())
    }

    #[test]
    fn approval_dedup_returns_already_pending() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let svc = GateService::new(Arc::new(GatewayStore::open(tmp.path()).unwrap()));
        let manifest = test_manifest();
        let sid = "ses-dedup-123";

        let action = make_credential_request_action("http://localhost:8080/api");

        // First request -> Suspended
        let req1 = GateRequest {
            kind: GateKind::Approval {
                action: action.clone(),
                targets: vec!["localhost".to_string()],
                match_strategy: MatchStrategy::HostLevel,
            },
            manifest: &manifest,
            session_id: Some(sid),
            run_context: None,
            config: None,
            reason: "first".to_string(),
            summary: "first".to_string(),
            approval_ref: None,
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };
        let result1 = svc.check(req1)?;
        let gate_id_1 = match &result1 {
            GateResult::Suspended { gate_id, .. } => gate_id.clone(),
            other => panic!("expected Suspended, got {:?}", other),
        };

        // Second request for same session/targets -> AlreadyPending
        let req2 = GateRequest {
            kind: GateKind::Approval {
                action: action.clone(),
                targets: vec!["localhost".to_string()],
                match_strategy: MatchStrategy::HostLevel,
            },
            manifest: &manifest,
            session_id: Some(sid),
            run_context: None,
            config: None,
            reason: "second".to_string(),
            summary: "second".to_string(),
            approval_ref: None,
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };
        let result2 = svc.check(req2)?;
        assert!(result2.enforced_rules().contains(&"P-2.3"));
        match result2 {
            GateResult::AlreadyPending { gate_id, .. } => {
                assert_eq!(gate_id, gate_id_1);
            }
            other => panic!("expected AlreadyPending, got {:?}", other),
        }
        Ok(())
    }

    #[test]
    fn approval_ref_validates_and_clears() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(tmp.path()).unwrap());
        let svc = GateService::new(store.clone());
        let manifest = test_manifest();
        let sid = "ses-ref-123";

        let action = make_credential_request_action("http://localhost:8080/api");

        // Create approval manually and approve it.
        let ref_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let mut approval = ApprovalRequest {
            request_id: ref_id.clone(),
            agent_id: manifest.agent.id.clone(),
            session_id: sid.to_string(),
            root_session_id: Some(sid.to_string()),
            workflow_id: None,
            task_id: None,
            action: action.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: None,
            decided_at: None,
            decided_by: None,
            reason: Some("test".to_string()),
            evidence_ref: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut approval)?;
        store.record_decision(&ref_id, "approved", "operator", &chrono::Utc::now().to_rfc3339(), None)?;

        // Now check with approval_ref -> should clear.
        let req = GateRequest {
            kind: GateKind::Approval {
                action: action.clone(),
                targets: vec!["localhost".to_string()],
                match_strategy: MatchStrategy::HostLevel,
            },
            manifest: &manifest,
            session_id: Some(sid),
            run_context: None,
            config: None,
            reason: "test".to_string(),
            summary: "test".to_string(),
            approval_ref: Some(&ref_id),
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };
        let result = svc.check(req)?;
        assert!(result.enforced_rules().contains(&"P-2.6"));
        match result {
            GateResult::Cleared { source: ClearanceSource::ApprovalRef(id), .. } => {
                assert_eq!(id, ref_id);
            }
            other => panic!("expected Cleared(ApprovalRef), got {:?}", other),
        }
        Ok(())
    }

    #[test]
    fn approval_ref_wrong_agent_does_not_clear() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(tmp.path()).unwrap());
        let svc = GateService::new(store.clone());
        let manifest = test_manifest();
        let sid = "ses-wrong-123";

        let action = make_credential_request_action("http://localhost:8080/api");

        // Create approval for a different agent.
        let ref_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let mut approval = ApprovalRequest {
            request_id: ref_id.clone(),
            agent_id: "other-agent".to_string(),
            session_id: sid.to_string(),
            root_session_id: Some(sid.to_string()),
            workflow_id: None,
            task_id: None,
            action: action.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: None,
            decided_at: None,
            decided_by: None,
            reason: Some("test".to_string()),
            evidence_ref: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut approval)?;
        store.record_decision(&ref_id, "approved", "operator", &chrono::Utc::now().to_rfc3339(), None)?;

        let req = GateRequest {
            kind: GateKind::Approval {
                action,
                targets: vec!["localhost".to_string()],
                match_strategy: MatchStrategy::HostLevel,
            },
            manifest: &manifest,
            session_id: Some(sid),
            run_context: None,
            config: None,
            reason: "test".to_string(),
            summary: "test".to_string(),
            approval_ref: Some(&ref_id),
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };
        let result = svc.check(req)?;
        // Should NOT clear — wrong agent.
        assert!(matches!(result, GateResult::Suspended { .. }));
        Ok(())
    }

    #[test]
    fn substitute_command_match_strategy_approval_ref_clears() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(tmp.path()).unwrap());
        let svc = GateService::new(store.clone());
        let manifest = test_manifest();
        let sid = "ses-substitute-123";

        // Approved action: a specific command that accessed api.example.com.
        let approved_action = ScheduledAction::SandboxExec {
            command: "python3 /tmp/fetch.py".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["api.example.com".to_string()]),
            intent: None,
        };

        let ref_id = format!("apr-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let mut approval = ApprovalRequest {
            request_id: ref_id.clone(),
            agent_id: manifest.agent.id.clone(),
            session_id: sid.to_string(),
            root_session_id: Some(sid.to_string()),
            workflow_id: None,
            task_id: None,
            action: approved_action.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: None,
            decided_at: None,
            decided_by: None,
            reason: Some("test".to_string()),
            evidence_ref: None,
            decision_reason: None,
            approval_level: ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        store.create_approval(&mut approval)?;
        store.record_decision(&ref_id, "approved", "operator", &chrono::Utc::now().to_rfc3339(), None)?;

        // Retry with a *different* command string but the same concrete target.
        // SubstituteCommand strategy should clear because the approved hosts cover
        // the requested targets, regardless of command string equality.
        let retry_action = ScheduledAction::SandboxExec {
            command: "python3 /tmp/wrapper.py".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["api.example.com".to_string()]),
            intent: None,
        };
        let req = GateRequest {
            kind: GateKind::Approval {
                action: retry_action,
                targets: vec!["api.example.com".to_string()],
                match_strategy: MatchStrategy::SubstituteCommand,
            },
            manifest: &manifest,
            session_id: Some(sid),
            run_context: None,
            config: None,
            reason: "test".to_string(),
            summary: "test".to_string(),
            approval_ref: Some(&ref_id),
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };
        let result = svc.check(req)?;
        assert!(result.is_cleared(), "expected SubstituteCommand approval_ref to clear");
        match result {
            GateResult::Cleared { source: ClearanceSource::ApprovalRef(id), .. } => {
                assert_eq!(id, ref_id);
            }
            other => panic!("expected Cleared(ApprovalRef), got {:?}", other),
        }
        Ok(())
    }

    #[test]
    fn gate_messages_store_and_retrieve() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(tmp.path()).unwrap());
        let svc = GateService::new(store.clone());

        let id1 = svc.add_gate_message("apr-test123", "operator", "Why does the agent need localhost?")?;
        let id2 = svc.add_gate_message("apr-test123", "system", "Agent says: API runs on localhost:9876")?;

        let msgs = svc.get_gate_messages("apr-test123")?;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].sender, "operator");
        assert_eq!(msgs[0].content, "Why does the agent need localhost?");
        assert_eq!(msgs[1].sender, "system");
        assert_eq!(msgs[1].id, id2);

        // Empty for a different gate.
        let empty = svc.get_gate_messages("apr-nonexistent")?;
        assert!(empty.is_empty());
        Ok(())
    }

    #[test]
    fn approval_auto_seeds_enrichment_message() -> Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(tmp.path()).unwrap());
        let svc = GateService::new(store.clone());
        let manifest = test_manifest();

        let req = GateRequest {
            kind: GateKind::Approval {
                action: make_credential_request_action("http://api.example.com/data"),
                targets: vec!["api.example.com".to_string()],
                match_strategy: MatchStrategy::HostLevel,
            },
            manifest: &manifest,
            session_id: Some("ses-seed-123"),
            run_context: None,
            config: None,
            reason: "API access required".to_string(),
            summary: "Fetch data".to_string(),
            approval_ref: None,
            pre_validated: false,
            cache_backfill: None,
            turn_id: None,
        };

        let result = svc.check(req)?;
        assert!(result.enforced_rules().contains(&"P-2.18"));
        let gate_id = match result {
            GateResult::Suspended { gate_id, .. } => gate_id,
            other => panic!("expected Suspended, got {:?}", other),
        };

        let msgs = svc.get_gate_messages(&gate_id)?;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].sender, "system");
        assert!(
            msgs[0].content.contains("API access required"),
            "seed message should contain the reason: {:?}",
            msgs[0].content
        );
        assert!(
            msgs[0].content.contains("api.example.com"),
            "seed message should contain the target: {:?}",
            msgs[0].content
        );
        Ok(())
    }

    #[test]
    fn session_grant_clearance_backfills_exec_cache() -> Result<()> {
        use crate::runtime::approved_exec_cache::{
            compute_fingerprint, ApprovedExecCache, ApprovedExecCacheBackfill,
        };
        use autonoetic_types::background::{GrantScope, GrantTarget};

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(tmp.path()).unwrap());
        let svc = GateService::new(store.clone());
        let manifest = test_manifest();
        let sid = "ses-cache-backfill-123";
        let root_sid = sid;
        let agent_id = manifest.agent.id.clone();

        // Seed a session grant covering api.example.com.
        store.insert_session_grant(
            root_sid,
            sid,
            &agent_id,
            &GrantScope::RootSession,
            &[GrantTarget::ExactHost("api.example.com".to_string())],
            "test",
            &chrono::Utc::now().to_rfc3339(),
            None,
            None,
        )?;

        // Build the cache backfill payload as sandbox_exec would.
        let code_content = r#"print("https://api.example.com/data")"#;
        let targets = vec!["api.example.com".to_string()];
        let fingerprint = compute_fingerprint(
            &agent_id,
            &targets,
            code_content,
            None,
            &manifest.capabilities,
        );

        let action = ScheduledAction::SandboxExec {
            command: "python3 /tmp/fetch.py".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(targets.clone()),
            intent: None,
        };
        let req = GateRequest {
            kind: GateKind::Approval {
                action,
                targets: targets.clone(),
                match_strategy: MatchStrategy::SubstituteCommand,
            },
            manifest: &manifest,
            session_id: Some(sid),
            run_context: None,
            config: None,
            reason: "network access required".to_string(),
            summary: "fetch data".to_string(),
            approval_ref: None,
            pre_validated: false,
            cache_backfill: Some(ApprovedExecCacheBackfill {
                gateway_dir: tmp.path().to_path_buf(),
                fingerprint: fingerprint.clone(),
                agent_id,
                remote_targets: targets.clone(),
                code_content: code_content.to_string(),
                approval_request_id: String::new(),
            }),
            turn_id: None,
        };

        let result = svc.check(req)?;
        assert!(
            result.is_cleared(),
            "session grant should clear the gate without creating an approval"
        );
        match result {
            GateResult::Cleared {
                source: ClearanceSource::SessionGrant,
                ..
            } => {}
            other => panic!("expected Cleared(SessionGrant), got {:?}", other),
        }

        // The cache should have been backfilled automatically.
        let cache = ApprovedExecCache::new(tmp.path())?;
        let entry = cache.find(&fingerprint).expect("cache entry was backfilled");
        assert_eq!(entry.remote_targets, targets);
        assert_eq!(entry.code_content, code_content);
        Ok(())
    }
}
