//! Enforcement Register — the generated bridge between constitutional
//! **principles** and the concrete code that enforces them (issue #298,
//! epic #297; design `docs/design/constitution-restructure.md`).
//!
//! The constitution is being restructured so the *signed* artifact holds a
//! small set of **principles** (the deliberated "why"), while the large,
//! churning "how" lives here as a register mapping each principle to its
//! mechanical checks, code citations, tests, and config knobs. Splitting
//! the two keeps the signed law legible and lets enforcement detail scale
//! with the code.
//!
//! This module is the **mechanism slice**: it establishes the data model,
//! a markdown generator, and referential-integrity + code↔register
//! meta-tests, proven end-to-end on **one family** — the loop guard
//! (principle `P-7`, "Bounded progress"). The remaining ~160 rules migrate
//! behind this mechanism in #303.
//!
//! Deliberately **not yet** in this slice (tracked in #298):
//! - a `#[enforces(...)]` proc-macro to derive entries from code annotations
//!   (entries are hand-authored here for now);
//! - runtime verification that each `test` actually exists/passes;
//! - signing the register digest into the constitution lock.
//!
//! Stable external references: each entry keeps its legacy `R-x.y` rule ID
//! as `legacy_rule_id`, so existing docs/tests that cite `R-7.19` keep
//! resolving after the flat rule table is replaced (#303).

/// A constitutional principle — an invariant, not a mechanism. The signed
/// constitution will carry these (`P-*`); the enforcement detail lives in
/// [`enforcement_register`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Principle {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
}

/// A single enforcement point: which principle it serves, the legacy rule
/// ID it preserves, the mechanical check, the code that implements it, the
/// test that pins it, and the config knob(s) that tune it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcementEntry {
    pub principle_id: &'static str,
    /// Pre-restructure rule ID, kept as a stable external reference key.
    pub legacy_rule_id: &'static str,
    /// Stable machine identifier for the check (matches the `reason` code
    /// emitted on causal events where applicable).
    pub check_id: &'static str,
    pub code: &'static str,
    pub test: &'static str,
    pub config: Option<&'static str>,
}

/// Principles defined so far. Seeded with the loop-guard family as the
/// proof; grows as #303 migrates the remaining sections.
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

/// The enforcement register. Seeded with `P-7`'s four checks — the rows
/// that were rules R-7.5 / R-7.7 / R-7.19 / R-7.20.
pub fn enforcement_register() -> &'static [EnforcementEntry] {
    &[
        EnforcementEntry {
            principle_id: "P-7",
            legacy_rule_id: "R-7.5",
            check_id: "tool_failure_budget",
            code: "guard.rs::register_failure + check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_tool_failure_budget",
            config: Some("loop_guard.max_tool_failures"),
        },
        EnforcementEntry {
            principle_id: "P-7",
            legacy_rule_id: "R-7.7",
            check_id: "no_meaningful_progress",
            code: "guard.rs::check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_max_loops",
            config: Some("loop_guard.max_loops_without_progress"),
        },
        EnforcementEntry {
            principle_id: "P-7",
            legacy_rule_id: "R-7.19",
            check_id: "rotating_polling_pattern",
            code: "guard.rs::register_progress_inner (window + trip) + check_loop",
            test: "runtime::guard::tests::rotating_polling_pattern_with_five_tools_trips",
            config: Some("loop_guard.rotation_window_size, loop_guard.rotation_distinct_floor"),
        },
        EnforcementEntry {
            principle_id: "P-7",
            legacy_rule_id: "R-7.20",
            check_id: "child_failure_budget",
            code: "guard.rs::register_child_failure + check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_child_failures",
            config: Some("loop_guard.max_child_failures"),
        },
    ]
}

/// Look up a principle by ID.
pub fn principle(id: &str) -> Option<&'static Principle> {
    principles().iter().find(|p| p.id == id)
}

/// Entries serving a given principle.
pub fn entries_for(principle_id: &str) -> impl Iterator<Item = &'static EnforcementEntry> + '_ {
    enforcement_register()
        .iter()
        .filter(move |e| e.principle_id == principle_id)
}

/// Render the register as a stable markdown document, grouped by principle.
/// This is the generated artifact committed at
/// `docs/constitution/enforcement-register.md`; the
/// [`tests::generated_register_matches_committed_doc`] test guards against
/// drift between code and the committed file.
pub fn render_register_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Enforcement Register (generated)\n\n");
    out.push_str(
        "> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by \
         hand — run the register generator. Maps each constitutional **principle** to the \
         mechanical checks, code, tests, and config that enforce it. Legacy `R-x.y` IDs are \
         preserved as stable reference keys. See `docs/design/constitution-restructure.md`.\n\n",
    );
    for p in principles() {
        out.push_str(&format!("## {} — {}\n\n", p.id, p.title));
        out.push_str(&format!("{}\n\n", p.statement));
        out.push_str("| legacy id | check | code | test | config |\n");
        out.push_str("|---|---|---|---|---|\n");
        for e in entries_for(p.id) {
            out.push_str(&format!(
                "| `{}` | `{}` | `{}` | `{}` | {} |\n",
                e.legacy_rule_id,
                e.check_id,
                e.code,
                e.test,
                e.config.map(|c| format!("`{c}`")).unwrap_or_else(|| "—".to_string()),
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── Referential integrity (the achievable totality checks now) ──────

    #[test]
    fn every_entry_references_an_existing_principle() {
        for e in enforcement_register() {
            assert!(
                principle(e.principle_id).is_some(),
                "entry {} references unknown principle {}",
                e.legacy_rule_id,
                e.principle_id
            );
        }
    }

    #[test]
    fn every_principle_has_at_least_one_entry() {
        for p in principles() {
            assert!(
                entries_for(p.id).next().is_some(),
                "principle {} has no enforcement entries",
                p.id
            );
        }
    }

    #[test]
    fn legacy_rule_ids_are_unique() {
        let mut seen = HashSet::new();
        for e in enforcement_register() {
            assert!(
                seen.insert(e.legacy_rule_id),
                "duplicate legacy_rule_id {}",
                e.legacy_rule_id
            );
        }
    }

    #[test]
    fn principle_check_pairs_are_unique() {
        let mut seen = HashSet::new();
        for e in enforcement_register() {
            assert!(
                seen.insert((e.principle_id, e.check_id)),
                "duplicate (principle, check) ({}, {})",
                e.principle_id,
                e.check_id
            );
        }
    }

    #[test]
    fn required_fields_are_non_empty() {
        for e in enforcement_register() {
            assert!(!e.legacy_rule_id.is_empty(), "empty legacy_rule_id");
            assert!(!e.check_id.is_empty(), "empty check_id for {}", e.legacy_rule_id);
            assert!(!e.code.is_empty(), "empty code for {}", e.legacy_rule_id);
            assert!(!e.test.is_empty(), "empty test for {}", e.legacy_rule_id);
        }
    }

    // ── Code ↔ register bridge (real, not aspirational) ─────────────────

    /// The loop-guard family in the register must agree with the
    /// `LoopGuardTripReason::rule_id()` mapping in code — each trip
    /// reason's rule_id must appear as a P-7 legacy_rule_id, and its
    /// `code()` reason string must match the register's `check_id`. This
    /// is the concrete code↔register integrity check the full mechanism
    /// generalises.
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
                .find(|e| e.legacy_rule_id == r.rule_id())
                .unwrap_or_else(|| panic!("no register entry for rule {}", r.rule_id()));
            assert_eq!(
                entry.principle_id, "P-7",
                "loop-guard rule {} must serve P-7",
                r.rule_id()
            );
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
    /// unless the env var is set, so normal test runs don't write files.
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

    /// The generated register must match the committed doc artifact. If
    /// this fails, regenerate the doc (the register changed) — this is the
    /// drift guard that keeps code and the published register in sync.
    #[test]
    fn generated_register_matches_committed_doc() {
        let committed = include_str!(
            "../../docs/constitution/enforcement-register.md"
        );
        assert_eq!(
            render_register_markdown(),
            committed,
            "generated enforcement register differs from the committed doc; \
             regenerate docs/constitution/enforcement-register.md"
        );
    }
}
