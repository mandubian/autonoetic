//! `nightwatch.default` holds the decider seat and nothing else (#1196).
//!
//! Deliberately **reads the bundle** rather than mirroring its capability list
//! in Rust, as the watchdog pin does. A mirrored list can drift from the file
//! it claims to pin, and for the one bundled agent whose whole design rests on
//! being minimally capable, "the test says so" is worth less than "the file
//! says so".

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate always has a workspace parent")
        .to_path_buf()
}

fn nightwatch_skill() -> String {
    let path = repo_root().join("agents/governance/nightwatch.default/SKILL.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn frontmatter() -> serde_yaml::Value {
    let raw = nightwatch_skill();
    let body = raw
        .strip_prefix("---\n")
        .expect("SKILL.md starts with YAML frontmatter");
    let end = body.find("\n---").expect("frontmatter is terminated");
    serde_yaml::from_str(&body[..end]).expect("frontmatter parses as YAML")
}

#[test]
fn the_night_watch_holds_gate_decider_for_approvals_and_nothing_else() {
    let fm = frontmatter();
    let caps = fm["metadata"]["autonoetic"]["capabilities"]
        .as_sequence()
        .expect("declares capabilities");

    assert_eq!(
        caps.len(),
        1,
        "the night watch reads a gate card and returns a verdict; every extra \
         capability is a way for the seat to do something other than decide. \
         Found: {caps:?}"
    );
    assert_eq!(caps[0]["type"].as_str(), Some("GateDecider"));
    let kinds: Vec<&str> = caps[0]["kinds"]
        .as_sequence()
        .expect("GateDecider declares kinds")
        .iter()
        .filter_map(|k| k.as_str())
        .collect();
    assert_eq!(
        kinds,
        vec!["approval"],
        "phase 1 seats it for approvals only; escalation is a separate grant"
    );
}

#[test]
fn the_night_watch_is_on_a_fixed_preset() {
    // A routing preset would pick a model per call, which `appoint` refuses —
    // but the bundle should not be shipping something that cannot be seated.
    let fm = frontmatter();
    assert_eq!(
        fm["metadata"]["autonoetic"]["llm_preset"].as_str(),
        Some("decider"),
        "the seat has its own preset so retuning another agent's model cannot \
         silently retune the judge"
    );
}

#[test]
fn the_night_watch_declares_a_verdict_output_contract() {
    let fm = frontmatter();
    let returns = &fm["metadata"]["autonoetic"]["io"]["returns"];
    let required: Vec<&str> = returns["required"]
        .as_sequence()
        .expect("declares required output fields")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        required.contains(&"verdict") && required.contains(&"reason"),
        "O-1 makes an unmotivated verdict refusable, so the contract must \
         require both: {required:?}"
    );
    let verdicts: Vec<&str> = returns["properties"]["verdict"]["enum"]
        .as_sequence()
        .expect("verdict is an enum")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        verdicts.contains(&"escalate"),
        "P-2.21 requires escalation to be a first-class answer, not a failure: {verdicts:?}"
    );
}
