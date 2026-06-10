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
        (
            ScheduledAction::WikiProposal {
                content: content_a,
                title: title_a,
                tags: tags_a,
                ..
            },
            ScheduledAction::WikiProposal {
                content: content_b,
                title: title_b,
                tags: tags_b,
                ..
            },
        ) => {
            let tokens_a: HashSet<&str> = content_a.split_whitespace().collect();
            let tokens_b: HashSet<&str> = content_b.split_whitespace().collect();
            let content_sim = jaccard(&tokens_a, &tokens_b);

            let title_a_set: HashSet<&str> = title_a.split_whitespace().collect();
            let title_b_set: HashSet<&str> = title_b.split_whitespace().collect();
            let title_sim = jaccard(&title_a_set, &title_b_set);

            let tags_a_set: HashSet<&str> = tags_a.iter().map(|s| s.as_str()).collect();
            let tags_b_set: HashSet<&str> = tags_b.iter().map(|s| s.as_str()).collect();
            let tags_sim = if tags_a_set.is_empty() && tags_b_set.is_empty() {
                1.0
            } else {
                jaccard(&tags_a_set, &tags_b_set)
            };

            0.5 * content_sim + 0.3 * title_sim + 0.2 * tags_sim
        }
        _ => {
            if a.kind() == b.kind() {
                0.5
            } else {
                0.0
            }
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
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
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

    #[test]
    fn test_identical_wiki_proposals_high_similarity() {
        let a = ScheduledAction::WikiProposal {
            page_id: "page-1".into(),
            title: "Getting Started".into(),
            content: "# Getting Started\n\nInstall the SDK with pip.".into(),
            tags: vec!["sdk".into(), "install".into()],
            content_sha256: None,
            proposed_by_agent: "agent-1".into(),
            proposed_by_session: Some("session-1".into()),
        };
        let b = ScheduledAction::WikiProposal {
            page_id: "page-2".into(),
            title: "Getting Started".into(),
            content: "# Getting Started\n\nInstall the SDK with pip.".into(),
            tags: vec!["sdk".into(), "install".into()],
            content_sha256: None,
            proposed_by_agent: "agent-2".into(),
            proposed_by_session: Some("session-2".into()),
        };
        assert!(compute_action_similarity(&a, &b) > 0.95);
    }

    #[test]
    fn test_different_wiki_proposals_low_similarity() {
        let a = ScheduledAction::WikiProposal {
            page_id: "page-1".into(),
            title: "Getting Started".into(),
            content: "Install the SDK with pip install autonoetic-sdk".into(),
            tags: vec!["sdk".into()],
            content_sha256: None,
            proposed_by_agent: "agent-1".into(),
            proposed_by_session: Some("session-1".into()),
        };
        let b = ScheduledAction::WikiProposal {
            page_id: "page-2".into(),
            title: "Architecture Overview".into(),
            content: "The system uses a gateway agent pattern with SQLite storage".into(),
            tags: vec!["architecture".into()],
            content_sha256: None,
            proposed_by_agent: "agent-2".into(),
            proposed_by_session: Some("session-2".into()),
        };
        assert!(compute_action_similarity(&a, &b) < 0.5);
    }
}
