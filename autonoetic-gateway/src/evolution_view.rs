//! The standing view of in-flight skill work (#818): what has been proposed for
//! a durable home, what was decided, and what is waiting to be promoted.
//!
//! Both halves of the loop record their decisions as global knowledge entries —
//! the curator's `promote_to_skill` graduations in `evolution/graduations`, the
//! crystallizer's verdicts in `evolution/crystallizations`, the steward's
//! judgments under `steward.graduation.<id>` — and the enacted result shows up as
//! a Candidate revision waiting on the promotion gate. Until now an operator
//! could only see that by running `scripts/evolution_digest.py` against the
//! SQLite file, so work that stalled mid-pipeline was effectively invisible.
//!
//! This module assembles those records into one list. It is **reporting, not
//! inference**: every field comes from a recorded entry, and a proposal with no
//! recorded decision is reported as `proposed` rather than guessed at. It lives
//! outside `router.rs` so it is unit-testable against a store directly, and so
//! the router's already-oversized dispatch frame does not grow (#884).

use anyhow::Result;
use std::collections::HashMap;

use autonoetic_types::agent_revision::AgentRevisionStatus;
use serde::{Deserialize, Serialize};

use crate::scheduler::gateway_store::GatewayStore;

/// Knowledge tags each kind of record carries. Written by the curator
/// (`curator_journal.rs`), the crystallizer, and the steward respectively.
const TAG_CRYSTALLIZATION: &str = "type:crystallization_verdict";
const TAG_GRADUATION: &str = "type:promote_to_skill";
const TAG_GRADUATION_SKIPPED: &str = "type:graduation_skipped";
/// Tag the steward writes on its decisions. The view resolves decisions by id
/// rather than by tag, so this is referenced only by tests that seed realistic
/// records.
#[cfg(test)]
const TAG_STEWARD_DECISION: &str = "lesson_graduation";

/// How far a proposal has got, derived only from what is on record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Recorded by its proposer; no decision found yet.
    Proposed,
    /// The steward recorded a judgment (see `outcome` for which).
    Judged,
    /// The proposal was dropped before judgment, with a reason on record.
    Skipped,
    /// The proposer decided against it (crystallizer verdict `none`).
    Declined,
}

/// One row of the standing view.
#[derive(Debug, Clone, Serialize)]
pub struct PendingEntry {
    /// Which record this row came from.
    pub kind: &'static str,
    /// Knowledge entry id — the handle to look the full record up by.
    pub id: String,
    /// Agent the work targets, when the record names one.
    pub target_agent: Option<String>,
    /// One line an operator can read: tactic title, instruction, or skip reason.
    pub summary: String,
    /// Crystallizer routing verdict (`graduate` / `adapt` / `crystallize` /
    /// `none`); absent for curator graduations, which have no verdict of their own.
    pub verdict: Option<String>,
    pub stage: Stage,
    /// Steward outcome when one is recorded: `landed`, `covered`, `rejected`,
    /// `factory_gate_failed`.
    pub outcome: Option<String>,
    pub recorded_at: String,
    /// Candidate revisions of `target_agent` that the promotion gate is holding.
    /// All of that agent's Candidates, not only ones this proposal produced —
    /// revisions carry no proposal id yet (#891).
    pub target_agent_candidates: Vec<String>,
}

/// Steward decision parsed out of a `steward.graduation.<id>` entry.
#[derive(Debug, Deserialize)]
struct StewardDecision {
    #[serde(default)]
    status: Option<String>,
}

fn json_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// First line of `text`, clipped to `max` chars so one row stays one row.
fn one_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let clipped: String = line.chars().take(max.saturating_sub(1)).collect();
    format!("{clipped}…")
}

/// Steward outcome for the lesson identified by `knowledge_entry_id`, if the
/// steward has recorded one. Keyed by the entry id the steward writes under
/// (`steward.graduation.<knowledge_entry_id>`), which is also the id the
/// crystallizer passes through as its `proposal_id`.
///
/// A **point lookup**, not a search over a recent-decisions window: with a window,
/// a decision older than the window's tail would be missed and the row reported as
/// `proposed` — the view claiming nobody had decided when someone had. Bounded by
/// the number of rows (≤ 200), so the cost is the same order as listing them.
fn steward_outcome(store: &GatewayStore, knowledge_entry_id: &str) -> Option<String> {
    let entry = store
        .memory_get_unrestricted(&format!("steward.graduation.{knowledge_entry_id}"))
        .ok()
        .flatten()?;
    let parsed: StewardDecision = serde_json::from_str(&entry.content).ok()?;
    parsed.status.filter(|s| !s.trim().is_empty())
}

/// Candidate revisions of `agent_id`, memoized so each agent is queried once
/// however many rows target it.
///
/// These are **all** Candidates of that agent, not the ones a specific proposal
/// produced: a revision carries no proposal id today, so nothing links the two.
/// Two proposals against the same agent therefore show the same list. The field is
/// named `target_agent_candidates` to say so — attributing a Candidate to a
/// proposal would be a guess, and #891 tracks recording the provenance that would
/// make it a fact.
fn target_agent_candidates<'a>(
    store: &GatewayStore,
    cache: &'a mut HashMap<String, Vec<String>>,
    agent_id: &str,
) -> &'a Vec<String> {
    cache.entry(agent_id.to_string()).or_insert_with(|| {
        store
            .list_agent_revisions(agent_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.status == AgentRevisionStatus::Candidate)
            .map(|r| r.revision_id)
            .collect()
    })
}

/// Assemble the view. `limit` bounds each record kind independently, so a flood
/// of graduations cannot hide every crystallization.
pub fn pending_view(store: &GatewayStore, limit: usize) -> Result<serde_json::Value> {
    let limit = limit.clamp(1, 200);

    let crystallizations = store.search_memories_by_tags(&[TAG_CRYSTALLIZATION], limit)?;
    let graduations = store.search_memories_by_tags(&[TAG_GRADUATION], limit)?;
    let skipped = store.search_memories_by_tags(&[TAG_GRADUATION_SKIPPED], limit)?;

    let mut entries: Vec<PendingEntry> = Vec::new();
    // One revision query per distinct target agent, not per row.
    let mut candidate_cache: HashMap<String, Vec<String>> = HashMap::new();

    for m in &crystallizations {
        let content: serde_json::Value =
            serde_json::from_str(&m.content).unwrap_or(serde_json::Value::Null);
        let verdict = json_str(&content, "verdict");
        let target_agent =
            json_str(&content, "target_agent").or_else(|| json_str(&content, "target"));
        let summary = content
            .get("tactic")
            .and_then(|t| t.get("title"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| json_str(&content, "rationale"))
            .unwrap_or_else(|| one_line(&m.content, 120));
        let outcome = steward_outcome(store, &m.memory_id);
        let stage = match (verdict.as_deref(), outcome.is_some()) {
            (Some("none"), _) => Stage::Declined,
            (_, true) => Stage::Judged,
            _ => Stage::Proposed,
        };
        entries.push(PendingEntry {
            kind: "crystallization",
            id: m.memory_id.clone(),
            target_agent_candidates: target_agent
                .as_deref()
                .map(|a| target_agent_candidates(store, &mut candidate_cache, a).clone())
                .unwrap_or_default(),
            target_agent,
            summary: one_line(&summary, 120),
            verdict,
            stage,
            outcome,
            recorded_at: m.created_at.clone(),
        });
    }

    for m in &graduations {
        let content: serde_json::Value =
            serde_json::from_str(&m.content).unwrap_or(serde_json::Value::Null);
        let target_agent = json_str(&content, "target_agent");
        // The curator records the lesson's own knowledge id; the steward keys its
        // decision on that, not on this entry's id.
        let lesson_id =
            json_str(&content, "knowledge_entry_id").unwrap_or_else(|| m.memory_id.clone());
        let outcome = steward_outcome(store, &lesson_id);
        let summary =
            json_str(&content, "proposed_instruction").unwrap_or_else(|| one_line(&m.content, 120));
        entries.push(PendingEntry {
            kind: "graduation",
            id: m.memory_id.clone(),
            target_agent_candidates: target_agent
                .as_deref()
                .map(|a| target_agent_candidates(store, &mut candidate_cache, a).clone())
                .unwrap_or_default(),
            target_agent,
            summary: one_line(&summary, 120),
            verdict: None,
            stage: if outcome.is_some() {
                Stage::Judged
            } else {
                Stage::Proposed
            },
            outcome,
            recorded_at: m.created_at.clone(),
        });
    }

    for m in &skipped {
        let content: serde_json::Value =
            serde_json::from_str(&m.content).unwrap_or(serde_json::Value::Null);
        let summary = json_str(&content, "skip_reason")
            .or_else(|| json_str(&content, "proposed_instruction"))
            .unwrap_or_else(|| one_line(&m.content, 120));
        entries.push(PendingEntry {
            kind: "graduation_skipped",
            id: m.memory_id.clone(),
            target_agent: json_str(&content, "target_agent"),
            summary: one_line(&summary, 120),
            verdict: None,
            stage: Stage::Skipped,
            outcome: None,
            recorded_at: m.created_at.clone(),
            target_agent_candidates: Vec::new(),
        });
    }

    // Newest first: an operator asking "what is in flight" means "what just
    // happened", and a stalled proposal keeps its place as newer ones arrive.
    entries.sort_by(|a, b| b.recorded_at.cmp(&a.recorded_at));

    let counts = serde_json::json!({
        "crystallizations": crystallizations.len(),
        "graduations": graduations.len(),
        "skipped": skipped.len(),
        "awaiting_promotion": entries
            .iter()
            .filter(|e| !e.target_agent_candidates.is_empty())
            .count(),
    });

    Ok(serde_json::json!({
        "pending": entries,
        "counts": counts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};

    fn store() -> (tempfile::TempDir, GatewayStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GatewayStore::open(dir.path()).expect("store");
        (dir, store)
    }

    fn put(store: &GatewayStore, id: &str, scope: &str, tags: &[&str], content: serde_json::Value) {
        let mut m = MemoryObject::new(
            id.to_string(),
            scope.to_string(),
            "test-agent".to_string(),
            "test-agent".to_string(),
            "session:test:io.returns".to_string(),
            content.to_string(),
        );
        m.source_type = MemorySourceType::AgentWrite;
        m.visibility = MemoryVisibility::Global;
        m.tags = tags.iter().map(|t| t.to_string()).collect();
        store.memory_upsert(&m).expect("upsert");
    }

    fn rows(v: &serde_json::Value) -> Vec<serde_json::Value> {
        v["pending"].as_array().cloned().unwrap_or_default()
    }

    fn row_of<'a>(rows: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
        rows.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("row '{id}' should be listed"))
    }

    #[test]
    fn empty_store_reports_nothing_rather_than_failing() {
        let (_d, store) = store();
        let v = pending_view(&store, 20).expect("view");
        assert!(rows(&v).is_empty());
        assert_eq!(v["counts"]["crystallizations"], 0);
    }

    #[test]
    fn crystallization_without_a_decision_is_proposed() {
        let (_d, store) = store();
        put(
            &store,
            "crys-1",
            "evolution/crystallizations",
            &[TAG_CRYSTALLIZATION],
            serde_json::json!({
                "verdict": "crystallize",
                "rationale": "nothing installed covers it",
                "tactic": { "title": "probe, back off on 429, then batch" },
                "target_agent": "batch-fetcher.default"
            }),
        );
        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        let r = row_of(&listed, "crys-1");
        assert_eq!(r["kind"], "crystallization");
        assert_eq!(r["verdict"], "crystallize");
        assert_eq!(r["stage"], "proposed");
        assert_eq!(r["summary"], "probe, back off on 429, then batch");
        assert!(r["outcome"].is_null());
    }

    /// The crystallizer's own `none` verdict is a decision, not a stall — an
    /// operator should not see it sitting in the list as if it were waiting.
    #[test]
    fn declined_crystallization_is_not_reported_as_pending_work() {
        let (_d, store) = store();
        put(
            &store,
            "crys-none",
            "evolution/crystallizations",
            &[TAG_CRYSTALLIZATION],
            serde_json::json!({
                "verdict": "none",
                "rationale": "single session, no recurrence evidence",
                "skip_reason": "single_session"
            }),
        );
        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        assert_eq!(row_of(&listed, "crys-none")["stage"], "declined");
    }

    /// The steward's judgment is keyed on the lesson id, which for a
    /// crystallization is the verdict entry's own id.
    #[test]
    fn steward_decision_advances_the_stage_and_carries_its_outcome() {
        let (_d, store) = store();
        put(
            &store,
            "crys-2",
            "evolution/crystallizations",
            &[TAG_CRYSTALLIZATION],
            serde_json::json!({
                "verdict": "graduate",
                "tactic": { "title": "always seal before running main.py" },
                "target_agent": "coder.default"
            }),
        );
        put(
            &store,
            "steward.graduation.crys-2",
            "evolution",
            &[TAG_STEWARD_DECISION],
            serde_json::json!({ "status": "landed", "target_agent": "coder.default" }),
        );
        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        let r = row_of(&listed, "crys-2");
        assert_eq!(r["stage"], "judged");
        assert_eq!(r["outcome"], "landed");
    }

    /// A curator graduation records the *lesson's* id in its content, and the
    /// steward keys on that rather than on the graduation entry's own id.
    #[test]
    fn graduation_outcome_is_matched_through_the_lesson_id() {
        let (_d, store) = store();
        put(
            &store,
            "grad-planner.default-lesson-77",
            "evolution/graduations",
            &[TAG_GRADUATION],
            serde_json::json!({
                "target_agent": "planner.default",
                "proposed_instruction": "Never call sandbox_exec directly; delegate via agent_spawn.",
                "knowledge_entry_id": "lesson-77"
            }),
        );
        put(
            &store,
            "steward.graduation.lesson-77",
            "evolution",
            &[TAG_STEWARD_DECISION],
            serde_json::json!({ "status": "covered" }),
        );
        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        let r = row_of(&listed, "grad-planner.default-lesson-77");
        assert_eq!(r["kind"], "graduation");
        assert_eq!(r["target_agent"], "planner.default");
        assert_eq!(r["stage"], "judged");
        assert_eq!(r["outcome"], "covered");
        assert!(r["summary"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Never call sandbox_exec"));
    }

    #[test]
    fn skipped_graduation_reports_its_reason() {
        let (_d, store) = store();
        put(
            &store,
            "grad-skip-1",
            "evolution/graduations",
            &[TAG_GRADUATION_SKIPPED],
            serde_json::json!({
                "target_agent": "auditor.default",
                "skip_reason": "target_agent is in the exempt agents list"
            }),
        );
        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        let r = row_of(&listed, "grad-skip-1");
        assert_eq!(r["stage"], "skipped");
        assert_eq!(r["summary"], "target_agent is in the exempt agents list");
    }

    /// Malformed content must not take the whole view down — an operator locked
    /// out of the listing by one bad entry has no way to find the bad entry.
    #[test]
    fn malformed_content_degrades_to_a_raw_summary() {
        let (_d, store) = store();
        let mut m = MemoryObject::new(
            "crys-bad".to_string(),
            "evolution/crystallizations".to_string(),
            "a".to_string(),
            "a".to_string(),
            "s".to_string(),
            "not json at all\nsecond line".to_string(),
        );
        m.visibility = MemoryVisibility::Global;
        m.tags = vec![TAG_CRYSTALLIZATION.to_string()];
        store.memory_upsert(&m).expect("upsert");

        let v = pending_view(&store, 20).expect("view should still build");
        let listed = rows(&v);
        let r = row_of(&listed, "crys-bad");
        assert_eq!(r["summary"], "not json at all");
        assert_eq!(r["stage"], "proposed");
    }

    #[test]
    fn rows_are_newest_first() {
        let (_d, store) = store();
        for id in ["crys-old", "crys-new"] {
            put(
                &store,
                id,
                "evolution/crystallizations",
                &[TAG_CRYSTALLIZATION],
                serde_json::json!({ "verdict": "adapt", "tactic": { "title": id } }),
            );
        }
        // Force distinguishable timestamps.
        let mut older = store
            .memory_get_unrestricted("crys-old")
            .unwrap()
            .expect("stored");
        older.created_at = "2020-01-01T00:00:00Z".to_string();
        store.memory_upsert(&older).unwrap();

        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        assert_eq!(listed.first().map(|r| r["id"].clone()).unwrap(), "crys-new");
        assert_eq!(listed.last().map(|r| r["id"].clone()).unwrap(), "crys-old");
    }

    /// The decision lookup must not depend on how many other decisions exist.
    /// It used to search a bounded window of recent `lesson_graduation` records,
    /// so a decision older than that window was missed and the row reported as
    /// `proposed` — the view claiming nobody had decided when someone had
    /// (#889 review).
    #[test]
    fn decision_is_found_behind_many_newer_decisions() {
        let (_d, store) = store();

        put(
            &store,
            "crys-old-decision",
            "evolution/crystallizations",
            &[TAG_CRYSTALLIZATION],
            serde_json::json!({
                "verdict": "graduate",
                "tactic": { "title": "decided long ago" },
                "target_agent": "coder.default"
            }),
        );
        // Its decision is the oldest record of its kind…
        put(
            &store,
            "steward.graduation.crys-old-decision",
            "evolution",
            &[TAG_STEWARD_DECISION],
            serde_json::json!({ "status": "landed" }),
        );
        let mut old = store
            .memory_get_unrestricted("steward.graduation.crys-old-decision")
            .unwrap()
            .expect("stored");
        old.created_at = "2020-01-01T00:00:00Z".to_string();
        store.memory_upsert(&old).unwrap();

        // …and 60 newer decisions bury it, well beyond any recent-window bound.
        for i in 0..60 {
            put(
                &store,
                &format!("steward.graduation.other-{i}"),
                "evolution",
                &[TAG_STEWARD_DECISION],
                serde_json::json!({ "status": "covered" }),
            );
        }

        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        let r = row_of(&listed, "crys-old-decision");
        assert_eq!(r["stage"], "judged", "the decision must still be found");
        assert_eq!(r["outcome"], "landed");
    }

    /// Candidates are reported per target agent, not per proposal: nothing links a
    /// revision to the proposal that caused it yet (#891). Two proposals against
    /// one agent therefore show the same list — pinned here so the field is not
    /// later misread as attribution.
    #[test]
    fn candidates_are_reported_per_agent_not_per_proposal() {
        use autonoetic_types::agent_revision::AgentRevisionRecord;
        use autonoetic_types::principal::PrincipalKind;

        let (_d, store) = store();
        for id in ["crys-a", "crys-b"] {
            put(
                &store,
                id,
                "evolution/crystallizations",
                &[TAG_CRYSTALLIZATION],
                serde_json::json!({
                    "verdict": "graduate",
                    "tactic": { "title": id },
                    "target_agent": "coder.default"
                }),
            );
        }
        let rec = AgentRevisionRecord {
            revision_id: "rev_sha256:cand".to_string(),
            agent_id: "coder.default".to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: "sha256:cand".to_string(),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: PrincipalKind::AutonoeticAgent.tag().to_string(),
            created_by_id: "specialized_builder.default".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: AgentRevisionStatus::Candidate,
            metadata_json: serde_json::json!({}),
            short_id: "cand".to_string(),
            detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store.insert_agent_revision(&rec).expect("candidate");

        let v = pending_view(&store, 20).expect("view");
        let listed = rows(&v);
        let expected = serde_json::json!(["rev_sha256:cand"]);
        assert_eq!(
            row_of(&listed, "crys-a")["target_agent_candidates"],
            expected
        );
        assert_eq!(
            row_of(&listed, "crys-b")["target_agent_candidates"],
            expected
        );
    }

    #[test]
    fn one_line_clips_to_a_single_line() {
        assert_eq!(one_line("first\nsecond", 100), "first");
        assert_eq!(one_line("  \n  kept  \n", 100), "kept");
        assert_eq!(
            one_line(&"x".repeat(200), 10),
            format!("{}…", "x".repeat(9))
        );
    }
}
