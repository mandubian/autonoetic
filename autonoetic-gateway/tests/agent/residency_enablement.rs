//! #1247 — shipped-manifest contract for residency enablement.
//!
//! Residency (`agent.resident_idle_ttl_secs`, #902) was complete but dormant:
//! no bundle declared a TTL, so the parked arm of the addressability union
//! never ran and peers had nobody to talk to. #1247 decided to enable it on
//! the roles whose job is being reachable, and this test pins that decision
//! from both sides:
//!
//! - the three shipped resident bundles must keep their TTL — deleting the
//!   field would silently reintroduce the empty-peer-set problem the doc
//!   describes;
//! - no other shipped bundle may acquire residency — gating roles
//!   (`accepts_from: []`) refuse peer mail, and workers are one-shot by
//!   contract, so a TTL there would be cost with no reachability benefit
//!   (and, for gating roles, parked sessions invisible as "finished").

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use autonoetic_gateway::runtime::parser::SkillParser;

/// Shipped residency: agent id -> declared TTL. Keep in sync with
/// docs/reference/agent-messaging.md ("Resident Sessions").
const EXPECTED_RESIDENT: &[(&str, u64)] = &[
    ("planner.default", 900),
    ("planner.collaborative", 900),
    ("watchdog.default", 900),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn skill_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![repo_root().join("agents")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn shipped_residency_roster_matches_the_1247_decision() {
    let mut actual: BTreeMap<String, u64> = BTreeMap::new();
    for path in skill_files() {
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let (manifest, _) = SkillParser::parse(&content)
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        if let Some(ttl) = manifest.agent.resident_idle_ttl_secs {
            let replaced = actual.insert(manifest.agent.id.clone(), ttl);
            if replaced.is_some() {
                panic!(
                    "{}: agent id '{}' declares residency in more than one shipped \
                     SKILL.md — the roster must name each resident agent exactly once",
                    path.display(),
                    manifest.agent.id
                );
            }
        }
    }

    let expected: BTreeMap<String, u64> = EXPECTED_RESIDENT
        .iter()
        .map(|(id, ttl)| (id.to_string(), *ttl))
        .collect();

    assert_eq!(
        actual, expected,
        "the shipped residency roster drifted from the #1247 decision — \
         a removed TTL silently empties the peer set, an added one parks a \
         session that has no reason to be reachable"
    );
}
