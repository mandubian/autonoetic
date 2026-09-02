//! §13 invariant enforcement — the mechanisms the amendment's new citations name.
//!
//! An invariant is a **universal** ("no path anywhere does X"), which no number
//! of examples can prove. A rule is **existential** ("this chokepoint behaves
//! this way") and is testable by calling it. That difference — not vagueness —
//! is why eight of fourteen invariants carried no enforcement citation before
//! the 2026.09.02 amendment.
//!
//! Enforcement of a universal means converting it into something finite. These
//! tests pin the two conversions the amendment claims are already complete:
//!
//! - **I-8** — make the bad state *unrepresentable*. Policy decision functions
//!   do not take reasoning as a parameter, so no call site can consult it,
//!   including call sites that do not exist yet. The test guards the signature.
//! - **I-9** — a *closed enum*. `YieldReason` cannot grow an unlisted variant
//!   without a compile error at every exhaustive match.
//!
//! The remaining conversions the amendment cites are covered elsewhere and not
//! duplicated here: I-2 by P-8.16's fsync ordering, I-4 by the discretion-leak
//! register (`runtime::discretion_leak::tests`), I-9's roundtrip by
//! `rights_mid_bucket.rs`.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gateway crate always has a workspace parent")
        .to_path_buf()
}

/// Parse `- **I-N** …` bullets out of a constitution's §13, returning
/// `(id, statement)` with continuation lines folded in.
fn invariants(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- **I-") {
            let Some((num, tail)) = rest.split_once("**") else {
                continue;
            };
            // `I-13 (Creation is not delegation.)` — the id stops at the space.
            let id = format!("I-{}", num.split_whitespace().next().unwrap_or(num));
            out.push((id, tail.trim().to_string()));
        } else if !trimmed.is_empty() && (line.starts_with("  ") || line.starts_with('\t')) {
            if let Some(last) = out.last_mut() {
                last.1.push(' ');
                last.1.push_str(trimmed);
            }
        }
    }
    out
}

/// An invariant "declares its enforcement" when its text names a code path, a
/// symbol, or an explicit status — i.e. a reader can go check. Bare prose
/// cannot fail, which is what made `R+9` survivable for months.
fn declares_enforcement(statement: &str) -> bool {
    statement.contains(".rs")
        || statement.contains("::")
        || statement.contains("ENFORCED")
        || statement.contains("PARTIAL")
        || statement.contains("DESIGN DEBT")
        || statement.contains("deliberate absence")
}

fn read_version(root: &Path, version: &str) -> String {
    let p = root
        .join("docs/constitution/versions")
        .join(version)
        .join("constitution.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The amendment's own before/after, captured as a test.
///
/// The amendment process requires "a test module that fails before the change
/// and passes after". For an amendment that adds *citations*, no behaviour
/// changes — so the thing that fails beforehand is this completeness check
/// against the pre-amendment text. Asserting both sides in one test makes the
/// delta the artifact, and keeps the claim honest: 2026.08.30 really did leave
/// eight invariants unfalsifiable.
#[test]
fn amendment_2026_09_02_closes_five_uncited_invariants() {
    let root = workspace_root();

    let before = invariants(&read_version(&root, "2026.08.30"));
    let after = invariants(&read_version(&root, "2026.09.02"));
    assert_eq!(
        before.len(),
        after.len(),
        "the amendment must not add or drop an invariant — it only adds citations"
    );
    assert!(before.len() >= 14, "expected the full §13 set, got {}", before.len());

    let bare = |v: &[(String, String)]| -> Vec<String> {
        v.iter()
            .filter(|(_, s)| !declares_enforcement(s))
            .map(|(id, _)| id.clone())
            .collect()
    };

    let bare_before = bare(&before);
    let bare_after = bare(&after);

    // The pre-amendment state this amendment exists to fix.
    for id in ["I-2", "I-3", "I-4", "I-8", "I-9"] {
        assert!(
            bare_before.contains(&id.to_string()),
            "{id} was expected to be uncited in 2026.08.30; \
             if it already had a citation this amendment's premise is wrong"
        );
        assert!(
            !bare_after.contains(&id.to_string()),
            "{id} must declare its enforcement in 2026.09.02"
        );
    }

    // Deliberately still bare — each needs work the amendment does not do:
    // I-5 wants static analysis over Rust constants, I-7 is a meta-rule about
    // amendment itself, I-13 documents a deliberate absence in its own words.
    assert_eq!(
        bare_after,
        vec!["I-5".to_string(), "I-7".to_string()],
        "only I-5 and I-7 should remain uncited; anything else is an unintended \
         change or a regression in the amendment text"
    );
}

/// **I-8, signature-level.** The gateway does not read minds because the
/// decision functions never receive the mind.
///
/// This is the strongest form of enforcement available for a universal: the
/// property holds for call sites that do not exist yet, because the parameter
/// is absent. The test exists so a refactor that threads reasoning into a
/// policy decision fails loudly rather than quietly turning the gateway into a
/// thought-policing engine (Ri-0.13(a), §14).
#[test]
fn i_8_policy_decision_signatures_cannot_see_reasoning() {
    let src = std::fs::read_to_string(workspace_root().join("autonoetic-gateway/src/policy.rs"))
        .expect("policy.rs is readable");

    // Only the production prefix: test fixtures may legitimately mention
    // reasoning while constructing unrelated state.
    let production = match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => &src[..],
    };

    let mut offenders = Vec::new();
    for (lineno, line) in production.lines().enumerate() {
        let t = line.trim();
        if !t.starts_with("pub fn ") && !t.starts_with("fn ") {
            continue;
        }
        // Decision surfaces: the functions whose verdict is policy.
        let is_decision = t.contains("can_") || t.contains("-> PolicyDecision");
        if !is_decision {
            continue;
        }
        // Scan the **parameter list only**, never the whole signature. I-8
        // forbids reasoning as an *input*; it does not forbid decisions whose
        // *subject* is reasoning. `can_audit_reasoning(&self, target_agent_id)`
        // is the `ReasoningAudit` capability check that Ri-0.13(c) explicitly
        // requires — matching on the function name would flag the constitution
        // working correctly.
        let Some(params) = t
            .find('(')
            .and_then(|open| t.rfind(')').map(|close| &t[open + 1..close]))
        else {
            continue;
        };
        let lower = params.to_lowercase();
        for banned in ["reasoning", "chain_of_thought", "scratchpad", "thought"] {
            if lower.contains(banned) {
                offenders.push(format!("policy.rs:{}: {}", lineno + 1, t));
                break;
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "I-8: a policy decision surface must not accept agent reasoning — \
         the invariant is enforced by the parameter being absent, so adding \
         one silently converts a universal guarantee into a convention.\n  {}",
        offenders.join("\n  ")
    );

    // Guard the guard: if the scan finds no decision surfaces at all, it has
    // gone blind (renamed functions, moved file) and would pass vacuously.
    let decision_fns = production
        .lines()
        .filter(|l| {
            let t = l.trim();
            (t.starts_with("pub fn ") || t.starts_with("fn "))
                && (t.contains("can_") || t.contains("-> PolicyDecision"))
        })
        .count();
    assert!(
        decision_fns >= 5,
        "expected policy.rs to expose several decision surfaces, found {decision_fns} — \
         this scan is no longer looking at the right thing"
    );
}

/// **I-9, closed enum.** An unlisted termination reason is a compile error, not
/// a runtime possibility.
///
/// `rights_mid_bucket.rs` already pins roundtrip and unknown-variant rejection.
/// What this adds is the *closedness* claim the new citation makes: the variant
/// set is fixed in source, so exhaustive matches cannot silently admit a new
/// reason.
#[test]
fn i_9_yield_reason_is_a_closed_enum_in_source() {
    let src = std::fs::read_to_string(
        workspace_root().join("autonoetic-gateway/src/runtime/checkpoint.rs"),
    )
    .expect("checkpoint.rs is readable");

    let decl = src
        .find("pub enum YieldReason")
        .expect("I-9 cites YieldReason as the closed list; it must exist in checkpoint.rs");

    // A `#[non_exhaustive]` enum is not a closed list: downstream matches would
    // need a wildcard arm, which is exactly the silent admission I-9 forbids.
    let preamble = &src[decl.saturating_sub(400)..decl];
    assert!(
        !preamble.contains("non_exhaustive"),
        "I-9: YieldReason must not be #[non_exhaustive] — a wildcard arm would \
         let an unlisted termination reason through without a compile error"
    );

    let body_start = decl + src[decl..].find('{').expect("enum body");
    let body_end = body_start
        + src[body_start..]
            .find("\n}")
            .expect("enum body terminates");
    let body = &src[body_start..body_end];

    let variants = body
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("//")
                && !l.starts_with("///")
                && !l.starts_with('#')
                && !l.starts_with('{')
        })
        .count();
    assert!(
        variants >= 10,
        "Ri-0.12 declares 12 yield causes; found {variants} variant lines — \
         either the enum shrank or this parse is wrong"
    );
}
