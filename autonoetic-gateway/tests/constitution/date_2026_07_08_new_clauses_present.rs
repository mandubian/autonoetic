//! Text-presence pin for the constitution amendment at
//! `docs/constitution/versions/2026.07.08/`, which is now the active, signed
//! constitution (`docs/constitution/CURRENT` points at `2026.07.08`).
//!
//! For declarative clauses with no enforcement code of their own
//! (§12's `U-*` rights, `I-12`), a text-presence assertion is the closest
//! approximation of "a right without a test is a lie" available: if a future
//! edit silently drops one of these rows, this test fails immediately rather
//! than the drop going unnoticed until someone reads the whole document.
//!
//! `Ri-0.17` and `O-6` additionally have real enforcement/registry coverage
//! — see `autonoetic-types/src/principal.rs`,
//! `autonoetic-gateway/src/enforcement_register.rs`, and
//! `autonoetic-gateway/src/router.rs`'s
//! `test_dispatch_constitution_resolve_proposal*` tests.

const DRAFT_CONSTITUTION: &str = include_str!("../../../docs/constitution/versions/2026.07.08/constitution.md");

#[test]
fn section_12_rights_of_the_served_is_present() {
    assert!(
        DRAFT_CONSTITUTION.contains("## 12. Rights of the Served"),
        "expected the new §12 section header"
    );
    assert!(DRAFT_CONSTITUTION.contains("PrincipalKind::ServedUser"));
}

#[test]
fn u_rights_are_present_and_declared_missing() {
    for id in ["U-1", "U-2", "U-3"] {
        let row_marker = format!("| {id} |");
        assert!(
            DRAFT_CONSTITUTION.contains(&row_marker),
            "expected a {id} row in §12"
        );
    }
    // All three are honestly undeclared as enforced — see the module doc.
    let section_start = DRAFT_CONSTITUTION
        .find("## 12. Rights of the Served")
        .expect("section must exist");
    let section_end = DRAFT_CONSTITUTION[section_start..]
        .find("## O. Decider Obligations")
        .map(|i| section_start + i)
        .expect("§O must follow §12");
    let section = &DRAFT_CONSTITUTION[section_start..section_end];
    for id in ["U-1", "U-2", "U-3"] {
        let row = section
            .lines()
            .find(|l| l.starts_with(&format!("| {id} |")))
            .unwrap_or_else(|| panic!("missing row for {id}"));
        assert!(
            row.trim_end().ends_with("| MISSING |"),
            "{id} should be honestly declared MISSING (not yet enforced), got: {row}"
        );
    }
}

#[test]
fn ri_0_17_emigration_right_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("| Ri-0.17 |"));
    assert!(DRAFT_CONSTITUTION.contains("cognitive capsule"));
    assert!(DRAFT_CONSTITUTION.contains("CapsuleExportTool"));
}

#[test]
fn o_6_proposal_adjudication_duty_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("| O-6 |"));
    assert!(DRAFT_CONSTITUTION.contains("constitution.resolve_proposal"));
    // Must not collide with the RFC #359-reserved O-3/O-4/O-5 block.
    assert!(!DRAFT_CONSTITUTION.contains("| O-3 |"));
    assert!(!DRAFT_CONSTITUTION.contains("| O-4 |"));
    assert!(!DRAFT_CONSTITUTION.contains("| O-5 |"));
}

#[test]
fn i_12_sybil_collapse_invariant_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("**I-12**"));
    assert!(DRAFT_CONSTITUTION.contains("spawn-descendants"));
}

#[test]
fn entrenched_clauses_paragraph_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("**Entrenched clauses.**"));
    for id in ["Ri-0.2", "Ri-0.3", "Ri-0.8", "Ri-0.11", "O-1"] {
        assert!(
            DRAFT_CONSTITUTION.contains(&format!("`{id}`")),
            "entrenched-clauses paragraph should name {id}"
        );
    }
}

#[test]
fn active_constitution_is_the_signed_amendment() {
    // The signed, running version must point at the activated amendment.
    // Repointed from 2026.07.08 → 2026.07.19 (anomaly/adjudication + genesis
    // batch) → 2026.07.30 (data-egress label plane, #910) → 2026.08.30
    // (text-then-law realignment, #1078) → 2026.09.02 (invariant enforcement
    // citations, #1281) → 2026.09.04 (the relational amendment, #1284);
    // each successor is a strict
    // superset of its predecessor's pinned clauses (§12, Ri-0.17, O-6,
    // I-12, the entrenched paragraph), so this remains a meaningful
    // active-version pin.
    let current = include_str!("../../../docs/constitution/CURRENT").trim();
    assert_eq!(
        current, "2026.09.04",
        "docs/constitution/CURRENT must point at the signed 2026.09.04 amendment"
    );
    assert_eq!(
        autonoetic_types::config::ACTIVE_CONSTITUTION_VERSION,
        "2026.09.04",
        "ACTIVE_CONSTITUTION_VERSION must match the activated amendment"
    );
    let active: &str = include_str!("../../../docs/constitution/versions/2026.09.04/constitution.md");
    assert!(
        active.contains("Rights of the Served"),
        "the active, signed constitution must carry the §12 text from 2026.07.08"
    );
}
