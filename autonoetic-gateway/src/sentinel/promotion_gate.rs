//! Pre-promotion sentinel gate.
//!
//! Before `atomic_promote` is called the gateway runs a Phase-1-only sentinel
//! sweep of the **full store** (not restricted to the promoting agent — any
//! critical finding in the system blocks promotion, providing a conservative
//! fail-closed posture). Per-agent scoping is planned for a future phase.
//!
//! If any `critical` findings exist the promotion is blocked. Scan errors also
//! block promotion (fail-closed). The sweep is time-boxed: if it does not
//! complete within `timeout_secs` the gate returns `Err` and the promotion is
//! also blocked.
//!
//! **No findings are persisted by the gate.** Persistence is the job of the
//! scheduled sweeps so that promotion attempts don't bloat `security_findings`
//! with duplicate rows. The gate is a read-evaluate-only check.

use anyhow::{anyhow, Result};
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
        /// Number of critical Phase-1 findings found.
        critical_count: usize,
    },
}

/// Run a pre-promotion Phase-1 sentinel sweep against the full store.
///
/// Returns `Ok(GateOutcome::Passed)` when no critical findings exist and no
/// scan errors occurred within `timeout_secs`. Returns `Ok(GateOutcome::Blocked)`
/// if critical findings were detected. Returns `Err` on timeout, scan errors, or
/// sweep panic — all are fail-closed.
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
            .scan_phase1_critical(&SweepConfig {
                sentinel_revision_id: rev_id,
                // Scan the full history — window_days controls capability-accretion
                // and approval-denial lookback (90 days).
                window_days: 90,
                ..SweepConfig::default()
            })
            .and_then(|(critical_count, scan_errors)| {
                // Any scan error is fail-closed: treat as a blocking gate failure.
                if !scan_errors.is_empty() {
                    return Err(anyhow!(
                        "Sentinel scan errors (fail-closed): {}",
                        scan_errors.join("; ")
                    ));
                }
                if critical_count == 0 {
                    Ok(GateOutcome::Passed)
                } else {
                    Ok(GateOutcome::Blocked {
                        reason: format!(
                            "{} critical Phase-1 finding(s) in the store",
                            critical_count
                        ),
                        critical_count,
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
