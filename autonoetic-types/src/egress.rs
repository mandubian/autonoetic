//! Data Envelopes — Egress Localization Label Plane
//!
//! Foundation for the gateway-enforced **label plane** described in
//! `docs/rfc/data-envelopes-egress-localization.md`. This module defines the
//! *types* only; gateway-side enforcement (chokepoint filtering, indication
//! substitution, routing) lands in subsequent phases (#905, #907).
//!
//! ## Complement, not replacement, of [`crate::disclosure`]
//!
//! - **Disclosure** = the *inward* direction (what the assistant may repeat to
//!   the user / viewers; `DisclosureClass`).
//! - **Egress** = the *outward* direction (where content may flow afterward:
//!   to a remote provider, a peer gateway, a `share_net` sandbox, durable
//!   memory, …).
//!
//! The two enums are kept separate (RFC §10); `UserReply` is the sink that
//! bridges them.
//!
//! ## Key invariants (RFC §2)
//!
//! - Labels are **declared metadata, manipulated only by the gateway** — never
//!   set, stripped, or read by agents (Lawful-Executor, RFC §14).
//! - Restriction is the **meet** of the lattice: an output's label is the
//!   intersection of its inputs' allowed-sink sets. The operation is named
//!   `restrict` / `intersect` in code and prose — **never `join`**, which in
//!   lattice terms means *widening* and produces inverted code.
//! - **Credentials are not envelopes** (RFC §3.2). There is deliberately no
//!   `secret` label: the vault path never creates envelopes, so there is
//!   nothing to label. Residual `credential_env` exposure lives in §11, not
//!   behind a vacuous `{}` label.
//!
//! ## Default
//!
//! Unlabeled content is [`EgressLabel::unrestricted`] by decision (RFC §2.2,
//! §14). Tightening later is a one-line config flip (`default_label`).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Sink
// ---------------------------------------------------------------------------

/// A sink class: a place data can flow to.
///
/// A provider is mapped to one of [`Sink::LocalModel`] / [`Sink::RemoteModel`]
/// via [`EgressClass`] + the preset's `egress_class` (RFC §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sink {
    /// Provider classified `local` (ollama/vllm/lmstudio/llamacpp, or explicit).
    LocalModel,
    /// Provider classified `remote` (the default for anything unclassified).
    RemoteModel,
    /// Another agent session on this gateway.
    LocalAgent,
    /// A peer gateway over OFP federation.
    FederatedAgent,
    /// Sandboxed code with `share_net`, `web.call` bodies, remote MCP args.
    Network,
    /// Durable memory (Tier-1 state/, Tier-2 SQLite) — survives cross-session.
    ///
    /// Included in [`EgressLabel::local_only`] by decision: durable *labeled*
    /// memory beats forcing a choice between memory and privacy (RFC §3.2).
    MemoryPersist,
    /// The assistant reply to the operator — bridges [`crate::disclosure`].
    UserReply,
}

impl Sink {
    /// All sinks, in a stable order. Used by [`EgressLabel::unrestricted`].
    pub const ALL: [Sink; 7] = [
        Sink::LocalModel,
        Sink::RemoteModel,
        Sink::LocalAgent,
        Sink::FederatedAgent,
        Sink::Network,
        Sink::MemoryPersist,
        Sink::UserReply,
    ];
}

// ---------------------------------------------------------------------------
// EgressLabel — allowed-sink set, a meet-lattice under restriction
// ---------------------------------------------------------------------------

/// Set of allowed sinks. The label plane's core type.
///
/// Labels form a **meet-lattice under restriction**: the meet of two labels is
/// [`EgressLabel::intersect`] (a.k.a. `restrict`) — the set of sinks *both*
/// permit. Restriction can only ever shrink the allowed-sink set; the only way
/// to widen a label is an operator-approved **declassification** (RFC §8).
///
/// Implemented as a [`BTreeSet<Sink>`] newtype (no `bitflags` dependency) for
/// deterministic serde + cheap equality.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EgressLabel(BTreeSet<Sink>);

impl EgressLabel {
    /// Build a label from an iterator of sinks.
    pub fn from_sinks<I: IntoIterator<Item = Sink>>(sinks: I) -> Self {
        Self(sinks.into_iter().collect())
    }

    /// Empty label — no sink is allowed. Excludes *everything*.
    ///
    /// This is the **absorbing element** of `intersect` (`x ∩ ∅ = ∅`), not the
    /// identity — the identity is the universe ([`EgressLabel::unrestricted`],
    /// since `x ∩ U = x`). Rarely the right default; prefer `unrestricted` or
    /// one of the named labels. Used internally where an over-restrictive
    /// fallback is the safe choice.
    pub fn empty() -> Self {
        Self(BTreeSet::new())
    }

    /// **`unrestricted`** (RFC §3.2) — all sinks allowed. The default for
    /// ordinary workspace content.
    pub fn unrestricted() -> Self {
        Self(Sink::ALL.iter().copied().collect())
    }

    /// **`local_only`** (RFC §3.2) — content the operator refuses to ship off
    /// the machine: `{LocalModel, LocalAgent, UserReply, MemoryPersist}`.
    /// Emails, personal files, etc.
    pub fn local_only() -> Self {
        Self::from_sinks([
            Sink::LocalModel,
            Sink::LocalAgent,
            Sink::UserReply,
            Sink::MemoryPersist,
        ])
    }

    /// **`no_remote_model`** (RFC §3.2) — business-confidential but
    /// federatable: all sinks except `RemoteModel` / `FederatedAgent`.
    pub fn no_remote_model() -> Self {
        Self::from_sinks([
            Sink::LocalModel,
            Sink::LocalAgent,
            Sink::Network,
            Sink::MemoryPersist,
            Sink::UserReply,
        ])
    }

    /// Whether the label permits the given sink.
    pub fn allows(&self, sink: Sink) -> bool {
        self.0.contains(&sink)
    }

    /// Whether the label permits every sink (i.e. is `unrestricted`).
    pub fn is_unrestricted(&self) -> bool {
        self.0.len() == Sink::ALL.len()
    }

    /// Whether the label is empty (no sink permitted at all).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterate over the allowed sinks, in stable order.
    pub fn iter(&self) -> impl Iterator<Item = Sink> + '_ {
        self.0.iter().copied()
    }

    /// Number of allowed sinks.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// **Restrict** this label to the intersection with `other` (lattice meet).
    ///
    /// The only composition operation the label plane permits. *Never* called
    /// `join` — that name means widening in lattice terms and inverts the
    /// semantics. An output's label is the intersection of its inputs' labels.
    #[must_use]
    pub fn restrict(self, other: &EgressLabel) -> EgressLabel {
        let mut out = self;
        out.0.retain(|s| other.0.contains(s));
        out
    }

    /// In-place variant of [`Self::restrict`].
    pub fn intersect(&mut self, other: &EgressLabel) {
        self.0.retain(|s| other.0.contains(s));
    }

    /// Restrict against a stream of labels; equivalent to folding `restrict`.
    /// Empty iterator → returns `self` unchanged (intersection identity = the
    /// original label, since restriction against the universe is a no-op).
    #[must_use]
    pub fn restrict_all<'a>(self, others: impl IntoIterator<Item = &'a EgressLabel>) -> EgressLabel {
        let mut acc = self;
        for o in others {
            acc.intersect(o);
        }
        acc
    }

    /// Snapshot as a sorted `BTreeSet<Sink>` (for serialization/debug).
    pub fn as_set(&self) -> &BTreeSet<Sink> {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// EgressClass — the provider-side classification that a sink derives from
// ---------------------------------------------------------------------------

/// Coarse classification of an LLM provider endpoint: `local` or `remote`.
///
/// A provider classified `local` maps to [`Sink::LocalModel`]; `remote` maps to
/// [`Sink::RemoteModel`]. This is the *only* thing the label plane needs from a
/// provider at request time (RFC §5.1). Defaults to [`EgressClass::Remote`]
/// (fail closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EgressClass {
    /// Provider resolves to [`Sink::LocalModel`] (ollama/vllm/lmstudio/llamacpp
    /// by inference, or explicitly set). A *remote* Ollama server is a real
    /// deployment shape — set `egress_class: remote` to override the inference.
    Local,
    /// Provider resolves to [`Sink::RemoteModel`]. **Default** for anything
    /// unclassified (RFC §2.2 fail-closed).
    #[default]
    Remote,
}

impl EgressClass {
    /// The [`Sink`] this class represents in the label plane.
    pub const fn as_sink(self) -> Sink {
        match self {
            EgressClass::Local => Sink::LocalModel,
            EgressClass::Remote => Sink::RemoteModel,
        }
    }
}

// ---------------------------------------------------------------------------
// Provenance, Indication, EnvelopeContent, DataEnvelope
// ---------------------------------------------------------------------------

/// Where an envelope's label came from (RFC §9.1).
///
/// Records the *inputs* to label resolution so "why is this labeled?" is always
/// answerable from the causal chain. Content-free metadata only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The source tool name that produced this envelope (e.g. `email.read`,
    /// `sandbox.exec`, `fs.read`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// SHA-256 digest (hex, truncated) of the producing tool-call arguments.
    /// Lets the causal event reference "which call" without embedding content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_digest: Option<String>,
    /// Names of the operator source rules whose intersection produced the label
    /// (RFC §4.1). Empty when only the default applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_rules: Vec<String>,
    /// Parent envelope ids when argument-taint contributed to this label
    /// (RFC §4.1 path 3). Populated in phase 2 (#907).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_envelope_ids: Vec<String>,
}

/// Safe substitute for a withheld envelope (RFC §3.3).
///
/// Generated from provenance metadata only — never content. Verbosity
/// (`terse` vs `descriptive`) is a session-policy knob consumed by the
/// chokepoint in phase 1b (#905).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Indication {
    /// Human-readable, non-divulging string, e.g.
    /// `[withheld: 2× email.read results — policy local_only]`.
    pub text: String,
    /// Terse form (`[content withheld]`) for maximally private deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terse: Option<String>,
}

/// The payload of an envelope. Held by the gateway; never serialized into agent
/// context (the chokepoint substitutes an [`Indication`] instead — RFC §3.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnvelopeContent {
    /// Textual content (tool result body, assistant message, …).
    Text(String),
    /// Reference into content-addressed storage (artifact id), for binary/large
    /// payloads.
    ArtifactRef { artifact_id: String, bytes_len: u64 },
}

impl EnvelopeContent {
    /// The textual view, if this envelope is text-backed. Returns `None` for
    /// artifact refs (the gateway resolves those lazily).
    pub fn as_text(&self) -> Option<&str> {
        match self {
            EnvelopeContent::Text(t) => Some(t),
            EnvelopeContent::ArtifactRef { .. } => None,
        }
    }
}

/// A data envelope — labeled content manipulated only by the gateway (RFC §3.1).
///
/// Envelopes are **born** at labeling boundaries (tool result, LLM response,
/// memory recall, …) and **flow** through the label plane: every derivation
/// intersects (`restrict`) the labels of its inputs. The gateway substitutes an
/// [`Indication`] wherever an envelope's label excludes the target sink.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataEnvelope {
    /// `env_<id>` — referenced by causal events. Ephemeral random id minted at
    /// labeling time.
    pub id: String,
    /// The allowed-sink set (label).
    pub label: EgressLabel,
    /// Where the label came from (RFC §9.1).
    pub provenance: Provenance,
    /// Safe substitute used when this envelope is withheld (RFC §3.3).
    #[serde(default)]
    pub indication: Indication,
    /// The payload.
    pub content: EnvelopeContent,
}

// ---------------------------------------------------------------------------
// Operator source rules + global config (RFC §4.2)
// ---------------------------------------------------------------------------

/// One operator source rule: "content from this source/path flows to this label"
/// (RFC §4.2). Rules can only **restrict**; the label of an envelope is the
/// intersection of *all* matching rules (order-independent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressRule {
    /// Source tool pattern. Supports `*`-suffix globs (`email.*`, `mcp.gmail.*`)
    /// and bare names (`fs.read`, `sandbox.exec`), mirroring the
    /// [`crate::disclosure::DisclosureRule`] shape.
    pub source: String,
    /// Optional path narrowing for path-taking tools (`~/mail/**`,
    /// `state/secrets/*`). When present, the rule matches only calls touching a
    /// path matched by the (very simple) glob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The label to apply. Named labels (`unrestricted`, `local_only`,
    /// `no_remote_model`) or a sink-set literal.
    pub label: EgressLabel,
}

/// Operator-global egress configuration (RFC §4.2 / §5.4). Rides on
/// [`crate::config::GatewayConfig`]. Session-scoped additions land in the
/// session `egress_policy` (phase 1b #905).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressConfig {
    /// Ordered list of operator source rules. All matching rules apply (their
    /// labels are intersected) — there is no first-match-wins (RFC §4.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<EgressRule>,

    /// Default label for content no rule labels. **`unrestricted`** by decision
    /// (RFC §2.2). One-line config flip to tighten later.
    #[serde(default = "EgressConfig::default_label_default")]
    pub default_label: NamedEgressLabel,

    /// How to treat sources no rule covers. `unrestricted` (decided default) is
    /// simply off; `prompt_once` (RFC §4.4) is deferred and not honored yet.
    #[serde(default = "EgressConfig::default_unclassified_mode")]
    pub unclassified_source_mode: UnclassifiedSourceMode,
}

impl EgressConfig {
    /// Default for [`EgressConfig::default_label`] — `unrestricted`.
    pub fn default_label_default() -> NamedEgressLabel {
        NamedEgressLabel::Unrestricted
    }

    /// Default for [`EgressConfig::unclassified_source_mode`] — `unrestricted`.
    pub fn default_unclassified_mode() -> UnclassifiedSourceMode {
        UnclassifiedSourceMode::Unrestricted
    }
}

/// Named, well-known labels usable directly from config / manifests (RFC §3.2).
///
/// Custom labels are just sink-set literals on [`EgressRule::label`]; these
/// names exist for ergonomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NamedEgressLabel {
    #[default]
    Unrestricted,
    LocalOnly,
    NoRemoteModel,
}

impl NamedEgressLabel {
    /// Materialize the named label into a concrete [`EgressLabel`].
    pub fn to_label(self) -> EgressLabel {
        match self {
            NamedEgressLabel::Unrestricted => EgressLabel::unrestricted(),
            NamedEgressLabel::LocalOnly => EgressLabel::local_only(),
            NamedEgressLabel::NoRemoteModel => EgressLabel::no_remote_model(),
        }
    }
}

/// How sources no operator rule covers are treated (RFC §4.2 / §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UnclassifiedSourceMode {
    /// Default label applies (`unrestricted` by decision). The current mode.
    #[default]
    Unrestricted,
    /// RFC §4.4 first-touch classification — **deferred**, not honored yet.
    PromptOnce,
    /// Treat unclassified sources as `local_only`. Conservative flip.
    LocalOnly,
}

// ---------------------------------------------------------------------------
// Simple `*`-suffix glob matching — shared between config parsing and runtime
// rule evaluation (RFC §4.2 path patterns; mirrors disclosure.rs).
// ---------------------------------------------------------------------------

/// Very simple glob-style matcher (`state/secrets/*`, `~/mail/**`, exact).
///
/// Intentionally limited to what [`crate::disclosure::DisclosureRule`]
/// already supports (a `*`-suffix prefix match) — no `globset` dependency.
/// Exposed here so both config-time validation and runtime evaluation agree.
pub fn matches_simple_glob(pattern: &str, candidate: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    // Treat a trailing `**` the same as a single `*`: prefix match against the
    // segment before it. Keeps the matcher uniform with disclosure.rs while
    // accepting the RFC's `~/mail/**` notation.
    let pat = pattern.trim_end_matches('*');
    if pat.is_empty() {
        // Pattern was all `*`-chars — matches everything non-empty.
        return !candidate.is_empty();
    }
    if pattern.ends_with('*') {
        candidate.starts_with(pat)
    } else {
        candidate == pat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sink ──────────────────────────────────────────────────────────────

    #[test]
    fn sink_serde_is_snake_case() {
        let v = serde_json::to_string(&Sink::LocalModel).unwrap();
        assert_eq!(v, "\"local_model\"");
        let s: Sink = serde_json::from_str("\"remote_model\"").unwrap();
        assert_eq!(s, Sink::RemoteModel);
        let s: Sink = serde_json::from_str("\"memory_persist\"").unwrap();
        assert_eq!(s, Sink::MemoryPersist);
        let s: Sink = serde_json::from_str("\"user_reply\"").unwrap();
        assert_eq!(s, Sink::UserReply);
        let s: Sink = serde_json::from_str("\"federated_agent\"").unwrap();
        assert_eq!(s, Sink::FederatedAgent);
    }

    #[test]
    fn sink_all_has_seven_distinct() {
        let set: BTreeSet<Sink> = Sink::ALL.iter().copied().collect();
        assert_eq!(set.len(), 7);
        assert_eq!(Sink::ALL.len(), 7);
    }

    // ── EgressLabel: predefined sets ──────────────────────────────────────

    #[test]
    fn unrestricted_allows_everything() {
        let l = EgressLabel::unrestricted();
        for s in Sink::ALL {
            assert!(l.allows(s), "unrestricted should allow {s:?}");
        }
        assert!(l.is_unrestricted());
        assert_eq!(l.len(), 7);
        assert!(!l.is_empty());
    }

    #[test]
    fn local_only_excludes_remote_and_network() {
        let l = EgressLabel::local_only();
        assert!(l.allows(Sink::LocalModel));
        assert!(l.allows(Sink::LocalAgent));
        assert!(l.allows(Sink::UserReply));
        assert!(l.allows(Sink::MemoryPersist));
        assert!(!l.allows(Sink::RemoteModel));
        assert!(!l.allows(Sink::FederatedAgent));
        assert!(!l.allows(Sink::Network));
        assert_eq!(l.len(), 4);
    }

    #[test]
    fn no_remote_model_blocks_only_remote_model_and_federated() {
        let l = EgressLabel::no_remote_model();
        assert!(!l.allows(Sink::RemoteModel));
        assert!(!l.allows(Sink::FederatedAgent));
        for s in [
            Sink::LocalModel,
            Sink::LocalAgent,
            Sink::Network,
            Sink::MemoryPersist,
            Sink::UserReply,
        ] {
            assert!(l.allows(s), "no_remote_model should allow {s:?}");
        }
        assert_eq!(l.len(), 5);
    }

    // ── EgressLabel: intersection (restrict) is monotonic ─────────────────

    #[test]
    fn restrict_never_widens() {
        let a = EgressLabel::unrestricted();
        let b = EgressLabel::local_only();
        let ab = a.restrict(&b);
        // restrict(unrestricted, local_only) == local_only
        assert_eq!(ab, b);
        assert!(!ab.allows(Sink::RemoteModel));
        assert!(ab.allows(Sink::LocalModel));
    }

    #[test]
    fn restrict_is_commutative() {
        let a = EgressLabel::local_only();
        let b = EgressLabel::no_remote_model();
        assert_eq!(a.clone().restrict(&b), b.clone().restrict(&a));
        // local_only ∩ no_remote_model == local_only (local_only is the stricter)
        assert_eq!(a.clone().restrict(&b), a);
    }

    #[test]
    fn restrict_with_unrestricted_is_identity() {
        let l = EgressLabel::local_only();
        assert_eq!(l.clone().restrict(&EgressLabel::unrestricted()), l);
    }

    #[test]
    fn restrict_can_only_shrink() {
        let base = EgressLabel::no_remote_model();
        let restricted = base.clone().restrict(&EgressLabel::local_only());
        // result is a subset of base
        for s in Sink::ALL {
            if restricted.allows(s) {
                assert!(base.allows(s), "restrict added a sink {s:?} — widening!");
            }
        }
    }

    #[test]
    fn restrict_all_folds_intersection() {
        let acc = EgressLabel::unrestricted()
            .restrict_all([&EgressLabel::local_only(), &EgressLabel::no_remote_model()]);
        assert_eq!(acc, EgressLabel::local_only());
    }

    #[test]
    fn intersect_in_place() {
        let mut l = EgressLabel::no_remote_model();
        l.intersect(&EgressLabel::local_only());
        assert_eq!(l, EgressLabel::local_only());
    }

    #[test]
    fn empty_label_blocks_everything() {
        let e = EgressLabel::empty();
        for s in Sink::ALL {
            assert!(!e.allows(s));
        }
        assert!(e.is_empty());
    }

    // ── EgressClass ───────────────────────────────────────────────────────

    #[test]
    fn egress_class_default_is_remote_fail_closed() {
        assert_eq!(EgressClass::default(), EgressClass::Remote);
        assert_eq!(EgressClass::default().as_sink(), Sink::RemoteModel);
        assert_eq!(EgressClass::Local.as_sink(), Sink::LocalModel);
    }

    #[test]
    fn egress_class_serde_snake_case() {
        assert_eq!(serde_json::to_string(&EgressClass::Local).unwrap(), "\"local\"");
        let r: EgressClass = serde_json::from_str("\"remote\"").unwrap();
        assert_eq!(r, EgressClass::Remote);
    }

    // ── Envelope serde roundtrip ──────────────────────────────────────────

    #[test]
    fn envelope_roundtrips() {
        let env = DataEnvelope {
            id: "env_abc12345".to_string(),
            label: EgressLabel::local_only(),
            provenance: Provenance {
                tool: Some("email.read".to_string()),
                args_digest: Some("deadbeef".to_string()),
                matched_rules: vec!["email.*".to_string()],
                parent_envelope_ids: vec![],
            },
            indication: Indication {
                text: "[withheld: 1× email.read result — policy local_only]".to_string(),
                terse: Some("[content withheld]".to_string()),
            },
            content: EnvelopeContent::Text("Subject: ...".to_string()),
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: DataEnvelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "env_abc12345");
        assert_eq!(back.label, EgressLabel::local_only());
        assert_eq!(back.provenance.tool.as_deref(), Some("email.read"));
        assert_eq!(back.provenance.matched_rules, vec!["email.*".to_string()]);
        assert_eq!(
            back.content.as_text().unwrap(),
            "Subject: ..."
        );
    }

    #[test]
    fn label_serializes_as_sink_array() {
        let l = EgressLabel::local_only();
        let s = serde_json::to_string(&l).unwrap();
        // BTreeSet serializes sorted; local_only = {local_agent, local_model,
        // memory_persist, user_reply}
        assert!(s.contains("\"local_model\""));
        assert!(s.contains("\"local_agent\""));
        assert!(s.contains("\"memory_persist\""));
        assert!(s.contains("\"user_reply\""));
        assert!(!s.contains("\"remote_model\""));
        let back: EgressLabel = serde_json::from_str(&s).unwrap();
        assert_eq!(back, l);
    }

    // ── NamedEgressLabel ──────────────────────────────────────────────────

    #[test]
    fn named_labels_materialize() {
        assert_eq!(
            NamedEgressLabel::Unrestricted.to_label(),
            EgressLabel::unrestricted()
        );
        assert_eq!(
            NamedEgressLabel::LocalOnly.to_label(),
            EgressLabel::local_only()
        );
        assert_eq!(
            NamedEgressLabel::NoRemoteModel.to_label(),
            EgressLabel::no_remote_model()
        );
    }

    #[test]
    fn named_label_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&NamedEgressLabel::LocalOnly).unwrap(),
            "\"local_only\""
        );
        let n: NamedEgressLabel = serde_json::from_str("\"no_remote_model\"").unwrap();
        assert_eq!(n, NamedEgressLabel::NoRemoteModel);
    }

    // ── Config shape ──────────────────────────────────────────────────────

    #[test]
    fn egress_config_parses_named_fields() {
        // The *named* label form (NamedEgressLabel) parses from a bare string;
        // EgressRule.label carries a concrete sink-set. Round-trip the whole
        // config through serde_json (serde_yaml is not a dep of this crate;
        // the shape is identical).
        let json = serde_json::json!({
            "rules": [
                {"source": "email.*", "label": ["local_model", "local_agent", "user_reply", "memory_persist"]},
                {"source": "fs.read", "path": "~/mail/**", "label": ["local_model", "local_agent", "user_reply", "memory_persist"]}
            ],
            "default_label": "unrestricted",
            "unclassified_source_mode": "unrestricted"
        });
        let cfg: EgressConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.default_label, NamedEgressLabel::Unrestricted);
        assert_eq!(cfg.unclassified_source_mode, UnclassifiedSourceMode::Unrestricted);
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[0].source, "email.*");
        assert_eq!(cfg.rules[0].label, EgressLabel::local_only());
        assert_eq!(cfg.rules[1].path.as_deref(), Some("~/mail/**"));
    }

    #[test]
    fn egress_config_defaults_are_unrestricted() {
        let cfg = EgressConfig::default();
        assert_eq!(cfg.default_label, NamedEgressLabel::Unrestricted);
        assert_eq!(cfg.unclassified_source_mode, UnclassifiedSourceMode::Unrestricted);
        assert!(cfg.rules.is_empty());
    }

    // ── Glob matcher ──────────────────────────────────────────────────────

    #[test]
    fn glob_matches_suffix_wildcard() {
        assert!(matches_simple_glob("state/secrets/*", "state/secrets/foo"));
        assert!(matches_simple_glob("~/mail/**", "~/mail/inbox/1"));
        assert!(!matches_simple_glob("state/secrets/*", "state/public/foo"));
    }

    #[test]
    fn glob_matches_exact_when_no_star() {
        assert!(matches_simple_glob("fs.read", "fs.read"));
        assert!(!matches_simple_glob("fs.read", "fs.write"));
    }

    #[test]
    fn glob_star_only_matches_anything_nonempty() {
        assert!(matches_simple_glob("*", "anything"));
        assert!(!matches_simple_glob("*", ""));
    }

    #[test]
    fn glob_empty_pattern_never_matches() {
        assert!(!matches_simple_glob("", "x"));
    }
}
