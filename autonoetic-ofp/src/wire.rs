//! OFP WireMessage — the base envelope for federation and IPC.
//!
//! All communication between Autonoetic/OpenFang peers uses JSON-framed messages
//! over TCP. Each message is prefixed with a 4-byte big-endian length header.
//!
//! Autonoetic is 100% wire-compatible with OpenFang.
//! Autonoetic extensions (`signature`, `seq_num`, `extensions`) are ignored by OpenFang
//! because OpenFang uses default serde parsing (which drops unknown fields).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A wire protocol message (envelope).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage {
    /// Unique message ID.
    pub id: String,

    /// Autonoetic extension: Per-message HMAC-SHA256 signature (if negotiated).
    /// Prevents session hijack and replay attacks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    /// Autonoetic extension: Sequence number for replay prevention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq_num: Option<u64>,

    /// Message variant (flattened directly into the JSON object).
    #[serde(flatten)]
    pub kind: WireMessageKind,
}

/// The different kinds of wire messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WireMessageKind {
    /// Request from one peer to another.
    #[serde(rename = "request")]
    Request(WireRequest),
    /// Response to a request.
    #[serde(rename = "response")]
    Response(WireResponse),
    /// One-way notification (no response expected).
    #[serde(rename = "notification")]
    Notification(WireNotification),
}

/// Request messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum WireRequest {
    /// Handshake: exchange peer identity.
    #[serde(rename = "handshake")]
    Handshake {
        /// The peer's unique node ID.
        node_id: String,
        /// Human-readable node name.
        node_name: String,
        /// Protocol version.
        protocol_version: u32,
        /// List of agents available on this peer.
        agents: Vec<RemoteAgentInfo>,
        /// Random nonce for HMAC authentication.
        #[serde(default)]
        nonce: String,
        /// HMAC-SHA256(shared_secret, nonce + node_id).
        #[serde(default)]
        auth_hmac: String,
        /// Autonoetic extension: canonical constitution digest advertised by sender.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        constitution_digest: Option<String>,
        /// Autonoetic extension: canonical constitution profile (rule/right tables)
        /// used for superset compatibility checks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        constitution_profile: Option<ConstitutionProfile>,
        /// Autonoetic extension: list of supported protocol extensions (e.g., ["msg_hmac", "resilience"]).
        #[serde(skip_serializing_if = "Option::is_none")]
        extensions: Option<Vec<String>>,
    },
    /// Discover agents matching a query on the remote peer.
    #[serde(rename = "discover")]
    Discover {
        /// Search query (matches name, tags, description).
        query: String,
    },
    /// Send a message to a specific agent on the remote peer.
    #[serde(rename = "agent_message")]
    AgentMessage {
        /// Target agent ID or name on the remote peer.
        agent: String,
        /// The message text.
        message: String,
        /// Optional sender identity.
        sender: Option<String>,
        /// Optional cross-gateway causal correlation reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_event_ref: Option<PeerEventRef>,
    },
    /// Exchange signed chain attestation digests.
    #[serde(rename = "chain_attestation")]
    ChainAttestation {
        /// Signed chain attestation from the requester.
        attestation: ChainAttestation,
        /// Requester Ed25519 public key (base64, 32 bytes).
        public_key_b64: String,
        /// Request that peer includes its own attestation in the ack.
        #[serde(default)]
        request_peer_attestation: bool,
    },
    /// Ping to check if the peer is alive.
    #[serde(rename = "ping")]
    Ping,

    /// Offer a Cognitive Capsule for transfer to the peer.
    /// The peer responds with `CapsuleAccept` (or `Error`), and the
    /// originator then streams chunks via `CapsuleData` followed by
    /// `CapsuleComplete`. Requires the `capsule_transfer` extension to
    /// be advertised by both peers at handshake time.
    #[serde(rename = "capsule_offer")]
    CapsuleOffer {
        /// ID of the capsule being offered.
        capsule_id: String,
        /// SHA-256 of the canonical manifest JSON (with the `signature`
        /// field cleared). Lets the receiver verify the eventual
        /// `capsule.json` matches what was advertised.
        manifest_digest: String,
        /// Total archive size in bytes.
        size_bytes: u64,
    },

    /// Stream a chunk of capsule archive bytes. `chunk_index` is zero-based
    /// and monotonically increasing within a single offer.
    #[serde(rename = "capsule_data")]
    CapsuleData {
        capsule_id: String,
        chunk_index: u32,
        /// Raw archive bytes (base64-encoded inside JSON).
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },

    /// Signal end of stream. The receiver computes a SHA-256 over the
    /// reassembled bytes and verifies it matches `digest`.
    #[serde(rename = "capsule_complete")]
    CapsuleComplete {
        capsule_id: String,
        /// SHA-256 hex of the reassembled archive bytes.
        digest: String,
    },
}

/// Response messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum WireResponse {
    /// Handshake acknowledgement.
    #[serde(rename = "handshake_ack")]
    HandshakeAck {
        node_id: String,
        node_name: String,
        protocol_version: u32,
        agents: Vec<RemoteAgentInfo>,
        /// Random nonce for HMAC authentication.
        #[serde(default)]
        nonce: String,
        /// HMAC-SHA256(shared_secret, nonce + node_id).
        #[serde(default)]
        auth_hmac: String,
        /// Autonoetic extension: canonical constitution digest advertised by sender.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        constitution_digest: Option<String>,
        /// Autonoetic extension: canonical constitution profile (rule/right tables)
        /// used for superset compatibility checks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        constitution_profile: Option<ConstitutionProfile>,
        /// Autonoetic extension: the extensions this peer agreed to enable.
        #[serde(skip_serializing_if = "Option::is_none")]
        extensions: Option<Vec<String>>,
    },
    /// Discovery results.
    #[serde(rename = "discover_result")]
    DiscoverResult { agents: Vec<RemoteAgentInfo> },
    /// Agent message response.
    #[serde(rename = "agent_response")]
    AgentResponse {
        /// The agent's response text.
        text: String,
        /// Optional cross-gateway causal correlation reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_event_ref: Option<PeerEventRef>,
        /// Whether the agent is suspended (waiting for children, approval, or user input).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suspended: Option<bool>,
        /// The kind of suspension, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suspension_kind: Option<String>,
    },
    /// Acknowledgement for `chain_attestation`.
    #[serde(rename = "chain_attestation_ack")]
    ChainAttestationAck {
        /// Whether attestation verification succeeded.
        accepted: bool,
        /// Optional reason when `accepted=false`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Optional peer attestation when requested.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_attestation: Option<ChainAttestation>,
        /// Optional peer Ed25519 public key (base64, 32 bytes).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_public_key_b64: Option<String>,
    },
    /// Pong response.
    #[serde(rename = "pong")]
    Pong {
        /// Uptime in seconds.
        uptime_secs: u64,
    },
    /// Error response.
    #[serde(rename = "error")]
    Error {
        /// Error code.
        code: i32,
        /// Error message.
        message: String,
        /// Optional cross-gateway causal correlation reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_event_ref: Option<PeerEventRef>,
    },

    /// Accept a previously-received `CapsuleOffer` — the originator may
    /// now begin streaming `CapsuleData` chunks.
    #[serde(rename = "capsule_accept")]
    CapsuleAccept { capsule_id: String },

    /// Acknowledge a completed capsule transfer with the import outcome.
    #[serde(rename = "capsule_ack")]
    CapsuleAck {
        capsule_id: String,
        /// Whether the import was successful on the receiver.
        imported: bool,
        /// Optional error reason when `imported = false`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// Revision ID created on the receiver (when imported).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision_id: Option<String>,
    },
}

/// Base64 helper for the `Vec<u8>` payload of [`WireRequest::CapsuleData`].
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

/// Name of the OFP extension that gates the `capsule_transfer` family.
/// Both peers must advertise this string in their handshake to enable
/// `CapsuleOffer` / `CapsuleAccept` / `CapsuleData` / `CapsuleComplete`
/// / `CapsuleAck`.
pub const CAPSULE_TRANSFER_EXTENSION: &str = "capsule_transfer";

/// Notification messages (one-way, no response).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum WireNotification {
    /// An agent was spawned on the peer.
    #[serde(rename = "agent_spawned")]
    AgentSpawned { agent: RemoteAgentInfo },
    /// An agent was terminated on the peer.
    #[serde(rename = "agent_terminated")]
    AgentTerminated { agent_id: String },
    /// Peer is shutting down.
    #[serde(rename = "shutting_down")]
    ShuttingDown,
}

/// Reference to the corresponding event on the remote gateway chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerEventRef {
    /// Gateway identity that emitted the referenced event.
    pub gateway_id: String,
    /// Event identifier on that gateway.
    pub event_id: String,
    /// Hash of the referenced chain entry.
    pub entry_hash: String,
}

/// Signed digest of a gateway's current chain prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainAttestation {
    /// Gateway identity that signed this attestation.
    pub gateway_id: String,
    /// Event ID at the attested prefix tip.
    pub event_id: String,
    /// Hash of chain prefix at `event_id`.
    pub chain_prefix_hash: String,
    /// RFC3339 timestamp when this digest was signed.
    pub attested_at: String,
    /// First 8 bytes of signing public key (hex).
    pub key_fingerprint: String,
    /// Base64 Ed25519 signature over canonical attestation payload.
    pub signature_b64: String,
}

/// Information about a remote agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteAgentInfo {
    /// Agent ID (UUID string).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Description of what the agent does.
    pub description: String,
    /// Tags for categorization/discovery.
    pub tags: Vec<String>,
    /// Available tools.
    pub tools: Vec<String>,
    /// Current state.
    pub state: String,
}

/// Canonical constitutional enforcement tables used for compatibility checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConstitutionProfile {
    /// Canonical rule ID -> enforcement citation table.
    pub rules_enforcement: BTreeMap<String, String>,
    /// Canonical right ID -> enforcement citation table.
    pub rights_enforcement: BTreeMap<String, String>,
}

/// Current protocol version. OpenFang expects 1.
pub const PROTOCOL_VERSION: u32 = 1;

/// Encode a wire message to bytes (4-byte big-endian length + JSON).
pub fn encode_message(msg: &WireMessage) -> Result<Vec<u8>, serde_json::Error> {
    let json = serde_json::to_vec(msg)?;
    let len = json.len() as u32;
    let mut bytes = Vec::with_capacity(4 + json.len());
    bytes.extend_from_slice(&len.to_be_bytes());
    bytes.extend_from_slice(&json);
    Ok(bytes)
}

/// Decode the length prefix from a 4-byte header.
pub fn decode_length(header: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*header)
}

/// Parse a JSON body into a WireMessage.
pub fn decode_message(body: &[u8]) -> Result<WireMessage, serde_json::Error> {
    serde_json::from_slice(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let msg = WireMessage {
            id: "msg-1".to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Request(WireRequest::Ping),
        };
        let bytes = encode_message(&msg).unwrap();
        // First 4 bytes are length
        let len = decode_length(&[bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(len as usize, bytes.len() - 4);
        let decoded = decode_message(&bytes[4..]).unwrap();
        assert_eq!(decoded.id, "msg-1");
    }

    #[test]
    fn capsule_offer_request_roundtrip() {
        let msg = WireMessage {
            id: "cap-offer-1".to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Request(WireRequest::CapsuleOffer {
                capsule_id: "cap_sha256:abc".to_string(),
                manifest_digest: "deadbeef".to_string(),
                size_bytes: 1024,
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("capsule_offer"));
        let decoded: WireMessage = serde_json::from_str(&json).unwrap();
        match decoded.kind {
            WireMessageKind::Request(WireRequest::CapsuleOffer {
                capsule_id,
                manifest_digest,
                size_bytes,
            }) => {
                assert_eq!(capsule_id, "cap_sha256:abc");
                assert_eq!(manifest_digest, "deadbeef");
                assert_eq!(size_bytes, 1024);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn capsule_data_base64_roundtrip() {
        let payload = vec![1u8, 2, 3, 4, 5];
        let msg = WireMessage {
            id: "cap-data-1".to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Request(WireRequest::CapsuleData {
                capsule_id: "cap_sha256:abc".to_string(),
                chunk_index: 0,
                data: payload.clone(),
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WireMessage = serde_json::from_str(&json).unwrap();
        match decoded.kind {
            WireMessageKind::Request(WireRequest::CapsuleData {
                data, chunk_index, ..
            }) => {
                assert_eq!(data, payload);
                assert_eq!(chunk_index, 0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn capsule_ack_response_roundtrip() {
        let msg = WireMessage {
            id: "cap-ack-1".to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Response(WireResponse::CapsuleAck {
                capsule_id: "cap_x".to_string(),
                imported: true,
                reason: None,
                revision_id: Some("rev_sha256:xyz".to_string()),
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("capsule_ack"));
        let decoded: WireMessage = serde_json::from_str(&json).unwrap();
        match decoded.kind {
            WireMessageKind::Response(WireResponse::CapsuleAck {
                imported,
                revision_id,
                ..
            }) => {
                assert!(imported);
                assert_eq!(revision_id.as_deref(), Some("rev_sha256:xyz"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn capsule_transfer_extension_name_is_stable() {
        assert_eq!(CAPSULE_TRANSFER_EXTENSION, "capsule_transfer");
    }

    #[test]
    fn test_handshake_serialization_with_extensions() {
        let msg = WireMessage {
            id: "hs-1".to_string(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Request(WireRequest::Handshake {
                node_id: "node-abc".to_string(),
                node_name: "autonoetic-kernel".to_string(),
                protocol_version: PROTOCOL_VERSION,
                agents: vec![RemoteAgentInfo {
                    id: "agent-1".to_string(),
                    name: "coder".to_string(),
                    description: "A coding agent".to_string(),
                    tags: vec!["code".to_string()],
                    tools: vec!["file_read".to_string()],
                    state: "running".to_string(),
                }],
                nonce: "test-nonce".to_string(),
                auth_hmac: "test-hmac".to_string(),
                constitution_digest: Some("abc123".to_string()),
                constitution_profile: Some(ConstitutionProfile {
                    rules_enforcement: BTreeMap::from([(
                        "P-1.1".to_string(),
                        "tool_call_processor".to_string(),
                    )]),
                    rights_enforcement: BTreeMap::from([(
                        "Ri-0.10".to_string(),
                        "constitution_read".to_string(),
                    )]),
                }),
                extensions: Some(vec!["msg_hmac".to_string(), "resilience".to_string()]),
            }),
        };
        let json = serde_json::to_string_pretty(&msg).unwrap();
        assert!(json.contains("handshake"));
        assert!(json.contains("coder"));
        assert!(json.contains("constitution_digest"));
        assert!(json.contains("constitution_profile"));
        assert!(json.contains("msg_hmac")); // Extension is serialized

        let decoded: WireMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "hs-1");

        if let WireMessageKind::Request(WireRequest::Handshake {
            constitution_digest,
            constitution_profile,
            extensions,
            ..
        }) = decoded.kind
        {
            assert_eq!(constitution_digest.as_deref(), Some("abc123"));
            assert!(constitution_profile.is_some());
            assert!(extensions.is_some());
            assert_eq!(extensions.unwrap().len(), 2);
        } else {
            panic!("Wrong message kind");
        }
    }
}
