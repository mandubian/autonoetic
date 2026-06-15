//! Session capability envelope: discovery proposals, locking, and grant materialization (#505).

use anyhow::{anyhow, Result};
use autonoetic_types::background::GrantScope;
use autonoetic_types::background::GrantTarget;
use autonoetic_types::capability::Capability;
use autonoetic_types::session_timeline::TimelineRefs;

use crate::scheduler::gateway_store::session_envelopes::SessionEnvelopeRecord;
use crate::scheduler::gateway_store::GatewayStore;

#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvelopeProposalResult {
    pub envelope_id: i64,
    pub hosts: Vec<String>,
    pub capabilities: Vec<Capability>,
    pub skipped: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EnvelopeLockResult {
    pub envelope_id: i64,
    pub grants_materialized: usize,
    pub locked_at: String,
    pub hosts: Vec<String>,
}

pub fn capabilities_from_hosts(hosts: &[String]) -> Vec<Capability> {
    if hosts.is_empty() {
        return Vec::new();
    }
    vec![Capability::NetworkAccess {
        hosts: hosts.to_vec(),
    }]
}

pub fn concrete_hosts(hosts: &[String]) -> Vec<String> {
    let mut out: Vec<String> = hosts
        .iter()
        .filter(|h| !h.is_empty() && h.as_str() != "*")
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Materialize a single `NetworkAccess` envelope entry into session approval grants.
pub fn materialize_network_grant(
    store: &GatewayStore,
    root_session_id: &str,
    hosts: &[String],
    locked_by: &str,
    source: &str,
    agent_id: Option<&str>,
) -> usize {
    let concrete = concrete_hosts(hosts);
    if concrete.is_empty() {
        return 0;
    }
    // A declared `*.example.com` is a subdomain wildcard, not a literal host:
    // materialize it as `HostSuffix` so it actually matches subdomains rather
    // than becoming a dead `ExactHost("*.example.com")` that never fires.
    // `HostSuffix` stores the canonical bare suffix (the CLI prepends `*.` on
    // display and `matches()` trims it), so strip the leading `*.` here.
    let targets: Vec<GrantTarget> = concrete
        .iter()
        .map(|h| match h.strip_prefix("*.") {
            Some(suffix) => GrantTarget::HostSuffix(suffix.to_string()),
            None => GrantTarget::ExactHost(h.clone()),
        })
        .collect();
    let grant_agent = agent_id.unwrap_or(root_session_id);
    if let Err(e) = store.insert_session_grant(
        root_session_id,
        root_session_id,
        grant_agent,
        &GrantScope::RootSession,
        &targets,
        locked_by,
        &chrono::Utc::now().to_rfc3339(),
        Some(source),
        None,
    ) {
        tracing::warn!(
            target: "session_envelope",
            error = %e,
            root_session_id,
            "network grant materialization failed"
        );
        return 0;
    }
    1
}

/// Dispatch each capability in a locked envelope to the appropriate grant store.
pub fn materialize_envelope(
    store: &GatewayStore,
    root_session_id: &str,
    envelope: &[Capability],
    locked_by: &str,
    source: &str,
) -> usize {
    let mut count = 0;
    for cap in envelope {
        match cap {
            Capability::NetworkAccess { hosts } => {
                count += materialize_network_grant(
                    store,
                    root_session_id,
                    hosts,
                    locked_by,
                    source,
                    None,
                );
            }
            Capability::PromoteWith { .. } => {
                // Stored in session_envelopes only; checked at promotion time (P-2.27).
            }
            _ => {}
        }
    }
    count
}

pub fn locked_network_hosts(store: &GatewayStore, root_session_id: &str) -> Result<Vec<String>> {
    let mut hosts = std::collections::BTreeSet::new();
    for record in store.get_active_envelopes(root_session_id)? {
        if let Capability::NetworkAccess { hosts: decl } = &record.capability {
            for h in decl {
                if !h.is_empty() && h.as_str() != "*" {
                    hosts.insert(h.clone());
                }
            }
        }
    }
    let mut sorted: Vec<String> = hosts.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

pub fn hosts_pending_proposal(store: &GatewayStore, root_session_id: &str) -> Result<Vec<String>> {
    let discovered = store.discover_observed_hosts(root_session_id)?;
    let locked = locked_network_hosts(store, root_session_id)?;
    let locked_set: std::collections::BTreeSet<_> = locked.into_iter().collect();
    Ok(discovered
        .into_iter()
        .filter(|h| !locked_set.contains(h))
        .collect())
}

fn find_matching_pending_envelope_id(
    store: &GatewayStore,
    root_session_id: &str,
    hosts: &[String],
) -> Result<Option<i64>> {
    let proposed = store.get_proposed_envelopes(root_session_id)?;
    Ok(proposed
        .into_iter()
        .find(|p| {
            matches!(
                &p.capability,
                Capability::NetworkAccess { hosts: pending } if pending == hosts
            )
        })
        .map(|p| p.id))
}

/// True when a locked session envelope `PromoteWith` covers `artifact_capabilities`.
pub fn promotion_preauthorized_by_envelope(
    store: &GatewayStore,
    root_session_id: &str,
    agent_id: &str,
    artifact_capabilities: &[Capability],
) -> Result<bool> {
    Ok(find_promote_with_envelope_id(store, root_session_id, agent_id, artifact_capabilities)?.is_some())
}

/// Find the envelope ID of a locked `PromoteWith` that covers `artifact_capabilities`.
/// Checks ALL matching entries (wildcard `agent_id: ""` + specific), returns the first
/// that covers. Used for causal chain cross-referencing (P-2.27 traceability).
pub fn find_promote_with_envelope_id(
    store: &GatewayStore,
    root_session_id: &str,
    agent_id: &str,
    artifact_capabilities: &[Capability],
) -> Result<Option<i64>> {
    for record in store.get_active_envelopes(root_session_id)? {
        if let Capability::PromoteWith {
            agent_id: pw_agent,
            capabilities,
        } = &record.capability
        {
            if (pw_agent.is_empty() || pw_agent == agent_id)
                && autonoetic_types::capability::capability_set_covers(
                    capabilities,
                    artifact_capabilities,
                )
            {
                return Ok(Some(record.id));
            }
        }
    }
    Ok(None)
}

fn promote_with_sets_equivalent(a: &[Capability], b: &[Capability]) -> bool {
    autonoetic_types::capability::capability_set_covers(a, b)
        && autonoetic_types::capability::capability_set_covers(b, a)
}

fn has_matching_pending_promote_with(
    store: &GatewayStore,
    root_session_id: &str,
    agent_id: &str,
    capabilities: &[Capability],
) -> Result<bool> {
    Ok(store.get_proposed_envelopes(root_session_id)?.into_iter().any(|record| {
        matches!(
            &record.capability,
            Capability::PromoteWith { agent_id: pw, capabilities: pending }
                if pw == agent_id && promote_with_sets_equivalent(pending, capabilities)
        )
    }))
}

pub fn propose_promote_with_envelope(
    store: &GatewayStore,
    root_session_id: &str,
    agent_id: &str,
    capabilities: &[Capability],
    source: &str,
    actor_id: &str,
) -> Result<Option<i64>> {
    if capabilities.is_empty() {
        return Ok(None);
    }
    // An empty agent_id is the wildcard ("any agent") form, which is reserved
    // for deliberate operator action — the automated proposal path must always
    // bind the proposal to a concrete agent.
    if agent_id.is_empty() {
        return Err(anyhow!(
            "PromoteWith proposal requires a non-empty agent_id"
        ));
    }
    if capabilities.iter().any(|c| matches!(c, Capability::PromoteWith { .. })) {
        return Err(anyhow!(
            "PromoteWith capabilities cannot contain nested PromoteWith"
        ));
    }
    if promotion_preauthorized_by_envelope(store, root_session_id, agent_id, capabilities)? {
        return Ok(None);
    }
    if has_matching_pending_promote_with(store, root_session_id, agent_id, capabilities)? {
        return Ok(None);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let promote_with = Capability::PromoteWith {
        agent_id: agent_id.to_string(),
        capabilities: capabilities.to_vec(),
    };
    let envelope_id = store.insert_envelope_proposal(
        root_session_id,
        &promote_with,
        source,
        Some(&now),
        None,
        &now,
    )?;
    emit_envelope_proposed_timeline(
        store,
        root_session_id,
        envelope_id,
        &[],
        source,
        actor_id,
        None,
        Some(&promote_with),
    );
    Ok(Some(envelope_id))
}

/// After `artifact_build`: propose observed network hosts and promotion pre-auth.
pub fn propose_envelopes_after_artifact_build(
    store: &GatewayStore,
    gateway_dir: &std::path::Path,
    root_session_id: &str,
    artifact_id: &str,
    kind: &autonoetic_types::artifact::ArtifactKind,
    actor_id: &str,
) -> Result<()> {
    let _ = propose_discovered_envelope(store, root_session_id, "discovered", None, actor_id)?;

    use autonoetic_types::artifact::ArtifactKind;
    if *kind != ArtifactKind::AgentBundle {
        return Ok(());
    }

    let artifact = crate::ArtifactStore::new(gateway_dir)?;
    let files = artifact.resolve_files(artifact_id)?;
    let Some(skill_bytes) = files
        .into_iter()
        .find(|(path, _)| path == "SKILL.md")
        .map(|(_, bytes)| bytes)
    else {
        return Ok(());
    };
    let skill_text = String::from_utf8_lossy(&skill_bytes);
    let (manifest, _) = crate::runtime::parser::SkillParser::parse(&skill_text)
        .map_err(|e| anyhow::anyhow!("artifact SKILL.md parse: {e}"))?;
    if manifest.capabilities.is_empty() {
        return Ok(());
    }

    let _ = propose_promote_with_envelope(
        store,
        root_session_id,
        &manifest.agent.id,
        &manifest.capabilities,
        &format!("artifact:{artifact_id}"),
        actor_id,
    )?;
    Ok(())
}

/// Propose a session envelope after plan approval: declared envelope when present,
/// otherwise discovery from observed hosts.
pub fn propose_plan_envelope_on_approval(
    store: &GatewayStore,
    plan: &autonoetic_types::plan_frame::PlanFrame,
    approver: &str,
) -> Result<Option<EnvelopeProposalResult>> {
    let source = format!("plan:{}", plan.plan_id);
    if !plan.capability_envelope.is_empty() {
        propose_envelope_from_capabilities(
            store,
            &plan.root_session_id,
            &plan.capability_envelope,
            &source,
            Some(&plan.plan_id),
            approver,
        )
    } else {
        propose_discovered_envelope(
            store,
            &plan.root_session_id,
            &source,
            Some(&plan.plan_id),
            approver,
        )
    }
}

/// Propose locking observed-but-unlocked hosts for a root session.
pub fn propose_discovered_envelope(
    store: &GatewayStore,
    root_session_id: &str,
    source: &str,
    plan_id: Option<&str>,
    actor_id: &str,
) -> Result<Option<EnvelopeProposalResult>> {
    let hosts = hosts_pending_proposal(store, root_session_id)?;
    if hosts.is_empty() {
        return Ok(None);
    }
    let capabilities = capabilities_from_hosts(&hosts);
    if let Some(envelope_id) = find_matching_pending_envelope_id(store, root_session_id, &hosts)? {
        return Ok(Some(EnvelopeProposalResult {
            envelope_id,
            hosts,
            capabilities,
            skipped: true,
        }));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let envelope_id = store.insert_envelope_proposal(
        root_session_id,
        &capabilities[0],
        source,
        Some(&now),
        plan_id,
        &now,
    )?;

    emit_envelope_proposed_timeline(store, root_session_id, envelope_id, &hosts, source, actor_id, plan_id, Some(&capabilities[0]));

    Ok(Some(EnvelopeProposalResult {
        envelope_id,
        hosts,
        capabilities,
        skipped: false,
    }))
}

pub fn propose_envelope_from_capabilities(
    store: &GatewayStore,
    root_session_id: &str,
    capabilities: &[Capability],
    source: &str,
    plan_id: Option<&str>,
    actor_id: &str,
) -> Result<Option<EnvelopeProposalResult>> {
    let hosts: Vec<String> = capabilities
        .iter()
        .filter_map(|c| match c {
            Capability::NetworkAccess { hosts } => Some(concrete_hosts(hosts)),
            _ => None,
        })
        .flatten()
        .collect();
    let hosts = concrete_hosts(&hosts);
    if hosts.is_empty() {
        return propose_discovered_envelope(store, root_session_id, source, plan_id, actor_id);
    }
    if let Some(envelope_id) = find_matching_pending_envelope_id(store, root_session_id, &hosts)? {
        return Ok(Some(EnvelopeProposalResult {
            envelope_id,
            hosts,
            capabilities: capabilities.to_vec(),
            skipped: true,
        }));
    }
    let now = chrono::Utc::now().to_rfc3339();
    // Every declared NetworkAccess is merged into one proposal row (union of hosts).
    let network_capability = Capability::NetworkAccess {
        hosts: hosts.clone(),
    };
    let envelope_id = store.insert_envelope_proposal(
        root_session_id,
        &network_capability,
        source,
        Some(&now),
        plan_id,
        &now,
    )?;
    for cap in capabilities {
        if matches!(cap, Capability::NetworkAccess { .. }) {
            continue;
        }
        if let Err(e) = store.insert_envelope_proposal(
            root_session_id,
            cap,
            source,
            Some(&now),
            plan_id,
            &now,
        ) {
            tracing::warn!(
                target: "session_envelope",
                error = %e,
                root_session_id,
                "non-network envelope proposal insert failed"
            );
        }
    }
    emit_envelope_proposed_timeline(store, root_session_id, envelope_id, &hosts, source, actor_id, plan_id, Some(&network_capability));
    Ok(Some(EnvelopeProposalResult {
        envelope_id,
        hosts,
        capabilities: capabilities.to_vec(),
        skipped: false,
    }))
}

pub fn lock_session_envelope(
    store: &GatewayStore,
    envelope_id: i64,
    locked_by: &str,
) -> Result<EnvelopeLockResult> {
    let Some(record) = store.get_envelope_by_id(envelope_id)? else {
        return Err(anyhow!("session envelope {envelope_id} not found"));
    };
    if record.locked_at.is_some() {
        return Err(anyhow!("session envelope {envelope_id} is already locked"));
    }

    let now = chrono::Utc::now().to_rfc3339();
    if !store.lock_envelope(envelope_id, locked_by, &now)? {
        return Err(anyhow!("session envelope {envelope_id} lock failed"));
    }

    let source = format!("session-envelope:{envelope_id}");
    let grants_materialized = materialize_envelope(
        store,
        &record.root_session_id,
        std::slice::from_ref(&record.capability),
        locked_by,
        &source,
    );

    let hosts = match &record.capability {
        Capability::NetworkAccess { hosts } => concrete_hosts(hosts),
        _ => Vec::new(),
    };

    emit_envelope_locked_timeline(
        store,
        &record,
        &hosts,
        locked_by,
        grants_materialized,
    );

    Ok(EnvelopeLockResult {
        envelope_id,
        grants_materialized,
        locked_at: now,
        hosts,
    })
}

/// Revoke a single envelope by ID. If it was locked and materialized as network
/// grants, those grants are surgically revoked too (by source). Emits an
/// `envelope.revoked` timeline event. Returns the envelope record that was
/// revoked (for the response payload), or None if not found.
pub fn revoke_session_envelope(
    store: &GatewayStore,
    envelope_id: i64,
    revoked_by: &str,
) -> Result<Option<SessionEnvelopeRecord>> {
    let Some(record) = store.get_envelope_by_id(envelope_id)? else {
        return Ok(None);
    };

    let was_locked = record.locked_at.is_some();
    let hosts = match &record.capability {
        Capability::NetworkAccess { hosts } => concrete_hosts(hosts),
        _ => Vec::new(),
    };

    if was_locked {
        let source = format!("session-envelope:{envelope_id}");
        store.revoke_session_grants_by_source(
            &record.root_session_id,
            &source,
            &format!("envelope revoked by {revoked_by}"),
        )?;
    }

    if !store.revoke_session_envelope_by_id(envelope_id)? {
        return Ok(None);
    }

    let (principal, role) = crate::runtime::session_timeline::decider_seat(revoked_by);
    let refs = autonoetic_types::session_timeline::TimelineRefs {
        plan_id: record.plan_id.clone(),
        ..Default::default()
    };
    let event = crate::runtime::session_timeline::build_timeline_event(
        record.root_session_id.clone(),
        record.root_session_id.clone(),
        None,
        &principal,
        &role,
        "envelope.revoked",
        None,
        Some(serde_json::json!({
            "envelope_id": envelope_id,
            "hosts": hosts,
            "source": record.source,
            "was_locked": was_locked,
            "revoked_by": revoked_by,
        })),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(target: "session_timeline", error = %e, "envelope.revoked timeline emit failed");
    }

    Ok(Some(record))
}

/// Hosts previously observed in-session but not yet covered by a locked envelope.
pub fn envelope_expansion_hint(
    store: &GatewayStore,
    root_session_id: &str,
    targets: &[String],
) -> Option<serde_json::Value> {
    let pending = hosts_pending_proposal(store, root_session_id).ok()?;
    if pending.is_empty() {
        return None;
    }
    let target_set: std::collections::BTreeSet<_> = targets.iter().cloned().collect();
    let overlap: Vec<String> = pending
        .iter()
        .filter(|h| target_set.contains(*h))
        .cloned()
        .collect();
    if overlap.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "observed_hosts": pending,
        "overlap_hosts": overlap,
        "message": "These hosts were used earlier in this session. Lock the session envelope to skip future prompts for them.",
    }))
}

fn emit_envelope_proposed_timeline(
    store: &GatewayStore,
    root_session_id: &str,
    envelope_id: i64,
    hosts: &[String],
    source: &str,
    actor_id: &str,
    plan_id: Option<&str>,
    capability: Option<&Capability>,
) {
    let (principal, role) = crate::runtime::session_timeline::decider_seat(actor_id);
    let refs = TimelineRefs {
        plan_id: plan_id.map(str::to_string),
        ..Default::default()
    };
    // Surface the proposed capability (esp. PromoteWith, whose `hosts` is empty)
    // so the operator deciding the lock sees exactly what they would bless.
    let event = crate::runtime::session_timeline::build_timeline_event(
        root_session_id.to_string(),
        root_session_id.to_string(),
        None,
        &principal,
        &role,
        "envelope.proposed",
        None,
        Some(serde_json::json!({
            "envelope_id": envelope_id,
            "hosts": hosts,
            "source": source,
            "capability": capability,
        })),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(target: "session_timeline", error = %e, "envelope.proposed timeline emit failed");
    }
}

fn emit_envelope_locked_timeline(
    store: &GatewayStore,
    record: &SessionEnvelopeRecord,
    hosts: &[String],
    locked_by: &str,
    grants_materialized: usize,
) {
    let (principal, role) = crate::runtime::session_timeline::decider_seat(locked_by);
    let refs = TimelineRefs {
        plan_id: record.plan_id.clone(),
        ..Default::default()
    };
    let event = crate::runtime::session_timeline::build_timeline_event(
        record.root_session_id.clone(),
        record.root_session_id.clone(),
        None,
        &principal,
        &role,
        "envelope.locked",
        None,
        Some(serde_json::json!({
            "envelope_id": record.id,
            "hosts": hosts,
            "source": record.source,
            "capability": record.capability,
            "grants_materialized": grants_materialized,
            "locked_by": locked_by,
        })),
        refs,
    );
    if let Err(e) = store.create_live_digest_event(&event) {
        tracing::debug!(target: "session_timeline", error = %e, "envelope.locked timeline emit failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::causal_chain::ExecutionTraceRecord;
    use tempfile::tempdir;

    fn curl_trace(session_id: &str, command: &str) -> ExecutionTraceRecord {
        ExecutionTraceRecord {
            trace_id: format!("trace-{}", uuid::Uuid::new_v4()),
            event_id: None,
            agent_id: "researcher.default".to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            timestamp: "2026-06-14T12:00:00Z".to_string(),
            tool_name: "sandbox_exec".to_string(),
            command: Some(command.to_string()),
            exit_code: Some(0),
            stdout: None,
            stderr: None,
            duration_ms: 10,
            success: 1,
            error_type: None,
            error_summary: None,
            approval_required: None,
            approval_request_id: None,
            arguments: Some(format!(r#"{{"command":"{command}"}}"#)),
            result: None,
        }
    }

    #[test]
    fn propose_and_lock_materializes_grants() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-505-lock";

        store.create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))?;

        let proposal = propose_discovered_envelope(&store, root, "discovered", None, "operator")?
            .expect("proposal");
        assert!(!proposal.skipped);
        assert_eq!(proposal.hosts, vec!["api.open-meteo.com".to_string()]);

        let lock = lock_session_envelope(&store, proposal.envelope_id, "operator")?;
        assert_eq!(lock.grants_materialized, 1);
        assert!(store.session_grants_cover_targets(root, &["api.open-meteo.com".to_string()]));
        Ok(())
    }

    #[test]
    fn wildcard_host_materializes_as_host_suffix_grant() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-wildcard-host";

        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["*.example.com".to_string()],
        }];
        let proposal =
            propose_envelope_from_capabilities(&store, root, &caps, "plan:test", None, "operator")?
                .expect("proposal");
        let lock = lock_session_envelope(&store, proposal.envelope_id, "operator")?;
        assert_eq!(lock.grants_materialized, 1);
        // A subdomain wildcard must actually cover concrete subdomains, not be
        // dropped as a dead ExactHost("*.example.com").
        assert!(
            store.session_grants_cover_targets(root, &["api.example.com".to_string()]),
            "subdomain wildcard grant should cover a concrete subdomain"
        );
        // Stored as the canonical bare suffix (CLI prepends `*.` on display).
        let grants = store.get_session_grants_structured(root)?;
        assert!(
            grants.iter().flat_map(|g| &g.targets).any(|t| matches!(
                t,
                GrantTarget::HostSuffix(s) if s == "example.com"
            )),
            "wildcard host should be stored as HostSuffix(\"example.com\"), got: {:?}",
            grants.iter().map(|g| &g.targets).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn propose_promote_with_rejects_empty_agent_id() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-empty-agent";

        let err = propose_promote_with_envelope(
            &store,
            root,
            "",
            &[Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            }],
            "test",
            "operator",
        )
        .expect_err("empty agent_id should be rejected");
        assert!(
            err.to_string().contains("non-empty agent_id"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[test]
    fn propose_skipped_returns_existing_pending_envelope_id() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-505-dedup-proposal";

        store.create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))?;

        let first = propose_discovered_envelope(&store, root, "discovered", None, "operator")?
            .expect("first proposal");
        assert!(!first.skipped);

        let second = propose_discovered_envelope(&store, root, "discovered", None, "operator")?
            .expect("second proposal");
        assert!(second.skipped);
        assert_eq!(second.envelope_id, first.envelope_id);
        assert_eq!(store.get_proposed_envelopes(root)?.len(), 1);
        Ok(())
    }

    #[test]
    fn propose_from_capabilities_stores_merged_network_access() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-504-cap-order";

        let caps = vec![
            Capability::SandboxFunctions {
                allowed: vec!["web.".to_string()],
            },
            Capability::NetworkAccess {
                hosts: vec!["api.example.com".to_string()],
            },
        ];
        let proposal = propose_envelope_from_capabilities(
            &store,
            root,
            &caps,
            "plan:test",
            None,
            "operator",
        )?
        .expect("proposal");
        let record = store
            .get_envelope_by_id(proposal.envelope_id)?
            .expect("network proposal row");
        assert!(matches!(
            &record.capability,
            Capability::NetworkAccess { hosts } if hosts == &vec!["api.example.com".to_string()]
        ));
        assert_eq!(store.get_proposed_envelopes(root)?.len(), 2);
        Ok(())
    }

    #[test]
    fn propose_promote_with_rejects_nested_promote_with() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-511-nested";

        let err = propose_promote_with_envelope(
            &store,
            root,
            "agent.test",
            &[Capability::PromoteWith {
                agent_id: "nested".to_string(),
                capabilities: vec![Capability::ReadAccess {
                    scopes: vec!["self.*".to_string()],
                }],
            }],
            "test",
            "operator",
        )
        .expect_err("nested PromoteWith should be rejected");

        assert!(
            err.to_string()
                .contains("PromoteWith capabilities cannot contain nested PromoteWith"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[test]
    fn expansion_hint_surfaces_observed_overlap() -> Result<()> {
        let dir = tempdir()?;
        let store = GatewayStore::open(dir.path())?;
        let root = "session-505-hint";

        store.create_execution_trace(&curl_trace(
            root,
            "curl -s https://api.open-meteo.com/v1/forecast",
        ))?;

        let hint = envelope_expansion_hint(
            &store,
            root,
            &["api.open-meteo.com".to_string(), "evil.example.com".to_string()],
        )
        .expect("hint");
        assert!(hint["overlap_hosts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "api.open-meteo.com"));
        Ok(())
    }
}
