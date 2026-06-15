//! Promotion safety governor (issue #25) — gate-level velocity, flapping,
//! and eval-regression checks. Tests target the governor module directly
//! against a temp `GatewayStore` + on-disk `PromotionStore`. The integration
//! through `agent.revision.promote` is exercised by existing promote tests;
//! adding governor checks to that surface is a pure additive concern.

use std::path::PathBuf;
use std::sync::Arc;

use autonoetic_gateway::runtime::promotion_governor::{
    check_eval_regression, check_flapping, check_velocity, run_governor_checks,
};
use autonoetic_gateway::runtime::promotion_store::PromotionStore;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent_revision::{
    AgentRevisionRecord, AgentRevisionStatus, PromotionKind, PromotionRecord as HistoryRecord,
};
use autonoetic_types::config::PromotionGovernorConfig;
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::promotion::{Finding, FindingSeverity, PromotionRole};
use chrono::{Duration, Utc};

fn temp_setup() -> (tempfile::TempDir, Arc<GatewayStore>, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let gateway_dir = temp.path().to_path_buf();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    (temp, store, gateway_dir)
}

fn enabled_config() -> PromotionGovernorConfig {
    PromotionGovernorConfig {
        enabled: true,
        velocity_window_hours: 24,
        max_promotions_per_window: 3,
        flapping_lookback: 4,
        eval_regression_streak: 3,
        eval_regression_lookback: 6,
    }
}

fn seed_promotion(
    store: &GatewayStore,
    agent_id: &str,
    new_revision_id: &str,
    previous_revision_id: Option<&str>,
    created_at: chrono::DateTime<Utc>,
) {
    let rec = HistoryRecord {
        promotion_id: format!("prom-{}-{}", agent_id, new_revision_id),
        kind: PromotionKind::Promote,
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        previous_revision_id: previous_revision_id.map(|s| s.to_string()),
        new_revision_id: new_revision_id.to_string(),
        source_eval_run_id: None,
        reason: None,
        created_at: created_at.to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "promotion_governor_integration".to_string(),
        origin_node_id: "gateway".to_string(),
        pre_authorization: None,
    };
    store.insert_promotion_record(&rec).unwrap();
}

fn seed_revision_with_artifact(store: &GatewayStore, agent_id: &str, revision_id: &str, artifact_id: &str) {
    let rec = AgentRevisionRecord {
        revision_id: revision_id.to_string(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: Some(artifact_id.to_string()),
        content_digest: format!("sha256:{}", revision_id),
        runtime_lock_hash: format!("sha256:lock-{}", revision_id),
        manifest_hash: format!("sha256:manifest-{}", revision_id),
        created_at: Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "promotion_governor_integration".to_string(),
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rec).unwrap();
}

fn seed_verdict_with_findings(
    promo_store: &PromotionStore,
    artifact_id: &str,
    warning_count: usize,
) {
    let findings: Vec<Finding> = (0..warning_count)
        .map(|i| Finding {
            severity: FindingSeverity::Warning,
            description: format!("seeded warning {}", i),
            evidence: Some("test".to_string()),
        })
        .collect();
    promo_store
        .record_promotion(
            artifact_id.to_string(),
            None,
            None,
            PromotionRole::Evaluator,
            "evaluator.default",
            true,
            findings,
            None,
        )
        .unwrap();
}

#[test]
fn governor_disabled_passes_unconditionally() {
    let (_temp, store, gateway_dir) = temp_setup();
    let agent_id = "agent.disabled";
    // Seed way more than the cap. With governor disabled, none of this matters.
    for i in 0..10 {
        seed_promotion(
            &store,
            agent_id,
            &format!("rev-{}", i),
            None,
            Utc::now() - Duration::hours(1),
        );
    }
    let cfg = PromotionGovernorConfig {
        enabled: false,
        ..enabled_config()
    };
    let rejection = run_governor_checks(&cfg, &store, &gateway_dir, agent_id, "rev-new").unwrap();
    assert!(rejection.is_none());
}

#[test]
fn velocity_under_cap_passes() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let agent_id = "agent.under-cap";
    let cfg = enabled_config();
    // cap = 3; seed 2 in window
    for i in 0..2 {
        seed_promotion(
            &store,
            agent_id,
            &format!("rev-{}", i),
            None,
            Utc::now() - Duration::hours(1),
        );
    }
    let rejection = check_velocity(&cfg, &store, agent_id).unwrap();
    assert!(rejection.is_none());
}

#[test]
fn velocity_at_cap_rejects() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let agent_id = "agent.at-cap";
    let cfg = enabled_config();
    for i in 0..3 {
        seed_promotion(
            &store,
            agent_id,
            &format!("rev-{}", i),
            None,
            Utc::now() - Duration::hours(1),
        );
    }
    let rejection = check_velocity(&cfg, &store, agent_id).unwrap().unwrap();
    assert_eq!(rejection.error, "promotion_velocity_exceeded");
    let p = &rejection.payload;
    assert_eq!(p["alias"], agent_id);
    assert_eq!(p["recent_promotions"], 3);
    assert_eq!(p["max_promotions_per_window"], 3);
    assert!(p["next_allowed_at"].is_string());
}

#[test]
fn velocity_only_counts_in_window() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let agent_id = "agent.window";
    let cfg = enabled_config(); // 24h window
    // 4 promotions 30h ago — outside window
    for i in 0..4 {
        seed_promotion(
            &store,
            agent_id,
            &format!("rev-old-{}", i),
            None,
            Utc::now() - Duration::hours(30),
        );
    }
    // 1 promotion 1h ago — inside window
    seed_promotion(
        &store,
        agent_id,
        "rev-recent",
        None,
        Utc::now() - Duration::hours(1),
    );
    let rejection = check_velocity(&cfg, &store, agent_id).unwrap();
    assert!(
        rejection.is_none(),
        "old promotions outside window must not count toward the cap"
    );
}

#[test]
fn flapping_re_promote_rejected() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let agent_id = "agent.flap";
    let cfg = enabled_config();
    // alias history: rev-A, rev-B, rev-C
    seed_promotion(&store, agent_id, "rev-A", None, Utc::now() - Duration::hours(3));
    seed_promotion(
        &store,
        agent_id,
        "rev-B",
        Some("rev-A"),
        Utc::now() - Duration::hours(2),
    );
    seed_promotion(
        &store,
        agent_id,
        "rev-C",
        Some("rev-B"),
        Utc::now() - Duration::hours(1),
    );
    // candidate = rev-A — already promoted recently → flapping
    let rejection = check_flapping(&cfg, &store, agent_id, "rev-A").unwrap().unwrap();
    assert_eq!(rejection.error, "promotion_flapping_detected");
    let recent = rejection.payload["recent_revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    // newest first
    assert_eq!(recent, vec!["rev-C", "rev-B", "rev-A"]);
}

#[test]
fn flapping_new_revision_allowed() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let agent_id = "agent.no-flap";
    let cfg = enabled_config();
    seed_promotion(&store, agent_id, "rev-A", None, Utc::now() - Duration::hours(2));
    seed_promotion(
        &store,
        agent_id,
        "rev-B",
        Some("rev-A"),
        Utc::now() - Duration::hours(1),
    );
    // candidate = brand-new revision → no flap
    let rejection = check_flapping(&cfg, &store, agent_id, "rev-NEW").unwrap();
    assert!(rejection.is_none());
}

#[test]
fn flapping_disabled_with_zero_lookback() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let agent_id = "agent.flap-disabled";
    let cfg = PromotionGovernorConfig {
        flapping_lookback: 0,
        ..enabled_config()
    };
    seed_promotion(&store, agent_id, "rev-A", None, Utc::now() - Duration::hours(1));
    let rejection = check_flapping(&cfg, &store, agent_id, "rev-A").unwrap();
    assert!(rejection.is_none());
}

#[test]
fn eval_regression_monotonic_rejects() {
    let (_temp, store, gateway_dir) = temp_setup();
    let agent_id = "agent.regress";
    let cfg = enabled_config(); // streak = 3, lookback = 6
    let promo_store = PromotionStore::new(&gateway_dir).unwrap();

    // Seed 4 promotions, oldest → newest with finding counts 1, 2, 3, 4
    // (4 counts → 3 strict increases → triggers streak = 3).
    let plan = [("rev-0", "art-0", 1), ("rev-1", "art-1", 2), ("rev-2", "art-2", 3), ("rev-3", "art-3", 4)];
    for (i, (rev, art, n)) in plan.iter().enumerate() {
        seed_revision_with_artifact(&store, agent_id, rev, art);
        seed_verdict_with_findings(&promo_store, art, *n);
        // hours_ago: index 0 = oldest, plan length = 4
        let hours_ago = (plan.len() - i) as i64;
        seed_promotion(&store, agent_id, rev, None, Utc::now() - Duration::hours(hours_ago));
    }
    let rejection = check_eval_regression(&cfg, &store, &gateway_dir, agent_id)
        .unwrap()
        .unwrap();
    assert_eq!(rejection.error, "promotion_eval_regression");
    let counts: Vec<u64> = rejection.payload["recent_finding_counts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    assert_eq!(counts, vec![1, 2, 3, 4]);
}

#[test]
fn eval_regression_non_monotonic_allows() {
    let (_temp, store, gateway_dir) = temp_setup();
    let agent_id = "agent.no-regress";
    let cfg = enabled_config();
    let promo_store = PromotionStore::new(&gateway_dir).unwrap();

    let plan = [
        ("rev-0", "art-0", 3),
        ("rev-1", "art-1", 2), // dip — kills the streak
        ("rev-2", "art-2", 3),
        ("rev-3", "art-3", 4),
    ];
    for (i, (rev, art, n)) in plan.iter().enumerate() {
        seed_revision_with_artifact(&store, agent_id, rev, art);
        seed_verdict_with_findings(&promo_store, art, *n);
        let hours_ago = (plan.len() - i) as i64;
        seed_promotion(&store, agent_id, rev, None, Utc::now() - Duration::hours(hours_ago));
    }
    let rejection = check_eval_regression(&cfg, &store, &gateway_dir, agent_id).unwrap();
    assert!(rejection.is_none());
}

#[test]
fn eval_regression_too_few_records_allows() {
    let (_temp, store, gateway_dir) = temp_setup();
    let agent_id = "agent.short-history";
    let cfg = enabled_config(); // streak = 3 (need streak+1 = 4 records)
    let promo_store = PromotionStore::new(&gateway_dir).unwrap();

    // Only 2 records — well under streak+1
    let plan = [("rev-0", "art-0", 1), ("rev-1", "art-1", 2)];
    for (i, (rev, art, n)) in plan.iter().enumerate() {
        seed_revision_with_artifact(&store, agent_id, rev, art);
        seed_verdict_with_findings(&promo_store, art, *n);
        let hours_ago = (plan.len() - i) as i64;
        seed_promotion(&store, agent_id, rev, None, Utc::now() - Duration::hours(hours_ago));
    }
    let rejection = check_eval_regression(&cfg, &store, &gateway_dir, agent_id).unwrap();
    assert!(rejection.is_none());
}

#[test]
fn run_governor_returns_velocity_before_flapping() {
    // When both signals would fire, the velocity check (cheapest) should
    // win. This pins ordering so the most informative rejection surfaces
    // first.
    let (_temp, store, gateway_dir) = temp_setup();
    let agent_id = "agent.both";
    let cfg = enabled_config();
    // 3 promotions in window: velocity at cap
    for i in 0..3 {
        seed_promotion(
            &store,
            agent_id,
            &format!("rev-{}", i),
            None,
            Utc::now() - Duration::hours(1),
        );
    }
    // candidate matches one of them → flapping would also fire
    let rejection = run_governor_checks(&cfg, &store, &gateway_dir, agent_id, "rev-1")
        .unwrap()
        .unwrap();
    assert_eq!(rejection.error, "promotion_velocity_exceeded");
}

#[test]
fn run_governor_returns_none_when_clear() {
    let (_temp, store, gateway_dir) = temp_setup();
    let cfg = enabled_config();
    let rejection =
        run_governor_checks(&cfg, &store, &gateway_dir, "agent.clean", "rev-fresh").unwrap();
    assert!(rejection.is_none());
}

#[test]
fn rejection_to_tool_error_shape() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let agent_id = "agent.shape";
    let cfg = enabled_config();
    for i in 0..3 {
        seed_promotion(
            &store,
            agent_id,
            &format!("rev-{}", i),
            None,
            Utc::now() - Duration::hours(1),
        );
    }
    let rejection = check_velocity(&cfg, &store, agent_id).unwrap().unwrap();
    let err = rejection.to_tool_error();
    assert_eq!(err["ok"], serde_json::Value::Bool(false));
    assert_eq!(err["error_type"], "governor");
    assert_eq!(err["error"], "promotion_velocity_exceeded");
    assert!(err["message"].as_str().unwrap().contains("velocity exceeded"));
    assert_eq!(err["recent_promotions"], 3);
    assert_eq!(err["max_promotions_per_window"], 3);
}
