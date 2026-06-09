//! `self_describe` — the autonoetic self-awareness surface (#300, epic #297;
//! design `docs/design/constitution-restructure.md`).
//!
//! Autonoetic consciousness is self-knowing across time. Today an agent
//! must *assemble* self-knowledge from scattered tools (`agent_inspect`,
//! `digest_query`, `constitution_read`, revision lookups). This tool makes
//! autonoesis a single first-class capability, answering in one call:
//!
//! - **who am I** — identity + persona (from the manifest)
//! - **what may I do** — declared capabilities + allowed tool tiers
//! - **what am I guaranteed** — the Bill of Rights (from the enforcement
//!   register's `rights()`), surfaced front-line
//! - **what have I done** — a pointer to the agent's own history surfaces
//! - **how do I evolve** — the amendment / promotion / revision paths open
//!   to it
//!
//! It is **always available**: an agent always has the standing to know
//! itself. It reports only the calling agent's *own* identity and the
//! *public* constitution — no cross-agent data, no privileged lookups — so
//! it needs no capability gate.
//!
//! This first slice composes the manifest + the register's rights with
//! structured pointers for the history/evolution dimensions; richer inline
//! history aggregation is a follow-up.

use std::path::Path;
use std::sync::Arc;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;

use crate::enforcement_register;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::tools::{NativeTool, NativeToolRunContext};

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(SelfDescribeTool));
}

pub struct SelfDescribeTool;

impl NativeTool for SelfDescribeTool {
    fn name(&self) -> &'static str {
        "self_describe"
    }

    /// Always available — an agent always has the standing to know itself.
    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Describe yourself: who you are (identity, persona), what you may do \
                          (capabilities, tool tiers), what you are guaranteed (your rights under \
                          the constitution), what you have done (where your history lives), and \
                          how you can evolve. Takes no arguments; reports only your own self."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let can_propose_amendments = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::ConstitutionalProposal { .. }));

        // what am I guaranteed — the Bill of Rights, surfaced front-line.
        let rights: Vec<serde_json::Value> = enforcement_register::rights()
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "title": r.title,
                    "guarantee": r.statement,
                    "binds": "gateway",
                })
            })
            .collect();

        let out = serde_json::json!({
            "ok": true,
            // who am I
            "identity": {
                "agent_id": manifest.agent.id,
                "name": manifest.agent.name,
                "description": manifest.agent.description,
            },
            // what may I do
            "may_do": {
                "capabilities": manifest
                    .capabilities
                    .iter()
                    .map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
                    .collect::<Vec<_>>(),
                "allowed_tool_tiers": manifest.allowed_tool_tiers,
            },
            // what am I guaranteed (rights front-line)
            "guaranteed": {
                "note": "Rights bind the gateway on your behalf — these are upheld for you, \
                         not granted at discretion.",
                "rights": rights,
            },
            // what have I done — pointer to the agent's own history surfaces
            "history": {
                "session_id": session_id,
                "how_to_inspect": [
                    "digest_query — your own session digests",
                    "observability_search / observability_read — your published session reports",
                ],
            },
            // how do I evolve
            "evolution": {
                "may_propose_amendments": can_propose_amendments,
                "paths": [
                    "agent revisions — immutable, content-addressed; promotion advances the alias",
                    "skill promotion — crystallise successful tactics into reusable skills",
                    if can_propose_amendments {
                        "constitution_propose_amendment — you hold the ConstitutionalProposal capability (Ri-0.8)"
                    } else {
                        "constitutional amendment — requires the ConstitutionalProposal capability, which you do not currently hold"
                    },
                ],
            },
        });
        Ok(out.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};

    fn manifest_with(caps: Vec<Capability>) -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "demo.agent".to_string(),
                name: "Demo".to_string(),
                description: "a demo".to_string(),
            },
            capabilities: caps,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        }
    }

    fn run(manifest: &AgentManifest) -> serde_json::Value {
        let tool = SelfDescribeTool;
        let policy = PolicyEngine::new(manifest.clone());
        let r = tool
            .execute(
                manifest,
                &policy,
                Path::new("/tmp"),
                None,
                "{}",
                Some("sess-1"),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        serde_json::from_str(&r).unwrap()
    }

    #[test]
    fn always_available() {
        assert!(SelfDescribeTool.is_available(&manifest_with(vec![])));
    }

    #[test]
    fn surfaces_identity_and_rights() {
        let v = run(&manifest_with(vec![]));
        assert_eq!(v["ok"], true);
        assert_eq!(v["identity"]["agent_id"], "demo.agent");
        // Rights are surfaced front-line, sourced from the register.
        let rights = v["guaranteed"]["rights"].as_array().unwrap();
        assert_eq!(rights.len(), enforcement_register::rights().len());
        assert!(rights.iter().all(|r| r["binds"] == "gateway"));
        assert!(
            rights.iter().any(|r| r["id"] == "Ri-0.14"),
            "expected the wake-up right to be surfaced"
        );
    }

    #[test]
    fn evolution_reflects_amendment_capability() {
        let without = run(&manifest_with(vec![]));
        assert_eq!(without["evolution"]["may_propose_amendments"], false);

        let with = run(&manifest_with(vec![Capability::ConstitutionalProposal {
            patterns: vec!["*".to_string()],
        }]));
        assert_eq!(with["evolution"]["may_propose_amendments"], true);
    }
}
