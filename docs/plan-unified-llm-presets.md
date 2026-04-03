# Plan: Unified LLM Presets — Routing as Dynamic Presets

**Goal:** Merge the two LLM config registries (`llm_presets` + `llm_routing.models`) into one. Routing strategies become dynamic presets that select from fixed presets at call time. Role mapping stays the same surface — values are always preset names.

**Why:** Currently, `llm_presets` and `llm_routing.models` are two separate registries for the same thing (LLM provider/model definitions). The preset's `provider`/`model` gets overridden by routing anyway. This creates confusion about which config wins, duplicates tier/cost info, and makes per-role routing impossible without agent_overrides hacks.

**Breaking change:** The old `llm_routing` fields (`strategy`, `models`, `deterministic`, `classifier`, `hybrid`) are **removed**. All model definitions and routing strategies now live in `llm_presets`. Existing configs must be updated.

---

## Target Config Shape

```yaml
# ── Fixed presets: concrete model definitions with tier/cost ──
llm_presets:
  haiku:
    provider: anthropic
    model: claude-haiku-3-20250307
    tier: economy
    cost:
      input_per_million: 0.25
      output_per_million: 1.25

  sonnet:
    provider: anthropic
    model: claude-sonnet-4-20250514
    tier: standard
    cost:
      input_per_million: 3.0
      output_per_million: 15.0

  opus:
    provider: anthropic
    model: claude-opus-4-20250514
    tier: premium
    cost:
      input_per_million: 15.0
      output_per_million: 75.0

  local:
    provider: lmstudio
    model: qwen-3
    base_url: http://localhost:1234/v1
    tier: economy

  # ── Routing presets: select from fixed presets at call time ──
  smart:
    routing:
      strategy: hybrid
      models: [opus, sonnet, haiku]        # fixed preset names
      classifier_preset: haiku             # fixed preset for classification
      deterministic:
        max_tier: premium
        budget_downgrade_threshold: 0.8
        enable_fallback_chain: true
      classifier:
        timeout_secs: 2
        skip_threshold: 0.95
      hybrid:
        ambiguity_threshold: 0.5

  budget:
    routing:
      strategy: deterministic
      models: [sonnet, haiku]
      deterministic:
        max_tier: standard
        max_cost_usd: 2.0
        budget_downgrade_threshold: 0.6
        enable_fallback_chain: true

# ── Role mapping (unchanged surface) ──
# Values are preset names. Fixed or routed — the mapping doesn't care.
llm_preset_mapping:
  planner: smart           # routed
  coder: smart             # routed
  researcher: sonnet       # fixed
  debugger: haiku          # fixed
  evaluator: budget        # routed
  architect: opus          # fixed
  default: sonnet          # fixed

# ── Cross-cutting routing concerns ──
llm_routing:
  agent_overrides:
    planner.production:
      min_tier: premium
  approval_gates:
    premium_model_first_use: false
    budget_threshold_crossed: 0.75
```

---

## Validation Rules

1. `routing.models` must reference **fixed presets only** (not routing presets). Enforced at config load.
2. `classifier_preset` must reference a **fixed preset**. Enforced at config load.
3. All referenced preset names must exist. Config load fails fast.
4. A preset with `routing` cannot also have `provider`/`model`. Mutually exclusive.
5. `llm_preset_mapping` values can reference either fixed or routing presets transparently.

---

## Implementation Phases

### Phase 1 — Type Changes (autonoetic-types)

**1.1 Extend `LlmPreset` with routing + tier/cost fields**

File: `autonoetic-types/src/config.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmPreset {
    // ── Fixed preset fields (mutually exclusive with routing) ──
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub fallback_provider: Option<String>,
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub chat_only: Option<bool>,
    #[serde(default)]
    pub context_window_tokens: Option<u32>,
    #[serde(default)]
    pub base_url: Option<String>,

    // ── Tier/cost (used by fixed presets when referenced by routing) ──
    #[serde(default)]
    pub tier: Option<CapabilityTier>,
    #[serde(default)]
    pub cost: Option<ModelCost>,
    #[serde(default)]
    pub latency: Option<ModelLatency>,

    // ── Routing preset fields (mutually exclusive with provider/model) ──
    #[serde(default)]
    pub routing: Option<RoutingPresetConfig>,
}
```

**1.2 Define `RoutingPresetConfig`**

```rust
/// Routing configuration within a preset. When present, the preset is a
/// dynamic preset that selects from other (fixed) presets at call time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingPresetConfig {
    pub strategy: RoutingStrategy,
    /// Fixed preset names to route between. Must all be fixed presets.
    #[serde(default)]
    pub models: Vec<String>,
    /// Fixed preset name for the classifier model (classifier/hybrid strategies).
    #[serde(default)]
    pub classifier_preset: Option<String>,
    /// Deterministic strategy settings.
    #[serde(default)]
    pub deterministic: DeterministicRoutingConfig,
    /// Classifier strategy settings.
    #[serde(default)]
    pub classifier: ClassifierRoutingConfig,
    /// Hybrid strategy settings.
    #[serde(default)]
    pub hybrid: HybridRoutingConfig,
}
```

**1.3 Strip `LlmRoutingConfig` down to cross-cutting concerns only**

Remove: `strategy`, `models`, `deterministic`, `classifier`, `hybrid` fields. Keep only:

```rust
/// Cross-cutting routing concerns (agent overrides and approval gates).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmRoutingConfig {
    /// Agent-specific overrides (agent_id → min_tier or explicit model).
    #[serde(default)]
    pub agent_overrides: std::collections::HashMap<String, ModelOverride>,
    /// Approval gates for routing decisions.
    #[serde(default)]
    pub approval_gates: ApprovalGatesConfig,
}
```

**1.4 Remove `ModelEntry` struct**

`ModelEntry` is replaced by fixed presets. All model metadata (provider, model, tier, cost, latency, context_window_tokens, base_url) lives on `LlmPreset` now. Delete `ModelEntry`, `ModelCost`, `ModelLatency` stay (used by `LlmPreset`).

**1.5 Add `routing_preset` to `LlmConfig` (agent manifest)**

File: `autonoetic-types/src/agent.rs`

```rust
pub struct LlmConfig {
    pub provider: String,
    pub model: String,
    // ... existing fields ...
    /// When set, this agent's LLM is resolved via the named routing preset
    /// at call time. provider/model are the fallback if routing is unavailable.
    #[serde(default)]
    pub routing_preset: Option<String>,
}
```

**1.6 Add config validation**

```rust
impl GatewayConfig {
    /// Validate that preset references are consistent.
    pub fn validate_llm_presets(&self) -> Result<(), Vec<String>> { ... }
}
```

Called at config load time. Checks:
- Every preset is either fixed (`provider`+`model` set) or routed (`routing` set), not both, not neither
- `routing.models` references only fixed presets that exist
- `classifier_preset` references a fixed preset that exists
- `llm_preset_mapping` values reference presets that exist

**Files changed:**
- `autonoetic-types/src/config.rs` — `LlmPreset` extended, `RoutingPresetConfig` new, `LlmRoutingConfig` trimmed, `ModelEntry` removed, validation
- `autonoetic-types/src/agent.rs` — `LlmConfig.routing_preset`

---

### Phase 2 — Preset Resolution (autonoetic-gateway)

**2.1 Add preset resolution helper**

File: `autonoetic-gateway/src/runtime/llm_preset_resolver.rs` (new file)

```rust
/// Resolves a preset name to either a concrete LlmConfig (fixed preset)
/// or delegates to routing (dynamic preset).
pub struct LlmPresetResolver<'a> {
    presets: &'a HashMap<String, LlmPreset>,
    routing_config: Option<&'a LlmRoutingConfig>,
}

impl<'a> LlmPresetResolver<'a> {
    pub fn new(
        presets: &'a HashMap<String, LlmPreset>,
        routing_config: Option<&'a LlmRoutingConfig>,
    ) -> Self { ... }

    /// Resolve a preset name to a concrete LlmConfig.
    /// For fixed presets: returns the config directly.
    /// For routing presets: runs the router and returns the routed config.
    pub async fn resolve(
        &self,
        preset_name: &str,
        ctx: &RoutingContext,
    ) -> anyhow::Result<LlmConfig> { ... }

    /// Resolve the primary (first) model from a routing preset's models list.
    /// Used at session start to build the initial driver.
    pub fn resolve_primary(&self, preset_name: &str) -> anyhow::Result<LlmConfig> { ... }

    /// Check if a preset is fixed or routed.
    pub fn is_routing_preset(&self, name: &str) -> bool { ... }

    /// Resolve routing preset names to LlmConfig list for the router.
    fn resolve_model_list(&self, model_names: &[String]) -> Vec<(String, LlmConfig, CapabilityTier)> { ... }
}
```

The key method `resolve_model_list()` converts preset names to their concrete configs by looking up each fixed preset and extracting its `provider`, `model`, `tier`, `cost`, `latency`, `context_window_tokens`, `base_url`.

**2.2 Update `llm_preset_to_config()` to handle new fields**

File: `autonoetic-gateway/src/runtime/post_session_digest.rs`

The existing `llm_preset_to_config()` needs to handle `provider`/`model` being `Option<String>` and the new `tier`/`cost` fields.

**Files changed:**
- `autonoetic-gateway/src/runtime/llm_preset_resolver.rs` (new)
- `autonoetic-gateway/src/runtime/post_session_digest.rs` — update conversion

---

### Phase 3 — Router Refactor (autonoetic-gateway)

**3.1 Update router factory to work with preset names**

File: `autonoetic-gateway/src/runtime/model_router.rs`

The router factory currently takes `RoutingStrategy` + `&LlmRoutingConfig` (which contained `models: Vec<ModelEntry>`). Refactor to take resolved data from presets:

```rust
/// Create router from a routing preset config.
/// `resolved_models`: (preset_name, LlmConfig, tier) tuples from resolving the preset's models list.
/// `classifier_config`: resolved LlmConfig from classifier_preset, if applicable.
pub fn create_router_from_preset(
    preset: &RoutingPresetConfig,
    resolved_models: &[(String, LlmConfig, CapabilityTier)],
    classifier_config: Option<LlmConfig>,
) -> Box<dyn ModelRouter> { ... }
```

**3.2 Refactor `LlmClassifierRouter`**

Remove `classifier_provider`/`classifier_model` fields. Instead receive a resolved `LlmConfig` for the classifier (looked up from `classifier_preset`). The `call_classifier()` method builds the driver from this resolved config.

**3.3 Refactor fallback chain builder**

`build_fallback_chain()` currently takes `Vec<ModelEntry>`. Refactor to take `&[(String, LlmConfig, CapabilityTier)]` (resolved from preset names). Same logic — filter by tier, exclude primary.

**3.4 Refactor `DeterministicRouter::route()`**

Currently looks up `primary_config` in `routing_config.models` to find the `ModelEntry`. Refactor to look up by matching `provider`+`model` in the resolved models list.

**3.5 Update `RoutingDecision`**

The `fallback_chain` field currently holds `(String, String)` = `(provider, model)`. Change to `(String, String, String)` = `(preset_name, provider, model)` so the resolver can trace back to the preset for `base_url`, `cost`, etc.

**Files changed:**
- `autonoetic-gateway/src/runtime/model_router.rs` — factory, classifier, fallback, all routers

---

### Phase 4 — Lifecycle Integration (autonoetic-gateway)

**4.1 Replace global routing block with preset-based routing**

File: `autonoetic-gateway/src/runtime/lifecycle.rs`

Replace the current routing block (lines ~962-1061) with:

```rust
// --- Model Routing ---
let routing_result = if let Some(preset_name) = &self.resolved_preset_name {
    let resolver = LlmPresetResolver::new(&config.llm_presets, config.llm_routing.as_ref());
    if resolver.is_routing_preset(preset_name) {
        Some(resolver.resolve(preset_name, &routing_ctx).await?)
    } else {
        None // fixed preset, no routing
    }
} else {
    None
};
```

The old path that checked `config.llm_routing.strategy` + `config.llm_routing.models` is removed entirely.

**4.2 Thread preset name through AgentExecutor**

```rust
pub struct AgentExecutor {
    // ... existing fields ...
    resolved_preset_name: Option<String>,  // NEW
}
```

Set during `ExecutionEngine::start_session()` when resolving the agent's LLM config.

**4.3 Update `ExecutionEngine::start_session()`**

File: `autonoetic-gateway/src/execution.rs`

```rust
// Resolve which preset name was used for this agent
let preset_name = loaded.manifest.llm_config.as_ref()
    .and_then(|c| c.routing_preset.clone());

let llm_config = loaded.manifest.llm_config.clone()
    .ok_or_else(|| anyhow!("missing llm_config"))?;

let driver = if let Some(ref name) = preset_name {
    let resolver = LlmPresetResolver::new(&self.config.llm_presets, ...);
    if resolver.is_routing_preset(name) {
        let primary = resolver.resolve_primary(name)?;
        build_driver(primary, self.http_client.clone())?
    } else {
        build_driver(llm_config, self.http_client.clone())?
    }
} else {
    build_driver(llm_config, self.http_client.clone())?
};

// ... pass preset_name to AgentExecutor
```

**Files changed:**
- `autonoetic-gateway/src/runtime/lifecycle.rs` — routing resolution, `resolved_preset_name` field
- `autonoetic-gateway/src/execution.rs` — driver build + preset name resolution

---

### Phase 5 — CLI Integration (autonoetic)

**5.1 Update `resolve_llm_config()` for routing presets**

File: `autonoetic/src/cli/agent.rs`

When the mapped preset is a routing preset:
- Use the first model from `routing.models` as the manifest's concrete `provider`/`model`
- Set `routing_preset: <name>` in the manifest's `LlmConfig`

```rust
fn preset_to_config(name: &str, preset: &LlmPreset, all_presets: &HashMap<String, LlmPreset>) -> LlmTemplateConfig {
    if let Some(routing) = &preset.routing {
        // Routing preset: use first model as concrete fallback, store preset name
        let first_model_name = routing.models.first()
            .ok_or_else(|| anyhow!("routing preset '{}' has no models", name));
        let first_preset = all_presets.get(first_model_name)
            .ok_or_else(|| anyhow!("model preset '{}' not found", first_model_name))?;
        LlmTemplateConfig {
            provider: first_preset.provider.clone().unwrap_or_default(),
            model: first_preset.model.clone().unwrap_or_default(),
            temperature: preset.temperature.unwrap_or(0.2),
            routing_preset: Some(name.to_string()),
            ..Default::default()
        }
    } else {
        // Fixed preset
        LlmTemplateConfig {
            provider: preset.provider.clone().unwrap_or_default(),
            model: preset.model.clone().unwrap_or_default(),
            temperature: preset.temperature.unwrap_or(0.2),
            routing_preset: None,
            ..Default::default()
        }
    }
}
```

**5.2 Update `LlmTemplateConfig`**

```rust
pub struct LlmTemplateConfig {
    pub provider: String,
    pub model: String,
    pub temperature: f64,
    pub chat_only: bool,
    pub base_url: Option<String>,
    pub routing_preset: Option<String>,  // NEW
}
```

**5.3 Update `render_skill_template()`**

Write `routing_preset:` into SKILL.md frontmatter when present.

**Files changed:**
- `autonoetic/src/cli/agent.rs` — `resolve_llm_config()`, `LlmTemplateConfig`, `render_skill_template()`

---

### Phase 6 — Clean Up Deleted Code

**6.1 Delete `ModelEntry` and related types**

File: `autonoetic-types/src/config.rs`

Remove:
- `ModelEntry` struct (replaced by fixed presets)
- `ModelCost` and `ModelLatency` stay (used by `LlmPreset`)
- `LlmRoutingConfig.strategy` field
- `LlmRoutingConfig.models` field
- `LlmRoutingConfig.deterministic` field
- `LlmRoutingConfig.classifier` field
- `LlmRoutingConfig.hybrid` field

**6.2 Delete `decision_to_llm_config()`**

File: `autonoetic-gateway/src/runtime/model_router.rs`

This function converts `RoutingDecision` + `ModelEntry` → `LlmConfig`. With the new design, `LlmPresetResolver.resolve()` returns an `LlmConfig` directly from the preset lookup. Delete the old function.

**6.3 Update `create_router()` factory**

The old `create_router(strategy, &LlmRoutingConfig)` is replaced by `create_router_from_preset()`. Delete the old factory.

**6.4 Update all callers of deleted code**

Search for all references to `ModelEntry`, `decision_to_llm_config`, old `create_router`, `LlmRoutingConfig.models`, `LlmRoutingConfig.strategy` and update them.

**Files changed:**
- `autonoetic-types/src/config.rs` — delete types
- `autonoetic-gateway/src/runtime/model_router.rs` — delete old factory + conversion
- All files referencing deleted types

---

### Phase 7 — Config Template + Docs

**7.1 Update `config/config-template.yaml`**

Replace the entire `llm_routing` section with the new unified format. Show fixed presets with `tier`/`cost`, routing presets with `routing:`, and the slimmed `llm_routing` for overrides/gates only.

**7.2 Update `docs/config-reference.md`**

- Update `LLM Presets` section: document fixed vs routing presets
- Document `routing` sub-object within presets
- Document `tier`, `cost`, `latency` fields on presets
- Rewrite `LLM Routing` section: agent_overrides + approval_gates only
- Remove all references to `llm_routing.strategy`, `llm_routing.models`

**7.3 Update `docs/plan-hermes-gap-closure.md`**

Add note to Feature 2 about the unified preset refactor.

**Files changed:**
- `config/config-template.yaml`
- `docs/config-reference.md`
- `docs/plan-hermes-gap-closure.md`

---

### Phase 8 — Tests

**8.1 Type validation tests** (`autonoetic-types/src/config.rs`)

- Fixed preset: `provider`+`model` required when no `routing`
- Routing preset: `routing` required when no `provider`/`model`
- Mutual exclusivity: `provider`/`model` + `routing` = error
- `routing.models` references only existing fixed presets
- `classifier_preset` references existing fixed preset
- `llm_preset_mapping` values reference existing presets
- Empty preset = error

**8.2 Resolution tests** (`autonoetic-gateway/src/runtime/llm_preset_resolver.rs`)

- Fixed preset → LlmConfig conversion
- Routing preset → resolve primary
- Routing preset → resolve with context (deterministic downgrade)
- Routing preset references non-existent preset → error
- Routing preset references another routing preset → error
- Routing preset with empty models list → error
- `classifier_preset` references routing preset → error

**8.3 Router tests** (`autonoetic-gateway/src/runtime/model_router.rs`)

- All existing tests updated to use `create_router_from_preset()` + resolved model list
- Deterministic routing with preset-referenced models
- Classifier routing with resolved classifier config
- Hybrid routing with ambiguity heuristic
- Fallback chain from preset-referenced models
- Agent overrides still enforced

**8.4 Integration tests**

- Full flow: preset mapping → routing preset → router → routed model → driver → completion
- Fallback chain execution with preset-referenced models
- Cost estimation uses correct preset's cost info
- Agent with `routing_preset` in SKILL.md resolves correctly
- Agent with fixed preset (no `routing_preset`) works unchanged

**Files:**
- `autonoetic-types/src/config.rs` — validation tests
- `autonoetic-gateway/src/runtime/llm_preset_resolver.rs` — resolution tests
- `autonoetic-gateway/src/runtime/model_router.rs` — updated router tests
- `autonoetic-gateway/tests/` — e2e tests

---

## Summary of Files Changed

| File | Changes |
|------|---------|
| `autonoetic-types/src/config.rs` | `LlmPreset` extended, `RoutingPresetConfig` new, `LlmRoutingConfig` trimmed, `ModelEntry` deleted, validation |
| `autonoetic-types/src/agent.rs` | `LlmConfig.routing_preset` field |
| `autonoetic-gateway/src/runtime/llm_preset_resolver.rs` | **New file** — preset resolution logic |
| `autonoetic-gateway/src/runtime/model_router.rs` | Refactored factory, classifier, fallback; old factory deleted |
| `autonoetic-gateway/src/runtime/lifecycle.rs` | Preset-based routing, `resolved_preset_name` field |
| `autonoetic-gateway/src/execution.rs` | Driver build with preset awareness |
| `autonoetic-gateway/src/runtime/post_session_digest.rs` | Updated preset conversion |
| `autonoetic/src/cli/agent.rs` | `resolve_llm_config()`, `LlmTemplateConfig`, `render_skill_template()` |
| `config/config-template.yaml` | New unified format |
| `docs/config-reference.md` | Updated reference |
