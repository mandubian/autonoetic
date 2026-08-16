//! Live LLM stream activity registry (#1081 follow-up).
//!
//! The stall detector (`complete_with_stall_detection`, #1044) already knows
//! whether a turn's stream is queueing (`awaiting first byte`) or flowing
//! (`streaming`, chunk/char counters). This module publishes that in-process
//! state so operator surfaces — the Session Room TUI, `gateway status` — can
//! answer "is the LLM stuck or just slow?" without reading the log tail.
//!
//! Deliberately **ephemeral**: entries live exactly as long as the stream
//! (an RAII guard removes them on every exit path — completion, stall, driver
//! error, join failure). Nothing is persisted; terminal outcomes already have
//! durable representations (`llm exchange` log lines, turn-error timeline
//! rows). A registry read is a mutex-protected clone of plain counters —
//! cheap enough to poll at 1s from a TUI.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Who is running the turn — the join keys a surface needs to attribute a
/// live stream to a session-tree row.
#[derive(Debug, Clone)]
pub struct LlmTurnCtx {
    pub session_id: String,
    pub agent_id: String,
}

impl LlmTurnCtx {
    /// A context for call sites with no session to attribute (unit tests,
    /// auxiliary completions). Registers like any other; only surfaces in
    /// unfiltered snapshots.
    pub fn detached() -> Self {
        Self {
            session_id: "(detached)".to_string(),
            agent_id: "(detached)".to_string(),
        }
    }
}

/// A live stream's phase. Ordered so snapshot consumers can sort
/// `awaiting_first_byte` above `streaming` (the operator's eye goes to the
/// stuck-looking one first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmStreamPhase {
    AwaitingFirstByte,
    Streaming,
}

impl LlmStreamPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingFirstByte => "awaiting_first_byte",
            Self::Streaming => "streaming",
        }
    }
}

struct Entry {
    session_id: String,
    agent_id: String,
    model: String,
    phase: LlmStreamPhase,
    started_at: Instant,
    ttfb: Option<Duration>,
    chunks: u64,
    text_chars: u64,
    last_event_at: Instant,
    idle_budget: Duration,
}

impl Entry {
    fn snapshot(&self, id: u64) -> LlmActivitySnapshot {
        LlmActivitySnapshot {
            stream_id: id,
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            model: self.model.clone(),
            phase: self.phase,
            elapsed_ms: self.started_at.elapsed().as_millis() as u64,
            ttfb_ms: self.ttfb.map(|d| d.as_millis() as u64),
            since_last_chunk_ms: self.last_event_at.elapsed().as_millis() as u64,
            chunks: self.chunks,
            text_chars: self.text_chars,
            idle_budget_ms: self.idle_budget.as_millis() as u64,
        }
    }
}

/// One live LLM stream, as seen by an operator surface. `elapsed_ms` /
/// `since_last_chunk_ms` are computed at snapshot time so a 1s poll shows
/// ticking clocks without the registry mutating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmActivitySnapshot {
    pub stream_id: u64,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
    pub phase: LlmStreamPhase,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_ms: Option<u64>,
    pub since_last_chunk_ms: u64,
    pub chunks: u64,
    pub text_chars: u64,
    /// The idle-gap budget in force — lets a surface flag
    /// `since_last_chunk_ms` approaching `idle_budget_ms` as "about to be
    /// declared stalled" rather than merely slow.
    pub idle_budget_ms: u64,
}

fn registry() -> &'static Mutex<HashMap<u64, Entry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<u64, Entry>> {
    // A panicked writer cannot leave meaningful state behind (updates are
    // counter bumps); recover rather than poison-cascade into every turn.
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII registration for one in-flight stream. Inserted at stream start;
/// removed on drop — every exit path of `complete_with_stall_detection`
/// (completion, stall, driver error, join failure) drops the guard.
pub struct LlmActivityGuard {
    id: u64,
}

impl LlmActivityGuard {
    /// Record the first received event (the TTFB discriminator). `text_chars`
    /// is the payload size of that event — a stream whose first event is a
    /// `TextDelta` (the common shape) must count those chars too, or the
    /// snapshot underreports. `idle_budget` is the budget now in force: the
    /// entry is registered with the TTFB budget (the wait for this very
    /// event); from here on inter-chunk silence is governed by the idle-gap
    /// budget, so the displayed "about to stall" denominator must swap.
    pub fn mark_first_byte(&self, text_chars: u64, idle_budget: Duration) {
        let mut map = lock();
        if let Some(e) = map.get_mut(&self.id) {
            e.phase = LlmStreamPhase::Streaming;
            e.ttfb = Some(e.started_at.elapsed());
            e.last_event_at = Instant::now();
            e.chunks += 1;
            e.text_chars += text_chars;
            e.idle_budget = idle_budget;
        }
    }

    /// Record a subsequent event (any chunk: text delta, tool-use frame…).
    pub fn mark_event(&self, text_chars: u64) {
        let mut map = lock();
        if let Some(e) = map.get_mut(&self.id) {
            e.phase = LlmStreamPhase::Streaming;
            e.last_event_at = Instant::now();
            e.chunks += 1;
            e.text_chars += text_chars;
        }
    }
}

impl Drop for LlmActivityGuard {
    fn drop(&mut self) {
        lock().remove(&self.id);
    }
}

/// Register a stream as started (phase: awaiting first byte). `idle_budget`
/// is the budget in force at registration — the TTFB budget; the stall
/// detector swaps it for the idle-gap budget via
/// [`LlmActivityGuard::mark_first_byte`] when the first event arrives.
pub fn begin_activity(ctx: &LlmTurnCtx, model: &str, idle_budget: Duration) -> LlmActivityGuard {
    let id = next_id();
    let now = Instant::now();
    lock().insert(
        id,
        Entry {
            session_id: ctx.session_id.clone(),
            agent_id: ctx.agent_id.clone(),
            model: model.to_string(),
            phase: LlmStreamPhase::AwaitingFirstByte,
            started_at: now,
            ttfb: None,
            chunks: 0,
            text_chars: 0,
            last_event_at: now,
            idle_budget,
        },
    );
    LlmActivityGuard { id }
}

/// All live streams. Sorted stuck-first (awaiting first byte), then by age.
pub fn snapshot() -> Vec<LlmActivitySnapshot> {
    let mut out: Vec<LlmActivitySnapshot> = lock().iter().map(|(id, e)| e.snapshot(*id)).collect();
    out.sort_by(|a, b| a.phase.cmp(&b.phase).then(b.elapsed_ms.cmp(&a.elapsed_ms)));
    out
}

/// Live streams belonging to a root session tree — `session_id == root` or
/// `session_id.starts_with(root + "/")`, the same subtree convention as the
/// timeline and verdict tooling.
pub fn snapshot_for_root(root: &str) -> Vec<LlmActivitySnapshot> {
    let prefix = format!("{root}/");
    snapshot()
        .into_iter()
        .filter(|s| s.session_id == root || s.session_id.starts_with(&prefix))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(session: &str) -> LlmTurnCtx {
        LlmTurnCtx {
            session_id: session.to_string(),
            agent_id: "test.agent".to_string(),
        }
    }

    #[test]
    fn guard_registers_and_drops() {
        // The registry is process-global and lib tests run in parallel, so
        // assert on *this* stream's presence, not global counts.
        {
            let _g = begin_activity(&ctx("s1"), "m", Duration::from_secs(10));
            let s = snapshot()
                .into_iter()
                .find(|s| s.session_id == "s1")
                .expect("stream must be registered while the guard lives");
            assert_eq!(s.phase, LlmStreamPhase::AwaitingFirstByte);
            assert_eq!(s.chunks, 0);
            assert!(s.ttfb_ms.is_none());
        }
        assert!(
            !snapshot().iter().any(|s| s.session_id == "s1"),
            "stream must be deregistered on guard drop"
        );
    }

    #[test]
    fn marks_advance_phase_and_counters() {
        let g = begin_activity(&ctx("s2"), "m", Duration::from_secs(10));
        // First event is a TextDelta (common shape): its chars must count.
        g.mark_first_byte(50, Duration::from_secs(10));
        g.mark_event(120);
        g.mark_event(80);
        let s = snapshot().into_iter().find(|s| s.session_id == "s2").unwrap();
        assert_eq!(s.phase, LlmStreamPhase::Streaming);
        assert_eq!(s.chunks, 3);
        assert_eq!(s.text_chars, 250);
        assert!(s.ttfb_ms.is_some());
        drop(g);
    }

    #[test]
    fn first_byte_swaps_displayed_budget() {
        // Registered with the TTFB budget (the wait for the first event);
        // once the first byte lands the idle-gap budget governs, and the
        // snapshot's "about to stall" denominator must reflect that.
        let g = begin_activity(&ctx("s-budget"), "m", Duration::from_secs(600));
        let s = snapshot()
            .into_iter()
            .find(|s| s.session_id == "s-budget")
            .unwrap();
        assert_eq!(s.idle_budget_ms, 600_000);
        g.mark_first_byte(0, Duration::from_secs(120));
        let s = snapshot()
            .into_iter()
            .find(|s| s.session_id == "s-budget")
            .unwrap();
        assert_eq!(s.idle_budget_ms, 120_000);
        drop(g);
    }

    #[test]
    fn root_filter_matches_subtree_only() {
        let g1 = begin_activity(&ctx("rootA"), "m", Duration::from_secs(10));
        let g2 = begin_activity(&ctx("rootA/child1"), "m", Duration::from_secs(10));
        let _g3 = begin_activity(&ctx("rootB"), "m", Duration::from_secs(10));
        let _g4 = begin_activity(&ctx("rootAX"), "m", Duration::from_secs(10));
        let ids: Vec<String> = snapshot_for_root("rootA")
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert!(ids.contains(&"rootA".to_string()));
        assert!(ids.contains(&"rootA/child1".to_string()));
        assert!(!ids.contains(&"rootB".to_string()));
        assert!(!ids.contains(&"rootAX".to_string())); // prefix ≠ subtree
        drop((g1, g2));
    }

    #[test]
    fn awaiting_sorts_above_streaming() {
        let g1 = begin_activity(&ctx("s-flow"), "m", Duration::from_secs(10));
        g1.mark_first_byte(0, Duration::from_secs(10));
        let g2 = begin_activity(&ctx("s-wait"), "m", Duration::from_secs(10));
        let snaps = snapshot();
        let wait_first = snaps
            .iter()
            .find(|s| s.session_id == "s-wait")
            .map(|s| snaps.iter().position(|x| x.stream_id == s.stream_id).unwrap())
            .unwrap();
        let flow_pos = snaps
            .iter()
            .position(|s| s.session_id == "s-flow")
            .unwrap();
        assert!(wait_first < flow_pos);
        drop((g1, g2));
    }
}
