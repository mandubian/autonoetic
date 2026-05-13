//! Promotion safety governor (issue #25).
//!
//! Gate-level checks applied at `agent.revision.promote` *before* the actual
//! `atomic_promote` write. Three independent signals:
//!
//! 1. **Velocity** — too many promotions per alias inside a window.
//! 2. **Flapping** — re-promoting a revision that was already promoted recently
//!    on this alias (A→B→A or A→B→A→B pattern).
//! 3. **Eval-regression** — the per-revision count of non-info findings is
//!    monotonically increasing across recent promotions.
//!
//! All three are gated on the same `PromotionGovernorConfig.enabled` flag and
//! all three are bypassable via an explicit operator force (`force: true` +
//! required `force_reason`). Bypass emits a `governor.override` causal event.
//!
//! Operator-side throughput safeguards on the federation escalation channel
//! (also discussed in issue #25) are a separate concern handled outside this
//! module — see `promotion-federation-followup-review.md` §5.

use std::path::Path;

use anyhow::Result;
use autonoetic_types::config::PromotionGovernorConfig;
use autonoetic_types::promotion::{Finding, FindingSeverity};

use crate::scheduler::gateway_store::GatewayStore;

/// Structured rejection returned by the governor. Convert to the tool's
/// JSON error response via [`GovernorRejection::to_tool_error`].
#[derive(Debug, Clone)]
pub struct GovernorRejection {
    /// Stable machine-readable error code (e.g. `promotion_velocity_exceeded`).
    pub error: &'static str,
    /// Human-readable message included in the tool response.
    pub message: String,
    /// Hint shown to the caller.
    pub repair_hint: String,
    /// Extra fields merged into the JSON response (e.g. `recent_promotions`,
    /// `next_allowed_at`, `recent_revisions`).
    pub payload: serde_json::Value,
}

impl GovernorRejection {
    /// Render the rejection as the `{ok: false, error_type: "governor", ...}`
    /// shape the promote tool returns to the caller.
    pub fn to_tool_error(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("ok".to_string(), serde_json::Value::Bool(false));
        obj.insert(
            "error_type".to_string(),
            serde_json::Value::String("governor".to_string()),
        );
        obj.insert(
            "error".to_string(),
            serde_json::Value::String(self.error.to_string()),
        );
        obj.insert(
            "message".to_string(),
            serde_json::Value::String(self.message.clone()),
        );
        obj.insert(
            "repair_hint".to_string(),
            serde_json::Value::String(self.repair_hint.clone()),
        );
        if let serde_json::Value::Object(extra) = &self.payload {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        serde_json::Value::Object(obj)
    }
}

/// Run all enabled governor checks. Returns the first rejection found, or
/// `Ok(None)` when promotion may proceed (also `None` when the governor is
/// disabled or the caller passed `force=true`).
pub fn run_governor_checks(
    config: &PromotionGovernorConfig,
    store: &GatewayStore,
    gateway_dir: &Path,
    agent_id: &str,
    revision_id: &str,
) -> Result<Option<GovernorRejection>> {
    if !config.enabled {
        return Ok(None);
    }
    if let Some(r) = check_velocity(config, store, agent_id)? {
        return Ok(Some(r));
    }
    if let Some(r) = check_flapping(config, store, agent_id, revision_id)? {
        return Ok(Some(r));
    }
    if let Some(r) = check_eval_regression(config, store, gateway_dir, agent_id)? {
        return Ok(Some(r));
    }
    Ok(None)
}

/// Velocity check: count `Promote` rows for the alias in the configured
/// window; reject when the count >= the configured cap.
pub fn check_velocity(
    config: &PromotionGovernorConfig,
    store: &GatewayStore,
    agent_id: &str,
) -> Result<Option<GovernorRejection>> {
    if config.max_promotions_per_window == 0 || config.velocity_window_hours == 0 {
        return Ok(None);
    }
    let window = chrono::Duration::hours(config.velocity_window_hours as i64);
    let since = chrono::Utc::now() - window;
    let since_rfc = since.to_rfc3339();
    let count = store.count_promotions_since(agent_id, &since_rfc)?;
    if count < config.max_promotions_per_window {
        return Ok(None);
    }
    let next_allowed_at = compute_next_allowed_at(store, agent_id, config)?;
    Ok(Some(GovernorRejection {
        error: "promotion_velocity_exceeded",
        message: format!(
            "Promotion velocity exceeded for alias '{}': {} promotions in the last {}h \
             (cap = {}). Wait until the oldest in-window promotion ages out, or retry \
             with `force: true` and `force_reason`.",
            agent_id, count, config.velocity_window_hours, config.max_promotions_per_window
        ),
        repair_hint: "Reduce promotion cadence, or pass `force: true` + `force_reason` to override."
            .to_string(),
        payload: serde_json::json!({
            "alias": agent_id,
            "window_hours": config.velocity_window_hours,
            "recent_promotions": count,
            "max_promotions_per_window": config.max_promotions_per_window,
            "next_allowed_at": next_allowed_at,
        }),
    }))
}

/// Flapping check: scan the most recent `flapping_lookback` promotion rows.
/// If the candidate `revision_id` appears among them, it means the alias is
/// being moved *back* to a revision already promoted in the recent past —
/// the canonical A→B→A oscillation signal.
pub fn check_flapping(
    config: &PromotionGovernorConfig,
    store: &GatewayStore,
    agent_id: &str,
    candidate_revision_id: &str,
) -> Result<Option<GovernorRejection>> {
    if config.flapping_lookback == 0 {
        return Ok(None);
    }
    let history = store.list_promotion_history(agent_id)?;
    let recent: Vec<_> = history
        .into_iter()
        .filter(|r| matches!(r.kind, autonoetic_types::agent_revision::PromotionKind::Promote))
        .take(config.flapping_lookback)
        .collect();

    if !recent
        .iter()
        .any(|r| r.new_revision_id == candidate_revision_id)
    {
        return Ok(None);
    }

    let recent_revs: Vec<String> = recent.iter().map(|r| r.new_revision_id.clone()).collect();
    Ok(Some(GovernorRejection {
        error: "promotion_flapping_detected",
        message: format!(
            "Promotion flapping detected: revision '{}' was already promoted to alias '{}' \
             within the last {} promotions. Repeated A→B→A oscillation is a runaway-evolution \
             signal; halt and investigate.",
            candidate_revision_id, agent_id, config.flapping_lookback
        ),
        repair_hint:
            "Investigate why this revision is being re-promoted. If the move is intentional \
             (e.g. validated rollback-then-redo), retry with `force: true` + `force_reason`."
                .to_string(),
        payload: serde_json::json!({
            "alias": agent_id,
            "candidate_revision_id": candidate_revision_id,
            "lookback": config.flapping_lookback,
            "recent_revisions": recent_revs,
        }),
    }))
}

/// Eval-regression check: walk the most recent promotions, look up each
/// revision's verdict findings, and count `eval_regression_streak`
/// consecutive monotonic increases in the non-info finding count. The
/// signal: even when the boolean `evaluator_pass` is `true`, a steady
/// rise in warning/error findings means quality is drifting downward.
pub fn check_eval_regression(
    config: &PromotionGovernorConfig,
    store: &GatewayStore,
    gateway_dir: &Path,
    agent_id: &str,
) -> Result<Option<GovernorRejection>> {
    if config.eval_regression_streak == 0 || config.eval_regression_lookback == 0 {
        return Ok(None);
    }
    let history = store.list_promotion_history(agent_id)?;
    let recent: Vec<_> = history
        .into_iter()
        .filter(|r| matches!(r.kind, autonoetic_types::agent_revision::PromotionKind::Promote))
        .take(config.eval_regression_lookback)
        .collect();

    // promotion_store opens the on-disk JSON each call; cheap enough for
    // the bounded lookback (≤ a few rows).
    let promo_store = crate::runtime::promotion_store::PromotionStore::new(gateway_dir)?;

    // Order recent[] is newest-first; reverse to oldest-first for monotonic
    // comparison.
    let mut counts: Vec<usize> = Vec::with_capacity(recent.len());
    for entry in recent.iter().rev() {
        let rev = match store.get_agent_revision(&entry.new_revision_id)? {
            Some(r) => r,
            None => continue,
        };
        let artifact_id = match rev.artifact_id.as_deref() {
            Some(a) => a,
            None => continue,
        };
        let record = match promo_store.get_promotion(artifact_id) {
            Some(r) => r,
            None => continue,
        };
        counts.push(non_info_finding_count(&record));
    }

    if counts.len() < config.eval_regression_streak + 1 {
        return Ok(None);
    }

    let need = config.eval_regression_streak;
    // Look at the last `need + 1` counts and check strict monotonic increase
    // across the trailing window: c[k-need-1] < c[k-need] < ... < c[k-1].
    let tail = &counts[counts.len() - (need + 1)..];
    let monotonic = tail.windows(2).all(|w| w[0] < w[1]);
    if !monotonic {
        return Ok(None);
    }

    Ok(Some(GovernorRejection {
        error: "promotion_eval_regression",
        message: format!(
            "Promotion eval-regression halt for alias '{}': non-info finding counts \
             have strictly increased {} times in a row ({:?}). Even with `evaluator_pass=true`, \
             this is degrading quality across revisions.",
            agent_id, need, tail
        ),
        repair_hint:
            "Inspect the last promotions' findings, address the worsening signals, then retry. \
             Use `force: true` + `force_reason` to override after operator review."
                .to_string(),
        payload: serde_json::json!({
            "alias": agent_id,
            "streak_threshold": need,
            "recent_finding_counts": tail,
        }),
    }))
}

/// Emit a `governor.rejected` causal event so the audit trail records the
/// rejection (the tool response is also returned to the caller, but the
/// event makes the rejection queryable later).
pub fn emit_rejected_event(
    store: &GatewayStore,
    caller_agent_id: &str,
    session_id: Option<&str>,
    target_agent_id: &str,
    revision_id: &str,
    rejection: &GovernorRejection,
) {
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("gov-rej-{}", uuid::Uuid::new_v4()),
        agent_id: caller_agent_id.to_string(),
        session_id: session_id.unwrap_or("").to_string(),
        turn_id: None,
        event_seq: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "revision".to_string(),
        action: "governor.rejected".to_string(),
        status: "blocked".to_string(),
        enforced_rules: Vec::new(),
        target: Some(target_agent_id.to_string()),
        payload: Some(
            serde_json::json!({
                "alias": target_agent_id,
                "revision_id": revision_id,
                "error": rejection.error,
                "payload": rejection.payload,
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: Some(rejection.message.clone()),
    };
    let _ = store.create_causal_event(&event);
}

/// Emit a `governor.override` causal event whenever the governor is bypassed
/// via `force: true`. Records the operator-supplied reason so the bypass is
/// always traceable.
pub fn emit_override_event(
    store: &GatewayStore,
    caller_agent_id: &str,
    session_id: Option<&str>,
    target_agent_id: &str,
    revision_id: &str,
    force_reason: &str,
) {
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("gov-ovr-{}", uuid::Uuid::new_v4()),
        agent_id: caller_agent_id.to_string(),
        session_id: session_id.unwrap_or("").to_string(),
        turn_id: None,
        event_seq: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "revision".to_string(),
        action: "governor.override".to_string(),
        status: "active".to_string(),
        enforced_rules: Vec::new(),
        target: Some(target_agent_id.to_string()),
        payload: Some(
            serde_json::json!({
                "alias": target_agent_id,
                "revision_id": revision_id,
                "force_reason": force_reason,
            })
            .to_string(),
        ),
        payload_ref: None,
        evidence_ref: None,
        reason: Some(format!("operator override: {}", force_reason)),
    };
    let _ = store.create_causal_event(&event);
}

fn non_info_finding_count(record: &autonoetic_types::promotion::PromotionRecord) -> usize {
    let buckets: [&Vec<Finding>; 5] = [
        &record.evaluator_findings,
        &record.auditor_findings,
        &record.static_evaluator_findings,
        &record.unit_test_runner_findings,
        &record.sealed_evaluator_findings,
    ];
    buckets
        .iter()
        .flat_map(|v| v.iter())
        .filter(|f| !matches!(f.severity, FindingSeverity::Info))
        .count()
}

/// Best-effort `next_allowed_at`: the timestamp at which the oldest
/// in-window promotion will age out, freeing one slot.
fn compute_next_allowed_at(
    store: &GatewayStore,
    agent_id: &str,
    config: &PromotionGovernorConfig,
) -> Result<Option<String>> {
    let history = store.list_promotion_history(agent_id)?;
    let window = chrono::Duration::hours(config.velocity_window_hours as i64);
    let since = chrono::Utc::now() - window;
    let mut in_window: Vec<_> = history
        .into_iter()
        .filter(|r| matches!(r.kind, autonoetic_types::agent_revision::PromotionKind::Promote))
        .filter_map(|r| {
            chrono::DateTime::parse_from_rfc3339(&r.created_at)
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc))
                .filter(|t| *t >= since)
        })
        .collect();
    in_window.sort();
    if in_window.len() < config.max_promotions_per_window {
        return Ok(None);
    }
    // Once the oldest in-window promotion is older than `window`, it no
    // longer counts. So the next allowed slot is at `oldest + window`.
    let oldest = in_window.first().copied();
    Ok(oldest.map(|t| (t + window).to_rfc3339()))
}
