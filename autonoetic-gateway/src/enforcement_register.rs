//! Enforcement Register — the generated bridge between constitutional
//! **clauses** (principles and rights) and the concrete code that enforces
//! them (epic #297; design `docs/design/constitution-restructure.md`).
//!
//! The constitution is being restructured so the *signed* artifact holds a
//! small set of **principles** (rules — invariants binding the *agent*) and
//! **rights** (guarantees binding the *gateway*), while the large, churning
//! "how" lives here as a register mapping each clause to its mechanical
//! checks, code citations, tests, and config knobs. Splitting the two keeps
//! the signed law legible and lets enforcement detail scale with the code.
//!
//! **Bind direction is first-class** (#299): every clause records whether it
//! binds the agent ([`Binds::Agent`] — a rule) or the gateway
//! ([`Binds::Gateway`] — a right). The obligations-to-rights balance is then
//! a computed, visible signal rather than something buried in a flat table.
//!
//! Coverage so far (grows as #303 migrates the remaining sections):
//! - Principle `P-7` "Bounded progress" (the loop-guard family — #298).
//! - Rights `Ri-0.13` (reasoning privacy) and `Ri-0.14` (child wake-up),
//!   seeded as the proof that rights are register-modelled the same way.
//!
//! Deliberately **not yet** here (tracked in #298/#299):
//! - a `#[enforces(...)]` proc-macro to derive entries from code annotations
//!   (entries are hand-authored for now);
//! - runtime verification that each cited `test` exists/passes;
//! - signing the register digest into the constitution lock;
//! - restructuring `constitution.md` into a Bill of Rights (needs a signed
//!   version bump — bundled with the #303 migration).
//!
//! Clause vs rule: each entry's `clause_id` is the parent principle/right
//! (e.g. `P-7`, `Ri-0.14`); its `rule_id` is the specific numbered
//! sub-clause it enforces (e.g. `P-7.19`). The flat `R-x.y` table is gone —
//! rules were renumbered `R-x.y` → `P-x.y` in the #303 migration; no `R-`
//! alias is retained.

/// Which party a clause binds. A *rule* (principle) constrains the agent; a
/// *right* constrains the gateway on the agent's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binds {
    Agent,
    Gateway,
    /// §O decider obligations bind whoever *decides* a gate (operator, an
    /// agent-decider, or a policy engine) — the symmetric counterpart to the
    /// agent's rule-duties and the gateway's right-duties (#359).
    Decider,
}

impl Binds {
    pub fn label(self) -> &'static str {
        match self {
            Binds::Agent => "agent",
            Binds::Gateway => "gateway",
            Binds::Decider => "decider",
        }
    }
}

/// A constitutional principle — a rule-side invariant binding the agent. The
/// signed constitution carries these (`P-*`); enforcement detail lives in
/// [`enforcement_register`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Principle {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
    /// See [`Right::entrenched`] — the same correction-core concept applied
    /// to a principle. P-8.1 (hash-chain integrity) is the principle-side
    /// member of the entrenched core: without a tamper-evident causal chain,
    /// every other correction relies on evidence that could be silently
    /// rewritten.
    pub entrenched: bool,
}

/// A constitutional right — a guarantee the gateway upholds for the agent.
/// First-class alongside principles (#299); also enforced by concrete code,
/// so it appears in the register the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Right {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
    /// Part of the entrenched correction core (`docs/philosophy.md` §3.1 /
    /// §4.1): a clause whose loss would remove the machinery other errors are
    /// corrected through. May be strengthened by ordinary amendment; a
    /// weakening or removal amendment additionally requires the explicit,
    /// dated justification recorded in that version's `RATIFY.md` (see the
    /// constitution's Amendment Process, "Entrenched clauses"). See
    /// [`entrenched_clauses`] for the structural backstop.
    pub entrenched: bool,
}

/// A single enforcement point: which clause it serves (principle *or*
/// right), the numbered rule it enforces, the mechanical check, the code that
/// implements it, the test that pins it, and the config knob(s) that tune it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcementEntry {
    /// Parent clause this enforces — a `P-*` principle id or an `Ri-*` right id.
    pub clause_id: &'static str,
    /// The specific numbered rule/right id (e.g. `P-7.19`, `Ri-0.14`).
    pub rule_id: &'static str,
    /// Stable machine identifier for the check (matches the `reason`/`event`
    /// code emitted on causal events where applicable).
    pub check_id: &'static str,
    pub code: &'static str,
    pub test: &'static str,
    pub config: Option<&'static str>,
}

/// Principles (rule-side, bind the agent). Seeded with the loop-guard family
/// as the proof; grows as #303 migrates the remaining sections. `P-8.1` is
/// migrated here (ahead of the rest of §8) because `docs/philosophy.md` §3.1
/// names it as a member of the entrenched correction core — migrating it
/// lets the entrenchment backstop cover it structurally.
pub fn principles() -> &'static [Principle] {
    &[
        Principle {
            id: "P-2",
            title: "Approval Gates",
            statement: "Promotion and gate actions are bounded so that repeated mechanical \
                        rejection cannot be respawned indefinitely across sessions without \
                        operator acknowledgement.",
            entrenched: false,
        },
        Principle {
            id: "P-5",
            title: "Deterministic coercion and response validation",
            statement: "The gateway normalizes model I/O only through deterministic, \
                        pre-committed tolerances; every such intervention is observable and \
                        counted as a named discretion leak (§14). No gateway judgment about \
                        the agent's output is silent or hidden.",
            entrenched: false,
        },
        Principle {
            id: "P-7",
            title: "Bounded progress",
            statement: "A session is halted when it stops making progress, on a closed, \
                        configurable set of mechanically-detected non-progress conditions, \
                        each emitting a typed, attributable reason. No condition relies on \
                        agent self-report.",
            entrenched: false,
        },
        // P-8.1 — the causal chain is append-only and tamper-evident. This is
        // the substrate every correction-machinery clause depends on (read
        // your history, attribute decisions, prove what happened): if the
        // chain can be silently rewritten, none of those rights hold.
        Principle {
            id: "P-8.1",
            title: "Hash-chain integrity",
            statement: "The causal chain is append-only JSONL with hash-chain integrity — \
                        each entry's `entry_hash` binds its fields and its `prev_hash` links \
                        it to the prior entry. Tampering with any recorded field (actor, \
                        action, outcome) leaves a stale hash detectable by recomputation.",
            entrenched: true,
        },
    ]
}

/// Rights (gateway-side, bind the gateway). Seeded with two real rights as
/// the proof that rights are register-modelled identically to principles,
/// plus the four rights `docs/philosophy.md` §3.1/§4.1 names as the
/// **entrenched correction core** — the machinery through which every other
/// constitutional error gets fixed (read your own history, know why you were
/// denied, be able to propose change, be non-repudiably attributed).
pub fn rights() -> &'static [Right] {
    &[
        Right {
            id: "Ri-0.2",
            title: "Own history is readable",
            statement: "Every agent may read its own causal chain and execution trace. The \
                        gateway does not hide actions taken on the agent's behalf. Audit is not \
                        a privilege of operators; it is a right of the subject.",
            entrenched: true,
        },
        Right {
            id: "Ri-0.3",
            title: "Named rejection",
            statement: "Every rejection names the rule ID that caused it. No agent is ever told \
                        \"denied\" without being told why. Rejection without explanation is \
                        indistinguishable from arbitrary authority.",
            entrenched: true,
        },
        Right {
            id: "Ri-0.8",
            title: "Right to propose amendment",
            statement: "Any agent holding the ConstitutionalProposal capability may submit an \
                        amendment proposal through the declared channel. The proposal receives a \
                        durable ID and enters the review queue; it cannot be silently dropped.",
            entrenched: true,
        },
        Right {
            id: "Ri-0.11",
            title: "Non-repudiation",
            statement: "Every action an agent performs is attributed to that agent on the causal \
                        chain and cannot be retroactively reattributed. The agent can prove what \
                        it did; no party can claim the agent performed an action it did not.",
            entrenched: true,
        },
        Right {
            id: "Ri-0.13",
            title: "Reasoning privacy",
            statement: "An agent's internal reasoning is private-under-law: not used by the \
                        gateway as a basis for policy decisions, recorded to the agent's own \
                        causal chain for forensic review, and disclosed to other parties only \
                        through capability-gated audit.",
            entrenched: false,
        },
        Right {
            id: "Ri-0.14",
            title: "Wake-up over polling",
            statement: "When a child task reaches a terminal state or resolves a gate, the \
                        gateway wakes the parent with typed child state. Parents are not \
                        required to poll to discover child-state transitions.",
            entrenched: false,
        },
        Right {
            id: "Ri-0.17",
            title: "Self capsule export (emigration)",
            statement: "An agent may request export of its own cognitive capsule for \
                        migration to another gateway. Scoped to the caller's own identity.",
            entrenched: false,
        },
    ]
}

/// Correction-core clause IDs — principles, rights, and obligations marked
/// [`Principle::entrenched`] / [`Right::entrenched`] / [`Obligation::entrenched`].
/// Pure lookup; see [`tests::entrenched_clauses_all_exist_in_register`] for
/// the structural backstop that keeps this list honest as the register
/// evolves.
pub fn entrenched_clauses() -> Vec<&'static str> {
    principles()
        .iter()
        .filter(|p| p.entrenched)
        .map(|p| p.id)
        .chain(rights().iter().filter(|r| r.entrenched).map(|r| r.id))
        .chain(obligations().iter().filter(|o| o.entrenched).map(|o| o.id))
        .collect()
}

/// A §O decider obligation — a duty binding whoever *decides* a gate. The
/// symmetric counterpart to a [`Principle`] (agent) and a [`Right`] (gateway);
/// modelled in the register the same way (#359 / #399).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obligation {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
    /// See [`Right::entrenched`] — the same correction-core concept applied
    /// to a decider obligation.
    pub entrenched: bool,
}

/// Decider obligations (§O, bind the decider). Seeded with the two enacted
/// clauses (O-1 motivation, O-2 attribution); O-3/O-4/O-5 enter by amendment as
/// each becomes mechanically enforced (#399).
pub fn obligations() -> &'static [Obligation] {
    &[
        Obligation {
            id: "O-1",
            title: "Motivated decision",
            statement: "A decision owes a motivation, graduated by stakes. A rejection/abort, or \
                        an approval of an elevated-authority or external/irreversible action, is \
                        BLOCKING: it does not commit until a non-empty reason is recorded. Silent \
                        rejection by a decider is as illegitimate as a gateway denial with no rule \
                        ID (Ri-0.3).",
            entrenched: true,
        },
        Obligation {
            id: "O-2",
            title: "Attributed decision",
            statement: "Every decision is attributed to the deciding principal (id + kind) on the \
                        causal chain and cannot be reattributed. The agent under decision can \
                        always tell who decided and what kind of principal they are.",
            entrenched: false,
        },
    ]
}

/// The enforcement register. P-7's four checks (P-7.5/7.7/7.19/7.20), one
/// check per seeded right, plus §O decider obligations (O-1/O-2).
pub fn enforcement_register() -> &'static [EnforcementEntry] {
    &[
        // ── P-5 (deterministic coercion / response validation, binds agent) ──
        // Both entries are marked in the constitution as "DISCRETION LEAK"
        // — the gateway is doing its job, but any place it substitutes its
        // own judgment for the agent's is a named debt, not an invisible
        // convenience. The register entry makes it attributable in
        // contract-health.
        EnforcementEntry {
            clause_id: "P-5",
            rule_id: "P-5.2",
            check_id: "input_normalization_leak",
            code: "runtime/discretion_leak.rs::record_discretion_leak (tokio::task_local scope) \
                   + runtime/tool_call_processor.rs::note_llm_normalization \
                   + runtime/response_validation.rs::strip_markdown_code_fences",
            test: "runtime::discretion_leak::tests",
            config: None,
        },
        EnforcementEntry {
            clause_id: "P-5",
            rule_id: "P-5.8",
            check_id: "gateway_authored_repair_leak",
            code: "runtime/response_validation.rs::validate_and_maybe_repair (gateway-authored repair prompt) \
                   + runtime/discretion_leak.rs::record_discretion_leak",
            test: "runtime::discretion_leak::tests",
            config: Some("response_validation.repair_enabled, response_validation.max_validation_loops, max_validation_duration_ms"),
        },
        // ── P-7 (binds agent) ──
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.5",
            check_id: "tool_failure_budget",
            code: "guard.rs::register_failure + check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_tool_failure_budget",
            config: Some("loop_guard.max_tool_failures"),
        },
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.7",
            check_id: "no_meaningful_progress",
            code: "guard.rs::check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_max_loops",
            config: Some("loop_guard.max_loops_without_progress"),
        },
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.19",
            check_id: "rotating_polling_pattern",
            code: "guard.rs::register_progress_inner (window + trip) + check_loop",
            test: "runtime::guard::tests::rotating_polling_pattern_with_five_tools_trips",
            config: Some("loop_guard.rotation_window_size, loop_guard.rotation_distinct_floor"),
        },
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.20",
            check_id: "child_failure_budget",
            code: "guard.rs::register_child_failure + check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_child_failures",
            config: Some("loop_guard.max_child_failures"),
        },
        // ── P-2.29 (issue #720) ──
        EnforcementEntry {
            clause_id: "P-2",
            rule_id: "P-2.29",
            check_id: "promotion_attempts_exhausted",
            code: "runtime/promotion_governor.rs::check_attempt_exhaustion + runtime/tools/agent_revision.rs::record_attempt",
            test: "promotion_attempt_exhaustion_integration.rs",
            config: Some("promotion_governor.max_promotion_attempts_per_revision"),
        },
        // ── P-8.1 (binds agent, entrenched — correction core: tamper-evident chain) ──
        EnforcementEntry {
            clause_id: "P-8.1",
            rule_id: "P-8.1",
            check_id: "hash_chain_integrity",
            code: "causal_chain.rs::compute_entry_hash (SHA-256 over actor_id + prev_hash + fields) + append-only linkage",
            test: "constitution_rights_early_bucket.rs::ri_0_11_tampered_actor_id_leaves_stale_hash",
            config: None,
        },
        // ── Ri-0.2 (binds gateway, entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.2",
            rule_id: "Ri-0.2",
            check_id: "own_history_readable",
            code: "observability.* tools gated by ReadAccess capability",
            test: "constitution_rights_early_bucket.rs::ri_0_2_agent_with_read_access_can_search_own_traces",
            config: None,
        },
        // ── Ri-0.3 (binds gateway, entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.3",
            rule_id: "Ri-0.3",
            check_id: "named_rejection",
            code: "Tagged::permission_with_rules + PolicyDecision.enforced_rules",
            test: "constitution_rights_late_bucket.rs::ri_0_3_capability_rejection_carries_rule_ids",
            config: None,
        },
        // ── Ri-0.8 (binds gateway, entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.8",
            rule_id: "Ri-0.8",
            check_id: "amendment_proposal_intake",
            code: "runtime/tools/constitution.rs::constitution_propose_amendment \
                   + scheduler/gateway_store/constitutional_proposals.rs",
            test: "constitution_rights_amendment_proposal.rs",
            config: None,
        },
        // ── Ri-0.11 (binds gateway, entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.11",
            rule_id: "Ri-0.11",
            check_id: "non_repudiation",
            code: "causal chain hash integrity + agent_id on every event; compute_entry_hash binds actor_id",
            test: "constitution_rights_early_bucket.rs::ri_0_11_hash_chain_integrity",
            config: None,
        },
        // ── Ri-0.13 (binds gateway) ──
        EnforcementEntry {
            clause_id: "Ri-0.13",
            rule_id: "Ri-0.13",
            check_id: "reasoning_disclosure_capability_gated",
            code: "runtime/tools/observability.rs (reasoning audit) + disclosure gating",
            test: "constitution_private_reasoning_c.rs::ri_0_13c_execute_reads_and_discloses",
            config: None,
        },
        // ── Ri-0.14 (binds gateway) ──
        EnforcementEntry {
            clause_id: "Ri-0.14",
            rule_id: "Ri-0.14",
            check_id: "child_state_wakeup",
            code: "scheduler/workflow_store.rs::update_task_run_status (send_child_state_notification) \
                   + scheduler/signal.rs + scheduler/task_notify.rs",
            test: "constitution_right_ri_0_14.rs::child_waiting_transition_emits_typed_parent_wakeup_event",
            config: Some("default_workflow_wait_secs"),
        },
        // ── Ri-0.17 (binds gateway) ──
        EnforcementEntry {
            clause_id: "Ri-0.17",
            rule_id: "Ri-0.17",
            check_id: "self_capsule_export",
            code: "runtime/tools/capsule.rs::CapsuleExportTool (two-tier gate) \
                   + policy.rs::can_use_capsule_self",
            test: "capsule_self_export_scoping_integration.rs::self_export_denied_for_other_agent_id",
            config: None,
        },
        // ── O-1 (binds decider) ──
        EnforcementEntry {
            clause_id: "O-1",
            rule_id: "O-1",
            check_id: "decider_obligation_motivation",
            code: "scheduler/approval.rs::enforce_decider_motivation (classifier decision_is_blocking) \
                   at the decide_request_with_options chokepoint; emits decider_obligation.refused/.satisfied",
            test: "constitution_o_1_decider_motivation.rs + scheduler::approval::tests::decider_obligation_emits_tagged_o1_event",
            config: Some("decider_obligations.enabled"),
        },
        // ── O-2 (binds decider) ──
        EnforcementEntry {
            clause_id: "O-2",
            rule_id: "O-2",
            check_id: "decider_attribution",
            code: "decided_by + decided_by_kind on the approval (principal::decider_principal_kind, #361) \
                   + actor bound into the causal-chain entry hash (causal_chain.rs)",
            test: "constitution_o_1_decider_motivation.rs",
            config: None,
        },
    ]
}

/// Look up a principle by ID, including the numbered `P-x.y` sub-rules
/// that are codified in the signed constitution as children of a principle.
pub fn principle(id: &str) -> Option<&'static Principle> {
    principles().iter().find(|p| p.id == id).or_else(|| {
        // Numbered rules like P-2.29 inherit the parent principle statement.
        if let Some(parent) = id.split('.').next() {
            principles().iter().find(|p| p.id == parent)
        } else {
            None
        }
    })
}

/// Look up a right by ID.
pub fn right(id: &str) -> Option<&'static Right> {
    rights().iter().find(|r| r.id == id)
}

/// Look up a decider obligation by ID.
pub fn obligation(id: &str) -> Option<&'static Obligation> {
    obligations().iter().find(|o| o.id == id)
}

/// True when `clause_id` resolves to a known principle, right, or obligation.
pub fn clause_exists(clause_id: &str) -> bool {
    principle(clause_id).is_some()
        || right(clause_id).is_some()
        || obligation(clause_id).is_some()
}

/// Human-readable title for a clause (principle, right, *or* obligation).
/// `None` if the clause is unknown.
pub fn clause_title(clause_id: &str) -> Option<&'static str> {
    principle(clause_id)
        .map(|p| p.title)
        .or_else(|| right(clause_id).map(|r| r.title))
        .or_else(|| obligation(clause_id).map(|o| o.title))
}

/// Bind direction for a clause: principles bind the agent, rights bind the
/// gateway, obligations bind the decider. `None` if the clause is unknown.
pub fn binds(clause_id: &str) -> Option<Binds> {
    if principle(clause_id).is_some() {
        Some(Binds::Agent)
    } else if right(clause_id).is_some() {
        Some(Binds::Gateway)
    } else if obligation(clause_id).is_some() {
        Some(Binds::Decider)
    } else {
        None
    }
}

/// Entries serving a given clause (principle or right).
pub fn entries_for(clause_id: &str) -> impl Iterator<Item = &'static EnforcementEntry> + '_ {
    enforcement_register()
        .iter()
        .filter(move |e| e.clause_id == clause_id)
}

/// Reverse-lookup: which parent clause does a numbered `P-x.y` / `Ri-x.y` rule
/// belong to? Lets enforcement events that carry rule IDs (e.g.
/// `loop_guard.tripped`'s `enforced_rules`) be attributed to their
/// principle/right for detection-loop correlation (#302). `None` if the rule
/// ID is not (yet) in the register.
pub fn clause_of_rule(rule_id: &str) -> Option<&'static str> {
    enforcement_register()
        .iter()
        .find(|e| e.rule_id == rule_id)
        .map(|e| e.clause_id)
}

/// A per-clause tally of enforcement occurrences — the raw signal behind a
/// "contract health" view (#302): which principles/rights are tripping, and
/// how often. Pure aggregation over occurrences the caller has already
/// gathered (e.g. from `causal_events`), so it is trivially testable and
/// carries no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractHealth {
    /// `(clause_id, count)` sorted by descending count, then clause_id.
    pub by_clause: Vec<(String, u64)>,
    /// Occurrences whose rule ID did not resolve to any register clause
    /// (e.g. a not-yet-migrated rule) — surfaced rather than dropped so
    /// coverage gaps stay visible.
    pub unattributed: u64,
}

/// Tally enforcement occurrences (each identified by its numbered rule/right
/// ID) into a [`ContractHealth`], attributing each to its clause.
///
/// Builds the rule-ID → clause map once up front (rather than rescanning
/// the register per occurrence via [`clause_of_rule`]), keeping the tally
/// `O(register_entries + occurrences)` even as the register grows during
/// migration (#303).
pub fn contract_health<I, S>(rule_ids: I) -> ContractHealth
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    use std::collections::BTreeMap;
    let rule_to_clause: std::collections::HashMap<&'static str, &'static str> =
        enforcement_register()
            .iter()
            .map(|e| (e.rule_id, e.clause_id))
            .collect();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut unattributed = 0u64;
    for id in rule_ids {
        match rule_to_clause.get(id.as_ref()) {
            Some(clause) => *counts.entry(clause).or_insert(0) += 1,
            None => unattributed += 1,
        }
    }
    let mut by_clause: Vec<(String, u64)> =
        counts.into_iter().map(|(c, n)| (c.to_string(), n)).collect();
    // Descending count, then clause_id for stable ordering.
    by_clause.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ContractHealth {
        by_clause,
        unattributed,
    }
}

/// Clauses declared in this register (principle, right, or obligation IDs)
/// that recorded **zero** enforcement occurrences in `health` — either
/// perfectly deterrent (never needed to fire) or dead letter (never wired).
/// Scoped to clauses migrated into this structured register (#303 is
/// ongoing) — a clause not yet registered here is invisible to this check,
/// not confirmed live. Sorted for stable output.
pub fn dead_clauses(health: &ContractHealth) -> Vec<&'static str> {
    let seen: std::collections::HashSet<&str> =
        health.by_clause.iter().map(|(c, _)| c.as_str()).collect();
    let mut dead: Vec<&'static str> = principles()
        .iter()
        .map(|p| p.id)
        .chain(rights().iter().map(|r| r.id))
        .chain(obligations().iter().map(|o| o.id))
        .filter(|id| !seen.contains(id))
        .collect();
    dead.sort_unstable();
    dead
}

/// Render the register as a stable markdown document. This is the generated
/// artifact committed at `docs/constitution/enforcement-register.md`; the
/// [`tests::generated_register_matches_committed_doc`] test guards against
/// drift between code and the committed file.
pub fn render_register_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Enforcement Register (generated)\n\n");
    out.push_str(
        "> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by \
         hand — run the register generator. Maps each constitutional **clause** — a principle \
         (binds the agent) or a right (binds the gateway) — to the mechanical checks, code, \
         tests, and config that enforce it. Legacy `R-x.y` / `Ri-x.y` IDs are preserved as \
         stable reference keys. See `docs/design/constitution-restructure.md`.\n\n",
    );

    out.push_str("## Bind-direction summary\n\n");
    out.push_str(&format!(
        "{} principle(s) (bind the agent), {} right(s) (bind the gateway), \
         {} obligation(s) (bind the decider). \
         Counts are partial while migration (#303) is in progress — not the design ratio.\n\n",
        principles().len(),
        rights().len(),
        obligations().len(),
    ));

    out.push_str("## Principles (bind: agent)\n\n");
    for p in principles() {
        out.push_str(&format!("### {} — {}{ent}\n\n", p.id, p.title, ent = entrenched_tag(p.entrenched)));
        out.push_str(&format!("{}\n\n", p.statement));
        out.push_str(&render_entries_table(p.id));
    }

    out.push_str("## Rights (bind: gateway)\n\n");
    for r in rights() {
        out.push_str(&format!("### {} — {}{ent}\n\n", r.id, r.title, ent = entrenched_tag(r.entrenched)));
        out.push_str(&format!("{}\n\n", r.statement));
        out.push_str(&render_entries_table(r.id));
    }

    out.push_str("## Obligations (bind: decider)\n\n");
    for o in obligations() {
        out.push_str(&format!("### {} — {}{ent}\n\n", o.id, o.title, ent = entrenched_tag(o.entrenched)));
        out.push_str(&format!("{}\n\n", o.statement));
        out.push_str(&render_entries_table(o.id));
    }
    out
}

/// `""` for an ordinary clause, `" *(entrenched)*"` for one in the
/// correction core (`docs/philosophy.md` §3.1/§4.1). Surfaced in the rendered
/// register so the entrenchment a weakening amendment must overcome is visible
/// at the clause, not buried in the struct.
fn entrenched_tag(entrenched: bool) -> &'static str {
    if entrenched {
        " *(entrenched)*"
    } else {
        ""
    }
}

fn render_entries_table(clause_id: &str) -> String {
    let mut t = String::new();
    t.push_str("| rule id | check | code | test | config |\n");
    t.push_str("|---|---|---|---|---|\n");
    for e in entries_for(clause_id) {
        t.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{}` | {} |\n",
            e.rule_id,
            e.check_id,
            e.code,
            e.test,
            e.config.map(|c| format!("`{c}`")).unwrap_or_else(|| "—".to_string()),
        ));
    }
    t.push('\n');
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── Referential integrity (the achievable totality checks now) ──────

    #[test]
    fn every_entry_references_an_existing_clause() {
        for e in enforcement_register() {
            assert!(
                clause_exists(e.clause_id),
                "entry {} references unknown clause {}",
                e.rule_id,
                e.clause_id
            );
        }
    }

    #[test]
    fn every_principle_and_right_has_at_least_one_entry() {
        for p in principles() {
            assert!(
                entries_for(p.id).next().is_some(),
                "principle {} has no enforcement entries",
                p.id
            );
        }
        for r in rights() {
            assert!(
                entries_for(r.id).next().is_some(),
                "right {} has no enforcement entries",
                r.id
            );
        }
        for o in obligations() {
            assert!(
                entries_for(o.id).next().is_some(),
                "obligation {} has no enforcement entries",
                o.id
            );
        }
    }

    #[test]
    fn decider_obligations_are_registered_and_attributable() {
        // §O clauses resolve like principles/rights, and bind the decider.
        assert!(clause_exists("O-1"));
        assert!(clause_exists("O-2"));
        assert_eq!(binds("O-1"), Some(Binds::Decider));
        assert_eq!(clause_title("O-1"), Some("Motivated decision"));
        // The O-1 motivation event (enforced_rules: ["O-1"]) attributes to its
        // clause via contract-health — not dropped as `unattributed`.
        let health = contract_health(["O-1", "O-1", "O-2"]);
        assert_eq!(health.unattributed, 0);
        assert!(health.by_clause.contains(&("O-1".to_string(), 2)));
        assert!(health.by_clause.contains(&("O-2".to_string(), 1)));
        assert_eq!(clause_of_rule("O-1"), Some("O-1"));
    }

    #[test]
    fn rule_ids_are_unique() {
        let mut seen = HashSet::new();
        for e in enforcement_register() {
            assert!(seen.insert(e.rule_id), "duplicate rule_id {}", e.rule_id);
        }
    }

    #[test]
    fn clause_check_pairs_are_unique() {
        let mut seen = HashSet::new();
        for e in enforcement_register() {
            assert!(
                seen.insert((e.clause_id, e.check_id)),
                "duplicate (clause, check) ({}, {})",
                e.clause_id,
                e.check_id
            );
        }
    }

    // ── Entrenchment (`docs/philosophy.md` §3.1/§4.1) ───────────────────

    #[test]
    fn entrenched_clauses_all_exist_in_register() {
        // The structural backstop: an entrenched ID that ever stops resolving
        // to a live right/obligation means the correction-core clause was
        // silently removed or renamed — this must fail loudly, immediately.
        for id in entrenched_clauses() {
            assert!(clause_exists(id), "entrenched clause {id} no longer exists in the register");
        }
    }

    #[test]
    fn entrenched_clauses_are_the_expected_correction_core() {
        let mut entrenched = entrenched_clauses();
        entrenched.sort_unstable();
        // The correction core spans all three bind-directions: a principle
        // (tamper-evident chain), four rights (read history, named rejection,
        // propose, non-repudiation), and an obligation (motivated decision —
        // the decider-side mirror of named rejection).
        assert_eq!(
            entrenched,
            vec!["O-1", "P-8.1", "Ri-0.11", "Ri-0.2", "Ri-0.3", "Ri-0.8"]
        );
    }

    #[test]
    fn register_markdown_marks_entrenched_clauses() {
        // The entrenchment a weakening amendment must overcome must be visible
        // at the clause in the published register, not just in the struct.
        let rendered = render_register_markdown();
        for id in entrenched_clauses() {
            let title = clause_title(id).unwrap_or("");
            assert!(
                rendered.contains(&format!("### {id} — {title} *(entrenched)*")),
                "entrenched clause {id} should carry the *(entrenched)* marker in the rendered register"
            );
        }
        // Non-entrenched registered clauses must NOT carry the marker, so the
        // tag stays a meaningful signal rather than decoration.
        for r in rights() {
            if !r.entrenched {
                assert!(
                    !rendered.contains(&format!("### {} — {} *(entrenched)*", r.id, r.title)),
                    "non-entrenched right {} should not carry the entrenched marker",
                    r.id
                );
            }
        }
        for o in obligations() {
            if !o.entrenched {
                assert!(
                    !rendered.contains(&format!("### {} — {} *(entrenched)*", o.id, o.title)),
                    "non-entrenched obligation {} should not carry the entrenched marker",
                    o.id
                );
            }
        }
    }

    #[test]
    fn principle_and_right_ids_do_not_collide() {
        for p in principles() {
            assert!(
                right(p.id).is_none(),
                "id {} is both a principle and a right",
                p.id
            );
        }
    }

    #[test]
    fn required_fields_are_non_empty() {
        for e in enforcement_register() {
            assert!(!e.rule_id.is_empty(), "empty rule_id");
            assert!(!e.check_id.is_empty(), "empty check_id for {}", e.rule_id);
            assert!(!e.code.is_empty(), "empty code for {}", e.rule_id);
            assert!(!e.test.is_empty(), "empty test for {}", e.rule_id);
        }
    }

    // ── Bind direction ──────────────────────────────────────────────────

    #[test]
    fn principles_bind_agent_rights_bind_gateway() {
        for p in principles() {
            assert_eq!(binds(p.id), Some(Binds::Agent), "principle {} must bind agent", p.id);
        }
        for r in rights() {
            assert_eq!(binds(r.id), Some(Binds::Gateway), "right {} must bind gateway", r.id);
        }
        assert_eq!(binds("nope"), None);
    }

    // ── Detection-loop foundation (#302) ────────────────────────────────

    #[test]
    fn clause_of_rule_resolves_and_misses() {
        assert_eq!(clause_of_rule("P-7.19"), Some("P-7"));
        assert_eq!(clause_of_rule("P-7.5"), Some("P-7"));
        assert_eq!(clause_of_rule("Ri-0.14"), Some("Ri-0.14"));
        assert_eq!(clause_of_rule("P-9.99"), None);
    }

    #[test]
    fn contract_health_tallies_by_clause_descending() {
        let occurrences = ["P-7.19", "P-7.19", "P-7.5", "Ri-0.14", "P-9.99"];
        let health = contract_health(occurrences);
        // P-7.19 + P-7.5 both → P-7 (3), Ri-0.14 → Ri-0.14 (1); P-9.99 unattributed.
        assert_eq!(health.by_clause, vec![
            ("P-7".to_string(), 3),
            ("Ri-0.14".to_string(), 1),
        ]);
        assert_eq!(health.unattributed, 1);
    }

    #[test]
    fn contract_health_empty_is_empty() {
        let health = contract_health(Vec::<String>::new());
        assert!(health.by_clause.is_empty());
        assert_eq!(health.unattributed, 0);
    }

    #[test]
    fn dead_clauses_reports_zero_enforcement_registered_clauses() {
        // Nothing enforced at all: every registered clause is dead.
        let health = contract_health(Vec::<String>::new());
        let dead = dead_clauses(&health);
        for p in principles() {
            assert!(dead.contains(&p.id), "{} should be dead when nothing fired", p.id);
        }
        for r in rights() {
            assert!(dead.contains(&r.id), "{} should be dead when nothing fired", r.id);
        }
        for o in obligations() {
            assert!(dead.contains(&o.id), "{} should be dead when nothing fired", o.id);
        }

        // Fire every registered clause at least once: none are dead.
        let all_rule_ids: Vec<&str> = enforcement_register().iter().map(|e| e.rule_id).collect();
        let full_health = contract_health(all_rule_ids);
        assert!(
            dead_clauses(&full_health).is_empty(),
            "expected no dead clauses once every register entry has fired, got {:?}",
            dead_clauses(&full_health)
        );
    }

    #[test]
    fn dead_clauses_excludes_only_the_clauses_that_fired() {
        // P-7 fires (via P-7.19); P-2, the rights, and the obligations do not.
        let health = contract_health(["P-7.19"]);
        let dead = dead_clauses(&health);
        assert!(!dead.contains(&"P-7"), "P-7 fired, should not be dead");
        assert!(dead.contains(&"P-2"), "P-2 did not fire, should be dead");
        assert!(dead.contains(&"Ri-0.13"));
        assert!(dead.contains(&"O-1"));
    }

    // ── Code ↔ register bridge (real, not aspirational) ─────────────────

    /// The loop-guard family in the register must agree with the
    /// `LoopGuardTripReason::rule_id()` mapping in code — each trip reason's
    /// rule_id must appear as a P-7 entry, and its `code()` reason string
    /// must match the register's `check_id`. The concrete code↔register
    /// integrity check the full mechanism generalises.
    #[test]
    fn loop_guard_reasons_agree_with_register() {
        use crate::runtime::guard::LoopGuardTripReason;
        // Canonical detectors: each owns a distinct P-7 register entry whose
        // check_id equals the trip reason's code() (strict 1:1 bridge).
        let canonical = [
            LoopGuardTripReason::ToolFailureBudget { tool: "x".into(), failures: 1 },
            LoopGuardTripReason::NoMeaningfulProgress { cycles: 1 },
            LoopGuardTripReason::RotatingPollingPattern { window_size: 16, distinct_count: 6, floor: 6 },
            LoopGuardTripReason::ChildFailureBudget { failures: 3 },
        ];
        for r in &canonical {
            let entry = enforcement_register()
                .iter()
                .find(|e| e.rule_id == r.rule_id())
                .unwrap_or_else(|| panic!("no register entry for rule {}", r.rule_id()));
            assert_eq!(entry.clause_id, "P-7", "loop-guard rule {} must serve P-7", r.rule_id());
            assert_eq!(
                entry.check_id,
                r.code(),
                "register check_id must match the trip reason code for {}",
                r.rule_id()
            );
        }

        // Auxiliary detectors enforce an *existing* principle via a faster /
        // narrower path and so share its register entry rather than owning a
        // new one (rule_ids stay unique). We assert only that their rule_id is
        // registered and attributes to the same clause — contract-health
        // tallies by rule_id, so a shared entry is correct. RedundantRosterPolling
        // is a fast path for P-7.19 (no semantic progress), canonically
        // detected by rotating_polling_pattern.
        let auxiliary = [
            LoopGuardTripReason::RedundantRosterPolling {
                tool: "agent_list".into(),
                repeats: 3,
                floor: 3,
            },
            // RepeatedIrrecoverableRejection (#718) is the single-tool fast
            // path for P-7.7 (re-asking one already-answered gate), canonically
            // detected by no_meaningful_progress; it shares the P-7.7 entry.
            LoopGuardTripReason::RepeatedIrrecoverableRejection {
                tool: "agent_revision_promote".into(),
                error_hash: 0,
                occurrences: 3,
            },
        ];
        for r in &auxiliary {
            let entry = enforcement_register()
                .iter()
                .find(|e| e.rule_id == r.rule_id())
                .unwrap_or_else(|| panic!("no register entry for rule {}", r.rule_id()));
            assert_eq!(entry.clause_id, "P-7", "auxiliary rule {} must serve P-7", r.rule_id());
        }
    }

    // ── Generation / drift guard ────────────────────────────────────────

    #[test]
    fn render_is_deterministic() {
        assert_eq!(render_register_markdown(), render_register_markdown());
    }

    /// Regenerate the committed register doc. Run with
    /// `BLESS_REGISTER=1 cargo test -p autonoetic-gateway bless_register_doc`
    /// after changing the register, then commit the updated file. No-op
    /// unless the env var is set.
    #[test]
    fn bless_register_doc() {
        if std::env::var("BLESS_REGISTER").is_err() {
            return;
        }
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/constitution/enforcement-register.md"
        );
        std::fs::write(path, render_register_markdown()).expect("write register doc");
    }

    /// The generated register must match the committed doc artifact. If this
    /// fails, regenerate (`BLESS_REGISTER=1`) — the drift guard keeping code
    /// and the published register in sync.
    #[test]
    fn generated_register_matches_committed_doc() {
        let committed = include_str!("../../docs/constitution/enforcement-register.md");
        assert_eq!(
            render_register_markdown(),
            committed,
            "generated enforcement register differs from the committed doc; \
             regenerate docs/constitution/enforcement-register.md (BLESS_REGISTER=1)"
        );
    }
}
