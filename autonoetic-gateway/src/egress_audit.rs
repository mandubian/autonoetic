//! The per-turn egress audit view — RFC data-envelopes §9.3.
//!
//! Answers the RFC §9.4 introspection bar for one session, from the causal
//! chain alone:
//!
//! 1. *What left the machine at turn N?* → the `egress.request_filtered` /
//!    `request_forwarded` summary (sink, preset, withheld/included counts).
//! 2. *Why was X withheld?* → `egress.envelope_withheld` (indication + label).
//! 3. *Why is this labeled?* → `egress.envelope_labeled` (matched rules).
//! 4. *Why did turn N run on this provider?* → the summary's `preset` +
//!    `target_sink`.
//! 5. *Who declassified what?* → `egress.declassified` (scope, target, sink).
//!
//! This module lives in the gateway crate, not in the CLI, because the report
//! has two consumers: `gateway egress-audit` and the `egress.audit` JSON-RPC
//! that the session room and remote operators use (#973). It sits outside
//! `router.rs` for the same reasons `evolution_view` does — it is unit-testable
//! against a store directly, and the router's dispatch frame does not grow
//! (#884, #916).
//!
//! It is **reporting, not inference**: every field comes from a recorded event
//! payload, and the report is content-free metadata throughout (ids, labels,
//! sinks, counts, and indication text — which is itself metadata by
//! construction, RFC §3.3). Rendering is left to callers.

use anyhow::Result;
use autonoetic_types::causal_chain::CausalEventRecord;

use crate::scheduler::gateway_store::GatewayStore;

/// Default cap on causal events scanned for one audit.
///
/// `search_causal_events` filters on session/agent, not category, so the audit
/// pulls the session's events and keeps the egress ones. Egress events are
/// sparse — they fire only on labeled content — so this covers long sessions
/// comfortably; when it is hit, [`EgressAudit::truncated`] says so rather than
/// letting the report look complete.
pub const DEFAULT_AUDIT_LIMIT: i64 = 50_000;

/// A report plus the honesty flags about how it was gathered. Both callers
/// surface `truncated`, so it belongs with the report rather than being
/// re-derived per caller.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressAudit {
    pub report: EgressAuditReport,
    /// The event scan hit `limit`; early turns may be missing and the totals
    /// are for the returned window only.
    pub truncated: bool,
    pub limit: i64,
}

/// Load one session's egress audit from the causal chain.
///
/// A store error propagates: an audit that silently reports "nothing left the
/// machine" because a read failed would be worse than no audit at all.
pub fn load_egress_audit(
    store: &GatewayStore,
    session_id: &str,
    limit: Option<i64>,
) -> Result<EgressAudit> {
    let limit = limit.unwrap_or(DEFAULT_AUDIT_LIMIT).max(1);
    let events = store.search_causal_events(Some(session_id), None, limit)?;
    let truncated = events.len() as i64 >= limit;
    let egress: Vec<CausalEventRecord> = events
        .into_iter()
        .filter(|e| e.category == "egress")
        .collect();
    Ok(EgressAudit {
        report: build_egress_audit(session_id, &egress),
        truncated,
        limit,
    })
}

/// One row in the per-turn audit report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressAuditRow {
    /// The egress action (`egress.envelope_labeled`, `egress.request_filtered`,
    /// `egress.envelope_withheld`, `egress.assertion_violation`, …).
    pub action: String,
    /// Stable display fields derived from the event payload (content-free).
    pub fields: EgressAuditFields,
}

/// The display fields the audit extracts from an event payload. Content-free
/// metadata only — ids, sink, counts, indication text (itself metadata).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressAuditFields {
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub target_sink: Option<String>,
    pub preset: Option<String>,
    pub indication: Option<String>,
    pub resolution: Option<String>,
    pub withheld_count: Option<u64>,
    pub included_count: Option<u64>,
    pub violation_count: Option<u64>,
    pub payload_digest: Option<String>,
    /// `egress.boundary_refused` — sandbox / web / hooks / mcp / ofp / compression.
    pub surface: Option<String>,
    pub label_name: Option<String>,
    pub reason: Option<String>,
    pub envelope_count: Option<u64>,
    /// Compression-only refusal metadata.
    pub preset_class: Option<String>,
    pub fallback: Option<String>,
    pub source_id_count: Option<u64>,
    /// `egress.declassified` grant metadata.
    pub allowed_sink: Option<String>,
    pub declass_scope: Option<String>,
    pub declass_target: Option<String>,
}

/// One turn's worth of egress events, in event-seq order.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressAuditTurn {
    pub turn_id: Option<String>,
    pub rows: Vec<EgressAuditRow>,
}

/// The full audit report — session id + per-turn rows + totals.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EgressAuditReport {
    pub session_id: String,
    pub turns: Vec<EgressAuditTurn>,
    pub total_withheld: u64,
    pub total_violations: u64,
    pub total_boundary_refusals: u64,
}

/// Pure rendering: derive the structured audit report from a set of egress
/// causal events for a session. Exposed so the rendering is unit-testable
/// without a live gateway store. `events` should already be filtered to
/// `category == "egress"` (the handler does this; the helper tolerates
/// non-egress events by ignoring them).
pub fn build_egress_audit(
    session_id: &str,
    events: &[CausalEventRecord],
) -> EgressAuditReport {
    use std::collections::BTreeMap;
    // Group by turn_id, preserving event_seq order within each turn.
    let mut by_turn: BTreeMap<Option<String>, Vec<&CausalEventRecord>> =
        BTreeMap::new();
    for ev in events {
        if ev.category != "egress" {
            continue;
        }
        by_turn.entry(ev.turn_id.clone()).or_default().push(ev);
    }

    let mut total_withheld = 0u64;
    let mut total_violations = 0u64;
    let mut total_boundary_refusals = 0u64;
    let mut turns: Vec<EgressAuditTurn> = Vec::new();

    for (turn_id, mut evs) in by_turn {
        evs.sort_by_key(|e| e.event_seq);
        let mut rows: Vec<EgressAuditRow> = Vec::new();
        for ev in &evs {
            let payload: serde_json::Value = ev
                .payload
                .as_ref()
                .and_then(|p| serde_json::from_str(p).ok())
                .unwrap_or(serde_json::Value::Null);
            let get = |k: &str| payload.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
            let get_u = |k: &str| payload.get(k).and_then(|v| v.as_u64());
            let array_len = |k: &str| {
                payload
                    .get(k)
                    .and_then(|v| v.as_array())
                    .map(|a| a.len() as u64)
            };
            let preset = get("preset").or_else(|| get("model"));
            let withheld_count = get_u("withheld_count");
            let violation_count = get_u("violation_count");
            // Tally totals only from the canonical `egress.request_filtered`
            // summary event. `egress.request_forwarded` (the tracer-side
            // mirror) carries the same counts; tallying both double-counts.
            if ev.action == "egress.request_filtered" {
                if let Some(w) = withheld_count {
                    total_withheld += w;
                }
                if let Some(v) = violation_count {
                    total_violations += v;
                }
            }
            if ev.action == "egress.boundary_refused" {
                total_boundary_refusals += 1;
            }
            let declass_target = payload
                .get("target")
                .and_then(|t| t.get("value").and_then(|x| x.as_str()).map(str::to_string))
                .or_else(|| ev.target.clone());
            let allowed_sink = payload.get("allowed_sink").and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            });
            let fields = EgressAuditFields {
                tool_call_id: get("tool_call_id"),
                tool_name: get("tool_name").or_else(|| get("tool")),
                target_sink: get("target_sink"),
                preset,
                indication: get("indication"),
                resolution: get("resolution"),
                withheld_count,
                included_count: get_u("included_count"),
                violation_count,
                payload_digest: get("payload_digest"),
                surface: get("surface"),
                label_name: get("label_name").or_else(|| get("band_label_name")),
                reason: get("reason"),
                envelope_count: array_len("envelope_ids"),
                preset_class: get("preset_class"),
                fallback: get("fallback"),
                source_id_count: array_len("source_ids"),
                allowed_sink,
                declass_scope: get("scope"),
                declass_target,
            };
            rows.push(EgressAuditRow {
                action: ev.action.clone(),
                fields,
            });
        }
        turns.push(EgressAuditTurn { turn_id, rows });
    }

    EgressAuditReport {
        session_id: session_id.to_string(),
        turns,
        total_withheld,
        total_violations,
        total_boundary_refusals,
    }
}
#[cfg(test)]
mod egress_audit_tests {
    use super::*;
    use autonoetic_types::causal_chain::CausalEventRecord;

    fn ev(action: &str, turn: Option<&str>, seq: u64, payload: serde_json::Value) -> CausalEventRecord {
        CausalEventRecord {
            event_id: format!("ev-{seq}"),
            agent_id: "test.agent".to_string(),
            session_id: "sess-test".to_string(),
            turn_id: turn.map(|t| t.to_string()),
            event_seq: seq,
            timestamp: "2026-07-28T00:00:00Z".to_string(),
            category: "egress".to_string(),
            action: action.to_string(),
            status: "active".to_string(),
            enforced_rules: vec![],
            target: None,
            payload: Some(payload.to_string()),
            payload_ref: None,
            evidence_ref: None,
            reason: None,
        }
    }

    #[test]
    fn empty_events_produces_empty_report() {
        let report = build_egress_audit("sess", &[]);
        assert!(report.turns.is_empty());
        assert_eq!(report.total_withheld, 0);
        assert_eq!(report.total_violations, 0);
        assert_eq!(report.total_boundary_refusals, 0);
    }

    #[test]
    fn groups_by_turn_and_counts_totals() {
        let events = vec![
            ev(
                "egress.envelope_labeled",
                Some("turn-1"),
                1,
                serde_json::json!({"tool_name": "email.read", "resolution": "operator_rule"}),
            ),
            ev(
                "egress.envelope_withheld",
                Some("turn-2"),
                2,
                serde_json::json!({"tool_call_id": "tc_1", "target_sink": "remote_model", "indication": "[withheld: 1× email.read result — policy local_only]"}),
            ),
            ev(
                "egress.request_filtered",
                Some("turn-2"),
                3,
                serde_json::json!({"target_sink": "remote_model", "preset": "sonnet", "withheld_count": 1, "included_count": 2, "violation_count": 0}),
            ),
            ev(
                "egress.assertion_violation",
                Some("turn-3"),
                4,
                serde_json::json!({"tool_call_id": "tc_1", "payload_digest": "abc123def456"}),
            ),
            ev(
                "egress.request_filtered",
                Some("turn-3"),
                5,
                serde_json::json!({"target_sink": "remote_model", "preset": "sonnet", "withheld_count": 0, "included_count": 3, "violation_count": 1}),
            ),
        ];
        let report = build_egress_audit("sess-test", &events);
        assert_eq!(report.turns.len(), 3);
        assert_eq!(report.total_withheld, 1); // turn-2 withheld 1
        assert_eq!(report.total_violations, 1); // turn-3 violation 1
        // turn-1 has the labeled event.
        assert_eq!(report.turns[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(report.turns[0].rows.len(), 1);
        assert_eq!(report.turns[0].rows[0].fields.tool_name.as_deref(), Some("email.read"));
        // turn-2 has withheld + request_filtered.
        assert_eq!(report.turns[1].turn_id.as_deref(), Some("turn-2"));
        assert_eq!(report.turns[1].rows.len(), 2);
        // The request_filtered row carries the preset + sink.
        let rf = report.turns[1].rows.iter().find(|r| r.action == "egress.request_filtered").unwrap();
        assert_eq!(rf.fields.preset.as_deref(), Some("sonnet"));
        assert_eq!(rf.fields.target_sink.as_deref(), Some("remote_model"));
        assert_eq!(rf.fields.withheld_count, Some(1));
    }

    #[test]
    fn ignores_non_egress_events() {
        let mut e = ev("egress.envelope_labeled", Some("t1"), 1, serde_json::json!({}));
        e.category = "tool_call".to_string();
        let report = build_egress_audit("sess", &[e]);
        assert!(report.turns.is_empty());
    }

    #[test]
    fn does_not_double_count_request_filtered_and_forwarded() {
        // Regression: both `egress.request_filtered` (canonical) and
        // `egress.request_forwarded` (tracer-side mirror) carry withheld_count
        // / violation_count. The audit must tally only the canonical event, or
        // totals double. Here, one turn has both events each reporting
        // withheld_count=1, violation_count=1 — the totals must be 1/1, not 2/2.
        let events = vec![
            ev(
                "egress.request_filtered",
                Some("t1"),
                1,
                serde_json::json!({"preset": "sonnet", "target_sink": "remote_model", "withheld_count": 1, "included_count": 2, "violation_count": 1}),
            ),
            ev(
                "egress.request_forwarded",
                Some("t1"),
                2,
                serde_json::json!({"model": "sonnet", "target_sink": "remote_model", "withheld_count": 1, "included_count": 2, "violation_count": 1}),
            ),
        ];
        let report = build_egress_audit("sess", &events);
        assert_eq!(report.total_withheld, 1, "must not double-count the mirror event");
        assert_eq!(report.total_violations, 1, "must not double-count the mirror event");
        // Both rows are still rendered (the audit shows both); only the totals dedupe.
        assert_eq!(report.turns[0].rows.len(), 2);
    }

    #[test]
    fn boundary_refused_surfaces_extracted_and_counted() {
        let events = vec![
            ev(
                "egress.boundary_refused",
                Some("turn-1"),
                1,
                serde_json::json!({
                    "surface": "sandbox",
                    "label_name": "local_only",
                    "envelope_ids": ["env_1"],
                    "reason": "session egress taint excludes Network",
                }),
            ),
            ev(
                "egress.boundary_refused",
                Some("turn-2"),
                2,
                serde_json::json!({
                    "surface": "web",
                    "label_name": "local_only",
                    "envelope_ids": [],
                    "reason": "network egress refused",
                }),
            ),
            ev(
                "egress.boundary_refused",
                Some("turn-3"),
                3,
                serde_json::json!({
                    "surface": "compression",
                    "band_label_name": "local_only",
                    "preset_class": "remote",
                    "source_ids": ["msg_1", "msg_2"],
                    "reason": "band ineligible for remote preset",
                    "fallback": "token_budget_truncation",
                }),
            ),
            ev(
                "egress.boundary_refused",
                Some("turn-4"),
                4,
                serde_json::json!({
                    "surface": "mcp",
                    "label_name": "local_only",
                    "envelope_ids": [],
                    "reason": "argument egress labels exclude Network",
                }),
            ),
            ev(
                "egress.boundary_refused",
                Some("turn-5"),
                5,
                serde_json::json!({
                    "surface": "ofp",
                    "label_name": "local_only",
                    "envelope_ids": [],
                    "reason": "session egress label excludes FederatedAgent",
                }),
            ),
            ev(
                "egress.boundary_refused",
                Some("turn-6"),
                6,
                serde_json::json!({
                    "surface": "hooks",
                    "label_name": "local_only",
                    "envelope_ids": [],
                    "reason": "hook delivery refused",
                }),
            ),
        ];
        let report = build_egress_audit("sess", &events);
        assert_eq!(report.total_boundary_refusals, 6);
        let sandbox = report.turns[0].rows[0].fields.clone();
        assert_eq!(sandbox.surface.as_deref(), Some("sandbox"));
        assert_eq!(sandbox.label_name.as_deref(), Some("local_only"));
        assert_eq!(sandbox.envelope_count, Some(1));
        let compression = report.turns[2].rows[0].fields.clone();
        assert_eq!(compression.surface.as_deref(), Some("compression"));
        assert_eq!(compression.label_name.as_deref(), Some("local_only"));
        assert_eq!(compression.preset_class.as_deref(), Some("remote"));
        assert_eq!(compression.source_id_count, Some(2));
        assert_eq!(compression.fallback.as_deref(), Some("token_budget_truncation"));
    }

    #[test]
    fn declassified_fields_extracted() {
        let events = vec![ev(
            "egress.declassified",
            Some("turn-1"),
            1,
            serde_json::json!({
                "target": {"kind": "source_pattern", "value": "session:root-1"},
                "allowed_sink": "network",
                "scope": "root_session",
                "reason": "web_fetch network egress under session egress taint (RFC §8)",
            }),
        )];
        let report = build_egress_audit("sess", &events);
        let fields = &report.turns[0].rows[0].fields;
        assert_eq!(fields.allowed_sink.as_deref(), Some("network"));
        assert_eq!(fields.declass_scope.as_deref(), Some("root_session"));
        assert_eq!(
            fields.declass_target.as_deref(),
            Some("session:root-1")
        );
    }
}
