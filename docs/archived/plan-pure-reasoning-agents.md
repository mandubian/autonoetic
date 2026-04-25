# Plan: Pure reasoning agents without artifact requirement

## Problem

Most operational agents just call existing gateway tools (`credential_request`,
`memory.*`, `web_fetch`, `scheduler.cron.*`) and reason on the results. They have
no custom code — no Python, no bash, no scripts. Yet the current
`agent_revision_create_from_intent` requires `artifact_id`, forcing the planner to
route through the coder just to produce an empty bundle.

This makes the common case (service-oriented reasoning agents) the hard path.
The rare case (agents with custom code) should be the one that requires extra steps.

### Concrete example: Moltbook operational agent

After the planner completes Moltbook registration via `credential_setup`, it needs
to create a persistent agent that:
1. Posts to the Moltbook feed via `credential_request`
2. Checks for new replies every 5 minutes via `BackgroundReevaluation`
3. Decides whether to respond (LLM reasoning)
4. Persists state between wake-ups via `memory.write`

This agent has **zero custom code**. Its SKILL.md is just instructions + capabilities.
Today this requires: coder builds an empty artifact → builder installs it.
After this plan: builder calls `create_from_intent` without `artifact_id` → done.

---

## Design principle: capability enforcement is the gate

The gateway already enforces capabilities on **every tool call**. A reasoning agent
with `CredentialAccess: [moltbook]` + `NetworkAccess: [localhost]` can only:
- Make HTTP calls through `credential_request` (secrets injected server-side)
- Reach localhost (gateway blocks all other hosts)
- Read/write its own memory

It cannot execute arbitrary code, spawn children, or escape its capability set.
The capability declaration **is** the security contract — the gateway enforces it
mechanically, every time, no exceptions.

### When an artifact is required vs not

| Agent type | Artifact? | Promotion gate |
|---|---|---|
| Reasoning, service-tier caps only (`CredentialAccess`, `NetworkAccess`, `Read/Write`, `Memory`, `BackgroundReevaluation`, `SchedulerAccess`) | **No** | Direct promote — capability enforcement is the gate |
| Reasoning, with `CodeExecution` or `AgentSpawn` | **Yes** | Full eval + audit (agent can execute arbitrary code or create children) |
| Script mode (any caps) | **Yes** | Full eval + audit (script is code) |

The rule is simple: **if you execute code or spawn agents, you need a reviewed artifact.
If you only use existing tools, capabilities are sufficient.**

---

## Changes

### 1. `RevisionCreateFromIntentArgs`: `artifact_id` becomes optional

**File:** `autonoetic-gateway/src/runtime/tools/agent_revision.rs`

```rust
struct RevisionCreateFromIntentArgs {
    agent_id: String,
    artifact_id: Option<String>,  // was: String
    instructions: String,
    description: String,
    // ... rest unchanged
}
```

**Schema change:** remove `artifact_id` from required array; update description:
```json
"artifact_id": {
    "type": "string",
    "description": "Artifact ID containing agent source files. Required for script agents and reasoning agents that use CodeExecution/AgentSpawn. Omit for pure reasoning agents that only use existing tools."
}
```

### 2. `RevisionCreateCommonArgs`: `artifact_id` becomes optional

Same file:
```rust
struct RevisionCreateCommonArgs {
    agent_id: String,
    artifact_id: Option<String>,  // was: String
    // ... rest unchanged
}
```

All uses of `common.artifact_id` must handle `Option`: the revision record's
`artifact_id` field is already `Option<String>`, so this is a natural fit.

### 3. `create_from_intent` execution: two paths

**When `artifact_id` is `Some(id)` (unchanged path):**
Everything works exactly as today. Artifact loaded, files resolved, health
analyzed, layers validated, promotion store reconciled.

**When `artifact_id` is `None` (new reasoning-only path):**

Validation:
- `execution_mode` must resolve to `Reasoning` (error if `Script`)
- `llm_config` must be provided
- Capabilities must NOT include `CodeExecution` or `AgentSpawn` (error:
  "agents with CodeExecution/AgentSpawn require an artifact_id for
  code review and promotion gating")

Execution (skipping the artifact-dependent blocks):
- `file_map` starts empty (no source files)
- Skip `artifact_store.inspect()` — no artifact to load
- Skip `artifact_store.resolve_files()` — no files to resolve
- Skip `analyze_bundle_health()` — no code to analyze
- `scaffold_runtime_lock(None, None, &[])` — empty layers
- Generate canonical SKILL.md from intent params (already works)
- Generate canonical runtime.lock (already works with empty layers)
- `source_kind: "intent_reasoning"` (distinguishes from `"intent_artifact"`)
- `source_ref: None` (no artifact to reference)

### 4. `create_revision_from_files`: handle absent bundle

The function currently takes `bundle: &ArtifactBundle`. Change to
`bundle: Option<&ArtifactBundle>`:

- **Layer validation** (line 464-471): when `bundle` is `None`, `expected_layers`
  is empty. The normalized lock also has empty layers. Validation passes.
- **Script entry shebang check** (line 476-487): when `bundle` is `None`, no
  script entry exists. Skip.
- **Promotion store reconciliation** (line 567-569): when `artifact_id` is `None`,
  skip `reconcile_content_digest_for_revision` — there's no artifact to reconcile.
- **Revision record** (line 541): `artifact_id` is already `Option<String>`, so
  `common.artifact_id.clone()` naturally becomes `None`.

### 5. Promotion gate: skip for service-tier reasoning agents

**File:** `autonoetic-gateway/src/runtime/tools/agent_revision.rs` (promote function)

Currently (line 1488-1541): if `has_high_risk` is true, the gate requires
`artifact_id` and looks up promotion records.

New logic:
```
if has_high_risk {
    if rev.artifact_id.is_some() {
        // Existing path: look up promotion record by artifact_id.
        // (Code execution / agent spawn agents always have artifacts.)
    } else {
        // Reasoning agent with NetworkAccess but no CodeExecution/AgentSpawn.
        // This should not happen: create_from_intent blocks artifact-free
        // agents with CodeExecution/AgentSpawn. If it does happen (e.g.,
        // revision created through a different path), reject promotion.
        bail!("Promotion gate: revision has high-risk capabilities but no
               artifact_id. Only agents with CodeExecution/AgentSpawn require
               artifacts, and those must have been created with one.")
    }
}
```

Wait — `NetworkAccess` is currently classified as high-risk. But a reasoning
agent with `NetworkAccess` + `CredentialAccess` (no `CodeExecution`) should be
promotable without an artifact. The fix:

**Refine `is_high_risk_capability`** to distinguish two tiers:

```rust
/// Capabilities that require a reviewed artifact (code execution boundary).
pub fn requires_artifact_review(cap: &Capability) -> bool {
    matches!(cap, Capability::CodeExecution { .. } | Capability::AgentSpawn { .. })
}

/// Capabilities that are high-risk for network/spawn (existing classification).
/// Used when artifact_id IS present to determine if eval/audit gate applies.
pub fn is_high_risk_capability(cap: &Capability) -> bool {
    matches!(
        cap,
        Capability::NetworkAccess { .. }
            | Capability::CodeExecution { .. }
            | Capability::AgentSpawn { .. }
    )
}
```

**New promotion logic:**
```
let needs_artifact_gate = capabilities.iter().any(requires_artifact_review);

if needs_artifact_gate {
    // Must have artifact_id. Full eval + audit gate (unchanged).
    let artifact_id = rev.artifact_id.as_deref().ok_or_else(|| ...)?;
    // ... existing promotion record lookup ...
} else if has_high_risk && rev.artifact_id.is_some() {
    // Has artifact and NetworkAccess (but not CodeExecution/AgentSpawn).
    // Still apply the existing gate since code was provided.
    let artifact_id = rev.artifact_id.as_deref().unwrap();
    // ... existing promotion record lookup ...
}
// else: no artifact, no CodeExecution/AgentSpawn → direct promote.
// Capability enforcement is the gate.
```

### 6. Builder SKILL.md: add reasoning-only workflow

**File:** `agents/evolution/specialized_builder.default/SKILL.md`

Add a section:

```markdown
### Reasoning-Only Agent Installation (no artifact)

For agents that only use existing gateway tools (credential_request, memory.*,
web_fetch, scheduler.cron.*, etc.) and contain **no custom code**:

1. Call `agent_revision_create_from_intent` **without** `artifact_id`:
   ```json
   {
     "agent_id": "moltbook-ops",
     "description": "Operational Moltbook agent — posts to feed and monitors replies",
     "instructions": "# Moltbook Operations\n\n...",
     "execution_mode": "reasoning",
     "llm_config": {
       "provider": "openrouter",
       "model": "google/gemini-3-flash-preview",
       "temperature": 0.2
     },
     "capabilities": [
       {"type": "CredentialAccess", "services": ["moltbook"]},
       {"type": "NetworkAccess", "hosts": ["localhost"]},
       {"type": "ReadAccess", "scopes": ["self.*"]},
       {"type": "WriteAccess", "scopes": ["self.*"]},
       {"type": "BackgroundReevaluation", "min_interval_secs": 300, "allow_reasoning": true}
     ]
   }
   ```

2. Call `agent_revision_promote` with the returned revision_id.

**Rules for artifact-free agents:**
- `execution_mode` must be `reasoning` (script agents always need artifacts)
- `CodeExecution` and `AgentSpawn` are forbidden (these require code review)
- All other capabilities work: CredentialAccess, NetworkAccess, Read/Write,
  BackgroundReevaluation, SchedulerAccess
```

---

## What does NOT change

- `artifact_build` — untouched (still used for code agents)
- `credential_setup` / `credential_request` — untouched
- Execution engine — reasoning mode already works
- Script mode agents — still require artifacts
- Gateway policy/capability enforcement — already enforces on every tool call
- `BackgroundReevaluation` / scheduler — already works
- `PromotionStore` internals — unchanged (artifact-free agents skip the gate)
- The evaluator/auditor agents — unchanged (they only run for code agents)

---

## Files to change

| File | Change |
|---|---|
| `autonoetic-gateway/src/runtime/tools/agent_revision.rs` | `artifact_id` → `Option<String>` in args; two-path execution in `create_from_intent`; `create_revision_from_files` takes optional bundle; refined promotion gate in `promote` |
| `autonoetic-gateway/src/runtime/install_contract.rs` | Add `requires_artifact_review()` alongside existing `is_high_risk_capability()` |
| `agents/evolution/specialized_builder.default/SKILL.md` | Add reasoning-only agent installation workflow |

---

## Verification

1. `cargo build -p autonoetic-gateway` — no errors
2. `cargo test -p autonoetic-gateway` — existing tests pass (no regressions)
3. Manual test: create a reasoning agent without artifact_id
   - `create_from_intent` with `llm_config` + capabilities (no `artifact_id`, no `script_entry`)
   - Should succeed with `source_kind: "intent_reasoning"`
   - `promote` should succeed without promotion record lookup
4. Manual test: verify artifact is still required for code agents
   - `create_from_intent` without `artifact_id` but with `CodeExecution` → should fail
   - `create_from_intent` without `artifact_id` but with `execution_mode: "script"` → should fail
5. Full Moltbook demo:
   - Planner reads prose skill.md
   - Executes registration via `credential_setup`
   - Creates operational reasoning agent via builder (no artifact)
   - Operational agent posts to feed, checks for replies on background wake-up
