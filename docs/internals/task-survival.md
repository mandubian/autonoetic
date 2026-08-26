# Task survival: how delegated work fails without being lost

A parent delegates to a child. The child is non-deterministic, may lack a
capability, may run out of budget, may return nothing. The question this
subsystem answers is not "how do we prevent that" — you cannot — but **how does
a task fail in a way the parent can act on**.

The governing rule is that the gateway *executes* pre-committed policy and does
not judge. It classifies a failure, applies the policy registered for that
class, and hands the parent a branch table. Which of those branches to take is
the parent's reasoning; whether a retry is even permitted is not.

## Failures are typed, and typing is what forbids the blind retry

Delegation failures arrive in the standard `ToolError` envelope
([`../reference/tool-errors.md`](../reference/tool-errors.md)) with a kind that
says *what shape of thing went wrong*, plus separate retry advice. The
separation matters: a kind is a fact about the failure, while advice is policy
over that fact, and mixing them is what produces retry loops.

Three kinds exist specifically for delegation:

| Kind | Means | Why retrying identically cannot work |
|---|---|---|
| `OutputContractUnmet` | The child "succeeded" but did not produce the `expected_outputs` the parent declared | The contract, not the attempt, is what failed. Something structural must change |
| `ChildGaveUp` | The session ended cleanly with no result and no account | Distinct from `Unknown`: this is not an unclassifiable error, it is a child that stopped without explaining. Counts toward the parent loop guard |
| `BadReference` | A name or handle that does not exist in the session — hallucinated or mutated | A deterministic lookup failure. The same reference will never resolve; pick a real one |

`RetryAdvice` carries the policy: `Wait`, `RetrySameStage`,
`RetryAfterExternalRecovery`, `DoNotRetry`, `EscalateHuman`. A parent branches on
one field instead of parsing prose, and spends reasoning only where reasoning
helps — deciding *how* to restructure, not *whether* the gateway will let it
try again.

## `expected_outputs` is a contract, checked at completion

A parent declares what a child will produce. That declaration is checked when
the child finishes: outputs absent means `OutputContractUnmet`, not success.

Without the check the failure is silent, and silence is the expensive part — the
parent proceeds on the assumption that work exists, and the error surfaces
somewhere unrelated.

### Re-spawning the same contract is itself a trip condition

The dangerous loop is not one failure, it is a parent that responds to
`OutputContractUnmet` by spawning the same child with the same contract again.
The LoopGuard detects that directly, keyed on **structural identity**:

```
hash(agent_id + expected_outputs + message_digest)
```

Reaching `max_spawn_identity_repeats` trips `RepeatedSpawnIdentity`
(`runtime/guard.rs`). Setting the threshold to `0` disables the detector.

The hash is deliberately strict — trivially rewording the message evades it. That
is a known limit, kept on purpose: measure real evasion before loosening, because
a fuzzy identity check that fires on legitimate retries is worse than a strict one
that misses some.

## Fail at plan time, not at step 7

A plan step that needs a capability its agent does not hold will fail — the only
question is whether it fails now or after six successful steps have already
spent budget. The preflight (`runtime/plan_preflight.rs`) resolves declared
`required_capabilities` against the agent registry for every step, before
execution, and reports per-step findings.

It is **purely static** by design (I-10): no LLM calls, no network fetches, no
hidden branches. Capability lookup goes through a trait, so the logic is
testable without a running gateway. Wired into `planframe_propose` and
`planframe_amend`, so a plan cannot be accepted with a step that was already
known to be impossible.

## Burn rate belongs in the attestation

Budget exhaustion mid-task is a survival problem, so the P-6.23 state
attestation carries a `BurnRateForecast` computed from the budget meters
(`runtime/state_attestation.rs`). The agent sees the trajectory of its spend
rather than only the current total, which is the difference between "I have 40%
left" and "I have 40% left and this rate exhausts it before step 3 completes".

The forecast is computed by the caller and passed in, keeping the attestation a
reporting surface rather than a calculator.

## Cross-provider failover, and the errors it must refuse

When a provider fails transiently, the driver retries within the provider and
then fails over to another preset. Eligibility is the interesting part — the rule
is not "retry on error" but "retry only where a different provider could
plausibly answer":

**Eligible:** HTTP 5xx, connection-level failures (refused, reset, timeout),
provider "overloaded" signals (529).

**Not eligible:** 400/401/403 (a bad request stays bad elsewhere), validation and
schema errors, and — specifically — **context overflow**, which is excluded even
when it arrives dressed as a 5xx. Overflow belongs to the context governor
(P-6.9); failing it over would send the same oversized prompt to a second
provider and pay twice for the same refusal.

A subtler exclusion sits next to it: a **response-body read failure** after a
successful HTTP status (dropped connection mid-body, truncated chunked transfer,
decode error) retries *within* the provider but is deliberately **not**
failover-eligible. The provider already consumed — and may have billed — the
request; failing over on a delivery blip risks paying twice for work that was
done. See `is_failover_eligible_error` and `next_body_read_retry_wait` in
`llm/mod.rs`.

Every completion, primary and every failover preset alike, passes through the
egress chokepoint (`llm/egress_chokepoint.rs`), so failover cannot become an
egress hole.

## Long tasks: compression instead of death

A task that runs long hits the context window. The capsule strategy in
`runtime/context_governor/capsule.rs` compresses history against the session's
current PlanFrame, using the active plan as a relevance lens: plan-advancing
decisions and identifiers are captured in the delta, abandoned detours are
dropped. The original context is archived on disk, so compression is not
destruction.

Details in [`prompt/context-compression.md`](prompt/context-compression.md).

## Related

- [`../reference/tool-errors.md`](../reference/tool-errors.md) — the envelope
  these kinds travel in, and the mechanical-classification fields
- [`divergence-detection.md`](divergence-detection.md) — the other half of "a
  session going wrong": trajectory pressure and the LoopGuard bands
- [`../archived/task-robustness.md`](../archived/task-robustness.md) — the
  design record, including open questions kept per their original framing
