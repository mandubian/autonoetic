use crate::runtime::compression::{self, resolve_compression_llm_config, split_compressible_messages};
use crate::runtime::context_governor::strategies::{GovernorContext, ReductionOutcome};
use crate::runtime::content_store::{ContentStore, ContentVisibility};
use autonoetic_types::config::LlmPreset;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const CAPSULE_ENV: &str = "AUTONOETIC_STATE_CAPSULE_COMPRESSION";

pub fn capsule_enabled() -> bool {
    std::env::var(CAPSULE_ENV).as_deref() == Ok("1")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateCapsule {
    pub version: u64,
    pub session_id: String,
    pub last_update_turn: u64,
    pub objective_and_criteria: String,
    pub decisions_and_rationale: Vec<CapsuleDecision>,
    pub stable_identifiers: Vec<StableIdentifier>,
    pub open_tasks: Vec<CapsuleTask>,
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
) -> String {
    format!(
        r#"You are a state capsule update extractor. Given the current session state capsule and recent conversation turns, produce a structured delta describing what changed.

Current State Capsule (JSON):
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

    for decision in &delta.new_decisions {
        if capsule.decisions_and_rationale.iter().any(|d| d.turn == decision.turn && d.summary == decision.summary) {
            anyhow::bail!("Duplicate decision rejected: turn {} '{}'", decision.turn, decision.summary);
        }
    }
    capsule.decisions_and_rationale.extend(delta.new_decisions);

    for id in &delta.new_identifiers {
        if capsule.stable_identifiers.iter().any(|existing| existing.category == id.category && existing.value == id.value) {
            anyhow::bail!("Duplicate stable identifier rejected: {} ({})", id.category, id.value);
        }
    }
    capsule.stable_identifiers.extend(delta.new_identifiers);

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
                previous_version_handle: None,
                source_history_handle: None,
                updated_at: chrono::Utc::now().to_rfc3339(),
            });
        }
    }
    None
}

fn cap_decisions(capsule: &mut StateCapsule, max_decisions: usize) {
    if capsule.decisions_and_rationale.len() > max_decisions {
        let overflow_count = capsule.decisions_and_rationale.len() - max_decisions;
        let overflow: Vec<_> = capsule.decisions_and_rationale.drain(..overflow_count).collect();
        let prior_summary = overflow
            .iter()
            .map(|d| format!("[Turn {}] {}: {}", d.turn, d.summary, d.rationale))
            .collect::<Vec<_>>()
            .join("\n");
        let priors = format!("Prior decisions (summarized):\n{}", prior_summary);
        capsule.objective_and_criteria = format!("{}\n\n---\n{}", capsule.objective_and_criteria, priors);
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
) -> anyhow::Result<CapsuleDelta> {
    let llm_config = match resolve_compression_llm_config(gateway_cfg, agent_cfg, presets) {
        Some(c) => c,
        None => anyhow::bail!("No compression LLM configured for capsule delta extraction"),
    };

    let driver = crate::llm::build_driver(llm_config.clone(), http_client.clone())?;
    let capsule_json = serde_json::to_string(capsule)?;
    let prompt = build_delta_extraction_prompt(compressible, &capsule_json);

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
        if !capsule_enabled() {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        }

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

        let mut capsule = match bootstrap_capsule_from_compressed_markers(
            &ctx.session_id,
            &ctx.history,
            ctx.turn_number,
        ) {
            Some(c) => c,
            None => StateCapsule {
                version: 1,
                session_id: ctx.session_id.clone(),
                last_update_turn: ctx.turn_number,
                objective_and_criteria: String::new(),
                decisions_and_rationale: Vec::new(),
                stable_identifiers: Vec::new(),
                open_tasks: Vec::new(),
                previous_version_handle: None,
                source_history_handle: None,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        };

        let delta = extract_delta(
            compressible,
            &capsule,
            cfg,
            ctx.agent_compression.as_ref(),
            &self.presets,
            &self.http_client,
        )
        .await?;

        validate_delta_approvals(&delta, ctx.turn_number)?;
        apply_delta(&mut capsule, delta, ctx.turn_number)?;
        cap_decisions(&mut capsule, max_capsule_decisions);
        cap_completed_tasks(&mut capsule, max_completed_tasks);

        if let Some(ref dir) = self.gateway_dir {
            if let Ok(store) = ContentStore::new(dir) {
                if let Ok(json_bytes) = serde_json::to_vec(&capsule) {
                    if let Ok(handle) = store.write(&json_bytes) {
                        let _ = store.register_name_with_visibility(
                            &ctx.session_id,
                            &format!("capsule_v{}_turn_{}", capsule.version - 1, ctx.turn_number),
                            &handle,
                            ContentVisibility::Private,
                        );
                    }
                }
            }
        }

        let injection_text = compile_capsule_injection(&capsule);
        let injection_msg = crate::llm::Message::system(injection_text);

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
