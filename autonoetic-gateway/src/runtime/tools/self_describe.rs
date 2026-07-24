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
//!
//! The evolution dimension is **derived, never asserted** (#818). It used to
//! be a hardcoded prose list, which let the runtime advertise a path the code
//! did not have — the confabulation P-6.23 exists to prevent, committed by
//! the gateway itself. See [`EVOLUTION_PATHS`].

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;

use crate::enforcement_register;
use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::tools::{NativeTool, NativeToolRunContext};
use crate::scheduler::gateway_store::GatewayStore;

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(SelfDescribeTool));
}

/// How an evolution path is actually enacted — the field that keeps the
/// "how do I evolve" answer honest.
///
/// Each variant carries what must be true for the path to be real, and the
/// guard tests in this module assert that each reference resolves: a
/// `SelfTool` names a registered native tool, a `Pipeline` or
/// `OperatorPipeline` names installed agent bundles, an `Unimplemented` names
/// the issue tracking it. A renamed tool or agent therefore breaks a test
/// rather than silently turning the answer into a lie.
#[derive(Debug, Clone, Copy)]
enum PathEnactor {
    /// The caller enacts it itself with this native tool — advertised as
    /// available only when the tool is available to the caller's manifest.
    SelfTool(&'static str),
    /// The evolution pipeline enacts it on the caller's behalf — advertised
    /// as available only when every listed agent is installed here.
    Pipeline(&'static [&'static str]),
    /// Same installation requirement as `Pipeline`, but **only an operator can
    /// start it**. Kept distinct so an agent does not read "available" as "I can
    /// trigger this" — that would be a fresh confabulation in place of the one
    /// #818 removed.
    OperatorPipeline {
        agents: &'static [&'static str],
        /// How the operator starts it, named so the agent can *ask* for it.
        trigger: &'static str,
    },
    /// Advertised historically but implemented by nothing. Reported as
    /// unavailable, naming the issue that tracks it.
    Unimplemented(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct EvolutionPath {
    id: &'static str,
    summary: &'static str,
    enactor: PathEnactor,
}

/// The revision one-door: the only agent licensed to call the revision tools.
const REVISION_PIPELINE: &[&str] = &["specialized_builder.default"];

/// The B.2 lesson-graduation pipeline: curator proposes, steward judges,
/// factory enacts.
const GRADUATION_PIPELINE: &[&str] = &[
    "memory-curator.default",
    "evolution-steward.default",
    "agent-factory.default",
];

/// The crystallization route (#818): the crystallizer decides which durable home
/// a proven tactic gets, and each of the three verdicts has its own enactor —
/// all four must be installed for the whole route to be open, and the reason
/// string names whichever is missing.
const CRYSTALLIZATION_PIPELINE: &[&str] = &[
    "skill-crystallizer.default",
    "evolution-steward.default",
    "agent-adapter.default",
    "agent-factory.default",
];

/// Every evolution path the runtime may tell an agent about.
///
/// Adding a row here is a claim, and the guard tests below make it a checkable
/// one.
const EVOLUTION_PATHS: &[EvolutionPath] = &[
    EvolutionPath {
        id: "agent_revision",
        summary: "Create a Candidate revision directly — immutable and content-addressed; \
                  promotion advances the alias.",
        enactor: PathEnactor::SelfTool("agent_revision_create_from_intent"),
    },
    EvolutionPath {
        id: "revision_via_builder",
        summary: "Delegate revision creation and promotion to the only agent licensed to call \
                  the revision tools (the one-door invariant, P-9.15).",
        enactor: PathEnactor::Pipeline(REVISION_PIPELINE),
    },
    EvolutionPath {
        id: "lesson_graduation",
        summary: "Lessons that recur across sessions graduate into your SKILL.md instruction \
                  text through the curator → steward → factory pipeline.",
        enactor: PathEnactor::Pipeline(GRADUATION_PIPELINE),
    },
    EvolutionPath {
        id: "skill_crystallization",
        summary: "Make a tactic that worked reusable: the crystallizer routes it to an \
                  instruction on an existing agent, a wrapper around one, or a new skill — \
                  whichever fits. An operator starts this; you cannot trigger it yourself, but \
                  you can ask for it.",
        enactor: PathEnactor::OperatorPipeline {
            agents: CRYSTALLIZATION_PIPELINE,
            trigger: "/crystallize in the session room (skill.crystallize_from_session)",
        },
    },
    EvolutionPath {
        id: "constitution_amendment",
        summary: "Propose an amendment to the law that binds you (Ri-0.8).",
        enactor: PathEnactor::SelfTool("constitution_propose_amendment"),
    },
];

/// Is `agent_id` installed — resolvable to a revision — on this gateway?
///
/// `None` when there is no gateway store in this call and installation cannot
/// be checked. Unknown is reported as unavailable, never as available: the
/// only safe direction for an honesty fix is to under-claim.
fn agent_is_installed(store: Option<&GatewayStore>, agent_id: &str) -> Option<bool> {
    let store = store?;
    Some(crate::runtime::tools::resolve_target_to_agent_ref(agent_id, store).is_ok())
}

/// Render one path with its availability derived from this caller's tools and
/// this gateway's installed agents.
fn describe_path(
    path: &EvolutionPath,
    manifest: &AgentManifest,
    available_tools: &HashSet<String>,
    store: Option<&GatewayStore>,
) -> serde_json::Value {
    let (available, enacted_by, via, unavailable_reason) = match path.enactor {
        PathEnactor::SelfTool(tool) => {
            let have_tool = available_tools.contains(tool);
            // Two distinct ways a tool can be absent from the available set, and
            // the agent needs to know which: an excluded tool stays closed no
            // matter what capabilities it is granted.
            let reason = if have_tool {
                None
            } else if crate::runtime::tools::is_tool_excluded_public(tool, manifest) {
                Some(format!(
                    "the '{tool}' tool is excluded by your manifest (excluded_tools) — this path \
                     stays closed even if you hold the capability it requires"
                ))
            } else {
                Some(format!(
                    "the '{tool}' tool is not available to you — you do not hold the capability \
                     it requires"
                ))
            };
            (have_tool, "self", vec![tool.to_string()], reason)
        }
        PathEnactor::Pipeline(agents) => {
            let reason = pipeline_unavailable_reason(agents, store);
            (
                reason.is_none(),
                "evolution_pipeline",
                agents.iter().map(|a| (*a).to_string()).collect(),
                reason,
            )
        }
        PathEnactor::OperatorPipeline { agents, .. } => {
            let reason = pipeline_unavailable_reason(agents, store);
            (
                reason.is_none(),
                "operator_pipeline",
                agents.iter().map(|a| (*a).to_string()).collect(),
                reason,
            )
        }
        PathEnactor::Unimplemented(issue) => (
            false,
            "nothing",
            Vec::new(),
            Some(format!("not implemented — tracked by {issue}")),
        ),
    };

    let mut out = serde_json::json!({
        "path": path.id,
        "available": available,
        "enacted_by": enacted_by,
        "via": via,
        "summary": path.summary,
        "unavailable_reason": unavailable_reason,
    });
    // An operator-started path names its trigger, so an agent that wants it can
    // ask for it by name instead of guessing at a tool it does not have.
    if let PathEnactor::OperatorPipeline { trigger, .. } = path.enactor {
        out["operator_trigger"] = serde_json::json!(trigger);
    }
    out
}

/// Why a pipeline of agents is not usable, or `None` when all are installed.
///
/// Unverifiable (no gateway store in this call) is reported as unavailable, not
/// assumed available — the same under-claiming rule the whole surface follows.
fn pipeline_unavailable_reason(
    agents: &'static [&'static str],
    store: Option<&GatewayStore>,
) -> Option<String> {
    let mut missing: Vec<String> = Vec::new();
    let mut unverified = false;
    for agent in agents {
        match agent_is_installed(store, agent) {
            Some(true) => {}
            Some(false) => missing.push((*agent).to_string()),
            None => unverified = true,
        }
    }
    if unverified {
        Some("installation could not be verified in this context — no gateway store".to_string())
    } else if !missing.is_empty() {
        Some(format!(
            "not installed on this gateway: {}",
            missing.join(", ")
        ))
    } else {
        None
    }
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
        gateway_store: Option<Arc<GatewayStore>>,
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

        // how do I evolve — derived from the tools this caller actually has
        // and the agents actually installed here (#818). Capability-level
        // availability: a tool advertised here can still be filtered out of a
        // given turn by the tool-tier filter (see `allowed_tool_tiers` above).
        let available_tools =
            crate::runtime::tools::default_registry().available_tool_names(manifest);
        let store_ref = gateway_store.as_deref();
        let evolution_paths: Vec<serde_json::Value> = EVOLUTION_PATHS
            .iter()
            .map(|path| describe_path(path, manifest, &available_tools, store_ref))
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
                "note": "Each path's availability is derived from the tools you hold and the \
                         agents installed here — an unavailable path is reported as such with \
                         its reason, never implied to work.",
                "paths": evolution_paths,
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
            singleton: false,
        },
            capabilities: caps,
            llm_overrides: None,
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
            excluded_tools: vec![],
            agentskills_import: None,
            compression: None,
            open_web: false,
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

    fn path_of(v: &serde_json::Value, id: &str) -> serde_json::Value {
        v["evolution"]["paths"]
            .as_array()
            .expect("paths should be an array")
            .iter()
            .find(|p| p["path"] == id)
            .unwrap_or_else(|| panic!("path '{id}' should be advertised"))
            .clone()
    }

    /// Directory names of every reference agent bundle (`agents/*/<id>/SKILL.md`).
    fn reference_bundle_ids() -> std::collections::HashSet<String> {
        let agents_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("agents");
        let mut ids = std::collections::HashSet::new();
        let groups = std::fs::read_dir(&agents_root)
            .unwrap_or_else(|e| panic!("agents dir {} should read: {e}", agents_root.display()));
        for group in groups.flatten() {
            if !group.path().is_dir() {
                continue;
            }
            let Ok(bundles) = std::fs::read_dir(group.path()) else {
                continue;
            };
            for bundle in bundles.flatten() {
                if bundle.path().join("SKILL.md").is_file() {
                    ids.insert(bundle.file_name().to_string_lossy().to_string());
                }
            }
        }
        ids
    }

    /// The guard that makes an advertised path a checkable claim (#818): a
    /// `SelfTool` path must name a tool the registry actually registers, so a
    /// tool rename breaks this test instead of turning the answer into a lie.
    #[test]
    fn advertised_self_tools_are_registered() {
        let registered = crate::runtime::tools::default_registry().registered_tool_names();
        for path in EVOLUTION_PATHS {
            if let PathEnactor::SelfTool(tool) = path.enactor {
                assert!(
                    registered.contains(tool),
                    "evolution path '{}' advertises tool '{}', which is not registered",
                    path.id,
                    tool
                );
            }
        }
    }

    /// Same guard for the pipeline paths: every agent named must exist as a
    /// reference bundle, so renaming an evolution agent cannot leave
    /// `self_describe` promising a pipeline nothing can run.
    #[test]
    fn advertised_pipeline_agents_have_reference_bundles() {
        let bundles = reference_bundle_ids();
        for path in EVOLUTION_PATHS {
            let agents = match path.enactor {
                PathEnactor::Pipeline(agents) => agents,
                PathEnactor::OperatorPipeline { agents, .. } => agents,
                _ => continue,
            };
            for agent in agents {
                assert!(
                    bundles.contains(*agent),
                    "evolution path '{}' advertises agent '{}', which has no reference bundle",
                    path.id,
                    agent
                );
            }
        }
    }

    #[test]
    fn unimplemented_paths_name_a_tracking_issue() {
        for path in EVOLUTION_PATHS {
            if let PathEnactor::Unimplemented(issue) = path.enactor {
                let number = issue.strip_prefix('#').unwrap_or_else(|| {
                    panic!("path '{}' issue ref should start with '#'", path.id)
                });
                assert!(
                    number.parse::<u32>().is_ok(),
                    "path '{}' issue ref '{}' should be '#<number>'",
                    path.id,
                    issue
                );
            }
        }
    }

    /// Crystallization is real now (#818), but only an operator starts it. The
    /// row must say so: an agent reading "available" as "I can trigger this"
    /// would be a fresh confabulation in place of the one this work removed.
    /// The trigger is named so the agent can ask for it instead of hunting for
    /// a tool that does not exist.
    #[test]
    fn crystallization_is_an_operator_started_route() {
        let path = path_of(&run(&manifest_with(vec![])), "skill_crystallization");
        assert_eq!(path["enacted_by"], "operator_pipeline");
        assert_eq!(
            path["operator_trigger"],
            "/crystallize in the session room (skill.crystallize_from_session)"
        );
        assert!(
            path["via"]
                .as_array()
                .expect("via should list the pipeline")
                .iter()
                .any(|a| a == "skill-crystallizer.default"),
            "the crystallizer should be named as the route's entry point, got {:?}",
            path["via"]
        );
        // No store in this context, so availability under-claims rather than
        // assuming the four agents are installed.
        assert_eq!(path["available"], false);
        assert!(path["unavailable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("could not be verified"));
    }

    /// Only paths nothing implements may carry an `Unimplemented` enactor. This
    /// keeps the table from drifting back into advertising vapour: adding such a
    /// row is allowed (honest), but it must name a tracking issue, and the
    /// `unimplemented_paths_name_a_tracking_issue` guard above enforces that.
    #[test]
    fn no_path_claims_self_service_crystallization() {
        for path in EVOLUTION_PATHS {
            if path.id != "skill_crystallization" {
                continue;
            }
            assert!(
                matches!(path.enactor, PathEnactor::OperatorPipeline { .. }),
                "crystallization must stay operator-started until an autonomous route ships \
                 (#880) — a SelfTool enactor here would tell agents they can mint skills alone"
            );
        }
    }

    #[test]
    fn amendment_path_availability_tracks_the_tool() {
        let without = path_of(&run(&manifest_with(vec![])), "constitution_amendment");
        assert_eq!(without["available"], false);
        assert!(without["unavailable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("constitution_propose_amendment"));

        let with = path_of(
            &run(&manifest_with(vec![Capability::ConstitutionalProposal {
                patterns: vec!["*".to_string()],
            }])),
            "constitution_amendment",
        );
        assert_eq!(with["available"], true);
        assert_eq!(with["enacted_by"], "self");
        assert_eq!(with["unavailable_reason"], serde_json::Value::Null);
    }

    /// A tool can also be missing from the available set because the manifest
    /// excludes it — a different fact than lacking the capability, and one the
    /// agent must not be told wrong: an excluded tool stays closed however many
    /// capabilities it is granted.
    #[test]
    fn excluded_tool_reports_exclusion_not_a_missing_capability() {
        let mut manifest = manifest_with(vec![Capability::ConstitutionalProposal {
            patterns: vec!["*".to_string()],
        }]);
        manifest.excluded_tools = vec!["constitution_propose_amendment".to_string()];

        let path = path_of(&run(&manifest), "constitution_amendment");
        assert_eq!(path["available"], false);
        let reason = path["unavailable_reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("excluded_tools"),
            "should name the exclusion, got: {reason}"
        );
        assert!(
            !reason.contains("do not hold the capability"),
            "must not blame a capability the agent actually holds, got: {reason}"
        );
    }

    /// The capability-cause wording is only honest while every `SelfTool` row
    /// names a capability-gated tool: granting that capability must flip the
    /// path to available.
    #[test]
    fn self_tool_paths_are_capability_gated() {
        let revision_without = path_of(&run(&manifest_with(vec![])), "agent_revision");
        assert_eq!(revision_without["available"], false);

        let revision_with = path_of(
            &run(&manifest_with(vec![Capability::AgentRevision {
                patterns: vec!["*".to_string()],
            }])),
            "agent_revision",
        );
        assert_eq!(revision_with["available"], true);
        assert_eq!(revision_with["unavailable_reason"], serde_json::Value::Null);
    }

    /// Without a gateway store the tool cannot verify that the pipeline agents
    /// are installed, so it under-claims rather than assuming they are.
    #[test]
    fn pipeline_paths_underclaim_without_a_store() {
        let path = path_of(&run(&manifest_with(vec![])), "lesson_graduation");
        assert_eq!(path["available"], false);
        assert_eq!(path["enacted_by"], "evolution_pipeline");
        assert!(path["unavailable_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("could not be verified"));
        // The mechanism is still named even when unverified — the agent learns
        // which pipeline would carry the lesson.
        assert_eq!(
            path["via"],
            serde_json::json!([
                "memory-curator.default",
                "evolution-steward.default",
                "agent-factory.default"
            ])
        );
    }
}
