use autonoetic_types::background::{ApprovalRequest, ScheduledAction};

// `ApprovalRisk` moved to `autonoetic_types::background` in #1195 so a decider
// appointment's `risk_ceiling` can reuse this vocabulary instead of inventing a
// parallel notion of gate altitude. Re-exported so existing callers are
// unaffected.
pub use autonoetic_types::background::ApprovalRisk;

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
        ScheduledAction::WikiProposal { .. } => ApprovalRisk::Standard,
        ScheduledAction::PlanFrame { .. } => ApprovalRisk::Standard,
        ScheduledAction::EgressDeclassify { .. } => ApprovalRisk::High,
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
        // RFC credential-egress-host-authorization: approving a
        // credential_request approves *secret delivery to a host* (the
        // gateway injects the secret), so the operator retypes a
        // host-naming phrase — the same P-2.24 protection the registration
        // class gets, even though the risk class is High.
        ApprovalRisk::High if matches!(action, ScheduledAction::CredentialRequest { .. }) => {
            Some(confirm_phrase_for(action))
        }
        _ => None,
    };
    ApprovalHardening {
        min_dwell_ms,
        confirm_phrase,
    }
}

pub fn enrich_request(
    request: &mut ApprovalRequest,
    config: Option<&autonoetic_types::config::GatewayConfig>,
) {
    let h = hardening_for_action(&request.action);
    request.min_dwell_ms = if h.min_dwell_ms > 0 {
        Some(h.min_dwell_ms)
    } else {
        None
    };
    request.confirm_phrase = h.confirm_phrase;

    // Standalone (non-workflow) approvals get a configurable TTL so they do not
    // sit pending forever in chat-spawned sessions. Workflow-bound approvals rely
    // on the task-level approval_timeout_secs instead.
    if request.workflow_id.is_none() && request.task_id.is_none() && request.expires_at.is_none() {
        if let Some(cfg) = config {
            let ttl = cfg.standalone_approval_timeout_secs;
            if ttl > 0 {
                // Base the TTL on the request's own creation time so the stored
                // expiry is consistent with `created_at` (callers, including
                // tests, may set it explicitly). Fall back to now if unset.
                let base = chrono::DateTime::parse_from_rfc3339(&request.created_at)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                request.expires_at =
                    Some((base + chrono::Duration::seconds(ttl as i64)).to_rfc3339());
            }
        }
    }
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
        // Host-naming phrase (RFC credential-egress-host-authorization):
        // the host is the security-relevant part — the credential id is
        // unwieldy to retype and the service is already on the card.
        // Mint sites always stash the host in `payload.host`; the URL is
        // the fallback for hand-built actions.
        ScheduledAction::CredentialRequest { url, payload, .. } => {
            let host = payload
                .as_ref()
                .and_then(|p| p.get("host"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    url::Url::parse(&url)
                        .ok()
                        .and_then(|u| u.host_str().map(String::from))
                        // Never fall back to the raw URL: path/query could
                        // leak into an operator retype prompt (review).
                        .unwrap_or_else(|| "unparsed-host".to_string())
                });
            format!("use credential at {}", host)
        }
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
            federation_context: None,
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
            intent: None,
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
            intent: None,
        };
        assert_eq!(classify_approval_risk(&action), ApprovalRisk::Standard);
    }

    #[test]
    fn dwell_ms_progresses() {
        assert!(DWELL_STANDARD_MS < DWELL_HIGH_MS);
        assert!(DWELL_HIGH_MS < DWELL_CRITICAL_MS);
    }

    #[test]
    fn credential_request_high_risk_carries_host_phrase() {
        // RFC credential-egress-host-authorization: the vault injects the
        // secret, so approving a credential_request approves secret delivery
        // to a host — High risk, but with the P-2.24 host-naming phrase.
        let action = ScheduledAction::CredentialRequest {
            credential_id: "cred_github_001".to_string(),
            url: "https://api.github.com/repos/rust-lang/rust".to_string(),
            method: None,
            headers: None,
            body: None,
            inject_secret_as: None,
            payload: Some(serde_json::json!({ "host": "api.github.com" })),
        };
        assert_eq!(classify_approval_risk(&action), ApprovalRisk::High);
        let h = hardening_for_action(&action);
        assert_eq!(h.min_dwell_ms, DWELL_HIGH_MS);
        assert_eq!(h.confirm_phrase.as_deref(), Some("use credential at api.github.com"));
    }

    #[test]
    fn credential_request_phrase_falls_back_to_url_host() {
        // Hand-built actions without payload.host still name the host.
        let action = ScheduledAction::CredentialRequest {
            credential_id: "cred_x".to_string(),
            url: "https://example.org/path".to_string(),
            method: None,
            headers: None,
            body: None,
            inject_secret_as: None,
            payload: None,
        };
        assert_eq!(
            confirm_phrase_for(&action),
            "use credential at example.org"
        );
    }

    #[test]
    fn credential_request_phrase_never_leaks_raw_url() {
        // An unparseable "url" must not surface in an operator retype
        // prompt — a non-leaking placeholder is used instead (review).
        let action = ScheduledAction::CredentialRequest {
            credential_id: "cred_x".to_string(),
            url: "not a url/with?query=secret".to_string(),
            method: None,
            headers: None,
            body: None,
            inject_secret_as: None,
            payload: None,
        };
        let phrase = confirm_phrase_for(&action);
        assert_eq!(phrase, "use credential at unparsed-host");
        assert!(!phrase.contains("query"));
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
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };
        enrich_request(&mut req, None);
        assert!(req.min_dwell_ms.unwrap() > 0);
        assert!(req.confirm_phrase.is_some());
    }
}
