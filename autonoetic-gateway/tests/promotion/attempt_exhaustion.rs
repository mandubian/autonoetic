//! Durable cross-session promotion-attempt ledger (issue #720).
//!
//! The ledger records terminal outcomes of `agent.revision.promote` calls
//! (gate rejections and successful promotions), keyed by `content_digest` so
//! a rebuilt identical revision shares the same budget. After
//! `max_promotion_attempts_per_revision` rejected attempts for the same
//! `(alias, digest)`, further attempts are blocked with
//! `promotion_attempts_exhausted` until an operator ack resets the counter.

use std::path::PathBuf;
use std::sync::Arc;

use autonoetic_gateway::runtime::promotion_governor::{
    check_attempt_exhaustion, record_rejected_attempt, run_governor_checks,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::PromotionGovernorConfig;

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
        max_promotions_per_window: 100,
        flapping_lookback: 0,
        eval_regression_streak: 0,
        eval_regression_lookback: 0,
        max_promotion_attempts_per_revision: 3,
    }
}

fn seed_rejected_attempt(
    store: &GatewayStore,
    alias_id: &str,
    revision_id: &str,
    content_digest: &str,
    session_id: &str,
) {
    store
        .record_promotion_attempt(
            &format!("patt-{}-{}", session_id, revision_id),
            alias_id,
            revision_id,
            content_digest,
            "rejected",
            Some("governor"),
            Some("promotion_velocity_exceeded"),
            Some(session_id),
            None,
        )
        .unwrap();
}

#[test]
fn same_digest_rejected_n_times_blocks_next_attempt() {
    let (_temp, store, gateway_dir) = temp_setup();
    let alias_id = "agent.exhaust";
    let digest = "sha256:same-digest";
    let cfg = enabled_config();

    // Two distinct root sessions each reject once.
    seed_rejected_attempt(&store, alias_id, "rev-a", digest, "root-session-1");
    seed_rejected_attempt(&store, alias_id, "rev-b", digest, "root-session-2");

    // Third rejection should trigger exhaustion.
    seed_rejected_attempt(&store, alias_id, "rev-c", digest, "root-session-3");

    let rejection = check_attempt_exhaustion(&cfg, &store, alias_id, digest)
        .unwrap()
        .unwrap();
    assert_eq!(rejection.error, "promotion_attempts_exhausted");
    assert_eq!(rejection.payload["rejected_attempts"], 3);
    assert_eq!(rejection.payload["rule_id"], "P-2.29");

    // A further rejection is blocked transactionally by record_rejected_attempt
    // (the count read and insert are serialized on the same SQLite connection).
    let rejection = record_rejected_attempt(
        &cfg,
        &store,
        alias_id,
        "rev-next",
        digest,
        Some("governor"),
        Some("promotion_velocity_exceeded"),
        Some("root-session-4"),
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(rejection.error, "promotion_attempts_exhausted");
    assert_eq!(rejection.payload["rejected_attempts"], 3);

    // run_governor_checks no longer performs a separate non-transactional
    // exhaustion read; with velocity/flapping/eval-regression disabled it
    // returns None.
    let result = run_governor_checks(
        &cfg,
        &store,
        &gateway_dir,
        alias_id,
        "rev-next",
        Some(digest),
    )
    .unwrap();
    assert!(result.is_none());
}

#[test]
fn new_digest_gets_fresh_budget() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let alias_id = "agent.fresh-digest";
    let cfg = enabled_config();

    for i in 0..3 {
        seed_rejected_attempt(
            &store,
            alias_id,
            &format!("rev-{}", i),
            "sha256:old-digest",
            &format!("root-session-{}", i),
        );
    }

    // Old digest is exhausted.
    assert!(
        check_attempt_exhaustion(&cfg, &store, alias_id, "sha256:old-digest")
            .unwrap()
            .is_some()
    );

    // New digest for the same alias is not.
    assert!(
        check_attempt_exhaustion(&cfg, &store, alias_id, "sha256:new-digest")
            .unwrap()
            .is_none()
    );
}

#[test]
fn operator_ack_resets_counter() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let alias_id = "agent.reset";
    let digest = "sha256:reset-digest";
    let cfg = enabled_config();

    for i in 0..3 {
        seed_rejected_attempt(
            &store,
            alias_id,
            &format!("rev-{}", i),
            digest,
            &format!("root-session-{}", i),
        );
    }
    assert!(
        check_attempt_exhaustion(&cfg, &store, alias_id, digest)
            .unwrap()
            .is_some()
    );

    let deleted = store.reset_promotion_attempts(alias_id, digest).unwrap();
    assert_eq!(deleted, 3);

    // After reset, promotion may proceed.
    assert!(
        check_attempt_exhaustion(&cfg, &store, alias_id, digest)
            .unwrap()
            .is_none()
    );
}

#[test]
fn successful_promote_after_two_rejections_passes() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let alias_id = "agent.promote-after-rejects";
    let digest = "sha256:recover-digest";
    let cfg = enabled_config();

    // Two rejected attempts leave room for one more attempt.
    for i in 0..2 {
        seed_rejected_attempt(
            &store,
            alias_id,
            &format!("rev-{}", i),
            digest,
            &format!("root-session-{}", i),
        );
    }

    assert!(
        check_attempt_exhaustion(&cfg, &store, alias_id, digest)
            .unwrap()
            .is_none()
    );

    // Record a successful promotion.
    store
        .record_promotion_attempt(
            "patt-success",
            alias_id,
            "rev-success",
            digest,
            "promoted",
            None,
            None,
            Some("root-session-success"),
            None,
        )
        .unwrap();

    // Ledger contains the promoted row but no new rejections, so a subsequent
    // identical-digest attempt is still allowed (rejection count did not grow).
    assert!(
        check_attempt_exhaustion(&cfg, &store, alias_id, digest)
            .unwrap()
            .is_none()
    );
}

#[test]
fn cap_of_zero_disables_exhaustion_check() {
    let (_temp, store, _gateway_dir) = temp_setup();
    let alias_id = "agent.zero-cap";
    let digest = "sha256:any";

    for i in 0..5 {
        seed_rejected_attempt(
            &store,
            alias_id,
            &format!("rev-{}", i),
            digest,
            &format!("root-session-{}", i),
        );
    }

    let mut cfg = enabled_config();
    cfg.max_promotion_attempts_per_revision = 0;

    assert!(
        check_attempt_exhaustion(&cfg, &store, alias_id, digest)
            .unwrap()
            .is_none()
    );
}
