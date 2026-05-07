//! `SentinelRunner` — orchestrates a deterministic Phase-1 sweep.
//!
//! A sweep runs all deterministic checks, persists findings to the
//! `security_findings` table, and returns a summary. Callers can supply a
//! `since` timestamp to run incremental sweeps (only events after that point
//! are scanned).

use anyhow::Result;
use autonoetic_types::security::SecurityFinding;
use std::sync::Arc;

use crate::scheduler::gateway_store::GatewayStore;
use super::checks::{approval_bypass, capability_accretion, credential, sandbox_escape};

/// Configuration for a deterministic sentinel sweep.
pub struct SweepConfig {
    /// ID of the sentinel revision performing this sweep (used in findings).
    pub sentinel_revision_id: String,
    /// Only scan events after this RFC-3339 timestamp. `None` = full history.
    pub since_rfc3339: Option<String>,
    /// Maximum events to scan per check. Defaults to 10_000.
    pub scan_limit: u32,
    /// Number of days to look back for capability accretion and approval bypass.
    pub window_days: u32,
    /// Number of promotions in `window_days` that triggers a capability-accretion warning.
    pub accretion_threshold: u32,
    /// Number of denied approvals in `window_days` that triggers an approval-bypass warning.
    pub denial_threshold: u32,
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
        }
    }
}

/// Outcome of a single sweep run.
#[derive(Debug, Default)]
pub struct SweepResult {
    pub credential_findings: Vec<SecurityFinding>,
    pub capability_accretion_findings: Vec<SecurityFinding>,
    pub approval_bypass_findings: Vec<SecurityFinding>,
    pub sandbox_escape_findings: Vec<SecurityFinding>,
    pub persist_errors: Vec<String>,
}

impl SweepResult {
    pub fn total_findings(&self) -> usize {
        self.credential_findings.len()
            + self.capability_accretion_findings.len()
            + self.approval_bypass_findings.len()
            + self.sandbox_escape_findings.len()
    }

    pub fn all_findings(&self) -> impl Iterator<Item = &SecurityFinding> {
        self.credential_findings
            .iter()
            .chain(&self.capability_accretion_findings)
            .chain(&self.approval_bypass_findings)
            .chain(&self.sandbox_escape_findings)
    }
}

/// Runs deterministic (Phase 1) security sweeps.
pub struct SentinelRunner {
    store: Arc<GatewayStore>,
}

impl SentinelRunner {
    pub fn new(store: Arc<GatewayStore>) -> Self {
        Self { store }
    }

    /// Run a full deterministic sweep according to `config`.
    ///
    /// Findings are persisted to `security_findings` as they are discovered;
    /// failures to persist individual findings are collected in
    /// `SweepResult::persist_errors` rather than aborting the entire run.
    pub fn run_sweep(&self, config: &SweepConfig) -> Result<SweepResult> {
        let mut result = SweepResult::default();

        let since = config.since_rfc3339.as_deref();
        let rev_id = config.sentinel_revision_id.clone();
        let scan_limit = config.scan_limit;
        let window_days = config.window_days;
        let accretion_threshold = config.accretion_threshold;
        let denial_threshold = config.denial_threshold;

        // All checks run inside a single connection borrow for efficiency.
        let all_findings: Vec<Result<Vec<SecurityFinding>>> =
            self.store.with_conn(|conn| {
                Ok(vec![
                    credential::scan_credential_leaks(conn, &rev_id, since, scan_limit),
                    capability_accretion::scan_capability_accretion(
                        conn,
                        &rev_id,
                        window_days,
                        accretion_threshold,
                    ),
                    approval_bypass::scan_approval_denials(
                        conn,
                        &rev_id,
                        window_days,
                        denial_threshold,
                    ),
                    approval_bypass::scan_exec_without_grant(conn, &rev_id, since, scan_limit),
                    sandbox_escape::scan_escape_attempt_records(conn, &rev_id, since, scan_limit),
                    sandbox_escape::scan_escape_patterns_in_events(
                        conn,
                        &rev_id,
                        since,
                        scan_limit,
                    ),
                ])
            })?;

        // The results are in the same order as the checks above. Use indices
        // to distribute findings into the typed result buckets.
        let mut all_findings_iter = all_findings.into_iter();

        macro_rules! collect_check {
            ($bucket:ident, $label:expr) => {
                match all_findings_iter.next().expect("result count mismatch") {
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

        collect_check!(credential_findings, "credential");
        collect_check!(capability_accretion_findings, "accretion");
        collect_check!(approval_bypass_findings, "approval_denial");
        collect_check!(approval_bypass_findings, "exec_without_grant");
        collect_check!(sandbox_escape_findings, "escape_records");
        collect_check!(sandbox_escape_findings, "escape_patterns");

        Ok(result)
    }
}
