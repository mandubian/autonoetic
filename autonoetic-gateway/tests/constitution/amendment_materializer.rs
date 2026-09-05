//! #810 — the amendment materializer closes the proposal loop's last mile:
//! an approved `cprop-` proposal mechanically becomes a candidate
//! constitution version (amended markdown + unsigned lock + provenance),
//! with the operator still sovereign at the signature.
//!
//! This suite pins the *contract* between the store and the materializer:
//! the approved-but-unmaterialized queue, the stamp-once guarantee, and the
//! candidate artifacts the CLI flow writes. Pure text-application semantics
//! live next to the implementation
//! (`src/constitution_materializer.rs::tests`).

use autonoetic_gateway::constitution_digest::compute_constitution_digest;
use autonoetic_gateway::constitution_materializer::{
    apply_proposals_to_text, materialize_candidate_version, MaterializableProposal,
};
use autonoetic_gateway::scheduler::gateway_store::constitutional_proposals::ConstitutionalProposal;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

const BASE: &str = "\
# Constitution

## 8. Causal chain

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-8.1 | Every causal event is append-only. | ARCHITECTURE.md | `causal_chain.rs` | ENFORCED | enforcer · none · preventive |
| P-8.2 | Events are never rewritten. | ARCHITECTURE.md | `causal_chain.rs` | ENFORCED | enforcer · none · detective |
";

fn write_base(versions: &std::path::Path, version: &str) {
    let dir = versions.join(version);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("constitution.md"), BASE).unwrap();
    let (digest, rules, rights) = compute_constitution_digest(BASE);
    let lock = serde_json::json!({
        "format_version": 1,
        "constitution_id": "autonoetic-gateway-constitution",
        "constitution_version": version,
        "constitution_source": format!("docs/constitution/versions/{version}/constitution.md"),
        "constitution_digest": digest,
        "rule_enforcement_count": rules,
        "right_enforcement_count": rights,
        "canonicalization": {
            "algorithm": "sha256",
            "payload": "json({constitution_text,rights_enforcement,rules_enforcement})",
            "rules_prefix": "P-",
            "rights_prefix": "Ri-"
        }
    });
    std::fs::write(
        dir.join("gateway-constitution.lock.json"),
        serde_json::to_string_pretty(&lock).unwrap(),
    )
    .unwrap();
}

fn proposal(id: &str, created_at: &str, status: &str) -> ConstitutionalProposal {
    ConstitutionalProposal {
        proposal_id: id.to_string(),
        proposer_agent_id: "auditor.default".to_string(),
        proposer_session_id: Some("sess-1".to_string()),
        kind: "modify_rule".to_string(),
        target_id: Some("P-8.1".to_string()),
        proposed_text: Some("Every causal event is append-only and fsync-durable.".to_string()),
        justification: "strengthens P-8.1".to_string(),
        evidence_json: serde_json::json!([]),
        status: status.to_string(),
        operator_decision: None,
        decision_reason: None,
        decided_by: None,
        decided_at: None,
        published_in_release: None,
        created_at: created_at.to_string(),
        sla_breached_at: None,
        materialized_in_version: None,
    }
}

/// The full store→materializer loop: approve, queue, draft, stamp — and the
/// candidate artifacts must be exactly what the signing ceremony expects.
#[test]
fn approved_proposals_flow_from_store_to_candidate_version() {
    let temp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(temp.path()).unwrap();
    let versions = temp.path().join("versions");
    write_base(&versions, "2026.01.01");

    store
        .insert_constitutional_proposal(&proposal("cprop-old", "2025-01-01T00:00:00Z", "approved"))
        .unwrap();
    store
        .insert_constitutional_proposal(&proposal(
            "cprop-new",
            "2025-06-01T00:00:00Z",
            "approved",
        ))
        .unwrap();
    // Noise the queue must exclude.
    store
        .insert_constitutional_proposal(&proposal("cprop-pend", "2025-02-01T00:00:00Z", "pending"))
        .unwrap();

    let queue = store.list_approved_unmaterialized_proposals().unwrap();
    let ids: Vec<&str> = queue.iter().map(|p| p.proposal_id.as_str()).collect();
    assert_eq!(ids, vec!["cprop-old", "cprop-new"], "oldest first, approved only");

    // Adjudicate through the store's decision path, as the RPC does.
    store
        .decide_constitutional_proposal("cprop-old", "approved", "operator", Some("yes"))
        .unwrap();

    let materializable: Vec<MaterializableProposal> =
        store.list_approved_unmaterialized_proposals().unwrap()
            .iter()
            .map(MaterializableProposal::from)
            .collect();
    let report = materialize_candidate_version(
        &versions,
        "2026.01.01",
        "2026.01.02",
        &materializable,
    )
    .unwrap();

    // Candidate text: base row amended, digest reproduces, arity holds.
    let text = std::fs::read_to_string(report.candidate_dir.join("constitution.md")).unwrap();
    assert!(text.contains("fsync-durable"));
    assert!(text.contains("Every causal event is append-only.") == false);
    let (_, rule_count, _) = compute_constitution_digest(&text);
    assert_eq!(rule_count, 2);
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('|') && t.ends_with('|') {
            let first = t[1..].split('|').next().unwrap().trim();
            if first.starts_with("P-") {
                let cells = t[1..t.len() - 1]
                    .split('|')
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .count();
                assert_eq!(cells, 6, "clause row {first} must stay well-formed");
            }
        }
    }

    // Provenance carries the adjudication.
    let provenance: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(report.candidate_dir.join("provenance.json")).unwrap(),
    )
    .unwrap();
    let props = provenance["proposals"].as_array().unwrap();
    assert_eq!(props.len(), 2);
    assert_eq!(props[0]["decision_reason"], "yes");

    // Stamping is transactional with the report's proposal set; the queue
    // drains; a second materialization run has nothing to draft.
    let stamped = store
        .mark_proposals_materialized(&report.proposal_ids, &report.candidate_version)
        .unwrap();
    assert_eq!(stamped.len(), 2);
    assert!(store.list_approved_unmaterialized_proposals().unwrap().is_empty());
    let done = store
        .get_constitutional_proposal("cprop-old")
        .unwrap()
        .unwrap();
    assert_eq!(done.materialized_in_version.as_deref(), Some("2026.01.02"));
}

/// Materialization must not mutate the base pair — the signed bytes of the
/// active version are frozen; a candidate is additive only.
#[test]
fn base_version_bytes_are_never_mutated() {
    let temp = tempfile::tempdir().unwrap();
    let versions = temp.path().join("versions");
    write_base(&versions, "2026.01.01");
    let base_md = versions.join("2026.01.01/constitution.md");
    let base_lock = versions.join("2026.01.01/gateway-constitution.lock.json");
    let md_before = std::fs::read(&base_md).unwrap();
    let lock_before = std::fs::read(&base_lock).unwrap();

    let p = [MaterializableProposal {
        proposal_id: "cprop-x".to_string(),
        proposer_agent_id: "auditor.default".to_string(),
        kind: "modify_rule".to_string(),
        target_id: Some("P-8.1".to_string()),
        proposed_text: Some("Amended.".to_string()),
        justification: "j".to_string(),
        decided_by: Some("operator".to_string()),
        decided_at: None,
        decision_reason: None,
    }];
    materialize_candidate_version(&versions, "2026.01.01", "2026.01.02", &p).unwrap();

    assert_eq!(std::fs::read(&base_md).unwrap(), md_before);
    assert_eq!(std::fs::read(&base_lock).unwrap(), lock_before);
    assert!(!versions.join("2026.01.01/provenance.json").exists());
}

/// An applied DRAFT row is well-formed and carries the exact placeholder
/// vocabulary the operator is expected to replace before signing.
#[test]
fn draft_rows_carry_placeholder_classification_not_invented_law() {
    let p = [MaterializableProposal {
        proposal_id: "cprop-add".to_string(),
        proposer_agent_id: "auditor.default".to_string(),
        kind: "add_rule".to_string(),
        target_id: Some("P-8.3".to_string()),
        proposed_text: Some("New clause.".to_string()),
        justification: "j".to_string(),
        decided_by: Some("operator".to_string()),
        decided_at: None,
        decision_reason: None,
    }];
    let applied = apply_proposals_to_text(BASE, &p).unwrap();
    let row = applied
        .text
        .lines()
        .find(|l| l.trim().starts_with("| P-8.3 "))
        .unwrap()
        .trim();
    assert!(row.contains("| DRAFT |"), "status must be the explicit DRAFT marker");
    assert!(row.contains("TBD"), "relation placeholders must be TBD, not invented");
    assert!(row.contains("Source pending operator classification"));
}
