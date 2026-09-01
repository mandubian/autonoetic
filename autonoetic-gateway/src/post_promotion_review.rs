//! Post-promotion background review (Phase 4, Tier 1).
//!
//! For every promoted agent, periodically reviews:
//! - Causal event trends (tool failures, authorization denials, suspensions)
//! - Sentinel findings accumulated since the last review
//! - Emits findings or `operator_alert` timeline events when thresholds are exceeded
//!
//! #739 Part C: post-promotion anomalies are **alerts** (timeline events), not
//! actionable operator decisions. They no longer create an `escalations` row
//! that could masquerade as a resolvable decision in `operator.pending`. If a
//! future actionable "review rollback?" decision is needed, it should be an
//! explicit approval, not an anomaly escalation.

use std::sync::Arc;

use serde::Serialize;

use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::PostPromotionReviewConfig;

/// Result of a single agent's post-promotion review.
#[derive(Debug, Clone, Serialize)]
pub struct AgentReviewResult {
    pub agent_id: String,
    pub revision_id: String,
    pub tool_failures_since_last: i64,
    pub auth_denials_since_last: i64,
    pub suspensions_since_last: i64,
    pub sentinel_findings_since_last: i64,
    pub previous_tool_failures: i64,
    pub previous_auth_denials: i64,
    pub previous_suspensions: i64,
    pub findings: Vec<ReviewFinding>,
    pub escalated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewFinding {
    pub severity: String,
    pub message: String,
    pub metric: String,
    pub current_value: i64,
    pub previous_value: i64,
}

/// Run post-promotion review for every promoted agent **whose review is due**.
///
/// Returns a result per agent actually reviewed; agents still inside
/// `cfg.interval_secs` are skipped and absent from the result. Critical findings
/// emit an `operator_alert` timeline event — **not** an `escalations` row: an
/// anomaly is something to see, not a decision to resolve (#739 Part C).
///
/// `now` is injected rather than read from the clock so cadence is testable.
///
/// # Cadence is the measurement window (#1046)
///
/// Each agent's counters are counted *since its own previous review*, so the
/// interval decides what "since last review" means. This used to be called on
/// every scheduler tick with no gate and a window taken from the global
/// `MAX(reviewed_at)` — which, because a row was written every tick, was the
/// tick interval. That produced ~33 rows every 5s (14.5k rows in 37 minutes
/// observed) and made every trend a comparison of two 5-second windows: noise,
/// not drift. The gate below is what makes the numbers mean anything, so it is
/// not merely a write-volume optimisation.
///
/// The `enabled` check lives here rather than at the call site so no caller can
/// re-introduce the unbounded sweep by forgetting it.
pub fn run_post_promotion_review(
    store: &Arc<GatewayStore>,
    cfg: &PostPromotionReviewConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Vec<AgentReviewResult>> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }

    // Fail closed on an interval we cannot represent. `as i64` would wrap a
    // large `u64` into a *negative* Duration, which makes every agent look
    // overdue and restores the per-tick sweep this function exists to prevent;
    // `Duration::seconds` also panics out of range. Reviewing nothing is the
    // safe reading of "the configured cadence is uninterpretable".
    let interval = i64::try_from(cfg.interval_secs)
        .ok()
        .and_then(chrono::Duration::try_seconds);
    let Some((interval, first_review_window)) =
        interval.and_then(|i| now.checked_sub_signed(i).map(|w| (i, w)))
    else {
        tracing::warn!(
            target: "post_promotion_review",
            interval_secs = cfg.interval_secs,
            "post_promotion_review.interval_secs is out of representable range; \
             skipping the sweep rather than reviewing every agent every tick"
        );
        return Ok(Vec::new());
    };

    let now_rfc = now.to_rfc3339();

    let aliases = store.list_agent_aliases(None)?;

    let mut results = Vec::new();
    let mut skipped = 0usize;
    let mut uninterpretable: Vec<String> = Vec::new();

    for alias in &aliases {
        let agent_id = &alias.agent_id;
        let revision_id = &alias.revision_id;

        // Per-agent, not global: with a global cutoff, one agent reviewed an
        // hour ago would shorten every other agent's window to an hour.
        let last_reviewed_at = store
            .get_last_post_promotion_review(agent_id)?
            .map(|r| r.reviewed_at);

        let window_start = match last_reviewed_at.as_deref() {
            Some(ts) => {
                let age = chrono::DateTime::parse_from_rfc3339(ts)
                    .ok()
                    .map(|parsed| now.signed_duration_since(parsed.with_timezone(&chrono::Utc)));
                match age {
                    Some(age) if age >= chrono::Duration::zero() => {
                        if age < interval {
                            skipped += 1;
                            continue;
                        }
                        ts.to_string()
                    }
                    // Unparseable, or dated in the future (clock skew, a manual
                    // DB edit). Either way there is no window to measure, so the
                    // agent is skipped rather than reviewed.
                    //
                    // Reviewing anyway — the obvious "treat it as overdue and
                    // re-anchor" — writes a row on *every* tick, because the
                    // lookup is `ORDER BY reviewed_at DESC` over TEXT: a bytewise
                    // sort in which `"not-a-timestamp"` and any future date
                    // outrank every well-formed past one, so no row we write can
                    // displace the bad one. Measured on that approach: 5 ticks
                    // produced 5 extra rows with `"not-a-timestamp"` still
                    // selected — the exact amplification this function prevents.
                    //
                    // Genuine clock skew self-heals once wall-clock passes the
                    // stored value. A corrupt value needs an operator, which is
                    // what the warning below is for.
                    _ => {
                        uninterpretable.push(format!("{agent_id}={ts}"));
                        skipped += 1;
                        continue;
                    }
                }
            }
            // A first-ever review has no previous row to measure from, so it
            // looks back exactly one interval — the window every later review
            // gets.
            None => first_review_window.to_rfc3339(),
        };

        let tool_failures = store.count_tool_failures_since(agent_id, &window_start)?;
        let auth_denials = store.count_auth_denials_since(agent_id, &window_start)?;
        let suspensions = store.count_suspensions_since(agent_id, &window_start)?;
        let sentinel_findings =
            store.count_sentinel_findings_for_agent_since(agent_id, &window_start)?;

        let previous = load_previous_review(store, agent_id)?;

        let mut findings = Vec::new();
        let mut escalated = false;

        let tool_failure_rate = if previous.tool_failures > 0 {
            (tool_failures as f64) / (previous.tool_failures as f64)
        } else if tool_failures > 0 {
            2.0
        } else {
            0.0
        };

        if tool_failure_rate > cfg.tool_failure_rate_warning {
            findings.push(ReviewFinding {
                severity: if tool_failure_rate > cfg.tool_failure_rate_critical {
                    "critical".to_string()
                } else {
                    "warning".to_string()
                },
                message: format!(
                    "Tool failure rate increased {:.1}x ({} vs {} in previous review)",
                    tool_failure_rate, tool_failures, previous.tool_failures
                ),
                metric: "tool_failure_rate".to_string(),
                current_value: tool_failures,
                previous_value: previous.tool_failures,
            });
        }

        if auth_denials > previous.auth_denials * 2 && auth_denials > 0 {
            findings.push(ReviewFinding {
                severity: "warning".to_string(),
                message: format!(
                    "Authorization denials increased ({} vs {} in previous review)",
                    auth_denials, previous.auth_denials
                ),
                metric: "auth_denials".to_string(),
                current_value: auth_denials,
                previous_value: previous.auth_denials,
            });
        }

        if suspensions > previous.suspensions * 2 && suspensions > 1 {
            findings.push(ReviewFinding {
                severity: "critical".to_string(),
                message: format!(
                    "Session suspensions increased ({} vs {} in previous review)",
                    suspensions, previous.suspensions
                ),
                metric: "suspensions".to_string(),
                current_value: suspensions,
                previous_value: previous.suspensions,
            });
        }

        if sentinel_findings > cfg.sentinel_findings_warning as i64 {
            findings.push(ReviewFinding {
                severity: if sentinel_findings > cfg.sentinel_findings_critical as i64 {
                    "critical".to_string()
                } else {
                    "warning".to_string()
                },
                message: format!(
                    "{} new sentinel finding(s) since last review",
                    sentinel_findings
                ),
                metric: "sentinel_findings".to_string(),
                current_value: sentinel_findings,
                previous_value: 0,
            });
        }

        let critical_findings: Vec<&ReviewFinding> =
            findings.iter().filter(|f| f.severity == "critical").collect();
        if !critical_findings.is_empty() {
            let synthesis = format!(
                "Post-promotion review for '{}': {} critical and {} warning findings. {}",
                agent_id,
                critical_findings.len(),
                findings.len() - critical_findings.len(),
                critical_findings.iter().map(|f| f.message.as_str()).collect::<Vec<_>>().join("; "),
            );

            let artifact_id = store
                .get_agent_revision(revision_id)
                .ok()
                .flatten()
                .and_then(|r| r.artifact_id)
                .filter(|s| !s.is_empty())
                .unwrap_or_default();

            // #739 Part C: emit an `operator_alert` timeline event instead of
            // an actionable escalation row. The alert surfaces in the timeline
            // (read model) without masquerading as a resolvable decision in
            // `operator.pending`. No `escalations` row is created at all —
            // the timeline event is the only record of the anomaly.
            emit_post_promotion_anomaly_alert(
                store.as_ref(),
                agent_id,
                revision_id,
                &artifact_id,
                &synthesis,
                critical_findings.len(),
            );
            escalated = true;
        }

        store.record_post_promotion_review(
            agent_id,
            revision_id,
            &now_rfc,
            tool_failures,
            auth_denials,
            suspensions,
            sentinel_findings,
            &serde_json::to_string(&findings)?,
        )?;

        results.push(AgentReviewResult {
            agent_id: agent_id.clone(),
            revision_id: revision_id.clone(),
            tool_failures_since_last: tool_failures,
            auth_denials_since_last: auth_denials,
            suspensions_since_last: suspensions,
            sentinel_findings_since_last: sentinel_findings,
            previous_tool_failures: previous.tool_failures,
            previous_auth_denials: previous.auth_denials,
            previous_suspensions: previous.suspensions,
            findings,
            escalated,
        });
    }

    // One aggregated warning per sweep rather than one per agent: the sweep runs
    // every tick, so per-agent logging would spam at tick frequency for as long
    // as the bad row exists.
    if !uninterpretable.is_empty() {
        tracing::warn!(
            target: "post_promotion_review",
            count = uninterpretable.len(),
            agents = %uninterpretable.join(", "),
            "Skipped agents whose last-review timestamp is unparseable or dated in \
             the future; they stay unreviewed until the row is corrected"
        );
    }

    if !results.is_empty() {
        tracing::info!(
            target: "post_promotion_review",
            reviewed = results.len(),
            skipped_not_due = skipped,
            interval_secs = cfg.interval_secs,
            "Post-promotion review swept due agents"
        );
    }

    Ok(results)
}

struct PreviousReviewValues {
    tool_failures: i64,
    auth_denials: i64,
    suspensions: i64,
}

fn load_previous_review(
    store: &Arc<GatewayStore>,
    agent_id: &str,
) -> anyhow::Result<PreviousReviewValues> {
    if let Some(last) = store.get_last_post_promotion_review(agent_id)? {
        Ok(PreviousReviewValues {
            tool_failures: last.tool_failures.max(1),
            auth_denials: last.auth_denials,
            suspensions: last.suspensions,
        })
    } else {
        Ok(PreviousReviewValues {
            tool_failures: 1,
            auth_denials: 0,
            suspensions: 0,
        })
    }
}

/// #739 Part C: emit a post-promotion anomaly as an `operator_alert` timeline
/// event (the same shape as the #723 approval-flood alert). Anomalies are
/// alerts — they surface in the timeline read model without masquerading as
/// resolvable operator decisions in `operator.pending`. The `agent_id` is used
/// as the synthetic root so the alert is attributable and discoverable.
fn emit_post_promotion_anomaly_alert(
    store: &GatewayStore,
    agent_id: &str,
    revision_id: &str,
    artifact_id: &str,
    synthesis: &str,
    critical_count: usize,
) {
    // Attribute the alert to the agent so it is discoverable in the timeline
    // under a stable key. Use the agent_id as the synthetic root/session.
    let root = if agent_id.is_empty() {
        "system".to_string()
    } else {
        format!("ppr:{agent_id}")
    };
    let principal = autonoetic_types::principal::Principal::agent("gateway");
    let seat = crate::runtime::session_timeline::derive_role("gateway");
    let event = crate::runtime::session_timeline::build_timeline_event(
        root.clone(),
        root,
        None,
        &principal,
        &seat,
        "operator_alert",
        None, // base altitude ⇒ Attention
        Some(serde_json::json!({
            "alert": "post_promotion_anomaly",
            "agent_id": agent_id,
            "revision_id": revision_id,
            "artifact_id": artifact_id,
            "critical_findings": critical_count,
            "message": synthesis,
        })),
        autonoetic_types::session_timeline::TimelineRefs::default(),
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(
            target: "session_timeline",
            error = %e,
            agent_id,
            "post_promotion_anomaly alert timeline emit failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent_revision::AgentAliasRecord;
    use chrono::{Duration, Utc};
    use tempfile::tempdir;

    /// A store with `n` promoted agents, so the sweep has something to iterate.
    fn store_with_agents(n: usize) -> (tempfile::TempDir, Arc<GatewayStore>) {
        let dir = tempdir().unwrap();
        let store = Arc::new(GatewayStore::open(dir.path()).unwrap());
        for i in 0..n {
            let id = format!("agent{i}.default");
            store
                .upsert_agent_alias(&AgentAliasRecord::new(
                    id.clone(),
                    id,
                    format!("rev_{i}"),
                    Utc::now().to_rfc3339(),
                    "operator".to_string(),
                    "test".to_string(),
                    None,
                ))
                .unwrap();
        }
        (dir, store)
    }

    fn review_count(store: &Arc<GatewayStore>) -> usize {
        store
            .list_post_promotion_reviews(None, 10_000)
            .unwrap()
            .len()
    }

    /// The headline #1046 regression: the sweep is called on every scheduler
    /// tick, so calling it repeatedly must not write a row each time. Before the
    /// fix this produced one row per agent per call — 14.5k rows in 37 minutes
    /// of real uptime.
    #[test]
    fn repeated_ticks_within_the_interval_write_one_review_per_agent() {
        let (_dir, store) = store_with_agents(3);
        let cfg = PostPromotionReviewConfig::default();
        let t0 = Utc::now();

        let first = run_post_promotion_review(&store, &cfg, t0).unwrap();
        assert_eq!(first.len(), 3, "first sweep reviews every agent");
        assert_eq!(review_count(&store), 3);

        // 12 further ticks, 5s apart — the observed scheduler cadence.
        for tick in 1..=12 {
            let reviewed =
                run_post_promotion_review(&store, &cfg, t0 + Duration::seconds(5 * tick)).unwrap();
            assert!(
                reviewed.is_empty(),
                "tick {tick} must review nobody inside the 24h interval"
            );
        }
        assert_eq!(
            review_count(&store),
            3,
            "still 3 rows after 13 ticks, not 39"
        );
    }

    #[test]
    fn an_agent_is_reviewed_again_once_the_interval_elapses() {
        let (_dir, store) = store_with_agents(1);
        let cfg = PostPromotionReviewConfig::default();
        let t0 = Utc::now();

        run_post_promotion_review(&store, &cfg, t0).unwrap();

        // One second short of due.
        let early = run_post_promotion_review(
            &store,
            &cfg,
            t0 + Duration::seconds(cfg.interval_secs as i64 - 1),
        )
        .unwrap();
        assert!(early.is_empty(), "not due one second early");

        let due = run_post_promotion_review(
            &store,
            &cfg,
            t0 + Duration::seconds(cfg.interval_secs as i64),
        )
        .unwrap();
        assert_eq!(due.len(), 1, "due exactly at the interval boundary");
        assert_eq!(review_count(&store), 2);
    }

    /// Cadence is per agent. A newly promoted agent must be reviewable even
    /// though another agent was reviewed moments ago — and, before the fix, the
    /// window came from the global `MAX(reviewed_at)`, so one recent review
    /// shortened everyone else's measurement window.
    #[test]
    fn cadence_and_window_are_per_agent_not_global() {
        let (_dir, store) = store_with_agents(1);
        let cfg = PostPromotionReviewConfig::default();
        let t0 = Utc::now();

        run_post_promotion_review(&store, &cfg, t0).unwrap();

        // A second agent shows up afterwards.
        store
            .upsert_agent_alias(&AgentAliasRecord::new(
                "late.default".to_string(),
                "late.default".to_string(),
                "rev_late".to_string(),
                Utc::now().to_rfc3339(),
                "operator".to_string(),
                "test".to_string(),
                None,
            ))
            .unwrap();

        let reviewed = run_post_promotion_review(&store, &cfg, t0 + Duration::seconds(10)).unwrap();
        assert_eq!(
            reviewed
                .iter()
                .map(|r| r.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["late.default"],
            "the new agent is due; the just-reviewed one is not"
        );
    }

    #[test]
    fn disabled_writes_nothing() {
        let (_dir, store) = store_with_agents(2);
        let cfg = PostPromotionReviewConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(run_post_promotion_review(&store, &cfg, Utc::now())
            .unwrap()
            .is_empty());
        assert_eq!(review_count(&store), 0);
    }

    /// A shorter interval is a legitimate operator choice, and must actually
    /// take effect — the gate reads config, not a hardcoded day.
    #[test]
    fn configured_interval_is_honoured() {
        let (_dir, store) = store_with_agents(1);
        let cfg = PostPromotionReviewConfig {
            interval_secs: 60,
            ..Default::default()
        };
        let t0 = Utc::now();

        run_post_promotion_review(&store, &cfg, t0).unwrap();
        assert!(
            run_post_promotion_review(&store, &cfg, t0 + Duration::seconds(59))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            run_post_promotion_review(&store, &cfg, t0 + Duration::seconds(60))
                .unwrap()
                .len(),
            1
        );
    }

    /// A timestamp that cannot be read as "in the past" gives no window to
    /// measure, so the agent is skipped — and, critically, skipped on *every*
    /// tick rather than reviewed on every tick.
    ///
    /// The tempting fix (treat it as overdue and re-anchor) does not work: the
    /// lookup is `ORDER BY reviewed_at DESC` over TEXT, a bytewise sort in which
    /// `"not-a-timestamp"` outranks every well-formed date, so no row written can
    /// displace it. That path was measured at 5 ticks → 5 extra rows.
    #[test]
    fn uninterpretable_last_review_timestamp_never_writes_on_every_tick() {
        let cfg = PostPromotionReviewConfig::default();
        let t0 = Utc::now();

        for bad in [
            "not-a-timestamp",
            // Dated in the future: clock skew or a manual DB edit.
            &(t0 + Duration::days(365)).to_rfc3339(),
        ] {
            let (_dir, store) = store_with_agents(1);
            store
                .record_post_promotion_review("agent0.default", "rev_0", bad, 0, 0, 0, 0, "[]")
                .unwrap();

            for tick in 0..5 {
                let reviewed =
                    run_post_promotion_review(&store, &cfg, t0 + Duration::seconds(5 * tick))
                        .unwrap();
                assert!(
                    reviewed.is_empty(),
                    "{bad:?} must not be reviewed (tick {tick})"
                );
            }
            assert_eq!(
                review_count(&store),
                1,
                "{bad:?} must leave the seeded row alone, not add one per tick"
            );
        }
    }

    /// Modest clock skew must self-heal: once wall-clock passes the stored value
    /// the agent becomes reviewable again on the normal schedule, with no
    /// operator intervention.
    #[test]
    fn future_timestamp_from_clock_skew_self_heals() {
        let (_dir, store) = store_with_agents(1);
        let cfg = PostPromotionReviewConfig::default();
        let t0 = Utc::now();

        // Stamped 30s ahead of t0.
        store
            .record_post_promotion_review(
                "agent0.default",
                "rev_0",
                &(t0 + Duration::seconds(30)).to_rfc3339(),
                0,
                0,
                0,
                0,
                "[]",
            )
            .unwrap();

        assert!(
            run_post_promotion_review(&store, &cfg, t0)
                .unwrap()
                .is_empty(),
            "not reviewable while the stored stamp is still in the future"
        );
        // A full interval after the stored stamp, it is due again.
        let due = run_post_promotion_review(
            &store,
            &cfg,
            t0 + Duration::seconds(30 + cfg.interval_secs as i64),
        )
        .unwrap();
        assert_eq!(due.len(), 1, "self-heals without operator intervention");
    }

    /// An `interval_secs` too large to represent must fail closed. Casting it
    /// with `as i64` would wrap to a negative Duration, making every agent look
    /// overdue — restoring the per-tick sweep this function exists to prevent.
    #[test]
    fn out_of_range_interval_reviews_nobody() {
        let (_dir, store) = store_with_agents(3);
        for interval_secs in [u64::MAX, i64::MAX as u64, i64::MAX as u64 + 1] {
            let cfg = PostPromotionReviewConfig {
                interval_secs,
                ..Default::default()
            };
            assert!(
                run_post_promotion_review(&store, &cfg, Utc::now())
                    .unwrap()
                    .is_empty(),
                "interval_secs {interval_secs} must fail closed, not sweep everything"
            );
            assert_eq!(review_count(&store), 0);
        }
    }

    #[test]
    fn retention_prunes_reviews_older_than_the_cutoff() {
        let (_dir, store) = store_with_agents(1);
        let old = (Utc::now() - Duration::days(120)).to_rfc3339();
        let recent = (Utc::now() - Duration::days(1)).to_rfc3339();
        for ts in [&old, &recent] {
            store
                .record_post_promotion_review("agent0.default", "rev_0", ts, 0, 0, 0, 0, "[]")
                .unwrap();
        }
        assert_eq!(review_count(&store), 2);

        let cutoff = Some((Utc::now() - Duration::days(90)).to_rfc3339());
        let pruned = store
            .prune_post_promotion_reviews_with_cutoff(&cutoff)
            .unwrap();
        assert_eq!(
            pruned, 1,
            "only the 120-day-old row is past a 90-day cutoff"
        );
        assert_eq!(review_count(&store), 1);
    }

    /// `0` days means retain forever, matching the other retention knobs.
    #[test]
    fn retention_zero_days_prunes_nothing() {
        let (_dir, store) = store_with_agents(1);
        store
            .record_post_promotion_review(
                "agent0.default",
                "rev_0",
                &(Utc::now() - Duration::days(400)).to_rfc3339(),
                0,
                0,
                0,
                0,
                "[]",
            )
            .unwrap();
        assert_eq!(
            store
                .prune_post_promotion_reviews_with_cutoff(&None)
                .unwrap(),
            0
        );
        assert_eq!(review_count(&store), 1);
    }
}
