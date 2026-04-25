//! Disclosure policy state and filtering logic.

use autonoetic_types::disclosure::{DisclosureClass, DisclosurePolicy};

/// Tracks tainted strings seen during the execution of a session.
#[derive(Debug, Default)]
pub struct DisclosureState {
    policy: DisclosurePolicy,
    taints: Vec<Taint>,
}

#[derive(Debug)]
struct Taint {
    content: String,
}

impl DisclosureState {
    pub fn new(policy: DisclosurePolicy) -> Self {
        Self {
            policy,
            taints: Vec::new(),
        }
    }

    /// Determines the disclosure class for a given source and optional path.
    pub fn evaluate_class(&self, source: &str, path: Option<&str>) -> DisclosureClass {
        for rule in &self.policy.rules {
            if rule.source == source {
                match (&rule.path_pattern, path) {
                    (Some(pattern), Some(actual_path)) => {
                        // Very simple glob style matching (e.g. "state/secrets/*" or exact)
                        if pattern.ends_with('*') {
                            let prefix = pattern.trim_end_matches('*');
                            if actual_path.starts_with(prefix) {
                                return rule.class;
                            }
                        } else if pattern == actual_path {
                            return rule.class;
                        }
                    }
                    (None, _) => {
                        // Rule applies to all calls to this source
                        return rule.class;
                    }
                    _ => {}
                }
            }
        }
        self.policy.default_class
    }

    /// Registers a tool result as tainted if its class is not Public.
    pub fn register_result(&mut self, source: &str, path: Option<&str>, result: &str) {
        let class = self.evaluate_class(source, path);
        let trimmed = result.trim();
        if !class.is_restricted() || trimmed.is_empty() {
            return;
        }

        self.taints.push(Taint {
            content: result.to_string(),
        });
    }

    /// Extends the state with a forcefully defined taint (e.g., from SecretStore)
    pub fn register_explicit_taint(&mut self, result: &str, class: DisclosureClass) {
        if class.is_restricted() && !result.trim().is_empty() {
            self.taints.push(Taint {
                content: result.to_string(),
            });
        }
    }

    /// Filters the given assistant reply, replacing verbatim matches of restricted content.
    ///
    /// This is intentionally exact-substring filtering (not fuzzy/semantic matching)
    /// so behavior stays deterministic, auditable, and low-overhead.
    pub fn filter_reply(&self, reply: &str) -> String {
        if self.taints.is_empty() {
            return reply.to_string();
        }

        let mut filtered = reply.to_string();

        // Sort taints by length descending to replace the longest matches first
        let mut sorted_taints = self.taints.iter().collect::<Vec<_>>();
        sorted_taints.sort_by(|a, b| b.content.len().cmp(&a.content.len()));

        for taint in sorted_taints {
            if filtered.contains(&taint.content) {
                filtered = filtered.replace(&taint.content, "[REDACTED]");
            }
        }

        filtered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::disclosure::DisclosureRule;

    #[test]
    fn test_disclosure_state_evaluation() {
        let policy = DisclosurePolicy {
            rules: vec![
                DisclosureRule {
                    source: "memory_read".to_string(),
                    path_pattern: Some("state/secrets/*".to_string()),
                    class: DisclosureClass::Restricted,
                },
                DisclosureRule {
                    source: "memory_read".to_string(),
                    path_pattern: None,
                    class: DisclosureClass::Public,
                },
            ],
            default_class: DisclosureClass::Public,
        };

        let state = DisclosureState::new(policy);

        assert_eq!(
            state.evaluate_class("memory_read", Some("state/secrets/keys.json")),
            DisclosureClass::Restricted
        );
        assert_eq!(
            state.evaluate_class("memory_read", Some("state/public/data.json")),
            DisclosureClass::Public
        );
        assert_eq!(
            state.evaluate_class("sandbox_exec", None),
            DisclosureClass::Public
        );
    }

    #[test]
    fn test_disclosure_filter_redaction() {
        let policy = DisclosurePolicy::default();
        let mut state = DisclosureState::new(policy);

        state.register_explicit_taint("super_secret_password_123", DisclosureClass::Restricted);
        state.register_explicit_taint("internal_api_key_456", DisclosureClass::Restricted);

        let input = "Here are the credentials. Password is super_secret_password_123 and the API key is internal_api_key_456.";
        let filtered = state.filter_reply(input);

        assert!(filtered.contains("[REDACTED]"));
        assert!(!filtered.contains("super_secret_password_123"));
        assert!(!filtered.contains("internal_api_key_456"));
    }

    #[test]
    fn test_register_result_taints_restricted_values() {
        let policy = DisclosurePolicy {
            rules: vec![DisclosureRule {
                source: "memory_read".to_string(),
                path_pattern: Some("state/secrets/*".to_string()),
                class: DisclosureClass::Restricted,
            }],
            default_class: DisclosureClass::Public,
        };
        let mut state = DisclosureState::new(policy);

        state.register_result("memory_read", Some("state/secrets/pin.txt"), "1234");
        let filtered = state.filter_reply("The PIN is 1234.");

        assert!(filtered.contains("[REDACTED]"));
        assert!(!filtered.contains("1234"));
    }

    #[test]
    fn test_filter_reply_does_not_redact_transformed_secret_variant() {
        let policy = DisclosurePolicy::default();
        let mut state = DisclosureState::new(policy);
        state.register_explicit_taint("super_secret_wahoo", DisclosureClass::Restricted);

        let filtered = state.filter_reply("I can only say: super secret wahoo");

        assert_eq!(filtered, "I can only say: super secret wahoo");
        assert!(!filtered.contains("[REDACTED]"));
    }
}
