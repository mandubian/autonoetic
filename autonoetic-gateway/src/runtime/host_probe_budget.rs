//! Per-session, per-host `sandbox_exec` probe budget (issue #853).
//!
//! When an agent researches an unextractable target (a JS-heavy SPA, a
//! rate-limited endpoint, an intermittently-failing host) it tends to retry the
//! *same* host with a stream of slightly-different scripts. The rotating-poll
//! detector in [`crate::runtime::guard`] is keyed on `(tool, normalized_args)`
//! fingerprints, so a new script each turn slips past it — the agent can burn
//! dozens of `sandbox_exec` calls against one host that only ever returns the
//! same SPA shell. Prose guidance in a SKILL.md ("stop after two failed fetches
//! for the same host") is a suggestion the model treats as optional.
//!
//! This module is a mechanical bound of the same family as the P-7.17 approval
//! flood cap and the #770 anomaly-flag flood cap: an agent can spam one
//! resource category faster than an operator can notice, so the gateway caps it
//! *loudly*.
//!
//! ## Model
//!
//! Per `(session_id, host)` we track a **strike** count and the set of content
//! hashes already seen from that host:
//!
//! - A **failed** probe (`ok == false`) is a strike.
//! - A **duplicate-content** probe (a success whose stdout hash was already
//!   seen from this host) is a strike — this is the case pure failure-counting
//!   misses, and the one that actually bit in `session-0718349d`, where every
//!   fetch returned exit 0 with the identical SvelteKit HTML.
//! - A **novel** success (new content hash) is *progress*: it resets the strike
//!   count to zero. So the budget bounds "wasted probes since the last new
//!   information", never a host that keeps yielding something new.
//!
//! When a host accumulates `max_probes_per_host` strikes, the *next*
//! `sandbox_exec` targeting it is refused with a `host_budget_exhausted` error
//! telling the agent to switch sources or return `status: partial`.
//!
//! ## Scope
//!
//! Strictly per exact `session_id`. A host exhausted for one researcher is
//! still available to a re-spawn with different instructions — the legitimate
//! recovery path. Held in memory on [`crate::scheduler::gateway_store`]
//! (surviving turns and suspend/resume within a gateway process, reset on
//! restart, like the flood-cap alert sets) rather than persisted; a fresh
//! budget after a gateway restart is acceptable degradation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

/// Upper bound on distinct hosts tracked per session. Beyond this the budget
/// simply stops applying to further novel hosts — a guard against a
/// prompt-injected agent inflating memory with thousands of distinct hosts.
const MAX_TRACKED_HOSTS_PER_SESSION: usize = 512;
/// Upper bound on distinct content hashes retained per host. Beyond this we
/// stop recording new hashes; very old content may then no longer be detected
/// as a duplicate, which only ever *under*-counts strikes (fail-open).
const MAX_TRACKED_HASHES_PER_HOST: usize = 64;

/// Outcome of recording one probe against a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Novel successful probe — new information. Strike count reset to zero.
    Progress,
    /// A wasted probe (failed, or duplicate content). Carries the new strike
    /// count and whether it just reached the cap (the trip edge, for
    /// once-per-host operator alerting).
    Strike {
        strikes: u32,
        duplicate: bool,
        reached_cap: bool,
    },
    /// The budget is disabled (`max_probes_per_host == 0`) — nothing tracked.
    Disabled,
}

#[derive(Default)]
struct HostEntry {
    strikes: u32,
    seen_hashes: HashSet<String>,
}

#[derive(Default)]
struct SessionHostBudget {
    hosts: HashMap<String, HostEntry>,
}

/// Registry of per-session host-probe budgets, shared on `GatewayStore`.
/// Cloning returns another handle to the same underlying map.
#[derive(Clone)]
pub struct HostProbeBudgetRegistry {
    inner: Arc<Mutex<HashMap<String, SessionHostBudget>>>,
    /// `max_probes_per_host`; `0` disables the budget. Atomic so it can be set
    /// once at daemon startup (tests open the store with the default of 0).
    cap: Arc<AtomicUsize>,
}

impl Default for HostProbeBudgetRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            cap: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// Stable content hash for a probe's output (hex sha256). Kept short-lived and
/// per-host, so a full digest is fine and collision-free in practice.
pub fn content_hash(output: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(output.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Stable machine-readable code (P-5.11) on the rejection a `sandbox_exec`
/// probe gets when its host's budget is exhausted. Consumers (and the agent's
/// own error handling) match on this rather than parsing the message string.
pub const HOST_BUDGET_EXHAUSTED_CODE: &str = "host_budget_exhausted";

/// Build the `sandbox_exec` rejection returned when a host's probe budget is
/// exhausted: a `quota_exceeded` tool error carrying [`HOST_BUDGET_EXHAUSTED_CODE`]
/// and a remedy pointing the agent at a different source / `status: partial`.
pub fn host_budget_exhausted_response(host: &str, strikes: u32, cap: usize) -> String {
    autonoetic_types::tool_error::ToolError::quota_exceeded(
        format!(
            "host {host} has been probed {strikes} time(s) this session without new \
             information — the per-host fetch budget (max_probes_per_host={cap}) is \
             exhausted for it"
        ),
        Some(format!(
            "Stop retrying {host}. Switch to a different source/host, or return \
             status: partial with what you already have. A re-spawn with different \
             instructions gets a fresh budget."
        )),
    )
    .with_code(HOST_BUDGET_EXHAUSTED_CODE)
    .to_error_response()
}

impl HostProbeBudgetRegistry {
    /// Set the per-host strike cap (`0` disables). Called once at startup from
    /// `config.max_probes_per_host`.
    pub fn set_cap(&self, cap: usize) {
        self.cap.store(cap, Ordering::Relaxed);
    }

    /// The current cap (`0` = disabled).
    pub fn cap(&self) -> usize {
        self.cap.load(Ordering::Relaxed)
    }

    /// Pre-execution check: if `host` has already struck `cap` times this
    /// session, return `Some(strikes)` — the caller must refuse the probe.
    /// Returns `None` when the budget is disabled or the host is under budget.
    pub fn exhausted(&self, session_id: &str, host: &str) -> Option<u32> {
        let cap = self.cap();
        if cap == 0 {
            return None;
        }
        let guard = self.inner.lock().ok()?;
        let strikes = guard.get(session_id)?.hosts.get(host)?.strikes;
        if (strikes as usize) >= cap {
            Some(strikes)
        } else {
            None
        }
    }

    /// Post-execution update: record one probe of `host` and classify it.
    ///
    /// - `ok`: whether the `sandbox_exec` ran the command to a clean exit.
    /// - `output_hash`: [`content_hash`] of the probe's stdout (only consulted
    ///   for successful probes; a failure is a strike regardless of output).
    pub fn record(
        &self,
        session_id: &str,
        host: &str,
        ok: bool,
        output_hash: &str,
    ) -> ProbeOutcome {
        let cap = self.cap();
        if cap == 0 {
            return ProbeOutcome::Disabled;
        }
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return ProbeOutcome::Disabled,
        };
        let session = guard.entry(session_id.to_string()).or_default();
        // Bound distinct hosts. An untracked novel host is treated as progress
        // (fail-open) so we never wrongly refuse it later.
        if !session.hosts.contains_key(host)
            && session.hosts.len() >= MAX_TRACKED_HOSTS_PER_SESSION
        {
            return ProbeOutcome::Progress;
        }
        let entry = session.hosts.entry(host.to_string()).or_default();

        if ok && !entry.seen_hashes.contains(output_hash) {
            // Novel information — progress. Reset strikes; remember the hash.
            entry.strikes = 0;
            if entry.seen_hashes.len() < MAX_TRACKED_HASHES_PER_HOST {
                entry.seen_hashes.insert(output_hash.to_string());
            }
            return ProbeOutcome::Progress;
        }

        // Failed, or a success repeating content already seen from this host.
        let duplicate = ok;
        entry.strikes = entry.strikes.saturating_add(1);
        let strikes = entry.strikes;
        ProbeOutcome::Strike {
            strikes,
            duplicate,
            reached_cap: (strikes as usize) == cap,
        }
    }

    /// Drop all budget state for a session (called on session end, mirroring
    /// session-grant cleanup). Safe to call for an untracked session.
    pub fn clear_session(&self, session_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(session_id);
        }
    }

    /// Test/diagnostic: current strike count for a `(session, host)`.
    pub fn strikes(&self, session_id: &str, host: &str) -> u32 {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.get(session_id).and_then(|s| s.hosts.get(host).map(|h| h.strikes)))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "root-x/researcher.default-abcd1234";
    const H: &str = "open-meteo.com";

    fn reg(cap: usize) -> HostProbeBudgetRegistry {
        let r = HostProbeBudgetRegistry::default();
        r.set_cap(cap);
        r
    }

    #[test]
    fn disabled_cap_never_tracks_or_trips() {
        let r = reg(0);
        for _ in 0..10 {
            assert_eq!(r.record(S, H, false, ""), ProbeOutcome::Disabled);
        }
        assert_eq!(r.exhausted(S, H), None);
    }

    #[test]
    fn repeated_failures_strike_and_trip() {
        let r = reg(3);
        let h = content_hash("");
        assert!(matches!(
            r.record(S, H, false, &h),
            ProbeOutcome::Strike { strikes: 1, duplicate: false, reached_cap: false }
        ));
        assert_eq!(r.exhausted(S, H), None, "1 strike is under a cap of 3");
        assert!(matches!(
            r.record(S, H, false, &h),
            ProbeOutcome::Strike { strikes: 2, .. }
        ));
        assert!(matches!(
            r.record(S, H, false, &h),
            ProbeOutcome::Strike { strikes: 3, reached_cap: true, .. }
        ));
        // Third strike reaches the cap → the next probe must be refused.
        assert_eq!(r.exhausted(S, H), Some(3));
    }

    #[test]
    fn duplicate_success_content_strikes() {
        // The session-0718349d shape: every fetch exits 0 but returns the same
        // SPA HTML. First is novel (progress); repeats are strikes.
        let r = reg(3);
        let spa = content_hash("<!doctype html><div id=svelte>…</div>");
        assert_eq!(r.record(S, H, true, &spa), ProbeOutcome::Progress);
        assert!(matches!(
            r.record(S, H, true, &spa),
            ProbeOutcome::Strike { strikes: 1, duplicate: true, .. }
        ));
        assert!(matches!(r.record(S, H, true, &spa), ProbeOutcome::Strike { strikes: 2, .. }));
        assert!(matches!(
            r.record(S, H, true, &spa),
            ProbeOutcome::Strike { strikes: 3, reached_cap: true, .. }
        ));
        assert_eq!(r.exhausted(S, H), Some(3));
    }

    #[test]
    fn novel_success_resets_strikes() {
        let r = reg(3);
        let a = content_hash("page-A");
        let b = content_hash("page-B");
        r.record(S, H, false, &a); // strike 1
        r.record(S, H, false, &a); // strike 2
        assert_eq!(r.strikes(S, H), 2);
        // A genuinely new page is progress and clears the debt.
        assert_eq!(r.record(S, H, true, &b), ProbeOutcome::Progress);
        assert_eq!(r.strikes(S, H), 0);
        assert_eq!(r.exhausted(S, H), None);
    }

    #[test]
    fn budget_is_per_session_and_per_host() {
        let r = reg(2);
        let h = content_hash("x");
        r.record(S, H, false, &h);
        r.record(S, H, false, &h);
        assert_eq!(r.exhausted(S, H), Some(2));
        // Different host in the same session is independent.
        assert_eq!(r.exhausted(S, "api.open-meteo.com"), None);
        // Different session (e.g. a re-spawn) starts fresh — the recovery path.
        assert_eq!(r.exhausted("root-x/researcher.default-zzzz9999", H), None);
    }

    #[test]
    fn exhausted_response_carries_stable_code() {
        // The rejection must expose `host_budget_exhausted` as the stable
        // machine-readable `error` code (P-5.11), not only in the message —
        // docs and the researcher SKILL.md tell agents to match on it.
        let json = host_budget_exhausted_response("open-meteo.com", 3, 3);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error_type"], "quota_exceeded");
        assert_eq!(v["error"], HOST_BUDGET_EXHAUSTED_CODE);
        assert!(
            v["message"].as_str().unwrap_or("").contains("open-meteo.com"),
            "message should name the exhausted host"
        );
    }

    #[test]
    fn clear_session_frees_the_budget() {
        let r = reg(2);
        let h = content_hash("x");
        r.record(S, H, false, &h);
        r.record(S, H, false, &h);
        assert_eq!(r.exhausted(S, H), Some(2));
        r.clear_session(S);
        assert_eq!(r.exhausted(S, H), None);
        assert_eq!(r.strikes(S, H), 0);
    }
}
