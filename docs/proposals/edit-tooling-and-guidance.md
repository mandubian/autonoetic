# Roadmap: edit tooling (`content_patch`) + modular guidance blocks

Two intertwined tracks:

- **Track A — `content_patch`** (ship ASAP): the token-efficient targeted-edit
  tool. Full spec in [`content-patch-tool.md`](./content-patch-tool.md).
- **Track B — Guidance Blocks** (design now, build incrementally): replace the
  "built on the fly" per-SKILL.md prose with composable, *targeted* guidance
  blocks (capability- / tool- / model-family- / role-gated). This is the
  mechanism that makes `content_patch` actually get used, and that Hermes's
  `_EDIT_FORMAT_GUIDANCE` proves you need.

The two meet at one point: `content_patch` needs editing doctrine in the prompt.
Track A ships that doctrine as a **single interim foundation block**; Track B
later generalizes it into the block mechanism and absorbs the interim file.

---

## Why Track B (the problem with today's assembly)

From the prompt-assembly audit (`context.rs:compose_system_instructions_full`):

- The **only** modular mechanism is `compose_foundation()` — capability-gated
  `include_str!` of `foundation_*.md` files.
- Per-tool guidance = the tool's `description` string. Nothing else.
- Editing doctrine ("prefer patch over rewrite") exists nowhere.
- Specialist guidance = ~350 lines of hand-written `SKILL.md` prose, copy-pasted
  doctrine per specialist. This is the "on the fly" fragility.
- **No model-family branching anywhere** — prompt is identical for
  Claude/GPT/Gemini. This is the core gap vs Hermes.

## Track B target design

A `GuidanceBlock` is a unit of prompt prose with a declarative activation
condition. Blocks are contributed by three sources — foundation set, tools
themselves, and roles — collected, filtered against the live turn, ordered, and
rendered into one section of the system prompt.

```rust
pub struct GuidanceBlock {
    pub id: &'static str,             // stable id, dedupe key
    pub prose: String,
    pub when: GuidanceCondition,      // activation predicate
    pub priority: i32,                // render order (foundation low, role high)
}

pub enum GuidanceCondition {
    Always,
    Capability(CapabilityKind),       // e.g. WriteAccess
    ToolPresent(&'static str),        // only if tool is in this turn's tool set
    ModelFamily(&'static [&'static str]), // ["claude","sonnet","opus","haiku"]
    Role(&'static str),               // "coder", "auditor", ...
    All(Vec<GuidanceCondition>),      // AND
    Any(Vec<GuidanceCondition>),      // OR
}
```

- **Tool-contributed blocks:** extend `NativeTool` with
  `fn guidance(&self) -> Vec<GuidanceBlock> { vec![] }`. `content_patch` ships
  its own "prefer me for edits" block, gated `All([WriteAccess,
  ToolPresent("content_patch")])`. A tool's guidance is only ever injected when
  the tool itself survived tier/capability filtering — guidance and capability
  stay in lockstep automatically.
- **Model-family:** derive family from the routed model id (mirror Hermes
  `_model_family()`); blocks can target families. Lets us say
  replace-mode→Claude, V4A→GPT *without* duplicating the whole prompt.
- **Composer:** `compose_guidance(blocks, ctx)` filters by `ctx`
  (capabilities, active tool names, model family, role), sorts by priority,
  dedupes by `id`, renders. Slots into `compose_system_instructions_full` as a
  new layer (after foundation, before/within agent instructions).
- **Migration:** move tool/capability *doctrine* out of specialist `SKILL.md`s
  into blocks; leave only genuinely role-specific prose in `SKILL.md`. Kills the
  copy-paste.

Layer order becomes:
`Foundation → Guidance blocks → tool-bridging → persona → user → role prose → contract`

---

## Build order

```
#461  content_patch tool ............................. Track A, NOW
#462  foundation_editing.md (interim doctrine) ........ Track A, NOW (ships w/ #461)
        └─ value delivered: patch tool + agents told to use it
#463  GuidanceBlock core + composer ................... Track B
#464  NativeTool::guidance() contribution hook ........ Track B (needs #463)
#465  model-family derivation + family targeting ...... Track B (needs #463)
#466  migrate SKILL.md doctrine → blocks; ............. Track B (needs #464)
        absorb foundation_editing.md into a block
```

(GitHub issues filed: #461–#466.)

#1 + #2 are independent of Track B and deliver the token win immediately. #2 is
deliberately a throwaway stepping stone: a plain capability-gated foundation
file, later refactored into a tool-contributed block by #4/#6.

---

## Issue drafts

### Issue 1 — Implement `content_patch` (targeted edits on content-store entries)
**Track A · depends on: none · ships with #2**

Add a core-tier `content_patch` tool (full spec: `docs/proposals/content-patch-tool.md`).

- [ ] `runtime/fuzzy_match.rs`: 3 strategies (exact, line-trimmed,
      whitespace-normalized) with indentation handling folded into replacement
      (re-indent to the matched region); uniqueness + `replace_all` rules
      (`replace_all` honored for exact only); per-match strategy reporting.
- [ ] `runtime/v4a.rs`: parse + two-phase (validate-all-then-apply) for
      `Update File` / `Add File`. `Delete`/`Move` parsed → explicit
      "not yet supported" error (needs store unregister/rename — separate issue).
- [ ] `runtime/tools/content_patch.rs`: `mode=replace|v4a`; resolve name/ref →
      read bytes → apply → `store.write` → `register_name_with_visibility`
      (preserve visibility); result mirrors `content_write` + `strategy` + `diff`.
- [ ] Register in `runtime/tools/mod.rs`. (`content_` prefix → core tier auto.)
- [ ] Anti-loop: escalation hint on every match failure (per-`(session,name)`
      counter dropped for v1 — see content-patch-tool.md).
- [ ] Tests: per-strategy units; v4a atomic-abort; integration (patch →
      `resolve` returns patched bytes, name re-points, **old handle still
      readable**, visibility preserved); multi-entry v4a atomicity;
      duplicate-name + unsafe-Add-name rejection.

### Issue 2 — Interim editing doctrine: `foundation_editing.md`
**Track A · depends on: #1**

Stop relying on per-SKILL.md prose for the write-vs-patch choice.

- [ ] Add `foundation_editing.md` + `include_str!` in `context.rs`, gated on
      `WriteAccess` in `compose_foundation()` (mirrors `FOUNDATION_ARTIFACT`).
- [ ] Doctrine: two editing tools; prefer `content_patch` (`mode=replace`) for
      edits; reserve `content_write` for new entries / unanchorable regions;
      `mode=v4a` only for multi-entry edits.
- [ ] Tighten `content_write` / `content_patch` `description` strings to
      cross-reference each other.
- [ ] Trim now-redundant editing prose from `coder.default/SKILL.md`.
- [ ] Note in the file header: superseded by Issue 6 (tool-contributed block).

### Issue 3 — `GuidanceBlock` core + composer
**Track B · depends on: none (parallel to A)**

- [ ] `GuidanceBlock` + `GuidanceCondition` types.
- [ ] `compose_guidance(blocks, ctx)`: filter / sort by priority / dedupe by id /
      render. `GuidanceContext { capabilities, active_tool_names, model_family,
      role }`.
- [ ] Wire a new "Guidance blocks" layer into
      `compose_system_instructions_full` (after foundation).
- [ ] Unit tests for each condition + ordering + dedupe.

### Issue 4 — Tool-contributed guidance (`NativeTool::guidance()`)
**Track B · depends on: #3**

- [ ] `fn guidance(&self) -> Vec<GuidanceBlock> { vec![] }` on `NativeTool`.
- [ ] Lifecycle collects guidance only from tools that survived tier/capability
      filtering, feeds into the composer.
- [ ] Move the editing doctrine into a `content_patch`-contributed block,
      gated `All([WriteAccess, ToolPresent("content_patch")])`.
- [ ] Tests: block present iff tool present.

### Issue 5 — Model-family-aware guidance
**Track B · depends on: #3**

- [ ] Derive model family from routed model id (mirror Hermes `_model_family`).
- [ ] Populate `GuidanceContext.model_family`; honor `ModelFamily` condition.
- [ ] Family-targeted edit-format hint: Claude→`replace`-first; GPT/Codex→V4A
      for multi-file (only if/when a non-Claude coder is configured).
- [ ] Tests across families.

### Issue 6 — Migrate specialist doctrine into blocks
**Track B · depends on: #4**

- [ ] Audit `agents/specialists/*/SKILL.md`; classify prose as
      tool/capability *doctrine* (→ blocks) vs *role-specific* (stays).
- [ ] Move doctrine into capability/tool-gated blocks; delete duplication.
- [ ] Absorb `foundation_editing.md` (Issue 2) into the tool-contributed block;
      remove the interim file.
- [ ] Verify composed prompts are equivalent-or-better (snapshot test on coder).

---

## Sequencing / mechanics

- Branch off `main` for Track A (`feat/content-patch`); per repo convention,
  branch early so concurrent edits don't clobber.
- #1 + #2 reviewed/merged together → immediate token win, no Track B dependency.
- Track B can start in parallel; #3 has no dependency on A.
- `foundation_editing.md` is intentionally disposable — #6 deletes it.
