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

        // Approval gates (P-2.x)
        "P-2.1" => "Remote access requires operator approval before execution",
        "P-2.2" => "Approval payload must be persisted before suspension",
        "P-2.3" => "Duplicate gate requests are deduplicated (same session + targets)",
        "P-2.4" => "Session grants bypass repeated approval for the same host",
        "P-2.6" => "Pre-validated or approval_ref-cleared operation",
        "P-2.10" => "All gate-suspended turns resume through a unified checkpoint path",
        "P-2.12" => "Approval resolution is decider-agnostic (human or agent)",
        "P-2.13" => "User interaction (user_ask) routed through unified GateService",
        "P-2.14" => "user_ask refused when a gate is already pending for the session",
        "P-2.18" => "All suspension points use the unified GateService",
        "P-2.19" => "Gate enrichment messages are auditable with sender attribution",
        "P-2.20" => "Agent-as-decider: agents may resolve gates for other agents",
        "P-2.21" => "Agent-decider must escalate to human when confidence is low",
        "P-2.22" => "Operator approval required when any federation-role verdict is present (FullJury gate)",
        "P-2.23" => "Session approval grants expire after a configured TTL",
        "P-2.24" => "Operator approval hardening (dwell time, typed confirmation, dedup)",

        // Execution safety (P-7.x)
        "P-7.15" => "Spawn-chain depth limit exceeded",
        "P-7.17" => "Approval flood cap — too many pending approvals per root session",
        "P-7.18" => "Degraded session mode — non-Core tools, network, and spawn revoked; reasoning retained",

        // Audit and attribution (P-8.x, I-6)
        "P-8.19" => "Gate decisions carry decider attribution",
        "I-6" => "Every enforcement action records the rule ID in the causal chain",

        // State attestation (P-6.x)
        "P-6.23" => "State attestation includes all pending gates (approvals, interactions, escalations)",

        // Promotion and capability (P-2.16)
        "P-2.16" => "Revision promotion requires operator approval for capability delta",

        // Self-approval ban (P-10.x)
        "P-10.7" => "Self-approval ban: agents cannot approve their own spawn-tree ancestors",

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
        assert!(rule_explanation("P-2.1").is_some());
        assert!(rule_explanation("P-2.18").is_some());
        assert!(rule_explanation("I-6").is_some());
    }

    #[test]
    fn unknown_rule_returns_none() {
        assert!(rule_explanation("P-999.999").is_none());
    }

    #[test]
    fn format_enforced_rules_empty() {
        let empty: Vec<&str> = vec![];
        assert_eq!(format_enforced_rules(&empty), "");
    }

    #[test]
    fn format_enforced_rules_with_known() {
        let rules = vec!["P-2.1", "P-2.18"];
        let result = format_enforced_rules(&rules);
        assert!(result.starts_with("Governed by: P-2.1"));
        assert!(result.contains("P-2.18"));
    }
}
