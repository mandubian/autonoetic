# Tool Error / Result Contract (P-5.11)

Every native tool reports failure through **one** uniform envelope — the
`ToolError` shape in `autonoetic-types/src/tool_error.rs`. This is constitutional
law (**P-5.11**); there is no second wrapper and no per-tool ad-hoc error JSON.

## The envelope

```jsonc
{
  "ok": false,                       // always false for a failure
  "error_type": "permission",        // coarse, closed enum (recoverability class)
  "error": "auditor_pass_missing",   // optional STABLE machine code (snake_case) — branch on this
  "message": "Promotion gate: auditor did not pass for artifact '…'.",
  "repair_hint": "Obtain an auditor pass record for this artifact, then retry.",
  // optional mechanical-classification fields (P-5.14), omitted when absent:
  "failure_class": …, "retry_advice": …, "retryable": …,
  "requires_external_event": …, "requires_human": …, "side_effect_state": …, "dedupe_key": …
}
```

- **`error_type`** — the coarse class that drives recoverability (`fatal` aborts
  the session per P-5.12/P-7.6; everything else is recoverable).
- **`error`** (the stable `code`) — finer-grained than `error_type`, so an
  orchestrator branches on **one field** instead of parsing `message` prose.
  Snake_case, stable across releases. Optional and additive; omitted when absent.
- **`message`** — human/LLM-readable prose. States the unmet precondition. It does
  **not** prescribe the orchestration plan (which agent to spawn, in what order) —
  that is the agent's reasoning, not the gateway's (the gateway is a Lawful
  Executor: it states the rule, it does not author the workflow).
- **`repair_hint`** — the mechanical remedy in terms of the rule ("obtain X, then
  retry"), again without naming specific agents to spawn.

## Err vs Ok — the rule

- **Expected, agent-correctable outcomes** (validation, permission/policy denial,
  gate/precondition unmet, not-found, conflict) → return `Ok(toolerror.to_json_string())`
  with `ok:false` + a stable `code`. The agent sees a normal tool result and reacts.
- **Genuine infrastructure failure** (DB open, serialization, IO) → return `Err`.
  The boundary (`tool_call_processor`) still renders it through the same envelope.

## How to construct one

```rust
use autonoetic_types::tool_error::ToolError;

// expected/blocked outcome — return as a tool result:
return Ok(ToolError::permission("Promotion gate: auditor did not pass for artifact 'x'.")
    .with_code("auditor_pass_missing")
    .with_repair_hint("Obtain an auditor pass record for this artifact, then retry.") // if a builder exists
    .to_json_string());
```

(Use the `error_type` constructor that matches the class: `validation`,
`permission`, `conflict`, `resource`, `not_found`, `execution`, `fatal`.)

## Stable code conventions

- snake_case, terse, stable. Name the *unmet precondition*, not the fix
  (`auditor_pass_missing`, not `run_auditor`).
- Reuse an existing code rather than minting a synonym. Current codes include
  `capability_delta_requires_approval`, `promotion_incomplete`,
  `protected_agent_requires_eval_run`, `sentinel_critical_findings_block_promotion`,
  `unresolved_dependencies`, …

## Rollout

The `error` code field is additive — existing tools keep working without it. New
and migrated tool failures should set a stable `code`. The migration of the
remaining bare-`anyhow` failure sites onto coded `ToolError`s proceeds
incrementally; the P-5.11 guard (`constitution_r_5_11_uniform_error_envelope.rs`)
enforces the envelope shape.

> The amendment that adds the `error` code to P-5.11 ships unsigned in
> `docs/constitution/versions/2026.06.16/`. The operator recomputes + signs the
> lock and activates the version before the code field is constitutionally
> blessed (see `CLAUDE.md` → "Recompute Constitution Lock").
