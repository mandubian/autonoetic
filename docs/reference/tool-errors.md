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
  gate/precondition unmet, not-found, conflict) → return `Ok(tool_error.to_error_response())`
  with `ok:false` + a stable `code`. The agent sees a normal tool result and reacts.
- **Genuine infrastructure failure** (DB open, serialization, IO) → return `Err`.
  The boundary (`tool_call_processor`) still renders it through the same envelope.

## How to construct one

```rust
use autonoetic_types::tool_error::ToolError;

// Expected/blocked outcome — return it as a tool result (ok:false), not an Err.
// `permission` takes only a message; attach the stable code with `.with_code`:
return Ok(ToolError::permission("Promotion gate: auditor did not pass for artifact 'x'.")
    .with_code("auditor_pass_missing")
    .to_error_response());

// Constructors that accept an optional repair_hint (validation / execution /
// conflict / resource / not_found) set it inline:
return Ok(ToolError::validation(
        "content_write requires both `name` and `content`",
        Some("Provide both fields, then retry."),
    )
    .with_code("missing_required_field")
    .to_error_response());
```

Use the constructor that matches the class: `validation`, `permission`,
`conflict`, `resource`, `not_found`, `execution`, `fatal`. `to_error_response()`
is the repo-wide convention for rendering the envelope to a tool-result string.

## Stable code conventions

- snake_case, terse, stable. Name the *unmet precondition*, not the fix
  (`auditor_pass_missing`, not `run_auditor`).
- Reuse an existing code rather than minting a synonym. Current codes include
  `capability_delta_requires_approval`, `promotion_incomplete`,
  `protected_agent_requires_eval_run`, `sentinel_critical_findings_block_promotion`,
  `unresolved_dependencies`, …

## Delegation failure kinds

Three `ToolErrorKind` values exist for failures of *delegated* work, where the
parent needs to know that retrying the identical call cannot help:

| Kind | Means | Retry |
|---|---|---|
| `output_contract_unmet` | The child completed but did not produce the `expected_outputs` its parent declared | **No blind retry** — the contract failed, not the attempt. Re-delegate, decompose, relax the contract, or escalate |
| `child_gave_up` | The session ended cleanly with no result and no account. Distinct from `unknown`, which is an unclassifiable *error*; this is a child that stopped without explaining | Parent reasons. Counts toward the parent loop guard |
| `bad_reference` | A name or handle that does not exist in the session — hallucinated or mutated ref | **No retry of the same ref**: a deterministic lookup failure cannot succeed on repetition. Pick a real one |

`retry_advice` carries the policy separately from the kind — `wait`,
`retry_same_stage`, `retry_after_external_recovery`, `do_not_retry`,
`escalate_human`. Keeping *what happened* apart from *what may be done about it*
is what lets a parent branch on one field, and what stops "recoverable" from
being read as "retry the same thing".

How these are produced and what enforces them is in
[`../internals/task-survival.md`](../internals/task-survival.md).

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
