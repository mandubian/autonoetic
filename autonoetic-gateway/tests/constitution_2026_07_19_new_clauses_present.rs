//! Text-presence pin for the constitution amendment at
//! `docs/constitution/versions/2026.07.19/` — the active, signed constitution
//! (`docs/constitution/CURRENT` points at `2026.07.19`).
//!
//! For declarative clauses with no enforcement code of their own (I-13), a
//! text-presence assertion is the closest approximation of "a right without
//! a test is a lie" available: if a future edit silently drops one of these
//! rows, this test fails immediately rather than the drop going unnoticed
//! until someone reads the whole document.
//!
//! Ri-0.18, O-6, O-7, P-9.15, and P-9.16 additionally have real register
//! coverage — see `autonoetic-gateway/src/enforcement_register.rs`.

const DRAFT_CONSTITUTION: &str = include_str!("../../docs/constitution/versions/2026.07.19/constitution.md");

#[test]
fn ri_0_18_anomaly_reporting_right_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("| Ri-0.18 |"));
    assert!(DRAFT_CONSTITUTION.contains("anomaly_flag"));
    assert!(DRAFT_CONSTITUTION.contains("without holding any capability"));
    // The no-sanction sentence is the clause's load-bearing commitment —
    // it is what keeps honest-but-wrong flags from being punished.
    assert!(
        DRAFT_CONSTITUTION
            .contains("Filing a flag is never, by itself, grounds for sanction"),
        "Ri-0.18 must keep the no-sanction sentence verbatim"
    );
}

#[test]
fn o_6_is_upgraded_to_enforced_with_sla() {
    // Find the §O table region so we assert on O-6's row specifically,
    // not on a stray prose mention elsewhere.
    let section_start = DRAFT_CONSTITUTION
        .find("## O. Decider Obligations")
        .expect("§O section must exist");
    let section_end = DRAFT_CONSTITUTION[section_start..]
        .find("\n---\n")
        .map(|i| section_start + i)
        .unwrap_or_else(|| DRAFT_CONSTITUTION.len());
    let o_section = &DRAFT_CONSTITUTION[section_start..section_end];

    let o6_row = o_section
        .lines()
        .find(|l| l.starts_with("| O-6 |"))
        .expect("O-6 row must exist in §O");
    assert!(
        o6_row.contains("bounded adjudication window"),
        "O-6 must carry the SLA duty (bounded window); got: {o6_row}"
    );
    assert!(
        o6_row.trim_end().ends_with("| ENFORCED |"),
        "O-6 status must be ENFORCED (upgraded from PARTIAL); got: {o6_row}"
    );
    assert!(
        !o6_row.contains("no timeliness/SLA enforcement yet"),
        "the stale 'no SLA yet' caveat from 2026.07.08 PARTIAL must be gone"
    );
}

#[test]
fn o_7_anomaly_adjudication_duty_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("| O-7 |"));
    assert!(DRAFT_CONSTITUTION.contains("anomaly.resolve"));
    assert!(DRAFT_CONSTITUTION.contains("anomaly_adjudicate"));
    assert!(DRAFT_CONSTITUTION.contains("decide_anomaly_flag"));
    assert!(DRAFT_CONSTITUTION.contains("bounded adjudication window"));
    // Must not collide with the RFC #359-reserved O-3/O-4/O-5 block.
    assert!(!DRAFT_CONSTITUTION.contains("| O-3 |"));
    assert!(!DRAFT_CONSTITUTION.contains("| O-4 |"));
    assert!(!DRAFT_CONSTITUTION.contains("| O-5 |"));
}

#[test]
fn p_9_15_single_door_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("| P-9.15 |"));
    assert!(DRAFT_CONSTITUTION.contains("Single door"));
    assert!(DRAFT_CONSTITUTION.contains("skill_install"));
    assert!(DRAFT_CONSTITUTION.contains("Candidate"));
    assert!(DRAFT_CONSTITUTION.contains("auto_promote"));
}

#[test]
fn p_9_16_import_provenance_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("| P-9.16 |"));
    assert!(DRAFT_CONSTITUTION.contains("source_kind"));
    assert!(DRAFT_CONSTITUTION.contains("source_ref"));
    assert!(DRAFT_CONSTITUTION.contains("skill_imported"));
}

#[test]
fn i_13_creation_is_not_delegation_is_present() {
    assert!(DRAFT_CONSTITUTION.contains("**I-13"));
    assert!(DRAFT_CONSTITUTION.contains("Creation is not delegation"));
    assert!(DRAFT_CONSTITUTION.contains("promotion gate"));
}

#[test]
fn rights_rules_ratio_prose_was_updated() {
    // Ri-0.18 added → 18 rights; P-9.15/P-9.16 added → 179 rules.
    // The §0 bind-direction paragraph cites the rights count as a design
    // signal; it must not be left stale after the amendment. The prose wraps
    // across two lines in the source, so we check the rights count token
    // rather than the full sentence.
    assert!(
        DRAFT_CONSTITUTION.contains("18 rights against"),
        "the rights/rules ratio prose should read 18 rights after the amendment"
    );
    assert!(
        !DRAFT_CONSTITUTION.contains("17 rights against"),
        "the pre-amendment (17 rights) ratio prose must be updated"
    );
}
