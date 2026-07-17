//! Operator-facing PlanFrame queries (chat TUI, CLI) without routing through an agent tool.

use anyhow::Result;
use autonoetic_types::plan_frame::PlanFrame;

use crate::scheduler::gateway_store::GatewayStore;

pub fn pending_plan_frames_for_root(
    store: &GatewayStore,
    root_session_id: &str,
) -> Result<Vec<PlanFrame>> {
    store.list_pending_plan_frames_for_root(root_session_id)
}
