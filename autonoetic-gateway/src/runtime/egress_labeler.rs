//! Egress labeler — RFC data-envelopes §4.1 (label resolution) + §9.1 (audit).
//!
//! Labels tool results at the tool-result commit boundary. Given the merged
//! operator source rules (global `egress.rules` + session-scoped additions,
//! the latter landing in phase 1b #905), an `EgressLabeler`:
//!
//! 1. Resolves the label for a `(source, path)` pair as the **intersection**
//!    (`restrict`) of every matching rule — no first-match-wins, rules can only
//!    restrict (RFC §4.1). When nothing matches, the configured default
//!    (`unrestricted` by decision) applies.
//! 2. For `sandbox.exec`, derives the `path` via static analysis of the command
//!    + script body (sibling of `RemoteAccessAnalyzer`) — see
//!    [`crate::runtime::egress_path_matcher`].
//! 3. Mints an envelope id (`env_<id>`), builds provenance (tool, args digest,
//!    matched rule names), and emits an `egress.envelope_labeled` causal event
//!    so "why is this labeled?" is always answerable from the chain (RFC §9.1).
//!
//! **Phase status (1c):** labels are computed + audited but not yet enforced
//! at the provider boundary. The chokepoint (withholding, indication
//! substitution, the canary test) lands in #905. Labels are recorded in-memory
//! keyed by tool-call id for #905 to re-key onto `msg_<ulid>` when the envelope
//! ↔ message sidecar lands.
//!
//! Labels are **declared metadata, manipulated only by the gateway** — agents
//! never set, strip, or read them (Lawful-Executor, RFC §14).

use std::collections::HashMap;
use std::sync::Arc;

use autonoetic_types::causal_chain::{default_enforced_rules, CausalEventRecord};
use autonoetic_types::egress::{
    matches_simple_glob, EgressConfig, EgressLabel, EgressRule, Provenance,
};
use autonoetic_types::id_format::short_random_id;

use crate::runtime::egress_path_matcher::{EgressPathMatcher, LabeledPathPattern};
use crate::scheduler::gateway_store::GatewayStore;

/// A label evaluation request at the tool-result boundary.
#[derive(Debug, Clone)]
pub struct LabelRequest<'a> {
    /// The canonical tool name (`email.read`, `sandbox.exec`, `fs.read`, …).
    pub tool: &'a str,
    /// The tool-call arguments JSON, used for the args digest in provenance and
    /// to extract the command/script for `sandbox.exec` path matching.
    pub arguments_json: &'a str,
    /// The tool-call id — keys the in-memory label record for phase 1b.
    pub tool_call_id: &'a str,
}

/// The outcome of labeling one tool result.
#[derive(Debug, Clone)]
pub struct LabelOutcome {
    /// `env_<id>` — referenced by causal events.
    pub envelope_id: String,
    /// The resolved label (intersection of matching rules, or the default).
    pub label: EgressLabel,
    /// Provenance for the `egress.envelope_labeled` event.
    pub provenance: Provenance,
}

impl LabelOutcome {
    /// Whether this outcome restricts content below `unrestricted` — i.e. the
    /// rule set actually labeled something. (In phase 1c this only affects
    /// audit; in 1b it drives withholding.)
    pub fn is_restricted(&self) -> bool {
        !self.label.is_unrestricted()
    }
}

/// The labeler: holds the merged rule set + default label and produces a label
/// per tool result. Cheap to construct; one per session.
#[derive(Debug, Clone)]
pub struct EgressLabeler {
    rules: Vec<EgressRule>,
    default_label: EgressLabel,
    /// Whether source-rule labeling is effectively off (no rules + default
    /// `unrestricted`). Lets the hot path skip provenance/event work entirely.
    inert: bool,
}

impl EgressLabeler {
    /// Build from the operator-global [`EgressConfig`] (session-scoped rules
    /// merge in via [`Self::with_session_rules`]).
    pub fn from_config(config: &EgressConfig) -> Self {
        let default_label = config.default_label.to_label();
        let inert = config.rules.is_empty() && default_label.is_unrestricted();
        Self {
            rules: config.rules.clone(),
            default_label,
            inert,
        }
    }

    /// Merge session-scoped rules (RFC §5.4) — these die with the root session.
    /// Session rules are appended to the operator-global set; intersection is
    /// order-independent, so merge order doesn't matter.
    pub fn with_session_rules(mut self, session_rules: Vec<EgressRule>) -> Self {
        if !session_rules.is_empty() {
            self.rules.extend(session_rules);
            // Re-evaluate inertness: session rules can only restrict, so if the
            // default is unrestricted but session rules exist, we are no longer
            // inert (a rule may match).
            self.inert = false;
        }
        self
    }

    /// Whether the labeler will ever produce a non-`unrestricted` label.
    /// Callers use this to skip the labeling + audit path entirely for
    /// unconfigured deployments (the common case).
    pub fn is_inert(&self) -> bool {
        self.inert
    }

    /// Resolve the label for a tool result, without emitting any event.
    ///
    /// This is the pure core: intersection of all matching rules (RFC §4.1),
    /// falling back to the default. Exposed so callers can label without a
    /// `GatewayStore` (e.g. unit tests).
    pub fn resolve_label(&self, source: &str, path: Option<&str>) -> (EgressLabel, Vec<String>) {
        // Start from the universe (unrestricted) and restrict down. The default
        // is applied last as a floor — it can only restrict the universe, and a
        // matching rule can only restrict further. This matches RFC §4.1:
        // resolution = intersection of (operator rules, default, …).
        let mut label = EgressLabel::unrestricted();
        let mut matched: Vec<String> = Vec::new();
        for rule in &self.rules {
            if rule_matches(rule, source, path) {
                label = label.restrict(&rule.label);
                matched.push(rule_source_key(rule));
            }
        }
        // Apply the configured default as a floor (it restricts the universe to
        // itself when nothing matched, and is a no-op intersection when rules
        // already restricted further — unless the default is stricter).
        label = label.restrict(&self.default_label);
        (label, matched)
    }

    /// Label a tool result end-to-end: resolve, mint envelope id, build
    /// provenance, emit `egress.envelope_labeled`, record in-memory.
    ///
    /// `sandbox_exec_script_body` is the inline script source for `sandbox.exec`
    /// path matching (None for all other tools). Returns `None` when the
    /// labeler is inert (nothing to label) — callers should treat that as
    /// "no envelope, unrestricted".
    pub fn label_tool_result(
        &self,
        req: &LabelRequest<'_>,
        sandbox_exec_script_body: Option<&str>,
        session_id: &str,
        agent_id: &str,
        turn_id: Option<&str>,
        store: Option<&Arc<GatewayStore>>,
        label_records: &mut HashMap<String, EgressLabel>,
    ) -> Option<LabelOutcome> {
        if self.inert {
            return None;
        }

        // Derive the (source, path) pair. For sandbox.exec, the "path" comes
        // from static analysis of command + script body against labeled path
        // patterns (RFC §4.2). For other tools, path is None at this layer —
        // structured tools surface their own path semantics later.
        let (label, matched): (EgressLabel, Vec<String>) = if req.tool == "sandbox.exec" {
            let (cmd, script) = extract_sandbox_command(req.arguments_json, sandbox_exec_script_body);
            let patterns: Vec<LabeledPathPattern> = self
                .rules
                .iter()
                .filter_map(|r| r.path.as_ref().map(|p| LabeledPathPattern::new(p.clone())))
                .collect();
            if patterns.is_empty() {
                self.resolve_label(req.tool, None)
            } else {
                let m = EgressPathMatcher::analyze(&cmd, script.as_deref(), &patterns);
                if m.matched() {
                    // Each matched path-pattern rule restricts; collect which
                    // rules fired for provenance.
                    let mut label = EgressLabel::unrestricted();
                    let mut fired: Vec<String> = Vec::new();
                    for rule in &self.rules {
                        let Some(rule_path) = &rule.path else { continue };
                        // A rule matches if its pattern is among the matched set.
                        if m.matched_patterns.iter().any(|mp| mp == rule_path) {
                            label = label.restrict(&rule.label);
                            fired.push(rule_source_key(rule));
                        }
                    }
                    label = label.restrict(&self.default_label);
                    (label, fired)
                } else {
                    self.resolve_label(req.tool, None)
                }
            }
        } else {
            // Structured tools: extract a `path` argument (common shapes) so
            // path-scoped rules can match. Unknown shapes → None (rule still
            // matches on source alone if it has no `path`).
            let path = extract_structured_path(req.arguments_json);
            self.resolve_label(req.tool, path.as_deref())
        };

        // If the resolved label is unrestricted, there's nothing to audit —
        // emitting an event for every clean tool result would be noise. The
        // default-unrestricted decision means the vast majority of results are
        // unrestricted; we only audit when a rule actually restricted.
        if label.is_unrestricted() {
            return None;
        }

        let envelope_id = short_random_id("env_");
        let args_digest = args_digest_of(req.arguments_json);
        let provenance = Provenance {
            tool: Some(req.tool.to_string()),
            args_digest: Some(args_digest),
            matched_rules: matched.clone(),
            parent_envelope_ids: Vec::new(), // argument-taint: phase 2 (#907)
        };

        // Record for phase 1b (re-keyed onto msg_<ulid> when the sidecar lands).
        label_records.insert(req.tool_call_id.to_string(), label.clone());

        // Best-effort causal event — a failed write is logged, not fatal.
        if let Some(store) = store {
            emit_envelope_labeled_event(
                store,
                &envelope_id,
                req.tool_call_id,
                req.tool,
                &label,
                &provenance,
                session_id,
                agent_id,
                turn_id,
            );
        }

        Some(LabelOutcome {
            envelope_id,
            label,
            provenance,
        })
    }
}

/// Does a rule match a given (source, path)?
///
/// Source supports `*`-suffix globs (`email.*`, `mcp.gmail.*`) and bare names.
/// Path is optional; when the rule has no `path`, it matches all calls to the
/// source. Mirrors [`crate::runtime::disclosure`]'s rule semantics.
fn rule_matches(rule: &EgressRule, source: &str, path: Option<&str>) -> bool {
    if !source_glob_matches(&rule.source, source) {
        return false;
    }
    match (&rule.path, path) {
        (None, _) => true,
        (Some(pattern), Some(actual)) => matches_simple_glob(pattern, actual),
        (Some(_), None) => false,
    }
}

/// `email.*` matches `email.read`; `fs.read` matches `fs.read` only.
fn source_glob_matches(pattern: &str, source: &str) -> bool {
    if pattern.ends_with('*') {
        let prefix = pattern.trim_end_matches('*');
        source.starts_with(prefix)
    } else {
        pattern == source
    }
}

/// Stable string key for a rule in provenance — `source` or `source:path`.
fn rule_source_key(rule: &EgressRule) -> String {
    match &rule.path {
        Some(p) => format!("{}:{}", rule.source, p),
        None => rule.source.clone(),
    }
}

/// Short args digest for provenance (so the event references "which call"
/// without embedding content). SHA-256 → 12 hex chars, matching the repo's
/// stable-id length convention.
fn args_digest_of(arguments_json: &str) -> String {
    autonoetic_types::id_format::hash_and_truncate(arguments_json, 12)
}

/// Extract the `command` and `script`/`code` fields from a sandbox.exec
/// arguments JSON. Returns (command, Some(script)) — both best-effort.
fn extract_sandbox_command(arguments_json: &str, script_body: Option<&str>) -> (String, Option<String>) {
    let parsed: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(v) => v,
        Err(_) => return (String::new(), script_body.map(|s| s.to_string())),
    };
    let cmd = parsed
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Inline script may arrive as `code` or `script`, or be passed out-of-band
    // via `sandbox_exec_script_body` (the manifest-declared script path).
    let script = parsed
        .get("code")
        .and_then(|v| v.as_str())
        .or_else(|| parsed.get("script").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .or_else(|| script_body.map(|s| s.to_string()));
    (cmd, script)
}

/// Extract a filesystem path from a structured tool's arguments JSON, for
/// path-scoped rule matching on tools like `fs.read`. Tries the common arg
/// names (`path`, `file`, `file_path`, `target`). Returns None when no path-like
/// argument is present, so source-only rules still match.
fn extract_structured_path(arguments_json: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments_json).ok()?;
    for key in ["path", "file", "file_path", "target"] {
        if let Some(s) = parsed.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Emit the `egress.envelope_labeled` causal event (RFC §9.1).
///
/// Content-free metadata only — envelope id, tool, label, matched rules, args
/// digest. Never the tool-result payload.
fn emit_envelope_labeled_event(
    store: &Arc<GatewayStore>,
    envelope_id: &str,
    tool_call_id: &str,
    tool: &str,
    label: &EgressLabel,
    provenance: &Provenance,
    session_id: &str,
    agent_id: &str,
    turn_id: Option<&str>,
) {
    let payload = serde_json::json!({
        "envelope_id": envelope_id,
        "tool_call_id": tool_call_id,
        "tool_name": tool,
        // Serialize the label as its sink-set (serde-transparent BTreeSet<Sink>,
        // snake_case). This is the same wire shape the chokepoint will compare
        // against in phase 1b.
        "label": serde_json::to_value(label).unwrap_or(serde_json::Value::Null),
        "matched_rules": provenance.matched_rules,
        "args_digest": provenance.args_digest,
        // Explicitly mark the resolution path so the audit answers "why?".
        "resolution": if provenance.matched_rules.is_empty() {
            "default"
        } else {
            "operator_rule"
        },
    });
    let event = CausalEventRecord {
        event_id: format!("egress-labeled-{}", uuid::Uuid::new_v4()),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.map(|t| t.to_string()),
        event_seq: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "egress".to_string(),
        action: "egress.envelope_labeled".to_string(),
        status: "active".to_string(),
        // Phase 1c carries only the baseline attribution rule. The
        // constitution clause for the label-plane invariant is phase 5 (#910).
        enforced_rules: default_enforced_rules(),
        target: Some(envelope_id.to_string()),
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: Some("egress_label_resolved".to_string()),
    };
    if let Err(e) = store.create_causal_event(&event) {
        tracing::warn!(
            target: "egress_labeler",
            error = %e,
            envelope_id = %envelope_id,
            tool = %tool,
            "failed to emit egress.envelope_labeled causal event"
        );
    }
}

#[cfg(test)]
mod tests {
    use autonoetic_types::egress::{EgressConfig, EgressLabel, EgressRule, NamedEgressLabel, Sink};

    use super::*;

    fn rule(source: &str, path: Option<&str>, label: NamedEgressLabel) -> EgressRule {
        EgressRule {
            source: source.to_string(),
            path: path.map(|s| s.to_string()),
            label: label.to_label(),
        }
    }

    fn cfg(rules: Vec<EgressRule>) -> EgressConfig {
        EgressConfig {
            rules,
            ..Default::default()
        }
    }

    // ── source matching ──────────────────────────────────────────────────

    #[test]
    fn bare_source_matches_exact() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("fs.read", None, NamedEgressLabel::LocalOnly)]));
        let (label, matched) = l.resolve_label("fs.read", None);
        assert_eq!(label, EgressLabel::local_only());
        assert_eq!(matched, vec!["fs.read"]);
    }

    #[test]
    fn source_glob_matches_suffix() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("email.*", None, NamedEgressLabel::LocalOnly)]));
        let (label, _) = l.resolve_label("email.read", None);
        assert_eq!(label, EgressLabel::local_only());
        let (label, _) = l.resolve_label("email.send", None);
        assert_eq!(label, EgressLabel::local_only());
        // Non-matching source stays unrestricted (default).
        let (label, _) = l.resolve_label("fs.read", None);
        assert!(label.is_unrestricted());
    }

    #[test]
    fn mcp_server_glob_matches() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("mcp.gmail.*", None, NamedEgressLabel::LocalOnly)]));
        let (label, _) = l.resolve_label("mcp.gmail.send_message", None);
        assert_eq!(label, EgressLabel::local_only());
        let (label, _) = l.resolve_label("mcp.outlook.send", None);
        assert!(label.is_unrestricted());
    }

    // ── path narrowing ───────────────────────────────────────────────────

    #[test]
    fn path_narrows_match() {
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("fs.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        // path supplied → matches
        let (label, _) = l.resolve_label("fs.read", Some("~/mail/inbox/1"));
        assert_eq!(label, EgressLabel::local_only());
        // different path → no match → unrestricted
        let (label, _) = l.resolve_label("fs.read", Some("/etc/passwd"));
        assert!(label.is_unrestricted());
        // no path supplied with a path-scoped rule → no match (conservative)
        let (label, _) = l.resolve_label("fs.read", None);
        assert!(label.is_unrestricted());
    }

    // ── intersection is monotonic ────────────────────────────────────────

    #[test]
    fn multiple_matching_rules_intersect_never_widen() {
        // Two rules both match `fs.read` with `~/mail/**`: one local_only,
        // one no_remote_model. Intersection = local_only (the stricter).
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("fs.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
            rule("fs.read", Some("~/mail/**"), NamedEgressLabel::NoRemoteModel),
        ]));
        let (label, matched) = l.resolve_label("fs.read", Some("~/mail/inbox/1"));
        assert_eq!(label, EgressLabel::local_only());
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn default_label_applies_when_no_rule_matches() {
        let mut c = cfg(vec![]);
        c.default_label = NamedEgressLabel::LocalOnly;
        let l = EgressLabeler::from_config(&c);
        let (label, matched) = l.resolve_label("anything", None);
        assert_eq!(label, EgressLabel::local_only());
        assert!(matched.is_empty());
    }

    #[test]
    fn default_unrestricted_is_inert() {
        let l = EgressLabeler::from_config(&EgressConfig::default());
        assert!(l.is_inert());
        let l2 = EgressLabeler::from_config(&cfg(vec![rule("email.*", None, NamedEgressLabel::LocalOnly)]));
        assert!(!l2.is_inert());
    }

    // ── session rules ────────────────────────────────────────────────────

    #[test]
    fn session_rules_merge_and_restrict() {
        let l = EgressLabeler::from_config(&cfg(vec![]))
            .with_session_rules(vec![rule("slack.*", None, NamedEgressLabel::NoRemoteModel)]);
        assert!(!l.is_inert());
        let (label, _) = l.resolve_label("slack.read", None);
        assert_eq!(label, EgressLabel::no_remote_model());
    }

    // ── inert fast path ──────────────────────────────────────────────────

    #[test]
    fn label_tool_result_returns_none_when_inert() {
        let l = EgressLabeler::from_config(&EgressConfig::default());
        let mut records = HashMap::new();
        let req = LabelRequest {
            tool: "fs.read",
            arguments_json: "{}",
            tool_call_id: "tc_1",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &mut records);
        assert!(out.is_none());
        assert!(records.is_empty());
    }

    #[test]
    fn label_tool_result_returns_none_when_label_is_unrestricted() {
        // A rule exists but doesn't match this source → unrestricted → no event.
        let l = EgressLabeler::from_config(&cfg(vec![rule("email.*", None, NamedEgressLabel::LocalOnly)]));
        let mut records = HashMap::new();
        let req = LabelRequest {
            tool: "fs.read",
            arguments_json: "{}",
            tool_call_id: "tc_1",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &mut records);
        assert!(out.is_none());
        assert!(records.is_empty());
    }

    #[test]
    fn label_tool_result_emits_envelope_when_restricted() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("email.read", None, NamedEgressLabel::LocalOnly)]));
        let mut records = HashMap::new();
        let req = LabelRequest {
            tool: "email.read",
            arguments_json: r#"{"box":"inbox"}"#,
            tool_call_id: "tc_42",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &mut records).expect("restricted");
        assert!(out.is_restricted());
        assert!(out.envelope_id.starts_with("env_"));
        assert_eq!(out.label, EgressLabel::local_only());
        assert_eq!(out.provenance.tool.as_deref(), Some("email.read"));
        assert_eq!(out.provenance.matched_rules, vec!["email.read"]);
        // In-memory record keyed by tool-call id for phase 1b.
        assert_eq!(records.get("tc_42"), Some(&EgressLabel::local_only()));
    }

    #[test]
    fn sandbox_exec_with_labeled_path_is_restricted() {
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        let mut records = HashMap::new();
        let req = LabelRequest {
            tool: "sandbox.exec",
            arguments_json: r#"{"command":"cat ~/mail/inbox/1"}"#,
            tool_call_id: "tc_exec",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &mut records).expect("restricted");
        assert_eq!(out.label, EgressLabel::local_only());
        assert!(out.provenance.matched_rules.iter().any(|r| r.contains("~/mail/**")));
    }

    #[test]
    fn sandbox_exec_clean_command_is_unrestricted() {
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        let mut records = HashMap::new();
        let req = LabelRequest {
            tool: "sandbox.exec",
            arguments_json: r#"{"command":"echo hello"}"#,
            tool_call_id: "tc_exec",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &mut records);
        assert!(out.is_none(), "clean exec should not be labeled");
    }
}
