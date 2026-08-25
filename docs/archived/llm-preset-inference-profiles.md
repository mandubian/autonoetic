> **Archived — shipped.** The behaviour this proposed is live and described in [`composition.md`](../internals/prompt/composition.md). Kept as the design record; not source of truth.

# RFC: LLM Preset Inference Profiles — Agent Identity vs Runtime Model Resolution

**Status:** Draft — 2026-06-09  
**Origin:** Operator need to continue a session after LLM provider errors without editing
`SKILL.md` or creating a new agent revision. Same agent bundles are reused across
deployments with different models; hard-coded `provider`/`model` in manifests is brittle.

**Related:** `docs/archived/plan-unified-llm-presets.md` (preset registry shape),
`docs/reference/config.md` (`llm_presets`, `llm_preset_mapping`, `llm_routing`),
`docs/ARCHITECTURE.md` (checkpoints, reproducibility fields).

---

## 1. Problem

Today a reasoning agent binds inference to its **identity bundle**:

```yaml
llm_config:
  provider: "openrouter"
  model: "google/gemini-3-flash-preview"
  temperature: 0.2
```

Bootstrap may patch these values from `llm_preset_mapping`, but the **canonical
store is still inline provider/model in `SKILL.md`**. Consequences:

| Pain | Why it hurts |
|------|----------------|
| LLM outage mid-session | No operator path to switch model and resume; must edit manifest or fork |
| Same agent, different deployments | Model choice is baked into revision content, not ops config |
| Drift | `planner.collaborative` comments say “overridden at bootstrap” while still carrying concrete model strings |
| Dead fields | `fallback_provider` / `fallback_model` on `LlmConfig` are stored in checkpoints but not used for generic failover |
| Driver lifetime | `build_driver()` runs once at spawn; per-turn routing can change `CompletionRequest.model` but not provider/base URL |

`runtime.lock` already separates **execution closure** (gateway, sandbox, layers)
from agent identity. LLM choice is a third axis that should follow the same pattern:
**stable agent bundle + gateway-resolved inference profile + optional session override**.

---

## 2. Decisions (anchor)

### 2.1 Agent bundle = cognitive identity; preset = inference profile

| Layer | Stable across deployments? | Owned by |
|-------|---------------------------|----------|
| Instructions, capabilities, I/O, `runtime.lock` | Yes (revisioned) | Agent bundle |
| Default inference profile (`llm_preset` name) | Yes (reference only) | Agent bundle |
| Concrete provider/model/tier/cost | No (ops-tunable) | `config.yaml` → `llm_presets` |
| Session override preset | No (incident response) | Operator / gateway session binding |

An agent revision should not change when ops switches from Gemini Flash to Claude
Sonnet. The revision records **which preset role** the agent expects (`coder`,
`smart`, `budget`), not the literal model string.

### 2.2 Separation of Powers — unchanged

The LLM is reasoning substrate. Capabilities, approvals, sandbox, and tool tiers
remain gateway-enforced. **Agents must not self-select models** (discretion leak).
Model changes are operator-initiated or gateway-routed (deterministic router +
documented fallback chain), always causal-logged.

### 2.3 Reproducibility — explicit, not frozen

Checkpoints already carry `llm_config_snapshot`. We extend audit fields to record
**preset names** and **session overrides**, not to block model changes. LLM output
stays `validation: soft`; evaluators run under reproducible artifact conditions.

### 2.4 Ri-0.6 analogue — no silent capability reduction

Switching to a `chat_only` preset mid-session when the agent has tool calls in
flight is a **capability reduction**. The gateway must reject or require explicit
operator acknowledgment (same pattern as tier downgrade), and emit a causal event.

---

## 3. Target shapes

### 3.1 `SKILL.md` — preset reference (medium-term canonical)

Reasoning agents declare a **preset name**, not inline provider/model:

```yaml
metadata:
  autonoetic:
    llm_preset: smart          # required for reasoning agents (new)
    # Optional per-agent overrides of preset-resolved fields:
    llm_overrides:
      temperature: 0.1
      thinking:
        effort: high
```

- `llm_preset` must name a key in gateway `llm_presets`.
- Fixed presets resolve to concrete `provider`/`model` at runtime.
- Routing presets (preset with `routing:` block) enable per-turn selection +
  `enable_fallback_chain` failover (existing `model_router` path).
- `llm_overrides` merges on top of resolved preset fields (temperature, thinking,
  `context_window_tokens` hint). It must **not** carry `provider`/`model` — those
  live only in `llm_presets`.

**Remove from new/edited agents:** inline `llm_config.provider` / `llm_config.model`
as the primary declaration. Keep `llm_config` struct internally as the **resolved**
runtime object.

### 3.2 Gateway config — unchanged registry, clearer roles

```yaml
llm_presets:
  sonnet: { provider: anthropic, model: claude-sonnet-4-20250514, tier: standard, ... }
  smart:
    routing:
      strategy: hybrid
      models: [opus, sonnet, haiku]
      deterministic: { enable_fallback_chain: true, ... }

llm_preset_mapping:
  coder.default: smart
  planner.default: smart
  default: sonnet
```

`llm_preset_mapping` remains the **deployment default** when an agent omits
`llm_preset` in its manifest (foundational agents shipped in-repo may omit and rely
on mapping). Mapping keys: full `agent_id`, then base role (`coder` from
`coder.default`), then `default`.

### 3.3 Session binding — operator override (short-term + medium-term)

Per root session (and propagated to child sessions in the same root tree unless
explicitly scoped):

```yaml
# logical — stored in gateway SQLite, not SKILL.md
session_inference_binding:
  root_session_id: "session-abc"
  preset_override: "fallback"    # optional; names llm_presets key
  reason: "openrouter 503"
  set_by: "operator:cli"
  set_at: "2026-06-09T12:00:00Z"
```

Resolution at wake / resume / each turn:

```
1. session.preset_override          (operator)
2. agent.manifest.llm_preset        (bundle)
3. llm_preset_mapping[agent_id]    (gateway default)
4. llm_preset_mapping[base_role]
5. llm_preset_mapping.default
→ resolve preset → LlmConfig (+ llm_overrides)
→ if routing preset → model_router per turn (+ fallback chain on LLM error)
```

---

## 4. Runtime behavior

### 4.1 Resolution module (single entry point)

Consolidate `resolve_llm_config` (CLI bootstrap), `execution.rs` spawn path, and
lifecycle per-turn routing behind:

```rust
resolve_inference_profile(
    agent_id: &str,
    manifest: &AgentManifest,
    gateway_config: &GatewayConfig,
    session_binding: Option<&SessionInferenceBinding>,
) -> ResolvedInferenceProfile {
    preset_name, resolved_llm_config, is_routing, ...
}
```

All spawn and resume paths call this. **Stop patching `provider`/`model` into
`SKILL.md` at bootstrap** except for `agent init` scaffolding (writes
`llm_preset: <mapped>` instead).

### 4.2 Driver rebuild on provider change

When resolved `provider` or `base_url` changes (session override, cross-provider
fallback — future), rebuild `Arc<dyn LlmDriver>` before the next completion.
Per-turn model changes within the same provider continue to use `CompletionRequest.model`
as today.

Wire `fallback_provider` / `fallback_model` from fixed presets into the existing
fallback loop in `lifecycle.rs`, or deprecate those fields in favor of routing-only
fallback chains (see §7).

### 4.3 LLM error recovery (short-term, no new concepts)

On `llm.complete` failure:

1. If routing preset with `enable_fallback_chain` → try chain (existing).
2. Else if resolved preset defines `fallback_provider` + `fallback_model` on the
   **fixed** preset → try once (wire dead fields).
3. Else if session has no override and `llm_preset_mapping` defines
   `llm_presets.<role>.on_error_preset` (new optional gateway field) → auto-switch
   session binding to that preset, causal event `session.inference_failover`, retry
   once.
4. Else yield checkpoint with `YieldReason::Error` so operator can
   `/model <preset>` and resume.

Step 3 is optional in Phase 1; Phase 1 minimum is operator `/model` + routing
fallback chains.

### 4.4 Causal events (audit)

| Event | When |
|-------|------|
| `inference.preset_resolved` | Session start / resume (preset name, resolved provider/model, source: agent/mapping/override) |
| `inference.model_routing` | Per-turn routing decision (existing) |
| `session.inference_override` | Operator sets/clears `/model` |
| `session.inference_failover` | Automatic switch to `on_error_preset` |
| `inference.capability_guard` | Rejected chat_only downgrade with active tool tier |

---

## 5. Checkpoint and resume

Extend `LlmConfigSnapshot` → rename conceptually to `InferenceSnapshot`:

```json
{
  "preset_name": "smart",
  "preset_source": "agent_manifest",
  "session_override_preset": "fallback",
  "provider": "openrouter",
  "model": "anthropic/claude-sonnet-4",
  "temperature": 0.2,
  "chat_only": false
}
```

- `preset_source`: `session_override` | `agent_manifest` | `mapping` | `legacy_inline`
- On resume: re-resolve from binding + manifest + config (not blind reuse of
  snapshot provider/model). Snapshot is **audit baseline**; divergence logs
  `inference.preset_drift` if resolved profile differs from snapshot without a
  recorded override event.

Store `session_override_preset` in checkpoint so fork/resume after gateway restart
recovers operator choice without a separate table read (table remains source of truth).

---

## 6. Storage

**New table** `session_inference_bindings`:

```sql
CREATE TABLE session_inference_bindings (
  root_session_id TEXT PRIMARY KEY,
  preset_override TEXT,           -- NULL = use agent/mapping default
  reason TEXT,
  set_by TEXT NOT NULL,
  set_at TEXT NOT NULL
);
```

Cleanup: delete row on root session end / emergency stop (same lifecycle as
`session_approval_grants`).

---

## 7. CLI and API

### 7.1 Chat slash commands

```
/model                      Show resolved preset + provider/model for this session
/model <preset> [reason]    Set session override (validates preset exists, chat_only guard)
/model clear                Remove override; revert to agent/mapping default
```

Also accept `autonoetic chat --llm-preset <name>` when starting or resuming a session.

### 7.2 JSON-RPC (SDK / remote)

```json
{ "method": "session.inference.get", "params": { "session_id": "..." } }
{ "method": "session.inference.set", "params": { "session_id": "...", "preset": "fallback", "reason": "..." } }
{ "method": "session.inference.clear", "params": { "session_id": "..." } }
```

Gateway-only; not exposed as an agent tool.

### 7.3 `agent init` / `agent install`

Templates emit:

```yaml
llm_preset: sonnet   # or mapped name from --template
```

`--provider` / `--model` remain as **one-shot init overrides** that create a
**custom fixed preset** in local config (`llm_presets._local.<agent_id>`) and set
`llm_preset` to that name — not inline provider/model in SKILL.

---

## 8. Migration

### Phase 1 — Runtime resolution + session override (deliverable first)

- [ ] `resolve_inference_profile()` in gateway
- [ ] `session_inference_bindings` table + CRUD
- [ ] Execution spawn/resume uses resolver (still accepts legacy inline `llm_config`)
- [ ] `/model` + `session.inference.*` RPC
- [ ] Causal events for override + resolution
- [ ] `InferenceSnapshot` fields on checkpoint (backward compatible serde defaults)
- [ ] Integration test: LLM error → `/model fallback` → resume completes turn

**Legacy path:** If manifest has `llm_config.provider` + `llm_config.model` and no
`llm_preset`, treat as `preset_source: legacy_inline` with warning log. No bootstrap
patching of inline models.

### Phase 2 — Manifest migration

- [x] Convert in-repo `agents/**/SKILL.md` to `llm_preset` (+ optional `llm_overrides`)
- [x] Update `specialized_builder` / `agent-factory` install contracts to emit
      `llm_preset` only
- [x] Remove `apply_llm_preset_to_skill()` provider/model regex patching
- [x] Update `docs/AGENTS.md`, `docs/reference/config.md`, config template

### Phase 3 — Failover polish

- [ ] Optional `on_error_preset` on fixed/routing presets in config
- [ ] Driver rebuild on provider change
- [ ] Cross-provider fallback (explicit opt-in per preset; never silent)

### Deprecations (Phase 2+)

| Field | Fate |
|-------|------|
| `llm_config.provider` / `model` in SKILL | Deprecated; legacy read-only |
| `llm_config.routing_preset` | Merged into `llm_preset` when preset is routing type |
| Bootstrap SKILL patching | Removed |
| `fallback_provider` / `fallback_model` on agent | Prefer routing fallback chain; wire or remove in Phase 3 |

---

## 9. Constitutional alignment

| Principle | How this RFC respects it |
|-----------|-------------------------|
| Lawful executor | Model changes are gateway-controlled, logged, never agent-discretionary |
| Reproducibility | Snapshots + causal chain; `runtime.lock` still does not pin LLM |
| Ri-0.6 | `chat_only` / tier downgrade guards block silent capability reduction |
| Separation of powers | Agent proposes tool intents; gateway picks inference profile |
| Bill of Rights | No new agent-facing obligation; operator controls override |

No constitution amendment required. If `on_error_preset` auto-failover ships,
document under existing budget/error yield rules (P-6.x checkpoint resume).

---

## 10. Test plan

| Test | Asserts |
|------|---------|
| `inference_resolve_agent_preset` | Manifest `llm_preset: smart` → routing active |
| `inference_resolve_mapping_fallback` | Agent without preset uses `llm_preset_mapping` |
| `session_inference_override` | `/model` changes resolved profile on next turn |
| `session_inference_clear` | Reverts to agent default |
| `inference_legacy_inline` | Old SKILL still runs; logs deprecation |
| `inference_chat_only_guard` | Override to chat_only preset rejected when tools required |
| `checkpoint_inference_snapshot` | Override survives hibernate + resume |
| `inference_failover_routing_chain` | Routing preset tries fallback models on 5xx |
| `emergency_stop_clears_binding` | Override removed with session grants |

---

## 11. Non-goals

- Agent self-service model picker tool
- Pinning LLM in `runtime.lock` or revision digest (model choice is not artifact identity)
- Honcho-style user modeling / per-user model preferences (future RFC)
- Replacing `llm_routing.agent_overrides` min_tier gates (complementary)

---

## 12. Open questions

1. **Child sessions:** Inherit root `preset_override` automatically (recommended) or
   require per-child override?
2. **Fork:** Copy `preset_override` to forked root session by default?
3. **`on_error_preset`:** Global config key vs per-preset field — prefer per-preset
   on routing presets (`smart.on_error: haiku`).
4. **Revision canonicalization:** Should `agent_revision_create_from_intent` strip
   resolved provider/model from stored SKILL if an agent submits inline values?

---

## 13. Summary

```
Agent bundle          Gateway config              Session (ephemeral)
─────────────         ──────────────              ───────────────────
llm_preset: smart  →  llm_presets.smart    →    preset_override: fallback
llm_overrides: ...    llm_preset_mapping        (operator, auditable)
instructions          tier / cost / routing
capabilities
runtime.lock
```

Hard-coded models leave the manifest. Ops tunes `llm_presets`; operators recover
from outages with `/model` without forking identity. The gateway remains the sole
authority that turns preset names into provider calls — consistent with Autonoetic's
existing separation between agent reasoners and the lawful executor.
