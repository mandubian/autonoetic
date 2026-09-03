//! Causal chain witness contract (#1278).
//!
//! The JSONL causal chain is the *witness*, not a second store: entries carry
//! `payload_hash` + `payload_ref`, the content lives once in the
//! content-addressed `payloads/` directory, and `enforced_rules` is bound
//! into the entry hash so enforcement attribution (I-6) is tamper-evident.
//! Legacy (v1) entries keep verifying under their original field set.

use crate::support::causal_entry_payload;
use autonoetic_gateway::causal_chain::{
    verify_chain, verify_entry_hash, CausalLogger,
};
use autonoetic_gateway::log_redaction::RedactedPayload;
use autonoetic_types::causal_chain::{default_enforced_rules, EntryStatus};
use tempfile::tempdir;

#[test]
fn witness_entries_carry_fingerprints_not_payload() {
    let temp = tempdir().unwrap();
    let history_dir = temp.path().join("history");
    std::fs::create_dir_all(&history_dir).unwrap();
    let path = history_dir.join("causal_chain.jsonl");

    let logger = CausalLogger::new(&path).unwrap();
    logger
        .log(
            "coder.default",
            "session-1",
            Some("turn-1"),
            1,
            "tool",
            "sandbox_exec",
            EntryStatus::Success,
            Some("cmd:ls"),
            &default_enforced_rules(),
            Some(RedactedPayload::from_redacted(serde_json::json!({
                "command": "ls -la /workspace",
            }))),
        )
        .unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    let line = serde_json::from_str::<serde_json::Value>(raw.trim()).unwrap();
    assert!(line.get("payload").is_none(), "payload leaked into witness");
    assert_eq!(line["v"], 2, "lean witness format version");
    assert_eq!(line["payload_ref"], line["payload_hash"], "CAS key is the payload hash");
    assert_eq!(line["target"], "cmd:ls");
    assert_eq!(line["enforced_rules"], serde_json::json!(["I-6"]));

    // The entry parses, verifies, and its payload resolves from the CAS with
    // hash checking.
    let entry = &CausalLogger::read_entries(&path).unwrap()[0];
    assert!(verify_entry_hash(entry).unwrap());
    let resolved = causal_entry_payload(&path, entry)
        .unwrap()
        .expect("payload should resolve");
    assert_eq!(resolved["command"], "ls -la /workspace");

    // Payload bytes exist exactly once — in the CAS, not the witness file.
    let cas_dir = history_dir.join("payloads");
    let cas_files: Vec<_> = std::fs::read_dir(&cas_dir).unwrap().collect();
    assert_eq!(cas_files.len(), 1, "exactly one content-addressed copy");
}

#[test]
fn enforced_rules_are_bound_into_the_entry_hash() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("causal_chain.jsonl");

    let logger = CausalLogger::new(&path).unwrap();
    let rules = vec!["I-6".to_string(), "P-7.17".to_string()];
    logger
        .log(
            "gateway",
            "system",
            None,
            1,
            "scheduler",
            "approval_requested",
            EntryStatus::Success,
            None,
            &rules,
            None,
        )
        .unwrap();

    let entry = &CausalLogger::read_entries(&path).unwrap()[0];
    assert_eq!(entry.enforced_rules, rules);
    assert!(verify_entry_hash(entry).unwrap(), "honest entry verifies");

    // Rewriting the witnessed attribution must invalidate the entry — that is
    // the I-6 tamper-evidence this change buys.
    let mut forged = entry.clone();
    forged.enforced_rules = default_enforced_rules();
    assert!(
        !verify_entry_hash(&forged).unwrap(),
        "swapping enforced_rules must break the entry hash"
    );
}

#[test]
fn verify_chain_accepts_legacy_v1_entries_before_lean_entries() {
    let temp = tempdir().unwrap();
    let history_dir = temp.path().join("history");
    std::fs::create_dir_all(&history_dir).unwrap();
    let path = history_dir.join("causal_chain.jsonl");

    // Hand-written v1 entry: inline payload, no `v` field, old field set.
    // payload_hash = sha256 of the compact-encoded payload.
    let payload = serde_json::json!({"k": "legacy"});
    let encoded = serde_json::to_string(&payload).unwrap();
    let payload_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(encoded.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let v1_entry = serde_json::json!({
        "timestamp": "2026-01-01T00:00:00+00:00",
        "log_id": "legacy-1",
        "actor_id": "coder.default",
        "session_id": "session-legacy",
        "turn_id": null,
        "event_seq": 1,
        "category": "tool",
        "action": "promotion_record",
        "target": null,
        "status": "SUCCESS",
        "reason": null,
        "payload": payload,
        "payload_hash": payload_hash,
        "prev_hash": "genesis",
        "entry_hash": "",
    });
    let mut v1 = autonoetic_types::causal_chain::CausalChainEntry {
        timestamp: v1_entry["timestamp"].as_str().unwrap().to_string(),
        log_id: "legacy-1".to_string(),
        actor_id: "coder.default".to_string(),
        session_id: "session-legacy".to_string(),
        turn_id: None,
        event_seq: 1,
        category: "tool".to_string(),
        action: "promotion_record".to_string(),
        target: None,
        status: EntryStatus::Success,
        reason: None,
        payload: Some(payload),
        payload_hash: Some(payload_hash),
        payload_ref: None,
        enforced_rules: Vec::new(),
        format_version: autonoetic_types::causal_chain::WITNESS_FORMAT_VERSION_V1,
        prev_hash: "genesis".to_string(),
        entry_hash: String::new(),
    };
    v1.entry_hash = autonoetic_gateway::causal_chain::compute_entry_hash(
        &v1.timestamp,
        &v1.log_id,
        &v1.actor_id,
        &v1.session_id,
        v1.turn_id.as_deref(),
        v1.event_seq,
        &v1.category,
        &v1.action,
        &v1.status,
        v1.payload_hash.as_deref(),
        &v1.prev_hash,
    )
    .unwrap();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", serde_json::to_string(&v1).unwrap()).unwrap();
    drop(file);

    // A live (v2) logger must chain onto the legacy tip, not restart it.
    let logger = CausalLogger::new(&path).unwrap();
    logger
        .log(
            "coder.default",
            "session-2",
            None,
            1,
            "tool",
            "content_write",
            EntryStatus::Success,
            None,
            &default_enforced_rules(),
            Some(RedactedPayload::from_redacted(serde_json::json!({"n": 1}))),
        )
        .unwrap();

    let verification = verify_chain(&history_dir).unwrap();
    assert!(
        verification.is_intact(),
        "mixed v1+v2 witness must verify: {:?}",
        verification.reason
    );
    assert_eq!(verification.total_entries, 2);
    assert_eq!(verification.verified_entries, 2);

    // The legacy entry's payload still resolves — inline, no CAS needed.
    let entries = CausalLogger::read_entries(&path).unwrap();
    assert!(entries[0].payload.is_some(), "v1 keeps its inline payload");
    assert!(entries[1].is_lean_witness());
}

#[test]
fn federation_events_are_witnessed_and_attest_the_witness_tip() {
    use autonoetic_gateway::server::ofp::{
        compose_local_chain_attestation, emit_federation_message_event,
        verify_chain_attestation,
    };

    let temp = tempdir().unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let local_ref = emit_federation_message_event(
        &gateway_dir,
        None, // no store: the witness write is what this test pins
        "node-a",
        "node-b",
        "agent_message_outbound",
        EntryStatus::Success,
        Some("planner.default"),
        Some("coder.default"),
        "hello peer",
        None,
    );

    // The federation event reached the witness as a lean entry. The witness
    // `entry_hash` is the chain hash; the continuity hash (`local_ref`)
    // lives inside the content-addressed payload.
    let witness_path = gateway_dir.join("history").join("causal_chain.jsonl");
    let entries = CausalLogger::read_entries(&witness_path).unwrap();
    assert_eq!(entries.len(), 1, "federation event witnessed");
    let entry = &entries[0];
    assert_eq!(entry.category, "federation");
    assert_eq!(entry.target.as_deref(), Some("node-b"));
    assert!(entry.is_lean_witness());
    assert!(verify_entry_hash(entry).unwrap());
    let resolved = causal_entry_payload(&witness_path, entry)
        .unwrap()
        .expect("federation payload should resolve and verify");
    assert_eq!(resolved["entry_hash"], local_ref.entry_hash);

    // A second message chains onto the first (prev-linkage) and advances the tip.
    emit_federation_message_event(
        &gateway_dir,
        None,
        "node-a",
        "node-c",
        "agent_message_outbound",
        EntryStatus::Success,
        Some("planner.default"),
        Some("coder.default"),
        "hello again",
        None,
    );
    let verification = verify_chain(&gateway_dir.join("history")).unwrap();
    assert!(verification.is_intact(), "{:?}", verification.reason);

    // The composed attestation signs the *witness* tip, verifiable with the
    // published key.
    let (attestation, public_key_b64) =
        compose_local_chain_attestation("node-a", &gateway_dir, None).unwrap();
    let entries = CausalLogger::read_entries(&witness_path).unwrap();
    let tip = entries.last().unwrap();
    assert_eq!(attestation.chain_prefix_hash, tip.entry_hash);
    assert_eq!(attestation.event_id, tip.log_id);
    verify_chain_attestation(&attestation, &public_key_b64)
        .expect("attestation must verify against its own key");
}
