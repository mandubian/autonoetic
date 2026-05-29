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
}

impl Binds {
    pub fn label(self) -> &'static str {
        match self {
            Binds::Agent => "agent",
            Binds::Gateway => "gateway",
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
}

/// A constitutional right — a guarantee the gateway upholds for the agent.
/// First-class alongside principles (#299); also enforced by concrete code,
/// so it appears in the register the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Right {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
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
/// as the proof; grows as #303 migrates the remaining sections.
pub fn principles() -> &'static [Principle] {
    &[Principle {
        id: "P-7",
        title: "Bounded progress",
        statement: "A session is halted when it stops making progress, on a closed, \
                    configurable set of mechanically-detected non-progress conditions, \
                    each emitting a typed, attributable reason. No condition relies on \
                    agent self-report.",
    }]
}

/// Rights (gateway-side, bind the gateway). Seeded with two real rights as
/// the proof that rights are register-modelled identically to principles.
pub fn rights() -> &'static [Right] {
    &[
        Right {
            id: "Ri-0.13",
            title: "Reasoning privacy",
            statement: "An agent's internal reasoning is private-under-law: not used by the \
                        gateway as a basis for policy decisions, recorded to the agent's own \
                        causal chain for forensic review, and disclosed to other parties only \
                        through capability-gated audit.",
        },
        Right {
            id: "Ri-0.14",
            title: "Wake-up over polling",
            statement: "When a child task reaches a terminal state or resolves a gate, the \
                        gateway wakes the parent with typed child state. Parents are not \
                        required to poll to discover child-state transitions.",
        },
    ]
}

/// The enforcement register. P-7's four checks (P-7.5/7.7/7.19/7.20) plus one
/// check per seeded right.
pub fn enforcement_register() -> &'static [EnforcementEntry] {
    &[
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
    ]
}

/// Look up a principle by ID.
pub fn principle(id: &str) -> Option<&'static Principle> {
    principles().iter().find(|p| p.id == id)
}

/// Look up a right by ID.
pub fn right(id: &str) -> Option<&'static Right> {
    rights().iter().find(|r| r.id == id)
}

/// True when `clause_id` resolves to a known principle or right.
pub fn clause_exists(clause_id: &str) -> bool {
    principle(clause_id).is_some() || right(clause_id).is_some()
}

/// Human-readable title for a clause (principle *or* right). `None` if the
/// clause is unknown.
pub fn clause_title(clause_id: &str) -> Option<&'static str> {
    principle(clause_id)
        .map(|p| p.title)
        .or_else(|| right(clause_id).map(|r| r.title))
}

/// Bind direction for a clause: principles bind the agent, rights bind the
/// gateway. `None` if the clause is unknown.
pub fn binds(clause_id: &str) -> Option<Binds> {
    if principle(clause_id).is_some() {
        Some(Binds::Agent)
    } else if right(clause_id).is_some() {
        Some(Binds::Gateway)
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
        "{} principle(s) (bind the agent), {} right(s) (bind the gateway). \
         Counts are partial while migration (#303) is in progress — not the design ratio.\n\n",
        principles().len(),
        rights().len(),
    ));

    out.push_str("## Principles (bind: agent)\n\n");
    for p in principles() {
        out.push_str(&format!("### {} — {}\n\n", p.id, p.title));
        out.push_str(&format!("{}\n\n", p.statement));
        out.push_str(&render_entries_table(p.id));
    }

    out.push_str("## Rights (bind: gateway)\n\n");
    for r in rights() {
        out.push_str(&format!("### {} — {}\n\n", r.id, r.title));
        out.push_str(&format!("{}\n\n", r.statement));
        out.push_str(&render_entries_table(r.id));
    }
    out
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

    // ── Code ↔ register bridge (real, not aspirational) ─────────────────

    /// The loop-guard family in the register must agree with the
    /// `LoopGuardTripReason::rule_id()` mapping in code — each trip reason's
    /// rule_id must appear as a P-7 entry, and its `code()` reason string
    /// must match the register's `check_id`. The concrete code↔register
    /// integrity check the full mechanism generalises.
    #[test]
    fn loop_guard_reasons_agree_with_register() {
        use crate::runtime::guard::LoopGuardTripReason;
        let reasons = [
            LoopGuardTripReason::ToolFailureBudget { tool: "x".into(), failures: 1 },
            LoopGuardTripReason::NoMeaningfulProgress { cycles: 1 },
            LoopGuardTripReason::RotatingPollingPattern { window_size: 16, distinct_count: 6, floor: 6 },
            LoopGuardTripReason::ChildFailureBudget { failures: 3 },
        ];
        for r in &reasons {
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
