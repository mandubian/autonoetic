use autonoetic_gateway::router::JsonRpcRouter;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::server::ofp::{
    compose_chain_attestation, hmac_sign, read_framed_message, start_ofp_server,
    verify_chain_attestation, write_framed_message,
};
use autonoetic_gateway::server::registry::PeerRegistry;
use autonoetic_ofp::wire::{
    ConstitutionProfile, PeerEventRef, RemoteAgentInfo, WireMessage, WireMessageKind, WireRequest,
    WireResponse, PROTOCOL_VERSION,
};
use autonoetic_types::config::{
    FederationConstitutionConfig, FederationConstitutionMode, GatewayConfig,
};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::Duration;

fn canonical_profile() -> ConstitutionProfile {
    ConstitutionProfile {
        rules_enforcement:
            autonoetic_gateway::constitution_digest::canonical_rule_enforcement_table(),
        rights_enforcement:
            autonoetic_gateway::constitution_digest::canonical_right_enforcement_table(),
    }
}

#[tokio::test]
async fn round_trip_peer_refs_and_chain_attestation_tamper_rejection() {
    let reg = PeerRegistry::new();
    let shared_secret = "federation-causal-continuity-secret".to_string();
    let store_dir = TempDir::new().unwrap();
    let gateway_store = Arc::new(GatewayStore::open(store_dir.path()).unwrap());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();
    drop(listener);

    let ofp_config = GatewayConfig {
        federation_constitution: FederationConstitutionConfig {
            mode: FederationConstitutionMode::Exact,
            known_compatible_digests: vec![],
            allow_missing_peer_digest: false,
        },
        ..GatewayConfig::default()
    };
    let router = Arc::new(JsonRpcRouter::new(
        GatewayConfig::default(),
        Some(gateway_store.clone()),
    ));
    let server = tokio::spawn(start_ofp_server(
        server_addr,
        "server-node".to_string(),
        "causal-continuity-server".to_string(),
        Arc::new(ofp_config),
        shared_secret.clone(),
        reg,
        router,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();

    let nonce = "continuity-client-nonce".to_string();
    let client_node_id = "continuity-client-node".to_string();
    let auth_data = format!("{}{}", nonce, client_node_id);
    let auth_hmac = hmac_sign(&shared_secret, auth_data.as_bytes());
    let source_agent_id = "remote_sender_agent".to_string();
    let handshake = WireMessage {
        id: "continuity-handshake".to_string(),
        signature: None,
        seq_num: None,
        kind: WireMessageKind::Request(WireRequest::Handshake {
            node_id: client_node_id.clone(),
            node_name: "continuity-client".to_string(),
            protocol_version: PROTOCOL_VERSION,
            agents: vec![RemoteAgentInfo {
                id: source_agent_id.clone(),
                name: source_agent_id.clone(),
                description: "sender identity for continuity test".to_string(),
                tags: vec!["test".to_string()],
                tools: vec![],
                state: "active".to_string(),
            }],
            nonce,
            auth_hmac,
            constitution_digest: Some(
                autonoetic_gateway::constitution_digest::constitution_digest().to_string(),
            ),
            constitution_profile: Some(canonical_profile()),
            extensions: None,
        }),
    };
    write_framed_message(&mut writer, &handshake).await.unwrap();

    let ack = read_framed_message(&mut reader).await.unwrap();
    match ack.kind {
        WireMessageKind::Response(WireResponse::HandshakeAck { .. }) => {}
        other => panic!("expected HandshakeAck, got {:?}", other),
    }

    // 1) Valid attestation exchange: peer attestation must verify.
    let client_key_dir = TempDir::new().unwrap();
    let (attestation, public_key_b64) = compose_chain_attestation(
        &client_node_id,
        client_key_dir.path(),
        "client-tip-event".to_string(),
        "a".repeat(64),
    )
    .unwrap();
    let attestation_req = WireMessage {
        id: "continuity-attestation".to_string(),
        signature: None,
        seq_num: None,
        kind: WireMessageKind::Request(WireRequest::ChainAttestation {
            attestation: attestation.clone(),
            public_key_b64,
            request_peer_attestation: true,
        }),
    };
    write_framed_message(&mut writer, &attestation_req)
        .await
        .unwrap();
    let attestation_resp = read_framed_message(&mut reader).await.unwrap();
    match attestation_resp.kind {
        WireMessageKind::Response(WireResponse::ChainAttestationAck {
            accepted,
            reason,
            peer_attestation,
            peer_public_key_b64,
        }) => {
            assert!(
                accepted,
                "expected accepted chain attestation, got reason: {:?}",
                reason
            );
            let peer_attestation = peer_attestation.expect("peer attestation should be returned");
            let peer_public_key_b64 = peer_public_key_b64.expect("peer key should be returned");
            verify_chain_attestation(&peer_attestation, &peer_public_key_b64).unwrap();
        }
        other => panic!("expected ChainAttestationAck, got {:?}", other),
    }

    // 2) AgentMessage round-trip: request carries peer ref, response carries peer ref.
    let outbound_ref = PeerEventRef {
        gateway_id: client_node_id.clone(),
        event_id: "client-msg-evt-1".to_string(),
        entry_hash: "b".repeat(64),
    };
    let agent_msg = WireMessage {
        id: "continuity-agent-message".to_string(),
        signature: None,
        seq_num: None,
        kind: WireMessageKind::Request(WireRequest::AgentMessage {
            agent: "missing_target_agent".to_string(),
            message: "hello federation continuity".to_string(),
            sender: Some(source_agent_id),
            peer_event_ref: Some(outbound_ref.clone()),
        }),
    };
    write_framed_message(&mut writer, &agent_msg).await.unwrap();
    let msg_resp = read_framed_message(&mut reader).await.unwrap();
    let server_ref = match msg_resp.kind {
        WireMessageKind::Response(WireResponse::AgentResponse { peer_event_ref, .. }) => {
            peer_event_ref.expect("agent response must include peer_event_ref")
        }
        WireMessageKind::Response(WireResponse::Error {
            code,
            peer_event_ref,
            ..
        }) => {
            assert!(
                code >= 400,
                "expected an error status code for missing target"
            );
            peer_event_ref.expect("error response must include peer_event_ref")
        }
        other => panic!("expected AgentResponse/Error, got {:?}", other),
    };
    assert_eq!(server_ref.gateway_id, "server-node");
    assert!(!server_ref.event_id.is_empty());
    assert_eq!(server_ref.entry_hash.len(), 64);

    let events = gateway_store
        .search_causal_events(Some("system"), Some("gateway"), 200)
        .unwrap();
    let inbound = events
        .into_iter()
        .find(|event| event.category == "federation" && event.action == "agent_message_inbound")
        .expect("expected inbound federation message causal event");
    let inbound_payload_raw = inbound
        .payload
        .expect("inbound federation payload should be present");
    let inbound_payload: serde_json::Value = serde_json::from_str(&inbound_payload_raw).unwrap();
    assert_eq!(
        inbound_payload["peer_event_ref"]["gateway_id"],
        outbound_ref.gateway_id
    );
    assert_eq!(
        inbound_payload["peer_event_ref"]["event_id"],
        outbound_ref.event_id
    );
    assert_eq!(inbound_payload["entry_hash"], server_ref.entry_hash);

    // 3) Tamper test: signature corruption must be rejected.
    let mut tampered = attestation;
    tampered.signature_b64 = "AA".repeat(64);
    let tampered_req = WireMessage {
        id: "continuity-attestation-tampered".to_string(),
        signature: None,
        seq_num: None,
        kind: WireMessageKind::Request(WireRequest::ChainAttestation {
            attestation: tampered,
            public_key_b64: compose_chain_attestation(
                &client_node_id,
                client_key_dir.path(),
                "unused".to_string(),
                "c".repeat(64),
            )
            .unwrap()
            .1,
            request_peer_attestation: false,
        }),
    };
    write_framed_message(&mut writer, &tampered_req)
        .await
        .unwrap();
    let tampered_resp = read_framed_message(&mut reader).await.unwrap();
    match tampered_resp.kind {
        WireMessageKind::Response(WireResponse::ChainAttestationAck {
            accepted, reason, ..
        }) => {
            assert!(!accepted, "tampered attestation should be rejected");
            let reason = reason.expect("rejection reason should be provided");
            assert!(
                reason.contains("signature"),
                "expected signature verification reason, got: {}",
                reason
            );
        }
        other => panic!(
            "expected ChainAttestationAck for tampered request, got {:?}",
            other
        ),
    }

    server.abort();
}
