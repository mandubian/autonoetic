# Divergence detection: how the gateway notices a session going wrong

Two different subsystems in this repo have "sentinel" in their name and they are
not related. This one watches a **session's trajectory** — is it looping, is it
failing tools, is it about to trip a guard. The other watches **artifacts and
sessions for security findings** and lives in
[`security-sentinel.md`](security-sentinel.md).

The signal substrate is `runtime/trajectory_health.rs`; the enforcement it feeds
is the `LoopGuard` in `runtime/lifecycle.rs`.

## Signals, bands, and what fires

A monitor computes per-signal pressure (`DivergenceSignalKind`) and assigns a
severity from how close that signal is to its hard trip:

| Band | Fraction of the trip threshold | Meaning |
|---|---|---|
| — | < 80% | nothing emitted |
| `Warn` | ≥ 80% | at least one signal crossed the warn threshold; not yet actionable |
| `Critical` | ≥ 95% | a trip is imminent; operator notification should fire |

The 80% warn fraction is deliberately the same threshold as
`LoopGuard::is_sub_trip_warning`, which is what P-7.18 degraded-mode entry keys
on — so the observability substrate and the enforcement agree about when a
session is in trouble instead of drifting apart.

Severity is **per signal, not per session**: one axis at 96% is critical even
while every other axis is quiet. Events are emitted on the causal chain under a
single divergence category, so `trace` surfaces can reconstruct why a session
was degraded or killed.

## The failure that shaped this: a legitimately failing test suite

The sharpest false-positive class came from `unit_test_runner`. A test suite
that *correctly* reports failure used to look identical to an agent thrashing:

1. `artifact_exec` runs the suite; tests exit non-zero.
2. The LoopGuard counted that as **both** no-progress **and** a tool failure.
3. With `max_tool_failures: 2` and `max_loops_without_progress: 2`, two such
   calls put the trajectory in the `Critical` band.
4. The session was killed **before** the runner could record its verdict with
   `promotion_record` — so a correct "these tests fail" outcome was lost.
5. Clarification children inherited the same tight budget and diverged the same
   way; the orphan reaper then cancelled them without notifying the parent,
   leaving the planner blocked in `workflow.wait`.

The whole chain converted a normal domain result into a lost session. Five
changes fixed it, and they are worth understanding as a set because each one
alone would have left the loop closed somewhere else.

### A non-zero exit is a domain result, not a tool failure

`artifact_exec` now separates *did the sandbox execute the command* from *did
the command succeed*. Tool success accepts the normal exit range:

```rust
ok = matches!(exit_code, Some(code) if (0..128).contains(&code))
```

with the actual command outcome carried alongside in `command_succeeded`. The
trajectory observation keys only on `ok`, so a failing suite no longer registers
as tool failure pressure.

The range matters. A signal kill has *no* exit code, and signal-derived codes
are ≥ 128 (SIGKILL/OOM 137, SIGTERM 143, SIGSYS/seccomp 159) — those stay
`ok: false`, so repeated OOM or timeout kills are still counted as failures
rather than laundered into "progress".

**Verdict safety is unaffected**, and that separation is the point:
`trace_indicates_pass` still gates on `exit_code == 0`, so a failing suite can
never produce a passing promotion verdict just because the tool call succeeded.
A regression test pins exactly that — a trace with a non-zero exit is a fail
even when the success flag is set.

Observability follows the command, not the sandbox: `infer_trace_success`
(`runtime/tool_call_processor.rs`) prefers `command_succeeded`, so a failing
suite records `success: 0, exit_code: 1` in the execution trace and in every
digest surface derived from it.

### Budgets matched to the workflow

`unit_test_runner.default`'s guard was raised to `max_loops_without_progress: 4`,
`max_tool_failures: 4`, `max_session_turns: 8` — enough turns to run a suite,
read the failures, and record a verdict.

### Clarification turns are exempt

A clarification child is a single read-only Q&A turn (`SessionState::Clarification`,
see `lifecycle.rs`). It is not a workflow and cannot make "progress" in the
sense the guard measures, so it is exempt from divergence escalation instead of
being killed for behaving exactly as designed.

### The reaper notifies, and `workflow.wait` detects drift

The orphan reaper notifies waiting parents when it cancels a child, and
`workflow.wait` detects a transcript/`TaskRun` status mismatch rather than
blocking forever on a child that is already gone. Together these close the
"planner stuck waiting on a session that no longer exists" path.

## The generalisable rule

Every one of those five fixes is the same shape: **a legitimate domain outcome
must not be indistinguishable from malfunction.** The guard's job is to catch an
agent that is not making progress — not to punish an agent that is correctly
reporting bad news. When adding a new divergence signal, the question to answer
first is what *correct* behaviour could produce it.

## Related

- [`security-sentinel.md`](security-sentinel.md) — the other sentinel: security
  findings, triage, supply chain, red team
- [`../proposals/divergence-sentinel.md`](../proposals/divergence-sentinel.md) —
  the design doc, still open on P4 validation
- [`../reference/config.md`](../reference/config.md) — LoopGuard and trajectory
  thresholds as config keys
