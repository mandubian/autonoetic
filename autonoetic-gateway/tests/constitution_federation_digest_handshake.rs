use autonoetic_gateway::router::JsonRpcRouter;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::server::ofp::{
    hmac_sign, read_framed_message, start_ofp_server, write_framed_message,
};
use autonoetic_gateway::server::registry::PeerRegistry;
use autonoetic_ofp::wire::{ConstitutionProfile, WireMessage, WireMessageKind, WireRequest, WireResponse, PROTOCOL_VERSION};
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::{
    FederationConstitutionConfig, FederationConstitutionMode, GatewayConfig,
};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::Duration;

fn canonical_profile() -> ConstitutionProfile {
    ConstitutionProfile {
        rules_enforcement: autonoetic_gateway::constitution_digest::canonical_rule_enforcement_table(
        ),
        rights_enforcement:
            autonoetic_gateway::constitution_digest::canonical_right_enforcement_table(),
    }
}

#[tokio::test]
async fn matching_digest_and_profile_are_accepted_and_audited() {
    let reg = PeerRegistry::new();
    let shared_secret = "constitution-e2e-match-secret".to_string();
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
        "constitution-e2e-server".to_string(),
        Arc::new(ofp_config),
        shared_secret.clone(),
        reg,
        router,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let nonce = "match-client-nonce".to_string();
    let node_id = "match-client-node".to_string();
    let auth_data = format!("{}{}", nonce, node_id);
    let auth_hmac = hmac_sign(&shared_secret, auth_data.as_bytes());
    let handshake = WireMessage {
        id: "constitution-match".to_string(),
        signature: None,
        seq_num: None,
        kind: WireMessageKind::Request(WireRequest::Handshake {
            node_id,
            node_name: "match-client".to_string(),
            protocol_version: PROTOCOL_VERSION,
            agents: vec![],
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

    let response = read_framed_message(&mut reader).await.unwrap();
    match response.kind {
        WireMessageKind::Response(WireResponse::HandshakeAck { .. }) => {}
        other => panic!("expected HandshakeAck, got {:?}", other),
    }

    let events = gateway_store
        .search_causal_events(Some("system"), Some("gateway"), 50)
        .unwrap();
    let constitution_event = events
        .into_iter()
        .find(|event| event.category == "federation" && event.action == "constitution_check")
        .expect("expected federation constitution check event");
    assert_eq!(constitution_event.status, EntryStatus::Success.to_string());
    let payload_raw = constitution_event
        .payload
        .expect("constitution event payload should exist");
    let payload: serde_json::Value = serde_json::from_str(&payload_raw).unwrap();
    assert_eq!(
        payload["local_constitution_digest"],
        autonoetic_gateway::constitution_digest::constitution_digest()
    );
    assert_eq!(
        payload["peer_constitution_digest"],
        autonoetic_gateway::constitution_digest::constitution_digest()
    );

    server.abort();
}

#[tokio::test]
async fn mismatched_digest_is_rejected_and_records_both_digests() {
    let reg = PeerRegistry::new();
    let shared_secret = "constitution-e2e-mismatch-secret".to_string();
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
        "constitution-e2e-server".to_string(),
        Arc::new(ofp_config),
        shared_secret.clone(),
        reg,
        router,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let stream = tokio::net::TcpStream::connect(server_addr).await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let nonce = "mismatch-client-nonce".to_string();
    let node_id = "mismatch-client-node".to_string();
    let auth_data = format!("{}{}", nonce, node_id);
    let auth_hmac = hmac_sign(&shared_secret, auth_data.as_bytes());
    let handshake = WireMessage {
        id: "constitution-mismatch".to_string(),
        signature: None,
        seq_num: None,
        kind: WireMessageKind::Request(WireRequest::Handshake {
            node_id,
            node_name: "mismatch-client".to_string(),
            protocol_version: PROTOCOL_VERSION,
            agents: vec![],
            nonce,
            auth_hmac,
            constitution_digest: Some("mismatched-digest".to_string()),
            constitution_profile: Some(canonical_profile()),
            extensions: None,
        }),
    };
    write_framed_message(&mut writer, &handshake).await.unwrap();

    let response = read_framed_message(&mut reader).await.unwrap();
    match response.kind {
        WireMessageKind::Response(WireResponse::Error { code, message }) => {
            assert_eq!(code, 409);
            assert!(message.contains("constitutional_incompatibility"));
        }
        other => panic!("expected constitutional incompatibility error, got {:?}", other),
    }

    let events = gateway_store
        .search_causal_events(Some("system"), Some("gateway"), 50)
        .unwrap();
    let constitution_event = events
        .into_iter()
        .find(|event| event.category == "federation" && event.action == "constitution_check")
        .expect("expected federation constitution check event");
    assert_eq!(constitution_event.status, EntryStatus::Error.to_string());
    let payload_raw = constitution_event
        .payload
        .expect("constitution event payload should exist");
    let payload: serde_json::Value = serde_json::from_str(&payload_raw).unwrap();
    assert_eq!(
        payload["local_constitution_digest"],
        autonoetic_gateway::constitution_digest::constitution_digest()
    );
    assert_eq!(payload["peer_constitution_digest"], "mismatched-digest");

    server.abort();
}
