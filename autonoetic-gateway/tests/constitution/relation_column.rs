//! §6.4 — the signed text and the register may not disagree about a clause's
//! relation (RFC #1283).
//!
//! This is the check the RFC has specified since revision 1 and that could not
//! be written until the 2026.09.04 amendment gave the document a `Relation`
//! column to agree *with*. It is also the reason the amendment is worth
//! making: a column of prose in a signed document drifts from the code that
//! implements it, unless the same test reads both.
//!
//! The failure it prevents already happened once at a smaller scale.
//! `self_describe` sourced rights from the enforcement register while
//! hardcoding their bind direction as a literal, so the one place that could
//! disagree with the register did — silently, because nothing compared them.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("gateway crate always has a workspace parent")
        .to_path_buf()
}

/// Version directories whose `constitution.md` carries a `Relation` column,
/// oldest first.
fn versions_with_relations() -> Vec<String> {
    let dir = workspace_root().join("docs/constitution/versions");
    let mut out: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|v| {
            std::fs::read_to_string(dir.join(v).join("constitution.md"))
                .map(|t| !declared_relations(&t).is_empty())
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

/// Read `docs/constitution/versions/<version>/constitution.md`.
fn version_text(version: &str) -> String {
    let p = workspace_root()
        .join("docs/constitution/versions")
        .join(version)
        .join("constitution.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// `(clause_id, relation_cell)` for every clause that declares one — table
/// rows by their last cell, `I-*` bullets by their `**Relation:**` sentence.
fn declared_relations(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if !(t.starts_with('|') && t.ends_with('|')) {
            continue;
        }
        let cells: Vec<&str> = t[1..t.len() - 1].split('|').map(str::trim).collect();
        let Some(id) = cells.first() else { continue };
        if !is_clause_id(id) {
            continue;
        }
        // The last cell is a Relation only if it has the shape. Versions
        // predating this amendment end each row with `Status`, and reading
        // "ENFORCED" as a relation would make the before/after test pass
        // against a document that has no column at all.
        if let Some(cell) = cells.last().filter(|c| is_relation_cell(c)) {
            out.push(((*id).to_string(), (*cell).to_string()));
        }
    }
    // `- **I-4** … **Relation:** enforcer · none · detective.`
    let flat = text.replace('\n', " ");
    let mut rest = flat.as_str();
    while let Some(at) = rest.find("**I-") {
        let after = &rest[at + 2..];
        let id: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        rest = &rest[at + 2..];
        if !is_clause_id(&id) {
            continue;
        }
        // The relation belongs to this bullet only if it appears before the
        // next one starts.
        let scope = rest.find("- **I-").map_or(rest, |n| &rest[..n]);
        if let Some(rel_at) = scope.find("**Relation:** ") {
            let tail = &scope[rel_at + "**Relation:** ".len()..];
            if let Some(end) = tail.find('.') {
                out.push((id, tail[..end].trim().to_string()));
            }
        }
    }
    out
}

/// `binds · owed to · requires` — three ` · `-joined lowercase tokens.
fn is_relation_cell(s: &str) -> bool {
    let parts: Vec<&str> = s.split(" · ").collect();
    parts.len() == 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c == '+' || c == ' ' || c == '(' || c == ')')
        })
}

fn is_clause_id(s: &str) -> bool {
    let Some((fam, rest)) = ["Ri-", "P-", "O-", "U-", "I-"]
        .into_iter()
        .find_map(|f| s.strip_prefix(f).map(|r| (f, r)))
    else {
        return false;
    };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    let dotted = rest.contains('.');
    let sectioned = matches!(fam, "Ri-" | "P-");
    dotted == sectioned && !rest.ends_with('.') && !rest.starts_with('.')
}

/// What the register would print for a clause, in the document's notation.
fn register_relation(clause_id: &str) -> Option<String> {
    use autonoetic_gateway::constitution_relations as rel;
    use autonoetic_gateway::enforcement_register::{Binds, OwedTo};
    let f = rel::relation(clause_id)?;
    let owed = match f.owed_to {
        OwedTo::NoOne => "none".to_string(),
        OwedTo::Seat(p) => format!("{} (seat)", p.label()),
        OwedTo::Principal(k) => k.as_str().to_string(),
    };
    let _ = Binds::ALL;
    Some(format!(
        "{} · {} · {}",
        f.binds.label(),
        owed,
        f.requires.label()
    ))
}

/// **The agreement.** Every clause the active constitution declares carries a
/// `Relation`, and it says exactly what the register says.
#[test]
fn the_signed_relation_column_agrees_with_the_register() {
    // Every version that carries the column, not just the active one. While
    // the amendment is a draft the active version predates it; after
    // activation the active version is one of these. Checking all of them
    // means this test needs no edit at activation, and it keeps holding for
    // superseded versions — whose bytes are frozen, so a disagreement there
    // would mean the *register* drifted away from law already ratified.
    let versions = versions_with_relations();
    assert!(
        !versions.is_empty(),
        "no constitution version declares a Relation column; this test has \
         nothing to compare and would pass vacuously"
    );

    let mut declared: Vec<(String, String)> = Vec::new();
    for v in &versions {
        let per_version = declared_relations(&version_text(v));
        assert!(
            per_version.len() >= 221,
            "{v} declares only {} relations; a version that has the column at \
             all must have it for every clause",
            per_version.len()
        );
        declared.extend(per_version);
    }

    let mut mismatches = Vec::new();
    for (id, cell) in &declared {
        match register_relation(id) {
            Some(expected) if expected == *cell => {}
            Some(expected) => mismatches.push(format!(
                "{id}: document says `{cell}`, register says `{expected}`"
            )),
            None => mismatches.push(format!(
                "{id}: the document declares a relation the register does not classify"
            )),
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} clause(s) disagree between the signed text and the register. The \
         document is the law and the register implements it, so a disagreement \
         means one of them is wrong about what a clause obliges — regenerate \
         the column from `constitution_relations`, or fix the register.\n\n  {}\n",
        mismatches.len(),
        mismatches.join("\n  ")
    );
}

/// The converse: nothing the register classifies is missing from the text.
///
/// Without this, the agreement above is satisfiable by a document that
/// declares one relation and omits 220.
#[test]
fn every_registered_clause_appears_in_the_signed_text() {
    for version in versions_with_relations() {
        let declared = declared_relations(&version_text(&version));
        let ids: std::collections::HashSet<String> =
            declared.into_iter().map(|(id, _)| id).collect();

        let missing: Vec<&str> = autonoetic_gateway::constitution_relations::relations()
            .iter()
            .map(|r| r.id)
            .filter(|id| !ids.contains(*id))
            .collect();
        assert!(
            missing.is_empty(),
            "{} clause(s) are classified in the register but carry no Relation \
             in {version}: {missing:?}",
            missing.len()
        );
    }
}

/// Collapse runs of whitespace, so a phrase that spans a hard-wrapped line
/// can be matched as written.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every clause row is well-formed — exactly six cells.
///
/// Two malformations of this class have been found by hand, and both made the
/// *digest* record the wrong enforcement citation, because
/// `extract_enforcement_table` filters empty cells and reads `cells[3]`
/// without ever checking arity:
///
/// - `P-9.13` was missing its `Source` cell, so `cells[3]` was its **Status**
///   ("ENFORCED"). Repaired by the 2026.09.04 amendment.
/// - `P-5.2` has a literal `|` inside a code span, producing a seventh cell,
///   so `cells[3]` was its **Source**. Repaired by 2026.09.05.
///
/// Neither was visible to any check. This closes the class.
///
/// Only the newest version is asserted: earlier ones are frozen bytes, and
/// several carry malformations repairable only by amendment.
#[test]
fn every_clause_row_is_well_formed() {
    let Some(version) = versions_with_relations().pop() else {
        panic!("no constitution version carries a Relation column");
    };
    let text = version_text(&version);

    let mut malformed = Vec::new();
    let mut checked = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if !(t.starts_with('|') && t.ends_with('|')) {
            continue;
        }
        let cells: Vec<&str> = t[1..t.len() - 1]
            .split('|')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        let Some(id) = cells.first() else { continue };
        if !is_clause_id(id) {
            continue;
        }
        checked += 1;
        // ID | statement | source/why | enforcement | status | relation
        if cells.len() != 6 {
            malformed.push(format!("{id}: {} cells, expected 6", cells.len()));
        }
    }

    assert!(
        checked >= 207,
        "expected every clause table row in {version}; saw {checked} — this \
         scan is no longer finding them"
    );
    assert!(
        malformed.is_empty(),
        "{} clause row(s) in {version} are malformed. The digest's enforcement \
         table reads `cells[3]` after filtering empty cells, so a missing or \
         extra cell silently records the wrong citation — a Source or a Status \
         where the code reference belongs. A literal `|` inside a code span is \
         the usual cause; escaping it does not help, because the parser \
         plain-splits.\n\n  {}\n",
        malformed.len(),
        malformed.join("\n  ")
    );
}

/// **The correcting sentence must itself be true**, and computed rather than
/// typed.
///
/// The amendment exists because a false asserted sentence sat pinned in signed
/// text for months. Its correction shipped in review with the wrong number —
/// "181 of the 182" and one exception, when the answer is 180 and two, because
/// `P-2.21` binds the decider. The document contradicted itself: the
/// measurement table said `enforcer: 215`, and 215 requires 180.
///
/// Prose that states a count about the register is the same failure mode as
/// prose that states a bind direction about a clause. Both must be read back
/// from the data, not maintained by hand.
#[test]
fn the_measured_counts_in_the_signed_text_match_the_register() {
    use autonoetic_gateway::constitution_relations as rel;
    use autonoetic_gateway::enforcement_register::Binds;

    let p_clauses: Vec<&'static str> = rel::relations()
        .iter()
        .filter(|r| r.id.starts_with("P-"))
        .map(|r| r.id)
        .collect();
    let enforcer = p_clauses
        .iter()
        .filter(|id| rel::relation(id).map(|f| f.binds) == Some(Binds::Enforcer))
        .count();
    let exceptions: Vec<&str> = p_clauses
        .iter()
        .copied()
        .filter(|id| rel::relation(id).map(|f| f.binds) != Some(Binds::Enforcer))
        .collect();

    for version in versions_with_relations() {
        // Whitespace-collapsed: the constitution is hard-wrapped at ~70
        // columns, so any phrase long enough to be worth asserting spans a
        // line break. Searching the raw text finds nothing and reports the
        // claim as missing — a false negative this file has produced twice.
        let text = squash(&version_text(&version));
        assert!(
            text.contains(&format!(
                "**{enforcer} of the {} `P-*` bind the",
                p_clauses.len()
            )),
            "{version}'s correction sentence does not state the measured count \
             ({enforcer} of {}). Registered exceptions: {exceptions:?}",
            p_clauses.len()
        );
        // Every exception must be named. A count with an unnamed exception is
        // how `P-2.21` got erased from the first draft.
        for id in &exceptions {
            assert!(
                text.contains(&format!("`{id}` binds")),
                "{version} states the count but does not name the exception {id}"
            );
        }
    }
}

/// The amendment's own before/after, captured as a test.
///
/// The process asks for something that fails before the change and passes
/// after. This amendment changes no behaviour, so what fails beforehand is the
/// document: `2026.09.02` has no `Relation` column at all, and `2026.09.04`
/// has one for all 221 clauses. Asserting both sides keeps the claim honest.
#[test]
fn amendment_2026_09_04_adds_the_relation_column() {
    assert!(
        declared_relations(&version_text("2026.09.02")).is_empty(),
        "the baseline was expected to carry no Relation column; if it does, \
         this amendment's premise is wrong"
    );
    let after = declared_relations(&version_text("2026.09.04"));
    assert_eq!(
        after.len(),
        221,
        "2026.09.04 must declare a relation for every clause"
    );

    // And the corrections the amendment exists to make are in the text.
    let text = version_text("2026.09.04");
    assert!(
        !text.contains("everything under §1–§11 binds the agent"),
        "the false uniform-by-section claim must be gone"
    );
    assert!(
        !text.contains("binds the **community**"),
        "§12's `community` aggregate must be gone — it broke §0's own \
         one-party rule"
    );
    assert!(
        text.contains("## Semantic Foundations"),
        "the vocabulary section (RFC §2.8) is part of this amendment"
    );

    // The Vision section carried the same false claim in different words, and
    // a targeted fix of §0 and §12 left it standing. Swept and pinned, because
    // "correct the claim where I noticed it" is how it survived the first
    // pass.
    assert!(
        !text.contains("Rules bind the\n  agent") && !text.contains("Rights bind the gateway"),
        "the Vision section must not restate the uniform-by-section claim"
    );
    // A stale cross-reference inherited from before §15 existed. An amendment
    // is the right vehicle for fixing a pointer in signed text.
    assert!(
        !text.contains("below, after §14"),
        "the amendment-process cross-reference must name §15"
    );
}
