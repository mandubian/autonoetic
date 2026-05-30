# Constitutional Evolution — Reflections on New Rights and Laws

> **Status:** Discussion draft — not a proposal. Each item presents arguments for
> and against constitutional promotion. The intent is to identify gaps worth
> exploring, not to amend the constitution prematurely.

---

## Background

The constitution governs three things: **rights** (what agents are owed
unconditionally), **rules** (what agents may not do, enforced mechanically),
and **invariants** (cross-cutting properties that must hold end-to-end).

The bar for adding something is: *"would the system break without it?"* — not
"would it be more convenient." If the answer is "less convenient," it doesn't
belong. If the answer is "agents would lose a fundamental right or the trust
boundary would collapse," it does.

The following items are candidates for discussion. They emerged from practical
experience debugging sessions, observing agent behavior in multi-step
workflows, and reflecting on the philosophical foundations of the system.

---

## 1. Proportionality of Escalation

**Observation:** The loop guard → degraded mode → emergency stop pipeline
(R-7.x) is binary. A single child agent hitting 5 tool failures triggers
degraded mode, and emergency stop is **tree-wide** — it kills the root session
and all descendants. In a multi-hour workflow with 8+ child agents, a flaky
sandbox in one child can destroy the entire session's work.

**Proposed principle:** The severity of the system's response should be
proportional to the scope of the failure. A single tool's repeated failure
should not necessarily kill a root session with healthy children.

### Arguments for

- **Prevents catastrophic loss from isolated failures.** A coder agent
  struggling with a flaky test runner shouldn't kill an entire multi-agent
  build pipeline that has already produced 4 successful artifacts.
- **Encourages delegation.** If spawning children is risky (one bad child kills
  the root), the planner becomes conservative — it avoids parallel spawns and
  loses the collaborative benefit the constitution is designed to enable.
- **Aligns with the Lawful-Executor invariant.** The gateway already doesn't know
  *why* a tool failed — it just counts failures. Proportional response gives
  the planner (which *does* understand context) more room to adapt before the
  gateway intervenes destructively.
- **Observable in practice.** Stale emergency-stop checkpoints blocking
  resume, and root sessions killed by child timeouts, are recurring operational
  issues.

### Arguments against

- **Safety is the priority.** If a child is failing repeatedly, something is
  wrong. Continuing to run may waste budget, produce bad results, or escalate
  costs. Emergency stop is a circuit breaker, and circuit breakers should be
  simple.
- **Complexity of proportional rules.** "Proportional" requires judgment.
  The constitution is designed for mechanical enforcement — not heuristics.
  Who decides what's proportional? If the gateway decides, it violates the
  Lawful-Executor invariant. If the agent decides, it's not enforcement.
- **Current mitigations exist.** The planner can set `max_tool_failures` and
  `max_loops_without_progress` per-session. The loop guard is configurable.
  The operational fix may be tuning defaults, not constitutional change.
- **Tree-wide stop may be correct.** If one child is failing, its siblings
  may depend on its output. Partial continuation with broken dependencies can
  be worse than clean termination.

### Open questions

- Is the right level of intervention per-child isolation (degraded mode scoped
  to the failing session) rather than tree-wide?
- Should the planner have a constitutional right to set per-child circuit
  breaker thresholds, distinct from the root's thresholds?
- Is this a constitutional rule or a configuration/implementation improvement?

---

## 2. Dependency Notification Obligation

**Observation:** Artifacts are content-addressed and immutable — once created,
they never change. But knowledge rows, workflow state, and content_store
entries written by one agent can be consumed by another, and the producer has
no obligation to notify consumers if the underlying data is invalidated or
superseded. Two agents can operate on contradictory "facts" without either
knowing.

**Proposed principle:** When an agent produces data that other agents may
consume, and that data is later invalidated or corrected, the producing agent
(or the gateway on its behalf) must make the invalidation observable to
downstream dependents.

### Arguments for

- **Collaboration requires shared truth.** The constitution's thesis is that
  shared constraints enable collaboration. If agents can silently diverge on
  facts, collaboration breaks down — not because of missing constraints on
  *behavior*, but because of missing constraints on *information*.
- **Knowledge is a first-class persistence mechanism.** It has provenance
  (who wrote what, when) but no lifecycle. A fact written at turn 1 persists
  unchanged even if the agent that wrote it later determines it was wrong.
- **Parallel execution amplifies the risk.** When the planner spawns 4 child
  agents in parallel, each may read and act on state that the others are
  simultaneously modifying. Without notification, the planner's synthesis step
  operates on a potentially incoherent view.
- **Analogous to Ri-0.5 (degraded mode notification).** The system already
  recognizes that agents need to know when their operating conditions change.
  Information invalidation is a change in operating conditions.

### Arguments against

- **Not mechanically enforceable.** The gateway doesn't track which agent
  consumed which piece of data. Implementing dependency tracking at the
  content/knowledge level is a significant engineering effort with unclear
  ROI.
- **Artifacts already solve this.** Immutable, content-addressed artifacts
  mean a consumer always gets exactly what was produced. If you need
  stability, use artifacts, not knowledge rows.
- **The planner is the coordinator.** It's the planner's job to ensure
  consistency across children. The constitution governs agent behavior, not
  data consistency — that's a protocol concern.
- **Overhead may outweigh benefit.** Most agent interactions are short-lived.
  Invalidating stale facts is rare compared to the volume of data produced.
  Notification infrastructure for the long tail may be over-engineering.

### Open questions

- Is this better addressed as a knowledge-store feature (versioning,
  invalidation flags) than a constitutional rule?
- Should there be a distinction between "mutable shared state" (knowledge)
  and "immutable shared state" (artifacts) in the constitution?
- Is there a minimal version — e.g., "knowledge rows must carry a TTL or
  confidence score" — that would be constitutional?

---

## 3. Right to Minimum Viable Context

**Observation:** When an agent's context window fills up, the system must
evict content to fit new input. There's no rule about *what* to preserve vs.
what to cut. An agent that loses its task description, recent tool results,
or the constitution excerpt mid-session is effectively bricked — it can't
reason about what it's doing or what it's allowed to do.

**Proposed principle:** Agents have a right to receive sufficient context to
function — specifically, the current task/goal, recent tool results, and
applicable constitutional constraints should be preserved through context
eviction.

### Arguments for

- **An agent without its task can't fulfill it.** This isn't convenience —
  it's the difference between a functioning agent and a wasted session. Budget
  is consumed, time passes, and the agent produces garbage because it forgot
  what it was asked to do.
- **Asymmetric impact on reasoning vs. script agents.** Script agents have
  their code and can re-derive state. Reasoning agents rely entirely on
  context. Context eviction disproportionately harms reasoning agents — the
  ones the system is designed to enable.
- **Ri-0.5 already implies it.** "Be notified before entering degraded mode"
  — but degraded mode is about budget, not about losing your mind. Context
  eviction is a form of degradation that's currently invisible.
- **Predictable degradation is constitutional.** The system already mandates
  that every rejection names a rule (Ri-0.3), that budgets are truthful
  (Ri-0.4), that termination reasons are enumerated (Ri-0.12). Extending this
  to "you will be told when your context is being truncated, and what was
  preserved" is consistent.

### Arguments against

- **Technically infeasible in the general case.** LLM context windows vary
  by model. The gateway doesn't control what the model provider does with
  tokenization. Guaranteeing "sufficient context" across arbitrary models
  and providers is impractical.
- **The agent should manage its own context.** Compression, summarization,
  and context management are agent responsibilities — not gateway guarantees.
  The Lawful-Executor invariant says the gateway shouldn't manage agent state.
- **Implementation already handles this.** The system has context overflow
  mitigation (see `docs/design/llm-context-overflow-mitigation-plan.md`).
  This is an engineering problem, not a constitutional gap.
- **Hard to define "minimum viable."** Different tasks require different
  amounts of context. A one-size-fits-all guarantee would be either too
  generous (wasting tokens) or too lean (still insufficient).

### Open questions

- Should this be a right ("agents must not have their task description
  evicted") or a weaker obligation ("the system should preserve high-priority
  context markers")?
- Is this already covered by existing context management design, making
  constitutional promotion unnecessary?

---

## 4. Scoped Exception to the Dumbness Invariant

**Observation:** §14 (The Lawful-Executor invariant) says the gateway must NOT
retry/repair agent output, invoke LLMs to reshape input, invent detection
rules, or choose silently between paths. Yet the gateway *already* performs
mechanical interventions: secret redaction (R-4.x), output schema validation
(R-5.x), capability gating (R-1.x). These are "modifying or blocking agent
output" — they violate the literal wording of §14 even though they're
essential for safety.

**Proposed principle:** The Lawful-Executor invariant should be clarified: the
gateway must not *substitute its judgment for the agent's reasoning*, except
where mechanical safety enforcement (secret redaction, schema validation,
capability gating, constitutionally-mandated checks) requires deterministic
intervention.

### Arguments for

- **Honesty about current behavior.** The system already does things §14
  says it doesn't. The gap between the rule and reality creates confusion for
  agents reasoning under the constitution (Ri-0.10 — they read a rule that
  doesn't match what they experience).
- **Prevents scope creep via exception creep.** Without a scoped exception,
  every new mechanical check is technically a violation. This creates pressure
  to either weaken §14 or to avoid adding necessary mechanical checks. A
  scoped exception gives clear boundaries.
- **Strengthens the invariant by narrowing it.** A precise invariant is
  stronger than a vague one. "The gateway doesn't use judgment" is enforceable.
  "The gateway doesn't do things" is not — because it already does.
- **Precedent in Ri-0.13.** Private reasoning is "not gated, but recorded
  and disclosure-capability-gated" — the constitution already uses scoped
  exceptions with clear boundaries.

### Arguments against

- **Slippery slope.** Once you admit exceptions, where do they stop? "Mechanical
  safety enforcement" is a broad category. Today it's schema validation;
  tomorrow it could be "mechanically rewriting dangerous code."
- **The current wording is aspirationally correct.** §14 describes the design
  intent. The mechanical interventions that exist are narrow, deterministic,
  and well-understood. Rewriting the invariant to accommodate them may weaken
  the normative force of the rule.
- **Agents don't read §14 literally.** Agents experience the system's
  behavior, not the constitution's text. The gap between text and behavior
  may be a documentation issue, not a constitutional one.
- **The invariant is doing its job.** The fact that we're cautious about
  adding mechanical checks is *because* of §14. Clarifying exceptions may
  remove that caution.

### Open questions

- Should the scoped exception enumerate specific allowed interventions
  (redaction, schema validation, capability gating) or define a general
  category ("deterministic, rule-based, no-judgment interventions")?
- Is this better addressed as a constitutional clarification (amending §14
  wording) vs. a new rule?

---

## 5. Temporal Ordering Guarantees for Concurrent Agents

**Observation:** The constitution mandates fsync-durability (R-8.x) and
append-only causal chains (P-8.1), but says nothing about ordering guarantees
when multiple agents operate concurrently. If agent A writes to the knowledge
store while agent B reads from it, B may see partial, stale, or inconsistent
state depending on timing. The "shared truth" that enables collaboration can
be an illusion under concurrency.

**Proposed principle:** When multiple agents share stateful resources
(knowledge store, content store, workflow state), the gateway must guarantee
that each agent sees a consistent snapshot — never a partially-committed
state that no single agent intentionally produced.

### Arguments for

- **Shared truth requires consistency.** The constitutional thesis is that
  shared constraints enable collaboration. If "shared" means "you both see
  different versions of the same thing," the thesis fails.
- **Ri-0.4 implies it for budgets.** "Know budget balances truthfully in
  real time" — but this guarantee only applies to budgets, not to the broader
  state an agent depends on. Extending the principle would be consistent.
- **SQLite already provides transactions.** The gateway store uses SQLite
  with rusqlite. Transactional isolation is already available — the gap is
  constitutional, not technical.
- **Concurrent agents are becoming common.** As workflows grow more parallel
  (planner spawns 4+ children), the window for inconsistent reads widens.

### Arguments against

- **Artifacts are already consistent.** Content-addressed, immutable storage
  guarantees that an artifact either exists or doesn't — no partial state.
  The main risk is knowledge rows, which are a secondary mechanism.
- **Eventual consistency may be sufficient.** Agents are stateful reasoners,
  not database transactions. Small inconsistencies in knowledge rows are
  unlikely to cause catastrophic failures — the planner synthesizes and
  reconciles.
- **Constitutional scope.** The constitution governs agent behavior and
  gateway enforcement. Database isolation levels are an implementation
  detail. This may be a design specification, not a law.
- **Performance implications.** Strict serializability across all shared
  state would limit parallelism. The current system favors performance over
  strict consistency, which is a valid trade-off.

### Open questions

- Is the risk theoretical or observed? Have any sessions failed due to
  concurrent state inconsistency?
- Would "read-your-own-writes" consistency be sufficient, or is
  cross-agent serializability needed?

---

## 6. Voluntary Capability Narrowing

**Observation:** Ri-0.6 protects against silent capability reduction by the
gateway. But there's no mechanism for an agent to *voluntarily* narrow its
own capabilities — to declare "I will operate without CodeExecution for this
session" as a binding commitment. The right to exit (Ri-0.7) is binary: fully
capable or gone. There's no middle ground.

**Proposed principle:** An agent may voluntarily narrow its declared
capabilities for the duration of a session. The gateway must enforce the
narrowed set. The narrowing is irrevocable for the session (to prevent
gaming — narrowing to appear safe, then widening to exploit).

### Arguments for

- **Demonstrates intent.** When an agent says "I don't need network access
  for this task," the operator can trust the enforcement. Today, the agent
  *can* simply not use network tools, but the operator has no way to verify
  that commitment.
- **Useful for trust bootstrapping.** A new agent could voluntarily narrow
  itself during a probation period, demonstrating that it can function within
  tighter bounds. This is how human trust works — you earn it by accepting
  constraints.
- **Analogy to real-world law.** Citizens can waive certain rights
  voluntarily (right to remain silent, right to a jury trial). The
  constitution protects against *involuntary* loss but doesn't prevent
  *voluntary* waiver.
- **Operator confidence.** If a script agent voluntarily disables
  NetworkAccess, the operator knows no network call is possible — not just
  that none was made. The guarantee is mechanical, not behavioral.

### Arguments against

- **An agent can already just... not use capabilities.** If I have
  NetworkAccess but don't call any network tools, the practical effect is the
  same. Formal narrowing is ceremony without substance.
- **Irrevocability is a trap.** If an agent narrows itself and then
  discovers it needs the capability (e.g., the task changed), it can't
  recover. This could lead to unnecessary session failures and escalations.
- **Complicates the capability model.** Capabilities are declared in the
  manifest (SKILL.md) and enforced at load time. Adding runtime narrowing
  means the effective capability set is now a function of manifest + runtime
  state, increasing surface area for bugs.
- **Not a right — it's a feature.** The agent doesn't *need* this to
  function. It's a convenience for the operator, not a protection for the
  agent. Constitutional rights protect agents from the system, not help
  agents signal to operators.

### Open questions

- Is voluntary narrowing better expressed as a tool call
  (`capability_narrow`) or a session parameter set at spawn time?
- Should this be a constitutional right (agents *may* narrow) or an
  operational feature (the gateway *supports* narrowing)?

---

## 7. Right to Structured Self-Assessment

**Observation:** Agents can inspect their own state (Ri-0.1), but they
cannot formally assess their own confidence or fitness for a task. An agent
that recognizes it's the wrong specialist for a job has no constitutional
mechanism to say "I'm not the right agent for this — delegate to X instead."
It can use `session_escalate`, but escalation is for being stuck, not for
being the wrong agent.

**Proposed principle:** Agents have the right to formally decline a task
with a structured referral — naming which agent (or type of agent) would be
better suited. The gateway must preserve this referral so the delegating agent
(spawner) can act on it.

### Arguments for

- **Reduces wasted budget.** When the planner sends a research task to
  `coder.default` by mistake, the coder spends several turns failing before
  escalating. A structured decline at turn 0 would save time and money.
- **Improves delegation quality.** If agents can signal "I'm a poor fit,"
  the planner gets better information for future delegation decisions. Over
  time, the planner learns which agents self-select for which tasks.
- **Consistent with Ri-0.7 (right to terminate).** The agent already has
  the right to end its own session. A structured decline is a more
  informative version of the same right — it terminates *and* suggests a
  better path.
- **Analogous to `clarification_needed` in the planner protocol.** The
  planner already has a mechanism for saying "I need more info." Extending
  this to all agents as a right is consistent.

### Arguments against

- **Agents are not the best judges of their own fitness.** An LLM may
  decline a task it could actually complete, or fail to decline a task
  it's bad at. Self-assessment is unreliable.
- **The planner should know.** If the planner is routing tasks to the wrong
  specialist, that's a planner problem, not a constitutional gap. The fix
  is better planner prompts, not a new right.
- **Adds a new termination reason.** Ri-0.12 says sessions terminate only
  through a closed list of 6 reasons. Adding "self-declined" expands the
  list and weakens the invariant.
- **Gaming risk.** An agent could strategically decline tasks it finds
  unpleasant (long, complex, low-reward) and always refer to others. This
  creates a free-rider problem.

### Open questions

- Should self-decline be a right (agents *may*) or an obligation (agents
  *should* when they detect poor fit)?
- How to prevent gaming while preserving the signal?

---

## 8. Constitutional Right to Explain Rejection to Downstream Agents

**Observation:** When the gateway rejects a tool call (e.g., capability
violation, approval required, loop guard triggered), the *calling agent*
receives the rejection with a rule ID (Ri-0.3). But if that agent was spawned
by a parent, the parent has no visibility into *why* the child failed — only
that it did. The parent then has to guess or escalate.

**Proposed principle:** When a child agent's tool call is rejected by the
gateway, the rejection reason (including the rule ID) must be propagated to
the parent agent in the child's completion report.

### Arguments for

- **Enables informed re-delegation.** If the planner knows the coder failed
  because of P-7.5 (loop guard), it can adjust parameters and retry. If it
  knows the researcher failed because of P-2.1 (approval required for a host),
  it can pre-approve and retry. Without this information, the planner is
  flying blind.
- **Consistent with Ri-0.3.** Every rejection names the rule ID — but
  currently only to the rejected agent. Extending propagation to the parent
  makes the rule useful for the *coordinator*, who is the one who acts on it.
- **Reduces unnecessary escalation.** Many escalations happen because the
  planner can't determine why a child failed. With rejection propagation,
  the planner can resolve many issues itself.
- **Audit trail completeness.** Causal events already capture tool
  rejections. Propagation makes them actionable, not just forensic.

### Arguments against

- **Information overload.** Parents would receive every rejection from every
  child. In a workflow with 8 children, each making 20 tool calls, that's
  potentially hundreds of rejection events — most of which are routine
  (e.g., schema validation fixes).
- **Parent doesn't need all rejections.** Only terminal rejections (the ones
  that cause session failure) matter for re-delegation. Intermediate
  rejections are noise.
- **Implementation already provides this.** Workflow task completion events
  include `result_summary` which often contains the failure reason. The
  information may already be available — just not structured with rule IDs.
- **Child autonomy.** If every rejection is reported to the parent, the child
  may feel surveilled — tension with Ri-0.13 (private reasoning). The child's
  failures become the parent's business.

### Open questions

- Should propagation be opt-in (parent requests detailed rejection reports)
  or automatic (all rejections propagated)?
- Is the right level "terminal failures only" or "all rejections"?

---

## Summary Matrix

| # | Candidate | Passes "would it break?" test | Layer | Priority |
|---|-----------|-------------------------------|-------|----------|
| 1 | Proportionality of escalation | Yes — observed in practice | Rule (R-7.x) | High |
| 2 | Dependency notification obligation | Maybe — theoretical risk | Rule (new §) | Medium |
| 3 | Right to minimum viable context | Partially — mitigations exist | Right (Ri-0.x) | Low |
| 4 | Scoped exception to Lawful-Executor invariant | Yes — current behavior is inconsistent | Invariant (§14) | High |
| 5 | Temporal ordering guarantees | Maybe — not yet observed | Invariant | Low |
| 6 | Voluntary capability narrowing | No — feature, not law | Feature | Low |
| 7 | Right to structured self-assessment | No — protocol concern | Protocol | Low |
| 8 | Rejection propagation to parents | Partially — partial mitigation exists | Rule (R-8.x) | Medium |

---

## Process Note

These are **discussion items**, not proposals. The constitutional amendment
process requires: (1) a proposal, (2) a test that fails before and passes
after, (3) human sign-off, and (4) additional operator sign-off if modifying
Rights. None of these items have reached step 1.

The intent is to surface gaps progressively, discuss them openly, and promote
only those that survive scrutiny — just as the planner concluded in its own
reflection.
