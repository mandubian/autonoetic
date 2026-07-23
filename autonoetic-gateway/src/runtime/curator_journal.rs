//! Memory curator decision journal (issue #30).
//!
//! After a curator-style agent emits structured output, the gateway parses
//! the `decision_journal` array and persists one `curator.decision` causal
//! event per entry. The event's `target` column carries the entry's `target`
//! field, which makes lookups like "why was memory X dropped" a direct query.
//!
//! Trust model: the schema validator (`response_validation.rs`) has already
//! checked the output against the agent's declared `io.returns` schema by
//! the time this module runs. We still defensively validate each entry's
//! shape and skip malformed ones with a warn log, so a buggy schema never
//! pollutes the causal chain.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};

/// One row of the curator's decision journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionJournalEntry {
    pub target: String,
    pub action: String,
    pub reason_code: String,
    #[serde(default)]
    pub reason_detail: Option<String>,
    #[serde(default)]
    pub metric_values: Option<serde_json::Value>,
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Required when action == "promote_to_skill": the agent whose SKILL.md
    /// receives the instruction.
    #[serde(default)]
    pub target_agent: Option<String>,
    /// Required when action == "promote_to_skill": the concrete instruction
    /// text to add to the target agent's SKILL.md.
    #[serde(default)]
    pub proposed_instruction: Option<String>,
}

impl DecisionJournalEntry {
    fn is_well_formed(&self) -> bool {
        !self.target.trim().is_empty()
            && !self.action.trim().is_empty()
            && !self.reason_code.trim().is_empty()
    }

    /// Validation error for promote_to_skill entries missing routing fields.
    /// Returns None if the entry is valid, Some(reason) if rejected.
    fn routing_error(&self) -> Option<String> {
        if self.action != "promote_to_skill" {
            return None;
        }
        let mut missing = Vec::new();
        if self
            .target_agent
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            missing.push("target_agent");
        }
        if self
            .proposed_instruction
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
        {
            missing.push("proposed_instruction");
        }
        if missing.is_empty() {
            None
        } else {
            Some(format!(
                "promote_to_skill entry for target '{}' is missing required routing field(s): {}. \
                 Include both 'target_agent' (the agent receiving the instruction) and \
                 'proposed_instruction' (the concrete instruction text to add to its SKILL.md).",
                self.target,
                missing.join(", ")
            ))
        }
    }
}

/// Extract the JSON object slice from text that may be wrapped in markdown
/// code fences or have leading/trailing prose. Mirrors `post_session_digest.rs`.
fn extract_json_object_slice(text: &str) -> Option<&str> {
    let t = text.trim();
    // Strip markdown code fences if present
    let scan = if let Some(pos) = t.find("```") {
        let after = t[pos + 3..].trim_start();
        let after = if after.starts_with("json") {
            after[4..].trim_start()
        } else {
            after
        };
        if let Some(end) = after.find("```") {
            &after[..end]
        } else {
            after
        }
    } else {
        t
    };
    let start = scan.find('{')?;
    let end = scan.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(scan[start..=end].trim())
}

/// Extract decision-journal entries from a structured assistant reply.
///
/// Returns:
/// - `None` when the reply is missing, not valid JSON, or contains no
///   `decision_journal` field — these are valid agent outputs that simply
///   don't carry a journal.
/// - `Some(vec)` when a `decision_journal` array is present. Malformed
///   entries inside the array are dropped with a warn log so the rest of
///   the journal is still persisted.
pub fn extract_decision_journal_entries(reply: &str) -> Option<Vec<DecisionJournalEntry>> {
    let json_slice = extract_json_object_slice(reply)?;
    let json: serde_json::Value = serde_json::from_str(json_slice).ok()?;
    let array = json.get("decision_journal")?.as_array()?;
    let mut out = Vec::with_capacity(array.len());
    for (idx, raw) in array.iter().enumerate() {
        match serde_json::from_value::<DecisionJournalEntry>(raw.clone()) {
            Ok(entry) => {
                if !entry.is_well_formed() {
                    tracing::warn!(
                        target: "curator_journal",
                        entry_index = idx,
                        "decision_journal entry missing required fields (target/action/reason_code); skipping"
                    );
                    continue;
                }
                if let Some(reject_reason) = entry.routing_error() {
                    tracing::warn!(
                        target: "curator_journal",
                        entry_index = idx,
                        target = %entry.target,
                        "decision_journal entry rejected: {}",
                        reject_reason
                    );
                    continue;
                }
                out.push(entry);
            }
            Err(err) => {
                tracing::warn!(
                    target: "curator_journal",
                    entry_index = idx,
                    error = %err,
                    "decision_journal entry failed deserialization; skipping"
                );
            }
        }
    }
    Some(out)
}

/// Persist a decision-journal batch as causal events.
///
/// Emits one `{category}.decision` event per entry (with the entry's `target`
/// in the event's `target` column for indexed query-by-target) plus one
/// `{category}.decision_journal_recorded` summary event keyed by the
/// agent's session so an operator can find the batch in one shot.
///
/// The `category` parameter allows any agent to opt into the decision-journal
/// surface without being misclassified. Curator agents should pass `"curator"`.
pub fn persist_decision_journal_entries(
    store: &GatewayStore,
    category: &str,
    agent_id: &str,
    session_id: &str,
    revision_id: Option<&str>,
    entries: &[DecisionJournalEntry],
) -> Result<()> {
    let timestamp = chrono::Utc::now().to_rfc3339();
    for (seq, entry) in entries.iter().enumerate() {
        let payload = serde_json::json!({
            "agent_id": agent_id,
            "session_id": session_id,
            "revision_id": revision_id,
            "target": entry.target,
            "action": entry.action,
            "reason_code": entry.reason_code,
            "reason_detail": entry.reason_detail,
            "metric_values": entry.metric_values,
            "confidence": entry.confidence,
            "target_agent": entry.target_agent,
            "proposed_instruction": entry.proposed_instruction,
        });
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("{}-dec-{}", category, uuid::Uuid::new_v4()),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: seq as u64,
            timestamp: timestamp.clone(),
            // TODO: parameterize category so non-curator agents opting into
            // decision_journal are not misclassified as "curator".
            category: category.to_string(),
            action: "decision".to_string(),
            status: "active".to_string(),
            enforced_rules: Vec::new(),
            target: Some(entry.target.clone()),
            payload: Some(payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: entry.reason_detail.clone(),
        };
        store.create_causal_event(&event)?;
    }

    // Summary event: one row per journal run, useful to scope a query to a
    // single run without joining over the per-entry events.
    let summary = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("{}-jrn-{}", category, uuid::Uuid::new_v4()),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq: entries.len() as u64,
        timestamp,
        category: category.to_string(),
        action: "decision_journal_recorded".to_string(),
        status: "active".to_string(),
        enforced_rules: Vec::new(),
        target: Some(session_id.to_string()),
        payload: Some(
            serde_json::json!({
                "agent_id": agent_id,
                "session_id": session_id,
                "revision_id": revision_id,
                "entry_count": entries.len(),
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    };
    store.create_causal_event(&summary)?;

    // Also persist promote_to_skill decisions as knowledge entries so the
    // evolution-orchestrator can find them via knowledge_search (causal events
    // are invisible to agent tools).
    for entry in entries {
        if entry.action != "promote_to_skill" {
            continue;
        }
        let Some(ref agent) = entry.target_agent else { continue };
        let Some(ref instruction) = entry.proposed_instruction else { continue };
        let content = format!(
            "promote_to_skill: agent={}, target={}, instruction={}",
            agent, entry.target, instruction
        );
        let mut memory = MemoryObject::new(
            format!("grad-{}-{}", agent, entry.target),
            "evolution/graduations".to_string(),
            agent_id.to_string(),
            agent_id.to_string(),
            format!("session:{}:decision_journal", session_id),
            content,
        );
        memory.source_type = MemorySourceType::AgentWrite;
        memory.tags = vec![
            "source:memory_curator".to_string(),
            "type:promote_to_skill".to_string(),
            format!("agent:{}", agent),
            format!("target:{}", entry.target),
        ];
        memory.visibility = MemoryVisibility::Global;
        memory.confidence = entry.confidence;
        store.memory_upsert(&memory)?;
    }

    Ok(())
}

/// Convenience: parse `reply` for a `decision_journal` array and persist any
/// well-formed entries. Returns the number of entries persisted (0 when the
/// reply carries no journal). Errors only on store-write failure.
pub fn extract_and_persist(
    store: &GatewayStore,
    category: &str,
    agent_id: &str,
    session_id: &str,
    revision_id: Option<&str>,
    reply: &str,
) -> Result<usize> {
    let Some(entries) = extract_decision_journal_entries(reply) else {
        return Ok(0);
    };
    if entries.is_empty() {
        return Ok(0);
    }
    let n = entries.len();
    persist_decision_journal_entries(store, category, agent_id, session_id, revision_id, &entries)?;

    // Store the full curator output as a knowledge entry so the
    // evolution-orchestrator can find it via knowledge_search.
    // The orchestrator spawns the curator as a workflow child but has
    // no reliable way to read the spawn return — this bridges the gap.
    if let Some(json_slice) = extract_json_object_slice(reply) {
        let content = json_slice.to_string();
        let mut memory = MemoryObject::new(
            format!("curator-output-{}", session_id.replace('/', "-")),
            "evolution/curator_output".to_string(),
            agent_id.to_string(),
            agent_id.to_string(),
            format!("session:{}:io.returns", session_id),
            content,
        );
        memory.source_type = MemorySourceType::AgentWrite;
        memory.tags = vec![
            "source:memory_curator".to_string(),
            "type:curator_output".to_string(),
            format!("session:{}", session_id),
        ];
        memory.visibility = MemoryVisibility::Global;
        memory.confidence = Some(1.0);
        if let Err(e) = store.memory_upsert(&memory) {
            tracing::warn!(
                target: "curator_journal",
                session_id = %session_id,
                error = %e,
                "failed to persist curator output as knowledge entry"
            );
        }
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let reply = r#"{"decision_journal": [{"target": "mem-1", "action": "keep", "reason_code": "high_confidence_pattern"}]}"#;
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, "mem-1");
        assert_eq!(entries[0].action, "keep");
    }

    #[test]
    fn parses_fenced_json() {
        let reply = "I previously wrapped my output in markdown code fences. Let me resubmit.\n\n```json\n{\"decision_journal\": [{\"target\": \"mem-2\", \"action\": \"promote_to_skill\", \"reason_code\": \"high_confidence_pattern\", \"reason_detail\": \"recurring across 3 sessions\", \"target_agent\": \"planner.default\", \"proposed_instruction\": \"Never call sandbox_exec directly.\"}]}\n```\n";
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, "mem-2");
        assert_eq!(entries[0].action, "promote_to_skill");
    }

    #[test]
    fn parses_fenced_json_without_lang_tag() {
        let reply = "Here's the journal:\n```\n{\"decision_journal\": [{\"target\": \"mem-3\", \"action\": \"drop\", \"reason_code\": \"stale\"}]}\n```";
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "drop");
    }

    #[test]
    fn returns_none_for_missing_journal() {
        let reply = r#"{"agent_scores": {}}"#;
        assert!(extract_decision_journal_entries(reply).is_none());
    }

    #[test]
    fn skips_malformed_entries() {
        let reply = r#"{"decision_journal": [
            {"target": "good", "action": "keep", "reason_code": "high_confidence_pattern"},
            {"action": "keep", "reason_code": "missing_target"},
            "not_an_object"
        ]}"#;
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].target, "good");
    }

    #[test]
    fn accepts_promote_to_skill_with_routing_fields() {
        let reply = r#"{"decision_journal": [{
            "target": "pattern-123", "action": "promote_to_skill", "reason_code": "high_confidence_pattern",
            "target_agent": "planner.default",
            "proposed_instruction": "Never call sandbox_exec directly; always delegate via agent_spawn."
        }]}"#;
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "promote_to_skill");
        assert_eq!(entries[0].target_agent.as_deref(), Some("planner.default"));
    }

    #[test]
    fn rejects_promote_to_skill_missing_target_agent() {
        let reply = r#"{"decision_journal": [{
            "target": "pattern-123", "action": "promote_to_skill", "reason_code": "high_confidence_pattern",
            "proposed_instruction": "Some instruction"
        }]}"#;
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn rejects_promote_to_skill_missing_proposed_instruction() {
        let reply = r#"{"decision_journal": [{
            "target": "pattern-123", "action": "promote_to_skill", "reason_code": "high_confidence_pattern",
            "target_agent": "planner.default"
        }]}"#;
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn rejects_promote_to_skill_missing_both_fields() {
        let reply = r#"{"decision_journal": [{
            "target": "pattern-123", "action": "promote_to_skill", "reason_code": "high_confidence_pattern"
        }]}"#;
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn keeps_valid_entries_when_mixed_with_rejected() {
        let reply = r#"{"decision_journal": [
            {"target": "pattern-keep", "action": "keep", "reason_code": "high_confidence_pattern"},
            {"target": "pattern-reject", "action": "promote_to_skill", "reason_code": "high_confidence_pattern"},
            {"target": "pattern-ok", "action": "promote_to_skill", "reason_code": "high_confidence_pattern",
             "target_agent": "executor.default", "proposed_instruction": "Use bash -c wrapper."}
        ]}"#;
        let entries = extract_decision_journal_entries(reply).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].target, "pattern-keep");
        assert_eq!(entries[1].target, "pattern-ok");
    }

    #[test]
    fn curator_output_memory_id_uses_session_id() {
        let session_id = "curator-final";
        let expected = format!("curator-output-{}", session_id.replace('/', "-"));
        assert_eq!(expected, "curator-output-curator-final");
    }

    #[test]
    fn curator_output_memory_id_escapes_slashes() {
        let session_id = "evo-cycle/memory-curator.default-abc123";
        let expected = format!("curator-output-{}", session_id.replace('/', "-"));
        assert_eq!(expected, "curator-output-evo-cycle-memory-curator.default-abc123");
    }
}
