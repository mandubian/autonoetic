use std::collections::HashSet;
use std::hash::Hash;

use autonoetic_types::background::{ApprovalRequest, ScheduledAction};

pub struct SimilarityResult {
    pub request_id: String,
    pub score: f64,
}

pub fn compute_action_similarity(a: &ScheduledAction, b: &ScheduledAction) -> f64 {
    match (a, b) {
        (
            ScheduledAction::SandboxExec {
                command: cmd_a,
                detected_hosts: hosts_a,
                ..
            },
            ScheduledAction::SandboxExec {
                command: cmd_b,
                detected_hosts: hosts_b,
                ..
            },
        ) => {
            let tokens_a = shell_words(cmd_a);
            let tokens_b = shell_words(cmd_b);
            let set_a: HashSet<&str> = tokens_a.iter().copied().collect();
            let set_b: HashSet<&str> = tokens_b.iter().copied().collect();
            let cmd_sim = jaccard(&set_a, &set_b);

            let host_set_a: HashSet<&str> = hosts_a
                .as_ref()
                .map(|h| h.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default();
            let host_set_b: HashSet<&str> = hosts_b
                .as_ref()
                .map(|h| h.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default();
            let host_sim = if host_set_a.is_empty() && host_set_b.is_empty() {
                1.0
            } else {
                jaccard(&host_set_a, &host_set_b)
            };

            0.7 * cmd_sim + 0.3 * host_sim
        }
        _ => {
            if a.kind() == b.kind() { 0.5 } else { 0.0 }
        }
    }
}

pub fn find_similar_approvals(
    new_request: &ApprovalRequest,
    candidates: &[ApprovalRequest],
    limit: usize,
    threshold: f64,
) -> Vec<SimilarityResult> {
    let mut scored: Vec<SimilarityResult> = candidates
        .iter()
        .filter(|c| c.request_id != new_request.request_id)
        .filter_map(|c| {
            let score = compute_action_similarity(&new_request.action, &c.action);
            if score >= threshold {
                Some(SimilarityResult {
                    request_id: c.request_id.clone(),
                    score,
                })
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

fn shell_words(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

fn jaccard<T: Hash + Eq>(a: &HashSet<T>, b: &HashSet<T>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { intersection / union }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_commands_high_similarity() {
        let a = ScheduledAction::SandboxExec {
            command: "curl https://api.github.com/users".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["api.github.com".to_string()]),
        };
        let b = ScheduledAction::SandboxExec {
            command: "curl https://api.github.com/users".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["api.github.com".to_string()]),
        };
        assert!(compute_action_similarity(&a, &b) > 0.99);
    }

    #[test]
    fn test_different_commands_low_similarity() {
        let a = ScheduledAction::SandboxExec {
            command: "curl https://api.github.com/users".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["api.github.com".to_string()]),
        };
        let b = ScheduledAction::SandboxExec {
            command: "python3 -m pytest tests/".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        };
        assert!(compute_action_similarity(&a, &b) < 0.3);
    }

    #[test]
    fn test_whitespace_insensitive() {
        let a = ScheduledAction::SandboxExec {
            command: "echo   hello   world".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        };
        let b = ScheduledAction::SandboxExec {
            command: "echo hello world".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        };
        assert!(compute_action_similarity(&a, &b) > 0.99);
    }
}
