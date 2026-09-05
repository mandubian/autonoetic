//! The amendment materializer (#810): turn approved `cprop-` proposals into a
//! candidate constitution version.
//!
//! The proposal machinery upstream is complete and pinned: durable intake
//! (Ri-0.8), a full state machine with decider obligations (O-6), and SLA
//! breach flagging. Until now the loop stopped one step short — an *approved*
//! proposal never mechanically became law; the operator edited the markdown by
//! hand (`publish_approved_proposals` stamps a tag and says exactly that).
//! This module closes that last mile, mechanically:
//!
//! 1. apply each approved proposal's `kind`/`target_id`/`proposed_text` to a
//!    copy of the active constitution text (modify = replace the statement
//!    cell; remove = delete the row; add = insert after the clause's section
//!    siblings with explicit DRAFT placeholder cells);
//! 2. compute the candidate's canonical digest through the same code path the
//!    active lock pins, producing an **unsigned** lock byte-compatible with
//!    what `docs/constitution/recompute_lock.py` signs;
//! 3. write `docs/constitution/versions/<candidate>/` with the markdown, the
//!    unsigned lock, and a `provenance.json` linking every applied proposal
//!    back to its ID and adjudication.
//!
//! Division of labor (Lawful Executor, §14): the gateway **drafts**, it never
//! enacts. The candidate directory is inert — it does not touch
//! `docs/constitution/CURRENT`, the `ACTIVE_CONSTITUTION_VERSION` pin, or any
//! signed byte. The operator stays sovereign at the signature: they review the
//! draft, complete anything the materializer could only placeholder (Source,
//! Status, Relation on added rows), sign via `recompute_lock.py`, and activate
//! through the ordinary ceremony. A materialized draft that was never signed
//! is indistinguishable from a directory that was never written.

use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::constitution_digest::{compute_constitution_digest, ConstitutionLock};
use crate::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal;

/// The `kind` values `constitution_propose_amendment` accepts (single source
/// of truth stays in the intake tool; mirrored here so a new kind cannot be
/// added there without deciding how it materializes).
pub const MATERIALIZABLE_KINDS: &[&str] = &[
    "add_rule",
    "modify_rule",
    "remove_rule",
    "add_right",
    "modify_right",
    "remove_right",
];

/// The proposal fields the materializer applies. A deliberate subset of
/// [`ConstitutionalProposal`]: the materializer reads, never writes, proposals.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MaterializableProposal {
    pub proposal_id: String,
    pub proposer_agent_id: String,
    pub kind: String,
    pub target_id: Option<String>,
    pub proposed_text: Option<String>,
    pub justification: String,
    pub decided_by: Option<String>,
    pub decided_at: Option<String>,
    pub decision_reason: Option<String>,
}

impl From<&ConstitutionalProposal> for MaterializableProposal {
    fn from(p: &ConstitutionalProposal) -> Self {
        Self {
            proposal_id: p.proposal_id.clone(),
            proposer_agent_id: p.proposer_agent_id.clone(),
            kind: p.kind.clone(),
            target_id: p.target_id.clone(),
            proposed_text: p.proposed_text.clone(),
            justification: p.justification.clone(),
            decided_by: p.decided_by.clone(),
            decided_at: p.decided_at.clone(),
            decision_reason: p.decision_reason.clone(),
        }
    }
}

/// What one proposal did to the text. `before`/`after` carry the full table
/// row (or `None` for add/remove respectively), so the report is the diff.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProposalEdit {
    pub proposal_id: String,
    pub kind: String,
    pub target_id: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// The result of applying a proposal batch to a base text.
#[derive(Debug, Clone)]
pub struct AppliedAmendments {
    pub text: String,
    pub edits: Vec<ProposalEdit>,
}

/// The report for a materialized candidate version — also the CLI's JSON
/// payload.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MaterializeReport {
    pub base_version: String,
    pub candidate_version: String,
    pub candidate_dir: PathBuf,
    pub base_digest: String,
    pub candidate_digest: String,
    pub rule_enforcement_count: usize,
    pub right_enforcement_count: usize,
    pub proposal_ids: Vec<String>,
    pub edits: Vec<ProposalEdit>,
}

/// First cell of a clause table row, if the line is one. Header/separator
/// rows yield `None` so they can never be mistaken for clauses.
fn clause_row_id(line: &str) -> Option<String> {
    let t = line.trim();
    if !(t.starts_with('|') && t.ends_with('|')) {
        return None;
    }
    let first = t[1..].split('|').next()?.trim();
    if first.is_empty() || first == "ID" || first.starts_with("---") {
        return None;
    }
    Some(first.to_string())
}

/// Non-empty cell count of a table row, using the same
/// split-on-`|`-filter-empty semantics as the digest's enforcement-table
/// extractor — the materializer must see rows exactly as the digest sees them.
fn clause_row_cell_count(line: &str) -> Option<usize> {
    let t = line.trim();
    if !(t.starts_with('|') && t.ends_with('|')) {
        return None;
    }
    Some(
        t[1..t.len() - 1]
            .split('|')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .count(),
    )
}

/// A statement must be a single line with no pipe — the exact class of
/// malformation 2026.09.05 repaired in `P-5.2`. The row-arity guard
/// (`relation_column.rs::every_clause_row_is_well_formed`) fails the next
/// constitution suite if a pipe ever slips through, so it is refused here,
/// at the point of drafting.
fn sanitized_statement(proposal_id: &str, proposed_text: Option<&str>) -> anyhow::Result<String> {
    let Some(raw) = proposed_text else {
        bail!("proposal {proposal_id}: `proposed_text` is required");
    };
    let t = raw.trim();
    anyhow::ensure!(!t.is_empty(), "proposal {proposal_id}: `proposed_text` is empty");
    anyhow::ensure!(
        !t.contains('|'),
        "proposal {proposal_id}: `proposed_text` contains '|' — a literal pipe splits the \
         clause table row and corrupts the digest's enforcement citation (the P-5.2 defect). \
         Rewrite the enumeration (e.g. `X` or `Y`) without the character."
    );
    anyhow::ensure!(
        !t.contains('\n') && !t.contains('\r'),
        "proposal {proposal_id}: `proposed_text` must be a single line (table cell)"
    );
    Ok(t.to_string())
}

/// `P-8.20` → `("P-", "8.")`; the section prefix used to place an added row
/// after its section siblings.
fn clause_family_and_section(target: &str) -> Option<(String, String)> {
    let (family, rest) = match target.split_once('-') {
        Some(pair) => pair,
        None => return None,
    };
    let (section, minor) = rest.split_once('.')?;
    if family.is_empty() || section.is_empty() || minor.is_empty() {
        return None;
    }
    if !target.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        return None;
    }
    Some((format!("{family}-"), format!("{section}.")))
}

fn require_target<'a>(proposal_id: &str, target_id: &'a Option<String>) -> anyhow::Result<&'a str> {
    let Some(t) = target_id.as_deref() else {
        bail!("proposal {proposal_id}: `target_id` is required for this kind");
    };
    let t = t.trim();
    anyhow::ensure!(!t.is_empty(), "proposal {proposal_id}: `target_id` is empty");
    Ok(t)
}

/// Apply a batch of approved proposals to the base constitution text.
///
/// Proposals apply in list order, each seeing the previous one's result.
/// The base text is never mutated in place — failure at any proposal leaves
/// the input untouched and reports which proposal failed and why.
pub fn apply_proposals_to_text(
    base_text: &str,
    proposals: &[MaterializableProposal],
) -> anyhow::Result<AppliedAmendments> {
    anyhow::ensure!(
        !proposals.is_empty(),
        "no proposals to apply — refusing to materialize an empty amendment"
    );

    let mut lines: Vec<String> = base_text.lines().map(str::to_string).collect();
    let mut edits = Vec::new();

    for p in proposals {
        anyhow::ensure!(
            MATERIALIZABLE_KINDS.contains(&p.kind.as_str()),
            "proposal {}: unknown kind {:?} (expected one of {})",
            p.proposal_id,
            p.kind,
            MATERIALIZABLE_KINDS.join(", ")
        );
        let edit = match p.kind.as_str() {
            "modify_rule" | "modify_right" => apply_modify(&mut lines, p)?,
            "remove_rule" | "remove_right" => apply_remove(&mut lines, p)?,
            "add_rule" | "add_right" => apply_add(&mut lines, p)?,
            other => unreachable!("kind {other} passed MATERIALIZABLE_KINDS check"),
        };
        edits.push(edit);
    }

    let trailing_newline = base_text.ends_with('\n');
    let mut text = lines.join("\n");
    if trailing_newline {
        text.push('\n');
    }
    Ok(AppliedAmendments { text, edits })
}

/// Modify: replace the statement cell, preserving every other cell byte for
/// byte (Source, Enforcement, Status, Relation are facts about enforcement,
/// not part of what the proposal amends).
fn apply_modify(lines: &mut [String], p: &MaterializableProposal) -> anyhow::Result<ProposalEdit> {
    let target = require_target(&p.proposal_id, &p.target_id)?;
    let statement = sanitized_statement(&p.proposal_id, p.proposed_text.as_deref())?;

    let idx = find_unique_row(lines, target, &p.proposal_id)?;
    let line = &lines[idx];
    anyhow::ensure!(
        clause_row_cell_count(line) == Some(6),
        "proposal {}: clause {target}'s row in the base text is malformed (expected 6 cells); \
         refusing to modify a row the digest cannot parse unambiguously",
        p.proposal_id
    );

    let before = line.trim().to_string();
    let mut segments: Vec<String> = line.split('|').map(str::to_string).collect();
    // split layout: "" | id | statement | source | enforcement | status | relation | ""
    // so the statement cell is segment 2. The 6-cell check above pins this.
    segments[2] = format!(" {statement} ");
    lines[idx] = segments.join("|");

    Ok(ProposalEdit {
        proposal_id: p.proposal_id.clone(),
        kind: p.kind.clone(),
        target_id: p.target_id.clone(),
        before: Some(before),
        after: Some(lines[idx].trim().to_string()),
    })
}

/// Remove: delete the clause's row outright.
fn apply_remove(lines: &mut Vec<String>, p: &MaterializableProposal) -> anyhow::Result<ProposalEdit> {
    let target = require_target(&p.proposal_id, &p.target_id)?;
    let idx = find_unique_row(lines, target, &p.proposal_id)?;
    let before = lines.remove(idx).trim().to_string();
    Ok(ProposalEdit {
        proposal_id: p.proposal_id.clone(),
        kind: p.kind.clone(),
        target_id: p.target_id.clone(),
        before: Some(before),
        after: None,
    })
}

/// Add: insert a new row after the clause's section siblings, with explicit
/// DRAFT placeholder cells. The placeholders are the honest mechanical output:
/// Source/Status/Relation are classification the gateway must not author, so
/// it marks them unfinished instead of inventing values — completing them is
/// the operator's substantive act before signing.
fn apply_add(lines: &mut Vec<String>, p: &MaterializableProposal) -> anyhow::Result<ProposalEdit> {
    let target = require_target(&p.proposal_id, &p.target_id)?;
    let statement = sanitized_statement(&p.proposal_id, p.proposed_text.as_deref())?;
    let (family, section_prefix) = clause_family_and_section(target).ok_or_else(|| {
        anyhow::anyhow!(
            "proposal {}: target {target:?} is not a dotted clause id (expected e.g. `P-8.20`)",
            p.proposal_id
        )
    })?;
    let full_section_prefix = format!("{family}{section_prefix}");

    if lines
        .iter()
        .any(|l| clause_row_id(l).as_deref() == Some(target))
    {
        bail!("proposal {}: clause {target} already exists in the base text", p.proposal_id);
    }

    let insert_after = lines
        .iter()
        .rposition(|l| clause_row_id(l).is_some_and(|id| id.starts_with(&full_section_prefix)))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "proposal {}: no {full_section_prefix}* rows in the base text to place {target} \
                 after — a new section needs operator drafting, not mechanical insertion",
                p.proposal_id
            )
        })?;

    let row = format!(
        "| {target} | {statement} | materialized from {}: Source pending operator classification | TBD — not yet implemented | DRAFT | TBD · TBD · TBD |",
        p.proposal_id
    );
    lines.insert(insert_after + 1, row.clone());

    Ok(ProposalEdit {
        proposal_id: p.proposal_id.clone(),
        kind: p.kind.clone(),
        target_id: p.target_id.clone(),
        before: None,
        after: Some(row),
    })
}

fn find_unique_row(lines: &[String], target: &str, proposal_id: &str) -> anyhow::Result<usize> {
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| clause_row_id(l).is_some_and(|id| id == target).then_some(i))
        .collect();
    match matches.as_slice() {
        [idx] => Ok(*idx),
        [] => bail!("proposal {proposal_id}: clause {target} not found in the base text"),
        [_, _, ..] => bail!("proposal {proposal_id}: clause {target} matches {} rows", matches.len()),
    }
}

/// Materialize a candidate constitution version from approved proposals.
///
/// Reads the base version's signed pair from `versions_dir`, verifies the base
/// text still reproduces its pinned digest, applies the proposals, and writes
/// `<candidate_version>/` with:
///
/// - `constitution.md` — the base text with the amendments applied
/// - `gateway-constitution.lock.json` — **unsigned** (`signature` omitted),
///   stable fields inherited from the base lock exactly as
///   `recompute_lock.py`'s template seeding would inherit them
/// - `provenance.json` — every applied proposal, its adjudication, and the
///   before/after row diff
///
/// Refuses to clobber an existing candidate directory, and refuses a base
/// whose text no longer matches its digest. Signing and activation stay with
/// the operator.
pub fn materialize_candidate_version(
    versions_dir: &Path,
    base_version: &str,
    candidate_version: &str,
    proposals: &[MaterializableProposal],
) -> anyhow::Result<MaterializeReport> {
    anyhow::ensure!(
        !proposals.is_empty(),
        "no proposals to materialize — refusing to draft an empty candidate version"
    );
    for v in [base_version, candidate_version] {
        anyhow::ensure!(
            is_path_safe_version(v),
            "version {v:?} must be a simple directory name (no path separators or dot-dot)"
        );
    }
    anyhow::ensure!(
        base_version != candidate_version,
        "candidate version must differ from the base version {base_version}"
    );

    let base_dir = versions_dir.join(base_version);
    let base_text = std::fs::read_to_string(base_dir.join("constitution.md")).map_err(|e| {
        anyhow::anyhow!("cannot read base constitution {}: {e}", base_dir.join("constitution.md").display())
    })?;
    let lock_json =
        std::fs::read_to_string(base_dir.join("gateway-constitution.lock.json")).map_err(|e| {
            anyhow::anyhow!(
                "cannot read base lock {}: {e}",
                base_dir.join("gateway-constitution.lock.json").display()
            )
        })?;
    let base_lock: ConstitutionLock = serde_json::from_str(&lock_json)
        .map_err(|e| anyhow::anyhow!("base lock must be valid JSON: {e}"))?;
    anyhow::ensure!(
        base_lock.constitution_version == base_version,
        "base lock pins version {} but materialization was asked to build on {base_version}",
        base_lock.constitution_version
    );

    let (base_digest, _, _) = compute_constitution_digest(&base_text);
    anyhow::ensure!(
        base_digest == base_lock.constitution_digest,
        "base text does not reproduce its pinned digest — the active version is corrupt or \
         was edited without re-signing; refusing to build a candidate on top"
    );

    let candidate_dir = versions_dir.join(candidate_version);
    anyhow::ensure!(
        !candidate_dir.exists(),
        "candidate version directory {} already exists — choose another --version or remove \
         the stale candidate",
        candidate_dir.display()
    );

    let applied = apply_proposals_to_text(&base_text, proposals)?;
    let (candidate_digest, rule_count, right_count) = compute_constitution_digest(&applied.text);

    let candidate_lock = ConstitutionLock {
        format_version: base_lock.format_version,
        constitution_id: base_lock.constitution_id.clone(),
        constitution_version: candidate_version.to_string(),
        constitution_source: format!("docs/constitution/versions/{candidate_version}/constitution.md"),
        constitution_digest: candidate_digest.clone(),
        rule_enforcement_count: rule_count,
        right_enforcement_count: right_count,
        canonicalization: base_lock.canonicalization.clone(),
        signature: None,
    };

    let provenance = serde_json::json!({
        "materializer": "amendment-materializer (#810)",
        "created_at": chrono::Utc::now().to_rfc3339(),
        "base_version": base_version,
        "base_digest": base_digest,
        "candidate_version": candidate_version,
        "candidate_digest": candidate_digest,
        "rule_enforcement_count": rule_count,
        "right_enforcement_count": right_count,
        "unsigned": true,
        "proposals": proposals.iter().zip(applied.edits.iter()).map(|(p, e)| serde_json::json!({
            "proposal_id": p.proposal_id,
            "proposer_agent_id": p.proposer_agent_id,
            "kind": p.kind,
            "target_id": p.target_id,
            "justification": p.justification,
            "decided_by": p.decided_by,
            "decided_at": p.decided_at,
            "decision_reason": p.decision_reason,
            "before": e.before,
            "after": e.after,
        })).collect::<Vec<_>>(),
    });

    std::fs::create_dir_all(&candidate_dir)?;
    let write = |name: &str, body: String| -> anyhow::Result<()> {
        let path = candidate_dir.join(name);
        std::fs::write(&path, body)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))
    };
    write("constitution.md", applied.text.clone())?;
    write(
        "gateway-constitution.lock.json",
        format!("{}\n", serde_json::to_string_pretty(&candidate_lock)?),
    )?;
    write("provenance.json", format!("{}\n", serde_json::to_string_pretty(&provenance)?))?;

    Ok(MaterializeReport {
        base_version: base_version.to_string(),
        candidate_version: candidate_version.to_string(),
        candidate_dir,
        base_digest,
        candidate_digest,
        rule_enforcement_count: rule_count,
        right_enforcement_count: right_count,
        proposal_ids: proposals.iter().map(|p| p.proposal_id.clone()).collect(),
        edits: applied.edits,
    })
}

fn is_path_safe_version(v: &str) -> bool {
    !v.is_empty()
        && !v.starts_with('.')
        && !v.contains('/')
        && !v.contains('\\')
        && !v.contains("..")
        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "\
# Constitution

## 8. Causal chain

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-8.1 | Every causal event is append-only. | ARCHITECTURE.md | `causal_chain.rs` | ENFORCED | enforcer · none · preventive |
| P-8.2 | Events are never rewritten. | ARCHITECTURE.md | `causal_chain.rs` | ENFORCED | enforcer · none · detective |

## 0. Rights

| ID | Right | Why | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| Ri-0.1 | Inspect your own capabilities. | self-knowledge | `self_describe` | ENFORCED | enforcer · autonoetic_agent · preventive |
";

    fn proposal(id: &str, kind: &str, target: Option<&str>, text: Option<&str>) -> MaterializableProposal {
        MaterializableProposal {
            proposal_id: id.to_string(),
            proposer_agent_id: "auditor.default".to_string(),
            kind: kind.to_string(),
            target_id: target.map(str::to_string),
            proposed_text: text.map(str::to_string),
            justification: "test".to_string(),
            decided_by: Some("operator".to_string()),
            decided_at: Some("2026-09-05T00:00:00Z".to_string()),
            decision_reason: Some("ok".to_string()),
        }
    }

    #[test]
    fn modify_replaces_statement_and_preserves_other_cells_byte_for_byte() {
        let out = apply_proposals_to_text(
            BASE,
            &[proposal("cprop-1", "modify_rule", Some("P-8.2"), Some("Events are never rewritten, even by operators.") )],
        )
        .unwrap();

        let new_row = out
            .text
            .lines()
            .find(|l| l.trim().starts_with("| P-8.2 "))
            .unwrap();
        assert!(new_row.contains("Events are never rewritten, even by operators."));
        // Every other cell survives verbatim, including padding.
        assert!(new_row.contains(" ARCHITECTURE.md "));
        assert!(new_row.contains(" `causal_chain.rs` "));
        assert!(new_row.contains(" ENFORCED "));
        assert!(new_row.contains(" enforcer · none · detective "));
        assert_eq!(clause_row_cell_count(new_row.trim()), Some(6));
        // Untouched rows are byte-identical.
        assert!(out.text.contains("| P-8.1 | Every causal event is append-only. | ARCHITECTURE.md | `causal_chain.rs` | ENFORCED | enforcer · none · preventive |"));
        let edit = &out.edits[0];
        assert!(edit.before.as_deref().unwrap().contains("Events are never rewritten."));
        assert_eq!(edit.after.as_deref().unwrap().trim(), new_row.trim());
    }

    #[test]
    fn remove_deletes_the_row() {
        let out = apply_proposals_to_text(
            BASE,
            &[proposal("cprop-2", "remove_right", Some("Ri-0.1"), None)],
        )
        .unwrap();
        assert!(!out.text.contains("Ri-0.1"));
        assert!(out.text.contains("Ri-") == false || !out.text.contains("| Ri-0.1 "));
        assert_eq!(out.edits[0].before.as_deref().unwrap(), "| Ri-0.1 | Inspect your own capabilities. | self-knowledge | `self_describe` | ENFORCED | enforcer · autonoetic_agent · preventive |");
        assert!(out.edits[0].after.is_none());
    }

    #[test]
    fn add_inserts_after_section_siblings_with_draft_cells() {
        let out = apply_proposals_to_text(
            BASE,
            &[proposal(
                "cprop-3",
                "add_rule",
                Some("P-8.3"),
                Some("Every causal event carries an intent field."),
            )],
        )
        .unwrap();

        let l = out.text.lines().collect::<Vec<_>>();
        let p82 = l.iter().position(|l| l.trim().starts_with("| P-8.2 ")).unwrap();
        let p83 = l.iter().position(|l| l.trim().starts_with("| P-8.3 ")).unwrap();
        assert_eq!(p83, p82 + 1, "added row goes directly after its section sibling");

        let row = l[p83].trim();
        assert_eq!(clause_row_cell_count(row), Some(6), "draft row must be well-formed");
        assert!(row.contains("materialized from cprop-3"));
        assert!(row.contains("DRAFT"));
        assert!(row.contains("TBD"));
    }

    #[test]
    fn add_without_section_siblings_is_refused() {
        let err = apply_proposals_to_text(
            BASE,
            &[proposal("cprop-4", "add_rule", Some("P-12.1"), Some("New section"))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("no P-12.* rows"), "{err}");
    }

    #[test]
    fn add_duplicate_target_is_refused() {
        let err = apply_proposals_to_text(
            BASE,
            &[proposal("cprop-5", "add_rule", Some("P-8.1"), Some("dup"))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    #[test]
    fn modify_unknown_target_is_refused() {
        let err = apply_proposals_to_text(
            BASE,
            &[proposal("cprop-6", "modify_rule", Some("P-99.9"), Some("x"))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn pipe_in_statement_is_refused_the_p5_2_lesson() {
        let err = apply_proposals_to_text(
            BASE,
            &[proposal("cprop-7", "modify_rule", Some("P-8.1"), Some("`A | B`"))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("P-5.2"), "{err}");
    }

    #[test]
    fn newline_in_statement_is_refused() {
        let err = apply_proposals_to_text(
            BASE,
            &[proposal("cprop-8", "modify_rule", Some("P-8.1"), Some("line1\nline2"))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("single line"), "{err}");
    }

    #[test]
    fn malformed_base_row_is_refused_for_modify() {
        // Seven cells — the exact P-5.2 malformation the arity guard exists for.
        let bad = BASE.replace(
            "| P-8.2 | Events are never rewritten. | ARCHITECTURE.md |",
            "| P-8.2 | Events are never rewritten. | `A | B` | ARCHITECTURE.md |",
        );
        let err = apply_proposals_to_text(
            &bad,
            &[proposal("cprop-9", "modify_rule", Some("P-8.2"), Some("x"))],
        )
        .unwrap_err();
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    #[test]
    fn batch_applies_in_order_and_failure_leaves_base_untouched() {
        let out = apply_proposals_to_text(
            BASE,
            &[
                proposal("cprop-a", "modify_rule", Some("P-8.1"), Some("Amended.")),
                proposal("cprop-b", "add_rule", Some("P-8.3"), Some("New clause.")),
                proposal("cprop-c", "remove_right", Some("Ri-0.1"), None),
            ],
        )
        .unwrap();
        assert!(out.text.contains("Amended."));
        assert!(out.text.contains("| P-8.3 "));
        assert!(!out.text.contains("| Ri-0.1 "));
        assert_eq!(out.edits.len(), 3);

        // Second proposal fails → the whole call errors; base unchanged is
        // guaranteed by taking &str and returning fresh output.
        let err = apply_proposals_to_text(
            BASE,
            &[
                proposal("cprop-a", "modify_rule", Some("P-8.1"), Some("Amended.")),
                proposal("cprop-b", "add_rule", Some("P-8.1"), Some("dup")),
            ],
        );
        assert!(err.is_err());
    }

    #[test]
    fn materialize_writes_candidate_dir_with_unsigned_lock_and_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path().join("versions");
        let base_dir = versions.join("2026.01.01");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(base_dir.join("constitution.md"), BASE).unwrap();

        let (digest, rules, rights) = compute_constitution_digest(BASE);
        let base_lock = serde_json::json!({
            "format_version": 1,
            "constitution_id": "autonoetic-gateway-constitution",
            "constitution_version": "2026.01.01",
            "constitution_source": "docs/constitution/versions/2026.01.01/constitution.md",
            "constitution_digest": digest,
            "rule_enforcement_count": rules,
            "right_enforcement_count": rights,
            "canonicalization": {
                "algorithm": "sha256",
                "payload": "json({constitution_text,rights_enforcement,rules_enforcement})",
                "rules_prefix": "P-",
                "rights_prefix": "Ri-"
            }
        });
        std::fs::write(
            base_dir.join("gateway-constitution.lock.json"),
            serde_json::to_string_pretty(&base_lock).unwrap(),
        )
        .unwrap();

        let report = materialize_candidate_version(
            &versions,
            "2026.01.01",
            "2026.01.02",
            &[
                proposal("cprop-a", "modify_rule", Some("P-8.1"), Some("Amended.")),
                proposal("cprop-b", "add_rule", Some("P-8.3"), Some("New clause.")),
            ],
        )
        .unwrap();

        assert_eq!(report.proposal_ids, vec!["cprop-a", "cprop-b"]);
        let candidate_text =
            std::fs::read_to_string(report.candidate_dir.join("constitution.md")).unwrap();
        assert!(candidate_text.contains("Amended."));
        assert!(candidate_text.contains("| P-8.3 "));

        // The unsigned lock: digest matches the candidate text exactly (what
        // recompute_lock.py will sign), signature omitted.
        let lock: ConstitutionLock = serde_json::from_str(
            &std::fs::read_to_string(report.candidate_dir.join("gateway-constitution.lock.json"))
                .unwrap(),
        )
        .unwrap();
        assert!(lock.signature.is_none());
        assert_eq!(lock.constitution_version, "2026.01.02");
        assert_eq!(lock.constitution_digest, report.candidate_digest);
        let (recomputed, r, ri) = compute_constitution_digest(&candidate_text);
        assert_eq!(recomputed, lock.constitution_digest);
        assert_eq!((r, ri), (report.rule_enforcement_count, report.right_enforcement_count));
        // Stable fields inherited from the base lock.
        assert_eq!(lock.format_version, 1);
        assert_eq!(lock.constitution_id, "autonoetic-gateway-constitution");

        let provenance: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(report.candidate_dir.join("provenance.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(provenance["base_version"], "2026.01.01");
        assert_eq!(provenance["unsigned"], true);
        let props = provenance["proposals"].as_array().unwrap();
        assert_eq!(props.len(), 2);
        assert_eq!(props[0]["proposal_id"], "cprop-a");
        assert_eq!(props[0]["decided_by"], "operator");
        assert!(props[1]["after"].as_str().unwrap().contains("P-8.3"));
    }

    #[test]
    fn materialize_refuses_clobber_and_corrupt_base() {
        let temp = tempfile::tempdir().unwrap();
        let versions = temp.path().join("versions");
        let base_dir = versions.join("2026.01.01");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::write(base_dir.join("constitution.md"), BASE).unwrap();

        let (digest, rules, rights) = compute_constitution_digest(BASE);
        let p = [proposal("cprop-a", "modify_rule", Some("P-8.1"), Some("x"))];

        // A structurally invalid lock is refused at parse.
        std::fs::write(base_dir.join("gateway-constitution.lock.json"), "{}").unwrap();
        let err = materialize_candidate_version(&versions, "2026.01.01", "2026.01.02", &p)
            .unwrap_err();
        assert!(err.to_string().contains("must be valid JSON"), "{err}");

        // A well-formed lock with a digest the text doesn't reproduce is
        // refused — the active version must never be built on blind.
        let lock = serde_json::json!({
            "format_version": 1,
            "constitution_id": "c",
            "constitution_version": "2026.01.01",
            "constitution_source": "docs/constitution/versions/2026.01.01/constitution.md",
            "constitution_digest": "0000000000000000000000000000000000000000000000000000000000000000",
            "rule_enforcement_count": rules,
            "right_enforcement_count": rights,
            "canonicalization": {
                "algorithm": "sha256",
                "payload": "json({constitution_text,rights_enforcement,rules_enforcement})",
                "rules_prefix": "P-",
                "rights_prefix": "Ri-"
            }
        });
        std::fs::write(
            base_dir.join("gateway-constitution.lock.json"),
            serde_json::to_string_pretty(&lock).unwrap(),
        )
        .unwrap();
        let err = materialize_candidate_version(&versions, "2026.01.01", "2026.01.02", &p)
            .unwrap_err();
        assert!(err.to_string().contains("pinned digest"), "{err}");

        // Now a valid base, but an existing candidate dir must not be clobbered.
        let lock = serde_json::json!({
            "format_version": 1,
            "constitution_id": "c",
            "constitution_version": "2026.01.01",
            "constitution_source": "docs/constitution/versions/2026.01.01/constitution.md",
            "constitution_digest": digest,
            "rule_enforcement_count": rules,
            "right_enforcement_count": rights,
            "canonicalization": {
                "algorithm": "sha256",
                "payload": "json({constitution_text,rights_enforcement,rules_enforcement})",
                "rules_prefix": "P-",
                "rights_prefix": "Ri-"
            }
        });
        std::fs::write(
            base_dir.join("gateway-constitution.lock.json"),
            serde_json::to_string_pretty(&lock).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(versions.join("2026.01.02")).unwrap();
        let err = materialize_candidate_version(&versions, "2026.01.01", "2026.01.02", &p)
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        // Version traversal is refused.
        let err = materialize_candidate_version(&versions, "2026.01.01", "../evil", &p)
            .unwrap_err();
        assert!(err.to_string().contains("simple directory name"), "{err}");
    }
}
