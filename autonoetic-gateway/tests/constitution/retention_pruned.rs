//! Constitution R+17 / P-8.17: Retention pruning emits causal event.
//!
//! When data retention is applied, the gateway must emit a single
//! `retention.pruned` causal event per batch with counts and bounds.
//! Without this, pruning is invisible and auditors cannot verify that
//! data lifecycle is being followed.


use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::causal_chain::default_enforced_rules;
use autonoetic_types::config::RetentionConfig;

#[test]
fn r_plus_17_retention_pruned_event_emitted() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let retention = RetentionConfig {
        execution_traces_days: 0,
        causal_events_days: 0,
        post_promotion_reviews_days: 0,
    };
    store.apply_retention_policy(&retention)?;

    let events = store.search_causal_events(None, None, 50)?;
    let pruned = events
        .iter()
        .find(|e| e.category == "retention" && e.action == "pruned");
    assert!(
        pruned.is_none(),
        "no retention.pruned event when nothing is pruned (0 days = keep all)"
    );

    Ok(())
}

#[test]
fn r_plus_17_retention_pruned_event_contains_counts() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: "test.agent".to_string(),
        session_id: "sess-old".to_string(),
        turn_id: None,
        event_seq: 1,
        timestamp: "2020-01-01T00:00:00Z".to_string(),
        category: "test".to_string(),
        action: "old_event".to_string(),
        status: "success".to_string(),
        enforced_rules: default_enforced_rules(),
        target: None,
        payload: None,
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    })?;

    let retention = RetentionConfig {
        execution_traces_days: 0,
        causal_events_days: 1,
        post_promotion_reviews_days: 0,
    };
    store.apply_retention_policy(&retention)?;

    let events = store.search_causal_events(None, None, 50)?;
    let pruned = events
        .iter()
        .find(|e| e.category == "retention" && e.action == "pruned");

    assert!(
        pruned.is_some(),
        "retention.pruned event must be emitted after pruning"
    );

    let pruned = pruned.unwrap();
    let payload: serde_json::Value =
        serde_json::from_str(pruned.payload.as_deref().expect("payload present"))?;

    assert!(
        payload.get("causal_events_pruned").is_some(),
        "payload must contain causal_events_pruned count"
    );
    assert!(
        payload.get("execution_traces_pruned").is_some(),
        "payload must contain execution_traces_pruned count"
    );
    assert!(
        payload.get("retention_config").is_some(),
        "payload must contain retention_config"
    );
    assert_eq!(payload["retention_config"]["causal_events_days"], 1);

    assert!(
        pruned.enforced_rules.iter().any(|r| r == "P-8.17"),
        "retention.pruned event must cite P-8.17 in enforced_rules"
    );

    Ok(())
}

#[test]
fn r_plus_17_retention_pruned_event_actor_is_gateway() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: "old.agent".to_string(),
        session_id: "old-sess".to_string(),
        turn_id: None,
        event_seq: 1,
        timestamp: "2020-01-01T00:00:00Z".to_string(),
        category: "test".to_string(),
        action: "old".to_string(),
        status: "success".to_string(),
        enforced_rules: default_enforced_rules(),
        target: None,
        payload: None,
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    })?;

    let retention = RetentionConfig {
        execution_traces_days: 0,
        causal_events_days: 1,
        post_promotion_reviews_days: 0,
    };
    store.apply_retention_policy(&retention)?;

    let events = store.search_causal_events(None, None, 50)?;
    let pruned = events
        .iter()
        .find(|e| e.category == "retention" && e.action == "pruned")
        .expect("pruned event must exist");

    assert_eq!(
        pruned.agent_id, "gateway",
        "retention.pruned must be attributed to gateway, not any agent"
    );
    assert_eq!(
        pruned.session_id, "system",
        "retention.pruned session must be 'system'"
    );

    Ok(())
}

#[test]
fn r_plus_17_zero_days_means_no_pruning() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    store.create_causal_event(&autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: "test.agent".to_string(),
        session_id: "sess-1".to_string(),
        turn_id: None,
        event_seq: 1,
        timestamp: "2020-01-01T00:00:00Z".to_string(),
        category: "test".to_string(),
        action: "event".to_string(),
        status: "success".to_string(),
        enforced_rules: default_enforced_rules(),
        target: None,
        payload: None,
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    })?;

    let retention = RetentionConfig {
        execution_traces_days: 0,
        causal_events_days: 0,
        post_promotion_reviews_days: 0,
    };
    store.apply_retention_policy(&retention)?;

    let events = store.search_causal_events(None, None, 50)?;
    assert_eq!(
        events.len(),
        1,
        "only the original event should exist, no pruned event"
    );

    Ok(())
}

/// #1046: `post_promotion_reviews` is subject to the same retention policy as
/// the events it summarises, and its pruning is auditable in the same
/// `retention.pruned` event. Before this the table had no retention at all
/// while being the fastest-growing table in the store.
#[test]
fn r_plus_17_post_promotion_reviews_are_pruned_and_reported() -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let gateway_dir = tempdir.path().join(".gateway");
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir)?);

    let old = (chrono::Utc::now() - chrono::Duration::days(200)).to_rfc3339();
    let recent = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    for ts in [&old, &recent] {
        store.record_post_promotion_review("test.agent", "rev_1", ts, 0, 0, 0, 0, "[]")?;
    }
    assert_eq!(store.list_post_promotion_reviews(None, 100)?.len(), 2);

    let retention = RetentionConfig {
        execution_traces_days: 0,
        causal_events_days: 0,
        post_promotion_reviews_days: 90,
    };
    store.apply_retention_policy(&retention)?;

    let remaining = store.list_post_promotion_reviews(None, 100)?;
    assert_eq!(remaining.len(), 1, "the 200-day-old review must be pruned");
    assert_eq!(remaining[0].reviewed_at, recent);

    // Pruning must be visible in the audit trail, not silent.
    let events = store.search_causal_events(None, None, 50)?;
    let pruned = events
        .iter()
        .find(|e| e.category == "retention" && e.action == "pruned")
        .expect("retention.pruned event must be emitted");
    let payload: serde_json::Value =
        serde_json::from_str(pruned.payload.as_deref().unwrap_or("{}"))?;
    assert_eq!(payload["post_promotion_reviews_pruned"], 1);
    assert_eq!(
        payload["retention_config"]["post_promotion_reviews_days"], 90,
        "the applied policy must be recorded alongside the count"
    );

    Ok(())
}
