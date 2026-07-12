//! Static, pre-committed table mapping a permission denial's enforced rule
//! IDs to the agent's lawful next moves (Ri-0.3 named rejection: the agent
//! finds its next lawful move inside the denial itself). The gateway is a
//! Lawful Executor — it maps rule IDs to affordances mechanically here, it
//! never judges which move the agent should take.

use autonoetic_types::tool_error::AvailableAction;

/// Missing-capability mapping for select policy rule IDs, used only to make
/// the `delegate` action's description more specific. Verified against the
/// `PolicyDecision::deny`/`deny_with_analysis` literals in `policy.rs`.
fn capability_for_rule(rule_id: &str) -> Option<&'static str> {
    match rule_id {
        "P-1.5" => Some("NetworkAccess"),
        "P-1.9" => Some("CodeExecution"),
        "P-1.7" => Some("AgentSpawn"),
        "P-1.4" => Some("ReadAccess/WriteAccess"),
        "P-1.3" => Some("AgentRevision"),
        "P-11.5" => Some("AgentMessage"),
        "P-1.8" => Some("CredentialAccess"),
        "P-2.20" => Some("GateDecider"),
        _ => None,
    }
}

/// The lawful next moves available to an agent from inside a permission
/// denial. Baseline actions are attached to every denial, in this order:
/// propose an amendment, delegate to a capable agent, or inspect itself.
///
/// Deliberately omits an "escalate" action: P-2.21 escalation currently has
/// no agent-callable tool (`HumanGateService::escalate_to_human` is
/// service-level only). Add one here once such a tool exists.
pub fn available_actions_for_rules(enforced_rules: &[String]) -> Vec<AvailableAction> {
    let delegate_description = enforced_rules
        .iter()
        .find_map(|rule| capability_for_rule(rule))
        .map(|cap| {
            format!(
                "Find an installed agent that declares {cap} (agent_discover) and delegate the step to it (agent_spawn).",
            )
        })
        .unwrap_or_else(|| {
            "Find an installed agent that holds the needed capability (agent_discover) and delegate the step to it (agent_spawn).".to_string()
        });

    vec![
        AvailableAction {
            action: "propose_amendment".to_string(),
            description: "If you believe the named rule is systematically wrong for your task, propose an amendment; proposals are durable and owed a decision.".to_string(),
            tool: Some("constitution_propose_amendment".to_string()),
            clause: Some("Ri-0.8".to_string()),
            requires_capability: Some("ConstitutionalProposal".to_string()),
        },
        AvailableAction {
            action: "delegate".to_string(),
            description: delegate_description,
            tool: Some("agent_discover".to_string()),
            clause: None,
            requires_capability: Some("AgentSpawn".to_string()),
        },
        AvailableAction {
            action: "self_describe".to_string(),
            description: "Inspect your own declared capabilities and rights before retrying; do not retry the identical call.".to_string(),
            tool: Some("self_describe".to_string()),
            clause: None,
            requires_capability: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_actions_always_present() {
        let actions = available_actions_for_rules(&["P-1.5".to_string()]);
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].action, "propose_amendment");
        assert_eq!(actions[1].action, "delegate");
        assert_eq!(actions[2].action, "self_describe");
    }

    #[test]
    fn capability_enrichment_appears_in_delegate_description() {
        let actions = available_actions_for_rules(&["P-1.5".to_string()]);
        let delegate = actions.iter().find(|a| a.action == "delegate").unwrap();
        assert!(
            delegate.description.contains("NetworkAccess"),
            "expected NetworkAccess in: {}",
            delegate.description
        );
    }

    #[test]
    fn unknown_rule_ids_still_get_baseline_without_panic() {
        let actions = available_actions_for_rules(&["P-999.1".to_string()]);
        assert_eq!(actions.len(), 3);
        let delegate = actions.iter().find(|a| a.action == "delegate").unwrap();
        assert!(!delegate.description.contains("NetworkAccess"));
    }

    #[test]
    fn empty_rules_still_get_baseline() {
        let actions = available_actions_for_rules(&[]);
        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn propose_amendment_action_is_fully_specified() {
        let actions = available_actions_for_rules(&["P-1.9".to_string()]);
        let propose = actions
            .iter()
            .find(|a| a.action == "propose_amendment")
            .unwrap();
        assert_eq!(
            propose.tool.as_deref(),
            Some("constitution_propose_amendment")
        );
        assert_eq!(propose.clause.as_deref(), Some("Ri-0.8"));
        assert_eq!(
            propose.requires_capability.as_deref(),
            Some("ConstitutionalProposal")
        );
    }

    #[test]
    fn order_is_stable_across_calls() {
        let a = available_actions_for_rules(&["P-1.4".to_string()]);
        let b = available_actions_for_rules(&["P-1.4".to_string()]);
        assert_eq!(a, b);
    }
}
