//! Operator-facing session envelope RPC handlers (#505).

use anyhow::Result;

use crate::runtime::session_envelope::{
    hosts_pending_proposal, lock_session_envelope, propose_discovered_envelope,
    EnvelopeLockResult, EnvelopeProposalResult,
};
use crate::scheduler::gateway_store::session_envelopes::SessionEnvelopeRecord;
use crate::scheduler::gateway_store::GatewayStore;

pub fn propose_session_envelope(
    store: &GatewayStore,
    root_session_id: &str,
    source: &str,
    plan_id: Option<&str>,
    proposed_by: &str,
) -> Result<Option<EnvelopeProposalResult>> {
    propose_discovered_envelope(store, root_session_id, source, plan_id, proposed_by)
}

pub fn lock_session_envelope_operator(
    store: &GatewayStore,
    envelope_id: i64,
    locked_by: &str,
) -> Result<EnvelopeLockResult> {
    lock_session_envelope(store, envelope_id, locked_by)
}

#[derive(Debug, serde::Serialize)]
pub struct SessionEnvelopeListResult {
    pub proposed: Vec<SessionEnvelopeRecord>,
    pub active: Vec<SessionEnvelopeRecord>,
    pub observed_hosts: Vec<String>,
    pub pending_hosts: Vec<String>,
}

pub fn list_session_envelopes(
    store: &GatewayStore,
    root_session_id: &str,
) -> Result<SessionEnvelopeListResult> {
    Ok(SessionEnvelopeListResult {
        proposed: store.get_proposed_envelopes(root_session_id)?,
        active: store.get_active_envelopes(root_session_id)?,
        observed_hosts: store.discover_observed_hosts(root_session_id)?,
        pending_hosts: hosts_pending_proposal(store, root_session_id)?,
    })
}
