//! Taint lineage — "why is this tainted, since when, and from what?" (#975).
//!
//! An operator can already see *that* a session is tainted (the `G`/`T` panels
//! show the label) but not *why*. The derivation is fully recorded — every
//! `egress.envelope_labeled` event carries its resolution inputs, and monotonic
//! intersection means each taint has an exact, replayable causal origin — but
//! nothing walked it.
//!
//! This module walks it. Given a root session (and optionally one envelope), it
//! follows `parent_envelope_ids` backwards to the envelopes that introduced the
//! restriction, recording at each hop *which* path applied: an operator rule, a
//! session rule, a bundle floor, argument taint, a stored artifact label, or the
//! configured default.
//!
//! For the email scenario it answers: *this session is local_only because turn
//! 4's `sandbox_exec` read `~/mail/**` and matched rule
//! `sandbox.exec:~/mail/**`, and argument taint carried it into turns 5–7.*
//!
//! ## What the ids mean
//!
//! `parent_envelope_ids` holds **tool-call ids**, not `env_*` ids — the
//! tool-call id is the envelope↔content join key for this phase (message ids are
//! RFC §3.4, still ahead). The walk therefore joins parent ids against each
//! row's `tool_call_id`, and the naming is inherited rather than chosen. Both
//! ids are reported per node so a caller never has to guess which space an id
//! lives in.
//!
//! Like [`crate::egress_audit`], this is **reporting, not inference**: every
//! field comes from a recorded event, the output is content-free metadata, and
//! it lives outside `router.rs` so it is unit-testable against a store directly
//! and the dispatch frame does not grow (#884, #916).

use anyhow::Result;
use autonoetic_types::egress::{EgressLabel, LabeledEnvelopeRow};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::scheduler::gateway_store::GatewayStore;

/// Default cap on envelope events scanned for one lineage walk. Mirrors
/// `labels.list`'s window — the walk reads the same rows.
pub const DEFAULT_LINEAGE_LIMIT: i64 = 5_000;

/// Hard ceiling, whatever a caller asks for. The scan materializes rows, so an
/// unbounded limit is a memory lever on an operator-facing endpoint; callers may
/// narrow the window but not widen it past the default (#992's lesson).
pub const MAX_LINEAGE_LIMIT: i64 = DEFAULT_LINEAGE_LIMIT;

/// Which resolution path gave a hop its label. Derived from the recorded event
/// fields, never guessed: when several applied, the most *specific* one is
/// reported and the rest remain visible in the node's other fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageOrigin {
    /// An operator rule from gateway config matched this source.
    OperatorRule,
    /// A rule from the root session's `egress_policy` matched.
    SessionRule,
    /// Both a global and a session rule matched.
    OperatorAndSessionRule,
    /// The producing bundle declared an `output_label` floor.
    BundleFloor,
    /// A stored artifact's label was intersected in (the artifact read path).
    ArtifactLabel,
    /// The agent workspace's durable label was intersected in (RFC §11, #1001)
    /// — content movement, not a rule, is why this result is labeled.
    WorkspaceLabel,
    /// A prior labeled result in this turn was referenced by the arguments.
    ArgumentTaint,
    /// Accumulated session taint was intersected in.
    SessionTaint,
    /// Nothing matched; the configured default label applied.
    Default,
}

impl LineageOrigin {
    /// Classify one recorded row. Order matters: it runs most-specific first, so
    /// a hop that both matched a rule *and* inherited argument taint is reported
    /// as the rule (the thing an operator can act on) while `parents` still
    /// exposes the inheritance.
    fn classify(row: &LabeledEnvelopeRow) -> Self {
        match row.resolution.as_deref() {
            Some("operator_and_session_rule") => return Self::OperatorAndSessionRule,
            Some("operator_rule") => return Self::OperatorRule,
            Some("session_rule") => return Self::SessionRule,
            _ => {}
        }
        if row.bundle_floor_applied {
            return Self::BundleFloor;
        }
        if !row.artifact_labels_applied.is_empty() {
            return Self::ArtifactLabel;
        }
        if !row.workspace_labels_applied.is_empty() {
            return Self::WorkspaceLabel;
        }
        if !row.parent_envelope_ids.is_empty() || row.taint_applied {
            // `taint_applied` means argument taint from prior envelopes
            // contributed; its lineage is `parent_envelope_ids`.
            return if row.parent_envelope_ids.is_empty() {
                Self::SessionTaint
            } else {
                Self::ArgumentTaint
            };
        }
        Self::Default
    }
}

/// One hop in a taint's derivation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LineageNode {
    /// `env_<id>` — the envelope identity minted at labeling time.
    pub envelope_id: String,
    /// The tool-call id, which is also the key `parent_envelope_ids` join on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub session_id: String,
    pub timestamp: String,
    pub label: EgressLabel,
    /// Why this hop is labeled the way it is.
    pub origin: LineageOrigin,
    /// Operator/session rules whose intersection produced the label.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    /// Stored artifacts whose labels were intersected in at this hop.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifact_ids: Vec<String>,
    /// Agent workspaces whose durable labels were intersected in at this hop
    /// (RFC §11, #1001).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workspace_agents: Vec<String>,
    /// Tool-call ids of the prior results this hop inherited from — the next
    /// hop back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>,
    /// Hops from the queried envelope. `0` is the envelope asked about.
    pub depth: u32,
    /// This hop claims **no** parents — a genuine origin of the restriction.
    ///
    /// Deliberately *not* "no parent was found": a hop that names parents the
    /// walk could not resolve is reported with
    /// [`LineageNode::unresolved_parents`] instead, because calling it an origin
    /// would answer "where did this come from?" confidently and wrongly.
    pub is_origin: bool,
    /// Parent ids this hop names that were **not** present in the scanned rows.
    ///
    /// Non-empty means the chain is cut here, and not necessarily because the
    /// window filled: `egress.envelope_labeled` writes are best-effort (the
    /// emitter logs and continues on a store failure), and P-8.6 retention
    /// prunes old causal events. So a parent can be permanently absent while the
    /// scan had room to spare — a different situation from a full window, and
    /// one a bigger `limit` will not fix.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved_parents: Vec<String>,
}

/// The walk's result: the chain(s) behind a taint, and the origins that explain
/// them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaintLineage {
    pub root_session_id: String,
    /// Envelope ids the walk started from.
    pub roots: Vec<String>,
    /// Every hop reached, nearest-first.
    pub nodes: Vec<LineageNode>,
    /// Envelope ids of the hops that claim no parents — the answer to "where did
    /// this come from". Excludes hops whose parents merely went unresolved.
    pub origins: Vec<String>,
    /// The event scan filled its window, so widening `limit` may reveal more.
    pub truncated: bool,
    /// Some hop named a parent that is not in the scanned rows. Distinct from
    /// [`Self::truncated`] on purpose: `truncated` means "retry with a bigger
    /// window", this means "the record itself is incomplete" (a dropped
    /// best-effort write, or retention pruning) and retrying will not help.
    /// Either way the chain shown is partial, which is what the operator needs
    /// to know before trusting an origin.
    pub incomplete: bool,
    pub limit: i64,
}

/// Walk a session tree's taint lineage.
///
/// `from` selects the starting envelope by **either** `env_*` id or tool-call
/// id — an operator reading an audit row has one or the other and should not
/// have to know which. `None` starts from every restricted envelope in the
/// window, which answers the session-level question ("why is this room
/// tainted?") rather than the per-envelope one.
///
/// A store error propagates: a lineage that silently reports "no origin" because
/// a read failed would be worse than no lineage.
pub fn walk_taint_lineage(
    store: &GatewayStore,
    root_session_id: &str,
    from: Option<&str>,
    limit: Option<i64>,
) -> Result<TaintLineage> {
    let limit = limit.unwrap_or(DEFAULT_LINEAGE_LIMIT).clamp(1, MAX_LINEAGE_LIMIT);
    let rows = store.list_envelope_events_for_root(root_session_id, limit)?;
    let truncated = rows.len() as i64 >= limit;

    // Index both ways: `parent_envelope_ids` holds tool-call ids, while callers
    // and audit rows speak `env_*`.
    let mut by_tool_call: HashMap<&str, &LabeledEnvelopeRow> = HashMap::new();
    let mut by_envelope: HashMap<&str, &LabeledEnvelopeRow> = HashMap::new();
    for row in &rows {
        by_envelope.insert(row.envelope_id.as_str(), row);
        if let Some(tcid) = row.tool_call_id.as_deref() {
            by_tool_call.insert(tcid, row);
        }
    }

    // Starting set.
    let start: Vec<&LabeledEnvelopeRow> = match from {
        Some(id) => by_envelope
            .get(id)
            .or_else(|| by_tool_call.get(id))
            .copied()
            .into_iter()
            .collect(),
        // Session-level question: every envelope that actually restricts.
        None => rows
            .iter()
            .filter(|r| !r.label.is_unrestricted())
            .collect(),
    };

    let mut nodes: Vec<LineageNode> = Vec::new();
    let mut origins: Vec<String> = Vec::new();
    let mut incomplete = false;
    // Guard against a repeated id forming a cycle: argument taint is derived
    // from ids the gateway mints, but a walk must terminate regardless.
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(&LabeledEnvelopeRow, u32)> =
        start.iter().map(|r| (*r, 0u32)).collect();

    while let Some((row, depth)) = queue.pop_front() {
        if !seen.insert(row.envelope_id.clone()) {
            continue;
        }
        let mut reachable_parents: Vec<&LabeledEnvelopeRow> = Vec::new();
        let mut unresolved_parents: Vec<String> = Vec::new();
        for p in &row.parent_envelope_ids {
            match by_tool_call.get(p.as_str()) {
                Some(parent) => reachable_parents.push(parent),
                None => unresolved_parents.push(p.clone()),
            }
        }
        // An origin *claims* no parents. A hop whose named parents could not be
        // resolved is not an origin — it is a cut chain, and saying otherwise
        // invents provenance.
        let is_origin = row.parent_envelope_ids.is_empty();
        if is_origin {
            origins.push(row.envelope_id.clone());
        }
        if !unresolved_parents.is_empty() {
            incomplete = true;
        }
        nodes.push(LineageNode {
            envelope_id: row.envelope_id.clone(),
            tool_call_id: row.tool_call_id.clone(),
            tool_name: row.tool_name.clone(),
            turn_id: row.turn_id.clone(),
            session_id: row.session_id.clone(),
            timestamp: row.timestamp.clone(),
            label: row.label.clone(),
            origin: LineageOrigin::classify(row),
            matched_rules: row.matched_rules.clone(),
            artifact_ids: row.artifact_labels_applied.clone(),
            workspace_agents: row.workspace_labels_applied.clone(),
            parents: row.parent_envelope_ids.clone(),
            depth,
            is_origin,
            unresolved_parents,
        });
        for parent in reachable_parents {
            queue.push_back((parent, depth + 1));
        }
    }

    Ok(TaintLineage {
        root_session_id: root_session_id.to_string(),
        roots: start.iter().map(|r| r.envelope_id.clone()).collect(),
        nodes,
        origins,
        truncated,
        incomplete,
        limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::egress::EgressLabel;

    fn row(
        env: &str,
        tcid: &str,
        turn: &str,
        label: EgressLabel,
        resolution: Option<&str>,
        parents: &[&str],
    ) -> LabeledEnvelopeRow {
        LabeledEnvelopeRow {
            envelope_id: env.to_string(),
            session_id: "sess".to_string(),
            turn_id: Some(turn.to_string()),
            timestamp: "2026-08-01T00:00:00Z".to_string(),
            tool_name: Some("sandbox_exec".to_string()),
            tool_call_id: Some(tcid.to_string()),
            label,
            resolution: resolution.map(str::to_string),
            matched_rules: vec![],
            matched_rule_scopes: vec![],
            parent_envelope_ids: parents.iter().map(|s| s.to_string()).collect(),
            taint_applied: !parents.is_empty(),
            artifact_labels_applied: vec![],
            workspace_labels_applied: vec![],
            bundle_floor_applied: false,
        }
    }

    #[test]
    fn classify_prefers_the_actionable_rule_over_inherited_taint() {
        // A hop that matched a rule *and* inherited taint reports the rule —
        // that is the thing an operator can change — while `parents` keeps the
        // inheritance visible.
        let mut r = row(
            "env_1",
            "tc_1",
            "t1",
            EgressLabel::local_only(),
            Some("operator_rule"),
            &["tc_0"],
        );
        assert_eq!(LineageOrigin::classify(&r), LineageOrigin::OperatorRule);
        r.resolution = None;
        assert_eq!(LineageOrigin::classify(&r), LineageOrigin::ArgumentTaint);
    }

    #[test]
    fn classify_distinguishes_session_taint_from_argument_taint() {
        // `taint_applied` with no parents is session taint; with parents it is
        // argument taint. Reporting both as "taint" would lose the distinction
        // that says whether there is a chain to walk.
        let mut r = row("env_1", "tc_1", "t1", EgressLabel::local_only(), None, &[]);
        r.taint_applied = true;
        assert_eq!(LineageOrigin::classify(&r), LineageOrigin::SessionTaint);

        let with_parents = row(
            "env_2",
            "tc_2",
            "t1",
            EgressLabel::local_only(),
            None,
            &["tc_1"],
        );
        assert_eq!(
            LineageOrigin::classify(&with_parents),
            LineageOrigin::ArgumentTaint
        );
    }

    #[test]
    fn classify_reports_artifact_and_floor_paths() {
        let mut r = row("env_1", "tc_1", "t1", EgressLabel::local_only(), None, &[]);
        r.artifact_labels_applied = vec!["art_x".to_string()];
        assert_eq!(LineageOrigin::classify(&r), LineageOrigin::ArtifactLabel);

        // The workspace label is its own origin: content movement, not a rule,
        // is why this result is labeled (#1001).
        let mut w = row("env_3", "tc_3", "t1", EgressLabel::local_only(), None, &[]);
        w.workspace_labels_applied = vec!["coder.abc".to_string()];
        assert_eq!(LineageOrigin::classify(&w), LineageOrigin::WorkspaceLabel);

        let mut f = row("env_2", "tc_2", "t1", EgressLabel::local_only(), None, &[]);
        f.bundle_floor_applied = true;
        assert_eq!(LineageOrigin::classify(&f), LineageOrigin::BundleFloor);
    }

    #[test]
    fn classify_falls_back_to_default() {
        let r = row("env_1", "tc_1", "t1", EgressLabel::local_only(), None, &[]);
        assert_eq!(LineageOrigin::classify(&r), LineageOrigin::Default);
    }
}
