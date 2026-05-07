//! `SentinelRunner` — orchestrates a deterministic Phase-1 and heuristic Phase-2 sweep.
//!
//! A sweep runs all configured checks, persists findings to the
//! `security_findings` table, and returns a summary. Callers can supply a
//! `since` timestamp to run incremental sweeps (only events after that point
//! are scanned).
//!
//! ## Phase 1 (deterministic)
//! Pure regex/SQL checks — findings may reach `critical` severity.
//!
//! ## Phase 2 (LLM-judgment heuristics)
//! Structural pattern matching that requires human or LLM reasoning to confirm.
//! Findings are `warning` severity with `llm_judgment` reproducibility.
//! The prompt-injection scan reads SKILL.md bodies from the filesystem via
//! `agents_dir` (optional — skipped if not set).
//!
//! NOTE: Emission of `security_finding_recorded` events to the causal chain is
//! planned for Phase 5 (scheduling integration). The current runner persists
//! findings to the `security_findings` table only.

use anyhow::Result;
use autonoetic_types::security::SecurityFinding;
use std::path::PathBuf;
use std::sync::Arc;

use crate::scheduler::gateway_store::GatewayStore;
use super::checks::{approval_bypass, capability_accretion, credential, prompt_injection, sandbox_escape, session_cluster};

/// Configuration for a sentinel sweep (Phase 1 + Phase 2).
pub struct SweepConfig {
    /// ID of the sentinel revision performing this sweep (used in findings).
    pub sentinel_revision_id: String,
    /// Only scan events after this RFC-3339 timestamp. `None` = full history.
    pub since_rfc3339: Option<String>,
    /// Maximum events to scan per check. Defaults to 10_000.
    pub scan_limit: u32,

    // ── Phase 1 thresholds ──────────────────────────────────────────────
    /// Number of days to look back for capability accretion and approval bypass.
    pub window_days: u32,
    /// Number of promotions in `window_days` that triggers a capability-accretion warning.
    pub accretion_threshold: u32,
    /// Number of denied approvals in `window_days` that triggers an approval-bypass warning.
    pub denial_threshold: u32,

    // ── Phase 2 thresholds ──────────────────────────────────────────────
    /// Minutes to look back for session-cluster anomaly detection.
    pub cluster_window_minutes: u32,
    /// Number of error-status events in `cluster_window_minutes` that triggers a failure-burst warning.
    pub failure_burst_threshold: u32,
    /// Number of identical sandbox_exec targets in `cluster_window_minutes` that triggers a repeat warning.
    pub exec_repeat_threshold: u32,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            sentinel_revision_id: "sentinel.baseline".to_string(),
            since_rfc3339: None,
            scan_limit: 10_000,
            window_days: 30,
            accretion_threshold: 10,
            denial_threshold: 5,
            cluster_window_minutes: 60,
            failure_burst_threshold: 20,
            exec_repeat_threshold: 10,
        }
    }
}

/// Outcome of a single sweep run.
#[derive(Debug, Default)]
pub struct SweepResult {
    // Phase 1 — deterministic
    pub credential_findings: Vec<SecurityFinding>,
    pub capability_accretion_findings: Vec<SecurityFinding>,
    pub approval_bypass_findings: Vec<SecurityFinding>,
    pub sandbox_escape_findings: Vec<SecurityFinding>,
    // Phase 2 — LLM-judgment heuristics
    pub prompt_injection_findings: Vec<SecurityFinding>,
    pub behavioral_anomaly_findings: Vec<SecurityFinding>,
    pub persist_errors: Vec<String>,
}

impl SweepResult {
    pub fn total_findings(&self) -> usize {
        self.credential_findings.len()
            + self.capability_accretion_findings.len()
            + self.approval_bypass_findings.len()
            + self.sandbox_escape_findings.len()
            + self.prompt_injection_findings.len()
            + self.behavioral_anomaly_findings.len()
    }

    pub fn all_findings(&self) -> impl Iterator<Item = &SecurityFinding> {
        self.credential_findings
            .iter()
            .chain(&self.capability_accretion_findings)
            .chain(&self.approval_bypass_findings)
            .chain(&self.sandbox_escape_findings)
            .chain(&self.prompt_injection_findings)
            .chain(&self.behavioral_anomaly_findings)
    }
}

/// Runs deterministic (Phase 1) and heuristic (Phase 2) security sweeps.
pub struct SentinelRunner {
    store: Arc<GatewayStore>,
    /// Root directory of the agents tree (e.g. `agents/` in the workspace).
    /// When set, Phase 2 prompt-injection scans read SKILL.md bodies from here.
    /// When `None`, the prompt-injection check is skipped.
    agents_dir: Option<PathBuf>,
}

impl SentinelRunner {
    pub fn new(store: Arc<GatewayStore>) -> Self {
        Self {
            store,
            agents_dir: None,
        }
    }

    /// Configure the agents directory for Phase 2 prompt-injection scanning.
    pub fn with_agents_dir(mut self, agents_dir: PathBuf) -> Self {
        self.agents_dir = Some(agents_dir);
        self
    }

    /// Run a full sweep according to `config`.
    ///
    /// Findings are persisted to `security_findings` as they are discovered;
    /// failures to persist individual findings are collected in
    /// `SweepResult::persist_errors` rather than aborting the entire run.
    pub fn run_sweep(&self, config: &SweepConfig) -> Result<SweepResult> {
        let mut result = SweepResult::default();

        let since = config.since_rfc3339.as_deref();
        let rev_id = &config.sentinel_revision_id;
        let scan_limit = config.scan_limit;
        let window_days = config.window_days;
        let accretion_threshold = config.accretion_threshold;
        let denial_threshold = config.denial_threshold;
        let cluster_window_minutes = config.cluster_window_minutes;
        let failure_burst_threshold = config.failure_burst_threshold;
        let exec_repeat_threshold = config.exec_repeat_threshold;

        // ── Phase 1: deterministic checks (single connection borrow) ───────────
        // ── Phase 2a: session-cluster heuristics (SQL, same borrow) ────────────
        //
        // Both phases run inside a single `with_conn` borrow for efficiency.
        // Results are collected in a flat vec; the macro below distributes
        // them into typed buckets in order.
        let all_db_checks: Vec<Result<Vec<SecurityFinding>>> = self.store.with_conn(|conn| {
            Ok(vec![
                // Phase 1 — deterministic
                credential::scan_credential_leaks(conn, rev_id, since, scan_limit),
                capability_accretion::scan_capability_accretion(
                    conn,
                    rev_id,
                    window_days,
                    accretion_threshold,
                ),
                approval_bypass::scan_approval_denials(conn, rev_id, window_days, denial_threshold),
                approval_bypass::scan_exec_without_grant(conn, rev_id, since, scan_limit),
                sandbox_escape::scan_escape_attempt_records(conn, rev_id, since, scan_limit),
                sandbox_escape::scan_escape_patterns_in_events(conn, rev_id, since, scan_limit),
                // Phase 2a — cluster heuristics
                session_cluster::scan_failure_bursts(
                    conn,
                    rev_id,
                    since,
                    cluster_window_minutes,
                    failure_burst_threshold,
                    scan_limit,
                ),
                session_cluster::scan_exec_repeats(
                    conn,
                    rev_id,
                    since,
                    cluster_window_minutes,
                    exec_repeat_threshold,
                    scan_limit,
                ),
            ])
        })?;

        let mut checks_iter = all_db_checks.into_iter();

        macro_rules! collect_check {
            ($bucket:ident, $label:expr) => {
                match checks_iter.next().expect("result count mismatch") {
                    Ok(findings) => {
                        for f in findings {
                            if let Err(e) = self.store.insert_security_finding(&f) {
                                result
                                    .persist_errors
                                    .push(format!("{} persist: {}", $label, e));
                            } else {
                                result.$bucket.push(f);
                            }
                        }
                    }
                    Err(e) => result
                        .persist_errors
                        .push(format!("{} scan: {}", $label, e)),
                }
            };
        }

        // Phase 1
        collect_check!(credential_findings, "credential");
        collect_check!(capability_accretion_findings, "accretion");
        collect_check!(approval_bypass_findings, "approval_denial");
        collect_check!(approval_bypass_findings, "exec_without_grant");
        collect_check!(sandbox_escape_findings, "escape_records");
        collect_check!(sandbox_escape_findings, "escape_patterns");
        // Phase 2a
        collect_check!(behavioral_anomaly_findings, "failure_burst");
        collect_check!(behavioral_anomaly_findings, "exec_repeat");

        // 2b. Prompt-injection surface scan (filesystem-backed, optional).
        if let Some(ref agents_dir) = self.agents_dir {
            match prompt_injection::scan_prompt_injection(
                agents_dir,
                rev_id,
                scan_limit as usize,
            ) {
                Ok(findings) => {
                    for f in findings {
                        if let Err(e) = self.store.insert_security_finding(&f) {
                            result
                                .persist_errors
                                .push(format!("prompt_injection persist: {}", e));
                        } else {
                            result.prompt_injection_findings.push(f);
                        }
                    }
                }
                Err(e) => result
                    .persist_errors
                    .push(format!("prompt_injection scan: {}", e)),
            }
        }

        Ok(result)
    }
}
