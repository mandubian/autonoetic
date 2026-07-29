//! Constitution R+4 / P-6.21 — Root-session tree budget.
//!
//! Tokens, tool invocations, wall clock, and price are aggregated across all
//! descendants of a root session and enforced at the tree level, not only
//! per-session.


use autonoetic_gateway::runtime::root_session_budget::RootSessionBudgetRegistry;
use autonoetic_gateway::runtime::session_budget::SessionBudgetRegistry;
use autonoetic_types::config::{RootSessionBudgetConfig, SessionBudgetConfig};

#[test]
fn tree_budget_denies_4th_child_when_aggregate_exceeds() {
    let per_session_budget = SessionBudgetConfig {
        max_llm_rounds: Some(10),
        ..Default::default()
    };
    let tree_budget = RootSessionBudgetConfig {
        max_llm_rounds: Some(30),
        ..Default::default()
    };

    let session_reg = SessionBudgetRegistry::new(per_session_budget);
    let tree_reg = RootSessionBudgetRegistry::new(tree_budget);
    let root = "root-tree-test";

    for child in 0..3 {
        let scope = format!("{}/child-{}", root, child);
        for _ in 0..10 {
            session_reg.check_pre_llm(&scope).unwrap();
            session_reg
                .record_llm_completion(&scope, 0, 0, None)
                .unwrap();
            tree_reg.check_pre_llm(root).unwrap();
            tree_reg.reserve_llm_round(root).unwrap();
            tree_reg.record_llm_completion(root, 0, 0, None).unwrap();
        }
    }

    assert_eq!(tree_reg.snapshot_counters(root), Some((30, 0, 0.0)));

    let child_4 = format!("{}/child-4", root);
    session_reg.check_pre_llm(&child_4).unwrap();
    let result = tree_reg.reserve_llm_round(root);
    assert!(
        result.is_err(),
        "4th child should be denied — tree budget exceeded"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("max_llm_rounds"),
        "error should mention max_llm_rounds: {msg}"
    );
    assert!(msg.contains("root:"), "error should mention root: {msg}");
}

#[test]
fn per_session_allows_but_tree_denies() {
    let per_session_budget = SessionBudgetConfig {
        max_llm_tokens: Some(1000),
        ..Default::default()
    };
    let tree_budget = RootSessionBudgetConfig {
        max_llm_tokens: Some(200),
        ..Default::default()
    };

    let session_reg = SessionBudgetRegistry::new(per_session_budget);
    let tree_reg = RootSessionBudgetRegistry::new(tree_budget);
    let root = "root-tighter-tree";

    let child_1 = format!("{}/child-1", root);
    session_reg.check_pre_llm(&child_1).unwrap();
    tree_reg.check_pre_llm(root).unwrap();
    session_reg
        .record_llm_completion(&child_1, 100, 90, None)
        .unwrap();
    tree_reg.record_llm_completion(root, 100, 90, None).unwrap();

    let child_2 = format!("{}/child-2", root);
    session_reg.check_pre_llm(&child_2).unwrap();
    tree_reg.check_pre_llm(root).unwrap();
    session_reg
        .record_llm_completion(&child_2, 5, 5, None)
        .unwrap();
    tree_reg.record_llm_completion(root, 5, 5, None).unwrap();

    let tree_result = tree_reg.record_llm_completion(root, 5, 5, None);
    assert!(
        tree_result.is_err(),
        "tree should deny even when per-session allows — tighter bound wins"
    );
}

#[test]
fn tree_tool_invocations_aggregate_across_children() {
    let tree_budget = RootSessionBudgetConfig {
        max_tool_invocations: Some(10),
        ..Default::default()
    };

    let tree_reg = RootSessionBudgetRegistry::new(tree_budget);
    let root = "root-tools";

    for _child in 0..3 {
        tree_reg.reserve_tool_invocations(root, 3).unwrap();
    }

    assert!(tree_reg.reserve_tool_invocations(root, 2).is_err());
}

#[test]
fn tree_price_aggregates_across_children() {
    let tree_budget = RootSessionBudgetConfig {
        max_session_price_usd: Some(0.10),
        ..Default::default()
    };

    let tree_reg = RootSessionBudgetRegistry::new(tree_budget);
    let root = "root-price";

    tree_reg.check_pre_llm(root).unwrap();
    tree_reg
        .record_llm_completion(root, 100, 100, Some(0.04))
        .unwrap();
    tree_reg
        .record_llm_completion(root, 100, 100, Some(0.04))
        .unwrap();

    assert!(tree_reg
        .record_llm_completion(root, 100, 100, Some(0.05))
        .is_err());
}

#[test]
fn remove_tree_resets_budget() {
    let tree_budget = RootSessionBudgetConfig {
        max_llm_rounds: Some(3),
        ..Default::default()
    };

    let tree_reg = RootSessionBudgetRegistry::new(tree_budget);
    let root = "root-reset";

    for _ in 0..3 {
        tree_reg.check_pre_llm(root).unwrap();
        tree_reg.reserve_llm_round(root).unwrap();
        tree_reg.record_llm_completion(root, 0, 0, None).unwrap();
    }
    assert!(tree_reg.reserve_llm_round(root).is_err());

    tree_reg.remove_tree(root);
    tree_reg.reserve_llm_round(root).unwrap();
}

#[test]
fn disabled_tree_budget_never_blocks() {
    let tree_reg = RootSessionBudgetRegistry::new(RootSessionBudgetConfig::default());
    assert!(!tree_reg.is_enabled());

    for _ in 0..100 {
        tree_reg.check_pre_llm("root").unwrap();
        tree_reg
            .record_llm_completion("root", u64::MAX, u64::MAX, Some(f64::MAX))
            .unwrap();
        tree_reg.reserve_tool_invocations("root", u64::MAX).unwrap();
    }
}
