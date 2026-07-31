# Divergence Sentinel — Design

> Status: **Draft (2026-05-20)** — feedback wanted before implementation.
> Tracking issue: [#238](https://github.com/mandubian/autonoetic/issues/238).
> Phase issues: P0 [#239](https://github.com/mandubian/autonoetic/issues/239),
> P1 [#240](https://github.com/mandubian/autonoetic/issues/240),
> P2 [#241](https://github.com/mandubian/autonoetic/issues/241),
> P3 [#242](https://github.com/mandubian/autonoetic/issues/242),
> P4 [#243](https://github.com/mandubian/autonoetic/issues/243).

## 1. Problem Statement

Autonoetic enforces a lawful environment where agents are free to plan and act, as
long as they obey constitutional rules. Many guards already exist to catch concrete
violations (capability misuse, repeated failures, budget exhaustion, etc.).

However, operators have observed a recurring failure mode that **none of the
existing guards catch reliably**:

> The planner believes it is making progress, but a human reading the live digest
> can immediately see it is going in circles, repeatedly retrying a doomed strategy,
> drilling into the wrong sub-problem, or "succeeding" on the wrong axis.

This is a **meta-cognitive blind spot**, not a rule violation. The agent's
self-narrative inside its own context window says "I'm progressing." The aggregate
view across many turns says "you're diverging." Existing guards trip on _patterns_
(same tool+args N times) but not on _aggregate trajectory_.

Anecdotal observation by the operator: when a separate Claude session is shown
the live `digest.md` of a stuck planner and asked "what's going wrong here?", it
consistently surfaces problems the planner itself never flags. This may be
**confirmation bias** (the operator already suspects divergence when asking) or
a **real meta-cognitive asymmetry** (an outside reader without the planner's
in-context optimism). Section 6 proposes an experiment to disambiguate.

## 2. Goals & Non-Goals

**Goals**

- Detect mid-session divergence early enough for the planner to course-correct.
- Notify the planner via the existing agent-messaging mechanism, so the planner
  can choose to adjust its strategy.
- Notify the operator when divergence crosses a threshold the planner cannot
  self-correct.
- Be **optional and composable** — operators must be able to disable, tune, or
  trigger the deeper LLM-based check on demand.
- **Factorize** existing scattered divergence-adjacent logic into a clear
  semantic cluster, before adding new code.

**Non-Goals**

- Replace existing guards. Capability, budget, approval, and emergency-stop
  guards remain as they are — Sentinel is observational.
- Be a security boundary. Sentinel produces signals, not denials. Hard limits
  remain in `LoopGuard`, `PolicyEngine`, and budgets.
- Be a learning system. Sentinel does not persist judgments across sessions
  beyond the causal log — that is the role of memory/curator agents.

## 3. Current Guard Catalog (Factorization View)

A thorough survey of `autonoetic-gateway/src/` reveals **14 distinct guard
mechanisms** today. Grouping them by _semantic domain_ rather than file
location yields five clusters:

### Cluster A — Trajectory guards (the divergence-relevant cluster)

| Guard | File | Trips on |
|---|---|---|
| `LoopGuard::check_loop` (loops without progress) | `runtime/guard.rs:75-104` | N consecutive turns without a new tool fingerprint |
| `LoopGuard::register_failure` (tool failure budget) | `runtime/guard.rs:127-145` | Same tool fails ≥ N times (any args) |
| `LoopGuard::register_child_failure` (delegation budget) | `runtime/guard.rs:147-153` | ≥ N failed `workflow.wait` children |
| `LoopGuard::is_sub_trip_warning` (80% threshold) | `runtime/guard.rs:106-125` | Approaching trip — used for P-7.18 degraded mode |
| `budget_tracker::emit_context_pressure_high_if_warranted` | `runtime/budget_tracker.rs:34-71` | Context utilization crosses `warn_at_pct` |

This cluster is **the natural home for divergence detection**. Today it is
internally LoopGuard-shaped: mechanical, per-tool, and per-turn. It already
emits sub-trip warnings (P-7.18) — they just are not aggregated into a
"session health" signal.

### Cluster B — Resource budgets

| Guard | File |
|---|---|
| `SessionBudget::check_pre_llm` | `runtime/session_budget.rs` |
| `RootSessionBudget::check_pre_llm` | `runtime/root_session_budget.rs` |
| `enforce_cost_catalog_preflight` | `runtime/budget_tracker.rs:156-200` |

These two budget structs have **parallel shapes** (`max_llm_rounds`,
`max_llm_tokens`, `max_wall_clock_secs`, `max_session_price_usd`,
`max_tool_invocations`) and parallel hooks (`check_pre_llm` /
`record_llm_completion`). They could share a `BudgetEnvelope` trait
without semantic loss. _Factorization opportunity F-1._

### Cluster C — Policy / capability denials

| Guard | File |
|---|---|
| `PolicyEngine::can_exec_shell_detailed` | `policy.rs:1-510` |
| `SecurityAnalyzer` (destructive, privilege, escape, injection) | `policy.rs` |
| `network_policy::validate_request` | `runtime/network_policy.rs` |
| `promotion::record` severity gate | `runtime/tools/promotion.rs:177-223` |

These are **rejection-time** guards — they deny the operation outright. Out
of scope for Sentinel.

### Cluster D — Gate / approval / continuation

Already factorized into `GateService` (`runtime/human_gate.rs`) — see
`human-gate-unification-plan.md`. Out of scope.

### Cluster E — Emergency / lifecycle / retention

| Guard | File |
|---|---|
| `emergency_stop_root_session` | `execution.rs:841-1187` |
| `active_execution_registry` (PID/abort tracking) | `runtime/active_execution_registry.rs` |
| `reconcile_stale_active_executions` | `scheduler/gateway_store/migrate.rs:686-695` |
| `RetentionConfig` (execution_traces, causal_events) | various |
| `runtime_lock` drift check | `runtime_lock.rs` |
| `constitution` signature verification | `docs/constitution-signing.md` |

Out of scope, except that emergency stop is the **escalation target** when
Sentinel fires its most severe judgment.

### Factorization Opportunities Identified

| ID | Opportunity | Effort |
|---|---|---|
| **F-1** | Unify `SessionBudget` + `RootSessionBudget` behind a single `BudgetEnvelope` trait (today they share 80% of code) | S |
| **F-2** | Promote LoopGuard's `is_sub_trip_warning` into a richer `TrajectoryHealth` state machine that the new Sentinel consumes | M |
| **F-3** | Add a `divergence.*` causal event family with consistent shape (severity, signal_type, evidence_ref) instead of ad-hoc fields on existing events | S |
| **F-4** | Unify retention TTLs into a single `RetentionConfig` enum so future Sentinel records share the lifecycle | S |
| **F-5** | Consider moving `progress_budget_tools` from `LoopGuardConfig` into a future `TrajectoryConfig` block (cluster A's container) | S |

F-2 and F-3 are **prerequisites** for Sentinel itself and ship in Phase 1.
F-1, F-4, F-5 are independent housekeeping and may be deferred.

## 4. Proposed Architecture

The Sentinel is implemented in **two layers** that the operator can enable
independently:

### Layer 1 — Deterministic Trajectory Monitor (in-gateway, always-on)

Lives in `autonoetic-gateway/src/runtime/trajectory_monitor.rs` (new file).
Hooks the existing causal event stream and the LoopGuard sub-trip warnings.
It is rule-based, cheap, and has no LLM cost.

**Inputs**

- LoopGuard state snapshots (per-turn)
- `causal_events` writes (subscribes to `tool.*`, `turn.*`, `error.*`)
- Live digest stall metrics (lines/turn rate over a sliding window)
- Tool error type distribution (via `execution_traces`)

**Signals computed per session**

| Signal | Definition | Default threshold |
|---|---|---|
| `loop_pressure` | `loop_guard.current_loops / max_loops` | warn ≥ 0.8 |
| `failure_pressure` | max over tools of `failures / max_tool_failures` | warn ≥ 0.8 |
| `child_failure_pressure` | `child_failures / max_child_failures` | warn ≥ 0.66 |
| `digest_stall` | turns since last new causal event category | warn ≥ 5 |
| `repetition_entropy` | Shannon entropy of last-N tool fingerprints (low = repetitive). **Advisory-only**: caps at `Warn` severity — never escalates to `Critical` on its own (repeating a tool call is weak evidence of being stuck; the gate-worthy `Critical` verdicts belong to the loop guard's semantic no-progress and the error-burst signal). Evidence still records how low entropy is (`"critically low"` below `critical_bits`); planner is still notified. Requires `min_turns` (default 3) warm-up before evaluation to avoid tripping on an agent's opening burst of similar calls. | warn ≤ 1.2, advisory (max Warn) |
| `error_burst` | error events in last-N turns | warn ≥ 5 |
| `context_pressure` | already exists in `budget_tracker.rs` | warn ≥ 0.8 |

A simple **weighted aggregate** produces a `TrajectoryHealth` enum:

```rust
enum TrajectoryHealth {
    Healthy,
    Watching { signals: Vec<DivergenceSignal> },   // ≥1 warn signal
    Diverging { signals: Vec<DivergenceSignal> },  // ≥2 warn signals or ≥1 critical
    Critical { signals: Vec<DivergenceSignal> },   // approaching trip (95%+)
}
```

**Actions taken (configurable per level)**

| Level | Action |
|---|---|
| `Healthy` | nothing |
| `Watching` | emit `divergence.observed` causal event (no further action) |
| `Diverging` | emit `divergence.detected` event + `agent.message` to root planner with the signals as evidence + (optional) trigger LLM watchdog (Layer 2) |
| `Critical` | all of `Diverging` actions + operator notification via existing user_interactions channel |

**Why this layer first**

- The symptoms operators name (loops, repeated errors, weird repetition) are
  mechanically detectable from existing data.
- Zero LLM cost. Zero new failure modes (no hallucinations).
- It's **falsifiable**: thresholds either fire on real divergence or they don't.
- It builds the substrate (the `divergence.*` event family, the
  `TrajectoryHealth` snapshot) that Layer 2 needs.

### Layer 2 — LLM Watchdog Agent (optional, triggerable)

A new specialist agent under `agents/specialists/watchdog.default/` (or similar). It is
explicitly **not** wired into every session. It runs only when:

1. The operator triggers it manually via a CLI command (`autonoetic watchdog
   <session_id>`).
2. Layer 1 reaches `Diverging` or `Critical` **and** the gateway config has
   `watchdog.auto_invoke_on_diverging = true` (default: false).
3. A scheduled health-check fires for long-running sessions
   (`watchdog.scheduled_review_every_n_turns`, default: disabled).

**Capabilities**

- `digest_query` — read the live digest of the target session
- `execution_search` — query causal events
- `agent.message` — send a message to the root planner
- `user_interaction.escalate` — escalate to the operator

It has **no execution capabilities**. It cannot kill processes, edit code, or
modify session state. Pure observer.

**Prompt skeleton**

> You are reviewing a live agent session for signs of divergence: loops,
> repeated failed strategies, drilling into the wrong sub-problem, declared
> "success" on the wrong axis, or any other pattern a fresh reader would
> immediately notice but the in-context planner has missed. You have the live
> digest, causal events, and trajectory health signals already computed by the
> deterministic monitor. **Default to silence** — only speak if you find a
> concrete issue with concrete evidence. If you do, send a brief, specific
> message to the planner. If the divergence is severe and the planner has
> already been warned, escalate to the operator.

**Why this is its own agent and not a function call**

- It needs LLM judgment, not deterministic rules, by hypothesis.
- It must be **observable** in the causal chain (its calls and decisions are
  audit events).
- It runs in its own session/budget — its own divergence cannot infect the
  target session's budget.
- It can be evolved/improved as a Skill like any other agent.

**Why it must be optional**

- LLM cost per invocation is non-trivial (reads the whole digest).
- Operator's hypothesis (digest readers catch things planners miss) is not
  yet validated — see §6.
- Adding an always-on LLM observer to every session would meaningfully
  change the cost profile of the runtime.

## 5. Rollout Phases

| Phase | Scope | Deliverable |
|---|---|---|
| **P0** | Factorization F-2, F-3 | `TrajectoryHealth` enum, `divergence.*` causal event family, no behavior change yet |
| **P1** | Layer 1 deterministic monitor | `trajectory_monitor.rs`, signal computation, `Watching`/`Diverging`/`Critical` levels emit events |
| **P2** | Layer 1 messaging | Planner receives `agent.message` on `Diverging`; operator notification on `Critical` |
| **P3** | Layer 2 watchdog agent | New agent bundle, manual CLI trigger only |
| **P4** | Layer 2 auto-invoke | Optional config flag to invoke watchdog from Layer 1 escalations |
| **P5** | Tuning | Threshold calibration from real session corpus; F-1, F-4, F-5 housekeeping |

Each phase is independently shippable and provides value on its own.

## 5b. Principle-Aware Correlation (Contract Health) — implemented foundation

The Sentinel correlates breaches by **constitutional clause**, not by ad-hoc
rule strings. This foundation is implemented (#302) ahead of the Layer-1
monitor and is reusable by both layers:

- **Events carry clause attribution.** Enforcement events keep their legacy
  rule/right IDs in `enforced_rules`; richer events (e.g.
  `loop_guard.tripped`) also resolve the owning clause into their payload
  (`rule_id` + `clause`).
- **Reverse lookup.** `enforcement_register::clause_of_rule(rule_id)` maps
  a numbered `P-x.y` / `Ri-x.y` rule ID to its owning principle or right. The register
  is the single source of truth for code ↔ constitution agreement, so the
  Sentinel never hard-codes rule→clause mappings.
- **Contract-health tally.** `GatewayStore::contract_health(since)` aggregates
  occurrences per clause over `causal_events`, surfacing IDs not yet migrated
  into the register as `unattributed` (a visible migration-coverage signal
  rather than a silent drop). The I-6 event-attribution invariant uses the
  `R+++3` placeholder string in code when no specific rule applies.
  excluded.
- **Operator surface.** `autonoetic trace contract-health [--since] [--json]`.

This gives the "report" half of report-and-correct a standing, queryable view:
which principles trip, how often, and whether enforcement is concentrating on
particular clauses. Layer 1 consumes the same correlation when emitting
`divergence.*` events; Layer 2 can cite per-clause history in its judgment.

## 6. Validating the "Digest-Reader Bias" Hypothesis

Before building Layer 2, run a **falsifiable experiment** on archived
sessions to test whether outside readers really do catch divergence the
planner misses, or whether the operator is biased by knowing the answer in
advance.

**Method**

1. Collect 20 archived sessions from `~/.autonoetic/sessions/`: 10 known
   to have diverged (operator-flagged), 10 known to have succeeded.
2. Strip outcome from each — present only the digest and causal events.
3. Run the watchdog prompt blind on all 20. Record its judgment per session.
4. Compute: true-positive rate, false-positive rate, precision, recall.
5. Compare against Layer 1 deterministic signals alone on the same corpus.

**Success criteria for proceeding to P3**

- Watchdog true-positive rate must be **strictly higher** than Layer 1 alone
  on the same corpus, by enough to justify the per-invocation cost.
- Watchdog false-positive rate on healthy sessions must be **≤ 10%** —
  noisy false alarms erode planner trust.

If the experiment fails, P3+ is dropped and Layer 1 is enough.

## 7. Open Questions

1. **Should the planner be able to disable Sentinel messages for a turn?**
   (E.g., "I know I look stuck but I'm working through a known-slow phase.")
   Argues for a `sentinel.suppress(turns=N)` tool with audit logging.
2. **What is the right TTL for `divergence.*` causal events?**
   Probably same as other causal events (90d), but worth confirming for
   privacy.
3. **Does Layer 2 need to read execution_traces (stdout/stderr) or only the
   digest?** Reading traces is much more expensive but might be necessary
   for some divergence types.
4. **Should F-1 (BudgetEnvelope unification) block P0?** Probably not —
   keep them independent.
5. **Constitutional alignment**: which rule numbers govern Sentinel's
   behavior? It is observational, so likely a new R-7.x family rather than
   extending R-3.x policy rules.

## 8. References

- `docs/archived/human-gate-unification-plan.md` — the precedent factorization
  effort this design follows
- `autonoetic-gateway/src/runtime/guard.rs` — LoopGuard, the closest existing
  cousin
- `autonoetic-gateway/src/runtime/budget_tracker.rs` — context pressure
  warning, a partial existing example of trajectory monitoring
- `autonoetic-gateway/src/runtime/post_session_digest.rs` — post-session
  analysis (Sentinel is its in-session counterpart)
- `docs/agent-learning.md` — `digest_query`, `execution_search` APIs the
  watchdog will use
- `docs/separation-of-powers.md` — why the watchdog has no execution
  capabilities
