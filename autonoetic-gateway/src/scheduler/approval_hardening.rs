use autonoetic_types::background::{ApprovalRequest, ScheduledAction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalRisk {
    Standard,
    High,
    Critical,
}

pub struct ApprovalHardening {
    pub min_dwell_ms: i64,
    pub confirm_phrase: Option<String>,
}

const DWELL_STANDARD_MS: i64 = 0;
const DWELL_HIGH_MS: i64 = 3_000;
const DWELL_CRITICAL_MS: i64 = 5_000;

pub fn classify_approval_risk(action: &ScheduledAction) -> ApprovalRisk {
    match action {
        ScheduledAction::RevisionPromote { .. } => ApprovalRisk::Critical,
        ScheduledAction::CredentialPrompt { .. } => ApprovalRisk::Critical,
        ScheduledAction::AgentInstall { .. } => ApprovalRisk::High,
        ScheduledAction::SessionEscalate { .. } => ApprovalRisk::High,
        ScheduledAction::SandboxExec { detected_hosts, .. } => {
            let hosts = detected_hosts.as_ref().map(|h| h.len()).unwrap_or(0);
            if hosts == 0 {
                ApprovalRisk::Standard
            } else {
                ApprovalRisk::High
            }
        }
        ScheduledAction::CredentialRequest { .. } => ApprovalRisk::High,
        ScheduledAction::WebFetch { .. } => ApprovalRisk::High,
        ScheduledAction::WebCall { .. } => ApprovalRisk::High,
        ScheduledAction::WebSearch { .. } => ApprovalRisk::High,
        ScheduledAction::LayerMount { .. } => ApprovalRisk::High,
        ScheduledAction::SessionContinue { .. } => ApprovalRisk::Standard,
        ScheduledAction::ProfileShare { .. } => ApprovalRisk::Standard,
        ScheduledAction::WriteFile { .. } => ApprovalRisk::Standard,
    }
}

pub fn hardening_for_action(action: &ScheduledAction) -> ApprovalHardening {
    let risk = classify_approval_risk(action);
    let min_dwell_ms = match risk {
        ApprovalRisk::Standard => DWELL_STANDARD_MS,
        ApprovalRisk::High => DWELL_HIGH_MS,
        ApprovalRisk::Critical => DWELL_CRITICAL_MS,
    };
    let confirm_phrase = match risk {
        ApprovalRisk::Critical => Some(confirm_phrase_for(action)),
        _ => None,
    };
    ApprovalHardening {
        min_dwell_ms,
        confirm_phrase,
    }
}

pub fn enrich_request(request: &mut ApprovalRequest) {
    let h = hardening_for_action(&request.action);
    request.min_dwell_ms = if h.min_dwell_ms > 0 {
        Some(h.min_dwell_ms)
    } else {
        None
    };
    request.confirm_phrase = h.confirm_phrase;
}

fn confirm_phrase_for(action: &ScheduledAction) -> String {
    match action {
        ScheduledAction::RevisionPromote {
            agent_id,
            revision_id,
            ..
        } => {
            let short_rev = if revision_id.len() > 16 {
                &revision_id[..16]
            } else {
                revision_id
            };
            format!("promote {} {}", agent_id, short_rev)
        }
        ScheduledAction::CredentialPrompt {
            service,
            credential_id,
            ..
        } => format!("register {} {}", service, credential_id),
        _ => "confirm".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_revision_promote_critical() {
        let action = ScheduledAction::RevisionPromote {
            agent_id: "test.agent".to_string(),
            revision_id: "rev_sha256:abc123".to_string(),
            outgoing_revision_id: "rev_sha256:old".to_string(),
            added_capabilities: vec!["NetworkAccess".to_string()],
            broadened_capabilities: vec![],
            payload: None,
        };
        assert_eq!(classify_approval_risk(&action), ApprovalRisk::Critical);
        let h = hardening_for_action(&action);
        assert_eq!(h.min_dwell_ms, DWELL_CRITICAL_MS);
        assert!(h.confirm_phrase.is_some());
        assert!(h.confirm_phrase.unwrap().contains("promote"));
    }

    #[test]
    fn classify_sandbox_exec_with_hosts_high() {
        let action = ScheduledAction::SandboxExec {
            command: "curl https://example.com".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(vec!["example.com".to_string()]),
        };
        assert_eq!(classify_approval_risk(&action), ApprovalRisk::High);
    }

    #[test]
    fn classify_sandbox_exec_no_hosts_standard() {
        let action = ScheduledAction::SandboxExec {
            command: "ls".to_string(),
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: None,
        };
        assert_eq!(classify_approval_risk(&action), ApprovalRisk::Standard);
    }

    #[test]
    fn dwell_ms_progresses() {
        assert!(DWELL_STANDARD_MS < DWELL_HIGH_MS);
        assert!(DWELL_HIGH_MS < DWELL_CRITICAL_MS);
    }

    #[test]
    fn enrich_sets_fields_on_critical() {
        let mut req = ApprovalRequest {
            request_id: "apr-test".to_string(),
            agent_id: "test".to_string(),
            session_id: "sess".to_string(),
            action: ScheduledAction::CredentialPrompt {
                service: "aws".to_string(),
                credential_id: "cred_123".to_string(),
                message: "enter keys".to_string(),
                secret_fields: vec![],
                payload: None,
            },
            created_at: chrono::Utc::now().to_rfc3339(),
            reason: None,
            evidence_ref: None,
            root_session_id: None,
            workflow_id: None,
            task_id: None,
            status: None,
            decided_at: None,
            decided_by: None,
            decision_reason: None,
            approval_level: Default::default(),
            similar_to_request_id: None,
            similarity_score: None,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,
        };
        enrich_request(&mut req);
        assert!(req.min_dwell_ms.unwrap() > 0);
        assert!(req.confirm_phrase.is_some());
    }
}
