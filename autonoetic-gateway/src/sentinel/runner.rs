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
//! ## Phase 3 dual-sweep
//! See [`super::dual_sweep::DualSweepRunner`] for the orchestrator that runs
//! the frozen baseline alongside the current sentinel and records disagreements.
//!
//! NOTE: Emission of `security_finding_recorded` events to the causal chain is
//! planned for Phase 5 (scheduling integration). The current runner persists
//! findings to the `security_findings` table only.

use anyhow::Result;
use autonoetic_types::security::{FindingSeverity, SecurityFinding};
use std::path::PathBuf;
use std::sync::Arc;

use crate::scheduler::gateway_store::GatewayStore;
use super::checks::{approval_bypass, capability_accretion, credential, prompt_injection, sandbox_escape, session_cluster, supply_chain};

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

    // ── Scope control ───────────────────────────────────────────────────
    /// When `true`, run only Phase-1 (deterministic) checks — skip cluster
    /// heuristics and prompt-injection scanning. Used by the dual-sweep
    /// orchestrator for the frozen baseline runner.
    pub phase1_only: bool,
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
            phase1_only: false,
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
    pub supply_chain_findings: Vec<SecurityFinding>,
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
            + self.supply_chain_findings.len()
            + self.prompt_injection_findings.len()
            + self.behavioral_anomaly_findings.len()
    }

    pub fn all_findings(&self) -> impl Iterator<Item = &SecurityFinding> {
        self.credential_findings
            .iter()
            .chain(&self.capability_accretion_findings)
            .chain(&self.approval_bypass_findings)
            .chain(&self.sandbox_escape_findings)
            .chain(&self.supply_chain_findings)
            .chain(&self.prompt_injection_findings)
            .chain(&self.behavioral_anomaly_findings)
    }

    /// Return only Phase 1 (deterministic) findings — used for baseline comparison.
    pub fn phase1_findings(&self) -> impl Iterator<Item = &SecurityFinding> {
        self.credential_findings
            .iter()
            .chain(&self.capability_accretion_findings)
            .chain(&self.approval_bypass_findings)
            .chain(&self.sandbox_escape_findings)
            .chain(&self.supply_chain_findings)
    }
}

/// Raw findings from a sweep, before persisting. Used internally by the runner
/// and by the dual-sweep orchestrator to annotate `baseline_agreed` before the
/// final persist step.
#[derive(Debug, Default)]
pub(super) struct RawSweepFindings {
    pub credential: Vec<SecurityFinding>,
    pub capability_accretion: Vec<SecurityFinding>,
    pub approval_bypass: Vec<SecurityFinding>,
    pub sandbox_escape: Vec<SecurityFinding>,
    pub supply_chain: Vec<SecurityFinding>,
    pub prompt_injection: Vec<SecurityFinding>,
    pub behavioral_anomaly: Vec<SecurityFinding>,
    pub scan_errors: Vec<String>,
}

impl RawSweepFindings {
    pub fn all_phase1(&self) -> impl Iterator<Item = &SecurityFinding> {
        self.credential
            .iter()
            .chain(&self.capability_accretion)
            .chain(&self.approval_bypass)
            .chain(&self.sandbox_escape)
            .chain(&self.supply_chain)
    }

    pub fn all(&self) -> impl Iterator<Item = &SecurityFinding> {
        self.credential
            .iter()
            .chain(&self.capability_accretion)
            .chain(&self.approval_bypass)
            .chain(&self.sandbox_escape)
            .chain(&self.supply_chain)
            .chain(&self.prompt_injection)
            .chain(&self.behavioral_anomaly)
    }
}

/// Runs deterministic (Phase 1) and heuristic (Phase 2) security sweeps.
pub struct SentinelRunner {
    pub(super) store: Arc<GatewayStore>,
    /// Root directory of the agents tree for Phase 2 prompt-injection scanning.
    pub(super) agents_dir: Option<PathBuf>,
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

    /// Run all checks and return raw (unpersisted) findings.
    ///
    /// The dual-sweep orchestrator calls this to collect findings from both the
    /// baseline and current sentinel before annotating `baseline_agreed` and
    /// persisting. Direct callers should use [`run_sweep`] instead.
    pub(super) fn collect_findings(&self, config: &SweepConfig) -> Result<RawSweepFindings> {
        let mut raw = RawSweepFindings::default();

        let since = config.since_rfc3339.as_deref();
        let rev_id = &config.sentinel_revision_id;
        let scan_limit = config.scan_limit;
        let window_days = config.window_days;
        let accretion_threshold = config.accretion_threshold;
        let denial_threshold = config.denial_threshold;
        let cluster_window_minutes = config.cluster_window_minutes;
        let failure_burst_threshold = config.failure_burst_threshold;
        let exec_repeat_threshold = config.exec_repeat_threshold;

        // All DB checks in a single connection borrow.
        let db_results: Vec<Result<Vec<SecurityFinding>>> = self.store.with_conn(|conn| {
            let mut checks = vec![
                // Phase 1 — deterministic
                credential::scan_credential_leaks(conn, rev_id, since, scan_limit),
                capability_accretion::scan_capability_accretion(
                    conn, rev_id, window_days, accretion_threshold,
                ),
                approval_bypass::scan_approval_denials(conn, rev_id, window_days, denial_threshold),
                approval_bypass::scan_exec_without_grant(conn, rev_id, since, scan_limit),
                sandbox_escape::scan_escape_attempt_records(conn, rev_id, since, scan_limit),
                sandbox_escape::scan_escape_patterns_in_events(conn, rev_id, since, scan_limit),
                // Phase 1 — supply-chain auditing
                supply_chain::scan_layer_scope_violations(conn, rev_id, since, scan_limit),
                supply_chain::scan_layer_provenance_gaps(conn, rev_id, since, scan_limit),
            ];
            // Phase 2a — cluster heuristics (always rolling window, not `since`).
            // Skipped when phase1_only is set (e.g. frozen baseline runner).
            if !config.phase1_only {
                checks.push(session_cluster::scan_failure_bursts(
                    conn, rev_id, cluster_window_minutes, failure_burst_threshold, scan_limit,
                ));
                checks.push(session_cluster::scan_exec_repeats(
                    conn, rev_id, cluster_window_minutes, exec_repeat_threshold, scan_limit,
                ));
            }
            Ok(checks)
        })?;

        let mut it = db_results.into_iter();
        macro_rules! take_check {
            ($bucket:ident, $label:expr) => {
                match it.next().expect("result count mismatch") {
                    Ok(v) => raw.$bucket.extend(v),
                    Err(e) => raw.scan_errors.push(format!("{} scan: {}", $label, e)),
                }
            };
        }
        take_check!(credential, "credential");
        take_check!(capability_accretion, "accretion");
        take_check!(approval_bypass, "approval_denial");
        take_check!(approval_bypass, "exec_without_grant");
        take_check!(sandbox_escape, "escape_records");
        take_check!(sandbox_escape, "escape_patterns");
        take_check!(supply_chain, "supply_chain_scope");
        take_check!(supply_chain, "supply_chain_provenance");
        if !config.phase1_only {
            take_check!(behavioral_anomaly, "failure_burst");
            take_check!(behavioral_anomaly, "exec_repeat");
        }

        // Phase 2b — prompt injection (filesystem, optional). Skipped for baseline.
        if !config.phase1_only {
            if let Some(ref agents_dir) = self.agents_dir {
                match prompt_injection::scan_prompt_injection(agents_dir, rev_id, scan_limit as usize) {
                    Ok(v) => raw.prompt_injection = v,
                    Err(e) => raw.scan_errors.push(format!("prompt_injection scan: {}", e)),
                }
            }
        }

        Ok(raw)
    }

    /// Persist pre-collected findings and return a structured `SweepResult`.
    pub(super) fn persist_findings(&self, raw: RawSweepFindings) -> SweepResult {
        let mut result = SweepResult::default();
        result.persist_errors = raw.scan_errors;

        macro_rules! persist_bucket {
            ($raw_field:expr, $result_field:ident, $label:expr) => {
                for f in $raw_field {
                    if let Err(e) = self.store.insert_security_finding(&f) {
                        result
                            .persist_errors
                            .push(format!("{} persist: {}", $label, e));
                    } else {
                        result.$result_field.push(f);
                    }
                }
            };
        }

        persist_bucket!(raw.credential, credential_findings, "credential");
        persist_bucket!(raw.capability_accretion, capability_accretion_findings, "accretion");
        persist_bucket!(raw.approval_bypass, approval_bypass_findings, "approval_bypass");
        persist_bucket!(raw.sandbox_escape, sandbox_escape_findings, "sandbox_escape");
        persist_bucket!(raw.supply_chain, supply_chain_findings, "supply_chain");
        persist_bucket!(raw.prompt_injection, prompt_injection_findings, "prompt_injection");
        persist_bucket!(raw.behavioral_anomaly, behavioral_anomaly_findings, "behavioral_anomaly");

        result
    }

    /// Scan Phase-1 checks and return `(critical_count, scan_errors)` without persisting.
    ///
    /// Used by the promotion gate to evaluate findings without creating duplicate
    /// DB rows on every promotion attempt. Any scan error is propagated in the
    /// returned `Vec<String>` so callers can treat non-empty errors as fail-closed.
    pub fn scan_phase1_critical(&self, config: &SweepConfig) -> Result<(usize, Vec<String>)> {
        let raw = self.collect_findings(&SweepConfig {
            phase1_only: true,
            ..SweepConfig {
                sentinel_revision_id: config.sentinel_revision_id.clone(),
                since_rfc3339: config.since_rfc3339.clone(),
                scan_limit: config.scan_limit,
                window_days: config.window_days,
                accretion_threshold: config.accretion_threshold,
                denial_threshold: config.denial_threshold,
                ..SweepConfig::default()
            }
        })?;
        let critical = raw
            .all_phase1()
            .filter(|f| f.severity == FindingSeverity::Critical)
            .count();
        Ok((critical, raw.scan_errors))
    }

    /// Run a full sweep: collect, persist, and return results.
    ///
    /// For dual-sweep (baseline + current with disagreement recording) use
    /// [`super::dual_sweep::DualSweepRunner`] instead.
    pub fn run_sweep(&self, config: &SweepConfig) -> Result<SweepResult> {
        let raw = self.collect_findings(config)?;
        Ok(self.persist_findings(raw))
    }
}
