//! Gateway Event Loop Server.

use crate::runtime::content_store::ContentStore;
use crate::server::registry::PeerRegistry;
use autonoetic_types::config::GatewayConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;

pub mod http;
pub mod jsonrpc;
pub mod ofp;
pub mod registry;
pub mod router;
pub mod transport;

pub struct GatewayServer {
    config: Arc<GatewayConfig>,
    registry: PeerRegistry,
}

impl GatewayServer {
    pub fn new(config: GatewayConfig) -> Self {
        // #1002 slice 4: legacy host-fs ro-binds the whole host `/` into every
        // bubblewrap sandbox. It stays the default until after launch (RFC
        // sandbox-mount-allow-set.md DP-1) but the deprecation warning makes
        // the migration visible from day one.
        match config.sandbox.host_fs.as_str() {
            "allow_set" => {
                tracing::info!(
                    target: "sandbox",
                    "sandbox.host_fs: allow_set — bubblewrap sandboxes mount only the \
                     gateway-asserted set"
                );
            }
            other => {
                // Unknown values fall back to legacy behaviour (matching the
                // sandbox dev_mode precedent), but the message must name what
                // actually happened, never hard-code the value (#1174 review).
                tracing::warn!(
                    target: "sandbox",
                    host_fs = %other,
                    "sandbox.host_fs is '{other}', not 'allow_set': bubblewrap sandboxes \
                     keep the legacy whole-host ro-bind. Set sandbox.host_fs: allow_set to \
                     mount only the gateway-asserted set (RFC \
                     docs/proposals/sandbox-mount-allow-set.md); the default flips after launch."
                );
            }
        }
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

        // Cache the resolved node id for the process lifetime so hot-path timeline/
        // event builders avoid a per-event `std::env::var` syscall (#586).
        crate::execution::init_gateway_node_id(&node_id);

        // Initialize sandbox config (config is authoritative by default; env overrides
        // are ignored unless AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true).
        crate::sandbox::init_sandbox_config(&self.config.sandbox);

        let gateway_dir = crate::execution::gateway_root_dir(&self.config);
        crate::bootstrap::bootstrap_constitution_snapshot(self.config.as_ref(), &gateway_dir)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Constitution bootstrap into gateway dir failed (target='{}'): {}",
                    gateway_dir.join("constitution").display(),
                    e
                )
            })?;
        crate::bootstrap::bootstrap_sdk_snapshot(&gateway_dir).map_err(|e| {
            anyhow::anyhow!(
                "SDK bootstrap into gateway dir failed (target='{}'): {}",
                gateway_dir.join("sdk").display(),
                e
            )
        })?;
        crate::bootstrap::bootstrap_wiki_snapshot(&gateway_dir).map_err(|e| {
            anyhow::anyhow!(
                "Wiki bootstrap into gateway dir failed (target='{}'): {}",
                gateway_dir.join("wiki").display(),
                e
            )
        })?;
        crate::sandbox::init_sdk_deployed_path(&gateway_dir);

        crate::constitution_digest::initialize_constitution(self.config.as_ref()).map_err(|e| {
            anyhow::anyhow!(
                "Constitution initialization failed (source='{}', lock='{}'): {}",
                self.config.constitution.source_path.display(),
                self.config.constitution.lock_path.display(),
                e
            )
        })?;

        // Constitution lock is an immutable contract artifact.
        // Refuse boot if lock metadata does not match canonical digest/profile extraction.
        crate::constitution_digest::verify_constitution_lock_integrity().map_err(|e| {
            anyhow::anyhow!(
                "Constitution lock integrity verification failed; refusing to start: {}",
                e
            )
        })?;

        let shared_secret = std::env::var("AUTONOETIC_SHARED_SECRET").map_err(|_| {
            anyhow::anyhow!("Missing required environment variable AUTONOETIC_SHARED_SECRET")
        })?;
        let jsonrpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.config.port);
        let ofp_addr: SocketAddr = format!("0.0.0.0:{}", self.config.ofp_port)
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid OFP bind address: {}", e))?;

        let gateway_store = Arc::new(crate::scheduler::gateway_store::GatewayStore::open(
            &gateway_dir,
        )?);
        gateway_store.set_approval_flood_cap(self.config.max_pending_approvals_per_root);
        gateway_store.set_escalation_flood_cap(self.config.max_pending_escalations_per_root);
        gateway_store
            .set_anomaly_flag_flood_cap(self.config.max_pending_anomaly_flags_per_reporter);
        gateway_store
            .host_probe_budget
            .set_cap(self.config.max_probes_per_host as usize);

        // Seed the built-in civic eval suite (#772 E.1). Idempotent.
        if let Err(e) = crate::runtime::civic_evals::ensure_civic_eval_suite(
            &gateway_store,
            &node_id,
        ) {
            tracing::warn!(
                target: "bootstrap",
                error = %e,
                "Failed to seed civic eval suite"
            );
        }

        {
            let probe_result = crate::vault::probe_master_key(&self.config.agents_dir);
            gateway_store.emit_vault_key_probe_event(&probe_result);
            if !probe_result.is_present() {
                let detail = vault_probe_failure_reason(&probe_result);
                anyhow::bail!(
                    "Vault master key probe failed (R+8): {}. Refusing startup. Configure AUTONOETIC_VAULT_KEY, AUTONOETIC_VAULT_KEY_PATH, or provision {}/.gateway/vault.key",
                    detail,
                    self.config.agents_dir.display()
                );
            }
        }

        // Apply data retention policy on startup
        if let Err(e) = gateway_store.apply_retention_policy(&self.config.retention) {
            tracing::warn!(
                target: "gateway_store",
                error = %e,
                "Failed to apply retention policy"
            );
        }

        if self.config.operator_activity.retention_days > 0 {
            if let Err(e) = gateway_store.prune_operator_activity(self.config.operator_activity.retention_days) {
                tracing::warn!(
                    target: "operator_activity",
                    error = %e,
                    "Failed to prune operator activity on startup"
                );
            }
        }

        // Continuation files are obsolete — all suspension state is now captured
        // in enriched checkpoints.  No reclamation needed.

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

        let auto_learning_results =
            crate::scheduler::auto_learning_jobs::inject_auto_learning_jobs(&self.config, &gateway_store);
        for r in &auto_learning_results {
            match r.action {
                crate::scheduler::system_agents::ReconcileAction::Created => {
                    tracing::info!(
                        target: "auto_learning",
                        agent_id = %r.agent_id,
                        "Auto-learning cron created: {}",
                        r.message
                    );
                }
                crate::scheduler::system_agents::ReconcileAction::Failed => {
                    tracing::warn!(
                        target: "auto_learning",
                        agent_id = %r.agent_id,
                        "Auto-learning scheduling failed: {}",
                        r.message
                    );
                }
                _ => {
                    tracing::debug!(
                        target: "auto_learning",
                        agent_id = %r.agent_id,
                        "{}",
                        r.message
                    );
                }
            }
        }

        let jsonrpc_router = Arc::new(crate::router::JsonRpcRouter::new(
            self.config.as_ref().clone(),
            Some(gateway_store.clone()),
        ));
        jsonrpc_router
            .execution_service()
            .warm_local_model_context()
            .await;

        // Warn at startup if any LLM preset has no context_window_tokens and
        // the provider cannot resolve one from env, static table, catalog, or probe.
        let env_override = std::env::var("AUTONOETIC_LLM_CONTEXT_WINDOW").ok();
        let local_context = jsonrpc_router.execution_service().local_model_context_cache();
        for (name, preset) in &self.config.llm_presets {
            if preset.routing.is_some() {
                continue;
            }
            if preset.context_window_tokens.is_some() {
                continue;
            }
            if env_override.is_some() {
                continue;
            }
            if let Some(ref model) = preset.model {
                if crate::runtime::context_governor::resolver::static_context_window(model).is_some() {
                    continue;
                }
                if let Some(ref base_url) = preset.base_url {
                    if local_context.get(base_url, model).is_some() {
                        continue;
                    }
                }
            }
            if preset.provider.as_deref().map(|p| p.eq_ignore_ascii_case("openrouter")).unwrap_or(false) {
                continue;
            }
            tracing::warn!(
                target: "gateway",
                preset = %name,
                provider = ?preset.provider,
                model = ?preset.model,
                "LLM preset has no context_window_tokens — context governor cannot enforce budget for this preset. \
                 Set context_window_tokens in the preset, run `autonoetic run --refresh-models`, \
                 or set AUTONOETIC_LLM_CONTEXT_WINDOW env var."
            );
        }

        crate::scheduler::signal::start_signal_poller_if_needed(
            self.config.agents_dir.clone(),
            self.config.port,
        )?;
        let background_scheduler =
            crate::scheduler::start_background_scheduler(jsonrpc_router.clone());
        let fast_scheduler = crate::scheduler::fast_scheduler::start_fast_scheduler(
            jsonrpc_router.execution_service(),
        );
        let eval_runner =
            crate::scheduler::eval_runner::start_eval_runner(jsonrpc_router.execution_service());

        tracing::info!(
            "GatewayServer starting (jsonrpc_port={}, http_port={}, ofp_port={}, node_id={})",
            self.config.port,
            self.config.http_port,
            self.config.ofp_port,
            node_id
        );

        let http_port = self.config.http_port;
        let gateway_dir_http = gateway_dir.clone();
        let shared_secret_http = shared_secret.clone();
        let jsonrpc_router_http = jsonrpc_router.clone();
        let http_server = async move {
            if http_port == 0 {
                tracing::info!("HTTP ingress disabled (http_port=0)");
                std::future::pending::<()>().await;
                unreachable!()
            }

            let http_addr =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), http_port);
            let store = ContentStore::new(&gateway_dir_http)?;
            let http_state = crate::server::http::HttpState {
                store: Arc::new(Mutex::new(store)),
                shared_secret: shared_secret_http,
                max_body_size: crate::server::http::DEFAULT_MAX_BODY_SIZE,
                router: Some(jsonrpc_router_http),
            };
            tracing::info!(
                http_listen = %http_addr,
                "HTTP ingress listening (0.0.0.0; Bearer-authenticated)"
            );
            crate::server::http::start_http_server(http_addr, http_state).await
        };

        // Phase 5/7: start OFP, JSON-RPC, and HTTP listeners concurrently.
        // Missing federation identity is a hard failure by design.
        //
        // `ofp_port: 0` disables the federation listener (same convention as
        // `http_port`): outbound federation is unwired and there are no peers
        // to receive from, so the listener is opt-in until federation ships.
        let ofp_port = self.config.ofp_port;
        let ofp_config = self.config.clone();
        let ofp_registry = self.registry.clone();
        let ofp_router = jsonrpc_router.clone();
        let ofp_shared_secret = shared_secret.clone();
        let ofp_server = async move {
            if ofp_port == 0 {
                tracing::info!("OFP federation listener disabled (ofp_port=0)");
                std::future::pending::<()>().await;
                unreachable!()
            }
            ofp::start_ofp_server(
                ofp_addr,
                node_id,
                node_name,
                ofp_config,
                ofp_shared_secret,
                ofp_registry,
                ofp_router,
            )
            .await
        };
        //
        // Each member is `Box::pin`ned so it lives on the heap rather than
        // inside `run`'s future. These are the six largest futures in the
        // process — every request handler, scheduler tick and background loop
        // they transitively await is part of their state machines — and joining
        // them inline makes `run`'s own future the sum of all six. That future
        // is polled on the caller's stack, which left roughly a page of
        // headroom before a deep `serde_json` parse or an `ed25519` verify (the
        // constitution lock does both at startup) overflowed it. Boxing costs
        // six one-time allocations and removes a whole class of
        // "unrelated change elsewhere aborts startup" failures.
        tokio::try_join!(
            Box::pin(ofp_server),
            Box::pin(jsonrpc::start_jsonrpc_server(
                jsonrpc_addr,
                (*jsonrpc_router).clone(),
                Some(shared_secret),
            )),
            Box::pin(http_server),
            Box::pin(background_scheduler),
            Box::pin(fast_scheduler),
            Box::pin(eval_runner),
        )?;
        Ok(())
    }
}

fn vault_probe_failure_reason(result: &crate::vault::KeyProbeResult) -> String {
    match result {
        crate::vault::KeyProbeResult::Present { .. } => "key present".to_string(),
        crate::vault::KeyProbeResult::NotConfigured => {
            "no AUTONOETIC_VAULT_KEY, AUTONOETIC_VAULT_KEY_PATH, or auto-generated vault.key found"
                .to_string()
        }
        crate::vault::KeyProbeResult::Missing { source, path } => format!(
            "{} points to missing file {}",
            vault_probe_source_name(source),
            path
        ),
        crate::vault::KeyProbeResult::Invalid { source, reason } => {
            format!("{} is invalid: {}", vault_probe_source_name(source), reason)
        }
    }
}

fn vault_probe_source_name(source: &crate::vault::KeySource) -> &'static str {
    match source {
        crate::vault::KeySource::EnvVar => "AUTONOETIC_VAULT_KEY",
        crate::vault::KeySource::FilePath => "AUTONOETIC_VAULT_KEY_PATH",
        crate::vault::KeySource::AutoGenerated => "auto-generated vault.key",
    }
}
