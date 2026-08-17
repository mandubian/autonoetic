//! Prompt-composition budget harness (`docs/prompt-burden-study.md`).
//!
//! The context governor treats `system_prompt_tokens` as a constant — nothing in
//! the runtime ever measures, let alone reduces, the fixed prompt. So the cost of
//! every doctrine addition has been invisible: prose lands in a `SKILL.md` or a
//! tool description and no test notices the prompt got bigger.
//!
//! This harness makes the number observable. It composes the real prompt inputs
//! for the two main agents from the repo's own `SKILL.md` files plus the live
//! tool registry, prints a per-layer breakdown, and asserts the phase-gating
//! mechanism actually moves tokens out of the pre-phase prompt.
//!
//! Run the report with:
//! ```bash
//! cargo test -p autonoetic-gateway --test prompt_composition_budget -- --nocapture
//! ```

use autonoetic_gateway::runtime::guidance::{
    self, GuidanceContext, SessionPhase, PHASE_ARTIFACT_BUILT,
};
#[allow(unused_imports)]
use autonoetic_gateway::runtime::tools::NativeToolRegistry;
use autonoetic_gateway::runtime::context::partition_gated_sections;
use autonoetic_gateway::runtime::parser::{split_extended_instructions, SkillParser};
use autonoetic_gateway::runtime::tools::{default_registry, ToolTierFilter};
use autonoetic_types::agent::AgentManifest;

/// Rough tokens-per-char. Only used for the human-readable report; every
/// assertion below compares *chars*, which are exact.
const CHARS_PER_TOKEN: usize = 4;

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn load_agent(rel: &str) -> (AgentManifest, String, Option<String>) {
    let path = repo_root().join(rel);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let (manifest, body) = SkillParser::parse(&content)
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
    let (core, extended) = split_extended_instructions(&body);
    (manifest, core.to_string(), extended.map(str::to_string))
}

/// Total chars of every tool definition the agent would be advertised — the
/// description plus the serialized input schema, which is what actually goes
/// over the wire.
fn tool_schema_chars(manifest: &AgentManifest, filter: &ToolTierFilter) -> (usize, usize) {
    let registry = default_registry();
    let defs = registry.available_definitions_filtered(manifest, Some(filter));
    let chars: usize = defs
        .iter()
        .map(|d| {
            d.name.len()
                + d.description.len()
                + serde_json::to_string(&d.input_schema).map_or(0, |s| s.len())
        })
        .sum();
    (defs.len(), chars)
}

/// Per-tool schema sizes, heaviest first. The work-list for RFC P2: tool schemas
/// are the largest single prompt layer, and the weight is concentrated in a few
/// definitions that carry *procedure* rather than signature.
fn tool_schema_sizes(manifest: &AgentManifest, filter: &ToolTierFilter) -> Vec<(String, usize)> {
    let registry = default_registry();
    let mut sizes: Vec<(String, usize)> = registry
        .available_definitions_filtered(manifest, Some(filter))
        .iter()
        .map(|d| {
            (
                d.name.clone(),
                d.name.len()
                    + d.description.len()
                    + serde_json::to_string(&d.input_schema).map_or(0, |s| s.len()),
            )
        })
        .collect();
    sizes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sizes
}

fn guidance_for(manifest: &AgentManifest, phase: &SessionPhase) -> String {
    let registry = default_registry();
    let filter = ToolTierFilter::all();
    let mut blocks = registry.collect_guidance_blocks(manifest, &filter);
    blocks.extend(guidance::builtin_blocks());

    // Advertise every tool the agent could hold, so `ToolPresent` gating in this
    // harness reflects the agent's real ceiling rather than one turn's subset.
    let tool_names: Vec<String> = registry
        .available_definitions_filtered(manifest, Some(&filter))
        .into_iter()
        .map(|d| d.name)
        .collect();

    let role_owned = manifest.agent.id.split('.').next().map(str::to_string);
    let ctx = GuidanceContext {
        capabilities: &manifest.capabilities,
        active_tool_names: &tool_names,
        model_family: Some("claude-opus-4-8"),
        role: role_owned.as_deref(),
        phase: Some(phase),
    };
    let composed = guidance::compose_guidance(&blocks, &ctx);
    // Both sections are prompt cost regardless of where they render, so the
    // budget report counts them together.
    [composed.standing, composed.phase_tail]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn tok(chars: usize) -> usize {
    chars / CHARS_PER_TOKEN
}

struct Report {
    label: &'static str,
    tools: usize,
    tool_chars: usize,
    skill_core: usize,
    skill_extended: usize,
    /// Sections evicted until a phase is reached (RFC P3), measured at
    /// `artifact_built`. Absent from turn 1, present in steady state.
    skill_earned: usize,
    foundation: usize,
    guidance_pre: usize,
    guidance_post: usize,
}

impl Report {
    /// The largest prompt layer that is not the tool schemas. Every layer the
    /// report prints participates — a canary that ignored one could pass while
    /// the thing it guards had already flipped.
    fn largest_non_tool_layer(&self) -> usize {
        self.skill_core
            .max(self.skill_extended)
            .max(self.foundation)
            .max(self.guidance_post)
    }
}

/// Foundation layers are selected from the manifest and are the same files for
/// every matching agent; read them from source so the total is honest.
fn foundation_chars() -> usize {
    ["core", "workflow", "artifact", "sdk"]
        .iter()
        .map(|n| {
            let p = repo_root()
                .join("autonoetic-gateway/src/runtime")
                .join(format!("foundation_{n}.md"));
            std::fs::read_to_string(p).map_or(0, |s| s.len())
        })
        .sum()
}

fn measure(label: &'static str, rel: &str) -> Report {
    let (manifest, core, extended) = load_agent(rel);
    let (tools, tool_chars) = tool_schema_chars(&manifest, &ToolTierFilter::all());

    let empty = SessionPhase::default();
    let mut built = SessionPhase::default();
    built.insert(PHASE_ARTIFACT_BUILT);

    // Section gates (RFC P3) evict from the standing body until their phase is
    // reached. Measure the standing halves, and the earned sections separately,
    // so turn 1 reflects the eviction and steady state reflects their return.
    let (core_standing, core_earned) =
        partition_gated_sections(&core, &manifest.sections, &built);
    let (ext_standing, ext_earned) = match extended.as_deref() {
        Some(e) => partition_gated_sections(e, &manifest.sections, &built),
        None => (String::new(), Vec::new()),
    };
    let earned: usize = core_earned
        .iter()
        .chain(ext_earned.iter())
        .map(String::len)
        .sum();

    Report {
        label,
        tools,
        tool_chars,
        skill_core: core_standing.len(),
        skill_extended: ext_standing.len(),
        skill_earned: earned,
        foundation: foundation_chars(),
        guidance_pre: guidance_for(&manifest, &empty).len(),
        guidance_post: guidance_for(&manifest, &built).len(),
    }
}

fn print_report(r: &Report) {
    let foundation = r.foundation;
    let pre_total = r.tool_chars + r.skill_core + foundation + r.guidance_pre;
    let post_total = r.tool_chars
        + r.skill_core
        + r.skill_extended
        + r.skill_earned
        + foundation
        + r.guidance_post;

    println!("\n=== {} — fixed system prompt ===", r.label);
    println!(
        "  tool schemas ({:>2} tools)   {:>7} ch  (~{:>5} tok)",
        r.tools,
        r.tool_chars,
        tok(r.tool_chars)
    );
    println!(
        "  SKILL.md core             {:>7} ch  (~{:>5} tok)",
        r.skill_core,
        tok(r.skill_core)
    );
    println!(
        "  SKILL.md extended         {:>7} ch  (~{:>5} tok)   [inlined from turn 2, permanently]",
        r.skill_extended,
        tok(r.skill_extended)
    );
    if r.skill_earned > 0 {
        println!(
            "  SKILL.md phase-earned     {:>7} ch  (~{:>5} tok)   [evicted until artifact_built]",
            r.skill_earned,
            tok(r.skill_earned)
        );
    }
    println!(
        "  foundation layers         {:>7} ch  (~{:>5} tok)",
        foundation,
        tok(foundation)
    );
    println!(
        "  guidance (pre-phase)      {:>7} ch  (~{:>5} tok)",
        r.guidance_pre,
        tok(r.guidance_pre)
    );
    println!(
        "  guidance (artifact_built) {:>7} ch  (~{:>5} tok)   [+{} ch entered at phase]",
        r.guidance_post,
        tok(r.guidance_post),
        r.guidance_post.saturating_sub(r.guidance_pre)
    );
    println!(
        "  ---- turn 1 total         {:>7} ch  (~{:>5} tok)",
        pre_total,
        tok(pre_total)
    );
    println!(
        "  ---- working total        {:>7} ch  (~{:>5} tok)   [extended loaded, no phase yet]",
        r.working_chars(),
        tok(r.working_chars())
    );
    println!(
        "  ---- steady-state total   {:>7} ch  (~{:>5} tok)",
        post_total,
        tok(post_total)
    );
}

impl Report {
    /// Everything the agent pays for in a steady-state turn.
    fn steady_state_chars(&self) -> usize {
        self.tool_chars
            + self.skill_core
            + self.skill_extended
            + self.skill_earned
            + self.foundation
            + self.guidance_post
    }

    /// What turn 1 costs, before the extended SKILL half is inlined (#1015) and
    /// before any phase fact lands. Tracked separately because the two totals
    /// respond to different levers: the `<!-- extended -->` split moves turn 1
    /// only (the extended half is inlined permanently from turn 2), while
    /// exclusions, tiering, and phase gating move both.
    fn turn_one_chars(&self) -> usize {
        self.tool_chars + self.skill_core + self.foundation + self.guidance_pre
    }

    /// The **modal** turn: extended half loaded (from turn 2 onward, #1015) but
    /// no phase fact earned yet. This is what a session that never builds an
    /// artifact pays on every turn — the majority of planner sessions — and it
    /// is the only total RFC P3's section gating moves. Turn 1 already excludes
    /// the extended half; steady state measures the post-`artifact_built` state
    /// where the gated sections have legitimately returned.
    fn working_chars(&self) -> usize {
        self.tool_chars
            + self.skill_core
            + self.skill_extended
            + self.foundation
            + self.guidance_pre
    }
}

/// Steady-state prompt ceilings, in characters.
///
/// **These are a ratchet, not a target.** Before this harness existed, nothing
/// measured the fixed prompt — `context_governor` treats `system_prompt_tokens`
/// as a constant — so every doctrine addition was free at the point of
/// authorship and the prompt grew unobserved to ~28k tokens for the planner.
///
/// A failure here is not a request to raise the number. It means a change added
/// prompt weight, and the question to answer in review is whether that weight
/// earns its place in *every* turn. If it genuinely does, lower-bound it
/// deliberately and say why in the commit. As the RFC rollout lands (P2–P5),
/// these should be ratcheted **down**, never up.
/// `(turn-1 ceiling, working ceiling, steady-state ceiling)` per agent. **Ratcheted down** as RFC
/// phases land — #1085 (collaborative trim) and P2 (this pass) are both locked in
/// below.
const PLANNER_CEILINGS: (usize, usize, usize) = (74_000, 90_000, 102_500);
const CODER_CEILINGS: (usize, usize, usize) = (60_000, 70_000, 70_000);
/// `planner.collaborative` is the chat-heavy twin and the agent currently being
/// trimmed by hand (#1085) — which is exactly why it needs a ceiling: hand-tuning
/// an agent nothing measures is how the prompt got here in the first place.
const PLANNER_COLLAB_CEILINGS: (usize, usize, usize) = (92_500, 103_000, 103_000);
/// The two phase-gated promotion procedures live in **disjoint** agent families,
/// so covering one does not cover the other:
///
/// - `promotion_record` is restricted to the four gate roles by
///   `manifest_may_record_promotion_verdicts` (sealed_evaluator, auditor,
///   static_evaluator, unit_test_runner) — represented here by
///   `unit_test_runner.default`.
/// - `agent_revision_promote` requires `Capability::AgentRevision`, which the
///   gate roles do **not** hold. Exactly one agent declares it in frontmatter:
///   `specialized_builder.default`.
///
/// Both are measured so the phase-gating of each procedure is observable
/// somewhere. The lead and coder agents see neither tool.
const UNIT_TEST_RUNNER_CEILINGS: (usize, usize, usize) = (49_500, 50_500, 50_500);
const SPECIALIZED_BUILDER_CEILINGS: (usize, usize, usize) = (85_000, 86_000, 86_000);
/// Now the sole owner of the credential ceremony, so it absorbs the schema the
/// planners shed. Measured here so the move is a *transfer with a ceiling*, not
/// weight pushed somewhere nobody looks.
const CREDENTIAL_ONBOARDING_CEILINGS: (usize, usize, usize) = (56_500, 56_500, 56_500);

#[test]
fn prompt_composition_report() {
    let planner = measure("planner.default", "agents/lead/planner.default/SKILL.md");
    let coder = measure("coder.default", "agents/specialists/coder.default/SKILL.md");
    let collab = measure(
        "planner.collaborative",
        "agents/lead/planner.collaborative/SKILL.md",
    );
    let utr = measure(
        "unit_test_runner.default",
        "agents/specialists/unit_test_runner.default/SKILL.md",
    );
    let builder = measure(
        "specialized_builder.default",
        "agents/evolution/specialized_builder.default/SKILL.md",
    );
    let onboarding = measure(
        "credential_onboarding.default",
        "agents/specialists/credential_onboarding.default/SKILL.md",
    );
    print_report(&planner);
    print_report(&coder);
    print_report(&collab);
    print_report(&utr);
    print_report(&builder);
    print_report(&onboarding);

    // Tool schemas are the largest SINGLE layer for both main agents. This is
    // the finding the RFC's lever ordering rests on; if it ever stops being
    // true, re-derive the ordering rather than silently ignoring it.
    for (r, (turn1_ceiling, working_ceiling, steady_ceiling)) in [
        (&planner, PLANNER_CEILINGS),
        (&coder, CODER_CEILINGS),
        (&collab, PLANNER_COLLAB_CEILINGS),
        (&utr, UNIT_TEST_RUNNER_CEILINGS),
        (&builder, SPECIALIZED_BUILDER_CEILINGS),
        (&onboarding, CREDENTIAL_ONBOARDING_CEILINGS),
    ] {
        // The ratchet. Growth used to be invisible AND free; the report made it
        // visible, this makes it cost something.
        for (label, actual, ceiling) in [
            ("turn-1", r.turn_one_chars(), turn1_ceiling),
            ("working (no phase reached)", r.working_chars(), working_ceiling),
            ("steady-state", r.steady_state_chars(), steady_ceiling),
        ] {
            assert!(
                actual <= ceiling,
                "{}: {label} prompt is {actual} ch (~{} tok), over the {ceiling} ch ceiling \
                 by {}. Something added weight paid on every turn — justify it and lower-bound \
                 the ceiling deliberately, or gate the addition (capability, tool, role, or \
                 phase) so only the turns that need it pay.",
                r.label,
                tok(actual),
                actual - ceiling
            );
        }

        let largest_other = r.largest_non_tool_layer();
        assert!(
            r.tool_chars > largest_other,
            "{}: tool schemas ({}) should be the largest single prompt layer, \
             but another layer is {}",
            r.label,
            r.tool_chars,
            largest_other
        );
    }
}

#[test]
fn phase_gating_keeps_procedure_out_of_the_pre_phase_prompt() {
    let (manifest, _core, _ext) = load_agent("agents/lead/planner.default/SKILL.md");

    let empty = SessionPhase::default();
    let pre = guidance_for(&manifest, &empty);
    assert!(
        !pre.contains("Escalating federation verdicts"),
        "federation procedure must be absent before an artifact exists"
    );

    let mut built = SessionPhase::default();
    built.insert(PHASE_ARTIFACT_BUILT);
    let post = guidance_for(&manifest, &built);
    assert!(
        post.contains("Escalating federation verdicts"),
        "federation procedure must appear once the session has an artifact"
    );

    // The whole point: the pre-phase prompt is strictly smaller, and the
    // procedure is not lost — it arrives when it becomes actionable.
    assert!(
        post.len() > pre.len(),
        "phase-gated block should add prose at the phase, got pre={} post={}",
        pre.len(),
        post.len()
    );
}

/// The P2 work-list. Prints where the tool-schema weight actually sits, so the
/// migration is driven by measurement rather than by which tool feels verbose.
#[test]
fn tool_schema_hotspots() {
    let (manifest, _core, _ext) = load_agent("agents/lead/planner.default/SKILL.md");
    let sizes = tool_schema_sizes(&manifest, &ToolTierFilter::all());
    let total: usize = sizes.iter().map(|(_, n)| n).sum();

    println!("\n=== planner.default — heaviest tool schemas (RFC P2 work-list) ===");
    for (name, chars) in sizes.iter().take(15) {
        println!(
            "  {chars:>6} ch (~{:>4} tok)  {:>4.1}%  {name}",
            chars / CHARS_PER_TOKEN,
            100.0 * *chars as f64 / total as f64
        );
    }
    let top10: usize = sizes.iter().take(10).map(|(_, n)| n).sum();
    println!(
        "  top 10 = {top10} ch of {total} ch ({:.0}% of all tool schema)",
        100.0 * top10 as f64 / total as f64
    );
}

#[test]
fn phase_guidance_renders_after_the_standing_prompt() {
    // Placement, not ordering, is what keeps the cache intact: the standing
    // guidance section is followed by the agent's whole SKILL.md, so an earned
    // block rendered there would re-cache all of it. The tail must come last.
    let (manifest, _core, _ext) = load_agent("agents/lead/planner.default/SKILL.md");
    let mut built = SessionPhase::default();
    built.insert(PHASE_ARTIFACT_BUILT);

    let registry = default_registry();
    let filter = ToolTierFilter::all();
    let mut blocks = registry.collect_guidance_blocks(&manifest, &filter);
    blocks.extend(guidance::builtin_blocks());
    let tool_names: Vec<String> = registry
        .available_definitions_filtered(&manifest, Some(&filter))
        .into_iter()
        .map(|d| d.name)
        .collect();
    let role_owned = manifest.agent.id.split('.').next().map(str::to_string);
    let composed = guidance::compose_guidance(
        &blocks,
        &GuidanceContext {
            capabilities: &manifest.capabilities,
            active_tool_names: &tool_names,
            model_family: Some("claude-opus-4-8"),
            role: role_owned.as_deref(),
            phase: Some(&built),
        },
    );

    assert!(
        composed.phase_tail.contains("Escalating federation verdicts"),
        "earned procedure belongs in the tail"
    );
    assert!(
        !composed.standing.contains("Escalating federation verdicts"),
        "earned procedure must NOT be in the standing section — that is the \
         section followed by SKILL.md in the cache prefix"
    );
    assert!(
        !composed.standing.is_empty(),
        "standing guidance should still render normally"
    );
}

#[test]
fn agents_without_the_tool_never_see_its_procedure() {
    // coder.default excludes `federation_*`, so neither the signature nor the
    // procedure should reach it in either phase. Guidance and capability stay in
    // lockstep — phase gating must not become a back door.
    let (manifest, _core, _ext) = load_agent("agents/specialists/coder.default/SKILL.md");
    let mut built = SessionPhase::default();
    built.insert(PHASE_ARTIFACT_BUILT);
    let rendered = guidance_for(&manifest, &built);
    assert!(
        !rendered.contains("Escalating federation verdicts"),
        "coder must not receive federation procedure even in the artifact_built phase"
    );
}
