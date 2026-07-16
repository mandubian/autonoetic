//! DISCRETION LEAK register — citizenship RFC Part D.3 (#771).
//!
//! The constitution names the gateway's own judgment lapses in prose at
//! their site (P-5.2: the gateway reshaping agent input; P-5.8: the gateway
//! driving repair of agent output) — named debts, not accepted behavior
//! (`docs/philosophy.md` §5.4). Until now they were not *monitored*: the
//! M1 doctrine (#619) made every normalization observable in traces via
//! `note_llm_normalization`, but traces are ephemeral and uncounted. This
//! module is the register: every recorded normalization/repair intervention
//! becomes a durable `discretion_leak` causal event carrying the rule ID of
//! the closest named constitutional site, so "top leaks this window" is a
//! queryable standing agenda (Fuller's congruence requirement applied to
//! the enforcer's own improvisations) instead of a prose footnote.
//!
//! ## Plumbing (ambient task context)
//!
//! The normalization chokepoints are deliberately deep library code (serde
//! deserializers, the fuzzy-match engine, fence stripping) where threading
//! a store + session context through every signature would be invasive and
//! — worse — patchy: any future call site that forgot to thread context
//! would silently drop out of the register. Instead the executor installs
//! a [`tokio::task_local`] scope ([`LeakScope`]) around the task regions
//! where leaks can occur (tool-call processing, response validation), and
//! [`record_discretion_leak`] reads it. A leak outside any scope still
//! emits its trace line (observability is never lost); it simply cannot be
//! attributed durably. Task-locals survive `.await` points within the same
//! task and are invisible to sibling tasks, so concurrent sessions never
//! cross-attribute.
//!
//! Event shape: `category = "discretion_leak"`, `action = <stable kind>`,
//! `status = "recorded"` (never DENIED/ERROR — a leak is not an agent
//! failure and must not page the policy-decision hooks), `enforced_rules`
//! naming the closest constitutional site.

use autonoetic_types::causal_chain::CausalEventRecord;

tokio::task_local! {
    /// Ambient per-task context for durable leak attribution. Installed by
    /// the executor around tool-call processing and response validation.
    static LEAK_SCOPE: LeakScope
}

/// Context needed to attribute a discretion leak durably: who was on stage
/// when the gateway exercised judgment, and where to record it.
#[derive(Clone)]
pub struct LeakScope {
    pub store: std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
}

impl LeakScope {
    pub fn new(
        store: std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
        agent_id: impl Into<String>,
        session_id: impl Into<String>,
        turn_id: Option<String>,
    ) -> Self {
        Self {
            store,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            turn_id,
        }
    }
}

/// Run `future` with `scope` installed as the ambient leak context. Tool
/// execution and response validation are wrapped in this so any
/// normalization/repair inside records durably.
pub async fn with_leak_scope<F: std::future::Future>(
    scope: LeakScope,
    future: F,
) -> F::Output {
    LEAK_SCOPE.scope(scope, future).await
}

/// Record a discretion leak: the structured trace line (M1 doctrine —
/// tolerance is observable, never silent) plus, when an ambient
/// [`LeakScope`] is installed, a durable `discretion_leak` causal event.
/// `kind` is a stable label (it becomes the event's `action`);
/// `enforced_rules` names the closest constitutional site (P-5.2 input
/// reshaping, P-5.8 output repair); `detail` must already be redacted —
/// never raw tool arguments or reply bodies (they can carry secrets).
///
/// Best-effort: a failed durable write degrades to trace-only with a
/// warning, mirroring the causal-event idiom elsewhere — the register must
/// never break the execution it observes.
pub(crate) fn record_discretion_leak(
    kind: &'static str,
    detail: &str,
    enforced_rules: &[&'static str],
) {
    tracing::info!(
        target: "llm_normalization",
        kind,
        detail,
        "tolerated model non-conformance ({})",
        kind
    );

    let recorded = LEAK_SCOPE.try_with(|scope| {
        let event = CausalEventRecord {
            event_id: format!("leak-{}", uuid::Uuid::new_v4()),
            agent_id: scope.agent_id.clone(),
            session_id: scope.session_id.clone(),
            turn_id: scope.turn_id.clone(),
            event_seq: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            category: "discretion_leak".to_string(),
            action: kind.to_string(),
            status: "recorded".to_string(),
            enforced_rules: enforced_rules.iter().map(|r| r.to_string()).collect(),
            target: None,
            payload: Some(
                serde_json::json!({
                    "kind": kind,
                    "detail": detail,
                })
                .to_string(),
            ),
            payload_ref: None,
            evidence_ref: None,
            reason: Some(
                "gateway exercised judgment reserved to the agent or to pre-committed law (§5.4)"
                    .to_string(),
            ),
        };
        scope.store.create_causal_event(&event)
    });

    match recorded {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(
                target: "discretion_leak",
                kind,
                error = %e,
                "Failed to record discretion leak durably — trace-only"
            );
        }
        Err(_) => {
            // Outside any LeakScope (driver-boundary paths, tests, CLI
            // tools): the trace line above is the record. Not a warn — this
            // is an expected configuration, not a failure.
            tracing::debug!(
                target: "discretion_leak",
                kind,
                "leak outside any LeakScope — trace-only"
            );
        }
    }
}

/// Read-side tally type for the "top leaks this window" standing agenda,
/// returned by
/// [`crate::scheduler::gateway_store::GatewayStore::discretion_leak_summary`].
/// Grouped by (rule, kind) so the steward office (RFC Part F) can see the
/// *shape* of the enforcer's improvisations, not just their volume.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DiscretionLeakTally {
    pub rule_id: String,
    pub kind: String,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store() -> (tempfile::TempDir, std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            crate::scheduler::gateway_store::GatewayStore::open(dir.path()).unwrap(),
        );
        (dir, store)
    }

    #[tokio::test]
    async fn leak_inside_scope_records_durable_event() {
        let (_dir, store) = open_store();
        let scope = LeakScope::new(store.clone(), "coder.default", "sess-1", None);

        with_leak_scope(scope, async {
            record_discretion_leak("lenient_string_coercion", "coerced scalar", &["P-5.2"]);
        })
        .await;

        let events = store
            .search_causal_events(None, Some("coder.default"), 50)
            .unwrap();
        let leaks: Vec<_> = events
            .iter()
            .filter(|e| e.category == "discretion_leak")
            .collect();
        assert_eq!(leaks.len(), 1);
        assert_eq!(leaks[0].action, "lenient_string_coercion");
        assert_eq!(leaks[0].status, "recorded");
        assert_eq!(leaks[0].enforced_rules, vec!["P-5.2".to_string()]);
        assert_eq!(leaks[0].session_id, "sess-1");
    }

    #[tokio::test]
    async fn leak_outside_scope_is_trace_only() {
        let (_dir, store) = open_store();
        // No scope installed: must not panic, must not record.
        record_discretion_leak("fuzzy_patch_match", "matched fuzzily", &["P-5.2"]);
        let events = store.search_causal_events(None, None, 50).unwrap();
        assert!(events.iter().all(|e| e.category != "discretion_leak"));
    }

    #[tokio::test]
    async fn nested_scopes_attribute_to_inner() {
        let (_dir, store) = open_store();
        let outer = LeakScope::new(store.clone(), "outer.agent", "sess-outer", None);
        let inner = LeakScope::new(store.clone(), "inner.agent", "sess-inner", None);

        with_leak_scope(outer, async {
            record_discretion_leak("k_outer", "d", &["P-5.2"]);
            with_leak_scope(inner, async {
                record_discretion_leak("k_inner", "d", &["P-5.2"]);
            })
            .await;
            record_discretion_leak("k_outer_again", "d", &["P-5.2"]);
        })
        .await;

        let inner_events = store
            .search_causal_events(None, Some("inner.agent"), 50)
            .unwrap();
        assert_eq!(inner_events.len(), 1);
        assert_eq!(inner_events[0].action, "k_inner");

        let outer_events = store
            .search_causal_events(None, Some("outer.agent"), 50)
            .unwrap();
        assert_eq!(outer_events.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_tasks_do_not_cross_attribute() {
        let (_dir, store) = open_store();
        let mut handles = Vec::new();
        for i in 0..4 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                let agent = format!("agent-{i}");
                let scope = LeakScope::new(store, agent.clone(), format!("sess-{i}"), None);
                with_leak_scope(scope, async move {
                    // Yield so tasks interleave on the worker threads.
                    tokio::task::yield_now().await;
                    record_discretion_leak("lenient_string_coercion", "d", &["P-5.2"]);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        for i in 0..4 {
            let events = store
                .search_causal_events(None, Some(&format!("agent-{i}")), 50)
                .unwrap();
            assert_eq!(
                events.iter().filter(|e| e.category == "discretion_leak").count(),
                1,
                "agent-{i} must have exactly its own leak"
            );
        }
    }
}
