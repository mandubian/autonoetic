/// Static one-line glossary of constitutional rules for human-readable surfaces.
///
/// Used by the TUI policy pane, approval cards, and `/why` command to translate
/// opaque rule IDs into actionable explanations. The glossary is intentionally
/// compiled into the binary so it works without filesystem access.
pub fn rule_explanation(rule_id: &str) -> Option<&'static str> {
    Some(match rule_id {
        // Rights
        "Ri-0.1" => "Agent right: pending gates must not block unrelated operations",
        "Ri-0.5" => "Agent right: degradation notice before restricted execution",

        // Approval gates (R-2.x)
        "R-2.1" => "Remote access requires operator approval before execution",
        "R-2.2" => "Approval payload must be persisted before suspension",
        "R-2.3" => "Duplicate gate requests are deduplicated (same session + targets)",
        "R-2.4" => "Session grants bypass repeated approval for the same host",
        "R-2.6" => "Pre-validated or approval_ref-cleared operation",
        "R-2.10" => "All gate-suspended turns resume through a unified checkpoint path",
        "R-2.12" => "Approval resolution is decider-agnostic (human or agent)",
        "R-2.13" => "User interaction (user_ask) routed through unified GateService",
        "R-2.14" => "user_ask refused when a gate is already pending for the session",
        "R-2.18" => "All suspension points use the unified GateService",
        "R-2.19" => "Gate enrichment messages are auditable with sender attribution",
        "R-2.20" => "Agent-as-decider: agents may resolve gates for other agents",
        "R-2.21" => "Agent-decider must escalate to human when confidence is low",

        // Execution safety (R-7.x)
        "R-7.15" => "Spawn-chain depth limit exceeded",
        "R-7.17" => "Approval flood cap — too many pending approvals per root session",
        "R-7.18" => "Sandbox escape attempt degradation threshold reached",

        // Audit and attribution (R-8.x, R+++3)
        "R-8.19" => "Gate decisions carry decider attribution",
        "R+++3" => "Every enforcement action records the rule ID in the causal chain",

        // State attestation (R-6.x)
        "R-6.23" => "State attestation includes all pending gates (approvals, interactions, escalations)",

        // Promotion and capability (R++2, R++4)
        "R++2" => "Revision promotion requires operator approval for capability delta",
        "R++4" => "Approval dwell time enforced before high-risk decisions",

        // Self-approval ban (R-10.x)
        "R-10.7" => "Self-approval ban: agents cannot approve their own spawn-tree ancestors",

        _ => return None,
    })
}

/// Format a list of enforced rule IDs as a human-readable "Governed by:" line.
pub fn format_enforced_rules(rules: &[impl AsRef<str>]) -> String {
    if rules.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = rules
        .iter()
        .map(|r| {
            let id = r.as_ref();
            match rule_explanation(id) {
                Some(desc) => format!("{} ({})", id, desc),
                None => id.to_string(),
            }
        })
        .collect();
    format!("Governed by: {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_rules_have_explanations() {
        assert!(rule_explanation("R-2.1").is_some());
        assert!(rule_explanation("R-2.18").is_some());
        assert!(rule_explanation("R+++3").is_some());
    }

    #[test]
    fn unknown_rule_returns_none() {
        assert!(rule_explanation("R-999.999").is_none());
    }

    #[test]
    fn format_enforced_rules_empty() {
        let empty: Vec<&str> = vec![];
        assert_eq!(format_enforced_rules(&empty), "");
    }

    #[test]
    fn format_enforced_rules_with_known() {
        let rules = vec!["R-2.1", "R-2.18"];
        let result = format_enforced_rules(&rules);
        assert!(result.starts_with("Governed by: R-2.1"));
        assert!(result.contains("R-2.18"));
    }
}
