use crate::runtime::compression::{self, resolve_compression_llm_config, split_compressible_messages};
use crate::runtime::context_governor::strategies::{GovernorContext, ReductionOutcome};
use crate::runtime::content_store::{ContentStore, ContentVisibility};
use autonoetic_types::config::LlmPreset;
use autonoetic_types::plan_frame::PlanFrameSummary;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateCapsule {
    pub version: u64,
    pub session_id: String,
    pub last_update_turn: u64,
    pub objective_and_criteria: String,
    pub decisions_and_rationale: Vec<CapsuleDecision>,
    pub stable_identifiers: Vec<StableIdentifier>,
    pub open_tasks: Vec<CapsuleTask>,
    /// Summary of prior decisions that were capped out of `decisions_and_rationale`.
    /// Replaced (not appended) on each overflow, so it stays bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_decisions_summary: Option<String>,
    pub previous_version_handle: Option<String>,
    pub source_history_handle: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleDecision {
    pub turn: u64,
    pub summary: String,
    pub rationale: String,
    pub referenced_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct StableIdentifier {
    pub category: String,
    pub value: String,
    pub label: Option<String>,
    pub first_seen_turn: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleTask {
    pub description: String,
    pub status: String,
    pub added_turn: u64,
    pub completed_turn: Option<u64>,
    pub blocker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleDelta {
    pub objective_update: Option<String>,
    pub new_decisions: Vec<CapsuleDecision>,
    pub new_identifiers: Vec<StableIdentifier>,
    pub task_updates: Vec<CapsuleTaskUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CapsuleTaskUpdate {
    Add(CapsuleTask),
    Complete { description: String, turn: u64 },
    Block { description: String, blocker: String },
    Remove { description: String },
}

pub struct CapsuleStrategy {
    http_client: reqwest::Client,
    presets: HashMap<String, LlmPreset>,
    gateway_dir: Option<PathBuf>,
}

impl CapsuleStrategy {
    pub fn new(http_client: reqwest::Client, presets: HashMap<String, LlmPreset>) -> Self {
        Self {
            http_client,
            presets,
            gateway_dir: None,
        }
    }

    pub fn with_gateway_dir(mut self, dir: PathBuf) -> Self {
        self.gateway_dir = Some(dir);
        self
    }
}

fn build_delta_extraction_prompt(
    compressible_messages: &[crate::llm::Message],
    capsule_json: &str,
    plan_anchor: Option<&PlanFrameSummary>,
) -> String {
    let plan_block = match plan_anchor {
        Some(p) => render_plan_anchor_block(p),
        None => String::new(),
    };
    format!(
        r#"You are a state capsule update extractor. Given the current session state capsule and recent conversation turns, produce a structured delta describing what changed.

{plan_block}Current State Capsule (JSON):
{capsule_json}

Recent Conversation Turns:
{}
---
Respond ONLY with a JSON object matching this schema:
{{
  "objective_update": string | null,
  "new_decisions": [
    {{
      "turn": number,
      "summary": "short summary",
      "rationale": "why this decision was made",
      "referenced_ids": ["id1", "id2"]
    }}
  ],
  "new_identifiers": [
    {{
      "category": "artifact|approval|revision|content|session",
      "value": "the id value",
      "label": "human-readable label or null",
      "first_seen_turn": number
    }}
  ],
  "task_updates": [
    {{
      "type": "Add|Complete|Block|Remove",
      "description": "task description",
      "turn": number,
      "blocker": "reason if blocked or null",
      "status": "open|in_progress|blocked|completed",
      "added_turn": number,
      "completed_turn": number | null
    }}
  ]
}}"#,
        compressible_messages
            .iter()
            .map(|m| format!("[{}] {}", m.role.as_str(), m.content))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Render the "Active Plan" block that anchors the delta-extraction prompt
/// to the session's current PlanFrame. The LLM uses this as a relevance
/// lens: plan-advancing items should be captured in the resulting delta
/// (under `new_decisions` / `new_identifiers` in the delta-extraction
/// schema), while abandoned detours can be safely dropped.
fn render_plan_anchor_block(p: &PlanFrameSummary) -> String {
    let mut s = String::from("Active Plan (use as a relevance lens — prefer plan-advancing items):\n");
    s.push_str(&format!(
        "- plan_id: {} v{} (status: {})\n",
        p.plan_id, p.version, p.status.as_str()
    ));
    if !p.title.is_empty() {
        s.push_str(&format!("- title: {}\n", p.title));
    }
    if p.step_count > 0 {
        s.push_str(&format!("- step_count: {}\n", p.step_count));
    }
    if !p.operator_steps.is_empty() {
        s.push_str("- operator/shared steps: ");
        s.push_str(&p.operator_steps.join(", "));
        s.push('\n');
    }
    if !p.agent_steps.is_empty() {
        s.push_str("- agent steps: ");
        s.push_str(&p.agent_steps.join(", "));
        s.push('\n');
    }
    if !p.required_validations.is_empty() {
        s.push_str("- required validations: ");
        s.push_str(&p.required_validations.join(", "));
        s.push('\n');
    }
    if !p.advisory_validations.is_empty() {
        s.push_str("- advisory validations: ");
        s.push_str(&p.advisory_validations.join(", "));
        s.push('\n');
    }
    s.push('\n');
    s
}

fn compile_capsule_injection(capsule: &StateCapsule) -> String {
    let mut out = format!(
        "[SESSION STATE CAPSULE v{} \u{2014} Turn {}]\n\n## Objective\n{}\n\n",
        capsule.version, capsule.last_update_turn, capsule.objective_and_criteria
    );

    if !capsule.decisions_and_rationale.is_empty() {
        out.push_str("## Key Decisions\n");
        for d in &capsule.decisions_and_rationale {
            out.push_str(&format!("- [Turn {}] {}: {}\n", d.turn, d.summary, d.rationale));
        }
        out.push('\n');
    }

    if let Some(ref prior) = capsule.prior_decisions_summary {
        out.push_str("## Prior Decisions (Summarized)\n");
        out.push_str(prior);
        out.push_str("\n\n");
    }

    if !capsule.stable_identifiers.is_empty() {
        out.push_str("## Active Identifiers\n");
        for id in &capsule.stable_identifiers {
            let label = id.label.as_deref().unwrap_or("");
            out.push_str(&format!("- [{}] {} ({})\n", id.category, id.value, label));
        }
        out.push('\n');
    }

    let open: Vec<_> = capsule.open_tasks.iter().filter(|t| t.status != "completed").collect();
    let completed: Vec<_> = capsule.open_tasks.iter().filter(|t| t.status == "completed").collect();

    if !open.is_empty() {
        out.push_str("## Open Tasks\n");
        for t in &open {
            out.push_str(&format!("- [{}] {}\n", t.status, t.description));
        }
        out.push('\n');
    }

    if !completed.is_empty() {
        out.push_str("## Completed Tasks (Recent)\n");
        for t in &completed {
            if let Some(ct) = t.completed_turn {
                out.push_str(&format!("- [done@{}] {}\n", ct, t.description));
            }
        }
        out.push('\n');
    }

    out
}

fn validate_delta_approvals(delta: &CapsuleDelta, _turn_number: u64) -> anyhow::Result<()> {
    for decision in &delta.new_decisions {
        for rid in &decision.referenced_ids {
            if rid.starts_with("appr_") {
                let preserved = delta.new_identifiers.iter().any(|id| id.value == *rid);
                if !preserved {
                    anyhow::bail!(
                        "Approval ID '{}' referenced in decision but not preserved in stable_identifiers",
                        rid
                    );
                }
            }
        }
    }
    Ok(())
}

fn apply_delta(
    capsule: &mut StateCapsule,
    delta: CapsuleDelta,
    turn_number: u64,
) -> anyhow::Result<()> {
    if let Some(obj) = delta.objective_update {
        if !obj.trim().is_empty() {
            capsule.objective_and_criteria = obj;
        }
    }

    let mut decisions = Vec::new();
    for decision in delta.new_decisions {
        if capsule.decisions_and_rationale.iter().any(|d| d.turn == decision.turn && d.summary == decision.summary) {
            tracing::warn!(target: "capsule", "Skipping duplicate decision: turn {} '{}'", decision.turn, decision.summary);
        } else {
            decisions.push(decision);
        }
    }
    capsule.decisions_and_rationale.extend(decisions);

    let mut identifiers = Vec::new();
    for id in delta.new_identifiers {
        if capsule.stable_identifiers.iter().any(|existing| existing.category == id.category && existing.value == id.value) {
            tracing::warn!(target: "capsule", "Skipping duplicate identifier: {} ({})", id.category, id.value);
        } else {
            identifiers.push(id);
        }
    }
    capsule.stable_identifiers.extend(identifiers);

    for update in delta.task_updates {
        match update {
            CapsuleTaskUpdate::Add(task) => {
                capsule.open_tasks.push(task);
            }
            CapsuleTaskUpdate::Complete { description, turn } => {
                if let Some(t) = capsule.open_tasks.iter_mut().find(|t| t.description == description) {
                    t.status = "completed".to_string();
                    t.completed_turn = Some(turn);
                }
            }
            CapsuleTaskUpdate::Block { description, blocker } => {
                if let Some(t) = capsule.open_tasks.iter_mut().find(|t| t.description == description) {
                    t.status = "blocked".to_string();
                    t.blocker = Some(blocker);
                }
            }
            CapsuleTaskUpdate::Remove { description } => {
                capsule.open_tasks.retain(|t| t.description != description);
            }
        }
    }

    capsule.version += 1;
    capsule.last_update_turn = turn_number;
    capsule.updated_at = chrono::Utc::now().to_rfc3339();

    Ok(())
}

fn bootstrap_capsule_from_compressed_markers(
    session_id: &str,
    history: &[crate::llm::Message],
    turn_number: u64,
) -> Option<StateCapsule> {
    for msg in history {
        if msg.content.contains("[COMPRESSED CONTEXT") {
            let content = msg.content.clone();
            return Some(StateCapsule {
                version: 1,
                session_id: session_id.to_string(),
                last_update_turn: turn_number,
                objective_and_criteria: content,
                decisions_and_rationale: Vec::new(),
                stable_identifiers: Vec::new(),
                open_tasks: Vec::new(),
                prior_decisions_summary: None,
                previous_version_handle: None,
                source_history_handle: None,
                updated_at: chrono::Utc::now().to_rfc3339(),
            });
        }
    }
    None
}

/// Decide the working capsule for this compression pass.
///
/// Precedence (first wins):
/// 1. **Reuse** the prior capsule carried in from the previous governor run
///    (`prior`). This is the incremental path: `extract_delta` then only sees
///    the newly-compressible turns against the accumulated state, instead of
///    re-summarizing the whole history into an empty shell every time.
/// 2. **Legacy bootstrap** from a `[COMPRESSED CONTEXT` marker in the live
///    history — kept so old sessions / fresh histories (no prior capsule yet)
///    still recover their compressed context.
/// 3. **Fresh empty shell** for the very first compression of a session.
///
/// `reused` is `true` on path 1 so the caller can record provenance
/// (`previous_version_handle`) before `apply_delta` mutates the capsule.
fn seed_capsule(
    prior: Option<&StateCapsule>,
    session_id: &str,
    history: &[crate::llm::Message],
    turn_number: u64,
) -> (StateCapsule, bool) {
    if let Some(prior) = prior {
        // Clone the prior capsule as-is; the caller stamps provenance and
        // apply_delta bumps version/last_update_turn.
        return (prior.clone(), true);
    }
    let bootstrapped = bootstrap_capsule_from_compressed_markers(session_id, history, turn_number);
    (bootstrapped.unwrap_or_else(|| fresh_capsule(session_id, turn_number)), false)
}

/// Construct a brand-new empty capsule (the first-compression baseline).
fn fresh_capsule(session_id: &str, turn_number: u64) -> StateCapsule {
    StateCapsule {
        version: 1,
        session_id: session_id.to_string(),
        last_update_turn: turn_number,
        objective_and_criteria: String::new(),
        decisions_and_rationale: Vec::new(),
        stable_identifiers: Vec::new(),
        open_tasks: Vec::new(),
        prior_decisions_summary: None,
        previous_version_handle: None,
        source_history_handle: None,
        updated_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn cap_decisions(capsule: &mut StateCapsule, max_decisions: usize) {
    if capsule.decisions_and_rationale.len() > max_decisions {
        let overflow_count = capsule.decisions_and_rationale.len() - max_decisions;
        let overflow: Vec<_> = capsule.decisions_and_rationale.drain(..overflow_count).collect();
        capsule.prior_decisions_summary = Some(
            overflow
                .iter()
                .map(|d| format!("[Turn {}] {}: {}", d.turn, d.summary, d.rationale))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
}

fn cap_completed_tasks(capsule: &mut StateCapsule, max_completed: usize) {
    let completed: Vec<usize> = capsule
        .open_tasks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.status == "completed")
        .map(|(i, _)| i)
        .collect();
    if completed.len() > max_completed {
        let to_remove = completed.len() - max_completed;
        let mut indices: Vec<usize> = completed.into_iter().take(to_remove).collect();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        for i in indices {
            capsule.open_tasks.remove(i);
        }
    }
}

pub(crate) async fn extract_delta(
    compressible: &[crate::llm::Message],
    capsule: &StateCapsule,
    gateway_cfg: &autonoetic_types::config::ContextCompressionConfig,
    agent_cfg: Option<&autonoetic_types::agent::CompressionConfig>,
    presets: &HashMap<String, LlmPreset>,
    http_client: &reqwest::Client,
    plan_anchor: Option<&PlanFrameSummary>,
) -> anyhow::Result<CapsuleDelta> {
    let llm_config = match resolve_compression_llm_config(gateway_cfg, agent_cfg, presets) {
        Some(c) => c,
        None => anyhow::bail!("No compression LLM configured for capsule delta extraction"),
    };

    let driver = crate::llm::build_driver(llm_config.clone(), http_client.clone())?;
    let capsule_json = serde_json::to_string(capsule)?;
    let prompt = build_delta_extraction_prompt(compressible, &capsule_json, plan_anchor);

    let system_msg = crate::llm::Message::system(
        "You are a state capsule extractor. Output ONLY valid JSON matching the schema.",
    );
    let user_msg = crate::llm::Message::user(prompt);
    let messages = vec![system_msg, user_msg];

    let req = crate::llm::CompletionRequest {
        model: llm_config.model.clone(),
        messages,
        tools: vec![],
        max_tokens: Some(2048),
        temperature: Some(0.0),
        metadata: None,
        thinking: None,
        // Per-session sticky routing so a session's successive capsule
        // delta-extraction calls reuse the same upstream provider instance
        // (and its implicit cache). The static system instruction is too small
        // (~14 tokens) to warrant a system_cache_prefix breakpoint; the bulk
        // of the tokens live in the varying user prompt. Kept harmless for
        // providers that ignore the field.
        prompt_cache_key: Some(format!("agw-capsule-{}", capsule.session_id)),
        system_cache_prefix_bytes: None,
    };

    let resp = driver.complete(&req).await?;
    let text = &resp.text;

    let cleaned = text
        .trim()
        .strip_prefix("```json")
        .or_else(|| text.trim().strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(text.trim());

    let delta: CapsuleDelta = serde_json::from_str(cleaned)
        .map_err(|e| anyhow::anyhow!("Failed to parse capsule delta: {e}\nRaw: {cleaned}"))?;

    Ok(delta)
}

#[async_trait]
impl super::ReductionStrategy for CapsuleStrategy {
    fn name(&self) -> &'static str {
        "capsule"
    }

    async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome> {
        let Some(ref cfg) = ctx.compression_config else {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        };
        if !cfg.enabled {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        }
        // No compression LLM resolved (no preset, no inline provider/model,
        // no mapping fallback hit). Skip gracefully so the governor cascade
        // can fall through to trim/demote instead of erroring out and
        // aborting the rest of the pipeline.
        if resolve_compression_llm_config(cfg, ctx.agent_compression.as_ref(), &self.presets)
            .is_none()
        {
            tracing::warn!(
                target: "autonoetic::capsule",
                "Context compression enabled but no compression LLM configured — skipping capsule strategy"
            );
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        }

        let (recent_turns_to_keep, max_capsule_decisions, max_completed_tasks) = {
            let agent = ctx.agent_compression.as_ref();
            (
                agent
                    .and_then(|a| a.recent_turns_to_keep)
                    .unwrap_or(cfg.recent_turns_to_keep),
                agent
                    .and_then(|a| a.max_capsule_decisions)
                    .unwrap_or(cfg.max_capsule_decisions),
                agent
                    .and_then(|a| a.max_completed_tasks)
                    .unwrap_or(cfg.max_completed_tasks),
            )
        };

        let (compressible, kept) = split_compressible_messages(&ctx.history, recent_turns_to_keep);

        if compressible.is_empty() {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        }

        // Egress compression-eligibility gate (RFC §5.7 rule 1): refuse to
        // summarize a tainted band on a preset that isn't cleared for it.
        // Compressing local_only history on a remote preset is a leak even with
        // per-envelope filtering — the whole point of the call is to transmit
        // that content. On refusal, fall back to Insufficient so the governor
        // cascade drops/truncates the band instead (an incomplete local context
        // beats a remote leak).
        if !ctx.egress_labels.is_empty() {
            let preset_cfg = resolve_compression_llm_config(
                cfg,
                ctx.agent_compression.as_ref(),
                &self.presets,
            );
            let preset_class = preset_cfg
                .as_ref()
                .and_then(|c| c.egress_class)
                .unwrap_or(autonoetic_types::egress::EgressClass::Remote);
            let elig = crate::runtime::egress_labeler::compression_preset_eligible(
                &compressible,
                &ctx.egress_labels,
                preset_class,
            );
            if !elig.is_eligible() {
                let crate::runtime::egress_labeler::CompressionEligibility::Ineligible {
                    reason,
                    leaked_tool_call_ids,
                } = elig
                else {
                    unreachable!("checked is_eligible above")
                };
                tracing::warn!(
                    target: "autonoetic::capsule::egress",
                    session_id = %ctx.session_id,
                    turn = ctx.turn_number,
                    preset_class = ?preset_class,
                    leaked_count = leaked_tool_call_ids.len(),
                    "compression eligibility gate refused — falling back to truncation (RFC §5.7)"
                );
                tracing::debug!(target: "autonoetic::capsule::egress", reason = %reason);
                // Emit an egress.boundary_refused causal event (best-effort;
                // the capsule strategy has no store handle, so we log only —
                // lifecycle's post-governor path can persist it if wired later).
                return Ok(ReductionOutcome::Insufficient {
                    tokens_remaining: ctx.breakdown.total_tokens,
                });
            }
        }

        let (mut capsule, reused) =
            seed_capsule(ctx.capsule_state.as_ref(), &ctx.session_id, &ctx.history, ctx.turn_number);

        // Provenance: if we are evolving a prior capsule, record the handle of
        // the capsule it descended from (the prior's own persisted handle) before
        // apply_delta bumps the version. This chains capsule versions for audit.
        if reused {
            if let Some(prior) = ctx.capsule_state.as_ref() {
                capsule.previous_version_handle = prior.source_history_handle.clone();
            }
        }

        let delta = extract_delta(
            compressible,
            &capsule,
            cfg,
            ctx.agent_compression.as_ref(),
            &self.presets,
            &self.http_client,
            ctx.plan_anchor.as_ref(),
        )
        .await?;

        validate_delta_approvals(&delta, ctx.turn_number)?;
        apply_delta(&mut capsule, delta, ctx.turn_number)?;
        cap_decisions(&mut capsule, max_capsule_decisions);
        cap_completed_tasks(&mut capsule, max_completed_tasks);

        // Persist the evolved capsule to the content store. The handle returned
        // by `store.write` is the capsule's own content-addressed handle; stamp
        // it into `source_history_handle` so the *next* compression can record
        // it as its `previous_version_handle` (chain of versions for audit).
        // We assign `ctx.capsule_state` only after the handle is known, so the
        // value carried forward (into self.capsule_state at the govern call
        // site) carries provenance.
        if let Some(ref dir) = self.gateway_dir {
            match ContentStore::new(dir) {
                Ok(store) => {
                    match serde_json::to_vec(&capsule) {
                        Ok(json_bytes) => {
                            match store.write(&json_bytes) {
                                Ok(handle) => {
                                    // This capsule's own handle — the next
                                    // pass reads it as previous_version_handle.
                                    capsule.source_history_handle = Some(handle.clone());
                                    if let Err(e) = store.register_name_with_visibility(
                                        &ctx.session_id,
                                        &format!("capsule_v{}_turn_{}", capsule.version - 1, ctx.turn_number),
                                        &handle,
                                        ContentVisibility::Private,
                                    ) {
                                        tracing::warn!(target: "capsule", "Failed to register capsule name: {e}");
                                    }
                                }
                                Err(e) => tracing::warn!(target: "capsule", "Failed to write capsule to content store: {e}"),
                            }
                        }
                        Err(e) => tracing::warn!(target: "capsule", "Failed to serialize capsule: {e}"),
                    }
                }
                Err(e) => tracing::warn!(target: "capsule", "Failed to open content store: {e}"),
            }
        }
        ctx.capsule_state = Some(capsule.clone());

        let injection_text = compile_capsule_injection(&capsule);
        let injection_msg = crate::llm::Message::system(injection_text);

        // RFC #780 Part E.1: archive the full pre-compression message history
        // to the content store BEFORE replacing it. The capsule itself is
        // already persisted above (for structured audit); this archives the
        // raw uncompressed messages so the exact pre-compression state can be
        // restored. Sets `compressed_context_handle` on the metadata (previously
        // always None — the helper existed at compression.rs:427 but was never
        // called).
        //
        // Metadata is updated unconditionally when compression occurs — even
        // if the archive write fails, the checkpoints must reflect that
        // compression happened (compression_count, messages_summarized, turn).
        // `compressed_context_handle` is set to the handle on success or None
        // on failure, so the audit trail is honest about what was archived.
        let compressible_count = compressible.len() as u64;
        let mut archive_handle: Option<String> = None;
        if let Some(ref dir) = self.gateway_dir {
            match ContentStore::new(dir) {
                Ok(store) => {
                    match serde_json::to_vec(&ctx.history) {
                        Ok(json) => {
                            match store.write(&json) {
                                Ok(handle) => {
                                    let name = format!("compressed_context_turn_{}", ctx.turn_number);
                                    if let Err(e) = store.register_name_with_visibility(
                                        &ctx.session_id,
                                        &name,
                                        &handle,
                                        ContentVisibility::Private,
                                    ) {
                                        tracing::warn!(target: "capsule", error = %e, "Failed to register compressed context name");
                                    }
                                    archive_handle = Some(handle);
                                }
                                Err(e) => tracing::warn!(target: "capsule", error = %e, "Failed to write compressed context to content store"),
                            }
                        }
                        Err(e) => tracing::warn!(target: "capsule", error = %e, "Failed to serialize pre-compression history"),
                    }
                }
                Err(e) => tracing::warn!(target: "capsule", error = %e, "Failed to open content store for pre-compression archive"),
            }
        }
        // Update metadata unconditionally — compression IS happening
        // regardless of whether the archive succeeded.
        let meta = ctx.compression_metadata.get_or_insert_with(|| {
            crate::runtime::compression::CompressionMetadata {
                last_compression_turn: 0,
                messages_summarized: 0,
                compressed_context_handle: None,
                compression_count: 0,
            }
        });
        meta.compressed_context_handle = archive_handle;
        meta.last_compression_turn = ctx.turn_number;
        meta.messages_summarized = compressible_count;
        meta.compression_count += 1;

        let mut new_history = Vec::with_capacity(kept.len() + 1);
        new_history.push(injection_msg);
        new_history.extend(kept.iter().cloned());
        ctx.history = new_history;

        let conv_text: String = ctx
            .history
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let tokens_after = crate::runtime::prompt_budget::estimate_tokens(&conv_text);
        let new_total = ctx
            .breakdown
            .total_tokens
            .saturating_sub(ctx.breakdown.conversation_tokens)
            .saturating_add(tokens_after);

        ctx.breakdown.conversation_tokens = tokens_after;
        ctx.breakdown.total_tokens = new_total;

        let still_over = ctx.breakdown.total_tokens > ctx.effective_limit;
        if still_over {
            Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            })
        } else {
            Ok(ReductionOutcome::Resolved {
                tokens_after: ctx.breakdown.total_tokens,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_capsule() -> StateCapsule {
        StateCapsule {
            version: 1,
            session_id: "test-session".into(),
            last_update_turn: 5,
            objective_and_criteria: "Build a thing".into(),
            decisions_and_rationale: vec![CapsuleDecision {
                turn: 1,
                summary: "Chose Rust".into(),
                rationale: "Best for perf".into(),
                referenced_ids: vec![],
            }],
            stable_identifiers: vec![StableIdentifier {
                category: "file".into(),
                value: "src/main.rs".into(),
                label: Some("Main".into()),
                first_seen_turn: 1,
            }],
            open_tasks: vec![
                CapsuleTask {
                    description: "Write parser".into(),
                    status: "in_progress".into(),
                    added_turn: 2,
                    completed_turn: None,
                    blocker: None,
                },
                CapsuleTask {
                    description: "Write tests".into(),
                    status: "completed".into(),
                    added_turn: 3,
                    completed_turn: Some(5),
                    blocker: None,
                },
            ],
            prior_decisions_summary: None,
            previous_version_handle: None,
            source_history_handle: None,
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn apply_delta_objective_update() {
        let mut capsule = make_capsule();
        let delta = CapsuleDelta {
            objective_update: Some("New objective".into()),
            new_decisions: vec![],
            new_identifiers: vec![],
            task_updates: vec![],
        };
        assert!(apply_delta(&mut capsule, delta, 6).is_ok());
        assert_eq!(capsule.objective_and_criteria, "New objective");
        assert_eq!(capsule.version, 2);
        assert_eq!(capsule.last_update_turn, 6);
    }

    #[test]
    fn apply_delta_add_decisions() {
        let mut capsule = make_capsule();
        let delta = CapsuleDelta {
            objective_update: None,
            new_decisions: vec![CapsuleDecision {
                turn: 3,
                summary: "Chose Axum".into(),
                rationale: "Async HTTP".into(),
                referenced_ids: vec![],
            }],
            new_identifiers: vec![],
            task_updates: vec![],
        };
        assert!(apply_delta(&mut capsule, delta, 7).is_ok());
        assert_eq!(capsule.decisions_and_rationale.len(), 2);
        assert_eq!(capsule.decisions_and_rationale[1].summary, "Chose Axum");
    }

    #[test]
    fn apply_delta_skips_duplicate_decisions() {
        let mut capsule = make_capsule();
        let delta = CapsuleDelta {
            objective_update: None,
            new_decisions: vec![
                CapsuleDecision {
                    turn: 1,
                    summary: "Chose Rust".into(),
                    rationale: "Better rationale".into(),
                    referenced_ids: vec![],
                },
                CapsuleDecision {
                    turn: 3,
                    summary: "Chose Axum".into(),
                    rationale: "Async HTTP".into(),
                    referenced_ids: vec![],
                },
            ],
            new_identifiers: vec![],
            task_updates: vec![],
        };
        assert!(apply_delta(&mut capsule, delta, 7).is_ok());
        assert_eq!(capsule.decisions_and_rationale.len(), 2);
        assert_eq!(capsule.decisions_and_rationale[1].summary, "Chose Axum");
    }

    #[test]
    fn apply_delta_skips_duplicate_identifiers() {
        let mut capsule = make_capsule();
        let delta = CapsuleDelta {
            objective_update: None,
            new_decisions: vec![],
            new_identifiers: vec![
                StableIdentifier {
                    category: "file".into(),
                    value: "src/main.rs".into(),
                    label: None,
                    first_seen_turn: 99,
                },
                StableIdentifier {
                    category: "dep".into(),
                    value: "tokio".into(),
                    label: Some("async runtime".into()),
                    first_seen_turn: 6,
                },
            ],
            task_updates: vec![],
        };
        assert!(apply_delta(&mut capsule, delta, 7).is_ok());
        assert_eq!(capsule.stable_identifiers.len(), 2);
        assert_eq!(capsule.stable_identifiers[1].value, "tokio");
    }

    #[test]
    fn apply_delta_task_updates() {
        let mut capsule = make_capsule();
        let delta = CapsuleDelta {
            objective_update: None,
            new_decisions: vec![],
            new_identifiers: vec![],
            task_updates: vec![
                CapsuleTaskUpdate::Add(CapsuleTask {
                    description: "Deploy".into(),
                    status: "pending".into(),
                    added_turn: 6,
                    completed_turn: None,
                    blocker: None,
                }),
                CapsuleTaskUpdate::Complete {
                    description: "Write parser".into(),
                    turn: 6,
                },
                CapsuleTaskUpdate::Block {
                    description: "Write tests".into(),
                    blocker: "Need CI".into(),
                },
            ],
        };
        assert!(apply_delta(&mut capsule, delta, 7).is_ok());
        assert_eq!(capsule.open_tasks.len(), 3);
        let parser = capsule
            .open_tasks
            .iter()
            .find(|t| t.description == "Write parser")
            .unwrap();
        assert_eq!(parser.status, "completed");
        assert_eq!(parser.completed_turn, Some(6));
        let tests = capsule
            .open_tasks
            .iter()
            .find(|t| t.description == "Write tests")
            .unwrap();
        assert_eq!(tests.status, "blocked");
        assert_eq!(tests.blocker.as_deref(), Some("Need CI"));
        let deploy = capsule
            .open_tasks
            .iter()
            .find(|t| t.description == "Deploy")
            .unwrap();
        assert_eq!(deploy.status, "pending");
    }

    #[test]
    fn apply_delta_task_remove() {
        let mut capsule = make_capsule();
        let delta = CapsuleDelta {
            objective_update: None,
            new_decisions: vec![],
            new_identifiers: vec![],
            task_updates: vec![CapsuleTaskUpdate::Remove {
                description: "Write parser".into(),
            }],
        };
        assert!(apply_delta(&mut capsule, delta, 7).is_ok());
        assert_eq!(capsule.open_tasks.len(), 1);
        assert_eq!(capsule.open_tasks[0].description, "Write tests");
    }

    #[test]
    fn cap_decisions_overflow_moves_to_prior_summary() {
        let mut capsule = make_capsule();
        capsule.decisions_and_rationale.push(CapsuleDecision {
            turn: 2,
            summary: "Chose Axum".into(),
            rationale: "Async".into(),
            referenced_ids: vec![],
        });
        capsule.decisions_and_rationale.push(CapsuleDecision {
            turn: 3,
            summary: "Chose SQLite".into(),
            rationale: "Embedded".into(),
            referenced_ids: vec![],
        });
        assert_eq!(capsule.decisions_and_rationale.len(), 3);

        cap_decisions(&mut capsule, 1);

        assert_eq!(capsule.decisions_and_rationale.len(), 1);
        let prior = capsule.prior_decisions_summary.as_deref().unwrap();
        assert!(prior.contains("[Turn 1]"));
        assert!(prior.contains("[Turn 2]"));
        assert_eq!(
            capsule.objective_and_criteria,
            make_capsule().objective_and_criteria
        );
    }

    #[test]
    fn cap_decisions_no_overflow_leaves_prior_unchanged() {
        let mut capsule = make_capsule();
        capsule.prior_decisions_summary = Some("Previous summary".into());
        cap_decisions(&mut capsule, 10);
        assert_eq!(capsule.decisions_and_rationale.len(), 1);
        assert_eq!(
            capsule.prior_decisions_summary.as_deref(),
            Some("Previous summary")
        );
    }

    #[test]
    fn cap_completed_tasks_removes_oldest_first() {
        let mut capsule = make_capsule();
        capsule.open_tasks.push(CapsuleTask {
            description: "Task A".into(),
            status: "completed".into(),
            added_turn: 1,
            completed_turn: Some(2),
            blocker: None,
        });
        capsule.open_tasks.push(CapsuleTask {
            description: "Task B".into(),
            status: "completed".into(),
            added_turn: 3,
            completed_turn: Some(4),
            blocker: None,
        });
        assert_eq!(capsule.open_tasks.len(), 4);

        cap_completed_tasks(&mut capsule, 1);

        let completed: Vec<_> = capsule
            .open_tasks
            .iter()
            .filter(|t| t.status == "completed")
            .collect();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].description, "Task B");
    }

    #[test]
    fn compile_capsule_injection_basic() {
        let capsule = make_capsule();
        let output = compile_capsule_injection(&capsule);

        assert!(output.contains("[SESSION STATE CAPSULE v1"));
        assert!(output.contains("## Objective"));
        assert!(output.contains("Build a thing"));
        assert!(output.contains("## Key Decisions"));
        assert!(output.contains("[Turn 1]"));
        assert!(output.contains("Chose Rust"));
        assert!(output.contains("## Active Identifiers"));
        assert!(output.contains("src/main.rs"));
        assert!(output.contains("## Open Tasks"));
        assert!(output.contains("Write parser"));
        assert!(output.contains("## Completed Tasks (Recent)"));
        assert!(output.contains("[done@5] Write tests"));
    }

    #[test]
    fn compile_capsule_injection_includes_prior_decisions() {
        let mut capsule = make_capsule();
        capsule.prior_decisions_summary = Some("[Turn 0] Started: Initial planning".into());
        let output = compile_capsule_injection(&capsule);

        assert!(output.contains("## Prior Decisions (Summarized)"));
        assert!(output.contains("[Turn 0] Started: Initial planning"));
    }

    #[test]
    fn bootstrap_capsule_from_compressed_markers_present() {
        use crate::llm::Role;

        let history = vec![crate::llm::Message {
            role: Role::Assistant,
            content: "Some text\n[COMPRESSED CONTEXT]\nprior info here\n[/COMPRESSED CONTEXT]"
                .into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }];

        let result = bootstrap_capsule_from_compressed_markers("sess-1", &history, 10);
        assert!(result.is_some());
        let capsule = result.unwrap();
        assert!(capsule.objective_and_criteria.contains("prior info here"));
        assert_eq!(capsule.session_id, "sess-1");
        assert_eq!(capsule.last_update_turn, 10);
    }

    #[test]
    fn bootstrap_capsule_from_compressed_markers_absent() {
        let history = vec![crate::llm::Message {
            role: crate::llm::Role::Assistant,
            content: "Just normal text".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }];

        let result = bootstrap_capsule_from_compressed_markers("sess-1", &history, 10);
        assert!(result.is_none());
    }

    #[test]
    fn seed_capsule_reuses_prior_when_present() {
        // When a prior capsule is supplied, seed_capsule returns it verbatim
        // (the reuse path) and reports reused=true. The governor then evolves
        // it via extract_delta instead of re-summarizing into an empty shell.
        let mut prior = make_capsule();
        prior.version = 7;
        prior.source_history_handle = Some("sha-prior-handle".into());
        let history = vec![]; // not consulted when prior is Some
        let (capsule, reused) = seed_capsule(Some(&prior), "sess-1", &history, 12);
        assert!(reused, "prior present ⇒ reused path");
        assert_eq!(capsule.version, 7, "prior capsule returned as-is (apply_delta bumps later");
        assert_eq!(capsule.objective_and_criteria, prior.objective_and_criteria);
        assert_eq!(
            capsule.source_history_handle.as_deref(),
            Some("sha-prior-handle"),
            "prior handle carried through"
        );
    }

    #[test]
    fn seed_capsule_falls_back_to_bootstrap_when_no_prior_but_marker_present() {
        // Legacy path: no prior capsule, but history carries a compressed
        // marker — recover the prior context from it.
        let history = vec![crate::llm::Message {
            role: crate::llm::Role::System,
            content: "[COMPRESSED CONTEXT] recovered objective".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }];
        let (capsule, reused) = seed_capsule(None, "sess-1", &history, 9);
        assert!(!reused, "no prior ⇒ bootstrapped, not reused");
        assert!(capsule.objective_and_criteria.contains("recovered objective"));
    }

    #[test]
    fn seed_capsule_fresh_shell_when_no_prior_no_marker() {
        let history = vec![crate::llm::Message {
            role: crate::llm::Role::User,
            content: "hello".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }];
        let (capsule, reused) = seed_capsule(None, "sess-1", &history, 3);
        assert!(!reused);
        assert_eq!(capsule.version, 1, "fresh baseline capsule");
        assert!(capsule.objective_and_criteria.is_empty());
        assert_eq!(capsule.session_id, "sess-1");
    }

    fn make_plan_summary() -> PlanFrameSummary {
        PlanFrameSummary {
            plan_id: "plan_abc".into(),
            version: 3,
            parent_version: Some(2),
            status: autonoetic_types::plan_frame::PlanStatus::Approved,
            title: "Add OAuth login".into(),
            step_count: 5,
            operator_steps: vec!["op_login".into(), "op_logout".into()],
            agent_steps: vec!["agent_oauth".into(), "agent_logout".into()],
            required_validations: vec!["security_review".into()],
            advisory_validations: vec!["unit_tests".into()],
        }
    }

    #[test]
    fn delta_prompt_without_plan_omits_anchor_block() {
        let messages = vec![crate::llm::Message {
            role: crate::llm::Role::User,
            content: "Discuss the auth flow".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }];
        let prompt = build_delta_extraction_prompt(&messages, "{}", None);
        assert!(!prompt.contains("Active Plan"));
        assert!(!prompt.contains("plan_id"));
        assert!(prompt.contains("Recent Conversation Turns"));
    }

    #[test]
    fn delta_prompt_with_plan_includes_anchor_block() {
        let messages = vec![crate::llm::Message {
            role: crate::llm::Role::User,
            content: "Discuss the auth flow".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }];
        let plan = make_plan_summary();
        let prompt = build_delta_extraction_prompt(&messages, "{}", Some(&plan));
        assert!(prompt.contains("Active Plan"), "expected 'Active Plan' in prompt");
        assert!(prompt.contains("plan_abc"), "expected plan_id in prompt");
        assert!(prompt.contains("Add OAuth login"), "expected title in prompt");
        assert!(prompt.contains("op_login, op_logout"), "expected operator steps in prompt");
        assert!(prompt.contains("agent_oauth, agent_logout"), "expected agent steps in prompt");
        assert!(prompt.contains("security_review"), "expected required validations");
        assert!(prompt.contains("unit_tests"), "expected advisory validations");
        assert!(prompt.contains("relevance lens"), "expected framing as relevance lens");
    }

    #[test]
    fn delta_prompt_with_minimal_plan_omits_empty_sections() {
        let plan = PlanFrameSummary {
            plan_id: "plan_min".into(),
            version: 1,
            parent_version: None,
            status: autonoetic_types::plan_frame::PlanStatus::AwaitingApproval,
            title: String::new(),
            step_count: 0,
            operator_steps: vec![],
            agent_steps: vec![],
            required_validations: vec![],
            advisory_validations: vec![],
        };
        let prompt = build_delta_extraction_prompt(&[], "{}", Some(&plan));
        assert!(prompt.contains("plan_min"));
        assert!(!prompt.contains("operator/shared steps"));
        assert!(!prompt.contains("required validations"));
        assert!(!prompt.contains("advisory validations"));
    }

    // ---- RFC #780 Part E.1: pre-compression history archiving tests ----

    #[test]
    fn persist_compressed_context_writes_history_and_returns_handle() {
        use crate::runtime::compression::CompressionMetadata;

        let tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(tmp.path()).unwrap();
        let history = vec![
            crate::llm::Message::system("system prompt"),
            crate::llm::Message::user("hello"),
            crate::llm::Message::assistant("hi there"),
        ];
        let metadata = CompressionMetadata {
            last_compression_turn: 5,
            messages_summarized: 2,
            compressed_context_handle: None,
            compression_count: 1,
        };

        let handle = crate::runtime::compression::persist_compressed_context(
            tmp.path(),
            "test-session",
            &history,
            &metadata,
        )
        .unwrap();

        assert!(handle.is_some(), "should return a content handle");
        let handle = handle.unwrap();

        // The content store should have the full history registered.
        let name = "compressed_context_turn_5";
        let resolved = store
            .resolve_name("test-session", name)
            .unwrap();
        assert_eq!(resolved.to_string(), handle);
    }

    #[test]
    fn persist_compressed_context_handle_is_round_trippable() {
        use crate::runtime::compression::CompressionMetadata;

        let tmp = tempfile::tempdir().unwrap();
        let history = vec![
            crate::llm::Message::user("turn 1"),
            crate::llm::Message::assistant("reply 1"),
            crate::llm::Message::user("turn 2"),
        ];
        let metadata = CompressionMetadata {
            last_compression_turn: 3,
            messages_summarized: 3,
            compressed_context_handle: None,
            compression_count: 1,
        };

        let handle = crate::runtime::compression::persist_compressed_context(
            tmp.path(),
            "session-rt",
            &history,
            &metadata,
        )
        .unwrap()
        .unwrap();

        // Read back and verify the history round-trips.
        let store = ContentStore::new(tmp.path()).unwrap();
        let bytes = store.read(&handle).unwrap();
        let restored: Vec<crate::llm::Message> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.len(), 3);
        assert_eq!(restored[0].content, "turn 1");
        assert_eq!(restored[2].content, "turn 2");
    }
}
