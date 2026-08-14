//! Prompt-composition budget harness (RFC `prompt-burden-phase-gated-guidance`).
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
    guidance::compose_guidance(&blocks, &ctx)
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
    guidance_pre: usize,
    guidance_post: usize,
}

fn measure(label: &'static str, rel: &str) -> Report {
    let (manifest, core, extended) = load_agent(rel);
    let (tools, tool_chars) = tool_schema_chars(&manifest, &ToolTierFilter::all());

    let empty = SessionPhase::default();
    let mut built = SessionPhase::default();
    built.insert(PHASE_ARTIFACT_BUILT);

    Report {
        label,
        tools,
        tool_chars,
        skill_core: core.len(),
        skill_extended: extended.as_deref().map_or(0, str::len),
        guidance_pre: guidance_for(&manifest, &empty).len(),
        guidance_post: guidance_for(&manifest, &built).len(),
    }
}

fn print_report(r: &Report) {
    // Foundation layers are selected from the manifest and are the same files for
    // every matching agent; report them from source so the total is honest.
    let foundation: usize = ["core", "workflow", "artifact", "sdk"]
        .iter()
        .map(|n| {
            let p = repo_root()
                .join("autonoetic-gateway/src/runtime")
                .join(format!("foundation_{n}.md"));
            std::fs::read_to_string(p).map_or(0, |s| s.len())
        })
        .sum();

    let pre_total = r.tool_chars + r.skill_core + foundation + r.guidance_pre;
    let post_total = r.tool_chars + r.skill_core + r.skill_extended + foundation + r.guidance_post;

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
        "  ---- steady-state total   {:>7} ch  (~{:>5} tok)",
        post_total,
        tok(post_total)
    );
}

#[test]
fn prompt_composition_report() {
    let planner = measure("planner.default", "agents/lead/planner.default/SKILL.md");
    let coder = measure("coder.default", "agents/specialists/coder.default/SKILL.md");
    print_report(&planner);
    print_report(&coder);

    // Tool schemas are the largest SINGLE layer for both main agents. This is
    // the finding the RFC's lever ordering rests on; if it ever stops being
    // true, re-derive the ordering rather than silently ignoring it.
    for r in [&planner, &coder] {
        let largest_other = r.skill_core.max(r.skill_extended).max(r.guidance_post);
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
