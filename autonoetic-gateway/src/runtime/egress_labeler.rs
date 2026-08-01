//! Egress labeler — RFC data-envelopes §4.1 (label resolution) + §9.1 (audit).
//!
//! Labels tool results at the tool-result commit boundary. Given the merged
//! operator source rules (global `egress.rules` + the root session's
//! `egress_policy.rules`), an `EgressLabeler`:
//!
//! 1. Resolves the label for a `(source, path)` pair as the **intersection**
//!    (`restrict`) of every matching rule — no first-match-wins, rules can only
//!    restrict (RFC §4.1). When nothing matches, the configured default
//!    (`unrestricted` by decision) applies.
//! 2. For exec-shaped tools (`sandbox.exec`, `artifact.exec`), derives the
//!    `path` via static analysis of the command **and its dependency sources**
//!    — the artifact bundle it runs, the workspace scripts it names (sibling of
//!    `RemoteAccessAnalyzer`) — see [`crate::runtime::egress_path_matcher`].
//! 3. Mints an envelope id (`env_<id>`), builds provenance (tool, args digest,
//!    matched rule names), and emits an `egress.envelope_labeled` causal event
//!    carrying every resolution input, so "why is this labeled?" is always
//!    answerable from the chain (RFC §9.1).
//!
//! Rule sources are matched **normalized** (`autonoetic_types::egress::
//! source_pattern_matches`): the RFC and every operator-facing example write
//! `sandbox.exec` / `mcp.gmail.*`, while the runtime's canonical tool names are
//! `sandbox_exec` / `mcp_gmail_send_message`. Either spelling matches.
//!
//! Labels are **declared metadata, manipulated only by the gateway** — agents
//! never set, strip, or read them (Lawful-Executor, RFC §14).

use std::sync::Arc;

use autonoetic_types::background::{
    GrantScope, UserInteraction, UserInteractionKind, UserInteractionOption, UserInteractionStatus,
};
use autonoetic_types::causal_chain::{default_enforced_rules, CausalEventRecord};
use autonoetic_types::egress::{
    label_display_name, matches_simple_glob, source_pattern_matches, EgressClass, EgressConfig,
    EgressLabel, EgressRule, EgressSessionPolicy, Provenance, Sink,
};
use autonoetic_types::id_format::short_random_id;
use serde::{Deserialize, Serialize};

use crate::runtime::egress_path_matcher::{
    collect_exec_dependency_sources, EgressPathMatcher, ExecSourceContext, LabeledPathPattern,
};
use crate::scheduler::gateway_store::GatewayStore;

/// A label evaluation request at the tool-result boundary.
#[derive(Debug, Clone)]
pub struct LabelRequest<'a> {
    /// The canonical tool name (`email_read`, `sandbox_exec`, `fs_read`, …).
    /// Rule sources are matched against it normalized, so operator rules may
    /// use the dotted spelling.
    pub tool: &'a str,
    /// The tool-call arguments JSON, used for the args digest in provenance and
    /// to extract the command/script for exec path matching.
    pub arguments_json: &'a str,
    /// The tool-call id. Until message ids land (RFC §3.4, phase 2 #907) this
    /// is the join key between an envelope and the content it labels.
    pub tool_call_id: &'a str,
}

/// A prior labeled result held by the session for argument-taint detection
/// (RFC §4.1 path 3).
///
/// The labeler scans a tool call's arguments for two deterministic signals:
/// 1. **Handle reference** — the prior `tool_call_id` (the map key) appears in
///    the args JSON.
/// 2. **Verbatim content** — `content_snippet` (when present) appears as a
///    substring of the args. Bounded and defeated by paraphrase — a tripwire,
///    not a proof.
#[derive(Debug, Clone)]
pub struct PriorLabeledResult {
    /// The label of the prior labeled result.
    pub label: EgressLabel,
    /// Optional truncated content from the prior result, for bounded verbatim
    /// taint detection. `None` when unavailable or too large.
    pub content_snippet: Option<String>,
}

/// Where a rule came from. Recorded per matched rule so the audit answers "was
/// this the operator's standing policy or something this session declared?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    /// `egress.rules` in the gateway config.
    Global,
    /// `egress_policy.rules` on the root session — dies with the session.
    Session,
}

impl RuleScope {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleScope::Global => "global",
            RuleScope::Session => "session",
        }
    }
}

/// One rule that contributed to a resolved label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRule {
    /// Stable key — `source` or `source:path`.
    pub key: String,
    pub scope: RuleScope,
}

/// A resolved label plus everything that went into resolving it (RFC §9.1:
/// "every labeling decision recorded with its inputs").
#[derive(Debug, Clone)]
pub struct Resolution {
    pub label: EgressLabel,
    pub matched: Vec<MatchedRule>,
    /// The configured path **patterns** that fired — never the observed path.
    /// A real path is content-adjacent (`~/mail/from-alice-re-divorce.eml`
    /// says plenty on its own) and every trace artifact here is content-free by
    /// design (RFC §9); the pattern is operator-authored, so it is safe to
    /// record and is what actually explains the decision.
    pub paths: Vec<String>,
    /// Whether the bundle-declared floor (RFC §4.1 path 2) contributed to this
    /// resolution. Recorded for the audit event so "why is this labeled?" is
    /// answerable when the floor was the only restricting input.
    pub bundle_floor_applied: bool,
    /// Whether argument-taint contributed to this resolution (RFC §4.1 path 3).
    /// `parent_envelope_ids` on the provenance carries the actual lineage.
    pub taint_applied: bool,
    /// Artifact ids whose stored labels contributed to this resolution (RFC §4.5
    /// artifact birth point, #980). Recorded so "why is this labeled?" names the
    /// artifact when a round trip through the content-addressed store — not a
    /// rule — is what restricted the result.
    pub artifact_labels_applied: Vec<String>,
    /// Agent ids whose workspace labels contributed to this resolution (RFC §11,
    /// #1001). Empty for non-exec tools, which never run in a workspace.
    pub workspace_labels_applied: Vec<String>,
}

impl Resolution {
    /// The resolution path taken, as recorded in the causal event.
    fn kind(&self) -> &'static str {
        let global = self.matched.iter().any(|m| m.scope == RuleScope::Global);
        let session = self.matched.iter().any(|m| m.scope == RuleScope::Session);
        match (global, session) {
            (true, true) => "operator_and_session_rule",
            (true, false) => "operator_rule",
            (false, true) => "session_rule",
            (false, false) => "default",
        }
    }

    fn rule_keys(&self) -> Vec<String> {
        self.matched.iter().map(|m| m.key.clone()).collect()
    }
}

/// A rule tagged with where it came from.
#[derive(Debug, Clone)]
struct ScopedRule {
    rule: EgressRule,
    scope: RuleScope,
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
/// per tool result. Cheap to construct; one per turn.
#[derive(Debug, Clone)]
pub struct EgressLabeler {
    rules: Vec<ScopedRule>,
    /// Effective default = global default ∩ session default.
    default_label: EgressLabel,
    /// The operator-global default, kept for the audit event.
    global_default: EgressLabel,
    /// Whether the session policy narrowed the default, for the audit event.
    session_default_applied: bool,
    /// The bundle-declared output floor (RFC §4.1 path 2). `None` when no
    /// floor is declared. When present, every resolution intersects this label
    /// — a floor restricts the bundle's own outputs, never widens operator
    /// policy.
    bundle_floor: Option<EgressLabel>,
    /// Whether source-rule labeling is effectively off (no rules + no floor +
    /// default `unrestricted`). Lets the hot path skip provenance/event work
    /// entirely.
    inert: bool,
}

impl EgressLabeler {
    /// Build from the operator-global [`EgressConfig`] (session-scoped rules
    /// merge in via [`Self::with_session_policy`]).
    pub fn from_config(config: &EgressConfig) -> Self {
        let default_label = config.default_label.to_label();
        let inert = config.rules.is_empty() && default_label.is_unrestricted();
        Self {
            rules: config
                .rules
                .iter()
                .cloned()
                .map(|rule| ScopedRule {
                    rule,
                    scope: RuleScope::Global,
                })
                .collect(),
            default_label: default_label.clone(),
            global_default: default_label,
            session_default_applied: false,
            bundle_floor: None,
            inert,
        }
    }

    /// Apply a bundle-declared output floor (RFC §4.1 path 2).
    ///
    /// A floor is a label declared in the agent's SKILL.md under
    /// `metadata.autonoetic.egress.output_label`. It **restricts** the bundle's
    /// own outputs: every resolution intersects it alongside operator rules,
    /// and it can never widen what operator policy restricted. A floor clears
    /// inertness — even with no operator rules, a bundle-only floor makes the
    /// labeler non-inert so the floor is actually applied.
    pub fn with_manifest_floor(mut self, floor: Option<EgressLabel>) -> Self {
        self.bundle_floor = floor;
        if self.bundle_floor.is_some() {
            self.inert = false;
        }
        self
    }

    /// Merge the root session's `egress_policy` (RFC §5.4) — it dies with the
    /// session. Session rules are appended to the operator-global set;
    /// intersection is order-independent, so merge order doesn't matter, and a
    /// session default can only *restrict* the global one.
    pub fn with_session_policy(mut self, policy: &EgressSessionPolicy) -> Self {
        if !policy.rules.is_empty() {
            self.rules
                .extend(policy.rules.iter().cloned().map(|rule| ScopedRule {
                    rule,
                    scope: RuleScope::Session,
                }));
            // Re-evaluate inertness: a session rule may match, so the fast path
            // no longer applies.
            self.inert = false;
        }
        if let Some(default) = policy.default_label {
            let narrowed = self.default_label.clone().restrict(&default.to_label());
            if narrowed != self.default_label {
                self.default_label = narrowed;
                self.session_default_applied = true;
                self.inert = self.inert && self.default_label.is_unrestricted();
            }
        }
        self
    }

    /// Merge session-scoped rules without a default override.
    pub fn with_session_rules(self, session_rules: Vec<EgressRule>) -> Self {
        self.with_session_policy(&EgressSessionPolicy {
            rules: session_rules,
            default_label: None,
            provider_constraint: None,
        })
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
    pub fn resolve_label(&self, source: &str, path: Option<&str>) -> Resolution {
        // Start from the universe (unrestricted) and restrict down. The default
        // is applied last as a floor — it can only restrict the universe, and a
        // matching rule can only restrict further. This matches RFC §4.1:
        // resolution = intersection of (operator rules, default, …).
        let mut label = EgressLabel::unrestricted();
        let mut matched: Vec<MatchedRule> = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        for scoped in &self.rules {
            if rule_matches(&scoped.rule, source, path) {
                label = label.restrict(&scoped.rule.label);
                matched.push(MatchedRule {
                    key: rule_source_key(&scoped.rule),
                    scope: scoped.scope,
                });
                if let Some(pattern) = &scoped.rule.path {
                    if !paths.iter().any(|p| p == pattern) {
                        paths.push(pattern.clone());
                    }
                }
            }
        }
        // Apply the configured default as a floor (it restricts the universe to
        // itself when nothing matched, and is a no-op intersection when rules
        // already restricted further — unless the default is stricter).
        label = label.restrict(&self.default_label);

        // Apply the bundle-declared floor (RFC §4.1 path 2). Intersection only —
        // a floor restricts the bundle's own outputs, never widens.
        let bundle_floor_applied = if let Some(floor) = &self.bundle_floor {
            label = label.restrict(floor);
            true
        } else {
            false
        };

        Resolution {
            label,
            matched,
            paths,
            bundle_floor_applied,
            taint_applied: false,
            artifact_labels_applied: Vec::new(),
            workspace_labels_applied: Vec::new(),
        }
    }

    /// Label a tool result end-to-end: resolve, intersect argument taint,
    /// mint envelope id, build provenance, emit `egress.envelope_labeled`.
    ///
    /// `exec_ctx` locates an exec-shaped call's dependency sources (artifact
    /// bundle, workspace scripts) so the static path matcher can see what the
    /// command alone doesn't show; `None` restricts the scan to the command and
    /// any inline script in the arguments. Returns `None` when the labeler is
    /// inert or the result is unrestricted — callers treat that as "no
    /// envelope".
    ///
    /// `prior_labels` carries the accumulated prior labeled results in this
    /// turn/session (RFC §4.1 path 3), keyed by `tool_call_id`. The labeler
    /// scans the arguments for references to these prior envelopes — by handle
    /// (tool_call_id) and by bounded verbatim content match — and intersects
    /// their labels. Empty for the first tool call in a turn.
    ///
    /// The durable record of the labeling decision is the
    /// `egress.envelope_labeled` causal event (persisted via `store`); the
    /// returned [`LabelOutcome`] gives the caller the envelope id + label for
    /// in-turn use (the chokepoint's `tool_call_id → label` map).
    pub fn label_tool_result(
        &self,
        req: &LabelRequest<'_>,
        exec_ctx: Option<&ExecSourceContext<'_>>,
        session_id: &str,
        agent_id: &str,
        turn_id: Option<&str>,
        store: Option<&Arc<GatewayStore>>,
        prior_labels: &std::collections::HashMap<String, PriorLabeledResult>,
    ) -> Option<LabelOutcome> {
        // Exec-shaped calls escape the inert fast-path (RFC §11, #1001): the
        // workspace label is *dynamic* store state, invisible to the static
        // `inert` computation, and an agent workspace can carry a taint long
        // after the rules that created it are gone (or in a session that never
        // had any). A rules-less session reading a laundered workspace path
        // must still be labeled. The resolution still returns unrestricted —
        // and `None` — when the workspace carries no label, so the cost is a
        // cheap resolve per exec, not a behavior change for clean ones.
        if self.inert && !is_exec_shaped(req.tool) {
            return None;
        }

        // Derive the (source, path) pair. For exec-shaped tools the "path"
        // comes from static analysis of the command and its dependency sources
        // against labeled path patterns (RFC §4.2). For other tools, path comes
        // from a structured argument.
        let mut resolution = if is_exec_shaped(req.tool) {
            self.resolve_exec_label(req, exec_ctx)
        } else {
            // Structured tools: extract a `path` argument (common shapes) so
            // path-scoped rules can match. Unknown shapes → None (rule still
            // matches on source alone if it has no `path`).
            let path = extract_structured_path(req.arguments_json);
            self.resolve_label(req.tool, path.as_deref())
        };

        // Argument taint (RFC §4.1 path 3): scan the arguments for references
        // to prior labeled envelopes. Two deterministic signals:
        // 1. Handle reference — prior tool_call_id appears in the args JSON.
        // 2. Verbatim content — prior result content snippet appears in args.
        let (taint_label, mut parent_envelope_ids) =
            argument_taint_from_prior(req.arguments_json, prior_labels);
        if !parent_envelope_ids.is_empty() {
            resolution.label = resolution.label.restrict(&taint_label);
            resolution.taint_applied = true;
        }

        // Artifact taint (RFC §4.5 artifact birth point, #980): any artifact
        // named in the arguments contributes its stored label. `prior_labels`
        // only reaches back across *this turn*, so without this an artifact
        // built by a tainted session and read by a later — or entirely
        // different — session came back unlabeled. Artifacts outlive the session
        // that produced them, which is what made this a laundering path through
        // the §5.5 compartment firebreak.
        //
        // Covers every artifact-naming tool at once (inspect, read_file, exec,
        // prepare) because it keys on the arguments rather than the tool name.
        // For exec-shaped calls it composes with the existing static path
        // analysis: the bundle's labeled *paths* and the artifact's own label are
        // independent inputs and both intersect.
        if let Some(store) = store {
            let (artifact_label, applied) =
                artifact_taint_from_store(req.arguments_json, store.as_ref());
            if !applied.is_empty() {
                resolution.label = resolution.label.restrict(&artifact_label);
                // NOT `taint_applied`: that flag means "argument taint from prior
                // envelopes contributed", and its lineage lives in
                // `parent_envelope_ids`. Setting it here would emit
                // `taint_applied: true` with an empty `parent_envelope_ids`,
                // which reads as a contradiction to anyone auditing the event.
                // `artifact_labels_applied` carries this path's lineage instead.
                resolution.artifact_labels_applied = applied;
            }
        }

        // Workspace taint (RFC §11, #1001): the agent workspace is a durable
        // object carrying a durable label. Content moved into it (unzip, cp,
        // git clone …) cannot be followed by path rules — that is the whole
        // laundering path #988 names — so any exec running in the workspace
        // intersects its label, exactly like an artifact's. Only exec-shaped
        // tools run in a workspace; structured tools stay rule-only.
        let exec_shaped = is_exec_shaped(req.tool);
        let mut workspace_read_failed = false;
        if exec_shaped {
            if let Some(store) = store {
                let (workspace_label, applied, read_failed) =
                    workspace_taint_from_store(agent_id, store.as_ref());
                workspace_read_failed = read_failed;
                if !applied.is_empty() {
                    resolution.label = resolution.label.restrict(&workspace_label);
                    resolution.workspace_labels_applied = applied;
                }
            }
        }

        // Restrict the workspace when this exec resolved restricted: content
        // tainted by rules, argument taint, an artifact, or the workspace's own
        // prior label is now (or was already) in the workspace, so the durable
        // label tightens. Coarse by design — the workspace is the labeled unit,
        // not the paths inside it, which is what removes the evasion surface.
        // Best-effort write: a failed restriction is logged, never fatal (the
        // read side fails closed regardless). A read error that failed closed
        // is NOT written back — a transient read failure must not become a
        // durable workspace restriction.
        if exec_shaped && !workspace_read_failed && !resolution.label.is_unrestricted() {
            if let Some(store) = store {
                if let Err(e) = store.restrict_workspace_egress_label(agent_id, &resolution.label) {
                    tracing::warn!(
                        target: "egress",
                        error = %e,
                        agent_id = %agent_id,
                        "workspace egress label restrict failed"
                    );
                }
            }
        }

        // If the resolved label is unrestricted, there's nothing to audit —
        // emitting an event for every clean tool result would be noise. The
        // default-unrestricted decision means the vast majority of results are
        // unrestricted; we only audit when a rule actually restricted.
        if resolution.label.is_unrestricted() {
            return None;
        }

        let envelope_id = short_random_id("env_");
        let args_digest = args_digest_of(req.arguments_json);
        let provenance = Provenance {
            tool: Some(req.tool.to_string()),
            args_digest: Some(args_digest),
            matched_rules: resolution.rule_keys(),
            parent_envelope_ids,
        };

        // Best-effort causal event — the durable record of this labeling
        // decision. A failed write is logged, not fatal.
        if let Some(store) = store {
            self.emit_envelope_labeled_event(
                store,
                &envelope_id,
                req,
                &resolution,
                &provenance,
                session_id,
                agent_id,
                turn_id,
            );
        }

        Some(LabelOutcome {
            envelope_id,
            label: resolution.label,
            provenance,
        })
    }

    /// Resolve an exec-shaped call by static analysis (RFC §4.2).
    ///
    /// Only rules whose `source` matches this tool apply — a path-bearing
    /// `fs.read` rule must not label a `sandbox.exec` result just because the
    /// command touched the same path. The static analyzer is source-agnostic;
    /// source filtering belongs here.
    fn resolve_exec_label(
        &self,
        req: &LabelRequest<'_>,
        exec_ctx: Option<&ExecSourceContext<'_>>,
    ) -> Resolution {
        let (cmd, inline_script) = extract_sandbox_command(req.arguments_json);
        let applicable: Vec<&ScopedRule> = self
            .rules
            .iter()
            .filter(|s| source_pattern_matches(&s.rule.source, req.tool))
            .collect();
        let patterns: Vec<LabeledPathPattern> = applicable
            .iter()
            .filter_map(|s| {
                s.rule
                    .path
                    .as_ref()
                    .map(|p| LabeledPathPattern::new(p.clone()))
            })
            .collect();
        if patterns.is_empty() {
            // No source+path rule applies; fall back to source-only matching
            // (a source-only exec rule still applies).
            return self.resolve_label(req.tool, None);
        }

        // Gather everything the exec will actually run: the command, any inline
        // script in the arguments, and — the dependency half of RFC §4.2 — the
        // artifact bundle and workspace scripts it names. Only reached when a
        // path-bearing rule exists, so unconfigured deployments never pay for
        // the reads.
        let mut sources: Vec<String> = Vec::new();
        if let Some(script) = inline_script {
            sources.push(script);
        }
        if let Some(ctx) = exec_ctx {
            sources.extend(collect_exec_dependency_sources(req.arguments_json, &cmd, ctx));
        }
        let source_refs: Vec<&str> = sources.iter().map(String::as_str).collect();

        let m = EgressPathMatcher::analyze_sources(&cmd, &source_refs, &patterns);
        if !m.matched() {
            return self.resolve_label(req.tool, None);
        }

        // Each matched path-pattern rule restricts; collect which rules fired
        // for provenance. Source-only rules restrict too (intersection is
        // order-independent).
        let mut label = EgressLabel::unrestricted();
        let mut matched: Vec<MatchedRule> = Vec::new();
        for scoped in &applicable {
            let fires = match &scoped.rule.path {
                // Source-only rule (no path) — always applies.
                None => true,
                // A path-bearing rule fires iff its pattern matched.
                Some(rule_path) => m.matched_patterns.iter().any(|mp| mp == rule_path),
            };
            if fires {
                label = label.restrict(&scoped.rule.label);
                matched.push(MatchedRule {
                    key: rule_source_key(&scoped.rule),
                    scope: scoped.scope,
                });
            }
        }
        // Apply default + bundle floor (same as resolve_label).
        label = label.restrict(&self.default_label);
        let bundle_floor_applied = if let Some(floor) = &self.bundle_floor {
            label = label.restrict(floor);
            true
        } else {
            false
        };

        Resolution {
            label,
            matched,
            paths: m.matched_patterns,
            bundle_floor_applied,
            taint_applied: false,
            artifact_labels_applied: Vec::new(),
            workspace_labels_applied: Vec::new(),
        }
    }

    /// Emit the `egress.envelope_labeled` causal event (RFC §9.1).
    ///
    /// Content-free metadata only — envelope id, tool, label, the rules that
    /// matched and where they came from, the default in force, the args digest.
    /// Never the tool-result payload. Together these are the complete input set
    /// of the resolution, which is what makes "why is this envelope labeled?"
    /// answerable from the chain alone.
    #[allow(clippy::too_many_arguments)]
    fn emit_envelope_labeled_event(
        &self,
        store: &Arc<GatewayStore>,
        envelope_id: &str,
        req: &LabelRequest<'_>,
        resolution: &Resolution,
        provenance: &Provenance,
        session_id: &str,
        agent_id: &str,
        turn_id: Option<&str>,
    ) {
        let payload = serde_json::json!({
            "envelope_id": envelope_id,
            // The envelope ↔ content binding in this phase. Message ids
            // (`msg_<ulid>`, RFC §3.4) land with phase 2 (#907).
            "tool_call_id": req.tool_call_id,
            "tool_name": req.tool,
            // Serialize the label as its sink-set (serde-transparent
            // BTreeSet<Sink>, snake_case) — the same wire shape the chokepoint
            // compares against.
            "label": serde_json::to_value(&resolution.label).unwrap_or(serde_json::Value::Null),
            "matched_rules": provenance.matched_rules,
            "matched_rule_scopes": resolution
                .matched
                .iter()
                .map(|m| serde_json::json!({ "rule": m.key, "scope": m.scope.as_str() }))
                .collect::<Vec<_>>(),
            "matched_paths": resolution.paths,
            "args_digest": provenance.args_digest,
            // The floor every resolution intersects against, and whether the
            // session narrowed it — without these, a label produced by the
            // default alone is unexplained.
            "default_label": serde_json::to_value(&self.default_label)
                .unwrap_or(serde_json::Value::Null),
            "global_default_label": serde_json::to_value(&self.global_default)
                .unwrap_or(serde_json::Value::Null),
            "session_default_applied": self.session_default_applied,
            // Whether the bundle-declared floor (RFC §4.1 path 2) contributed.
            "bundle_floor_applied": resolution.bundle_floor_applied,
            // Argument taint (RFC §4.1 path 3) — parent envelope ids whose
            // labels were intersected into this resolution.
            "parent_envelope_ids": provenance.parent_envelope_ids,
            "taint_applied": resolution.taint_applied,
            // Artifact birth point (RFC §4.5) — artifact ids whose stored labels
            // were intersected in. Named because a round trip through the
            // content-addressed store, not a rule, may be the only reason this
            // result is labeled, and an operator asking "why?" needs to see it.
            "artifact_labels_applied": resolution.artifact_labels_applied,
            // Workspace (RFC §11) — agent ids whose workspace labels were
            // intersected in. The workspace is a durable object: content moved
            // into it cannot be followed by path rules, so the label attaches to
            // the workspace as a whole and this names it for "why?".
            "workspace_labels_applied": resolution.workspace_labels_applied,
            // Explicitly name the resolution path so the audit answers "why?".
            "resolution": resolution.kind(),
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
            // Label-plane events carry the §15 LLM-plane clause (constitution
            // 2026.07.30 / #910) on top of the baseline attribution rule.
            enforced_rules: vec![
                default_enforced_rules(),
                vec!["P-15.1".to_string()],
            ]
            .concat(),
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
                tool = %req.tool,
                "failed to emit egress.envelope_labeled causal event"
            );
        }
    }
}

/// Drop the root session's `egress_policy` — it dies with the session
/// (RFC §5.4). Best-effort: a failed delete is logged, never fatal.
///
/// Deliberately out of line and `#[inline(never)]`. Both call sites (session
/// close in `lifecycle.rs`, emergency stop in `execution.rs`) sit inside very
/// large `async fn`s whose generated futures live on the stack, and the
/// server-bootstrap path already runs close to the 2 MiB test-thread limit. A
/// `tracing` event expands to a non-trivial set of locals; keeping them in
/// their own frame instead of folding them into those futures costs nothing.
#[inline(never)]
pub fn clear_session_egress_policy(store: &GatewayStore, root_session_id: &str, context: &str) {
    if let Err(e) = store.delete_egress_session_policy(root_session_id) {
        tracing::warn!(
            target: "egress_labeler",
            error = %e,
            root_session_id = %root_session_id,
            context = %context,
            "failed to delete egress session policy"
        );
    }
}

/// Tools whose result envelope is labeled by static analysis of what they run,
/// rather than by a structured path argument (RFC §4.2).
fn is_exec_shaped(tool: &str) -> bool {
    matches!(
        autonoetic_types::egress::normalize_source_key(tool).as_str(),
        "sandbox_exec" | "artifact_exec"
    )
}

/// Does a rule match a given (source, path)?
///
/// Source supports `*`-suffix globs (`email.*`, `mcp.gmail.*`) and bare names,
/// matched normalized so the dotted and snake_case spellings are equivalent.
/// Path is optional; when the rule has no `path`, it matches all calls to the
/// source. Mirrors [`crate::runtime::disclosure`]'s rule semantics.
fn rule_matches(rule: &EgressRule, source: &str, path: Option<&str>) -> bool {
    if !source_pattern_matches(&rule.source, source) {
        return false;
    }
    match (&rule.path, path) {
        (None, _) => true,
        (Some(pattern), Some(actual)) => matches_simple_glob(pattern, actual),
        (Some(_), None) => false,
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

/// Extract the `command` and any inline `script`/`code` field from an
/// exec-shaped arguments JSON. Returns (command, inline script) — both
/// best-effort. Dependency sources on disk are resolved separately by
/// [`collect_exec_dependency_sources`].
fn extract_sandbox_command(arguments_json: &str) -> (String, Option<String>) {
    let parsed: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(v) => v,
        Err(_) => return (String::new(), None),
    };
    let cmd = parsed
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let script = parsed
        .get("code")
        .and_then(|v| v.as_str())
        .or_else(|| parsed.get("script").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
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

/// Emit the chokepoint causal events derived from a [`FilterReport`] (RFC §9.1).
///
/// Called by lifecycle.rs after a completion returns. Emits, per report:
/// - one `egress.envelope_withheld` per withheld entry,
/// - one `egress.request_filtered` (summary),
/// - one `egress.assertion_violation` per violation.
///
/// All payloads are content-free metadata (ids, labels, sink, counts). Best-
/// effort: a failed write is logged, not fatal. No-op when the report shows
/// nothing was withheld AND no violation fired (the common, uneventful case).
pub fn emit_chokepoint_events(
    store: &Arc<GatewayStore>,
    report: &crate::llm::egress_chokepoint::FilterReport,
    preset: &str,
    session_id: &str,
    agent_id: &str,
    turn_id: Option<&str>,
) {
    // Skip entirely when there's nothing to report — keeps the causal chain
    // free of noise for the vast majority of (clean) completions.
    if !report.withheld_any() && !report.has_violations() {
        return;
    }

    // One envelope_withheld per withheld entry.
    for entry in &report.withheld {
        let payload = serde_json::json!({
            "tool_call_id": entry.tool_call_id,
            "target_sink": report.sink,
            "label": serde_json::to_value(&entry.label).unwrap_or(serde_json::Value::Null),
            // The indication that replaced the content — content-free by
            // construction (RFC §3.3), so safe to include.
            "indication": entry.indication,
        });
        emit_egress_event(
            store,
            "egress.envelope_withheld",
            &entry.tool_call_id,
            Some(payload),
            session_id,
            agent_id,
            turn_id,
            "egress_envelope_withheld",
        );
    }

    // One request_filtered summary.
    let summary = serde_json::json!({
        "target_sink": report.sink,
        "preset": preset,
        "withheld_count": report.withheld.len(),
        "included_count": report.included,
        "violation_count": report.violations.len(),
    });
    emit_egress_event(
        store,
        "egress.request_filtered",
        preset,
        Some(summary),
        session_id,
        agent_id,
        turn_id,
        "egress_request_filtered",
    );

    // One assertion_violation per violation (RFC §5.2.3 tripwire).
    for v in &report.violations {
        let payload = serde_json::json!({
            "tool_call_id": v.tool_call_id,
            "target_sink": report.sink,
            "payload_digest": v.payload_digest,
            "found_in_message_index": v.found_in_message_index,
        });
        emit_egress_event(
            store,
            "egress.assertion_violation",
            &v.tool_call_id,
            Some(payload),
            session_id,
            agent_id,
            turn_id,
            "egress_assertion_violation",
        );
    }
}

/// Shared builder for one egress causal event. Mirrors
/// [`emit_envelope_labeled_event`]'s shape — content-free metadata, baseline
/// attribution rule plus the §15 clause for the action (constitution
/// 2026.07.30 / #910): P-15.1 for the LLM-plane events, P-15.2 for boundary
/// refusals, P-15.3 for the widening path.
fn emit_egress_event(
    store: &Arc<GatewayStore>,
    action: &str,
    target: &str,
    payload: Option<serde_json::Value>,
    session_id: &str,
    agent_id: &str,
    turn_id: Option<&str>,
    reason: &str,
) {
    let clause = match action {
        "egress.boundary_refused" => "P-15.2",
        "egress.declassified" | "egress.relabel" => "P-15.3",
        _ => "P-15.1",
    };
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("egress-{}-{}", action.split('.').last().unwrap_or(action), uuid::Uuid::new_v4()),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.map(|t| t.to_string()),
        event_seq: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "egress".to_string(),
        action: action.to_string(),
        status: "active".to_string(),
        enforced_rules: vec![
            autonoetic_types::causal_chain::RULE_ID_EVENT_ATTRIBUTION.to_string(),
            clause.to_string(),
        ],
        target: Some(target.to_string()),
        payload: payload.map(|p| p.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: Some(reason.to_string()),
    };
    if let Err(e) = store.create_causal_event(&event) {
        tracing::warn!(
            target: "egress_labeler",
            error = %e,
            action = %action,
            "failed to emit egress causal event"
        );
    }
}

// ---------------------------------------------------------------------------
// Compression-preset eligibility (RFC §5.7 rule 1)
// ---------------------------------------------------------------------------

/// Whether a compression preset may summarize a given band of history.
///
/// Compressing `local_only` history on a remote preset is a leak *even with
/// per-envelope filtering* — the whole point of the compression call is to
/// transmit that content (RFC §5.7). So the eligibility gate is a separate
/// check from the chokepoint: it runs *before* the compression LLM is called,
/// and on refusal the governor falls back to token-budget truncation for that
/// band (an incomplete local context beats a remote leak).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressionEligibility {
    /// The preset may compress the band.
    Eligible,
    /// The preset must NOT compress the band — it would leak labeled content.
    /// `leaked_tool_call_ids` are the tool results whose labels block the call.
    Ineligible {
        reason: String,
        leaked_tool_call_ids: Vec<String>,
    },
}

impl CompressionEligibility {
    pub fn is_eligible(&self) -> bool {
        matches!(self, CompressionEligibility::Eligible)
    }
}

/// Decide whether a compression preset may summarize a band of history.
///
/// - Derives the band's taint by intersecting the labels of every labeled
///   tool result in the band (joined via `tool_call_id`).
/// - An `unrestricted` band (no labeled tool results, or all unrestricted) is
///   always eligible.
/// - A tainted band is eligible only if the preset's sink covers the taint —
///   i.e. `taint.allows(preset_sink)`. A `local_only` band against a remote
///   preset is ineligible; against a local preset it's eligible (the local
///   model is a cleared sink). A `local_only` band against *any* preset where
///   the taint excludes that sink is ineligible.
///
/// `band` is the slice of messages about to be compressed. `labels` is the
/// session's `tool_call_id → EgressLabel` map. `preset_class` is the resolved
/// compression preset's egress classification.
pub fn compression_preset_eligible(
    band: &[crate::llm::Message],
    labels: &std::collections::HashMap<String, EgressLabel>,
    preset_class: EgressClass,
) -> CompressionEligibility {
    if labels.is_empty() {
        // Unconfigured deployment — nothing is labeled, so nothing can leak.
        return CompressionEligibility::Eligible;
    }
    let preset_sink = preset_class.as_sink();
    // Intersect the labels of every labeled message in the band — tool results
    // (joined by `tool_call_id`) AND non-tool messages such as an
    // LLM-response-labeled local summary (joined by `msg_<id>`, RFC §4.5). A
    // labeled summary must gate the compression call exactly like a labeled
    // tool result, or a mixed band would be summarized on an ineligible preset.
    let mut band_taint = EgressLabel::unrestricted();
    let mut leaked: Vec<String> = Vec::new();
    for msg in band {
        let Some(key) = message_egress_key(msg) else {
            continue;
        };
        let Some(label) = labels.get(key) else {
            // No label entry → unrestricted default → doesn't taint the band.
            continue;
        };
        if !label.allows(preset_sink) {
            // This message's label excludes the preset's sink.
            leaked.push(key.to_string());
        }
        band_taint = band_taint.restrict(label);
    }
    if band_taint.allows(preset_sink) {
        CompressionEligibility::Eligible
    } else {
        let reason = format!(
            "compression preset (egress_class={:?}, sink={}) is not cleared for the band's taint \
             ({} labeled result(s) would leak); falling back to token-budget truncation \
             (RFC §5.7 — an incomplete local context beats a remote leak)",
            preset_class,
            sink_str(preset_sink),
            leaked.len(),
        );
        CompressionEligibility::Ineligible {
            reason,
            leaked_tool_call_ids: leaked,
        }
    }
}

fn sink_str(s: Sink) -> &'static str {
    match s {
        Sink::LocalModel => "local_model",
        Sink::RemoteModel => "remote_model",
        Sink::LocalAgent => "local_agent",
        Sink::FederatedAgent => "federated_agent",
        Sink::Network => "network",
        Sink::MemoryPersist => "memory_persist",
        Sink::UserReply => "user_reply",
    }
}

// ---------------------------------------------------------------------------
// Per-label-band compression (RFC §5.7 rule 2)
// ---------------------------------------------------------------------------

/// One label band of compressible history (RFC §5.7 rule 2).
///
/// Clean and tainted messages compress in **separate** bands so a mixed
/// session never collapses into a single over-tainted summary. `label` is the
/// band's exact label (also the intersection of its members when partitioned
/// by equality); `source_ids` are the egress keys whose provenance the
/// synthesized summary records.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelBand {
    pub label: EgressLabel,
    pub messages: Vec<crate::llm::Message>,
    /// Egress sidecar keys (`tool_call_id` / `msg_<id>`) of labeled members.
    pub source_ids: Vec<String>,
}

/// Partition compressible messages into per-label bands (RFC §5.7 rule 2).
///
/// Each non-system message joins the band matching its sidecar label
/// ([`message_egress_key`]); unlabeled / keyless messages join the
/// `unrestricted` band. System messages are skipped — callers re-attach them
/// outside the band loop. Bands are ordered unrestricted-first, then by
/// [`label_display_name`] for stable summary placement.
pub fn partition_by_label(
    band: &[crate::llm::Message],
    labels: &std::collections::HashMap<String, EgressLabel>,
) -> Vec<LabelBand> {
    let mut bands: Vec<LabelBand> = Vec::new();
    for msg in band {
        if msg.role == crate::llm::Role::System {
            continue;
        }
        let (source_id, label) = match message_egress_key(msg) {
            Some(key) => {
                let label = labels
                    .get(key)
                    .cloned()
                    .unwrap_or_else(EgressLabel::unrestricted);
                (Some(key.to_string()), label)
            }
            None => (None, EgressLabel::unrestricted()),
        };
        if let Some(existing) = bands.iter_mut().find(|b| b.label == label) {
            existing.messages.push(msg.clone());
            if let Some(id) = source_id {
                existing.source_ids.push(id);
            }
        } else {
            bands.push(LabelBand {
                label,
                messages: vec![msg.clone()],
                source_ids: source_id.into_iter().collect(),
            });
        }
    }
    bands.sort_by(|a, b| {
        match (a.label.is_unrestricted(), b.label.is_unrestricted()) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => label_display_name(&a.label).cmp(&label_display_name(&b.label)),
        }
    });
    bands
}

/// Build the egress label + provenance for a synthesized compression /
/// truncation block (RFC §5.7 rule 2 + §9.1). Label is the band's
/// intersection (already computed by [`partition_by_label`]); provenance
/// records the source envelope ids so the summary's lineage is queryable.
pub fn synthesized_band_label(band: &LabelBand) -> (EgressLabel, Provenance) {
    let provenance = Provenance {
        tool: Some("context.compress".to_string()),
        args_digest: None,
        matched_rules: vec![],
        parent_envelope_ids: band.source_ids.clone(),
    };
    (band.label.clone(), provenance)
}

/// Emit `egress.boundary_refused` (RFC §9.1) when a compression band is
/// ineligible for the resolved preset — the durable counterpart of the
/// tracing log so "why wasn't this band summarized remotely?" is answerable
/// from the causal chain. Content-free: band label, preset class, source ids.
pub fn emit_boundary_refused(
    store: &Arc<GatewayStore>,
    session_id: &str,
    agent_id: &str,
    turn_id: Option<&str>,
    band_label: &EgressLabel,
    preset_class: EgressClass,
    source_ids: &[String],
    reason: &str,
) {
    let payload = serde_json::json!({
        "surface": "compression",
        "band_label": serde_json::to_value(band_label).unwrap_or(serde_json::Value::Null),
        "band_label_name": label_display_name(band_label),
        "preset_class": format!("{preset_class:?}").to_ascii_lowercase(),
        "source_ids": source_ids,
        "reason": reason,
        "fallback": "token_budget_truncation",
    });
    emit_egress_event(
        store,
        "egress.boundary_refused",
        &label_display_name(band_label),
        Some(payload),
        session_id,
        agent_id,
        turn_id,
        "egress_boundary_refused",
    );
}

/// Emit `egress.boundary_refused` for a non-LLM egress surface (RFC §7 / §9.1).
pub fn emit_surface_boundary_refused(
    store: &Arc<GatewayStore>,
    session_id: &str,
    agent_id: &str,
    turn_id: Option<&str>,
    surface: &str,
    label: &EgressLabel,
    envelope_ids: &[String],
    reason: &str,
) {
    let payload = serde_json::json!({
        "surface": surface,
        "label": serde_json::to_value(label).unwrap_or(serde_json::Value::Null),
        "label_name": label_display_name(label),
        "envelope_ids": envelope_ids,
        "reason": reason,
    });
    emit_egress_event(
        store,
        "egress.boundary_refused",
        &label_display_name(label),
        Some(payload),
        session_id,
        agent_id,
        turn_id,
        "egress_boundary_refused",
    );
}

/// Resolve the session's accumulated egress taint for boundary gates (RFC §7).
///
/// Prefers the in-memory taint on [`NativeToolRunContext`]; falls back to the
/// durable `session_egress_taint` row. Errors if the store read fails.
///
/// Prefer [`require_boundary_session_taint`] at network/federation surfaces —
/// this helper's `Ok(None)` is ambiguous (no session vs no store vs no row).
pub fn resolve_session_egress_taint(
    run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    store: Option<&GatewayStore>,
    session_id: Option<&str>,
) -> anyhow::Result<Option<EgressLabel>> {
    if let Some(taint) = run_context.and_then(|c| c.egress_taint.clone()) {
        return Ok(Some(taint));
    }
    let Some(sid) = session_id.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(store) = store else {
        return Ok(None);
    };
    store.get_session_egress_taint(sid).map_err(Into::into)
}

/// Fail-closed taint for sandbox / web / hooks / MCP / OFP boundaries.
///
/// **Unknown ⇒ refuse:** missing `session_id`, missing store, or store read
/// failure are errors. A successful store miss (no taint row) is
/// [`EgressLabel::unrestricted`] — the Phase 3 default — never silent
/// fail-open when we couldn't check.
pub fn require_boundary_session_taint(
    run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    store: Option<&GatewayStore>,
    session_id: Option<&str>,
) -> anyhow::Result<EgressLabel> {
    if let Some(taint) = run_context.and_then(|c| c.egress_taint.clone()) {
        return Ok(taint);
    }
    let Some(sid) = session_id.filter(|s| !s.is_empty()) else {
        anyhow::bail!("egress_boundary_unknown_taint: missing session_id");
    };
    let Some(store) = store else {
        anyhow::bail!("egress_boundary_unknown_taint: GatewayStore required to confirm session taint");
    };
    match store.get_session_egress_taint(sid)? {
        Some(taint) => Ok(taint),
        None => Ok(EgressLabel::unrestricted()),
    }
}

/// Session-scoped declassification target for widening to [`Sink::Network`].
///
/// Used when operator approval enables network egress for a tainted session
/// (sandbox `share_net`, web tools, hooks). Pattern is bound (`session:<root>`),
/// never a bare `*`.
pub fn session_network_declass_target(
    root_session_id: &str,
) -> autonoetic_types::egress::EgressDeclassificationTarget {
    session_egress_declass_target(root_session_id)
}

/// Session-wide declassification target (`session:<root>`), sink-agnostic.
///
/// Host-scoped network grants use
/// [`session_host_network_declass_target`]; this is the session-wide shape an
/// explicit `EgressDeclassify` approval materializes (RFC §8) — for
/// `Sink::Network`, and for `Sink::RemoteModel` (the routing-plane widening).
pub fn session_egress_declass_target(
    root_session_id: &str,
) -> autonoetic_types::egress::EgressDeclassificationTarget {
    autonoetic_types::egress::EgressDeclassificationTarget::SourcePattern(format!(
        "session:{root_session_id}"
    ))
}

/// Whether an active (non-revoked, non-expired) session-wide declassification
/// grant allows `sink` for this session. Use-time lookup, never cached.
pub fn session_sink_declassified(
    store: &GatewayStore,
    session_id: &str,
    root_session_id: &str,
    sink: Sink,
) -> bool {
    store
        .egress_declassification_allows(
            &session_egress_declass_target(root_session_id),
            sink,
            session_id,
            root_session_id,
        )
        .unwrap_or(false)
}

/// File (or reuse) a pending `EgressDeclassify` approval offering the operator
/// a one-approve path out of an `egress_no_eligible_provider` refusal
/// (RFC §5.3 / §8): the session has no preset cleared for the tainted batch,
/// so the offer is a session-wide declassification to `RemoteModel`.
///
/// Dedup: an identical pending request (same target × sink under the root) is
/// reused, so a session retrying a refused turn doesn't flood the pending
/// list. Returns the request id (new or existing); `None` when filing fails
/// (e.g. the approval flood cap rejects it — the refusal text then just omits
/// the offer).
pub fn file_declassify_offer(
    store: &GatewayStore,
    config: &autonoetic_types::config::GatewayConfig,
    session_id: &str,
    root_session_id: &str,
    agent_id: &str,
    batch: &EgressLabel,
) -> Option<String> {
    use autonoetic_types::background::{ApprovalRequest, ScheduledAction};

    let target = session_egress_declass_target(root_session_id);
    if let Ok(pending) = store.get_pending_approvals_for_root(root_session_id) {
        for req in &pending {
            if let ScheduledAction::EgressDeclassify {
                target: t,
                allowed_sink,
                ..
            } = &req.action
            {
                if *t == target && *allowed_sink == Sink::RemoteModel {
                    return Some(req.request_id.clone());
                }
            }
        }
    }

    let reason = format!(
        "turn refused (egress_no_eligible_provider): this turn's new data is labeled {} and no \
         configured preset is cleared for it. Approving declassifies this root session to \
         RemoteModel (session-wide); reject to keep it local-only.",
        label_display_name(batch)
    );
    let action = ScheduledAction::EgressDeclassify {
        target,
        allowed_sink: Sink::RemoteModel,
        reason: reason.clone(),
        payload: None,
    };
    let request_id = autonoetic_types::id_format::short_random_id("apr-");
    let mut request = ApprovalRequest {
        request_id: request_id.clone(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        root_session_id: Some(root_session_id.to_string()),
        workflow_id: None,
        task_id: None,
        approval_level: crate::scheduler::approval::resolve_approval_level(config, &action),
        action,
        created_at: chrono::Utc::now().to_rfc3339(),
        status: None,
        decided_at: None,
        decided_by: None,
        reason: Some(reason),
        evidence_ref: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    store.create_approval(&mut request).ok()?;
    Some(request_id)
}

// ── RFC §5.3 inline ask (pin × taint conflict, #968) ─────────────────────

/// Option ids of the three-way inline ask filed when a pinned preset
/// conflicts with a tainted batch (RFC §5.3 / #968).
pub mod egress_ask_options {
    pub const DECLASSIFY: &str = "declassify";
    pub const RUN_LOCAL: &str = "run_local";
    pub const ABORT: &str = "abort";
}

/// The routing state of a filed pin×taint conflict ask (RFC §5.3, #968),
/// carried in the checkpoint so the resumed turn honors the operator's choice
/// without re-deriving the (already consumed) batch taint.
///
/// `local_preset` is the deterministic reroute candidate the "run local once"
/// option names; `None` when no buildable preset is cleared for the batch (the
/// ask then offers only declassify / abort).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressAskState {
    pub interaction_id: String,
    pub turn_id: String,
    pub batch: EgressLabel,
    pub pinned_preset: String,
    pub local_preset: Option<PresetCandidate>,
}

/// Whether an interaction is a gateway-filed egress pin×taint ask (tagged in
/// its `context` payload).
pub fn is_egress_pin_ask(interaction: &UserInteraction) -> bool {
    interaction
        .context
        .as_deref()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        .and_then(|v| v.get("egress_pin_ask").cloned())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Why a pinned primary is not cleared for this turn. This decides which
/// options can *actually* resolve the conflict, which is not cosmetic: an
/// option the gateway cannot honor sends the operator round the same ask again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinAskBlocker {
    /// The batch's taint excludes the pinned preset's sink. Declassifying the
    /// root session to `RemoteModel` lifts it, so that option is real.
    BatchTaint,
    /// A session `provider_constraint` (RFC §5.4 rung 1) makes the pinned
    /// preset ineligible. Declassification **cannot** lift this — the
    /// constraint outranks grants by design ("an operator who pinned the room
    /// local wins over an earlier declassification until they clear it") — so
    /// offering it would file the ask, materialize a grant that changes
    /// nothing, and re-file the ask on resume, indefinitely. The way out is to
    /// clear the room pin, which is a separate operator action (`/private`).
    ProviderConstraint,
}

/// The option ids actually offered by an egress pin×taint ask, for the
/// `egress.provider_selected` inline-ask payload.
///
/// `declassify` appears only when it can help (see [`PinAskBlocker`]);
/// `run_local` only when a buildable local preset is cleared for the batch.
/// `abort` is always present, so the ask is never a dead end.
pub fn egress_ask_options_payload(
    blocker: PinAskBlocker,
    local_preset: Option<&PresetCandidate>,
) -> Vec<&'static str> {
    let mut ids = Vec::with_capacity(3);
    if blocker == PinAskBlocker::BatchTaint {
        ids.push(egress_ask_options::DECLASSIFY);
    }
    if local_preset.is_some() {
        ids.push(egress_ask_options::RUN_LOCAL);
    }
    ids.push(egress_ask_options::ABORT);
    ids
}

/// File (or reuse) the three-way inline ask for a pin × taint conflict
/// (RFC §5.3 / #968): a pinned remote preset cannot take a tainted batch, and
/// the gateway must neither silently downgrade (a discretion leak) nor
/// hard-refuse without a path forward — the operator picks declassify / run
/// this turn on local preset X / abort.
///
/// The choice is carried by a `Decision` `UserInteraction`; the suspended turn
/// resumes on the answer and honors it at routing time (see
/// [`EgressAskState`]). Dedup: a pending egress ask for the root session is
/// reused so retried conflicts don't flood the pending list (same shape as
/// [`file_declassify_offer`]).
#[allow(clippy::too_many_arguments)]
pub fn file_egress_pin_ask(
    store: &GatewayStore,
    session_id: &str,
    root_session_id: &str,
    agent_id: &str,
    turn_id: &str,
    batch: &EgressLabel,
    pinned_preset: &str,
    local_preset: Option<&PresetCandidate>,
    blocker: PinAskBlocker,
) -> Option<String> {
    if let Ok(pending) = store.get_pending_interactions_for_root_session(root_session_id) {
        for it in &pending {
            if is_egress_pin_ask(it) {
                return Some(it.interaction_id.clone());
            }
        }
    }

    // Options are built from the same predicate as
    // `egress_ask_options_payload`, so what the operator is offered and what the
    // `egress.provider_selected` audit says was offered cannot drift.
    let offered = egress_ask_options_payload(blocker, local_preset);
    let options: Vec<UserInteractionOption> = offered
        .iter()
        .map(|id| {
            let label = match *id {
                egress_ask_options::DECLASSIFY => {
                    "Declassify this root session to the remote model".to_string()
                }
                egress_ask_options::RUN_LOCAL => format!(
                    "Run this turn on the local preset '{}'",
                    local_preset.map(|c| c.name.as_str()).unwrap_or("local"),
                ),
                _ => "Abort this turn".to_string(),
            };
            UserInteractionOption {
                id: (*id).to_string(),
                label,
                value: (*id).to_string(),
            }
        })
        .collect();

    let context = serde_json::json!({
        "egress_pin_ask": true,
        "batch": batch,
        "pinned_preset": pinned_preset,
        "local_preset": local_preset.map(|c| c.name.clone()),
        // Recorded so the resume side and the audit can tell *why* the pin was
        // blocked, not merely that it was.
        "blocker": match blocker {
            PinAskBlocker::BatchTaint => "batch_taint",
            PinAskBlocker::ProviderConstraint => "provider_constraint",
        },
    })
    .to_string();

    // The question names only the choices actually offered — promising a local
    // run when no local preset is configured, or a declassification that cannot
    // lift a room pin, reads as a broken UI and wastes the operator's decision.
    let run_local_clause = local_preset
        .map(|c| format!(" run this turn on the local preset '{}',", c.name))
        .unwrap_or_default();
    let question = match blocker {
        PinAskBlocker::BatchTaint => format!(
            "The pinned preset '{}' is not cleared for this turn's new data (labeled {}). \
             The gateway will not silently run it on another model. Choose: declassify this \
             root session to the remote model,{} or abort this turn.",
            pinned_preset,
            label_display_name(batch),
            run_local_clause,
        ),
        PinAskBlocker::ProviderConstraint => format!(
            "This room is pinned local (session provider_constraint), so the pinned preset \
             '{}' cannot run this turn. The gateway will not silently run it on another \
             model. Choose:{} or abort this turn. Declassification is not offered — the room \
             pin outranks it; lift the pin itself (`/private`) to use '{}' again.",
            pinned_preset,
            if run_local_clause.is_empty() {
                String::new()
            } else {
                run_local_clause.trim_end_matches(',').to_string()
            },
            pinned_preset,
        ),
    };

    let interaction = UserInteraction {
        interaction_id: short_random_id("ui-"),
        session_id: session_id.to_string(),
        root_session_id: root_session_id.to_string(),
        workflow_id: None,
        task_id: None,
        agent_id: agent_id.to_string(),
        turn_id: turn_id.to_string(),
        kind: UserInteractionKind::Decision,
        question,
        context: Some(context),
        options,
        allow_freeform: false,
        status: UserInteractionStatus::Pending,
        answer_option_id: None,
        answer_text: None,
        answered_by: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        answered_at: None,
        expires_at: None,
        checkpoint_turn_id: Some(turn_id.to_string()),
    };
    store.create_user_interaction(&interaction).ok()?;
    Some(interaction.interaction_id)
}

/// Materialize the "declassify" choice of an answered egress pin×taint ask:
/// the operator's explicit answer is the authorization (recorded in
/// `answered_by`), so the session-wide `RemoteModel` declassification grant is
/// inserted directly — the same grant an approved `EgressDeclassify` approval
/// would produce (RFC §8), with the interaction as provenance. A no-op for
/// any other interaction or option.
///
/// Single-shot because `GatewayStore::answer_user_interaction` **rejects** an
/// interaction that is no longer `Pending` (it errors rather than returning
/// early), so a replayed answer never reaches this function a second time. The
/// guard is upstream and is a refusal, not a silent no-op — worth stating
/// precisely, since "returns early" would imply a second call lands here
/// harmlessly.
pub fn apply_egress_ask_declassification(
    store: &GatewayStore,
    interaction: &UserInteraction,
) -> anyhow::Result<()> {
    if !is_egress_pin_ask(interaction)
        || interaction.answer_option_id.as_deref() != Some(egress_ask_options::DECLASSIFY)
    {
        return Ok(());
    }
    let root: String = if interaction.root_session_id.is_empty() {
        crate::runtime::content_store::root_session_id(&interaction.session_id).to_string()
    } else {
        interaction.root_session_id.clone()
    };
    let target = session_egress_declass_target(&root);
    let reason = format!(
        "operator answered egress pin×taint ask {}: declassify to remote model",
        interaction.interaction_id
    );
    let answered_at = interaction
        .answered_at
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    store.insert_egress_declassification_grant(
        &root,
        &interaction.session_id,
        &interaction.agent_id,
        &target,
        Sink::RemoteModel,
        &GrantScope::RootSession,
        interaction.answered_by.as_deref().unwrap_or("gateway"),
        &answered_at,
        Some(&format!("interaction:{}", interaction.interaction_id)),
        None,
    )?;
    emit_declassified(
        store,
        &interaction.session_id,
        &interaction.agent_id,
        &target,
        Sink::RemoteModel,
        GrantScope::RootSession,
        None,
        &reason,
        None,
    );
    Ok(())
}

/// Whether an active (non-revoked, non-expired) declassification grant allows
/// `Sink::Network` for this session. Lookup is always at use time — no cache.
pub fn session_network_declassified(
    store: &GatewayStore,
    session_id: &str,
    root_session_id: &str,
) -> bool {
    let target = session_network_declass_target(root_session_id);
    store
        .egress_declassification_allows(
            &target,
            Sink::Network,
            session_id,
            root_session_id,
        )
        .unwrap_or(false)
}

/// Host-scoped declassification target: widens [`Sink::Network`] for one host
/// only (`session:<root>:host:<host>`).
///
/// This is what an ordinary network approval (web_fetch / web_call /
/// web_search / sandbox_exec) materializes under taint — the operator approved
/// egress to *these hosts*, so the grant widens exactly that, never the whole
/// session. Session-wide declassification (`session:<root>`) stays available
/// through the explicit `EgressDeclassify` action only.
pub fn session_host_network_declass_target(
    root_session_id: &str,
    host: &str,
) -> autonoetic_types::egress::EgressDeclassificationTarget {
    let normalized = host.trim().trim_end_matches('.').to_ascii_lowercase();
    autonoetic_types::egress::EgressDeclassificationTarget::SourcePattern(format!(
        "session:{root_session_id}:host:{normalized}"
    ))
}

/// Host-aware variant of [`session_network_declassified`]: true when a
/// session-wide grant exists, or when `hosts` is non-empty and **every** host
/// holds an active host-scoped grant. An empty host list never satisfies the
/// check (fail-closed — mirrors the `NetworkCoverage::Unresolved` hard-refuse).
pub fn session_network_declassified_for_hosts(
    store: &GatewayStore,
    session_id: &str,
    root_session_id: &str,
    hosts: &[String],
) -> bool {
    if session_network_declassified(store, session_id, root_session_id) {
        return true;
    }
    if hosts.is_empty() {
        return false;
    }
    hosts.iter().all(|host| {
        let target = session_host_network_declass_target(root_session_id, host);
        store
            .egress_declassification_allows(&target, Sink::Network, session_id, root_session_id)
            .unwrap_or(false)
    })
}

/// Fail-closed network egress gate for web tools, hooks, and similar surfaces.
///
/// Returns a JSON tool-error body when the session taint excludes
/// [`Sink::Network`] and no active declassification grant widens it; `None` when
/// egress is allowed. Emits `egress.boundary_refused` on refuse when a
/// [`GatewayStore`] is available.
pub fn network_egress_boundary_refusal_json(
    surface: &str,
    tool_name: &str,
    run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    gateway_store: Option<&Arc<GatewayStore>>,
    session_id: Option<&str>,
    agent_id: &str,
    turn_id: Option<&str>,
    host: Option<&str>,
) -> Option<String> {
    let tool_name =
        crate::runtime::tool_call_processor::canonical_tool_name(tool_name);
    let Some(store) = gateway_store else {
        let payload = serde_json::json!({
            "ok": false,
            "error_type": "egress_boundary_refused",
            "surface": surface,
            "tool": tool_name,
            "message": format!(
                "{tool_name} refused: GatewayStore required to confirm session egress taint"
            ),
            "repair_hint": "Ensure the tool runs with GatewayStore available.",
        });
        return Some(payload.to_string());
    };

    let session_taint = match require_boundary_session_taint(
        run_context,
        Some(store.as_ref()),
        session_id,
    ) {
        Ok(t) => t,
        Err(e) => {
            emit_surface_boundary_refused(
                store,
                session_id.unwrap_or(""),
                agent_id,
                turn_id,
                surface,
                &EgressLabel::empty(),
                &[],
                &format!("session egress taint unresolved: {e}"),
            );
            let payload = serde_json::json!({
                "ok": false,
                "error_type": "egress_boundary_refused",
                "surface": surface,
                "tool": tool_name,
                "message": format!("{tool_name} refused: cannot establish session egress taint ({e})"),
                "repair_hint": "Ensure the tool runs with a session id and GatewayStore so taint can be confirmed.",
            });
            return Some(payload.to_string());
        }
    };

    if session_taint.allows(Sink::Network) {
        return None;
    }

    let Some(sid) = session_id.filter(|s| !s.is_empty()) else {
        emit_surface_boundary_refused(
            store,
            "",
            agent_id,
            turn_id,
            surface,
            &session_taint,
            &[],
            "session egress taint excludes Network but session_id is missing",
        );
        let payload = serde_json::json!({
            "ok": false,
            "error_type": "egress_boundary_refused",
            "surface": surface,
            "tool": tool_name,
            "message": format!("{tool_name} refused: session egress taint excludes Network"),
            "repair_hint": "Operator-declassify Sink::Network for this session before network egress.",
        });
        return Some(payload.to_string());
    };

    let root = run_context
        .map(|c| c.root_session_id.as_str())
        .or_else(|| sid.split('/').next())
        .unwrap_or(sid);
    let host_buf;
    let hosts: &[String] = match host {
        Some(h) => {
            host_buf = vec![h.to_string()];
            &host_buf
        }
        None => &[],
    };
    if session_network_declassified_for_hosts(store.as_ref(), sid, root, hosts) {
        return None;
    }

    emit_surface_boundary_refused(
        store,
        sid,
        agent_id,
        turn_id,
        surface,
        &session_taint,
        &[],
        &format!("session egress taint excludes Network ({tool_name}, RFC §7)"),
    );
    let repair_hint = match host {
        Some(h) => format!(
            "Approve this {tool_name} request — approval declassifies Network egress to {h} only (host-scoped grant). For session-wide Network, use an explicit EgressDeclassify approval."
        ),
        None => format!(
            "Approve egress declassification for Sink::Network on this session (explicit EgressDeclassify), or clear session taint."
        ),
    };
    let payload = serde_json::json!({
        "ok": false,
        "error_type": "egress_boundary_refused",
        "surface": surface,
        "tool": tool_name,
        "message": format!(
            "{tool_name} refused: session egress taint excludes Network; operator declassification required"
        ),
        "repair_hint": repair_hint,
    });
    Some(payload.to_string())
}

/// Artifact ids referenced in a tool call's arguments (RFC §4.5 artifact birth
/// point, #980).
///
/// Ids are `art_` + exactly 8 lowercase hex chars (`compute_deterministic_artifact_id`),
/// which is specific enough to scan for without parsing every tool's argument
/// schema — the same "deterministic textual signal" approach argument taint uses
/// for handle references. Deduped and sorted so the audit record is stable.
///
/// Over-matching is safe in the direction that matters: a spurious id resolves to
/// no stored label and contributes nothing. Under-matching (an artifact named in
/// a shape this misses) is the risk, which is why the *write* side stamps the
/// builder's taint rather than relying on read-side detection alone.
pub fn artifact_ids_in_arguments(arguments_json: &str) -> Vec<String> {
    const PREFIX: &str = "art_";
    const ID_HEX_LEN: usize = 8;
    let bytes = arguments_json.as_bytes();
    let mut out: Vec<String> = Vec::new();
    for (start, _) in arguments_json.match_indices(PREFIX) {
        let hex_start = start + PREFIX.len();
        // Slice BYTES, never the `&str`. `arguments_json` is arbitrary
        // agent-supplied JSON, so the 8 bytes after `art_` may be the middle of a
        // multi-byte codepoint (`art_€€€`); a str slice at a non-boundary index
        // panics, which in this path would take down egress labeling itself.
        let Some(hex) = bytes.get(hex_start..hex_start + ID_HEX_LEN) else {
            continue;
        };
        if !hex.iter().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        // Reject a longer alphanumeric run: `art_deadbeefcafe` is not an id.
        if bytes
            .get(hex_start + ID_HEX_LEN)
            .is_some_and(|b| b.is_ascii_alphanumeric())
        {
            continue;
        }
        // Every byte is an ASCII hexdigit, so this cannot fail; matched rather
        // than unwrapped so there is no branch that could produce a wrong id.
        let Ok(hex) = std::str::from_utf8(hex) else {
            continue;
        };
        let id = format!("{PREFIX}{hex}");
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out.sort();
    out
}

/// Intersect the stored labels of every artifact named in `arguments_json`.
///
/// The read half of the artifact birth point: content that was labeled when it
/// was produced stays labeled after a round trip through the content-addressed
/// store. Returns the intersection and the ids that contributed.
///
/// A store read failure fails closed to `local_only` for that artifact — an
/// artifact whose label could not be read might have been restricted, and
/// treating it as clean is the one outcome §2.2 forbids.
pub fn artifact_taint_from_store(
    arguments_json: &str,
    store: &GatewayStore,
) -> (EgressLabel, Vec<String>) {
    let mut label = EgressLabel::unrestricted();
    let mut applied: Vec<String> = Vec::new();
    for id in artifact_ids_in_arguments(arguments_json) {
        match store.get_artifact_egress_label(&id) {
            Ok(Some(stored)) => {
                label = label.restrict(&stored);
                applied.push(id);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    target: "egress",
                    error = %e,
                    artifact_id = %id,
                    "artifact egress label read failed — failing closed to local_only \
                     for this result (RFC §2.2)"
                );
                label = label.restrict(&EgressLabel::local_only());
                applied.push(id);
            }
        }
    }
    (label, applied)
}

/// Read an agent workspace's stored label, failing closed to `local_only`.
///
/// The read half of RFC §11 (#1001): a workspace whose label could not be read
/// might have been restricted, and treating it as clean is the one outcome §2.2
/// forbids. Returns the label, the agent id (as provenance, mirroring the
/// artifact path) when the workspace carries a restrictive label, and whether
/// the read failed. Callers must NOT write back on a failed read — failing
/// closed is a per-result decision, and a transient read failure must not
/// ratchet the durable workspace label.
pub fn workspace_taint_from_store(
    agent_id: &str,
    store: &GatewayStore,
) -> (EgressLabel, Vec<String>, bool) {
    match store.get_workspace_egress_label(agent_id) {
        Ok(Some(stored)) => (stored, vec![agent_id.to_string()], false),
        Ok(None) => (EgressLabel::unrestricted(), Vec::new(), false),
        Err(e) => {
            tracing::warn!(
                target: "egress",
                error = %e,
                agent_id = %agent_id,
                "workspace egress label read failed — failing closed to local_only \
                 for this result, not persisting to the workspace (RFC §2.2)"
            );
            (EgressLabel::local_only(), vec![agent_id.to_string()], true)
        }
    }
}

/// Intersect argument-carried taint from prior labeled results in this turn.
pub fn argument_taint_from_prior(
    arguments_json: &str,
    prior_labels: &std::collections::HashMap<String, PriorLabeledResult>,
) -> (EgressLabel, Vec<String>) {
    let mut taint_label = EgressLabel::unrestricted();
    let mut parent_envelope_ids: Vec<String> = Vec::new();
    if prior_labels.is_empty() {
        return (taint_label, parent_envelope_ids);
    }
    for (prior_tcid, prior) in prior_labels {
        let handle_match = arguments_json.contains(prior_tcid.as_str());
        let verbatim_match = prior
            .content_snippet
            .as_deref()
            .map(|snip| !snip.is_empty() && arguments_json.contains(snip))
            .unwrap_or(false);
        if handle_match || verbatim_match {
            taint_label = taint_label.restrict(&prior.label);
            parent_envelope_ids.push(prior_tcid.clone());
        }
    }
    parent_envelope_ids.sort();
    (taint_label, parent_envelope_ids)
}

/// Persist restrictive in-memory session taint when filing a network approval
/// so `apply_decision` can materialize declassification before session finalize.
pub fn snapshot_session_egress_taint_for_approval(
    store: &GatewayStore,
    session_id: &str,
    run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
) -> anyhow::Result<()> {
    if session_id.is_empty() {
        return Ok(());
    }
    let taint = if let Some(t) = run_context.and_then(|c| c.egress_taint.clone()) {
        t
    } else if let Some(row) = store.get_session_egress_taint(session_id)? {
        row
    } else {
        return Ok(());
    };
    if !taint.is_unrestricted() {
        store.set_session_egress_taint(session_id, &taint)?;
    }
    Ok(())
}

/// Fail-closed MCP egress gate for remote servers: after the session-level
/// network gate (including declassification), argument-carried taint must allow
/// [`Sink::Network`]. Emits `egress.boundary_refused` with `surface: "mcp"` on refuse.
pub fn mcp_remote_egress_refusal_json(
    tool_name: &str,
    arguments_json: &str,
    run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    gateway_store: Option<&Arc<GatewayStore>>,
    session_id: Option<&str>,
    agent_id: &str,
    turn_id: Option<&str>,
    prior_labels: &std::collections::HashMap<String, PriorLabeledResult>,
    host: Option<&str>,
) -> Option<String> {
    let tool_name =
        crate::runtime::tool_call_processor::canonical_tool_name(tool_name);
    if let Some(refusal) = network_egress_boundary_refusal_json(
        "mcp",
        tool_name,
        run_context,
        gateway_store,
        session_id,
        agent_id,
        turn_id,
        host,
    ) {
        return Some(refusal);
    }

    let store = gateway_store?;
    let (arg_taint, parent_envelope_ids) = argument_taint_from_prior(arguments_json, prior_labels);
    if arg_taint.is_unrestricted() {
        return None;
    }
    if arg_taint.allows(Sink::Network) {
        return None;
    }

    emit_surface_boundary_refused(
        store,
        session_id.unwrap_or(""),
        agent_id,
        turn_id,
        "mcp",
        &arg_taint,
        &parent_envelope_ids,
        "MCP remote tools/call refused: argument egress labels exclude Network (RFC §7)",
    );
    let payload = serde_json::json!({
        "ok": false,
        "error_type": "egress_boundary_refused",
        "surface": "mcp",
        "tool": tool_name,
        "message": format!(
            "{tool_name} refused: argument egress labels exclude Network for remote MCP"
        ),
        "parent_envelope_ids": parent_envelope_ids,
        "repair_hint": "Remove restricted content from arguments, or use a local MCP server.",
    });
    Some(payload.to_string())
}

/// Default inbound OFP label when the peer omits or sends an unparseable
/// `egress_label` — fail-closed, never [`EgressLabel::unrestricted`].
pub fn ofp_inbound_fail_closed_label() -> EgressLabel {
    EgressLabel::no_remote_model()
}

/// Parse an inbound OFP `egress_label` wire field. Missing or unparseable ⇒
/// [`ofp_inbound_fail_closed_label`].
pub fn parse_ofp_inbound_egress_label(raw: Option<&serde_json::Value>) -> EgressLabel {
    let Some(value) = raw else {
        return ofp_inbound_fail_closed_label();
    };
    match serde_json::from_value::<EgressLabel>(value.clone()) {
        Ok(label) if !label.is_empty() => label,
        _ => ofp_inbound_fail_closed_label(),
    }
}

/// Metadata key for an operator's per-message mark (RFC §5.4 rung 3).
pub const OPERATOR_LABEL_METADATA_KEY: &str = "operator_egress_label";

/// Metadata key for a federation peer's inbound wire label (RFC §7).
pub const OFP_INBOUND_LABEL_METADATA_KEY: &str = "ofp_inbound_egress_label";

/// Metadata key carrying a delegating parent's accumulated taint to its child
/// (RFC §5.5, the downward direction). Named as a constant because the producer
/// (`agent_spawn`) and the consumer ([`resolve_ingest_turn_label`]) sit in
/// different modules and a typo would silently mean "child starts clean".
pub const PARENT_TAINT_METADATA_KEY: &str = "parent_egress_taint";

/// Metadata keys that can declare a label for an incoming user-role turn. All
/// are *declared inputs* in the I-14 sense — an operator's per-message mark, a
/// peer's wire label, a delegating parent's accumulated taint — never model
/// output.
///
/// **Reserved namespace.** Because these are read as declarations, any path that
/// forwards *model-supplied* metadata into an ingest must strip them first (see
/// [`strip_ingest_label_keys`]) — otherwise an agent could forge an operator
/// mark or a peer label. Intersection bounds the damage to over-restriction
/// rather than a leak, but a label the gateway did not stamp has no business in
/// the resolver at all.
pub const INGEST_LABEL_METADATA_KEYS: [&str; 3] = [
    OPERATOR_LABEL_METADATA_KEY,
    OFP_INBOUND_LABEL_METADATA_KEY,
    PARENT_TAINT_METADATA_KEY,
];

/// Remove every [`INGEST_LABEL_METADATA_KEYS`] entry from a metadata object,
/// returning the keys that were present.
///
/// Call this on any metadata an *agent* supplied before it is forwarded into an
/// ingest. The gateway re-stamps whatever it has actually resolved afterwards,
/// so stripping first — rather than only overwriting — is what makes the gateway
/// the sole author: an overwrite leaves a forged key in place in exactly the
/// case where the gateway has nothing to write (a clean parent).
///
/// A non-empty return is worth logging: nothing legitimate puts these keys in
/// agent-authored metadata.
pub fn strip_ingest_label_keys(metadata: &mut serde_json::Value) -> Vec<&'static str> {
    let Some(map) = metadata.as_object_mut() else {
        return Vec::new();
    };
    INGEST_LABEL_METADATA_KEYS
        .into_iter()
        .filter(|key| map.remove(*key).is_some())
        .collect()
}

/// Resolve the label for an incoming user-role turn (RFC §4.5 "User/operator
/// message"), or `None` when nothing restricts it.
///
/// The pure core of ingest labeling: the caller supplies the session policy's
/// `default_label` (already read from the store, `None` when unset) and the
/// ingest metadata; this intersects every declaration found.
///
/// - `policy_default` — the room-wide declaration for otherwise-unlabeled content.
/// - `policy_read_failed` — the caller's policy read errored. Fails closed to
///   `local_only`: an unreadable policy could have been restrictive, and shipping
///   the turn remotely on the strength of a failed lookup is the one outcome
///   §2.2 forbids.
/// - metadata keys in [`INGEST_LABEL_METADATA_KEYS`] — the per-message mark and
///   the federation label. A malformed value fails closed rather than dropping
///   the restriction it was trying to express.
///
/// Every contributor intersects, so none can widen what another tightened
/// (§2.4). `None` is returned for an unrestricted result because absence *is*
/// the unrestricted encoding throughout the plane.
pub fn resolve_ingest_turn_label(
    policy_default: Option<autonoetic_types::egress::NamedEgressLabel>,
    policy_read_failed: bool,
    metadata: Option<&serde_json::Value>,
) -> Option<EgressLabel> {
    let mut acc = EgressLabel::unrestricted();
    let mut any = false;

    if policy_read_failed {
        acc = acc.restrict(&EgressLabel::local_only());
        any = true;
    }
    if let Some(named) = policy_default {
        acc = acc.restrict(&named.to_label());
        any = true;
    }
    for key in INGEST_LABEL_METADATA_KEYS {
        let Some(value) = metadata.and_then(|m| m.get(key)) else {
            continue;
        };
        match serde_json::from_value::<EgressLabel>(value.clone()) {
            Ok(label) => acc = acc.restrict(&label),
            Err(_) => acc = acc.restrict(&EgressLabel::local_only()),
        }
        any = true;
    }

    if any && !acc.is_unrestricted() {
        Some(acc)
    } else {
        None
    }
}

/// Whether outbound federation may carry `message` for `session_taint`.
pub fn ofp_outbound_allows_federation(session_taint: &EgressLabel) -> bool {
    session_taint.allows(Sink::FederatedAgent)
}

/// Fail-closed OFP egress gate before writing `AgentMessage` to a peer.
///
/// Returns an error message when the session taint excludes
/// [`Sink::FederatedAgent`]. Emits `egress.boundary_refused` with
/// `surface: "ofp"` on refuse.
pub fn ofp_federated_egress_refusal(
    message: &str,
    sender_session_id: Option<&str>,
    sender_agent_id: &str,
    gateway_store: Option<&Arc<GatewayStore>>,
) -> Option<anyhow::Error> {
    let Some(store) = gateway_store else {
        return Some(anyhow::anyhow!(
            "OFP agent_message refused: GatewayStore required to confirm session egress taint"
        ));
    };
    let session_taint = match require_boundary_session_taint(None, Some(store.as_ref()), sender_session_id)
    {
        Ok(t) => t,
        Err(e) => {
            emit_surface_boundary_refused(
                store,
                sender_session_id.unwrap_or(""),
                sender_agent_id,
                None,
                "ofp",
                &EgressLabel::empty(),
                &[],
                &format!("session egress taint unresolved: {e}"),
            );
            return Some(anyhow::anyhow!("OFP agent_message refused: {e}"));
        }
    };
    if ofp_outbound_allows_federation(&session_taint) {
        return None;
    }
    emit_surface_boundary_refused(
        store,
        sender_session_id.unwrap_or(""),
        sender_agent_id,
        None,
        "ofp",
        &session_taint,
        &[],
        "OFP agent_message refused: session egress label excludes FederatedAgent (RFC §7)",
    );
    let _ = message; // content-free audit; body never leaves when refused
    Some(anyhow::anyhow!(
        "OFP agent_message refused: session egress label excludes FederatedAgent"
    ))
}

/// Build outbound OFP wire fields for an allowed federation send.
///
/// Returns `(message_text, egress_label, withheld_indication)`. The label is
/// omitted on the wire when unrestricted.
pub fn ofp_outbound_wire_fields(
    message: &str,
    session_taint: &EgressLabel,
) -> (String, Option<serde_json::Value>, Option<String>) {
    let egress_label = if session_taint.is_unrestricted() {
        None
    } else {
        serde_json::to_value(session_taint).ok()
    };
    (message.to_string(), egress_label, None)
}

/// Emit `egress.envelope_labeled` for a synthesized compression/truncation
/// block so the summary's band membership + parent lineage is queryable
/// (RFC §5.7 rule 2 + §9.1).
pub fn emit_synthesized_envelope_labeled(
    store: &Arc<GatewayStore>,
    session_id: &str,
    agent_id: &str,
    turn_id: Option<&str>,
    envelope_id: &str,
    label: &EgressLabel,
    provenance: &Provenance,
) {
    let payload = serde_json::json!({
        "envelope_id": envelope_id,
        "label": serde_json::to_value(label).unwrap_or(serde_json::Value::Null),
        "label_name": label_display_name(label),
        "provenance": serde_json::to_value(provenance).unwrap_or(serde_json::Value::Null),
        "synthesized": true,
        "kind": "compressed_context",
    });
    emit_egress_event(
        store,
        "egress.envelope_labeled",
        envelope_id,
        Some(payload),
        session_id,
        agent_id,
        turn_id,
        "egress_envelope_labeled",
    );
}

// ---------------------------------------------------------------------------
// Taint-following routing (RFC §5.3)
// ---------------------------------------------------------------------------
//
// Once a tool batch is labeled, the *next* completion must run on a provider
// cleared for that batch's taint — deterministically, without operator
// per-turn preset flipping and without an LLM deciding (a discretion leak).
// The rule: intersect the labels of the envelopes added since the last
// completion (the "batch"); a preset is eligible iff its `egress_class` sink is
// in that intersection. Routing (and the failover chain) then pick among
// eligible candidates only. The helpers here are the pure core; the lifecycle
// wires them into the completion path.

/// The egress-sidecar join key for a message (RFC §3.4): tool results join by
/// their `tool_call_id`; every other message (assistant / user / synthesized)
/// joins by its stable `msg_<id>`. `None` for a message carrying neither
/// (transient or predating message ids) — such a message is treated as
/// unlabeled. This is the single definition the chokepoint, compression
/// eligibility, and per-band compression all key off, so they never disagree
/// about which envelope a message belongs to.
pub fn message_egress_key(msg: &crate::llm::Message) -> Option<&str> {
    if msg.role == crate::llm::Role::Tool {
        msg.tool_call_id.as_deref()
    } else {
        msg.id.as_deref()
    }
}

/// A session's **accumulated taint** (RFC §5.5): the intersection of the labels
/// of everything the session touched — i.e. every value in its egress-label
/// sidecar. This is the label a child session's return value (or a session's
/// outbound `agent_message`) carries when it crosses to another session, so a
/// tainted child can't hand content to a remote-pinned sibling (closes the
/// `LocalAgent` hole).
///
/// Empty map → [`EgressLabel::unrestricted`] (the session touched nothing
/// restrictive). Because it is an intersection it is the *most restrictive*
/// bound — conservative by design (never under-labels a cross-session
/// transfer), at the cost of possibly over-restricting when a session mixed
/// clean and tainted work.
pub fn session_accumulated_taint(
    labels: &std::collections::HashMap<String, EgressLabel>,
) -> EgressLabel {
    let mut acc = EgressLabel::unrestricted();
    for label in labels.values() {
        acc = acc.restrict(label);
    }
    acc
}

/// The [`Sink`] a preset's completions land in (RFC §5.1): its `egress_class`
/// mapped to a sink, defaulting to [`Sink::RemoteModel`] when unclassified
/// (fail-closed).
pub fn preset_sink(egress_class: Option<EgressClass>) -> Sink {
    egress_class.unwrap_or(EgressClass::Remote).as_sink()
}

/// Whether a preset may receive a completion carrying `batch` taint (RFC §5.3):
/// the preset's sink must be permitted by the batch's allowed-sink set. An
/// `unrestricted` batch admits every preset; a `local_only` batch admits only
/// `local`-classified presets.
pub fn preset_batch_eligible(batch: &EgressLabel, egress_class: Option<EgressClass>) -> bool {
    batch.allows(preset_sink(egress_class))
}

/// One preset considered for taint-following routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetCandidate {
    pub name: String,
    pub egress_class: Option<EgressClass>,
}

/// The routing decision for one completion's batch taint (RFC §5.3 + §9.1).
///
/// `eligible` is the audit set (every configured preset cleared for the batch);
/// `reroute_to` names the preset the primary should switch to when the primary
/// itself is ineligible (prefer `local`, then stable by name), or `None` when
/// the primary is already eligible or nothing is eligible. `primary_eligible`
/// and `batch` complete the `egress.provider_selected` payload.
///
/// `batch` is the **effective** batch: when the session policy declares a
/// `provider_constraint` (RFC §5.4 rung 1), the input batch is intersected
/// with the constraint's label (e.g. `local_only`), so a clean batch in a
/// constrained session is still routed — and audited — as restricted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRoutingPlan {
    pub batch: EgressLabel,
    pub provider_constraint: Option<autonoetic_types::egress::ProviderConstraint>,
    pub primary_eligible: bool,
    pub eligible: Vec<String>,
    pub reroute_to: Option<PresetCandidate>,
}

impl EgressRoutingPlan {
    /// No eligible preset exists for a tainted batch — the turn must refuse
    /// with `egress_no_eligible_provider` (RFC §5.3) rather than ship taint to
    /// an ineligible provider.
    pub fn no_eligible_provider(&self) -> bool {
        !self.primary_eligible && self.reroute_to.is_none()
    }
}

/// Plan taint-following routing for one completion (RFC §5.3), a pure function
/// of (batch, primary class, configured presets, session provider constraint).
///
/// - An unrestricted batch with no constraint is a fast no-op: primary
///   eligible, no reroute.
/// - A `provider_constraint` (RFC §5.4 rung 1) intersects the batch with the
///   constraint's label first: `local_only` makes every batch — clean ones
///   included — ineligible for non-local presets. Provider *selection* itself
///   is constrained, not just content.
/// - Otherwise the primary is eligible iff its own sink is cleared; when it is
///   not, an eligible preset is chosen to reroute to — **preferring `local`
///   presets**, then stable-sorted by name for determinism.
/// - `eligible` lists every cleared preset for the audit event, regardless of
///   whether a reroute happened.
///
/// Selection never widens: it only ever picks a preset whose sink the batch
/// already permits, so a rerouted completion cannot leak (RFC §2.2 fail-closed).
pub fn plan_taint_following_route(
    batch: &EgressLabel,
    primary_class: Option<EgressClass>,
    presets: &[PresetCandidate],
    provider_constraint: Option<autonoetic_types::egress::ProviderConstraint>,
) -> EgressRoutingPlan {
    // RFC §5.4 rung 1: a provider constraint narrows the effective batch so
    // non-local presets are ineligible even for clean turns.
    let constrained_batch;
    let batch = match provider_constraint {
        Some(autonoetic_types::egress::ProviderConstraint::LocalOnly) => {
            constrained_batch = batch.clone().restrict(&EgressLabel::local_only());
            &constrained_batch
        }
        None => batch,
    };
    // Fast path: an unrestricted batch admits everything — no filtering needed.
    if batch.is_unrestricted() {
        return EgressRoutingPlan {
            batch: batch.clone(),
            provider_constraint,
            primary_eligible: true,
            eligible: Vec::new(),
            reroute_to: None,
        };
    }

    let primary_eligible = preset_batch_eligible(batch, primary_class);

    // Every configured preset cleared for the batch, for the audit set.
    let mut eligible_candidates: Vec<&PresetCandidate> = presets
        .iter()
        .filter(|c| preset_batch_eligible(batch, c.egress_class))
        .collect();
    // Deterministic order: local presets first (the usual target for a tainted
    // batch), then by name.
    eligible_candidates.sort_by(|a, b| {
        let a_local = preset_sink(a.egress_class) == Sink::LocalModel;
        let b_local = preset_sink(b.egress_class) == Sink::LocalModel;
        b_local.cmp(&a_local).then_with(|| a.name.cmp(&b.name))
    });
    let eligible: Vec<String> = eligible_candidates.iter().map(|c| c.name.clone()).collect();

    // Reroute only when the primary itself can't take the batch. Pick the first
    // eligible candidate in the deterministic order above.
    let reroute_to = if primary_eligible {
        None
    } else {
        eligible_candidates.first().map(|c| (*c).clone())
    };

    EgressRoutingPlan {
        batch: batch.clone(),
        provider_constraint,
        primary_eligible,
        eligible,
        reroute_to,
    }
}

/// Emit `egress.relabel` for an operator (or sweep) reclassification of stored
/// content (RFC §6.7 / #908). Content-free metadata only.
#[allow(clippy::too_many_arguments)]
pub fn emit_relabel(
    store: &Arc<GatewayStore>,
    session_id: &str,
    agent_id: &str,
    kind: &str,
    count: u64,
    new_label: &EgressLabel,
    memory_scope: Option<&str>,
    trace_session: Option<&str>,
) {
    let payload = serde_json::json!({
        "kind": kind,
        "count": count,
        "new_label": serde_json::to_value(new_label).unwrap_or(serde_json::Value::Null),
        "new_label_name": label_display_name(new_label),
        "memory_scope": memory_scope,
        "trace_session": trace_session,
    });
    emit_egress_event(
        store,
        "egress.relabel",
        kind,
        Some(payload),
        session_id,
        agent_id,
        None,
        "operator or sweep reclassified stored content egress label",
    );
}

/// Emit `egress.declassified` when an operator grant widens a content target
/// to a sink (RFC §8 / #909). Content-free metadata only.
pub fn emit_declassified(
    store: &GatewayStore,
    session_id: &str,
    agent_id: &str,
    target: &autonoetic_types::egress::EgressDeclassificationTarget,
    allowed_sink: autonoetic_types::egress::Sink,
    scope: autonoetic_types::background::GrantScope,
    source_approval_id: Option<&str>,
    reason: &str,
    expires_at: Option<&str>,
) {
    let payload = serde_json::json!({
        "target": target,
        "allowed_sink": serde_json::to_value(allowed_sink).unwrap_or(serde_json::Value::Null),
        "scope": scope.as_str(),
        "source_approval_id": source_approval_id,
        "expires_at": expires_at,
        "reason": reason,
    });
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("egress-declassified-{}", uuid::Uuid::new_v4()),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "egress".to_string(),
        action: "egress.declassified".to_string(),
        status: "active".to_string(),
        enforced_rules: vec![
            autonoetic_types::causal_chain::RULE_ID_EVENT_ATTRIBUTION.to_string(),
            "P-15.3".to_string(),
        ],
        target: Some(target.value().to_string()),
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: Some("operator-approved egress declassification grant materialized".to_string()),
    };
    if let Err(e) = store.create_causal_event(&event) {
        tracing::warn!(
            target: "egress_labeler",
            error = %e,
            "failed to emit egress.declassified causal event"
        );
    }
}

/// Emit `egress.provider_selected` (RFC §9.1) — the per-completion routing
/// audit that makes "why did turn N run on this provider?" answerable from the
/// chain alone. Content-free: batch label, eligible set, chosen preset, whether
/// a reroute happened, and any fallback presets skipped as ineligible.
///
/// `chosen_preset` is `None` when the turn refused with
/// `egress_no_eligible_provider`. Only meaningful for tainted batches — the
/// lifecycle skips emission entirely for the unrestricted (clean) case.
///
/// `inline_ask` (RFC §9.1's "inline-ask outcome", #968) carries the
/// pin×taint ask state for this completion: `{status: "filed", interaction_id,
/// options: [...]}` when the completion suspended on a filed ask, or
/// `{status: "answered", outcome: <option id>, interaction_id}` when the
/// resumed completion ran (or refused) per the operator's choice. `None`
/// when no ask was involved.
#[allow(clippy::too_many_arguments)]
pub fn emit_provider_selected(
    store: &Arc<GatewayStore>,
    session_id: &str,
    agent_id: &str,
    turn_id: Option<&str>,
    plan: &EgressRoutingPlan,
    chosen_preset: Option<&str>,
    fallback_skipped: &[String],
    rerouted: bool,
    declassified_remote: bool,
    inline_ask: Option<&serde_json::Value>,
) {
    let payload = serde_json::json!({
        "batch_label": serde_json::to_value(&plan.batch).unwrap_or(serde_json::Value::Null),
        "batch_label_name": label_display_name(&plan.batch),
        "provider_constraint": plan.provider_constraint.map(|c| c.as_str()),
        "primary_eligible": plan.primary_eligible,
        "eligible_presets": plan.eligible,
        "chosen_preset": chosen_preset,
        "rerouted": rerouted,
        "fallback_skipped": fallback_skipped,
        "no_eligible_provider": chosen_preset.is_none(),
        "declassified_remote": declassified_remote,
        "inline_ask": inline_ask,
    });
    emit_egress_event(
        store,
        "egress.provider_selected",
        chosen_preset.unwrap_or("none"),
        Some(payload),
        session_id,
        agent_id,
        turn_id,
        "egress_provider_selected",
    );
}

#[cfg(test)]
mod tests {
    use autonoetic_types::egress::{
        EgressConfig, EgressLabel, EgressRule, EgressSessionPolicy, NamedEgressLabel, Sink,
    };

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

    fn keys(r: &Resolution) -> Vec<String> {
        r.rule_keys()
    }

    fn no_prior() -> std::collections::HashMap<String, PriorLabeledResult> {
        std::collections::HashMap::new()
    }

    fn prior(
        entries: &[(&str, EgressLabel, Option<&str>)],
    ) -> std::collections::HashMap<String, PriorLabeledResult> {
        entries
            .iter()
            .map(|(tcid, label, snip)| {
                (
                    tcid.to_string(),
                    PriorLabeledResult {
                        label: label.clone(),
                        content_snippet: snip.map(|s| s.to_string()),
                    },
                )
            })
            .collect()
    }

    // ── source matching ──────────────────────────────────────────────────

    #[test]
    fn bare_source_matches_exact() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("fs.read", None, NamedEgressLabel::LocalOnly)]));
        let r = l.resolve_label("fs.read", None);
        assert_eq!(r.label, EgressLabel::local_only());
        assert_eq!(keys(&r), vec!["fs.read"]);
    }

    #[test]
    fn source_glob_matches_suffix() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("email.*", None, NamedEgressLabel::LocalOnly)]));
        assert_eq!(l.resolve_label("email.read", None).label, EgressLabel::local_only());
        assert_eq!(l.resolve_label("email.send", None).label, EgressLabel::local_only());
        // Non-matching source stays unrestricted (default).
        assert!(l.resolve_label("fs.read", None).label.is_unrestricted());
    }

    #[test]
    fn mcp_server_glob_matches() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("mcp.gmail.*", None, NamedEgressLabel::LocalOnly)]));
        // MCP tools reach the boundary as `mcp_<server>_<tool>`.
        assert_eq!(
            l.resolve_label("mcp_gmail_send_message", None).label,
            EgressLabel::local_only()
        );
        assert!(l.resolve_label("mcp_outlook_send", None).label.is_unrestricted());
    }

    /// The dotted spelling in the RFC/config template must match the runtime's
    /// canonical snake_case tool name — otherwise every documented rule is a
    /// silent no-op.
    #[test]
    fn dotted_rule_source_matches_canonical_tool_name() {
        let l = EgressLabeler::from_config(&cfg(vec![rule(
            "fs.read",
            None,
            NamedEgressLabel::LocalOnly,
        )]));
        assert_eq!(l.resolve_label("fs_read", None).label, EgressLabel::local_only());
    }

    // ── path narrowing ───────────────────────────────────────────────────

    #[test]
    fn path_narrows_match() {
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("fs.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        // path supplied → matches
        assert_eq!(
            l.resolve_label("fs.read", Some("~/mail/inbox/1")).label,
            EgressLabel::local_only()
        );
        // different path → no match → unrestricted
        assert!(l.resolve_label("fs.read", Some("/etc/passwd")).label.is_unrestricted());
        // no path supplied with a path-scoped rule → no match (conservative)
        assert!(l.resolve_label("fs.read", None).label.is_unrestricted());
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
        let r = l.resolve_label("fs.read", Some("~/mail/inbox/1"));
        assert_eq!(r.label, EgressLabel::local_only());
        assert_eq!(r.matched.len(), 2);
    }

    #[test]
    fn default_label_applies_when_no_rule_matches() {
        let mut c = cfg(vec![]);
        c.default_label = NamedEgressLabel::LocalOnly;
        let l = EgressLabeler::from_config(&c);
        let r = l.resolve_label("anything", None);
        assert_eq!(r.label, EgressLabel::local_only());
        assert!(r.matched.is_empty());
        assert_eq!(r.kind(), "default");
    }

    #[test]
    fn default_unrestricted_is_inert() {
        let l = EgressLabeler::from_config(&EgressConfig::default());
        assert!(l.is_inert());
        let l2 = EgressLabeler::from_config(&cfg(vec![rule("email.*", None, NamedEgressLabel::LocalOnly)]));
        assert!(!l2.is_inert());
    }

    // ── session policy ───────────────────────────────────────────────────

    #[test]
    fn session_rules_merge_and_restrict() {
        let l = EgressLabeler::from_config(&cfg(vec![]))
            .with_session_rules(vec![rule("slack.*", None, NamedEgressLabel::NoRemoteModel)]);
        assert!(!l.is_inert());
        let r = l.resolve_label("slack.read", None);
        assert_eq!(r.label, EgressLabel::no_remote_model());
        assert_eq!(r.kind(), "session_rule");
        assert_eq!(r.matched[0].scope, RuleScope::Session);
    }

    #[test]
    fn global_and_session_rules_both_recorded() {
        let l = EgressLabeler::from_config(&cfg(vec![rule(
            "email.*",
            None,
            NamedEgressLabel::NoRemoteModel,
        )]))
        .with_session_rules(vec![rule("email.read", None, NamedEgressLabel::LocalOnly)]);
        let r = l.resolve_label("email.read", None);
        // Intersection of no_remote_model and local_only = local_only.
        assert_eq!(r.label, EgressLabel::local_only());
        assert_eq!(r.kind(), "operator_and_session_rule");
        assert!(r.matched.iter().any(|m| m.scope == RuleScope::Global));
        assert!(r.matched.iter().any(|m| m.scope == RuleScope::Session));
    }

    #[test]
    fn session_default_restricts_but_cannot_widen() {
        // Global default local_only; a session asking for no_remote_model (a
        // *wider* label) must not loosen it — resolution intersects.
        let mut c = cfg(vec![]);
        c.default_label = NamedEgressLabel::LocalOnly;
        let l = EgressLabeler::from_config(&c).with_session_policy(&EgressSessionPolicy {
            rules: vec![],
            default_label: Some(NamedEgressLabel::NoRemoteModel),
            provider_constraint: None,
        });
        assert_eq!(l.resolve_label("anything", None).label, EgressLabel::local_only());

        // The other direction genuinely restricts: unrestricted global default
        // narrowed to local_only by the session.
        let l2 = EgressLabeler::from_config(&EgressConfig::default()).with_session_policy(
            &EgressSessionPolicy {
                rules: vec![],
                default_label: Some(NamedEgressLabel::LocalOnly),
                provider_constraint: None,
            },
        );
        assert!(!l2.is_inert(), "a restricting session default cancels the fast path");
        assert_eq!(l2.resolve_label("anything", None).label, EgressLabel::local_only());
    }

    #[test]
    fn empty_session_policy_leaves_the_fast_path_intact() {
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_session_policy(&EgressSessionPolicy::default());
        assert!(l.is_inert());
    }

    // ── inert fast path ──────────────────────────────────────────────────

    #[test]
    fn label_tool_result_returns_none_when_inert() {
        let l = EgressLabeler::from_config(&EgressConfig::default());
        let req = LabelRequest {
            tool: "fs_read",
            arguments_json: "{}",
            tool_call_id: "tc_1",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior());
        assert!(out.is_none());
    }

    #[test]
    fn label_tool_result_returns_none_when_label_is_unrestricted() {
        // A rule exists but doesn't match this source → unrestricted → no event.
        let l = EgressLabeler::from_config(&cfg(vec![rule("email.*", None, NamedEgressLabel::LocalOnly)]));
        let req = LabelRequest {
            tool: "fs_read",
            arguments_json: "{}",
            tool_call_id: "tc_1",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior());
        assert!(out.is_none());
    }

    #[test]
    fn label_tool_result_emits_envelope_when_restricted() {
        let l = EgressLabeler::from_config(&cfg(vec![rule("email.read", None, NamedEgressLabel::LocalOnly)]));
        let req = LabelRequest {
            tool: "email.read",
            arguments_json: r#"{"box":"inbox"}"#,
            tool_call_id: "tc_42",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior()).expect("restricted");
        assert!(out.is_restricted());
        assert!(out.envelope_id.starts_with("env_"));
        assert_eq!(out.label, EgressLabel::local_only());
        assert_eq!(out.provenance.tool.as_deref(), Some("email.read"));
        assert_eq!(out.provenance.matched_rules, vec!["email.read"]);
    }

    // ── exec-shaped tools ────────────────────────────────────────────────

    #[test]
    fn exec_shaped_covers_both_spellings_and_both_tools() {
        assert!(is_exec_shaped("sandbox.exec"));
        assert!(is_exec_shaped("sandbox_exec"));
        assert!(is_exec_shaped("artifact.exec"));
        assert!(is_exec_shaped("artifact_exec"));
        assert!(!is_exec_shaped("fs_read"));
    }

    /// Regression for the canonical-name mismatch: the tool arrives as
    /// `sandbox_exec`, the rule is written `sandbox.exec`. Before normalization
    /// the static path matcher never ran at all.
    #[test]
    fn sandbox_exec_with_labeled_path_is_restricted() {
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        let req = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"cat ~/mail/inbox/1"}"#,
            tool_call_id: "tc_exec",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior()).expect("restricted");
        assert_eq!(out.label, EgressLabel::local_only());
        assert!(out.provenance.matched_rules.iter().any(|r| r.contains("~/mail/**")));
    }

    #[test]
    fn sandbox_exec_clean_command_is_unrestricted() {
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        let req = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"echo hello"}"#,
            tool_call_id: "tc_exec",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior());
        assert!(out.is_none(), "clean exec should not be labeled");
    }

    /// Regression for the source-mismatch bug (PR #911 review): a path-bearing
    /// rule for `fs.read` must NOT label a `sandbox.exec` result just because
    /// the command touched the same path. Only rules whose `source` matches
    /// `sandbox.exec` apply to a sandbox.exec result.
    #[test]
    fn sandbox_exec_ignores_path_rules_for_other_sources() {
        let l = EgressLabeler::from_config(&cfg(vec![
            // fs.read rule with the same path pattern as the sandbox rule.
            rule("fs.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
            // No sandbox.exec rule at all.
        ]));
        let req = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"cat ~/mail/inbox/1"}"#,
            tool_call_id: "tc_exec",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior());
        // The fs.read rule does not apply to sandbox.exec → unrestricted → no label.
        assert!(out.is_none(), "fs.read path rule must not label sandbox.exec");
    }

    #[test]
    fn sandbox_exec_source_only_rule_applies_without_path_match() {
        // A source-only sandbox.exec rule (no path) applies to every exec.
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("sandbox.exec", None, NamedEgressLabel::NoRemoteModel),
        ]));
        let req = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"echo hello"}"#,
            tool_call_id: "tc_exec",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior()).expect("restricted");
        assert_eq!(out.label, EgressLabel::no_remote_model());
    }

    // ── Agent workspace taint (RFC §11, #1001) ───────────────────────────

    fn ws_store(
    ) -> (tempfile::TempDir, std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(tmp.path()).unwrap(),
        );
        (tmp, store)
    }

    /// The narrow half of the workspace ratchet: an exec that resolves
    /// restricted (here: a rule's static path match) tightens the agent's
    /// durable workspace label.
    #[test]
    fn exec_resolving_restricted_narrows_the_workspace_label() {
        let (_tmp, store) = ws_store();
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        let req = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"unzip ~/mail/mail.zip -d /tmp/w && cat /tmp/w/inbox.mbox"}"#,
            tool_call_id: "tc_ws",
        };
        let out = l
            .label_tool_result(&req, None, "sess", "coder.abc", None, Some(&store), &no_prior())
            .expect("rule-matched exec must be restricted");
        assert_eq!(out.label, EgressLabel::local_only());
        assert_eq!(
            store.get_workspace_egress_label("coder.abc").unwrap(),
            Some(EgressLabel::local_only()),
            "a restricted exec narrows the workspace it ran in"
        );
    }

    /// The apply half: a *clean* exec (no rule fires) running in a labeled
    /// workspace inherits the workspace's label — this is what closes the
    /// laundering path when the moved content is read back later.
    #[test]
    fn exec_in_a_labeled_workspace_inherits_the_label() {
        let (_tmp, store) = ws_store();
        store
            .restrict_workspace_egress_label("coder.abc", &EgressLabel::local_only())
            .unwrap();
        // No rules at all: only the workspace label can restrict.
        let l = EgressLabeler::from_config(&cfg(vec![]));
        let req = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"cat /tmp/w/inbox.mbox"}"#,
            tool_call_id: "tc_ws2",
        };
        let out = l
            .label_tool_result(&req, None, "sess", "coder.abc", None, Some(&store), &no_prior())
            .expect("workspace label must carry the taint");
        assert_eq!(out.label, EgressLabel::local_only());
        // The workspace label was intersected in and names the agent.
        let l2 = EgressLabeler::from_config(&cfg(vec![]));
        let req2 = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"cat /tmp/w/inbox.mbox"}"#,
            tool_call_id: "tc_ws2b",
        };
        let out2 = l2
            .label_tool_result(&req2, None, "sess", "coder.abc", None, Some(&store), &no_prior())
            .expect("workspace label applies again");
        // The apply is stable — the workspace label is not widened by use.
        assert_eq!(out2.label, EgressLabel::local_only());
    }

    /// Structured tools do not run in a workspace: they neither inherit its
    /// label nor narrow it.
    #[test]
    fn structured_tools_do_not_touch_the_workspace_label() {
        let (_tmp, store) = ws_store();
        store
            .restrict_workspace_egress_label("coder.struct", &EgressLabel::no_remote_model())
            .unwrap();
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("content.read", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        // A structured read of a laundered path: no rule fires (path outside
        // the pattern), and the workspace label must NOT apply.
        let clean = LabelRequest {
            tool: "content.read",
            arguments_json: r#"{"path":"/tmp/w/inbox.mbox"}"#,
            tool_call_id: "tc_ws3",
        };
        assert!(
            l.label_tool_result(&clean, None, "sess", "coder.struct", None, Some(&store), &no_prior())
                .is_none(),
            "structured tools must not inherit workspace taint"
        );
        // A restricted structured result must NOT narrow the workspace either.
        let restricted = LabelRequest {
            tool: "content.read",
            arguments_json: r#"{"path":"~/mail/inbox/1"}"#,
            tool_call_id: "tc_ws4",
        };
        let out = l
            .label_tool_result(&restricted, None, "sess", "coder.struct", None, Some(&store), &no_prior())
            .expect("rule-matched structured read restricted");
        assert_eq!(out.label, EgressLabel::local_only());
        assert_eq!(
            store.get_workspace_egress_label("coder.struct").unwrap(),
            Some(EgressLabel::no_remote_model()),
            "structured tools never write the workspace label"
        );
    }

    /// The read side's failure contract (#1001, Copilot review): a store read
    /// error fails closed to `local_only` for the result, and marks the read as
    /// failed so the caller skips the write-back — a transient read failure must
    /// not become a durable workspace restriction (only an exec that *actually*
    /// resolved restricted may ratchet the workspace).
    #[test]
    fn workspace_read_error_fails_closed_and_marks_failure() {
        let (tmp, store) = ws_store();
        // Force a read error by dropping the table out from under the store.
        let raw = rusqlite::Connection::open(tmp.path().join("gateway.db")).unwrap();
        raw.execute_batch("DROP TABLE agent_workspace_egress_labels")
            .unwrap();
        let (label, applied, failed) = workspace_taint_from_store("coder.abc", store.as_ref());
        assert_eq!(label, EgressLabel::local_only());
        assert_eq!(applied, vec!["coder.abc".to_string()]);
        assert!(failed, "the caller must be told the read failed");
        // A healthy read marks no failure.
        let (_tmp2, store2) = ws_store();
        let (label, applied, failed) = workspace_taint_from_store("coder.xyz", store2.as_ref());
        assert!(label.is_unrestricted());
        assert!(applied.is_empty());
        assert!(!failed);
    }

    // ── Compression-preset eligibility (RFC §5.7) ─────────────────────────

    fn tool_msg(id: &str, content: &str) -> crate::llm::Message {
        crate::llm::Message {
            id: None,
            role: crate::llm::Role::Tool,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: Some(id.to_string()),
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    fn user_msg(content: &str) -> crate::llm::Message {
        crate::llm::Message {
            id: None,
            role: crate::llm::Role::User,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    #[test]
    fn compression_eligible_when_no_labels() {
        // Unconfigured: nothing labeled → always eligible.
        let band = vec![tool_msg("tc_1", "data")];
        let elig = compression_preset_eligible(&band, &Default::default(), EgressClass::Remote);
        assert!(elig.is_eligible());
    }

    #[test]
    fn compression_eligible_when_all_unrestricted() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("tc_1".to_string(), EgressLabel::unrestricted());
        let band = vec![tool_msg("tc_1", "public data")];
        // Unrestricted band → eligible on either sink.
        assert!(compression_preset_eligible(&band, &labels, EgressClass::Remote).is_eligible());
        assert!(compression_preset_eligible(&band, &labels, EgressClass::Local).is_eligible());
    }

    #[test]
    fn compression_ineligible_local_only_on_remote_preset() {
        // The core §5.7 case: local_only history on a remote compression preset.
        let mut labels = std::collections::HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let band = vec![tool_msg("tc_secret", "CANARY-SECRET")];
        let elig = compression_preset_eligible(&band, &labels, EgressClass::Remote);
        assert!(!elig.is_eligible());
        match elig {
            CompressionEligibility::Ineligible { leaked_tool_call_ids, .. } => {
                assert_eq!(leaked_tool_call_ids, vec!["tc_secret".to_string()]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn compression_eligible_local_only_on_local_preset() {
        // The local model is a cleared sink for local_only — eligible.
        let mut labels = std::collections::HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let band = vec![tool_msg("tc_secret", "secret")];
        let elig = compression_preset_eligible(&band, &labels, EgressClass::Local);
        assert!(elig.is_eligible());
    }

    #[test]
    fn compression_ineligible_no_remote_model_on_remote_preset() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("tc_conf".to_string(), EgressLabel::no_remote_model());
        let band = vec![tool_msg("tc_conf", "business-confidential")];
        assert!(
            !compression_preset_eligible(&band, &labels, EgressClass::Remote).is_eligible(),
            "no_remote_model band must not compress on remote preset"
        );
        assert!(
            compression_preset_eligible(&band, &labels, EgressClass::Local).is_eligible(),
            "no_remote_model band may compress on local preset"
        );
    }

    #[test]
    fn compression_mixed_band_tainted_by_any_local_only() {
        // One local_only result in the band taints the whole compression call.
        let mut labels = std::collections::HashMap::new();
        labels.insert("tc_public".to_string(), EgressLabel::unrestricted());
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let band = vec![tool_msg("tc_public", "ok"), tool_msg("tc_secret", "secret")];
        let elig = compression_preset_eligible(&band, &labels, EgressClass::Remote);
        assert!(!elig.is_eligible());
    }

    #[test]
    fn partition_by_label_splits_clean_and_tainted() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("tc_public".to_string(), EgressLabel::unrestricted());
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let band = vec![
            user_msg("clean work"),
            tool_msg("tc_public", "ok"),
            tool_msg("tc_secret", "CANARY"),
        ];
        let bands = partition_by_label(&band, &labels);
        assert_eq!(bands.len(), 2, "mixed history must yield two bands");
        assert!(bands[0].label.is_unrestricted(), "unrestricted band first");
        assert_eq!(bands[0].messages.len(), 2); // user + public tool
        assert_eq!(bands[1].label, EgressLabel::local_only());
        assert_eq!(bands[1].messages.len(), 1);
        assert_eq!(bands[1].source_ids, vec!["tc_secret".to_string()]);
        // Per-band eligibility: clean may go remote; tainted must not.
        assert!(compression_preset_eligible(&bands[0].messages, &labels, EgressClass::Remote)
            .is_eligible());
        assert!(!compression_preset_eligible(&bands[1].messages, &labels, EgressClass::Remote)
            .is_eligible());
    }

    #[test]
    fn synthesized_band_label_records_parent_ids() {
        let band = LabelBand {
            label: EgressLabel::local_only(),
            messages: vec![tool_msg("tc_secret", "x")],
            source_ids: vec!["tc_secret".into(), "msg_summary".into()],
        };
        let (label, prov) = synthesized_band_label(&band);
        assert_eq!(label, EgressLabel::local_only());
        assert_eq!(prov.tool.as_deref(), Some("context.compress"));
        assert_eq!(
            prov.parent_envelope_ids,
            vec!["tc_secret".to_string(), "msg_summary".to_string()]
        );
    }

    #[test]
    fn compression_unlabeled_tool_result_does_not_taint() {
        // A tool result with no label entry = unrestricted default → doesn't block.
        let labels: std::collections::HashMap<String, EgressLabel> = std::collections::HashMap::new();
        // tc_unlabeled has no entry in `labels`.
        let band = vec![tool_msg("tc_unlabeled", "data")];
        assert!(
            compression_preset_eligible(&band, &labels, EgressClass::Remote).is_eligible(),
            "unlabeled tool result (unrestricted default) must not block compression"
        );
    }

    #[test]
    fn compression_ignores_non_tool_messages() {
        // User/assistant messages don't carry tool_call_id labels in this phase.
        let mut labels = std::collections::HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let band = vec![
            user_msg("summarize my emails"),
            tool_msg("tc_secret", "secret"),
        ];
        // The user message is ignored; the tool result taints → ineligible.
        assert!(
            !compression_preset_eligible(&band, &labels, EgressClass::Remote).is_eligible()
        );
    }

    #[test]
    fn artifact_exec_uses_the_same_static_analysis() {
        let l = EgressLabeler::from_config(&cfg(vec![
            rule("artifact.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        let req = LabelRequest {
            tool: "artifact_exec",
            arguments_json: r#"{"command":"python3 read.py ~/mail/archive.mbox"}"#,
            tool_call_id: "tc_exec",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior()).expect("restricted");
        assert_eq!(out.label, EgressLabel::local_only());
    }

    /// The dependency read (RFC §4.2, §5.6 step 3): the labeled path is only in
    /// the *script*, which the command merely names. Scanning the command line
    /// alone misses it.
    #[test]
    fn sandbox_exec_labels_a_dependency_script_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("parse_mail.py"),
            "import mailbox\nmb = mailbox.mbox(\"~/mail/archive.mbox\")\n",
        )
        .unwrap();

        let l = EgressLabeler::from_config(&cfg(vec![
            rule("sandbox.exec", Some("~/mail/**"), NamedEgressLabel::LocalOnly),
        ]));
        let req = LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"python3 parse_mail.py"}"#,
            tool_call_id: "tc_exec",
        };

        // Without the context the script is invisible → nothing labeled.
        assert!(
            l.label_tool_result(&req, None, "sess", "agent", None, None, &no_prior()).is_none(),
            "command line alone carries no labeled path"
        );

        // With it, the script's read is caught.
        let ctx = ExecSourceContext {
            agent_dir: Some(dir.path()),
            gateway_dir: None,
            session_id: Some("sess"),
            gateway_store: None,
        };
        let out = l
            .label_tool_result(&req, Some(&ctx), "sess", "agent", None, None, &no_prior())
            .expect("dependency read should be labeled");
        assert_eq!(out.label, EgressLabel::local_only());
    }

    #[test]
    fn sink_set_label_from_a_rule_is_honored_verbatim() {
        // A rule may carry a raw sink-set rather than a named label.
        let custom = EgressLabel::from_sinks([Sink::LocalModel, Sink::UserReply]);
        let l = EgressLabeler::from_config(&cfg(vec![EgressRule {
            source: "email.*".to_string(),
            path: None,
            label: custom.clone(),
        }]));
        assert_eq!(l.resolve_label("email_read", None).label, custom);
    }

    // ── Bundle-declared floor (RFC §4.1 path 2) ───────────────────────────

    #[test]
    fn floor_applies_from_manifest_with_no_operator_rules() {
        // No operator rules, unrestricted default — but a bundle-only floor
        // makes the labeler non-inert and restricts every result.
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(Some(EgressLabel::local_only()));
        assert!(!l.is_inert(), "a bundle floor clears inertness");
        let r = l.resolve_label("anything", None);
        assert_eq!(r.label, EgressLabel::local_only());
        assert!(r.bundle_floor_applied);
    }

    #[test]
    fn floor_cannot_widen_operator_policy() {
        // Operator says local_only; bundle declares an unrestricted floor →
        // the floor is a no-op (intersection with unrestricted = identity).
        let l = EgressLabeler::from_config(&cfg(vec![rule(
            "email.read",
            None,
            NamedEgressLabel::LocalOnly,
        )]))
        .with_manifest_floor(Some(EgressLabel::unrestricted()));
        let r = l.resolve_label("email.read", None);
        assert_eq!(r.label, EgressLabel::local_only());
        // The floor was "applied" but didn't change anything (unrestricted is
        // the identity for intersection).
        assert!(r.bundle_floor_applied);
    }

    #[test]
    fn bundle_floor_intersects_with_operator_rule() {
        // Operator says no_remote_model; bundle floor is local_only →
        // intersection = local_only (the stricter).
        let l = EgressLabeler::from_config(&cfg(vec![rule(
            "fs.read",
            None,
            NamedEgressLabel::NoRemoteModel,
        )]))
        .with_manifest_floor(Some(EgressLabel::local_only()));
        let r = l.resolve_label("fs.read", None);
        assert_eq!(r.label, EgressLabel::local_only());
    }

    #[test]
    fn floor_without_rules_still_labels_clean_source() {
        // A bundle floor applies to every tool result, even sources no rule
        // mentions — that's the point of a bundle-wide floor.
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(Some(EgressLabel::no_remote_model()));
        let r = l.resolve_label("completely_unknown_tool", None);
        assert_eq!(r.label, EgressLabel::no_remote_model());
        assert!(r.bundle_floor_applied);
    }

    #[test]
    fn no_floor_keeps_inert_behavior() {
        // No floor, no rules, unrestricted default → still inert.
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(None);
        assert!(l.is_inert());
    }

    #[test]
    fn floor_labeled_result_emits_envelope() {
        // End-to-end: floor makes the labeler non-inert and emits an event
        // for a restricted result, even with no operator rules at all.
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(Some(EgressLabel::local_only()));
        let req = LabelRequest {
            tool: "custom_tool",
            arguments_json: "{}",
            tool_call_id: "tc_floor",
        };
        let out = l
            .label_tool_result(&req, None, "sess", "agent", None, None, &no_prior())
            .expect("floor should produce a restricted envelope");
        assert_eq!(out.label, EgressLabel::local_only());
        assert!(out.is_restricted());
    }

    // ── Argument taint (RFC §4.1 path 3) ──────────────────────────────────

    #[test]
    fn taint_by_handle_reference_intersects_parent_label() {
        // Tool called with an argument referencing a prior local_only tool
        // result → output labeled local_only, parent_envelope_ids populated.
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(Some(EgressLabel::unrestricted()));
        let taint = prior(&[("tc_prior_secret", EgressLabel::local_only(), None)]);
        // The args reference the prior tool_call_id by handle.
        let req = LabelRequest {
            tool: "content_write",
            arguments_json: r#"{"source_ref":"tc_prior_secret","content":"derived"}"#,
            tool_call_id: "tc_derived",
        };
        let out = l
            .label_tool_result(&req, None, "sess", "agent", None, None, &taint)
            .expect("tainted result should be labeled");
        assert_eq!(out.label, EgressLabel::local_only());
        assert_eq!(
            out.provenance.parent_envelope_ids,
            vec!["tc_prior_secret".to_string()]
        );
    }

    #[test]
    fn taint_by_verbatim_content_intersects_parent_label() {
        // The arguments contain the prior result's content verbatim → taint
        // fires even without an explicit handle reference.
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(Some(EgressLabel::unrestricted()));
        let prior_content = "CANARY-SECRET-EMAIL-CONTENT";
        let taint = prior(&[(
            "tc_email_read",
            EgressLabel::local_only(),
            Some(prior_content),
        )]);
        let req = LabelRequest {
            tool: "content_write",
            arguments_json: &format!(r#"{{"content":"Here is the data: {prior_content}"}}"#),
            tool_call_id: "tc_copy",
        };
        let out = l
            .label_tool_result(&req, None, "sess", "agent", None, None, &taint)
            .expect("verbatim taint should fire");
        assert_eq!(out.label, EgressLabel::local_only());
        assert!(out.provenance.parent_envelope_ids.contains(&"tc_email_read".to_string()));
    }

    #[test]
    fn clean_argument_produces_no_taint() {
        // No reference to any prior labeled result → no taint, no parents.
        let l = EgressLabeler::from_config(&cfg(vec![rule(
            "content_write",
            None,
            NamedEgressLabel::Unrestricted,
        )]))
        .with_manifest_floor(Some(EgressLabel::unrestricted()));
        let taint = prior(&[(
            "tc_prior_secret",
            EgressLabel::local_only(),
            Some("SECRET-DATA-NOT-IN-ARGS"),
        )]);
        let req = LabelRequest {
            tool: "content_write",
            arguments_json: r#"{"content":"completely unrelated clean content"}"#,
            tool_call_id: "tc_clean",
        };
        // Unrestricted label → label_tool_result returns None (no envelope).
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &taint);
        assert!(out.is_none(), "clean argument with unrestricted label → no envelope");
    }

    #[test]
    fn taint_intersection_of_two_parents() {
        // Two tainted parents: local_only ∩ no_remote_model = local_only.
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(Some(EgressLabel::unrestricted()));
        let taint = prior(&[
            ("tc_a", EgressLabel::local_only(), None),
            ("tc_b", EgressLabel::no_remote_model(), None),
        ]);
        // Args reference both prior ids.
        let req = LabelRequest {
            tool: "content_write",
            arguments_json: r#"{"refs":["tc_a","tc_b"]}"#,
            tool_call_id: "tc_combined",
        };
        let out = l
            .label_tool_result(&req, None, "sess", "agent", None, None, &taint)
            .expect("tainted by two parents");
        assert_eq!(out.label, EgressLabel::local_only());
        assert_eq!(out.provenance.parent_envelope_ids.len(), 2);
    }

    #[test]
    fn taint_intersects_with_rule_label() {
        // A rule already restricts to no_remote_model; a tainted parent adds
        // local_only → intersection = local_only.
        let l = EgressLabeler::from_config(&cfg(vec![rule(
            "content_write",
            None,
            NamedEgressLabel::NoRemoteModel,
        )]))
        .with_manifest_floor(Some(EgressLabel::unrestricted()));
        let taint = prior(&[("tc_secret", EgressLabel::local_only(), None)]);
        let req = LabelRequest {
            tool: "content_write",
            arguments_json: r#"{"ref":"tc_secret"}"#,
            tool_call_id: "tc_out",
        };
        let out = l
            .label_tool_result(&req, None, "sess", "agent", None, None, &taint)
            .expect("should be labeled");
        assert_eq!(out.label, EgressLabel::local_only());
    }

    #[test]
    fn empty_snippet_does_not_taint() {
        // An empty content snippet must not match (would match everything).
        let l = EgressLabeler::from_config(&EgressConfig::default())
            .with_manifest_floor(Some(EgressLabel::unrestricted()));
        let taint = prior(&[("tc_empty", EgressLabel::local_only(), Some(""))]);
        let req = LabelRequest {
            tool: "content_write",
            arguments_json: r#"{"content":"anything"}"#,
            tool_call_id: "tc_test",
        };
        let out = l.label_tool_result(&req, None, "sess", "agent", None, None, &taint);
        // No handle match (tc_empty not in args), no verbatim match (empty
        // snippet) → unrestricted → None.
        assert!(out.is_none());
    }

    // ── Taint-following routing (RFC §5.3) ────────────────────────────────

    fn cand(name: &str, class: Option<EgressClass>) -> PresetCandidate {
        PresetCandidate {
            name: name.to_string(),
            egress_class: class,
        }
    }

    #[test]
    fn preset_sink_defaults_remote_fail_closed() {
        assert_eq!(preset_sink(None), Sink::RemoteModel);
        assert_eq!(preset_sink(Some(EgressClass::Remote)), Sink::RemoteModel);
        assert_eq!(preset_sink(Some(EgressClass::Local)), Sink::LocalModel);
    }

    #[test]
    fn eligibility_local_only_batch_admits_only_local() {
        let batch = EgressLabel::local_only();
        assert!(preset_batch_eligible(&batch, Some(EgressClass::Local)));
        assert!(!preset_batch_eligible(&batch, Some(EgressClass::Remote)));
        // Unclassified defaults remote → excluded by a local_only batch.
        assert!(!preset_batch_eligible(&batch, None));
    }

    #[test]
    fn unrestricted_batch_is_a_fast_noop() {
        let plan = plan_taint_following_route(
            &EgressLabel::unrestricted(),
            Some(EgressClass::Remote),
            &[cand("local", Some(EgressClass::Local))],
            None,
        );
        assert!(plan.primary_eligible);
        assert!(plan.reroute_to.is_none());
        assert!(!plan.no_eligible_provider());
        // Fast path does not bother enumerating the eligible set.
        assert!(plan.eligible.is_empty());
    }

    #[test]
    fn clean_batch_keeps_remote_primary() {
        // A `no_remote_model` batch still excludes remote, but an unrestricted
        // batch (the clean code turn) keeps the remote primary.
        let plan = plan_taint_following_route(
            &EgressLabel::unrestricted(),
            Some(EgressClass::Remote),
            &[
                cand("sonnet", Some(EgressClass::Remote)),
                cand("local", Some(EgressClass::Local)),
            ],
            None,
        );
        assert!(plan.primary_eligible);
        assert!(plan.reroute_to.is_none());
    }

    #[test]
    fn tainted_batch_reroutes_remote_primary_to_local() {
        // The email turn: local_only batch, remote primary → reroute to the
        // local preset; the remote preset is not eligible.
        let plan = plan_taint_following_route(
            &EgressLabel::local_only(),
            Some(EgressClass::Remote),
            &[
                cand("sonnet", Some(EgressClass::Remote)),
                cand("ollama", Some(EgressClass::Local)),
            ],
            None,
        );
        assert!(!plan.primary_eligible);
        assert_eq!(plan.eligible, vec!["ollama".to_string()]);
        assert_eq!(plan.reroute_to.as_ref().map(|c| c.name.as_str()), Some("ollama"));
        assert!(!plan.no_eligible_provider());
    }

    #[test]
    fn tainted_batch_local_primary_stays_no_reroute() {
        let plan = plan_taint_following_route(
            &EgressLabel::local_only(),
            Some(EgressClass::Local),
            &[cand("ollama", Some(EgressClass::Local))],
            None,
        );
        assert!(plan.primary_eligible);
        assert!(plan.reroute_to.is_none());
    }

    #[test]
    fn tainted_batch_no_local_preset_refuses() {
        // local_only batch, remote primary, no local preset configured → no
        // eligible provider → the turn must refuse (egress_no_eligible_provider).
        let plan = plan_taint_following_route(
            &EgressLabel::local_only(),
            Some(EgressClass::Remote),
            &[cand("sonnet", Some(EgressClass::Remote))],
            None,
        );
        assert!(!plan.primary_eligible);
        assert!(plan.eligible.is_empty());
        assert!(plan.reroute_to.is_none());
        assert!(plan.no_eligible_provider());
    }

    #[test]
    fn reroute_prefers_local_then_stable_by_name() {
        // Two local presets eligible → prefer local (both are), then the
        // alphabetically-first name, deterministically.
        let plan = plan_taint_following_route(
            &EgressLabel::local_only(),
            Some(EgressClass::Remote),
            &[
                cand("zeta-local", Some(EgressClass::Local)),
                cand("alpha-local", Some(EgressClass::Local)),
            ],
            None,
        );
        assert_eq!(
            plan.reroute_to.as_ref().map(|c| c.name.as_str()),
            Some("alpha-local")
        );
        assert_eq!(
            plan.eligible,
            vec!["alpha-local".to_string(), "zeta-local".to_string()]
        );
    }

    #[test]
    fn no_remote_model_batch_admits_local_and_network_not_remote() {
        // A `no_remote_model` batch (business-confidential but federatable-ish)
        // excludes a remote preset but admits a local one.
        let batch = EgressLabel::no_remote_model();
        let plan = plan_taint_following_route(
            &batch,
            Some(EgressClass::Remote),
            &[
                cand("sonnet", Some(EgressClass::Remote)),
                cand("ollama", Some(EgressClass::Local)),
            ],
            None,
        );
        assert!(!plan.primary_eligible);
        assert_eq!(plan.reroute_to.as_ref().map(|c| c.name.as_str()), Some("ollama"));
    }

    // ── message_egress_key (RFC §3.4 join key) ────────────────────────────

    #[test]
    fn message_egress_key_tool_uses_tool_call_id_others_use_id() {
        // Tool results join by tool_call_id.
        let mut tool = tool_msg("tc_1", "x");
        tool.id = Some("msg_ignored".to_string());
        assert_eq!(message_egress_key(&tool), Some("tc_1"));

        // Non-tool messages join by the stable msg id.
        let mut asst = crate::llm::Message::assistant("summary");
        asst.id = Some("msg_summary".to_string());
        assert_eq!(message_egress_key(&asst), Some("msg_summary"));

        // A non-tool message with no id has no key (treated as unlabeled).
        let user = crate::llm::Message::user("hi");
        assert_eq!(message_egress_key(&user), None);

        // A tool message with no tool_call_id also has no key.
        let mut orphan_tool = tool_msg("tc_x", "y");
        orphan_tool.tool_call_id = None;
        assert_eq!(message_egress_key(&orphan_tool), None);
    }

    // ── session_accumulated_taint (RFC §5.5 cross-agent) ──────────────────

    #[test]
    fn session_accumulated_taint_intersects_everything_touched() {
        use std::collections::HashMap;
        // Empty session touched nothing → unrestricted (nothing to carry).
        assert!(session_accumulated_taint(&HashMap::new()).is_unrestricted());

        // A session that touched only clean content → unrestricted.
        let mut clean = HashMap::new();
        clean.insert("tc_1".to_string(), EgressLabel::unrestricted());
        assert!(session_accumulated_taint(&clean).is_unrestricted());

        // A session that read email → carries local_only.
        let mut mail = HashMap::new();
        mail.insert("tc_email".to_string(), EgressLabel::local_only());
        mail.insert("tc_clean".to_string(), EgressLabel::unrestricted());
        assert_eq!(session_accumulated_taint(&mail), EgressLabel::local_only());

        // Mixed restrictive labels intersect to the most restrictive.
        let mut mixed = HashMap::new();
        mixed.insert("a".to_string(), EgressLabel::no_remote_model());
        mixed.insert("b".to_string(), EgressLabel::local_only());
        let taint = session_accumulated_taint(&mixed);
        assert_eq!(taint, EgressLabel::local_only());
        assert!(!taint.allows(Sink::RemoteModel));
    }

    #[test]
    fn compression_band_tainted_by_labeled_assistant_message() {
        // §5.7 + §4.5: a labeled assistant summary (joined by msg id) taints the
        // compression band exactly like a labeled tool result — so a mixed band
        // is ineligible on a remote preset.
        let mut summary = crate::llm::Message::assistant("the local summary");
        summary.id = Some("msg_summary".to_string());
        let band = vec![crate::llm::Message::user("q"), summary];
        let mut labels = std::collections::HashMap::new();
        labels.insert("msg_summary".to_string(), EgressLabel::local_only());
        let elig = compression_preset_eligible(&band, &labels, EgressClass::Remote);
        assert!(!elig.is_eligible(), "local_only summary must block remote compression");
        let elig_local = compression_preset_eligible(&band, &labels, EgressClass::Local);
        assert!(elig_local.is_eligible(), "local preset may compress the local_only band");
    }
}
