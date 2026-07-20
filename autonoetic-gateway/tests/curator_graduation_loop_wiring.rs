//! B.2 lesson-graduation loop wiring (#773 Part F) — curator proposes,
//! orchestrator routes, steward judges, factory enacts.
//!
//! The curator's `promote_to_skill` decisions used to terminate at a
//! `curator.decision` causal event with no consumer: the design doc claimed
//! "the evolution-steward reads promote_to_skill decisions" but nothing
//! ever did. This test pins the now-wired loop across the three agent
//! manifests — it parses the REAL SKILL.md files (not synthetic fixtures)
//! so a future edit that severs the loop (drops the routing step, drops
//! the structured fields, drops the steward's graduation path) fails here
//! instead of silently returning the loop to vapor.
//!
//! What this does NOT cover: actual LLM-driven execution of the pipeline
//! (that requires a stub-LLM multi-agent harness). This is the
//! declarative contract all three offices are expected to honor.

use autonoetic_gateway::runtime::parser::SkillParser;

const CURATOR_SKILL_MD: &str =
    include_str!("../../agents/evolution/memory-curator.default/SKILL.md");
const ORCHESTRATOR_SKILL_MD: &str =
    include_str!("../../agents/evolution/evolution-orchestrator.default/SKILL.md");
const STEWARD_SKILL_MD: &str =
    include_str!("../../agents/evolution/evolution-steward.default/SKILL.md");

#[test]
fn all_three_manifests_parse() {
    for (name, md) in [
        ("memory-curator", CURATOR_SKILL_MD),
        ("evolution-orchestrator", ORCHESTRATOR_SKILL_MD),
        ("evolution-steward", STEWARD_SKILL_MD),
    ] {
        SkillParser::parse(md).unwrap_or_else(|e| panic!("{name} SKILL.md must parse: {e}"));
    }
}

#[test]
fn curator_emit_structured_graduation_fields() {
    // The decision_journal schema must carry the routing fields the
    // orchestrator reads mechanically — not prose buried in reason_detail.
    assert!(
        CURATOR_SKILL_MD.contains("\"target_agent\""),
        "curator graduation decisions must carry target_agent"
    );
    assert!(
        CURATOR_SKILL_MD.contains("\"proposed_instruction\""),
        "curator graduation decisions must carry proposed_instruction"
    );
    // And the frontmatter schema documents them (the consumer contract).
    assert!(
        CURATOR_SKILL_MD.contains("orchestrator routes on this field"),
        "the io.returns schema must document target_agent as the routing field"
    );
}

#[test]
fn orchestrator_routes_promote_to_skill_to_steward() {
    assert!(
        ORCHESTRATOR_SKILL_MD.contains("### Step 4b: Route lesson graduations"),
        "orchestrator must have a graduation routing step"
    );
    assert!(
        ORCHESTRATOR_SKILL_MD.contains("promote_to_skill"),
        "orchestrator must filter promote_to_skill decisions"
    );
    assert!(
        ORCHESTRATOR_SKILL_MD.contains("\"graduation\""),
        "orchestrator must spawn the steward with the graduation payload shape"
    );
    assert!(
        ORCHESTRATOR_SKILL_MD.contains("evolution-steward.default"),
        "orchestrator must route graduations to the steward"
    );
    // Dedup at the routing layer: one graduation per lesson per run.
    assert!(
        ORCHESTRATOR_SKILL_MD.contains("one graduation per lesson per run"),
        "orchestrator must dedup graduations per lesson per run"
    );
}

#[test]
fn steward_consumes_graduation_input_and_dedups() {
    assert!(
        STEWARD_SKILL_MD.contains("## Lesson Graduation (B.2 loop)"),
        "steward must have a graduation path"
    );
    // The input shape the orchestrator spawns with.
    assert!(
        STEWARD_SKILL_MD.contains("\"graduation\""),
        "steward must document the graduation spawn shape"
    );
    // Dedup via a knowledge record so the next curator run doesn't re-route.
    assert!(
        STEWARD_SKILL_MD.contains("steward.graduation.<knowledge_entry_id>"),
        "steward must dedup graduations via a knowledge record"
    );
    // The enactment goes through the one-door factory path, never a direct edit.
    assert!(
        STEWARD_SKILL_MD.contains("agent-factory.default"),
        "steward graduation must delegate to agent-factory"
    );
    assert!(
        STEWARD_SKILL_MD.contains("P-9.15"),
        "steward graduation must cite the one-door invariant (P-9.15)"
    );
}

#[test]
fn loop_fields_agree_across_all_three_manifests() {
    // The field names must match end-to-end — a rename in one manifest
    // severs the loop. Check the exact field names appear in all three.
    for field in ["target_agent", "proposed_instruction", "knowledge_entry_id"] {
        assert!(
            CURATOR_SKILL_MD.contains(field),
            "curator must carry {field}"
        );
        assert!(
            ORCHESTRATOR_SKILL_MD.contains(field),
            "orchestrator must route {field}"
        );
        assert!(
            STEWARD_SKILL_MD.contains(field),
            "steward must consume {field}"
        );
    }
}
