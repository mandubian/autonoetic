//! Composable, targeted prompt-guidance blocks (issue #463).
//!
//! Replaces hand-pasted per-`SKILL.md` doctrine with units of prose that declare
//! *when* they apply. Blocks are contributed from several sources (the builtin
//! set here, tools via #464, roles), collected, filtered against the live turn,
//! deduped by `id`, and rendered into the system prompt (see
//! `context::compose_system_instructions_full`).
//!
//! Blocks render into **two** sections, by when they can activate: blocks fixed
//! at spawn go into the standing Guidance section, and [`GuidanceCondition::Phase`]
//! blocks — the only ones that can appear mid-session — go into a tail section at
//! the end of the cache prefix. See [`ComposedGuidance`] for why that split is
//! load-bearing rather than cosmetic.
//!
//! Block *content* lives with whatever owns it: tools contribute blocks via
//! `NativeTool::guidance()` (#464), model-family conditions are populated from
//! the manifest (#465), and cross-cutting doctrine not owned by any tool lives
//! in [`builtin_blocks`] (#466, e.g. the clarification principle).

use autonoetic_types::capability::Capability;
use serde::{Deserialize, Serialize};
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
    /// The session has reached a phase (see [`SessionPhase`]) — e.g.
    /// [`PHASE_ARTIFACT_BUILT`]. This is the only condition whose truth changes
    /// *during* a session; every other condition is fixed at spawn.
    Phase(&'static str),
    /// All sub-conditions hold.
    All(Vec<GuidanceCondition>),
    /// Any sub-condition holds.
    Any(Vec<GuidanceCondition>),
    /// The sub-condition does NOT hold (for exclusions).
    Not(Box<GuidanceCondition>),
}

/// An artifact exists in this workflow — built by this agent, or observed in a
/// tool result (a child's `artifact_ref`, `workflow_state.reuse_guards`).
pub const PHASE_ARTIFACT_BUILT: &str = "artifact_built";
/// At least one federation gate verdict has been recorded this session.
pub const PHASE_GATE_VERDICT_RECORDED: &str = "gate_verdict_recorded";
/// A candidate revision has been seeded (`agent_revision_create`).
pub const PHASE_REVISION_SEEDED: &str = "revision_seeded";
/// This agent has spawned at least one child.
pub const PHASE_CHILD_SPAWNED: &str = "child_spawned";
/// A credential has been configured this session.
pub const PHASE_CREDENTIAL_CONFIGURED: &str = "credential_configured";

/// Minimum `priority` for any block carrying a [`GuidanceCondition::Phase`].
///
/// Placement is what actually keeps the prompt cache intact — phase-gated blocks
/// render in their own tail section at the end of the cache prefix
/// ([`ComposedGuidance`]), and *within* that tail they are ordered by fact
/// arrival, not priority.
///
/// This floor is therefore a **belt-and-braces guard, not the mechanism**: it
/// keeps priorities legible (a phase-gated block never looks like it competes
/// with standing doctrine) and makes an accidental mis-gating visible in review.
///
/// Enforced by `phase_gated_blocks_render_after_unconditional_ones`.
pub const PHASE_GATED_PRIORITY_FLOOR: i32 = 100;

/// Monotonic, mechanically-derived record of how far a session has progressed.
///
/// This is the axis [`GuidanceCondition`] was missing. Capabilities, tools,
/// model and role are all decided at spawn, so a block gated only on them is
/// effectively static: prose that *might* matter at turn 40 is paid for at
/// turn 1. `SessionPhase` lets a block say "not until the work reaches this
/// point", which is what actually distinguishes a 32k prompt from a 14k one.
///
/// **Derivation is a pure function of gateway-observed tool results** (P-5.14,
/// Lawful Executor): agent prose never sets a fact. Facts are monotonic —
/// never retracted — so a block cannot flicker in and out across turns and
/// invalidate the provider's prompt cache more than once.
/// Facts are held in **arrival order**, not sorted. Arrival order is what makes
/// the phase tail append-only: a block earned later must render after one earned
/// earlier, or it inserts into the middle of the cached prefix and re-caches
/// everything downstream. (Serialization is a JSON array either way, so this is
/// wire-compatible with the sorted-set form.) It also makes the
/// `autonoetic::session_phase` traces read chronologically, which is what you
/// want when reconstructing a real session.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPhase {
    #[serde(default)]
    facts: Vec<String>,
}

impl SessionPhase {
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    pub fn has(&self, fact: &str) -> bool {
        self.facts.iter().any(|f| f == fact)
    }

    /// Arrival index of `fact`, or `None` if it has not been earned. Lower means
    /// earned earlier; this is the sort key for the phase guidance tail.
    pub fn arrival_index(&self, fact: &str) -> Option<usize> {
        self.facts.iter().position(|f| f == fact)
    }

    /// Record a fact. Returns `true` when it was newly added. Monotonic — a fact
    /// already present keeps its original arrival position.
    pub fn insert(&mut self, fact: &str) -> bool {
        if self.has(fact) {
            return false;
        }
        self.facts.push(fact.to_string());
        true
    }

    /// Facts in the order they were earned.
    pub fn facts(&self) -> impl Iterator<Item = &str> {
        self.facts.iter().map(String::as_str)
    }

    /// Derive phase facts from one executed tool result. Returns the facts newly
    /// added by this observation (empty in the overwhelmingly common case).
    ///
    /// Two mechanical sources, both gateway-observed:
    ///
    /// 1. **The agent's own successful action** — `artifact_build` returning an
    ///    explicit `"ok": true` means an artifact exists. See [`fact_for_tool`].
    /// 2. **Evidence carried in any result** — a planner never calls
    ///    `artifact_build` itself (its coder child does), so tool-name mapping
    ///    alone would leave a lead agent permanently pre-phase. Any result
    ///    carrying a non-empty `artifact_ref`, or `reuse_guards.has_coder_artifact`,
    ///    is proof the workflow has an artifact regardless of who made it.
    pub fn observe(&mut self, tool_name: &str, result_json: &str) -> Vec<&'static str> {
        let mut added = Vec::new();

        // Fail closed on anything we cannot read. A result the gateway can't
        // parse is not gateway-observed state, and treating it as success would
        // let a malformed or truncated payload advance the phase.
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result_json) else {
            return added;
        };

        // A result that explicitly failed proves nothing about progress.
        let failed = parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
            || parsed.get("error_type").is_some();
        if failed {
            return added;
        }

        // The agent's own action counts only under an explicit success envelope.
        // Every tool in `fact_for_tool` emits `"ok": true` on success, so
        // requiring it strands nothing while keeping the derivation sound.
        if parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            if let Some(fact) = fact_for_tool(tool_name) {
                if self.insert(fact) {
                    added.push(fact);
                }
            }
        }

        // Evidence carried by a non-failed result from an artifact-domain tool:
        // `workflow_state` reporting a prior artifact is proof regardless of who
        // built it. This deliberately does not require `ok: true` — the
        // evidence-bearing results (child terminal state, reuse guards) do not
        // all carry an envelope — but it is gated on the call not having failed,
        // so an error that merely *names* an artifact_ref cannot advance the
        // phase, and on [`tool_emits_artifact_evidence`], so an unrelated tool
        // that happens to echo the key cannot either.
        if tool_emits_artifact_evidence(tool_name)
            && json_has_artifact_evidence(&parsed, 0)
            && self.insert(PHASE_ARTIFACT_BUILT)
        {
            added.push(PHASE_ARTIFACT_BUILT);
        }

        added
    }
}

/// Tool name → the phase fact a successful call proves. Exhaustive by intent:
/// only tools whose success is *unambiguous* evidence of progress belong here.
pub fn fact_for_tool(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "artifact_build" => Some(PHASE_ARTIFACT_BUILT),
        "promotion_record" => Some(PHASE_GATE_VERDICT_RECORDED),
        "agent_revision_create" | "agent_revision_create_from_intent" => {
            Some(PHASE_REVISION_SEEDED)
        }
        "agent_spawn" => Some(PHASE_CHILD_SPAWNED),
        "credential_setup" => Some(PHASE_CREDENTIAL_CONFIGURED),
        _ => None,
    }
}

/// Whether a tool's results may be *read as evidence* that an artifact exists.
///
/// The content scan (`json_has_artifact_evidence`) is what lets a delegating
/// agent earn `artifact_built` from its child's work, but scanning every tool's
/// output makes the fact hostage to any future tool that happens to echo an
/// `artifact_ref` for unrelated reasons — and by then the fact is a contract
/// that many gated blocks depend on. This allowlist is the audited surface: every
/// tool here operates *on* artifacts, so a non-failed result naming one is
/// genuine proof the workflow has an artifact.
///
/// The bias is deliberate. A missing block is recoverable — the agent still has
/// the tool signature, and tool errors carry repair hints. A phantom block is
/// paid by every session that trips it, forever.
///
/// Note this is about *reading evidence*, not about advancing on the tool's own
/// success — that is [`fact_for_tool`], which is narrower still.
pub fn tool_emits_artifact_evidence(tool_name: &str) -> bool {
    const EVIDENCE_PREFIXES: &[&str] = &[
        "artifact_",
        "promotion_",
        "federation_",
        "agent_revision_",
        "workflow_",
        "workbench_",
    ];
    const EVIDENCE_EXACT: &[&str] = &["resolve", "sandbox_exec"];

    EVIDENCE_PREFIXES.iter().any(|p| tool_name.starts_with(p))
        || EVIDENCE_EXACT.contains(&tool_name)
}

/// Depth-bounded scan for proof that an artifact exists somewhere in a result.
/// Bounded so a pathological nested result can't make this quadratic.
fn json_has_artifact_evidence(value: &serde_json::Value, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    match value {
        serde_json::Value::Object(map) => {
            if map
                .get("artifact_ref")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
            {
                return true;
            }
            if map
                .get("has_coder_artifact")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                return true;
            }
            map.values().any(|v| json_has_artifact_evidence(v, depth + 1))
        }
        serde_json::Value::Array(items) => {
            items.iter().any(|v| json_has_artifact_evidence(v, depth + 1))
        }
        _ => false,
    }
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
    /// How far the session has progressed. `None` for composition paths with no
    /// live session (bootstrap, static analysis) — `Phase` then never matches,
    /// so a phase-gated block stays out of the prompt.
    pub phase: Option<&'a SessionPhase>,
}

impl GuidanceCondition {
    /// Whether this condition (or any sub-condition) depends on session phase —
    /// i.e. whether the block can enter the prompt mid-session.
    pub fn is_phase_gated(&self) -> bool {
        match self {
            GuidanceCondition::Phase(_) => true,
            GuidanceCondition::All(conds) | GuidanceCondition::Any(conds) => {
                conds.iter().any(GuidanceCondition::is_phase_gated)
            }
            GuidanceCondition::Not(cond) => cond.is_phase_gated(),
            _ => false,
        }
    }

    /// The arrival index at which this condition's *phase* requirement became
    /// satisfiable, given `phase`. `None` for conditions with no phase component.
    ///
    /// `All` takes the max (the block appears when its LAST required fact lands);
    /// `Any` takes the min (its FIRST). Getting this right is what keeps the tail
    /// append-only: sorting a block by anything other than the moment it actually
    /// appeared can place it ahead of a block already rendered.
    fn activation_index(&self, phase: &SessionPhase) -> Option<usize> {
        match self {
            GuidanceCondition::Phase(fact) => phase.arrival_index(fact),
            GuidanceCondition::All(conds) => {
                conds.iter().filter_map(|c| c.activation_index(phase)).max()
            }
            GuidanceCondition::Any(conds) => {
                conds.iter().filter_map(|c| c.activation_index(phase)).min()
            }
            // A negated phase condition is true from turn 1 and goes false later
            // — non-monotonic, and excluded from the tail by `is_phase_gated`
            // callers ordering it last. It has no meaningful arrival moment.
            GuidanceCondition::Not(_) => None,
            _ => None,
        }
    }

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
            GuidanceCondition::Phase(fact) => {
                ctx.phase.is_some_and(|phase| phase.has(fact))
            }
            GuidanceCondition::All(conds) => conds.iter().all(|c| c.matches(ctx)),
            GuidanceCondition::Any(conds) => conds.iter().any(|c| c.matches(ctx)),
            GuidanceCondition::Not(cond) => !cond.matches(ctx),
        }
    }
}

/// The two guidance sections, which occupy different positions in the prompt.
///
/// They are separate because of where they sit in the **cache prefix**, not for
/// presentation. `compose_system_instructions_full` renders `standing` early
/// (foundation → guidance → … → SKILL.md → output contract) and `phase_tail`
/// **last**, at the very end of the cacheable prefix.
///
/// That placement is the whole point. A block that enters the prompt mid-session
/// invalidates every cached byte after it — and the standing guidance section is
/// followed by the agent's entire `SKILL.md` (~11k tokens for the planner) plus
/// the output contract. Rendering earned blocks in the standing section would
/// re-cache all of that on every fact, no matter how they were ordered *within*
/// the section. In the tail, each fact is a pure append.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ComposedGuidance {
    /// Blocks whose activation is fixed at spawn — stable for the whole session.
    pub standing: String,
    /// Blocks earned during the session, in fact-arrival order. Rendered at the
    /// end of the cache prefix so each new fact extends it rather than splitting it.
    pub phase_tail: String,
}

impl ComposedGuidance {
    pub fn is_empty(&self) -> bool {
        self.standing.is_empty() && self.phase_tail.is_empty()
    }
}

/// Filter `blocks` against `ctx`, dedupe by id, and render them into the two
/// sections of [`ComposedGuidance`].
///
/// `standing` is ordered by priority then id (deterministic, stable all session).
/// `phase_tail` is ordered by **fact-arrival index** then id, so a block earned
/// at fact 3 always renders after one earned at fact 1 — regardless of their
/// priorities or ids, which is what append-only requires.
pub fn compose_guidance(blocks: &[GuidanceBlock], ctx: &GuidanceContext) -> ComposedGuidance {
    let empty_phase = SessionPhase::default();
    let phase = ctx.phase.unwrap_or(&empty_phase);

    let mut standing: Vec<&GuidanceBlock> = Vec::new();
    let mut earned: Vec<(usize, &GuidanceBlock)> = Vec::new();
    let mut seen = HashSet::new();

    let mut active: Vec<&GuidanceBlock> = blocks.iter().filter(|b| b.when.matches(ctx)).collect();
    active.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(b.id)));

    for block in active {
        if !seen.insert(block.id) {
            continue;
        }
        if block.when.is_phase_gated() {
            // `usize::MAX` only for a phase-gated block with no resolvable
            // arrival moment (a negated phase condition); it sorts last.
            let at = block.when.activation_index(phase).unwrap_or(usize::MAX);
            earned.push((at, block));
        } else {
            standing.push(block);
        }
    }

    earned.sort_by(|(a_at, a), (b_at, b)| a_at.cmp(b_at).then_with(|| a.id.cmp(b.id)));

    let render = |bs: Vec<&GuidanceBlock>| -> String {
        bs.into_iter()
            .map(|b| b.prose.trim())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    ComposedGuidance {
        standing: render(standing),
        phase_tail: render(earned.into_iter().map(|(_, b)| b).collect()),
    }
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

/// `(fingerprint, owning guidance block — where the doctrine lives now)`.
/// Doctrine that has been centralized into tool-contributed guidance blocks must
/// NOT be re-pasted into individual `SKILL.md` files (repo-authored or
/// runtime-born via `create_from_intent`). Single source of truth shared by the
/// CI regression guard (`tests/skill_doctrine_guard.rs`) and the create-time
/// scan (`install_contract::scan_body_for_migrated_doctrine`, RFC #799 F.4b).
/// Each phrase was verified absent from every SKILL.md at migration time.
pub const MIGRATED_DOCTRINE_FINGERPRINTS: &[(&str, &str)] = &[
    ("Forbidden shell commands", "sandbox.forbidden_commands (sandbox_exec.guidance)"),
    (
        "requires both `name` and `content`",
        "content.write_protocol (content_write.guidance)",
    ),
    (
        "alternate names like `outcome`",
        "promotion.record_protocol (promotion_record.guidance)",
    ),
    (
        "do not invent or guess",
        "exec.approval_continuation (sandbox_exec/artifact_exec.guidance)",
    ),
    (
        "never restart from scratch",
        "resumption.workflow_state_first (workflow_state.guidance)",
    ),
    (
        "warrant a round-trip",
        "clarification.ask_or_default (builtin block)",
    ),
    (
        "wrap JSON in markdown code fences",
        "the io.returns Output Contract renderer (context.rs) — declare io.returns instead",
    ),
    (
        "Return a single raw JSON object",
        "the io.returns Output Contract renderer (context.rs) — declare io.returns instead",
    ),
    // Centralized into foundation_core.md §7 — the rights/self-describe/community
    // doctrine every agent already receives. Keep these specific enough that they
    // only match the centralized phrasing, not legitimate role-specific wording.
    (
        "Your headline rights, in force every turn",
        "foundation_core.md §7 (the constitution is your contract)",
    ),
    (
        "are one call away: `self_describe()`",
        "foundation_core.md §7 (self_describe nudge)",
    ),
    (
        "its rights bind the gateway as its rules bind you",
        "foundation_core.md §7 (community / social-contract framing)",
    ),
    (
        "standing witness contract",
        "the io.returns Output Contract renderer (context.rs) — `anomalies` is gateway-injected (RFC C.2, #770), declare it in your own schema only if you need custom fields",
    ),
];

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
        Capability::ArtifactExecution => "artifact_execution",
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
        Capability::AnomalyAdjudicate { .. } => "anomaly_adjudicate",
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
        assert_eq!(compose_guidance(&[block("a", GuidanceCondition::Always, 0)], &ctx).standing, "a");
    }

    #[test]
    fn capability_gates() {
        let caps = write_access();
        let ctx = GuidanceContext { capabilities: &caps, ..Default::default() };
        let blocks = vec![
            block("w", GuidanceCondition::Capability("write_access"), 0),
            block("n", GuidanceCondition::Capability("network_access"), 0),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx).standing, "w");
    }

    #[test]
    fn tool_present_gates() {
        let tools = vec!["content_patch".to_string()];
        let ctx = GuidanceContext { active_tool_names: &tools, ..Default::default() };
        let blocks = vec![
            block("p", GuidanceCondition::ToolPresent("content_patch"), 0),
            block("x", GuidanceCondition::ToolPresent("missing_tool"), 0),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx).standing, "p");
    }

    #[test]
    fn model_family_substring_case_insensitive() {
        let ctx = GuidanceContext { model_family: Some("claude-opus-4-8"), ..Default::default() };
        let claude = block("c", GuidanceCondition::ModelFamily(&["claude", "sonnet"]), 0);
        let gpt = block("g", GuidanceCondition::ModelFamily(&["gpt"]), 0);
        assert_eq!(compose_guidance(&[claude, gpt], &ctx).standing, "c");
    }

    #[test]
    fn model_family_none_never_matches() {
        let ctx = GuidanceContext::default();
        let b = block("c", GuidanceCondition::ModelFamily(&["claude"]), 0);
        assert_eq!(compose_guidance(&[b], &ctx).standing, "");
    }

    #[test]
    fn role_gates() {
        let ctx = GuidanceContext { role: Some("coder"), ..Default::default() };
        let blocks = vec![
            block("c", GuidanceCondition::Role("coder"), 0),
            block("a", GuidanceCondition::Role("auditor"), 0),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx).standing, "c");
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
        assert_eq!(compose_guidance(&[block("both", both, 0)], &ctx).standing, "both");
        assert_eq!(compose_guidance(&[block("either", either, 0)], &ctx).standing, "either");
        assert_eq!(compose_guidance(&[block("neither", neither, 0)], &ctx).standing, "");
    }

    #[test]
    fn orders_by_priority_then_id() {
        let ctx = GuidanceContext::default();
        let blocks = vec![
            block("z", GuidanceCondition::Always, 10),
            block("a", GuidanceCondition::Always, 10),
            block("first", GuidanceCondition::Always, -5),
        ];
        assert_eq!(compose_guidance(&blocks, &ctx).standing, "first\n\na\n\nz");
    }

    #[test]
    fn dedupes_by_id_keeping_one() {
        let ctx = GuidanceContext::default();
        let blocks = vec![
            GuidanceBlock { id: "dup", prose: "low".into(), when: GuidanceCondition::Always, priority: 0 },
            GuidanceBlock { id: "dup", prose: "high".into(), when: GuidanceCondition::Always, priority: 5 },
        ];
        // Lower priority wins (rendered first, dedupe keeps first).
        assert_eq!(compose_guidance(&blocks, &ctx).standing, "low");
    }

    #[test]
    fn empty_when_nothing_active() {
        let ctx = GuidanceContext::default();
        let b = block("n", GuidanceCondition::Capability("network_access"), 0);
        assert_eq!(compose_guidance(&[b], &ctx).standing, "");
    }

    #[test]
    fn builtin_clarification_block_is_always_active() {
        // The clarification principle (#466) is an Always builtin → renders for
        // any agent, even with no capabilities/tools.
        let out = compose_guidance(&builtin_blocks(), &GuidanceContext::default()).standing;
        assert!(out.contains("Don't fabricate a missing fact"), "got: {out:?}");
    }

    #[test]
    fn phase_condition_requires_the_fact() {
        let mut phase = SessionPhase::default();
        let b = block("p", GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT), 0);

        // No phase context at all (bootstrap/static paths) → never matches.
        assert!(compose_guidance(&[b.clone()], &GuidanceContext::default()).is_empty());

        let ctx = GuidanceContext { phase: Some(&phase), ..Default::default() };
        assert!(compose_guidance(&[b.clone()], &ctx).is_empty());

        phase.insert(PHASE_ARTIFACT_BUILT);
        let ctx = GuidanceContext { phase: Some(&phase), ..Default::default() };
        let composed = compose_guidance(&[b], &ctx);
        // Earned blocks land in the tail, never the standing section.
        assert_eq!(composed.phase_tail, "p");
        assert_eq!(composed.standing, "");
    }

    #[test]
    fn phase_tail_is_ordered_by_fact_arrival_not_id_or_priority() {
        // The append-only property. `zeta` is earned first and `alpha` second, so
        // `zeta` must render first — even though `alpha` sorts earlier by id.
        // Ordering by id (the previous tie-break) would insert `alpha` ahead of
        // an already-rendered `zeta` and re-cache everything downstream.
        let mut phase = SessionPhase::default();
        phase.insert(PHASE_CHILD_SPAWNED);
        phase.insert(PHASE_ARTIFACT_BUILT);

        let zeta = block(
            "zeta",
            GuidanceCondition::Phase(PHASE_CHILD_SPAWNED),
            PHASE_GATED_PRIORITY_FLOOR,
        );
        let alpha = block(
            "alpha",
            GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT),
            PHASE_GATED_PRIORITY_FLOOR,
        );

        let ctx = GuidanceContext { phase: Some(&phase), ..Default::default() };
        assert_eq!(
            compose_guidance(&[alpha, zeta], &ctx).phase_tail,
            "zeta\n\nalpha"
        );
    }

    #[test]
    fn phase_tail_grows_by_appending_as_facts_land() {
        // The property the whole tail placement exists for: the tail rendered at
        // fact N must be a prefix of the tail rendered at fact N+1.
        let first = block(
            "a_first",
            GuidanceCondition::Phase(PHASE_CHILD_SPAWNED),
            PHASE_GATED_PRIORITY_FLOOR,
        );
        let second = block(
            "b_second",
            GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT),
            PHASE_GATED_PRIORITY_FLOOR,
        );
        let blocks = [first, second];

        let mut phase = SessionPhase::default();
        phase.insert(PHASE_CHILD_SPAWNED);
        let ctx = GuidanceContext { phase: Some(&phase), ..Default::default() };
        let after_one = compose_guidance(&blocks, &ctx).phase_tail;

        phase.insert(PHASE_ARTIFACT_BUILT);
        let ctx = GuidanceContext { phase: Some(&phase), ..Default::default() };
        let after_two = compose_guidance(&blocks, &ctx).phase_tail;

        assert!(
            after_two.starts_with(&after_one),
            "tail must extend, not rewrite: {after_one:?} -> {after_two:?}"
        );
    }

    #[test]
    fn all_condition_activates_at_its_last_fact() {
        // An All(...) block appears when its LAST required fact lands, so that is
        // its arrival moment. Ordering it by the first would place it ahead of
        // blocks already rendered between the two facts.
        let mut phase = SessionPhase::default();
        phase.insert(PHASE_CHILD_SPAWNED);
        phase.insert(PHASE_ARTIFACT_BUILT);

        let both = GuidanceCondition::All(vec![
            GuidanceCondition::Phase(PHASE_CHILD_SPAWNED),
            GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT),
        ]);
        assert_eq!(both.activation_index(&phase), Some(1));

        let either = GuidanceCondition::Any(vec![
            GuidanceCondition::Phase(PHASE_CHILD_SPAWNED),
            GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT),
        ]);
        assert_eq!(either.activation_index(&phase), Some(0));
    }

    #[test]
    fn facts_keep_arrival_order_and_original_position_on_reinsert() {
        let mut phase = SessionPhase::default();
        phase.insert(PHASE_ARTIFACT_BUILT);
        phase.insert(PHASE_CHILD_SPAWNED);
        assert_eq!(phase.arrival_index(PHASE_ARTIFACT_BUILT), Some(0));
        assert_eq!(phase.arrival_index(PHASE_CHILD_SPAWNED), Some(1));

        // Monotonic: re-observing an existing fact must not move it, or the tail
        // would reorder and break append-only.
        assert!(!phase.insert(PHASE_ARTIFACT_BUILT));
        assert_eq!(phase.arrival_index(PHASE_ARTIFACT_BUILT), Some(0));
        assert_eq!(phase.facts().collect::<Vec<_>>(), vec![PHASE_ARTIFACT_BUILT, PHASE_CHILD_SPAWNED]);
    }

    #[test]
    fn evidence_scan_is_limited_to_artifact_domain_tools() {
        // A tool outside the artifact domain must not advance the fact even if it
        // echoes an artifact_ref: the fact is a contract many gated blocks will
        // depend on, and a phantom block is paid by every session that trips it.
        let mut phase = SessionPhase::default();
        let looks_like_evidence = r#"{"ok":true,"revisions":[{"artifact_ref":"ar.existing"}]}"#;
        assert!(phase.observe("agent_inspect", looks_like_evidence).is_empty());
        assert!(phase.observe("execution_search", looks_like_evidence).is_empty());
        assert!(phase.observe("knowledge_search", looks_like_evidence).is_empty());
        assert!(!phase.has(PHASE_ARTIFACT_BUILT));

        // The audited artifact-domain surface still advances it.
        assert!(tool_emits_artifact_evidence("workflow_state"));
        assert!(tool_emits_artifact_evidence("resolve"));
        assert!(tool_emits_artifact_evidence("promotion_query"));
        assert!(tool_emits_artifact_evidence("artifact_inspect"));
        assert!(!tool_emits_artifact_evidence("agent_inspect"));
        assert_eq!(
            phase.observe("workflow_state", looks_like_evidence),
            vec![PHASE_ARTIFACT_BUILT]
        );
    }

    #[test]
    fn observe_maps_successful_tool_to_fact() {
        let mut phase = SessionPhase::default();
        assert_eq!(
            phase.observe("artifact_build", r#"{"ok":true,"artifact_ref":"ar.abc"}"#),
            vec![PHASE_ARTIFACT_BUILT]
        );
        // Monotonic: a second observation adds nothing.
        assert!(phase
            .observe("artifact_build", r#"{"ok":true,"artifact_ref":"ar.abc"}"#)
            .is_empty());
    }

    #[test]
    fn observe_ignores_failed_results() {
        let mut phase = SessionPhase::default();
        assert!(phase.observe("artifact_build", r#"{"ok":false}"#).is_empty());
        assert!(phase
            .observe("promotion_record", r#"{"error_type":"validation"}"#)
            .is_empty());
        assert!(!phase.has(PHASE_ARTIFACT_BUILT));
        assert!(!phase.has(PHASE_GATE_VERDICT_RECORDED));
    }

    #[test]
    fn observe_fails_closed_on_unreadable_or_unenveloped_results() {
        // Derivation claims to be a pure function of gateway-observed state, so
        // anything the gateway cannot actually read must advance nothing.
        let mut phase = SessionPhase::default();
        assert!(phase.observe("artifact_build", "not json at all").is_empty());
        assert!(phase.observe("artifact_build", "").is_empty());
        assert!(phase.observe("artifact_build", "truncated {\"ok\": tr").is_empty());
        // Parseable but with no success envelope — still not proof.
        assert!(phase.observe("artifact_build", r#"{"status":"maybe"}"#).is_empty());
        assert!(!phase.has(PHASE_ARTIFACT_BUILT));

        // The explicit envelope is what advances it.
        assert_eq!(
            phase.observe("artifact_build", r#"{"ok":true}"#),
            vec![PHASE_ARTIFACT_BUILT]
        );
    }

    #[test]
    fn observe_ignores_artifact_ref_named_by_a_failing_result() {
        // An error that merely *names* an artifact is not proof one was built —
        // otherwise "artifact not found" would advance the phase.
        let mut phase = SessionPhase::default();
        let err = r#"{"ok":false,"error_type":"resource","message":"artifact not found","artifact_ref":"ar.missing"}"#;
        assert!(phase.observe("resolve", err).is_empty());
        assert!(!phase.has(PHASE_ARTIFACT_BUILT));
    }

    #[test]
    fn observe_derives_artifact_from_evidence_not_just_own_action() {
        // The planner never calls artifact_build — its coder child does. Tool-name
        // mapping alone would leave a lead agent permanently pre-phase, which is
        // exactly the agent the RFC is trying to make cheaper.
        let mut phase = SessionPhase::default();
        let workflow_state = r#"{"ok":true,"reuse_guards":{"has_coder_artifact":true}}"#;
        assert_eq!(
            phase.observe("workflow_state", workflow_state),
            vec![PHASE_ARTIFACT_BUILT]
        );

        let mut phase = SessionPhase::default();
        let child_done = r#"{"ok":true,"result":{"status":"ok","artifact_ref":"ar.deadbeef"}}"#;
        assert_eq!(
            phase.observe("workflow_wait", child_done),
            vec![PHASE_ARTIFACT_BUILT]
        );
    }

    #[test]
    fn observe_ignores_empty_or_absent_artifact_ref() {
        let mut phase = SessionPhase::default();
        assert!(phase.observe("agent_list", r#"{"ok":true,"artifact_ref":""}"#).is_empty());
        assert!(phase.observe("agent_list", r#"{"ok":true,"agents":[]}"#).is_empty());
        assert!(phase.observe("agent_list", "not json at all").is_empty());
        assert!(!phase.has(PHASE_ARTIFACT_BUILT));
    }

    #[test]
    fn session_phase_survives_a_serde_round_trip() {
        // Checkpoint persistence: a resumed session must keep the guidance it
        // earned, or the prompt silently loses procedure at the most advanced
        // point of the work.
        let mut phase = SessionPhase::default();
        phase.insert(PHASE_ARTIFACT_BUILT);
        phase.insert(PHASE_REVISION_SEEDED);
        let json = serde_json::to_string(&phase).expect("serialize");
        let back: SessionPhase = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(phase, back);
        assert!(back.has(PHASE_REVISION_SEEDED));

        // Checkpoints predating the field deserialize as "no phase yet".
        let old: SessionPhase = serde_json::from_str("{}").expect("empty");
        assert!(old.is_empty());
    }

    #[test]
    fn is_phase_gated_sees_through_combinators() {
        let phase = GuidanceCondition::Phase(PHASE_ARTIFACT_BUILT);
        assert!(phase.is_phase_gated());
        assert!(GuidanceCondition::All(vec![
            GuidanceCondition::ToolPresent("x"),
            phase.clone()
        ])
        .is_phase_gated());
        assert!(GuidanceCondition::Any(vec![phase.clone()]).is_phase_gated());
        assert!(GuidanceCondition::Not(Box::new(phase)).is_phase_gated());
        assert!(!GuidanceCondition::ToolPresent("x").is_phase_gated());
        assert!(!GuidanceCondition::Always.is_phase_gated());
    }

    #[test]
    fn phase_gated_blocks_render_after_unconditional_ones() {
        // A fact landing mid-session must APPEND to the guidance section, not
        // insert into it: appending extends the cached prompt prefix, inserting
        // re-caches everything after the insertion point. With RFC P2 migrating
        // many tools onto this mechanism, that is the difference between one
        // cache extension per fact and a full re-cache per fact.
        let registry = crate::runtime::tools::default_registry();
        let mut blocks = registry.all_guidance_blocks();
        blocks.extend(builtin_blocks());

        let lowest_phase_gated = blocks
            .iter()
            .filter(|b| b.when.is_phase_gated())
            .map(|b| b.priority)
            .min();
        let highest_unconditional = blocks
            .iter()
            .filter(|b| !b.when.is_phase_gated())
            .map(|b| b.priority)
            .max();

        if let (Some(lowest), Some(highest)) = (lowest_phase_gated, highest_unconditional) {
            assert!(
                lowest > highest,
                "phase-gated blocks must render after every unconditional block \
                 (lowest phase-gated priority {lowest} <= highest unconditional {highest}); \
                 use PHASE_GATED_PRIORITY_FLOOR"
            );
        }

        for block in blocks.iter().filter(|b| b.when.is_phase_gated()) {
            assert!(
                block.priority >= PHASE_GATED_PRIORITY_FLOOR,
                "phase-gated block '{}' has priority {} below PHASE_GATED_PRIORITY_FLOOR ({})",
                block.id,
                block.priority,
                PHASE_GATED_PRIORITY_FLOOR
            );
        }
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
        assert_eq!(compose_guidance(&[a], &ctx).standing, "");
        // network_access absent → Not(network_access) is true → included.
        let b = block(
            "b",
            GuidanceCondition::Not(Box::new(GuidanceCondition::Capability("network_access"))),
            0,
        );
        assert_eq!(compose_guidance(&[b], &ctx).standing, "b");
    }

    #[test]
    fn approval_block_excluded_for_promotion_gate_roles() {
        // P-3.10: promotion-gate agents can't get network approval, so the
        // exec approval-continuation block must not fire for them.
        let block = vec![crate::runtime::tools::sandbox::exec_approval_continuation_block()];
        let tools = vec!["artifact_exec".to_string()];
        let coder = GuidanceContext { active_tool_names: &tools, role: Some("coder"), ..Default::default() };
        assert!(compose_guidance(&block, &coder).standing.contains("Approval continuation"));
        let utr = GuidanceContext { active_tool_names: &tools, role: Some("unit_test_runner"), ..Default::default() };
        assert_eq!(compose_guidance(&block, &utr).standing, "");
    }
}
