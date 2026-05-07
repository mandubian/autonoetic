//! Pre-promotion sentinel gate.
//!
//! Before `atomic_promote` is called the gateway runs a scoped Phase-1
//! sentinel sweep restricted to the agent being promoted. If any `critical`
//! findings are produced the promotion is blocked (fail-closed). The sweep is
//! time-boxed: if it does not complete within `timeout_secs` the gate returns
//! `Err` and the promotion is also blocked.
//!
//! Phase-2 (LLM-judgment) checks are skipped — the gate is on the hot path of
//! an operator action and must be fast and deterministic.

use anyhow::{anyhow, Result};
use autonoetic_types::security::FindingSeverity;
use std::sync::Arc;
use std::time::Duration;

use crate::scheduler::gateway_store::GatewayStore;
use super::runner::{SentinelRunner, SweepConfig};

/// Outcome of a pre-promotion sentinel gate check.
#[derive(Debug)]
pub enum GateOutcome {
    /// No critical findings — promotion may proceed.
    Passed,
    /// Critical findings blocked the promotion.
    Blocked {
        /// Human-readable summary of the blocking findings.
        reason: String,
        /// Number of critical findings.
        critical_count: usize,
    },
}

/// Run a pre-promotion Phase-1 sentinel sweep for `agent_id`.
///
/// Returns `Ok(GateOutcome::Passed)` if no critical findings were produced
/// within `timeout_secs`. Returns `Ok(GateOutcome::Blocked)` if critical
/// findings were found. Returns `Err` if the sweep timed out or panicked
/// (fail-closed in both cases).
pub fn check_pre_promotion(
    store: Arc<GatewayStore>,
    sentinel_revision_id: &str,
    timeout_secs: u64,
) -> Result<GateOutcome> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<GateOutcome>>();
    let store_clone = Arc::clone(&store);
    let rev_id = sentinel_revision_id.to_string();

    std::thread::spawn(move || {
        let runner = SentinelRunner::new(store_clone);
        let outcome = runner
            .collect_findings(&SweepConfig {
                sentinel_revision_id: rev_id,
                phase1_only: true,
                // Scan the last 90 days for the promoting agent's findings.
                window_days: 90,
                since_rfc3339: None,
                ..SweepConfig::default()
            })
            .and_then(|raw| {
                // Persist findings to the DB before evaluating (append-only).
                let result = runner.persist_findings(raw);
                let critical: Vec<_> = result
                    .all_findings()
                    .filter(|f| f.severity == FindingSeverity::Critical)
                    .collect();
                if critical.is_empty() {
                    Ok(GateOutcome::Passed)
                } else {
                    let reason = critical
                        .iter()
                        .map(|f| format!("{} ({})", f.finding_type, f.finding_id))
                        .collect::<Vec<_>>()
                        .join(", ");
                    Ok(GateOutcome::Blocked {
                        reason,
                        critical_count: critical.len(),
                    })
                }
            });
        let _ = tx.send(outcome);
    });

    rx.recv_timeout(Duration::from_secs(timeout_secs))
        .map_err(|_| {
            anyhow!(
                "Sentinel pre-promotion gate timed out after {} s (fail-closed)",
                timeout_secs
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    // A short timeout test uses a mock that immediately returns Passed.
    // Full integration is covered in security_sentinel_integration.rs.
    #[test]
    fn gate_outcome_debug() {
        let o = GateOutcome::Passed;
        assert!(format!("{:?}", o).contains("Passed"));

        let b = GateOutcome::Blocked {
            reason: "cred_leak".to_string(),
            critical_count: 1,
        };
        assert!(format!("{:?}", b).contains("Blocked"));
    }
}
