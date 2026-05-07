//! Gateway Message Router.
//!
//! Handles JSON-RPC messages from agents. Routes `ecosystem.send_message`
//! locally or transparently over OFP federation.

use crate::server::ofp::{
    compose_local_chain_attestation, emit_federation_message_event,
    evaluate_constitution_compatibility, hmac_sign, hmac_verify, parse_ofp_response,
    sign_wire_message, verify_chain_attestation, verify_wire_message, write_framed_message,
};
use crate::server::registry::PeerRegistry;
use autonoetic_ofp::wire::{
    RemoteAgentInfo, WireMessage, WireMessageKind, WireRequest, WireResponse, PROTOCOL_VERSION,
};
use autonoetic_types::config::FederationConstitutionConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpStream;
use tracing::{debug, info};

pub struct MessageRouter {
    registry: PeerRegistry,
    node_id: String,
    shared_secret: String,
    federation_constitution: FederationConstitutionConfig,
    gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    gateway_dir: PathBuf,
}

impl MessageRouter {
    pub fn new(
        registry: PeerRegistry,
        node_id: String,
        shared_secret: String,
        federation_constitution: FederationConstitutionConfig,
    ) -> Self {
        Self {
            registry,
            node_id,
            shared_secret,
            federation_constitution,
            gateway_store: None,
            gateway_dir: PathBuf::from(".gateway"),
        }
    }

    pub fn with_gateway_store(
        mut self,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    ) -> Self {
        self.gateway_store = gateway_store;
        self
    }

    pub fn with_gateway_dir(mut self, gateway_dir: PathBuf) -> Self {
        self.gateway_dir = gateway_dir;
        self
    }

    /// Route an `ecosystem.send_message` outgoing call from a local agent.
    pub async fn route_send_message(
        &self,
        sender_agent_id: &str,
        target_agent_id: &str,
        message: &str,
    ) -> anyhow::Result<String> {
        // 1. Check if the target is remote via PeerRegistry
        if let Some(peer_node_id) = self.registry.resolve_agent_node(target_agent_id).await {
            info!(
                "Routing message from {} to remote agent {} (on node {})",
                sender_agent_id, target_agent_id, peer_node_id
            );
            return self
                .send_via_ofp(&peer_node_id, target_agent_id, message, sender_agent_id)
                .await;
        }

        // 2. Fallback to local routing
        info!(
            "Routing message from {} to local agent {}",
            sender_agent_id, target_agent_id
        );

        // TODO: Actually deliver to local agent's session inbox
        Ok("Delivered locally (stub)".to_string())
    }

    /// Forward a message to a remote peer via OFP TCP connection.
    async fn send_via_ofp(
        &self,
        peer_node_id: &str,
        target_agent: &str,
        message: &str,
        sender_agent: &str,
    ) -> anyhow::Result<String> {
        let peer = self
            .registry
            .get_peer(peer_node_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Peer {} dropped from registry", peer_node_id))?;

        debug!(
            "Connecting to OFP peer {} at {}",
            peer_node_id, peer.address
        );
        let stream = TcpStream::connect(peer.address).await?;
        let (mut reader, mut writer) = stream.into_split();

        // 1. Send Handshake
        let nonce = uuid::Uuid::new_v4().to_string();
        let auth_data = format!("{}{}", nonce, self.node_id);
        let auth_hmac = hmac_sign(&self.shared_secret, auth_data.as_bytes());

        let handshake = WireMessage {
            id: uuid::Uuid::new_v4().to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Request(WireRequest::Handshake {
                node_id: self.node_id.clone(),
                node_name: "autonoetic-router".into(), // TODO: use actual node name
                protocol_version: PROTOCOL_VERSION,
                agents: vec![RemoteAgentInfo {
                    id: sender_agent.to_string(),
                    name: sender_agent.to_string(),
                    description: "federation sender identity".to_string(),
                    tags: vec!["federation".to_string()],
                    tools: vec![],
                    state: "active".to_string(),
                }],
                nonce,
                auth_hmac,
                constitution_digest: Some(
                    crate::constitution_digest::constitution_digest().to_string(),
                ),
                constitution_profile: Some(
                    crate::constitution_digest::canonical_constitution_profile(),
                ),
                extensions: Some(vec!["msg_hmac".into()]),
            }),
        };
        write_framed_message(&mut writer, &handshake).await?;

        // Wait for HandshakeAck
        let ack = parse_ofp_response(&mut reader).await?;
        let (
            ack_node_id,
            ack_protocol_version,
            ack_nonce,
            ack_auth_hmac,
            _ack_constitution_digest,
            ack_constitution_profile,
            ack_extensions,
        ) = match ack.kind {
            WireMessageKind::Response(WireResponse::HandshakeAck {
                node_id,
                protocol_version,
                nonce,
                auth_hmac,
                constitution_digest,
                constitution_profile,
                extensions,
                ..
            }) => (
                node_id,
                protocol_version,
                nonce,
                auth_hmac,
                constitution_digest,
                constitution_profile,
                extensions,
            ),
            WireMessageKind::Response(WireResponse::Error { code, message, .. }) => {
                anyhow::bail!("Handshake failed: [{}]: {}", code, message);
            }
            _ => anyhow::bail!("Expected HandshakeAck"),
        };

        if ack_protocol_version != PROTOCOL_VERSION {
            anyhow::bail!(
                "Peer protocol mismatch: expected {}, got {}",
                PROTOCOL_VERSION,
                ack_protocol_version
            );
        }
        let ack_expected_data = format!("{}{}", ack_nonce, ack_node_id);
        if !hmac_verify(
            &self.shared_secret,
            ack_expected_data.as_bytes(),
            &ack_auth_hmac,
        ) {
            anyhow::bail!(
                "HandshakeAck HMAC verification failed for peer {}",
                peer_node_id
            );
        }
        let local_constitution_digest = crate::constitution_digest::constitution_digest();
        let local_constitution_profile =
            crate::constitution_digest::canonical_constitution_profile();
        evaluate_constitution_compatibility(
            &self.federation_constitution,
            local_constitution_digest.as_ref(),
            _ack_constitution_digest.as_deref(),
            &local_constitution_profile,
            ack_constitution_profile.as_ref(),
        )
        .map_err(|e| anyhow::anyhow!("constitutional_incompatibility: {}", e))?;
        let negotiated_extensions = ack_extensions.unwrap_or_default();
        let use_msg_hmac = negotiated_extensions
            .iter()
            .any(|ext| ext.eq_ignore_ascii_case("msg_hmac"));

        // 2. Exchange signed chain attestations (R++7)
        let (local_attestation, local_public_key_b64) = compose_local_chain_attestation(
            &self.node_id,
            &self.gateway_dir,
            self.gateway_store.clone(),
        )?;
        let mut outbound_seq: u64 = 1;
        let mut expected_inbound_seq: u64 = 1;
        let mut attestation_req = WireMessage {
            id: uuid::Uuid::new_v4().to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Request(WireRequest::ChainAttestation {
                attestation: local_attestation,
                public_key_b64: local_public_key_b64,
                request_peer_attestation: true,
            }),
        };
        if use_msg_hmac {
            attestation_req.seq_num = Some(outbound_seq);
            attestation_req.signature =
                Some(sign_wire_message(&self.shared_secret, &attestation_req)?);
            outbound_seq += 1;
        }
        write_framed_message(&mut writer, &attestation_req).await?;

        let attestation_resp = parse_ofp_response(&mut reader).await?;
        if use_msg_hmac {
            verify_wire_message(&self.shared_secret, &attestation_resp, expected_inbound_seq)?;
            expected_inbound_seq += 1;
        }
        match attestation_resp.kind {
            WireMessageKind::Response(WireResponse::ChainAttestationAck {
                accepted,
                reason,
                peer_attestation,
                peer_public_key_b64,
            }) => {
                if !accepted {
                    anyhow::bail!(
                        "chain attestation rejected by peer {}: {}",
                        peer_node_id,
                        reason.unwrap_or_else(|| "unknown reason".to_string())
                    );
                }
                let peer_attestation = peer_attestation.ok_or_else(|| {
                    anyhow::anyhow!(
                        "peer {} accepted chain attestation but omitted peer_attestation payload",
                        peer_node_id
                    )
                })?;
                let peer_public_key_b64 = peer_public_key_b64.ok_or_else(|| {
                    anyhow::anyhow!(
                        "peer {} accepted chain attestation but omitted peer_public_key_b64",
                        peer_node_id
                    )
                })?;
                verify_chain_attestation(&peer_attestation, &peer_public_key_b64).map_err(|e| {
                    anyhow::anyhow!(
                        "peer {} returned invalid chain attestation: {}",
                        peer_node_id,
                        e
                    )
                })?;
            }
            WireMessageKind::Response(WireResponse::Error { code, message, .. }) => {
                anyhow::bail!(
                    "chain attestation exchange failed [{}] from peer {}: {}",
                    code,
                    peer_node_id,
                    message
                );
            }
            other => anyhow::bail!("Expected ChainAttestationAck, got {:?}", other),
        }

        // 3. Send AgentMessage with peer_event_ref.
        let local_peer_event_ref = emit_federation_message_event(
            self.gateway_store.clone(),
            &self.node_id,
            peer_node_id,
            "agent_message_outbound",
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(sender_agent),
            Some(target_agent),
            message,
            None,
        );
        let mut agent_msg = WireMessage {
            id: uuid::Uuid::new_v4().to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Request(WireRequest::AgentMessage {
                agent: target_agent.to_string(),
                message: message.to_string(),
                sender: Some(sender_agent.to_string()),
                peer_event_ref: Some(local_peer_event_ref),
            }),
        };
        if use_msg_hmac {
            agent_msg.seq_num = Some(outbound_seq);
            agent_msg.signature = Some(sign_wire_message(&self.shared_secret, &agent_msg)?);
        }
        write_framed_message(&mut writer, &agent_msg).await?;

        // 4. Wait for AgentResponse / Error and correlate peer refs.
        let resp = parse_ofp_response(&mut reader).await?;
        if use_msg_hmac {
            verify_wire_message(&self.shared_secret, &resp, expected_inbound_seq)?;
        }
        match resp.kind {
            WireMessageKind::Response(WireResponse::AgentResponse {
                text,
                peer_event_ref,
            }) => {
                let _ = emit_federation_message_event(
                    self.gateway_store.clone(),
                    &self.node_id,
                    peer_node_id,
                    "agent_message_response",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    Some(sender_agent),
                    Some(target_agent),
                    message,
                    peer_event_ref.as_ref(),
                );
                Ok(text)
            }
            WireMessageKind::Response(WireResponse::Error {
                code,
                message: error_message,
                peer_event_ref,
            }) => {
                let _ = emit_federation_message_event(
                    self.gateway_store.clone(),
                    &self.node_id,
                    peer_node_id,
                    "agent_message_response",
                    autonoetic_types::causal_chain::EntryStatus::Error,
                    Some(sender_agent),
                    Some(target_agent),
                    message,
                    peer_event_ref.as_ref(),
                );
                anyhow::bail!("Agent error [{}]: {}", code, error_message);
            }
            _ => anyhow::bail!("Expected AgentResponse"),
        }
    }
}
