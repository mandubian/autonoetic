//! Composable, targeted prompt-guidance blocks (issue #463).
//!
//! Replaces hand-pasted per-`SKILL.md` doctrine with units of prose that declare
//! *when* they apply. Blocks are contributed from several sources (the builtin
//! set here, tools via #464, roles), collected, filtered against the live turn,
//! ordered by priority, deduped by `id`, and rendered into one section of the
//! system prompt (see `context::compose_system_instructions_full`).
//!
//! Block *content* lives with whatever owns it: tools contribute blocks via
//! `NativeTool::guidance()` (#464), model-family conditions are populated from
//! the manifest (#465), and cross-cutting doctrine not owned by any tool lives
//! in [`builtin_blocks`] (#466, e.g. the clarification principle).

use autonoetic_types::capability::Capability;
use std::collections::HashSet;

/// Predicate describing when a [`GuidanceBlock`] should be injected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuidanceCondition {
    /// Always active.
    Always,
    /// The agent holds a capability of this kind (see [`capability_kind`]).
    Capability(&'static str),
    /// A tool with this exact name is in the active tool set this turn.
    ToolPresent(&'static str),
    /// The routed model's family matches one of these (case-insensitive
    /// substring against the model id, e.g. `"claude"`, `"gpt"`).
    ModelFamily(&'static [&'static str]),
    /// The agent's role matches (e.g. `"coder"`).
    Role(&'static str),
    /// All sub-conditions hold.
    All(Vec<GuidanceCondition>),
    /// Any sub-condition holds.
    Any(Vec<GuidanceCondition>),
    /// The sub-condition does NOT hold (for exclusions).
    Not(Box<GuidanceCondition>),
}

/// A unit of prompt prose with a declarative activation condition.
#[derive(Debug, Clone)]
pub struct GuidanceBlock {
    /// Stable identity; also the dedupe key.
    pub id: &'static str,
    /// The prose injected into the prompt when active.
    pub prose: String,
    /// When this block applies.
    pub when: GuidanceCondition,
    /// Render order — lower first. Foundation-like blocks low, role-specific high.
    pub priority: i32,
}

/// The live-turn facts conditions are evaluated against.
#[derive(Debug, Default, Clone)]
pub struct GuidanceContext<'a> {
    pub capabilities: &'a [Capability],
    pub active_tool_names: &'a [String],
    pub model_family: Option<&'a str>,
    pub role: Option<&'a str>,
}

impl GuidanceCondition {
    fn matches(&self, ctx: &GuidanceContext) -> bool {
        match self {
            GuidanceCondition::Always => true,
            GuidanceCondition::Capability(kind) => {
                ctx.capabilities.iter().any(|c| capability_kind(c) == *kind)
            }
            GuidanceCondition::ToolPresent(name) => {
                ctx.active_tool_names.iter().any(|t| t == name)
            }
            GuidanceCondition::ModelFamily(families) => match ctx.model_family {
                Some(model) => {
                    let model = model.to_ascii_lowercase();
                    families
                        .iter()
                        .any(|f| model.contains(&f.to_ascii_lowercase()))
                }
                None => false,
            },
            GuidanceCondition::Role(role) => ctx.role == Some(*role),
            GuidanceCondition::All(conds) => conds.iter().all(|c| c.matches(ctx)),
            GuidanceCondition::Any(conds) => conds.iter().any(|c| c.matches(ctx)),
            GuidanceCondition::Not(cond) => !cond.matches(ctx),
        }
    }
}

/// Filter `blocks` against `ctx`, order by priority (then id for determinism),
/// dedupe by id, and render the active prose joined by blank lines.
///
/// Returns an empty string when no block is active.
pub fn compose_guidance(blocks: &[GuidanceBlock], ctx: &GuidanceContext) -> String {
    let mut active: Vec<&GuidanceBlock> = blocks.iter().filter(|b| b.when.matches(ctx)).collect();
    active.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(b.id)));

    let mut seen = HashSet::new();
    let mut rendered = Vec::new();
    for block in active {
        if seen.insert(block.id) {
            let prose = block.prose.trim();
            if !prose.is_empty() {
                rendered.push(prose.to_string());
            }
        }
    }
    rendered.join("\n\n")
}

/// Cross-cutting guidance blocks not owned by any single tool (#466). Tool- and
/// role-specific doctrine lives with its tool's `guidance()`; this is for
/// universal doctrine plus role-gated builtins that should not be duplicated
/// across every agent's SKILL.md (e.g. the clarification principle and the
/// planner Sentinel self-correction rule).
pub fn builtin_blocks() -> Vec<GuidanceBlock> {
    vec![
        GuidanceBlock {
            // Universal clarification principle (#466 recurring-section migration).
            // Each role keeps its own *triggers* (what counts as blocked); this is
            // the shared "ask-or-default, don't fabricate" rule.
            id: "clarification.ask_or_default",
            when: GuidanceCondition::Always,
            priority: 5,
            prose: "**Don't fabricate a missing fact.** When you're blocked on something only the \
caller or operator can supply — a missing required parameter, a genuinely ambiguous instruction, or \
conflicting requirements — do not guess and do not spin discovery tools (`agent_list`, \
`workflow_state`, repeated re-reads) to manufacture the answer. Return `clarification_needed` (or use \
`user_ask` if you hold that tool) and end the turn — the reply must still satisfy your declared \
output schema (required fields, types). Otherwise proceed with a sensible, documented default — a \
reasonable default or a clearly-better interpretation does not warrant a round-trip."
                .to_string(),
        },
        GuidanceBlock {
            // D.7b planner doctrine: Sentinel notices are advisory self-correction signals.
            // Only applies to lead planners; other agents rely on their own SKILL.md doctrine
            // or the LoopGuard directly.
            id: "sentinel.self_correct_planner",
            when: GuidanceCondition::Role("planner"),
            priority: 10,
            prose: "**Sentinel notices are advisory — self-correct, don't ask.** When the gateway emits a `sentinel_notice` \
(repetition, ignored feedback, loop pressure, or trajectory divergence), treat it as a hint to replan, \
NOT as a reason to ask the operator. Stop repeating the same action; inspect `workflow_state` and, for \
`planner.collaborative`, `planframe_get` to reconcile with ground truth; change shape (yield for children, \
use `debugger.default`, apply feedback, amend the plan). Use `user_ask` or `clarification_needed` only for \
genuine missing facts the operator must supply. The operator can stop the session via `/sentinel stop` or \
`Ctrl+X` if they want to."
                .to_string(),
        },
    ]
}

/// Stable discriminant string for a capability, matched by
/// [`GuidanceCondition::Capability`]. Exhaustive on purpose: adding a capability
/// forces a decision here (and a chance to give it guidance).
pub fn capability_kind(cap: &Capability) -> &'static str {
    match cap {
        Capability::SandboxFunctions { .. } => "sandbox_functions",
        Capability::ReadAccess { .. } => "read_access",
        Capability::WriteAccess { .. } => "write_access",
        Capability::NetworkAccess { .. } => "network_access",
        Capability::AgentSpawn { .. } => "agent_spawn",
        Capability::AgentMessage { .. } => "agent_message",
        Capability::BackgroundReevaluation { .. } => "background_reevaluation",
        Capability::CodeExecution { .. } => "code_execution",
        Capability::EmergencyStop => "emergency_stop",
        Capability::AgentRevision { .. } => "agent_revision",
        Capability::Evaluation { .. } => "evaluation",
        Capability::ApprovalQueue { .. } => "approval_queue",
        Capability::SchedulerSignal { .. } => "scheduler_signal",
        Capability::CredentialAccess { .. } => "credential_access",
        Capability::UserProfileAccess { .. } => "user_profile_access",
        Capability::SchedulerAccess { .. } => "scheduler_access",
        Capability::SkillInstall { .. } => "skill_install",
        Capability::ConstitutionalProposal { .. } => "constitutional_proposal",
        Capability::ReasoningAudit { .. } => "reasoning_audit",
        Capability::BudgetNoPriceAvailableAllow => "budget_no_price_available_allow",
        Capability::GithubIssueCreate { .. } => "github_issue_create",
        Capability::SecurityRedTeam => "security_red_team",
        Capability::CapsuleExport => "capsule_export",
        Capability::SelfCapsuleExport => "self_capsule_export",
        Capability::WikiContribute => "wiki_contribute",
        Capability::PlanFrameAccess { .. } => "plan_frame_access",
        Capability::PromoteWith { .. } => "promote_with",
        Capability::GateDecider { .. } => "gate_decider",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: &'static str, when: GuidanceCondition, priority: i32) -> GuidanceBlock {
        GuidanceBlock { id, prose: id.to_string(), when, priority }
    }

    fn write_access() -> Vec<Capability> {
        vec![Capability::WriteAccess { scopes: vec!["*".into()] }]
    }

    #[test]
    fn always_matches() {
        let ctx = GuidanceContext::default();
        assert_eq!(compose_guidance(&[block("a", GuidanceCondition::Always, 0)], &ctx), "a");
    }

    #[test]
    fn capability_gates() {
        let caps = write_access();
        let ctx = GuidanceContext { capabilities: &caps, ..Default::default() };
        let blocks = vec![
            block("w", GuidanceCondition::Capability("write_access"), 0),
            block("n", GuidanceCondition::Capability("network_access"), 0),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx), "w");
    }

    #[test]
    fn tool_present_gates() {
        let tools = vec!["content_patch".to_string()];
        let ctx = GuidanceContext { active_tool_names: &tools, ..Default::default() };
        let blocks = vec![
            block("p", GuidanceCondition::ToolPresent("content_patch"), 0),
            block("x", GuidanceCondition::ToolPresent("missing_tool"), 0),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx), "p");
    }

    #[test]
    fn model_family_substring_case_insensitive() {
        let ctx = GuidanceContext { model_family: Some("claude-opus-4-8"), ..Default::default() };
        let claude = block("c", GuidanceCondition::ModelFamily(&["claude", "sonnet"]), 0);
        let gpt = block("g", GuidanceCondition::ModelFamily(&["gpt"]), 0);
        assert_eq!(compose_guidance(&[claude, gpt], &ctx), "c");
    }

    #[test]
    fn model_family_none_never_matches() {
        let ctx = GuidanceContext::default();
        let b = block("c", GuidanceCondition::ModelFamily(&["claude"]), 0);
        assert_eq!(compose_guidance(&[b], &ctx), "");
    }

    #[test]
    fn role_gates() {
        let ctx = GuidanceContext { role: Some("coder"), ..Default::default() };
        let blocks = vec![
            block("c", GuidanceCondition::Role("coder"), 0),
            block("a", GuidanceCondition::Role("auditor"), 0),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx), "c");
    }

    #[test]
    fn all_and_any_compose() {
        let caps = write_access();
        let tools = vec!["content_patch".to_string()];
        let ctx = GuidanceContext { capabilities: &caps, active_tool_names: &tools, ..Default::default() };
        let both = GuidanceCondition::All(vec![
            GuidanceCondition::Capability("write_access"),
            GuidanceCondition::ToolPresent("content_patch"),
        ]);
        let either = GuidanceCondition::Any(vec![
            GuidanceCondition::Capability("network_access"),
            GuidanceCondition::ToolPresent("content_patch"),
        ]);
        let neither = GuidanceCondition::All(vec![
            GuidanceCondition::Capability("write_access"),
            GuidanceCondition::ToolPresent("missing"),
        ]);
        assert_eq!(compose_guidance(&[block("both", both, 0)], &ctx), "both");
        assert_eq!(compose_guidance(&[block("either", either, 0)], &ctx), "either");
        assert_eq!(compose_guidance(&[block("neither", neither, 0)], &ctx), "");
    }

    #[test]
    fn orders_by_priority_then_id() {
        let ctx = GuidanceContext::default();
        let blocks = vec![
            block("z", GuidanceCondition::Always, 10),
            block("a", GuidanceCondition::Always, 10),
            block("first", GuidanceCondition::Always, -5),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx), "first\n\na\n\nz");
    }

    #[test]
    fn dedupes_by_id_keeping_one() {
        let ctx = GuidanceContext::default();
        let blocks = vec![
            GuidanceBlock { id: "dup", prose: "low".into(), when: GuidanceCondition::Always, priority: 0 },
            GuidanceBlock { id: "dup", prose: "high".into(), when: GuidanceCondition::Always, priority: 5 },
        ];
        // Lower priority wins (rendered first, dedupe keeps first).
        assert_eq!(compose_guidance(&blocks, &ctx), "low");
    }

    #[test]
    fn empty_when_nothing_active() {
        let ctx = GuidanceContext::default();
        let b = block("n", GuidanceCondition::Capability("network_access"), 0);
        assert_eq!(compose_guidance(&[b], &ctx), "");
    }

    #[test]
    fn builtin_clarification_block_is_always_active() {
        // The clarification principle (#466) is an Always builtin → renders for
        // any agent, even with no capabilities/tools.
        let out = compose_guidance(&builtin_blocks(), &GuidanceContext::default());
        assert!(out.contains("Don't fabricate a missing fact"), "got: {out:?}");
    }

    #[test]
    fn not_negates() {
        let caps = write_access();
        let ctx = GuidanceContext { capabilities: &caps, ..Default::default() };
        // write_access present → Not(write_access) is false → excluded.
        let a = block(
            "a",
            GuidanceCondition::Not(Box::new(GuidanceCondition::Capability("write_access"))),
            0,
        );
        assert_eq!(compose_guidance(&[a], &ctx), "");
        // network_access absent → Not(network_access) is true → included.
        let b = block(
            "b",
            GuidanceCondition::Not(Box::new(GuidanceCondition::Capability("network_access"))),
            0,
        );
        assert_eq!(compose_guidance(&[b], &ctx), "b");
    }

    #[test]
    fn approval_block_excluded_for_promotion_gate_roles() {
        // P-3.10: promotion-gate agents can't get network approval, so the
        // exec approval-continuation block must not fire for them.
        let block = vec![crate::runtime::tools::sandbox::exec_approval_continuation_block()];
        let tools = vec!["artifact_exec".to_string()];
        let coder = GuidanceContext { active_tool_names: &tools, role: Some("coder"), ..Default::default() };
        assert!(compose_guidance(&block, &coder).contains("Approval continuation"));
        let utr = GuidanceContext { active_tool_names: &tools, role: Some("unit_test_runner"), ..Default::default() };
        assert_eq!(compose_guidance(&block, &utr), "");
    }
}
