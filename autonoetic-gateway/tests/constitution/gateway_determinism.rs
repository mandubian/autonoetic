//! Constitution I-10 capstone: gateway decisions are pure functions of
//! declared capability-set, tool-call input, and recorded session state.
//!
//! This property test intentionally exercises deterministic decision surfaces only:
//! - `PolicyEngine` capability checks
//! - tool-tier classification/filtering
//! - degraded-mode blocked-tool gate
//!
//! No LLM call and no network I/O are involved.


use autonoetic_gateway::policy::{PolicyDecision, PolicyEngine};
use autonoetic_gateway::runtime::prompt_budget::tool_tier;
use autonoetic_gateway::runtime::tool_call_processor::is_degraded_mode_tool_blocked;
use autonoetic_gateway::runtime::tools::ToolTierFilter;
use autonoetic_types::agent::{
    AgentIdentity, AgentManifest, SessionState, ToolTier,
};
use autonoetic_types::capability::Capability;
use proptest::prelude::*;
use crate::support::manifest_builder::TestManifest;

#[derive(Debug, Clone)]
struct GatewayInput {
    sandbox_allow_all: bool,
    sandbox_allow_web: bool,
    sandbox_allow_agent: bool,
    sandbox_allow_approval: bool,
    network_allow_all: bool,
    network_allow_api: bool,
    read_allow_self: bool,
    read_allow_shared: bool,
    write_allow_self: bool,
    code_allow_all: bool,
    code_allow_python: bool,
    code_allow_bash: bool,
    can_spawn: bool,
    can_emergency_stop: bool,
    message_allow_coder: bool,
    revision_allow_coder: bool,
    evaluation_allow_eval: bool,
    scheduler_allow_cron: bool,
    skill_install_allow_github: bool,
    reasoning_audit_allow_coder: bool,
    session_degraded: bool,
    allow_approval_exception: bool,
    allowed_tier_mask: u8,
    tool_name: String,
    command: String,
    host: String,
    read_path: String,
    write_path: String,
    target_agent: String,
    suite_id: String,
    subject_agent_id: String,
    schedule_operation: String,
    install_host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DecisionSummary {
    allowed: bool,
    enforced_rules: Vec<String>,
    threats: Vec<String>,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GatewayDecisionSnapshot {
    tool_gate: DecisionSummary,
    shell_gate: DecisionSummary,
    net_gate: DecisionSummary,
    read_gate: DecisionSummary,
    write_gate: DecisionSummary,
    spawn_gate: DecisionSummary,
    emergency_stop_gate: DecisionSummary,
    message_gate: DecisionSummary,
    revision_gate: DecisionSummary,
    evaluation_gate: DecisionSummary,
    evaluation_publish_gate: DecisionSummary,
    schedule_gate: DecisionSummary,
    install_gate: DecisionSummary,
    reasoning_gate: DecisionSummary,
    computed_tier: ToolTier,
    filter_allows_tier: bool,
    degraded_mode_blocks_tool: bool,
}

fn base_manifest(capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "determinism.gateway.default".to_string(),
            name: "Determinism Gateway".to_string(),
            description: "Property test manifest".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        ..TestManifest::new().build()
    }
}

fn build_capabilities(input: &GatewayInput) -> Vec<Capability> {
    let mut caps = Vec::new();

    let mut sandbox_allowed = Vec::<String>::new();
    if input.sandbox_allow_all {
        sandbox_allowed.push("*".to_string());
    }
    if input.sandbox_allow_web {
        sandbox_allowed.push("web_".to_string());
    }
    if input.sandbox_allow_agent {
        sandbox_allowed.push("agent_".to_string());
    }
    if input.sandbox_allow_approval {
        sandbox_allowed.push("approval_".to_string());
    }
    if !sandbox_allowed.is_empty() {
        caps.push(Capability::SandboxFunctions {
            allowed: sandbox_allowed,
        });
    }

    let mut network_hosts = Vec::<String>::new();
    if input.network_allow_all {
        network_hosts.push("*".to_string());
    }
    if input.network_allow_api {
        network_hosts.push("api.example.com".to_string());
    }
    if !network_hosts.is_empty() {
        caps.push(Capability::NetworkAccess {
            hosts: network_hosts,
        });
    }

    let mut read_scopes = Vec::<String>::new();
    if input.read_allow_self {
        read_scopes.push("self/".to_string());
    }
    if input.read_allow_shared {
        read_scopes.push("shared/".to_string());
    }
    if !read_scopes.is_empty() {
        caps.push(Capability::ReadAccess {
            scopes: read_scopes,
        });
    }

    if input.write_allow_self {
        caps.push(Capability::WriteAccess {
            scopes: vec!["self/".to_string()],
        });
    }

    let mut code_patterns = Vec::<String>::new();
    if input.code_allow_all {
        code_patterns.push("*".to_string());
    }
    if input.code_allow_python {
        code_patterns.push("python3".to_string());
    }
    if input.code_allow_bash {
        code_patterns.push("bash".to_string());
    }
    if !code_patterns.is_empty() {
        caps.push(Capability::CodeExecution {
            patterns: code_patterns,
            commands: Vec::new(),
        });
    }

    if input.can_spawn {
        caps.push(Capability::AgentSpawn {
            max_children: 4,
            max_spawn_depth: 0,
        });
    }
    if input.can_emergency_stop {
        caps.push(Capability::EmergencyStop);
    }
    if input.message_allow_coder {
        caps.push(Capability::AgentMessage {
            patterns: vec!["coder.".to_string()],
        });
    }
    if input.revision_allow_coder {
        caps.push(Capability::AgentRevision {
            patterns: vec!["coder.".to_string()],
        });
    }
    if input.evaluation_allow_eval {
        caps.push(Capability::Evaluation {
            patterns: vec!["eval.".to_string()],
        });
    }
    if input.scheduler_allow_cron {
        caps.push(Capability::SchedulerAccess {
            patterns: vec!["scheduler.cron.".to_string()],
        });
    }
    if input.skill_install_allow_github {
        caps.push(Capability::SkillInstall {
            allowed_sources: vec!["github.com".to_string()],
        });
    }
    if input.reasoning_audit_allow_coder {
        caps.push(Capability::ReasoningAudit {
            targets: vec!["coder.".to_string()],
        });
    }

    caps
}

fn summarize(decision: PolicyDecision) -> DecisionSummary {
    let (threats, reason) = if let Some(analysis) = decision.security_analysis {
        (
            analysis
                .threats
                .into_iter()
                .map(|t| format!("{t:?}"))
                .collect(),
            analysis.reason,
        )
    } else {
        (Vec::new(), None)
    };

    DecisionSummary {
        allowed: decision.allowed,
        enforced_rules: decision
            .enforced_rules
            .into_iter()
            .map(str::to_string)
            .collect(),
        threats,
        reason,
    }
}

fn tier_vector_from_mask(mask: u8) -> Vec<ToolTier> {
    let mut tiers = Vec::new();
    if mask & 0b001 != 0 {
        tiers.push(ToolTier::Core);
    }
    if mask & 0b010 != 0 {
        tiers.push(ToolTier::Workflow);
    }
    if mask & 0b100 != 0 {
        tiers.push(ToolTier::Specialized);
    }
    tiers
}

fn evaluate_gateway_decision(input: &GatewayInput) -> GatewayDecisionSnapshot {
    let manifest = base_manifest(build_capabilities(input));
    let policy = PolicyEngine::new(manifest);
    let filter = ToolTierFilter {
        allowed_tiers: tier_vector_from_mask(input.allowed_tier_mask),
        always_include_approval_tools: input.allow_approval_exception,
        always_include_inspection_tools: false,
        clarification_read_only: false,
        allow_promotion_record_without_specialized_tier: false,
    };
    let session_state = if input.session_degraded {
        SessionState::Degraded
    } else {
        SessionState::Normal
    };

    GatewayDecisionSnapshot {
        tool_gate: summarize(policy.can_invoke_tool(&input.tool_name)),
        shell_gate: summarize(policy.can_exec_shell(&input.command)),
        net_gate: summarize(policy.can_connect_net(&input.host)),
        read_gate: summarize(policy.can_read_path(&input.read_path)),
        write_gate: summarize(policy.can_write_path(&input.write_path)),
        spawn_gate: summarize(policy.can_spawn_agent()),
        emergency_stop_gate: summarize(policy.can_request_emergency_stop()),
        message_gate: summarize(policy.can_message_agent(&input.target_agent)),
        revision_gate: summarize(policy.can_agent_revision(&input.target_agent)),
        evaluation_gate: summarize(
            policy.can_evaluate_suite(&input.suite_id, &input.subject_agent_id),
        ),
        evaluation_publish_gate: summarize(policy.can_evaluate_suite_publish(&input.suite_id)),
        schedule_gate: summarize(policy.can_schedule(&input.schedule_operation)),
        install_gate: summarize(policy.can_install_skill(&input.install_host)),
        reasoning_gate: summarize(policy.can_audit_reasoning(&input.target_agent)),
        computed_tier: tool_tier(&input.tool_name),
        filter_allows_tier: filter.allows(&input.tool_name),
        degraded_mode_blocks_tool: is_degraded_mode_tool_blocked(session_state, &input.tool_name),
    }
}

fn gateway_input_strategy() -> impl Strategy<Value = GatewayInput> {
    let tool_name_strategy = prop_oneof![
        Just("sandbox_exec".to_string()),
        Just("artifact_exec".to_string()),
        Just("web_search".to_string()),
        Just("approval_status".to_string()),
        Just("agent_spawn".to_string()),
        Just("unknown_tool".to_string()),
    ];
    let command_strategy = prop_oneof![
        Just("python3 script.py".to_string()),
        Just("bash -c 'echo hi'".to_string()),
        Just("rm -rf /".to_string()),
        Just("curl https://example.com".to_string()),
        Just("echo ok".to_string()),
    ];
    let host_strategy = prop_oneof![
        Just("api.example.com".to_string()),
        Just("evil.example.com".to_string()),
        Just("*".to_string()),
    ];
    let read_path_strategy = prop_oneof![
        Just("self/data.txt".to_string()),
        Just("shared/notes.md".to_string()),
        Just("other/secret.txt".to_string()),
    ];
    let write_path_strategy = prop_oneof![
        Just("self/out.txt".to_string()),
        Just("shared/forbidden.txt".to_string()),
        Just("tmp/scratch.txt".to_string()),
    ];
    let target_agent_strategy = prop_oneof![
        Just("coder.default".to_string()),
        Just("planner.default".to_string()),
        Just("ops.default".to_string()),
    ];
    let subject_agent_strategy = prop_oneof![
        Just("coder.default".to_string()),
        Just("planner.default".to_string()),
        Just("ops.default".to_string()),
    ];
    let suite_strategy = prop_oneof![
        Just("eval.security".to_string()),
        Just("eval.performance".to_string()),
        Just("qa.quick".to_string()),
    ];
    let scheduler_op_strategy = prop_oneof![
        Just("scheduler.cron.create".to_string()),
        Just("scheduler.cron.pause".to_string()),
        Just("workflow.pause".to_string()),
    ];
    let install_host_strategy = prop_oneof![
        Just("github.com".to_string()),
        Just("example.org".to_string()),
        Just("localhost".to_string()),
    ];

    (
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        ),
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        ),
        (
            0u8..=7u8,
            tool_name_strategy,
            command_strategy,
            host_strategy,
            read_path_strategy,
            write_path_strategy,
            target_agent_strategy,
            suite_strategy,
            subject_agent_strategy,
            scheduler_op_strategy,
            install_host_strategy,
        ),
    )
        .prop_map(
            |(
                (
                    sandbox_allow_all,
                    sandbox_allow_web,
                    sandbox_allow_agent,
                    sandbox_allow_approval,
                    network_allow_all,
                    network_allow_api,
                    read_allow_self,
                    read_allow_shared,
                    write_allow_self,
                    code_allow_all,
                    code_allow_python,
                ),
                (
                    code_allow_bash,
                    can_spawn,
                    can_emergency_stop,
                    message_allow_coder,
                    revision_allow_coder,
                    evaluation_allow_eval,
                    scheduler_allow_cron,
                    skill_install_allow_github,
                    reasoning_audit_allow_coder,
                    session_degraded,
                    allow_approval_exception,
                ),
                (
                    allowed_tier_mask,
                    tool_name,
                    command,
                    host,
                    read_path,
                    write_path,
                    target_agent,
                    suite_id,
                    subject_agent_id,
                    schedule_operation,
                    install_host,
                ),
            )| GatewayInput {
                sandbox_allow_all,
                sandbox_allow_web,
                sandbox_allow_agent,
                sandbox_allow_approval,
                network_allow_all,
                network_allow_api,
                read_allow_self,
                read_allow_shared,
                write_allow_self,
                code_allow_all,
                code_allow_python,
                code_allow_bash,
                can_spawn,
                can_emergency_stop,
                message_allow_coder,
                revision_allow_coder,
                evaluation_allow_eval,
                scheduler_allow_cron,
                skill_install_allow_github,
                reasoning_audit_allow_coder,
                session_degraded,
                allow_approval_exception,
                allowed_tier_mask,
                tool_name,
                command,
                host,
                read_path,
                write_path,
                target_agent,
                suite_id,
                subject_agent_id,
                schedule_operation,
                install_host,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn r_plus_plus_9_gateway_decisions_are_pure_functions(input in gateway_input_strategy()) {
        let first = evaluate_gateway_decision(&input);
        let second = evaluate_gateway_decision(&input);
        let third = evaluate_gateway_decision(&input);
        prop_assert_eq!(&first, &second);
        prop_assert_eq!(&second, &third);
    }
}
