//! OpenFang Protocol (OFP) Gateway Server.
//!
//! Handles TCP listener for incoming federated peers, length-prefixed framing,
//! and HMAC-SHA256 authenticated handshakes with optional extensions (msg_hmac).

use crate::server::registry::{PeerEntry, PeerRegistry, PeerState};
use crate::server::transport::TransportListener;
use autonoetic_ofp::wire::{
    decode_length, decode_message, encode_message, ChainAttestation, ConstitutionProfile,
    PeerEventRef, WireMessage, WireMessageKind, WireRequest, WireResponse, PROTOCOL_VERSION,
};
use autonoetic_types::config::{
    FederationConstitutionConfig, FederationConstitutionMode, GatewayConfig,
};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

type HmacSha256 = Hmac<Sha256>;

const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024; // 16 MB

pub fn evaluate_constitution_compatibility(
    config: &FederationConstitutionConfig,
    local_digest: &str,
    peer_digest: Option<&str>,
    local_profile: &ConstitutionProfile,
    peer_profile: Option<&ConstitutionProfile>,
) -> anyhow::Result<()> {
    let mode = config.mode;
    let known_compatible = &config.known_compatible_digests;

    let peer_digest = match peer_digest {
        Some(digest) => digest,
        None if config.allow_missing_peer_digest => return Ok(()),
        None => anyhow::bail!("peer did not advertise constitution_digest"),
    };

    let exact = peer_digest == local_digest;
    let known = known_compatible.iter().any(|digest| digest == peer_digest);

    match mode {
        FederationConstitutionMode::Exact => {
            if exact {
                Ok(())
            } else {
                anyhow::bail!(
                    "digest mismatch (local={}, peer={})",
                    local_digest,
                    peer_digest
                )
            }
        }
        FederationConstitutionMode::KnownCompatible => {
            if exact || known {
                Ok(())
            } else {
                anyhow::bail!(
                    "peer digest not in known-compatible set (local={}, peer={})",
                    local_digest,
                    peer_digest
                )
            }
        }
        FederationConstitutionMode::Superset => {
            if exact || known {
                return Ok(());
            }
            let peer_profile = peer_profile
                .ok_or_else(|| anyhow::anyhow!("peer did not advertise constitution_profile"))?;
            ensure_constitution_profile_superset(local_profile, peer_profile)
        }
    }
}

fn ensure_constitution_profile_superset(
    local_profile: &ConstitutionProfile,
    peer_profile: &ConstitutionProfile,
) -> anyhow::Result<()> {
    ensure_table_superset(
        "rules",
        &local_profile.rules_enforcement,
        &peer_profile.rules_enforcement,
    )?;
    ensure_table_superset(
        "rights",
        &local_profile.rights_enforcement,
        &peer_profile.rights_enforcement,
    )?;
    Ok(())
}

fn ensure_table_superset(
    table_name: &str,
    local_table: &std::collections::BTreeMap<String, String>,
    peer_table: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for (id, local_enforcement) in local_table {
        let Some(peer_enforcement) = peer_table.get(id) else {
            anyhow::bail!("peer missing {} entry {}", table_name, id);
        };
        if peer_enforcement != local_enforcement {
            anyhow::bail!(
                "peer {} entry {} differs (local='{}', peer='{}')",
                table_name,
                id,
                local_enforcement,
                peer_enforcement
            );
        }
    }
    Ok(())
}

fn federation_constitution_mode_label(mode: FederationConstitutionMode) -> &'static str {
    match mode {
        FederationConstitutionMode::Exact => "exact",
        FederationConstitutionMode::KnownCompatible => "known_compatible",
        FederationConstitutionMode::Superset => "superset",
    }
}

fn emit_federation_constitution_event(
    gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    peer_node_id: &str,
    peer_addr: SocketAddr,
    local_digest: &str,
    peer_digest: Option<&str>,
    mode: FederationConstitutionMode,
    status: autonoetic_types::causal_chain::EntryStatus,
    reason: Option<String>,
) {
    let Some(store) = gateway_store else {
        return;
    };

    let now = chrono::Utc::now();
    let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
    rules.push("P-10.9".to_string());
    let payload = serde_json::json!({
        "peer_node_id": peer_node_id,
        "peer_addr": peer_addr.to_string(),
        "local_constitution_digest": local_digest,
        "peer_constitution_digest": peer_digest,
        "compatibility_mode": federation_constitution_mode_label(mode),
    });
    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: format!("federation-constitution-{}", uuid::Uuid::new_v4()),
        agent_id: "gateway".to_string(),
        session_id: "system".to_string(),
        turn_id: None,
        event_seq: now.timestamp_millis().max(0) as u64,
        timestamp: now.to_rfc3339(),
        category: "federation".to_string(),
        action: "constitution_check".to_string(),
        status: status.to_string(),
        enforced_rules: rules,
        target: Some(peer_node_id.to_string()),
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason,
    };

    if let Err(e) = store.create_causal_event(&event) {
        warn!(
            target: "federation",
            error = %e,
            peer_node_id = peer_node_id,
            "Failed to write federation constitution causal event"
        );
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn federation_rules_for_message_continuity() -> Vec<String> {
    let mut rules = autonoetic_types::causal_chain::default_enforced_rules();
    rules.push("P-10.6".to_string());
    rules.push("P-10.6".to_string());
    rules
}

/// The gateway's chain tip, witness-first (#1278).
///
/// The lean JSONL witness is tamper-evident end to end (per-entry hashes plus
/// prev-linkage), while `causal_events` is a mutable SQLite mirror — an
/// attestation composed from the DB attests whatever the DB currently says.
/// So the tip comes from the witness's latest `federation` entry; the DB
/// search remains only as a fallback for witnesses that predate #1278 and
/// hold no federation entries. The witness hash is opaque to peers (they
/// verify the signature, not the derivation), so the hash *kind* changing is
/// not a protocol break.
fn latest_federation_chain_tip(
    gateway_dir: &Path,
    gateway_store: Option<&Arc<crate::scheduler::gateway_store::GatewayStore>>,
) -> (String, String) {
    let genesis = ("genesis".to_string(), sha256_hex(b"genesis"));
    let history_dir = gateway_dir.join("history");
    match crate::causal_chain::read_all_entries_across_segments(&history_dir) {
        Ok(entries) => {
            if let Some(entry) = entries.iter().rev().find(|e| e.category == "federation") {
                return (entry.log_id.clone(), entry.entry_hash.clone());
            }
        }
        Err(e) => {
            warn!(
                target: "federation",
                error = %e,
                "failed to read causal witness for chain tip; falling back to DB"
            );
        }
    }

    let Some(store) = gateway_store else {
        return genesis;
    };
    let events = match store.search_causal_events(Some("system"), Some("gateway"), 512) {
        Ok(events) => events,
        Err(_) => return genesis,
    };
    for event in events {
        if event.category != "federation" {
            continue;
        }
        let Some(raw_payload) = event.payload else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw_payload) else {
            continue;
        };
        let Some(entry_hash) = payload.get("entry_hash").and_then(|v| v.as_str()) else {
            continue;
        };
        if entry_hash.trim().is_empty() {
            continue;
        }
        return (event.event_id, entry_hash.to_string());
    }
    genesis
}

fn canonical_chain_attestation_payload(
    gateway_id: &str,
    event_id: &str,
    chain_prefix_hash: &str,
    attested_at: &str,
    key_fingerprint: &str,
) -> anyhow::Result<Vec<u8>> {
    #[derive(serde::Serialize)]
    struct ChainAttestationPayload<'a> {
        gateway_id: &'a str,
        event_id: &'a str,
        chain_prefix_hash: &'a str,
        attested_at: &'a str,
        key_fingerprint: &'a str,
    }
    Ok(serde_json::to_vec(&ChainAttestationPayload {
        gateway_id,
        event_id,
        chain_prefix_hash,
        attested_at,
        key_fingerprint,
    })?)
}

/// Compose a signed chain attestation digest for federation continuity checks.
pub fn compose_chain_attestation(
    gateway_id: &str,
    gateway_dir: &Path,
    event_id: String,
    chain_prefix_hash: String,
) -> anyhow::Result<(ChainAttestation, String)> {
    let key = crate::runtime::crypto::GatewayIdentityKey::load_or_generate(gateway_dir)?;
    let key_fingerprint = key.fingerprint();
    let attested_at = chrono::Utc::now().to_rfc3339();
    let payload = canonical_chain_attestation_payload(
        gateway_id,
        &event_id,
        &chain_prefix_hash,
        &attested_at,
        &key_fingerprint,
    )?;
    let signature_b64 = key.sign(&payload);
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let public_key_b64 = STANDARD.encode(key.public_key_bytes());
    Ok((
        ChainAttestation {
            gateway_id: gateway_id.to_string(),
            event_id,
            chain_prefix_hash,
            attested_at,
            key_fingerprint,
            signature_b64,
        },
        public_key_b64,
    ))
}

/// Compose an attestation from the latest locally persisted federation chain tip.
pub fn compose_local_chain_attestation(
    gateway_id: &str,
    gateway_dir: &Path,
    gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
) -> anyhow::Result<(ChainAttestation, String)> {
    let (event_id, chain_prefix_hash) = latest_federation_chain_tip(gateway_dir, gateway_store.as_ref());
    compose_chain_attestation(gateway_id, gateway_dir, event_id, chain_prefix_hash)
}

/// Verify signature + key fingerprint on a received chain attestation.
pub fn verify_chain_attestation(
    attestation: &ChainAttestation,
    public_key_b64: &str,
) -> anyhow::Result<()> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let key_bytes = STANDARD
        .decode(public_key_b64)
        .map_err(|e| anyhow::anyhow!("invalid attestation public key: {}", e))?;
    anyhow::ensure!(
        key_bytes.len() == 32,
        "invalid attestation public key length (expected 32, got {})",
        key_bytes.len()
    );
    let mut key_arr = [0u8; 32];
    key_arr.copy_from_slice(&key_bytes);
    let expected_fp = hex::encode(&key_arr[..8]);
    anyhow::ensure!(
        expected_fp == attestation.key_fingerprint,
        "attestation fingerprint mismatch (expected {}, got {})",
        expected_fp,
        attestation.key_fingerprint
    );
    let payload = canonical_chain_attestation_payload(
        &attestation.gateway_id,
        &attestation.event_id,
        &attestation.chain_prefix_hash,
        &attestation.attested_at,
        &attestation.key_fingerprint,
    )?;
    let valid = crate::runtime::crypto::verify_attestation_signature(
        &key_arr,
        &payload,
        &attestation.signature_b64,
    )?;
    anyhow::ensure!(valid, "attestation signature verification failed");
    Ok(())
}

/// Emit a federation message event and return the local peer reference.
///
/// The event is witnessed in the gateway's causal chain *before* the DB
/// mirror write (#1278): the chain tip that `compose_local_chain_attestation`
/// signs comes from the witness, so a federation continuity event that never
/// reached the witness would be invisible to attestation. The DB write stays
/// the queryable mirror; the witness write is best-effort and warns loudly
/// on failure.
#[allow(clippy::too_many_arguments)]
pub fn emit_federation_message_event(
    gateway_dir: &Path,
    gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
    local_gateway_id: &str,
    peer_gateway_id: &str,
    action: &str,
    status: autonoetic_types::causal_chain::EntryStatus,
    sender_agent: Option<&str>,
    target_agent: Option<&str>,
    message: &str,
    peer_event_ref: Option<&PeerEventRef>,
) -> PeerEventRef {
    let event_id = format!("federation-msg-{}", uuid::Uuid::new_v4());
    let message_sha256 = sha256_hex(message.as_bytes());
    let canonical = serde_json::json!({
        "gateway_id": local_gateway_id,
        "peer_gateway_id": peer_gateway_id,
        "event_id": event_id,
        "action": action,
        "status": status.to_string(),
        "sender_agent": sender_agent,
        "target_agent": target_agent,
        "message_sha256": message_sha256,
        "peer_event_ref": peer_event_ref,
    });
    let canonical_bytes = serde_json::to_vec(&canonical)
        .expect("federation continuity canonical payload must serialize");
    let entry_hash = sha256_hex(&canonical_bytes);
    let local_ref = PeerEventRef {
        gateway_id: local_gateway_id.to_string(),
        event_id: event_id.clone(),
        entry_hash: entry_hash.clone(),
    };

    let now = chrono::Utc::now();
    let payload = serde_json::json!({
        "gateway_id": local_gateway_id,
        "peer_gateway_id": peer_gateway_id,
        "sender_agent": sender_agent,
        "target_agent": target_agent,
        "message_sha256": message_sha256,
        "peer_event_ref": peer_event_ref,
        "entry_hash": entry_hash,
    });

    // Witness first: the attestation tip is read from this file.
    let witness_path = gateway_dir.join("history").join("causal_chain.jsonl");
    if let Some(parent) = witness_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match crate::causal_chain::CausalLogger::new(&witness_path) {
        Ok(logger) => {
            if let Err(e) = logger.log(
                "gateway",
                "system",
                None,
                now.timestamp_millis().max(0) as u64,
                "federation",
                action,
                status.clone(),
                Some(peer_gateway_id),
                &federation_rules_for_message_continuity(),
                Some(crate::log_redaction::RedactedPayload::from_redacted(
                    payload.clone(),
                )),
            ) {
                warn!(
                    target: "federation",
                    error = %e,
                    peer_gateway_id = peer_gateway_id,
                    action = action,
                    "Failed to witness federation continuity event in the causal chain"
                );
            }
        }
        Err(e) => warn!(
            target: "federation",
            error = %e,
            peer_gateway_id = peer_gateway_id,
            action = action,
            "Failed to open causal witness for federation continuity event"
        ),
    }

    let Some(store) = gateway_store else {
        return local_ref;
    };

    let event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id,
        agent_id: "gateway".to_string(),
        session_id: "system".to_string(),
        turn_id: None,
        event_seq: now.timestamp_millis().max(0) as u64,
        timestamp: now.to_rfc3339(),
        category: "federation".to_string(),
        action: action.to_string(),
        status: status.to_string(),
        enforced_rules: federation_rules_for_message_continuity(),
        target: Some(peer_gateway_id.to_string()),
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: None,
    };
    if let Err(e) = store.create_causal_event(&event) {
        warn!(
            target: "federation",
            error = %e,
            peer_gateway_id = peer_gateway_id,
            action = action,
            "Failed to write federation continuity causal event"
        );
    }
    local_ref
}

/// Generate HMAC-SHA256 signature for message authentication.
pub fn hmac_sign(secret: &str, data: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(data);
    hex::encode(mac.finalize().into_bytes())
}

/// Verify HMAC-SHA256 signature using constant-time comparison.
pub fn hmac_verify(secret: &str, data: &[u8], signature: impl AsRef<str>) -> bool {
    let expected = hmac_sign(secret, data);
    subtle::ConstantTimeEq::ct_eq(expected.as_bytes(), signature.as_ref().as_bytes()).into()
}

fn unsigned_wire_payload(msg: &WireMessage) -> anyhow::Result<Vec<u8>> {
    let mut unsigned = msg.clone();
    // signature is excluded from the signed payload to avoid self-reference.
    unsigned.signature = None;
    Ok(serde_json::to_vec(&unsigned)?)
}

/// Sign an OFP wire message envelope for `msg_hmac`.
pub fn sign_wire_message(secret: &str, msg: &WireMessage) -> anyhow::Result<String> {
    let payload = unsigned_wire_payload(msg)?;
    Ok(hmac_sign(secret, &payload))
}

/// Verify sequence and signature constraints for `msg_hmac`.
pub fn verify_wire_message(
    secret: &str,
    msg: &WireMessage,
    expected_seq: u64,
) -> anyhow::Result<()> {
    let actual_seq = msg
        .seq_num
        .ok_or_else(|| anyhow::anyhow!("Missing seq_num for msg_hmac-protected message"))?;
    if actual_seq != expected_seq {
        anyhow::bail!(
            "Invalid sequence number: expected {}, got {}",
            expected_seq,
            actual_seq
        );
    }

    let signature = msg
        .signature
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Missing signature for msg_hmac-protected message"))?;
    let payload = unsigned_wire_payload(msg)?;
    if !hmac_verify(secret, &payload, signature) {
        anyhow::bail!("Invalid message signature");
    }

    Ok(())
}

/// Start the OFP TCP listener
pub async fn start_ofp_server(
    listen_addr: SocketAddr,
    node_id: String,
    node_name: String,
    config: Arc<GatewayConfig>,
    shared_secret: String,
    registry: PeerRegistry,
    router: std::sync::Arc<crate::router::JsonRpcRouter>,
) -> anyhow::Result<()> {
    crate::constitution_digest::initialize_constitution(config.as_ref()).map_err(|e| {
        anyhow::anyhow!(
            "failed to initialize constitution for OFP server (source='{}', lock='{}'): {}",
            config.constitution.source_path.display(),
            config.constitution.lock_path.display(),
            e
        )
    })?;
    let mut listener = crate::server::transport::TcpListenerAdapter::bind(listen_addr).await?;
    info!(
        "OFP Server listening on {} (node_id={})",
        listener.local_addr()?,
        node_id
    );

    loop {
        match listener.accept().await {
            Ok((conn, addr)) => {
                // OFP federation structurally needs inet peer addresses: the
                // peer registry stores them for dial-back. A non-TCP
                // TransportListener therefore cannot serve federation without
                // deeper registry changes — fail loudly rather than fake one.
                let peer_addr = addr.as_tcp().ok_or_else(|| {
                    anyhow::anyhow!(
                        "OFP server requires an inet peer address (TCP transport), got {addr}"
                    )
                })?;
                debug!("OFP: accepted connection from {}", peer_addr);
                let node_id_clone = node_id.clone();
                let node_name_clone = node_name.clone();
                let secret_clone = shared_secret.clone();
                let registry_clone = registry.clone();
                let router_clone = router.clone();
                let config_clone = config.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_inbound_connection(
                        conn,
                        peer_addr,
                        node_id_clone,
                        node_name_clone,
                        config_clone,
                        secret_clone,
                        registry_clone,
                        router_clone,
                    )
                    .await
                    {
                        warn!("OFP connection from {} closed: {}", peer_addr, e);
                    }
                });
            }
            Err(e) => {
                error!("OFP accept error: {}", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_inbound_connection(
    conn: crate::server::transport::BoxedConnection,
    peer_addr: SocketAddr,
    local_node_id: String,
    local_node_name: String,
    config: Arc<GatewayConfig>,
    shared_secret: String,
    registry: PeerRegistry,
    router: std::sync::Arc<crate::router::JsonRpcRouter>,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(conn);
    let gateway_store = router.execution_service().gateway_store();
    let gateway_dir = crate::execution::gateway_root_dir(config.as_ref());

    // 1. Read the handshake request
    let msg = parse_ofp_response(&mut reader).await?;

    let (
        peer_node_id,
        peer_node_name,
        peer_protocol_version,
        peer_agents,
        peer_constitution_digest,
        peer_constitution_profile,
        peer_extensions,
    ) = match msg.kind {
        WireMessageKind::Request(WireRequest::Handshake {
            node_id,
            node_name,
            protocol_version,
            agents,
            nonce,
            auth_hmac,
            constitution_digest,
            constitution_profile,
            extensions,
        }) => {
            if protocol_version != PROTOCOL_VERSION {
                let err = WireMessage {
                    id: msg.id.clone(),
                    signature: None,
                    seq_num: None,
                    kind: WireMessageKind::Response(WireResponse::Error {
                        code: 1,
                        message: format!("Version mismatch. Expected {}", PROTOCOL_VERSION),
                        peer_event_ref: None,
                    }),
                };
                write_framed_message(&mut writer, &err).await?;
                anyhow::bail!("Protocol version mismatch from {}", peer_addr);
            }

            // Verify HMAC
            let expected_data = format!("{}{}", nonce, node_id);
            if !hmac_verify(&shared_secret, expected_data.as_bytes(), &auth_hmac) {
                let err = WireMessage {
                    id: msg.id.clone(),
                    signature: None,
                    seq_num: None,
                    kind: WireMessageKind::Response(WireResponse::Error {
                        code: 403,
                        message: "HMAC authentication failed".into(),
                        peer_event_ref: None,
                    }),
                };
                write_framed_message(&mut writer, &err).await?;
                anyhow::bail!("HMAC auth failed for {}", peer_addr);
            }

            info!(
                "OFP: authenticated handshake via {} ({}) from {} [{} agents]",
                node_name,
                node_id,
                peer_addr,
                agents.len()
            );
            (
                node_id,
                node_name,
                protocol_version,
                agents,
                constitution_digest,
                constitution_profile,
                extensions.unwrap_or_default(),
            )
        }
        _ => {
            let err = WireMessage {
                id: msg.id.clone(),
                signature: None,
                seq_num: None,
                kind: WireMessageKind::Response(WireResponse::Error {
                    code: 401,
                    message: "First message must be Handshake".into(),
                    peer_event_ref: None,
                }),
            };
            write_framed_message(&mut writer, &err).await?;
            anyhow::bail!("Unauthenticated connection attempt from {}", peer_addr);
        }
    };
    if let Some(digest) = &peer_constitution_digest {
        debug!(
            "OFP: peer {} advertised constitution_digest={}",
            peer_node_id, digest
        );
    }
    let local_constitution_digest = crate::constitution_digest::constitution_digest();
    let local_constitution_profile = crate::constitution_digest::canonical_constitution_profile();
    if let Err(e) = evaluate_constitution_compatibility(
        &config.federation_constitution,
        local_constitution_digest.as_ref(),
        peer_constitution_digest.as_deref(),
        &local_constitution_profile,
        peer_constitution_profile.as_ref(),
    ) {
        emit_federation_constitution_event(
            gateway_store.clone(),
            &peer_node_id,
            peer_addr,
            local_constitution_digest.as_ref(),
            peer_constitution_digest.as_deref(),
            config.federation_constitution.mode,
            autonoetic_types::causal_chain::EntryStatus::Error,
            Some(e.to_string()),
        );
        let err = WireMessage {
            id: msg.id.clone(),
            signature: None,
            seq_num: None,
            kind: WireMessageKind::Response(WireResponse::Error {
                code: 409,
                message: format!("constitutional_incompatibility: {}", e),
                peer_event_ref: None,
            }),
        };
        write_framed_message(&mut writer, &err).await?;
        anyhow::bail!(
            "Constitution incompatibility for peer {}: {}",
            peer_node_id,
            e
        );
    }
    emit_federation_constitution_event(
        gateway_store.clone(),
        &peer_node_id,
        peer_addr,
        local_constitution_digest.as_ref(),
        peer_constitution_digest.as_deref(),
        config.federation_constitution.mode,
        autonoetic_types::causal_chain::EntryStatus::Success,
        None,
    );

    // 2. Compute agreed extensions
    let mut agreed_extensions = Vec::new();
    if peer_extensions.contains(&"msg_hmac".to_string()) {
        agreed_extensions.push("msg_hmac".to_string());
    }
    if peer_extensions.contains(&"resilience".to_string()) {
        agreed_extensions.push("resilience".to_string());
    }
    registry
        .add_peer(PeerEntry {
            node_id: peer_node_id.clone(),
            node_name: peer_node_name,
            address: peer_addr,
            agents: peer_agents,
            state: PeerState::Connected,
            connected_at: chrono::Utc::now(),
            protocol_version: peer_protocol_version,
            negotiated_extensions: agreed_extensions.clone(),
        })
        .await;

    // 3. Send HandshakeAck
    let ack_nonce = uuid::Uuid::new_v4().to_string();
    let ack_auth_data = format!("{}{}", ack_nonce, local_node_id);
    let ack_hmac = hmac_sign(&shared_secret, ack_auth_data.as_bytes());

    let ack = WireMessage {
        id: msg.id,
        signature: None,
        seq_num: None,
        kind: WireMessageKind::Response(WireResponse::HandshakeAck {
            node_id: local_node_id.clone(),
            node_name: local_node_name.clone(),
            protocol_version: PROTOCOL_VERSION,
            agents: vec![], // TODO: populate from Gateway state
            nonce: ack_nonce,
            auth_hmac: ack_hmac,
            constitution_digest: Some(local_constitution_digest.to_string()),
            constitution_profile: Some(local_constitution_profile),
            extensions: if agreed_extensions.is_empty() {
                None
            } else {
                Some(agreed_extensions.clone())
            },
        }),
    };
    write_framed_message(&mut writer, &ack).await?;

    // 4. Enter connection loop
    let use_msg_hmac = agreed_extensions.contains(&"msg_hmac".to_string());
    if use_msg_hmac {
        info!("OFP: msg_hmac extension enabled for {}", peer_node_id);
    }
    let mut expected_inbound_seq: u64 = 1;
    let mut outbound_seq: u64 = 1;

    loop {
        let req = match parse_ofp_response(&mut reader).await {
            Ok(m) => m,
            Err(e) => {
                debug!("OFP peer {} disconnected: {}", peer_node_id, e);
                break;
            }
        };

        if use_msg_hmac {
            if let Err(e) = verify_wire_message(&shared_secret, &req, expected_inbound_seq) {
                let err = WireMessage {
                    id: req.id.clone(),
                    signature: None,
                    seq_num: None,
                    kind: WireMessageKind::Response(WireResponse::Error {
                        code: 403,
                        message: format!("Invalid msg_hmac envelope: {}", e),
                        peer_event_ref: None,
                    }),
                };
                write_framed_message(&mut writer, &err).await?;
                registry.mark_disconnected(&peer_node_id).await;
                anyhow::bail!("Invalid msg_hmac message from {}: {}", peer_node_id, e);
            }
            expected_inbound_seq += 1;
        }

        // Handle Ping, ChainAttestation, AgentMessage...
        match req.kind.clone() {
            WireMessageKind::Request(WireRequest::Ping) => {
                let mut resp = WireMessage {
                    id: req.id.clone(),
                    signature: None,
                    seq_num: None,
                    kind: WireMessageKind::Response(WireResponse::Pong { uptime_secs: 1 }), // TODO: real uptime
                };
                if use_msg_hmac {
                    resp.seq_num = Some(outbound_seq);
                    resp.signature = Some(sign_wire_message(&shared_secret, &resp)?);
                    outbound_seq += 1;
                }
                write_framed_message(&mut writer, &resp).await?;
            }
            WireMessageKind::Request(WireRequest::ChainAttestation {
                attestation,
                public_key_b64,
                request_peer_attestation,
            }) => {
                let mut accepted = true;
                let mut reason: Option<String> = None;
                if attestation.gateway_id != peer_node_id {
                    accepted = false;
                    reason = Some(format!(
                        "attestation gateway_id mismatch: expected {}, got {}",
                        peer_node_id, attestation.gateway_id
                    ));
                }
                if accepted {
                    if let Err(e) = verify_chain_attestation(&attestation, &public_key_b64) {
                        accepted = false;
                        reason = Some(e.to_string());
                    }
                }
                let (peer_attestation, peer_public_key_b64) = if accepted
                    && request_peer_attestation
                {
                    match compose_local_chain_attestation(
                        &local_node_id,
                        &gateway_dir,
                        gateway_store.clone(),
                    ) {
                        Ok((attestation, public_key_b64)) => {
                            (Some(attestation), Some(public_key_b64))
                        }
                        Err(e) => {
                            accepted = false;
                            reason = Some(format!("cannot compose local chain attestation: {}", e));
                            (None, None)
                        }
                    }
                } else {
                    (None, None)
                };
                let mut resp = WireMessage {
                    id: req.id.clone(),
                    signature: None,
                    seq_num: None,
                    kind: WireMessageKind::Response(WireResponse::ChainAttestationAck {
                        accepted,
                        reason,
                        peer_attestation,
                        peer_public_key_b64,
                    }),
                };
                if use_msg_hmac {
                    resp.seq_num = Some(outbound_seq);
                    resp.signature = Some(sign_wire_message(&shared_secret, &resp)?);
                    outbound_seq += 1;
                }
                write_framed_message(&mut writer, &resp).await?;
            }
            WireMessageKind::Request(WireRequest::AgentMessage {
                agent,
                message,
                sender,
                peer_event_ref,
                egress_label,
                withheld_indication: _,
            }) => {
                let session_id = uuid::Uuid::new_v4().to_string();
                let inbound_label = crate::runtime::egress_labeler::parse_ofp_inbound_egress_label(
                    egress_label.as_ref(),
                );
                if let Some(ref store) = gateway_store {
                    // Same mechanism the ingest path uses (`restrict_`, not
                    // `set_`), so inbound taint accumulates monotonically by one
                    // rule rather than two. `session_id` is a fresh uuid here so
                    // the two are equivalent today; keeping them the same means
                    // that stays true if this ever seeds an existing session.
                    // An unrestricted peer label stores nothing (absence ⇒
                    // unrestricted), matching the `spawn_metadata` guard below.
                    if let Err(e) = store.restrict_session_egress_taint(&session_id, &inbound_label)
                    {
                        tracing::warn!(
                            target: "egress",
                            error = %e,
                            session_id = %session_id,
                            "failed to seed OFP inbound session egress taint before spawn"
                        );
                    }
                }
                let spawn_metadata = if inbound_label.is_unrestricted() {
                    None
                } else {
                    Some(serde_json::json!({
                        "ofp_inbound_egress_label": inbound_label,
                    }))
                };
                let mut resp = match sender.as_deref() {
                    Some(sender_agent)
                        if registry.peer_hosts_agent(&peer_node_id, sender_agent).await =>
                    {
                        match router
                            .spawn_agent_once(
                                &agent,
                                &message,
                                &session_id,
                                None,
                                true,
                                None,
                                spawn_metadata.as_ref(),
                            )
                            .await
                        {
                            Ok(result) => {
                                let text = result.assistant_reply.unwrap_or_default();
                                let suspended = result.suspended_for_approval.is_some()
                                    || result.suspended_for_user_input
                                    || result.suspended_for_child_wait;
                                let suspension_kind = if result.suspended_for_approval.is_some() {
                                    Some("approval".to_string())
                                } else if result.suspended_for_user_input {
                                    Some("user_input".to_string())
                                } else if result.suspended_for_child_wait {
                                    Some("child_wait".to_string())
                                } else {
                                    None
                                };
                                let local_peer_event_ref = emit_federation_message_event(
                                    &gateway_dir,
                                    gateway_store.clone(),
                                    &local_node_id,
                                    &peer_node_id,
                                    "agent_message_inbound",
                                    autonoetic_types::causal_chain::EntryStatus::Success,
                                    Some(sender_agent),
                                    Some(&agent),
                                    &message,
                                    peer_event_ref.as_ref(),
                                );
                                WireMessage {
                                    id: req.id.clone(),
                                    signature: None,
                                    seq_num: None,
                                    kind: WireMessageKind::Response(WireResponse::AgentResponse {
                                        text,
                                        peer_event_ref: Some(local_peer_event_ref),
                                        suspended: Some(suspended),
                                        suspension_kind,
                                    }),
                                }
                            }
                            Err(e) => WireMessage {
                                id: req.id.clone(),
                                signature: None,
                                seq_num: None,
                                kind: WireMessageKind::Response(WireResponse::Error {
                                    code: 500,
                                    message: format!("Agent spawn failed: {}", e),
                                    peer_event_ref: Some(emit_federation_message_event(
                                        &gateway_dir,
                                        gateway_store.clone(),
                                        &local_node_id,
                                        &peer_node_id,
                                        "agent_message_inbound",
                                        autonoetic_types::causal_chain::EntryStatus::Error,
                                        Some(sender_agent),
                                        Some(&agent),
                                        &message,
                                        peer_event_ref.as_ref(),
                                    )),
                                }),
                            },
                        }
                    }
                    Some(sender_agent) => WireMessage {
                        id: req.id.clone(),
                        signature: None,
                        seq_num: None,
                        kind: WireMessageKind::Response(WireResponse::Error {
                            code: 403,
                            message: format!(
                                "Sender '{}' is not advertised by authenticated peer '{}'",
                                sender_agent, peer_node_id
                            ),
                            peer_event_ref: Some(emit_federation_message_event(
                                &gateway_dir,
                                gateway_store.clone(),
                                &local_node_id,
                                &peer_node_id,
                                "agent_message_inbound",
                                autonoetic_types::causal_chain::EntryStatus::Denied,
                                Some(sender_agent),
                                Some(&agent),
                                &message,
                                peer_event_ref.as_ref(),
                            )),
                        }),
                    },
                    None => WireMessage {
                        id: req.id.clone(),
                        signature: None,
                        seq_num: None,
                        kind: WireMessageKind::Response(WireResponse::Error {
                            code: 400,
                            message: "AgentMessage sender is required for federated delivery"
                                .into(),
                            peer_event_ref: Some(emit_federation_message_event(
                                &gateway_dir,
                                gateway_store.clone(),
                                &local_node_id,
                                &peer_node_id,
                                "agent_message_inbound",
                                autonoetic_types::causal_chain::EntryStatus::Denied,
                                None,
                                Some(&agent),
                                &message,
                                peer_event_ref.as_ref(),
                            )),
                        }),
                    },
                };

                if use_msg_hmac {
                    resp.seq_num = Some(outbound_seq);
                    resp.signature = Some(sign_wire_message(&shared_secret, &resp)?);
                    outbound_seq += 1;
                }
                write_framed_message(&mut writer, &resp).await?;
            }
            // For now, return Error on everything else
            WireMessageKind::Request(_) => {
                let mut resp = WireMessage {
                    id: req.id.clone(),
                    signature: None,
                    seq_num: None,
                    kind: WireMessageKind::Response(WireResponse::Error {
                        code: 501,
                        message: "Not Implemented".into(),
                        peer_event_ref: None,
                    }),
                };
                if use_msg_hmac {
                    resp.seq_num = Some(outbound_seq);
                    resp.signature = Some(sign_wire_message(&shared_secret, &resp)?);
                    outbound_seq += 1;
                }
                write_framed_message(&mut writer, &resp).await?;
            }
            _ => {}
        }
    }
    registry.mark_disconnected(&peer_node_id).await;

    Ok(())
}

/// Read exactly 4 bytes length, then that many bytes of JSON payload.
pub async fn read_framed_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> anyhow::Result<WireMessage> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;

    let len = decode_length(&header);
    if len > MAX_MESSAGE_SIZE {
        anyhow::bail!(
            "Message too large: {} exceeds limit {}",
            len,
            MAX_MESSAGE_SIZE
        );
    }

    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;

    Ok(decode_message(&body)?)
}

/// Backward-compatible alias for older call sites.
pub async fn parse_ofp_response<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> anyhow::Result<WireMessage> {
    read_framed_message(reader).await
}

/// Encode JSON payload, prepend 4-byte length, and write to socket.
pub async fn write_framed_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &WireMessage,
) -> anyhow::Result<()> {
    let bytes = encode_message(msg)?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn base_profile() -> ConstitutionProfile {
        ConstitutionProfile {
            rules_enforcement: BTreeMap::from([(
                "P-1.1".to_string(),
                "tool_call_processor".to_string(),
            )]),
            rights_enforcement: BTreeMap::from([(
                "Ri-0.10".to_string(),
                "constitution_read".to_string(),
            )]),
        }
    }

    #[test]
    fn constitution_compat_exact_rejects_mismatch() {
        let config = FederationConstitutionConfig {
            mode: FederationConstitutionMode::Exact,
            known_compatible_digests: vec![],
            allow_missing_peer_digest: false,
        };
        let local_profile = base_profile();
        let peer_profile = base_profile();
        let err = evaluate_constitution_compatibility(
            &config,
            "local-digest",
            Some("peer-digest"),
            &local_profile,
            Some(&peer_profile),
        )
        .expect_err("mismatch must fail in exact mode");
        assert!(err.to_string().contains("digest mismatch"));
    }

    #[test]
    fn constitution_compat_known_mode_accepts_allowlist() {
        let config = FederationConstitutionConfig {
            mode: FederationConstitutionMode::KnownCompatible,
            known_compatible_digests: vec!["peer-digest".to_string()],
            allow_missing_peer_digest: false,
        };
        let local_profile = base_profile();
        let peer_profile = base_profile();
        evaluate_constitution_compatibility(
            &config,
            "local-digest",
            Some("peer-digest"),
            &local_profile,
            Some(&peer_profile),
        )
        .expect("known-compatible digest should pass");
    }

    #[test]
    fn constitution_compat_missing_digest_respects_policy() {
        let strict_config = FederationConstitutionConfig {
            mode: FederationConstitutionMode::Exact,
            known_compatible_digests: vec![],
            allow_missing_peer_digest: false,
        };
        let permissive_config = FederationConstitutionConfig {
            mode: FederationConstitutionMode::Exact,
            known_compatible_digests: vec![],
            allow_missing_peer_digest: true,
        };
        let local_profile = base_profile();
        let peer_profile = base_profile();

        evaluate_constitution_compatibility(
            &strict_config,
            "local-digest",
            None,
            &local_profile,
            Some(&peer_profile),
        )
        .expect_err("strict mode must reject missing digest");
        evaluate_constitution_compatibility(
            &permissive_config,
            "local-digest",
            None,
            &local_profile,
            Some(&peer_profile),
        )
        .expect("permissive mode should accept missing digest");
    }

    #[test]
    fn constitution_compat_superset_requires_peer_profile() {
        let config = FederationConstitutionConfig {
            mode: FederationConstitutionMode::Superset,
            known_compatible_digests: vec![],
            allow_missing_peer_digest: false,
        };
        let local_profile = base_profile();
        let err = evaluate_constitution_compatibility(
            &config,
            "local-digest",
            Some("peer-digest"),
            &local_profile,
            None,
        )
        .expect_err("superset mode requires profile exchange");
        assert!(err.to_string().contains("constitution_profile"));
    }

    #[test]
    fn constitution_compat_superset_accepts_profile_superset() {
        let config = FederationConstitutionConfig {
            mode: FederationConstitutionMode::Superset,
            known_compatible_digests: vec![],
            allow_missing_peer_digest: false,
        };
        let local_profile = base_profile();
        let mut peer_profile = base_profile();
        peer_profile.rules_enforcement.insert(
            "P-9.9".to_string(),
            "extra_enforcement_reference".to_string(),
        );
        evaluate_constitution_compatibility(
            &config,
            "local-digest",
            Some("peer-digest"),
            &local_profile,
            Some(&peer_profile),
        )
        .expect("superset profile should be accepted");
    }
}
