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
}

impl DecisionJournalEntry {
    fn is_well_formed(&self) -> bool {
        !self.target.trim().is_empty()
            && !self.action.trim().is_empty()
            && !self.reason_code.trim().is_empty()
    }
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
    let json: serde_json::Value = serde_json::from_str(reply).ok()?;
    let array = json.get("decision_journal")?.as_array()?;
    let mut out = Vec::with_capacity(array.len());
    for (idx, raw) in array.iter().enumerate() {
        match serde_json::from_value::<DecisionJournalEntry>(raw.clone()) {
            Ok(entry) if entry.is_well_formed() => out.push(entry),
            Ok(_) => {
                tracing::warn!(
                    target: "curator_journal",
                    entry_index = idx,
                    "decision_journal entry missing required fields (target/action/reason_code); skipping"
                );
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
/// Emits one `curator.decision` event per entry (with the entry's `target`
/// in the event's `target` column for indexed query-by-target) plus one
/// `curator.decision_journal_recorded` summary event keyed by the
/// curator's session so an operator can find the batch in one shot.
pub fn persist_decision_journal_entries(
    store: &GatewayStore,
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
        });
        let event = autonoetic_types::causal_chain::CausalEventRecord {
            event_id: format!("curator-dec-{}", uuid::Uuid::new_v4()),
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            event_seq: seq as u64,
            timestamp: timestamp.clone(),
            // TODO: parameterize category so non-curator agents opting into
            // decision_journal are not misclassified as "curator".
            category: "curator".to_string(),
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

    // Summary event: one row per curator run, useful to scope a query to a
    // single run without joining over the per-entry events.
    let summary = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("curator-jrn-{}", uuid::Uuid::new_v4()),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq: 0,
        timestamp,
        category: "curator".to_string(),
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

    Ok(())
}

/// Convenience: parse `reply` for a `decision_journal` array and persist any
/// well-formed entries. Returns the number of entries persisted (0 when the
/// reply carries no journal). Errors only on store-write failure.
pub fn extract_and_persist(
    store: &GatewayStore,
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
    persist_decision_journal_entries(store, agent_id, session_id, revision_id, &entries)?;
    Ok(n)
}
