//! Gateway Event Loop Server.

use crate::server::registry::PeerRegistry;
use autonoetic_types::config::GatewayConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

pub mod http;
pub mod jsonrpc;
pub mod ofp;
pub mod registry;
pub mod router;

pub struct GatewayServer {
    config: Arc<GatewayConfig>,
    registry: PeerRegistry,
}

impl GatewayServer {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: Arc::new(config),
            registry: PeerRegistry::new(),
        }
    }

    /// Run the main event loop for the Gateway daemon.
    pub async fn run(&self) -> anyhow::Result<()> {
        let node_id =
            std::env::var("AUTONOETIC_NODE_ID").unwrap_or_else(|_| self.config.node_id.clone());
        let node_name =
            std::env::var("AUTONOETIC_NODE_NAME").unwrap_or_else(|_| self.config.node_name.clone());

        // Propagate resolved identity to env so runtime helpers (gateway_actor_id, etc.) pick it up.
        std::env::set_var("AUTONOETIC_NODE_ID", &node_id);
        std::env::set_var("AUTONOETIC_NODE_NAME", &node_name);

        // Initialize sandbox config (config is authoritative by default; env overrides
        // are ignored unless AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true).
        crate::sandbox::init_sandbox_config(&self.config.sandbox);

        let shared_secret = std::env::var("AUTONOETIC_SHARED_SECRET").map_err(|_| {
            anyhow::anyhow!("Missing required environment variable AUTONOETIC_SHARED_SECRET")
        })?;
        let jsonrpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.config.port);
        let ofp_addr: SocketAddr = format!("0.0.0.0:{}", self.config.ofp_port)
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid OFP bind address: {}", e))?;

        let gateway_dir = crate::execution::gateway_root_dir(&self.config);
        let gateway_store = Arc::new(crate::scheduler::gateway_store::GatewayStore::open(
            &gateway_dir,
        )?);
        gateway_store.set_approval_flood_cap(self.config.max_pending_approvals_per_root);

        // Apply data retention policy on startup
        if let Err(e) = gateway_store.apply_retention_policy(&self.config.retention) {
            tracing::warn!(
                target: "gateway_store",
                error = %e,
                "Failed to apply retention policy"
            );
        }

        // Reap orphaned continuation files from crash/restart
        match crate::runtime::continuation::reap_orphaned_continuations(
            &self.config,
            &gateway_store,
        ) {
            Ok(n) if n > 0 => tracing::info!(
                target: "gateway",
                "Reaped {} orphaned continuation file(s)",
                n
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                target: "gateway",
                error = %e,
                "Continuation reaper failed"
            ),
        }

        // Reconcile system agents (create cron jobs if missing)
        let reconcile_results =
            crate::scheduler::system_agents::reconcile_system_agents(&self.config, &gateway_store);
        for r in &reconcile_results {
            match r.action {
                crate::scheduler::system_agents::ReconcileAction::Created => {
                    tracing::info!(
                        target: "system_agents",
                        agent_id = %r.agent_id,
                        "System agent cron job created: {}", r.message
                    );
                }
                crate::scheduler::system_agents::ReconcileAction::Failed => {
                    tracing::warn!(
                        target: "system_agents",
                        agent_id = %r.agent_id,
                        "System agent reconciliation failed: {}", r.message
                    );
                }
                _ => {
                    tracing::debug!(
                        target: "system_agents",
                        agent_id = %r.agent_id,
                        "System agent skipped: {}", r.message
                    );
                }
            }
        }

        let jsonrpc_router = Arc::new(crate::router::JsonRpcRouter::new(
            self.config.as_ref().clone(),
            Some(gateway_store.clone()),
        ));
        crate::scheduler::signal::start_signal_poller_if_needed(
            self.config.agents_dir.clone(),
            self.config.port,
        )?;
        let background_scheduler =
            crate::scheduler::start_background_scheduler(jsonrpc_router.execution_service());
        let eval_runner =
            crate::scheduler::eval_runner::start_eval_runner(jsonrpc_router.execution_service());

        tracing::info!(
            "GatewayServer starting (jsonrpc_port={}, ofp_port={}, node_id={})",
            self.config.port,
            self.config.ofp_port,
            node_id
        );

        // Phase 5/7: start OFP and JSON-RPC listeners concurrently.
        // Missing federation identity is a hard failure by design.
        tokio::try_join!(
            ofp::start_ofp_server(
                ofp_addr,
                node_id,
                node_name,
                shared_secret.clone(),
                self.registry.clone(),
                jsonrpc_router.clone(),
            ),
            jsonrpc::start_jsonrpc_server(
                jsonrpc_addr,
                (*jsonrpc_router).clone(),
                Some(shared_secret),
            ),
            background_scheduler,
            eval_runner,
        )?;
        Ok(())
    }
}
