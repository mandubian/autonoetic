//! Signed turn-boundary state attestation — R++1 (issue #48).
//!
//! At every turn boundary the gateway composes a small JSON block
//! describing the agent's *current* operational state — remaining budget,
//! active capabilities, pending approvals, spawn depth, session ids, turn
//! counter — signs it with the per-gateway Ed25519 identity key, and
//! injects the wrapper as a system-prompt tail. The agent's foundation
//! prompt teaches it that this block is authoritative; its own memory of
//! these facts is not.
//!
//! Threat addressed: LLM reasoning state diverges from gateway ground
//! truth. The model's sense of what's true comes from its conversation
//! history, which it also shapes; an agent can plan coherently on false
//! premises for many turns before contradiction. The attestation is a
//! signed fact-of-the-moment the model can re-read without trusting the
//! transcript.
//!
//! Verification: see `crypto::verify_attestation_signature`. The wrapper
//! includes a short `key_fingerprint` (first 8 bytes of the public key,
//! hex) so a verifier can pin the gateway's identity without serialising
//! the whole 32-byte key inline every turn.

use crate::runtime::crypto::{verify_attestation_signature, GatewayIdentityKey};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use serde::{Deserialize, Serialize};

/// Inputs to compose a fresh attestation. Pulled from the executor at turn
/// start so the block reflects post-tool-result state.
pub struct AttestationInputs<'a> {
    pub agent_id: &'a str,
    pub session_id: Option<&'a str>,
    pub root_session_id: Option<&'a str>,
    pub turn_counter: u64,
    pub manifest: &'a AgentManifest,
    pub gateway_node_id: &'a str,
    /// Pending-approval request IDs visible from this session's scope.
    /// Bounded by the caller (we cap to 32 in the wrapper to keep the
    /// system-prompt tail small).
    pub pending_approval_ids: Vec<String>,
    /// Pending user-interaction IDs (GateKind::UserInput).
    pub pending_user_interaction_ids: Vec<String>,
    /// Pending escalation IDs (GateKind::Escalation).
    pub pending_escalation_ids: Vec<String>,
    /// Named budget meters from the session registry. `limit` is `None`
    /// when no cap is configured for that meter (e.g. price tracking
    /// without a ceiling). Empty when budgets are disabled or the
    /// session has not yet recorded any usage.
    pub budget_meters: Vec<BudgetMeter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetMeter {
    pub name: String,
    pub used: f64,
    pub limit: Option<f64>,
}

impl BudgetMeter {
    pub fn remaining(&self) -> Option<f64> {
        self.limit.map(|lim| (lim - self.used).max(0.0))
    }
}

/// The signed payload itself (the JSON object that gets hashed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateAttestationPayload {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_session_id: Option<String>,
    pub turn_counter: u64,
    /// Capability *type names* (e.g. `["NetworkAccess", "WriteAccess"]`)
    /// — names only, not the full structured form, to keep the block small.
    pub active_capabilities: Vec<String>,
    pub pending_approval_count: usize,
    pub pending_approval_ids: Vec<String>,
    pub pending_user_interaction_count: usize,
    pub pending_user_interaction_ids: Vec<String>,
    pub pending_escalation_count: usize,
    pub pending_escalation_ids: Vec<String>,
    pub spawn_depth: u32,
    pub budget: Vec<BudgetMeter>,
    pub gateway_node_id: String,
    pub attested_at: String,
}

/// Wrapper that bundles a payload with its signature and key fingerprint.
/// This is what gets serialised into the system-prompt tail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateAttestation {
    pub payload: StateAttestationPayload,
    /// Base64 Ed25519 signature over the canonical JSON of `payload`.
    pub signature: String,
    /// First 8 bytes of the gateway public key, hex-encoded.
    pub key_fingerprint: String,
}

/// Maximum number of pending-approval IDs surfaced inline. Beyond this we
/// truncate and emit a count — the goal is to keep the per-turn tail
/// bounded.
pub const MAX_PENDING_APPROVALS_INLINE: usize = 32;

/// Compose, sign, and return the wrapper.
pub fn compose_and_sign(
    inputs: AttestationInputs<'_>,
    key: &GatewayIdentityKey,
) -> anyhow::Result<StateAttestation> {
    let pending_total = inputs.pending_approval_ids.len();
    let pending_inline: Vec<String> = inputs
        .pending_approval_ids
        .into_iter()
        .take(MAX_PENDING_APPROVALS_INLINE)
        .collect();

    let pending_ui_total = inputs.pending_user_interaction_ids.len();
    let pending_ui_inline: Vec<String> = inputs
        .pending_user_interaction_ids
        .into_iter()
        .take(MAX_PENDING_APPROVALS_INLINE)
        .collect();

    let pending_esc_total = inputs.pending_escalation_ids.len();
    let pending_esc_inline: Vec<String> = inputs
        .pending_escalation_ids
        .into_iter()
        .take(MAX_PENDING_APPROVALS_INLINE)
        .collect();

    let active_capabilities = inputs
        .manifest
        .capabilities
        .iter()
        .map(capability_type_name)
        .collect::<Vec<_>>();

    let spawn_depth = inputs
        .session_id
        .map(|s| s.matches('/').count() as u32)
        .unwrap_or(0);

    let payload = StateAttestationPayload {
        agent_id: inputs.agent_id.to_string(),
        session_id: inputs.session_id.map(str::to_string),
        root_session_id: inputs.root_session_id.map(str::to_string),
        turn_counter: inputs.turn_counter,
        active_capabilities,
        pending_approval_count: pending_total,
        pending_approval_ids: pending_inline,
        pending_user_interaction_count: pending_ui_total,
        pending_user_interaction_ids: pending_ui_inline,
        pending_escalation_count: pending_esc_total,
        pending_escalation_ids: pending_esc_inline,
        spawn_depth,
        budget: inputs.budget_meters,
        gateway_node_id: inputs.gateway_node_id.to_string(),
        attested_at: chrono::Utc::now().to_rfc3339(),
    };

    let canonical = canonical_payload_bytes(&payload)?;
    let signature = key.sign(&canonical);

    Ok(StateAttestation {
        payload,
        signature,
        key_fingerprint: key.fingerprint(),
    })
}

/// Render the wrapper as a system-prompt tail. Keep the format machine-
/// readable: agents (and human reviewers) can parse the JSON body between
/// the markers. Markers also bracket the block so accidental
/// concatenation with surrounding text can't be confused for inclusion.
pub fn render_tail(att: &StateAttestation) -> anyhow::Result<String> {
    let body = serde_json::to_string_pretty(att).map_err(|e| {
        anyhow::anyhow!(
            "Cannot serialise state attestation for prompt injection: {}",
            e
        )
    })?;
    Ok(format!(
        "---\n\nGateway State Attestation (R++1)\n\n\
         The block below is signed by the gateway's identity key. It is the \
         **authoritative** statement of your remaining budget, active \
         capabilities, pending gates (approvals, user interactions, \
         escalations), spawn depth, session ids, and turn counter. If your \
         own memory of these facts disagrees with the block, the block is \
         correct.\n\n\
         <gateway_state_attestation>\n{body}\n</gateway_state_attestation>\n",
    ))
}

/// Verify a wrapper against a known public key. Returns the parsed payload
/// when the signature checks out. Used by tests and (eventually) by
/// federated verifiers.
pub fn verify(
    public_key_bytes: &[u8; 32],
    attestation: &StateAttestation,
) -> anyhow::Result<StateAttestationPayload> {
    let expected_fp = hex::encode(&public_key_bytes[..8]);
    anyhow::ensure!(
        attestation.key_fingerprint == expected_fp,
        "State attestation key fingerprint mismatch: block claims {}, verifier expected {}",
        attestation.key_fingerprint,
        expected_fp
    );
    let canonical = canonical_payload_bytes(&attestation.payload)?;
    let ok = verify_attestation_signature(public_key_bytes, &canonical, &attestation.signature)?;
    if !ok {
        anyhow::bail!("State attestation signature did not verify");
    }
    Ok(attestation.payload.clone())
}

/// Canonical bytes for signing/verification. Uses `serde_json::to_vec` —
/// `serde_json` emits keys in struct definition order, which is stable
/// across builds for our concrete `StateAttestationPayload`.
fn canonical_payload_bytes(payload: &StateAttestationPayload) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(payload)
        .map_err(|e| anyhow::anyhow!("Cannot canonicalise attestation payload: {}", e))
}

/// Capability type names — duplicate of the small mapping used elsewhere
/// in the gateway (kept local so this module has no upward dependency).
fn capability_type_name(cap: &Capability) -> String {
    match cap {
        Capability::SandboxFunctions { .. } => "SandboxFunctions",
        Capability::ReadAccess { .. } => "ReadAccess",
        Capability::WriteAccess { .. } => "WriteAccess",
        Capability::NetworkAccess { .. } => "NetworkAccess",
        Capability::AgentSpawn { .. } => "AgentSpawn",
        Capability::AgentMessage { .. } => "AgentMessage",
        Capability::BackgroundReevaluation { .. } => "BackgroundReevaluation",
        Capability::CodeExecution { .. } => "CodeExecution",
        Capability::EmergencyStop => "EmergencyStop",
        Capability::AgentRevision { .. } => "AgentRevision",
        Capability::Evaluation { .. } => "Evaluation",
        Capability::ApprovalQueue { .. } => "ApprovalQueue",
        Capability::SchedulerSignal { .. } => "SchedulerSignal",
        Capability::CredentialAccess { .. } => "CredentialAccess",
        Capability::UserProfileAccess { .. } => "UserProfileAccess",
        Capability::SchedulerAccess { .. } => "SchedulerAccess",
        Capability::SkillInstall { .. } => "SkillInstall",
        Capability::ConstitutionalProposal { .. } => "ConstitutionalProposal",
        Capability::ReasoningAudit { .. } => "ReasoningAudit",
        Capability::GithubIssueCreate { .. } => "GithubIssueCreate",
        Capability::BudgetNoPriceAvailableAllow => "budget.no_price_available.allow",
        Capability::SecurityRedTeam => "SecurityRedTeam",
        Capability::CapsuleExport => "CapsuleExport",
        Capability::PlanFrameAccess { .. } => "PlanFrameAccess",
        Capability::WikiContribute => "WikiContribute",
        Capability::PromoteWith { .. } => "PromoteWith",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};

    fn manifest_with_caps(caps: Vec<Capability>) -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "agent-1".to_string(),
                name: "agent-1".to_string(),
                description: "test".to_string(),
            },
            capabilities: caps,
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        }
    }

    #[test]
    fn compose_then_verify_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let manifest = manifest_with_caps(vec![Capability::NetworkAccess {
            hosts: vec!["api.example.com".to_string()],
        }]);
        let att = compose_and_sign(
            AttestationInputs {
                agent_id: "agent-1",
                session_id: Some("root/child"),
                root_session_id: Some("root"),
                turn_counter: 7,
                manifest: &manifest,
                gateway_node_id: "node-a",
                pending_approval_ids: vec!["apr-aaa".to_string(), "apr-bbb".to_string()],
                pending_user_interaction_ids: vec![],
                pending_escalation_ids: vec![],
                budget_meters: vec![BudgetMeter {
                    name: "llm_rounds".to_string(),
                    used: 3.0,
                    limit: Some(20.0),
                }],
            },
            &key,
        )
        .unwrap();

        let pub_bytes = key.public_key_bytes();
        let payload = verify(&pub_bytes, &att).expect("verify");
        assert_eq!(payload.agent_id, "agent-1");
        assert_eq!(payload.turn_counter, 7);
        assert_eq!(payload.spawn_depth, 1);
        assert_eq!(payload.active_capabilities, vec!["NetworkAccess"]);
        assert_eq!(payload.pending_approval_count, 2);
        assert_eq!(payload.budget[0].remaining(), Some(17.0));
        assert_eq!(att.key_fingerprint, key.fingerprint());
    }

    #[test]
    fn tampered_payload_fails_verify() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let manifest = manifest_with_caps(vec![]);
        let mut att = compose_and_sign(
            AttestationInputs {
                agent_id: "agent-1",
                session_id: Some("root"),
                root_session_id: Some("root"),
                turn_counter: 1,
                manifest: &manifest,
                gateway_node_id: "node-a",
                pending_approval_ids: vec![],
                pending_user_interaction_ids: vec![],
                pending_escalation_ids: vec![],
                budget_meters: vec![],
            },
            &key,
        )
        .unwrap();

        // Mutate the payload after signing — the body now claims a higher
        // turn counter than what the operator signed.
        att.payload.turn_counter = 999;

        let pub_bytes = key.public_key_bytes();
        let err = verify(&pub_bytes, &att).expect_err("tampered payload must reject");
        assert!(err.to_string().contains("did not verify"), "{}", err);
    }

    #[test]
    fn tampered_signature_fails_verify() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let manifest = manifest_with_caps(vec![]);
        let mut att = compose_and_sign(
            AttestationInputs {
                agent_id: "agent-1",
                session_id: Some("root"),
                root_session_id: Some("root"),
                turn_counter: 1,
                manifest: &manifest,
                gateway_node_id: "node-a",
                pending_approval_ids: vec![],
                pending_user_interaction_ids: vec![],
                pending_escalation_ids: vec![],
                budget_meters: vec![],
            },
            &key,
        )
        .unwrap();

        // Flip a base64 char that decodes to a different sig byte.
        let mut chars: Vec<char> = att.signature.chars().collect();
        chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
        att.signature = chars.into_iter().collect();

        let pub_bytes = key.public_key_bytes();
        let err = verify(&pub_bytes, &att).expect_err("tampered sig must reject");
        assert!(err.to_string().contains("did not verify"), "{}", err);
    }

    #[test]
    fn pending_approvals_truncated_with_count_preserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let manifest = manifest_with_caps(vec![]);
        let lots: Vec<String> = (0..50).map(|i| format!("apr-{:03}", i)).collect();
        let att = compose_and_sign(
            AttestationInputs {
                agent_id: "a",
                session_id: Some("s"),
                root_session_id: Some("s"),
                turn_counter: 0,
                manifest: &manifest,
                gateway_node_id: "node",
                pending_approval_ids: lots,
                pending_user_interaction_ids: vec![],
                pending_escalation_ids: vec![],
                budget_meters: vec![],
            },
            &key,
        )
        .unwrap();
        assert_eq!(att.payload.pending_approval_count, 50);
        assert_eq!(
            att.payload.pending_approval_ids.len(),
            MAX_PENDING_APPROVALS_INLINE
        );
    }

    #[test]
    fn render_tail_contains_authoritative_marker_and_json() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let manifest = manifest_with_caps(vec![]);
        let att = compose_and_sign(
            AttestationInputs {
                agent_id: "a",
                session_id: Some("s"),
                root_session_id: Some("s"),
                turn_counter: 0,
                manifest: &manifest,
                gateway_node_id: "node",
                pending_approval_ids: vec![],
                pending_user_interaction_ids: vec![],
                pending_escalation_ids: vec![],
                budget_meters: vec![],
            },
            &key,
        )
        .unwrap();
        let tail = render_tail(&att).unwrap();
        assert!(tail.contains("Gateway State Attestation"));
        assert!(tail.contains("authoritative"));
        assert!(tail.contains("<gateway_state_attestation>"));
        assert!(tail.contains("</gateway_state_attestation>"));
        assert!(tail.contains(&att.signature));
    }

    #[test]
    fn tampered_fingerprint_breaks_verification() {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = GatewayIdentityKey::load_or_generate(temp.path()).unwrap();
        let manifest = manifest_with_caps(vec![]);
        let mut att = compose_and_sign(
            AttestationInputs {
                agent_id: "a",
                session_id: Some("s"),
                root_session_id: Some("s"),
                turn_counter: 0,
                manifest: &manifest,
                gateway_node_id: "node",
                pending_approval_ids: vec![],
                pending_user_interaction_ids: vec![],
                pending_escalation_ids: vec![],
                budget_meters: vec![],
            },
            &key,
        )
        .unwrap();
        att.key_fingerprint = "0000000000000000".to_string();
        let pub_bytes = key.public_key_bytes();
        let err = verify(&pub_bytes, &att).expect_err("tampered fingerprint must reject");
        assert!(err.to_string().contains("fingerprint mismatch"), "{}", err);
    }
}
