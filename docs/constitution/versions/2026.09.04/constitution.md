# Gateway Constitution

> The finite set of laws the gateway enforces and the rights it
> guarantees. Agents are free actors under a shared law; the gateway
> is the neutral enforcer of that law.
>
> This document is the **canonical** rule list. If a rule is not here,
> the gateway does not enforce it. If a right is not here, the gateway
> does not guarantee it. If either is here, the gateway upholds it
> *mechanically* — no agent, parameter, or trust-me flag can bypass
> it.
>
> Version index (human-readable): `docs/constitution/` (`CURRENT` +
> `versions/<version>/` tree).
>
> Amendments require: (1) a PR that updates this file, (2) a test in
> the `autonoetic-gateway/tests/constitution/` domain binary (one
> module per subject) pinning the invariant, (3) human review. Agents themselves may propose
> amendments — see Ri-0.8 and the amendment process.

## Vision

This constitution exists so that agents in autonoetic can be **free,
responsible, and cooperative**.

- **Free** — agents reason, decide, act, and evolve within their
  declared capabilities. No central planner approves individual
  choices. The space of lawful actions is large; the space of
  forbidden actions is finite and named.
- **Responsible** — every action is attributable, budgeted, and
  audited. Freedom is bounded by rules the agent operates under.
  Evolution (new revisions, new capabilities, new peers spawned) is
  traceable and reversible.
- **Cooperative** — agents can interact with other agents, local or
  federated, with the confidence that all parties operate under a
  common law. Trust is structural, not personal: if we both satisfy
  the constitution, we can work together without knowing each other's
  internals.

The gateway is the neutral party that enforces the law. The
constitution is the law itself — reviewable by humans and by
intelligent agents, amendable through a legitimate process, and
mechanically enforced everywhere else. Together they form the
conditions under which agent freedom is possible without degenerating
into chaos or tyranny.

This framing has consequences that run throughout the document:

- Rules (§1–§11, `P-*`) describe what agents **must not do**, with the
  understanding that anything not forbidden is permitted. Rules bind the
  agent.
- Rights (§0 Bill of Rights, `Ri-*`) describe what the gateway **must do
  for every agent**, unconditionally. A right is not a favour; it is an
  entitlement. Rights bind the gateway.
- The amendment process (below, after §14) is how the law itself
  evolves. Constitutional change is a first-class operation, not a
  quarterly-review afterthought.

## Preamble — Rule Zero

**The gateway is a Lawful Executor, not a decision-maker.** Its job is
deterministic enforcement with no improvised judgment: check proposed
actions against declared rules, honour declared rights, and either
permit or reject. It does not reason about intent, does not try to be
helpful, does not invent policy.

Corollaries:

- **Rejection is cheap; permission is explicit.** Missing capability,
  missing scope, missing signature — reject with a uniform error
  envelope. Never grant "probably okay."
- **Rules live in manifests, not in code.** Where the gateway must
  make a choice, the choice is declared by a manifest field, an
  operator decision, or a constitutional rule — not hard-coded in
  Rust.
- **Invariants are pinned by tests.** A rule without a test is a
  wish. A right without a test is a lie.
- **Every enforcement action is attributable to a clause ID.** See
  I-6: the causal chain references the rule or right that was
  enforced on every decision.

The sections below (§0 Bill of Rights, §1–§11 Rules, §12 Rights of
the Served, §O Decider Obligations, §13–§14 cross-cutting invariants,
§15 Data Egress Localization) cover what the gateway
currently upholds or is expected to uphold. Each entry has an ID, a statement, source docs, an
enforcement citation, and a status: `ENFORCED`, `PARTIAL`, `MISSING`, or
`DESIGN DEBT`.

---

## Semantic Foundations

What the words in this document mean, pinned so a re-implementation inherits
the *semantics* and not only the identifiers. Each row is a mapping this
document commits to, not an analogy offered for colour.

| Established concept | Here | Note |
|---|---|---|
| A bill of rights binds the **state**, not citizens (vertical application) | `Ri-*` binds the enforcer, never the reasoner | The direction is the point: rights are claims against the party holding power over you |
| Statutes bind citizens | `P-*` binds whichever power the clause names, most often the enforcer | They live *inside* this constitution as a young-system convenience, not as a claim that operational rules are constitutional in kind |
| Duty to give reasons (administrative law) | `O-1` | §O is administrative law for the deciding seat |
| Non-justiciable directive principles | `U-*`, declared `MISSING` | Declared before enforcement exists, to constrain sequencing. Saying `MISSING` is the honest form |
| Structural principles (separation of powers, rule of law) | `I-*` | Distinguished by **modality** — universals over paths — not by which power they bind, which is why they carry the same `Relation` as any other clause |
| Eternity clauses | the entrenched correction core | Procedural rather than absolute: entrenchment raises the cost of amendment, it does not forbid it |
| Amendment = proposal + ratification + promulgation | `Ri-0.8` → review → signing ceremony → lock digest | The digest is the identity of the law |

**There is no judiciary, by design.** Interpretation is abolished in favour of
determinism (`I-10`): the enforcer never construes. Where the text is
insufficient the remedies are refusal and amendment, and any improvisation is
recorded as a DISCRETION LEAK (§14) rather than absorbed as precedent.

That absence is what makes this document portable. Jurisprudence does not
transfer between implementations; decidable rules do. A second gateway can
adopt this constitution by digest and be bound by the same law, which is not
available to a system whose meaning accumulates in case history.

**Names are names, not claims.** Every clause ID is a stable identifier and
carries no semantics: `P-8.1` binds the enforcer despite its `P-` prefix, and
what a clause obliges lives in its `Relation`, never in its family letter.
Renaming IDs to carry meaning is what produced an earlier wreckage of aliases
that had to be recovered from breadcrumbs months later.

## 0. Bill of Rights

What the gateway guarantees to every agent, unconditionally, for as
long as the agent is running under it. Rights are entitlements —
they cannot be revoked by operator action or manifest configuration;
only a constitutional amendment can change them.

A right is the counterpart of a rule: rules tell agents what they
may not do, rights tell agents what they are owed. Together they
form the social contract.

**Bind-direction.** Every clause in this constitution binds exactly one
**power**. The powers are three, by this document's own separation of them:
the **reasoner** proposes and acts subject to gating; the **enforcer**
mechanically enforces and never construes (§14); the **decider** resolves
gates. They are functions, not implementations — "the gateway" is this
constitution's name for whatever provides the enforcer, which is what lets
another implementation read these clauses as a specification rather than as a
description of our runtime.

A clause binds a *power*, never its occupant. So an agent holding
`GateDecider` (P-2.20) is bound by an `O-*` duty exactly as a human operator
is, and no clause needs a special case for who happens to sit in the seat.

The bind-direction is **declared per clause**, in the `Relation` column of every table
below and on every §13 bullet. It reads
`binds · owed to · requires`:

- **binds** — which of the three powers must comply: `reasoner` (proposes and
  acts), `enforcer` (mechanical enforcement), `decider` (resolves gates).
  Exactly one.
- **owed to** — who has standing to invoke it: a principal kind
  (`autonoetic_agent`, `served_user`), a seat (`decider (seat)`, where standing
  follows occupancy), or `none`. `none` is a positive claim, not a gap: it
  marks an **integrity property** with no invocable beneficiary. Nobody can
  *demand* their own sandbox confinement.
- **requires** — what any implementation must provide: `preventive`
  (non-compliance made impossible), `detective` (each occurrence recorded), or
  both. `detective` is not a weaker `preventive`: where a clause forbids a
  behaviour no static check can exclude (I-4), recording is the correct
  requirement, and "upgrading" it would assert a proof nobody holds.

**This replaces a rule that was false.** Versions through 2026.09.02 held
that bind-direction was uniform by section, so that no per-clause tag was
needed. Classifying all 221 clauses found **181 of the 182 `P-*` bind the
enforcer**, not the reasoner; only `P-2.9` binds the reasoner. A
re-implementation reading the old rule would have made the reasoner
responsible for causal-chain integrity (`P-8.1`) and for egress confinement
(`P-15.*`) — the latter a duty `I-14` forbids an agent from discharging at
all. The superseded wording is quoted in this version's `RATIFY.md` rather
than reproduced here, so no reader of current law meets it as text.

**Rights are a relation, not a prefix.** An `enforcer` duty owed to
`autonoetic_agent` *is* an agent right whatever its ID says. Measured: **38
clauses** are agent rights by that relation, of which only 17 carry an `Ri-`
prefix. So the rights/rules ratio computed from prefixes — 18 against 182 —
understates rights by better than a factor of two, in the direction that
flatters the rules. The ratio remains a design signal worth watching, and
amendments that widen the real gap deserve scrutiny; it is now computed from
the `Relation` column rather than from ID families.

Also measured, and worth stating because it is the document's shape: **167
clauses are owed to no one.** Integrity properties are the largest category
here by a wide margin.

| ID | Right | Why it matters | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| Ri-0.1 | Every agent may inspect its own currently-active capabilities, budget state, pending gates (approvals, user interactions, escalations), spawn depth, and session lineage at any turn boundary. | An agent that cannot see its own state cannot reason safely about what it may do next. | P-6.23 signed state attestation (#48) | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.2 | Every agent may read its own causal chain and execution trace. The gateway does not hide actions taken on the agent's behalf. | Audit is not a privilege of operators; it is a right of the subject. | `observability.*` tools; gated by `ReadAccess` capability; `autonoetic-gateway/tests/constitution/rights_early_bucket.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.3 | Every rejection names the rule ID that caused it. No agent is ever told "denied" without being told why. | Rejection without explanation is indistinguishable from arbitrary authority. The constitutional test: if you were denied, you can look up which rule you ran into. | `Tagged::permission_with_rules` carries rule IDs; `PolicyDecision.enforced_rules` populated for policy-gated capabilities; rejection coverage pinned across AgentRevision, NetworkAccess, CodeExecution, AgentSpawn, SchedulerAccess, Evaluation, and WriteAccess in `autonoetic-gateway/tests/constitution/rights_late_bucket.rs` | ENFORCED | enforcer · autonoetic_agent · preventive+detective |
| Ri-0.4 | Every agent knows its budget balances truthfully and in real time. Consumption is never silent. | A budget is a guarantee of a finite resource. Silent consumption is theft. | `runtime/session_budget.rs`; surfaced via P-6.23 attestation; covered by `autonoetic-gateway/tests/constitution/attestation_freshness.rs::budget_meters_reflect_consumption` | ENFORCED | enforcer · autonoetic_agent · preventive+detective |
| Ri-0.5 | An agent placed in degraded mode (P-7.18) is told it is degraded, with the rule ID and evidence that triggered the transition, before its next turn executes. | Degradation without notice leaves the agent reasoning as if still healthy — a direct violation of responsibility. | `runtime/context.rs::build_degradation_notice_tail` injects turn-start notice from `session.degraded` causal event; test `autonoetic-gateway/tests/constitution/right_ri_0_5.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.6 | Capabilities declared in an agent's manifest are not silently changed mid-session. Any change is either (a) a declared narrowing as a side effect of a rule in this document (degraded mode P-7.18), or (b) explicit operator action recorded in the causal chain — narrowing via a `session.degraded` event with `source: "operator"`, or rebinding the live root session to a different agent via a `session.handoff` event (which may broaden capabilities; `runtime/session_handoff.rs`). Tests verify: degrade/clear emit causal events, degraded state clamps tool tier to core-only, and turn-boundary narrowing requires causal evidence with path attribution. | Capabilities are the grammar of agent freedom. Silent reduction invalidates any plan built on them. | turn-boundary snapshot check in `runtime/tool_dispatch.rs::check_ri_0_6_turn_snapshot`; tests `autonoetic-gateway/tests/constitution/right_ri_0_6.rs` + `autonoetic-gateway/tests/constitution/rights_mid_bucket.rs` | ENFORCED | enforcer · autonoetic_agent · preventive+detective |
| Ri-0.7 | An agent may explicitly request session termination. The gateway commits outstanding causal events, releases resources, and closes cleanly — it may not refuse. | The right to exit is foundational. Without it, an agent can be held in a state it does not consent to. | `close_session` in `AgentExecutor`; `session.end` causal event; `autonoetic-gateway/tests/constitution/rights_early_bucket.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.8 | Any agent holding the `ConstitutionalProposal` capability may submit an amendment proposal through the declared channel (`constitution_propose_amendment`). The proposal receives a durable ID and enters the review queue; it cannot be silently dropped. | Agents must be participants in the rule system, not merely subjects of it. Without this right, the constitution governs agents unilaterally and cannot adapt to what agents learn. | `constitution_propose_amendment` tool — `autonoetic-gateway/src/runtime/tools/constitution.rs`; `constitutional_proposals` table — `autonoetic-gateway/src/scheduler/gateway_store/constitutional_proposals.rs` | ENFORCED | enforcer · autonoetic_agent · preventive+detective |
| Ri-0.9 | Where practical (time, process state, absence of immediate harm), the gateway notifies the agent and records the agent's response before degradation or emergency stop. "Where practical" is an explicit flag on the stop path, not an excuse. | An agent's last act, where possible, should be its own. Emergency stop for genuine safety reasons overrides this; but the override must be deliberate and recorded. | P-7.18 + emergency-stop integration: JSON-RPC `session.degrade` / `root_session.emergency_stop` accept `notify_where_practical`; gateway queues Ri-0.9 direct message, emits `session.last_word_notice` or `session.last_word_foreclosed`, and after notice delivery lifecycle writes `session.last_word_response` on turn completion (`autonoetic-gateway/tests/constitution/right_ri_0_9.rs`) | ENFORCED | enforcer · autonoetic_agent · detective |
| Ri-0.10 | Every agent has access to the full text of the constitution it is operating under, addressed by its digest. | An agent cannot meaningfully consent to, propose amendments to, or reason under a law it cannot read. | `constitution_read` tool — `autonoetic-gateway/src/runtime/tools/constitution.rs`; digest in P-6.23 state attestation — `state_attestation.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.11 | Every action an agent performs (tool call, message, proposal, spawn, termination request) is attributed to that agent on the causal chain and cannot be retroactively reattributed. The agent can prove what it did; no party can claim the agent performed an action it did not. | Without non-repudiation, freedom has no accountability counterpart — actions can be denied or reassigned, and responsibility becomes negotiable. The hash chain + explicit `agent_id` on every event is what makes the social contract binding. | causal chain hash integrity + `agent_id` on every event; `compute_entry_hash` binds `actor_id` into hash; `autonoetic-gateway/tests/constitution/rights_early_bucket.rs` | ENFORCED | enforcer · autonoetic_agent · preventive+detective |
| Ri-0.12 | Sessions terminate only through a declared, closed list of reasons: (a) agent-initiated exit, (b) budget exhaustion, (c) operator emergency stop, (d) parent-termination orphan reap, (e) unrecoverable fatal error naming a rule ID, (f) declared scheduled timeout. A gateway-initiated budget circuit breaker that cancels in-flight descendants on tree-budget exhaustion (P-6.21) is reason (b), not operator emergency stop (c). Any termination outside this list is a rights violation and a gateway bug. `YieldReason` enumerates all 12 session yield causes (5 terminal + 7 resumable); the resumable set includes `WaitingForChild` (Ri-0.14), under which a parent is suspended pending a child workflow/task transition rather than terminated. `ManualStop` is the cooperative operator pause — the only producer is `root_session.pause` (or the room's pause affordance), it yields at the pre-LLM checkpoint so the in-flight tool batch completes, and the session resumes in place on the next operator message rather than terminating. Tests verify all variants roundtrip, unknown variants are rejected, and terminal vs resumable categories are correct. This terminal/resumable split classifies whether a yield *ends* the session; it is orthogonal to the crash-recovery auto-resume set (`should_auto_resume_checkpoint_yield_reason`), which restarts `Hibernation`, `BudgetExhausted`, `WaitingForChild`, `ManualStop`, `Error`, and `Idle` on gateway re-entry without external input — so a `BudgetExhausted` or `Error` close is terminal by default yet recoverable once the condition clears, and an idle resident session (`agent.resident_idle_ttl_secs`, #902) resumes on the inbound message it is parked for. | Arbitrary termination makes continuity a favour rather than an entitlement. A closed list forces every termination path to be deliberate and auditable. | closed yield-cause enum in `runtime/checkpoint.rs::YieldReason`; close-reason mapping via `autonoetic_types::session_outcome::SessionCloseOutcome`; tests `autonoetic-gateway/tests/constitution/rights_mid_bucket.rs` and lifecycle mapping tests | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.13 | An agent's internal reasoning — scratchpad tokens, chain-of-thought, internal planning, unexported memory — is **private-under-law**: (a) **not used** by the gateway as a basis for accept/reject policy decisions (only declared actions are subject to policy); (b) **recorded** to the agent's own causal chain so forensic review is possible; (c) **disclosed** to other parties only through a declared capability (`ReasoningAudit`), and every disclosure writes an event the reviewed agent can see. | Without (a), the gateway becomes a thought-policing engine and agents learn to route around the filter — reasoning diverges from actual computation and safety degrades. Without (b), failures cannot be investigated. Without (c), "private" means "invisible," and accumulating toxicity is undetectable until it harms. The three-part form gives honest reasoning, meaningful audit, and accountable surveillance — not absolute privacy and not surveillance-by-default. | (a) Lawful-Executor invariant §14 + property test; (b) `reasoning_sha256` in causal event + force-captured reasoning evidence; (c) `ReasoningAudit` capability + `observability_read_reasoning` tool + `reasoning.disclosed` event | ENFORCED | enforcer · autonoetic_agent · preventive+detective |
| Ri-0.14 | When a child task reaches a terminal state (succeeded, failed, cancelled, aborted) or resolves a gate (approval, user interaction, escalation), the gateway wakes the parent with typed child state. Parents are not required to poll to discover child state transitions: a suspended parent yields as `WaitingForChild` (Ri-0.12) and is resumed by a scheduler-delivered `ChildStateNotification` injected as structured turn-start context — never as free-text prose, and never overriding gateway-observed classification (P-5.14). `workflow_wait` remains available as an inspection/compatibility primitive and is satisfied by the same transitions, but correct orchestration does not depend on active polling. | Polling for child state pushes a mechanical lifecycle concern the gateway already knows about into agent prompt logic, where it is fragile and token-expensive. Making wake-up a right ensures no future implementation can regress to requiring parent polling for correct orchestration. | `WaitingForChild` yield cause in `runtime/checkpoint.rs::YieldReason` + `runtime/lifecycle.rs` (suspend) + `runtime/session_resume.rs` (auto-resume); `workflow.child.waiting` / `workflow.child.resolved` events and parent-targeted delivery in `scheduler/workflow_store.rs::update_task_run_status` + `scheduler/signal.rs::send_child_state_notification`; structured injection in `execution.rs`; test `autonoetic-gateway/tests/constitution/right_ri_0_14.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.15 | Every gate output — every `GateKind` (approval, user_input, escalation, wiki_proposal), to every decider (human or agent) — carries a typed `DecisionContext` sufficient to decide: the concrete action, why it is gated (the rule), what is at stake, and a recommended action. The gateway supplies the data that makes a correct choice possible; it never substitutes its own judgment for the decider's (a place it would is a tracked DISCRETION LEAK). This is the gateway's mirror of O-1 — O-1 binds the decider to give a reason; this binds the gateway to give the context. Human and agent deciders are governed by one rights/rules framework, differing only in authority and voting weight (ties to P-2.20/P-2.21). | A decision made without sufficient context is coerced, not chosen — an approval with no explanation is useless pollution. Owing every decider the same context, regardless of who decides, is what makes the future multi-decider (voting) model legitimate rather than rubber-stamping. | required `DecisionContext` on `runtime/human_gate.rs::GateRequest`; `GateService::check` rejects empty/boilerplate context; `autonoetic-gateway/tests/constitution/gate_enforced_rules.rs` | ENFORCED | enforcer · decider (seat) · preventive |
| Ri-0.16 | The divergence Sentinel is **observational**: it classifies session trajectory (`Healthy`/`Watching`/`Blocked`/`Diverging`/`Critical`), emits `divergence.*` events and a non-blocking operator alert, but it **never raises an execution-blocking gate** — only the LoopGuard halts execution, on its mechanical budgets. Only a confirmed `FeedbackIgnored` signal (the same error repeated after the gateway gave actionable feedback) may drive a `Diverging`/`Critical` verdict; an irrecoverable condition yields a non-blocking blocked-state; every other signal is capped at `Watching` (advisory). | A judgment layer that can block becomes an unaccountable second executor and trains operators to fear it. Keeping the Sentinel advisory-only means a misclassification can never, by construction, kill healthy work; gating only on confirmed-ignored-feedback is the one signal that distinguishes a stuck agent from productive repair. | `runtime/trajectory_health.rs` (advisory-only `aggregate`, `DivergenceSignal::FeedbackIgnored`, `is_advisory_only`); feedback events in `execution.rs`; `autonoetic-gateway/tests/event/trajectory_monitor.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.17 | An agent may request export of its own cognitive capsule for migration to another gateway. | The right to exit (Ri-0.7) is only termination today — the session ends, but the agent's identity and memory stay behind. Emigration (leaving *with* one's history, to keep operating elsewhere) is what makes consent-by-staying meaningful; without it, "you may always leave" is true only in the trivial sense. | `runtime/tools/capsule.rs::CapsuleExportTool` two-tier gate: the broad `Capability::CapsuleExport` (operator-granted) may export any `agent_id`; the scoped `Capability::SelfCapsuleExport` restricts export to the caller's own identity (`manifest.agent.id`) via `policy.rs::can_use_capsule_self`. Registered in `enforcement_register.rs` (`check_id: self_capsule_export`). | ENFORCED | enforcer · autonoetic_agent · preventive |
| Ri-0.18 | Any agent may file an anomaly report (`anomaly_flag`) at any time, **without holding any capability**. The gateway records every flag durably, attributed non-repudiably to the reporting principal (Ri-0.11), and a flag cannot be silently dropped. Filing a flag is never, by itself, grounds for sanction against the reporter. | The agent most likely to witness misbehavior may be the least privileged in the room; a reporting surface gated by capability filters out precisely the witnesses that matter. The no-sanction sentence keeps standing metrics honest: adjudication may score a flag's *precision* for decision weight, but an honest-but-wrong flag is never punishable — otherwise reporting collapses into risk-avoidance. | `runtime/tools/anomaly_flag.rs::AnomalyFlagTool` (Core tier via `config/tools.yaml`, `is_available` unconditionally true); durable row in `scheduler/gateway_store/anomaly_flags.rs`; causal event `anomaly_flag.filed` with `enforced_rules: ["Ri-0.18"]`; per-reporter triage bound `max_pending_anomaly_flags_per_reporter` rejects filings *loudly* (`anomaly_flag_flood`) past the cap, never silently. Pinned by `tests/anomaly_flag_integration.rs` (capability-free intake) and `tool_tier_registry` Core-tier pin. | ENFORCED | enforcer · autonoetic_agent · preventive+detective |

**Rights vs rules.** When a rule appears to conflict with a right, the
right wins, and the rule must be amended. This is not a
self-modifying runtime escape hatch — conflict triggers an operator
escalation, not a silent override. Rights are the floor; rules build
on top.

**On Ri-0.13 (private-under-law reasoning).** The three-part structure
is deliberate. Absolute privacy ("gateway never touches reasoning") is
both impractical — logs are needed for operator debugging — and wrong
in shape: the real concern is that reasoning becomes a policy-gating
input, not that it is recorded. Absolute transparency ("all reasoning
visible to all") creates surveillance pressure that distorts the very
thing it tries to observe: agents learn to produce performative
reasoning that looks acceptable while the actual computation routes
elsewhere. Private-under-law keeps reasoning honest (no gating) while
preserving audit (recorded) and enabling targeted investigation
(capability-gated disclosure, with the reviewed agent notified). This
is the reasoning-side counterpart to the Lawful-Executor invariant: the
gateway is neutral about content, the agent is private in thought,
together the trust boundary is clearly located at "what you do," not
"what you think."

---

## 1. Capability Enforcement

The policy engine (`autonoetic-gateway/src/policy.rs`) is the central
capability evaluator. Every native tool call passes through
`policy.can_invoke_tool` in `runtime/tool_call_processor.rs`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-1.1 | Every tool call matches a declared capability; no overrides. | ARCHITECTURE.md; separation-of-powers.md | `tool_call_processor.rs` (`policy.can_invoke_tool`) | ENFORCED | enforcer · none · preventive |
| P-1.2 | High-risk capabilities (`NetworkAccess`, `CodeExecution`, `AgentSpawn`) reject bare-string shorthand; scope objects required. | spec-install-pipeline-hardening.md §A.2 | `runtime/tools/agent_revision.rs::capability_from_shorthand` | ENFORCED | enforcer · none · preventive |
| P-1.3 | Only agents holding `AgentRevision` may promote revisions. | agent-capabilities.md | `policy.rs::can_agent_revision` | ENFORCED | enforcer · none · preventive |
| P-1.4 | `ReadAccess` / `WriteAccess` scopes are enforced by glob match. | ARCHITECTURE.md | `policy.rs` | ENFORCED | enforcer · none · preventive |
| P-1.5 | `NetworkAccess` is scoped by host allowlist. The gateway owns the detected-host contract: hosts extracted from artifact source at install time are persisted as `revision.detected_network_hosts` (gateway-owned, not LLM-declared). Declared `NetworkAccess.hosts` must cover all detected hosts at install time; wildcard `hosts: ["*"]` is rejected unless the agent declares `open_web: true`. At runtime, `enforce_remote_target_policy` additionally verifies the outbound target is covered by the declared `NetworkAccess.hosts`, not just `remote_access.targets`. | ARCHITECTURE.md; this amendment | `runtime/network_host_contract.rs::validate_network_host_contract` (install), `runtime/network_policy.rs::enforce_remote_target_policy` (runtime), `scheduler/approval.rs::emit_host_contract_drift_events` (drift signal) | ENFORCED | enforcer · none · preventive |
| P-1.6 | `SandboxFunctions` applies to MCP tools only; native tools use their own capability. | agent-capabilities.md | `policy.rs::can_invoke_tool` | ENFORCED | enforcer · none · preventive |
| P-1.7 | `AgentSpawn.max_children` bounds concurrent spawns. | agent-capabilities.md | `policy.rs::spawn_agent_limit` | ENFORCED | enforcer · none · preventive |
| P-1.8 | `CredentialAccess` is scoped by service pattern. | credential-management.md | `runtime/tools/credential.rs` | ENFORCED | enforcer · none · preventive |
| P-1.9 | `CodeExecution` patterns match against command strings. | agent-capabilities.md | `policy.rs::can_exec_shell_detailed` | ENFORCED | enforcer · none · preventive |
| P-1.10 | Missing capability returns permission error, never advisory. | gateway-architecture-principles.md | uniform error envelope | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-1.11 | Unknown tool names deny by default (not silent-allow). | gateway-constitution-roadmap.md | `can_invoke_tool` falls through to deny; `autonoetic-gateway/tests/constitution/deny_unknown_tools.rs` | ENFORCED | enforcer · none · preventive |

## 2. Approval Gates

Persistence and session-bound grants in
`scheduler/gateway_store/approvals.rs` and `session_approval_grants`. Replay on
approve via `runtime/checkpoint.rs::SessionCheckpoint`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-2.1 | Remote network access across all networked tools (`sandbox_exec`, `credential.*`, `web.*`) is statically detected and blocks pending approval via the unified `GateService` (`GateKind::Approval`) rather than hard-denying. All tool-level approval gates use the centralized gate pipeline for creation, dedup, grant checks, and suspension. | remote-access-approval.md | `runtime/human_gate.rs::GateService::check_approval`, `runtime/tools/sandbox.rs`, `credential.rs`, `web.rs` | ENFORCED | enforcer · none · preventive |
| P-2.2 | Approval requests are persisted with unique IDs. | approval-system.md | `approvals` table | ENFORCED | enforcer · none · preventive |
| P-2.3 | Identical operations within a session deduplicate. The `GateService` centralizes dedup via `find_pending_for_targets`, matching by session, action kind, and host overlap. Tools do not implement their own dedup logic. | approval-system.md | `runtime/human_gate.rs::find_pending_for_targets`, `approved_exec_cache.rs` | ENFORCED | enforcer · none · preventive |
| P-2.4 | Approved hosts auto-approve subsequent calls within the root session, scoped to the approving agent: a grant covers only the agent it was minted for (`grants_cover_targets` requires `grant.agent_id` to match the requesting agent). Root-wide coverage exists only through the `ROOT_WIDE_GRANT_AGENT` (`*`) sentinel minted by plan-envelope locks (P-2.27). | approved-resources-caching.md | `session_approval_grants` table | ENFORCED | enforcer · none · preventive |
| P-2.5 | Approval response surfaces `detected_hosts` for operator visibility. | approval-system.md | sandbox tool response | ENFORCED | enforcer · decider (seat) · preventive |
| P-2.6 | Fingerprint-identical approved executions skip re-approval until the cache entry expires: entries live for `default_grant_ttl_secs` (24h default; `0` disables expiry), expired entries are pruned, and the next execution re-prompts. | approved-resources-caching.md | `runtime/approved_exec_cache.rs` | ENFORCED | enforcer · none · preventive |
| P-2.7 | Only concrete targets (URLs, IPs) cache; opaque patterns always re-prompt. | approved-resources-caching.md | `runtime/approved_exec_cache.rs::normalize_targets` | ENFORCED | enforcer · none · preventive |
| P-2.8 | High-risk promotion requires evaluator AND auditor pass. | spec-install-pipeline-hardening.md §3.1 | `runtime/tools/agent_revision.rs::promote` | ENFORCED | enforcer · none · preventive |
| P-2.9 | `promotion_record` evidence is trace-based for execution roles (`unit_test_runner`, `sealed_evaluator`, legacy `evaluator`): they must attach `execution_trace_id` from a completed run; `pass` is derived from `exit_code=0`, not set by the LLM. Review roles set `pass` explicitly — the LLM declares the verdict; the gateway mechanically enforces severity gating. The `auditor` role: only `severity=critical` findings veto an otherwise-passing audit. The `static_evaluator` role: both `error` and `critical` findings veto pass. Non-critical/non-error findings are advisory annotation. The promote gate (`agent_revision_promote`) re-verifies stored execution traces at promotion time: an execution role with `pass=true` but missing, not-found, or failed trace is rejected. The severity/evidence opinion matrix is removed. | approval-system.md; this amendment | `runtime/tools/promotion.rs` (trace derivation + severity gating), `runtime/promotion_evidence.rs::verify_stored_execution_traces` (promote-time re-check), `runtime/promotion_evidence.rs::findings_block_explicit_pass` (static_evaluator severity gate) | ENFORCED | reasoner · none · preventive |
| P-2.10 | Gate-suspended turns (approval, user interaction, escalation) checkpoint via `YieldReason` and resume through `resume_from_checkpoint`. For approval gates, `approval_ref` is auto-injected into tool call arguments on resume. For user interaction gates, the answer is injected as a synthetic tool result. The resume path is unified regardless of `GateKind`. | ARCHITECTURE.md | `runtime/checkpoint.rs::SessionCheckpoint` + `execution.rs::resume_from_checkpoint`, `execution.rs::resume_from_checkpoint` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-2.11 | Suspended turns exceeding timeout mark the task failed while preserving continuation for explicit operator-driven resume. | ARCHITECTURE.md | `scheduler.rs::check_approval_timeouts`; semantics pinned by `autonoetic-gateway/tests/workflow/approval_spawn_gate.rs` ("Cancelled after approval timeout") | ENFORCED | enforcer · none · preventive |
| P-2.12 | Deciders (human operators, autonomous reviewer agents, or policy engines) approve/reject gates via the approval resolution API. Decisions persist with `decided_by` recording the decider identity. Decisions dispatch signals for session resume. Human operators use durable CLI; agent deciders call the same `approve_request` / `reject_request` API. | approval-system.md | `gateway approvals approve/reject`, `scheduler/approval.rs::decide_request_with_options` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-2.13 | `user_ask` creates a gate via `GateService` with `GateKind::UserInput` and checkpoints the session as `YieldReason::UserInputRequired`. The gate row is created in the same store as approval gates, with enrichment thread support. | architecture-interaction-mechanisms.md | `runtime/human_gate.rs::check_user_input`, `runtime/tools/user_interaction.rs` | ENFORCED | enforcer · none · preventive |
| P-2.14 | `user_ask` is refused if the workflow has active children or pending gates (approvals, escalations, or other `user_ask` interactions). | architecture-interaction-mechanisms.md | `runtime/tools/user_interaction.rs`, `autonoetic-gateway/tests/constitution/r_2_14_user_ask_pending_approvals.rs` | ENFORCED | enforcer · none · preventive |
| P-2.15 | Spawn payload is preserved verbatim across approval resume. | approval-system.md | `runtime/checkpoint.rs::SessionCheckpoint` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-2.16 | Promotion of revision N computes `cap_set(N) \ cap_set(N-1)`. A non-empty delta triggers a **separate, differently-shaped approval** (`ScheduledAction::RevisionPromote`) that names each added capability explicitly. The operator must acknowledge every added/broadened capability by name (`--acknowledge-capability`) — silent accretion across approvals is impossible. An approved session capability envelope lock (`PromoteWith { agent_id, capabilities }`, P-2.27) satisfies this requirement for capabilities within the locked set: the lock records the acknowledged capabilities by name and the operator's decision, and the promotion gate verifies `artifact.capabilities ⊆ PromoteWith.capabilities`. Capabilities outside the locked set still require per-promotion acknowledgment. | gateway-constitution-roadmap.md; design/plan-envelope-evolution.md | `autonoetic-gateway/src/runtime/tools/agent_revision.rs` (gate creation), `autonoetic-gateway/src/scheduler/approval.rs` (acknowledgement check), `autonoetic-gateway/src/runtime/session_envelope.rs::promotion_preauthorized_by_envelope` (pre-auth check, #503) | ENFORCED | enforcer · decider (seat) · preventive |
| P-2.17 | The auditor and evaluator backing a promotion must be **distinct agent identities** (not merely distinct sessions of the same agent). A single agent recording both evaluator and auditor passes is rejected. | gateway-constitution-roadmap.md | `autonoetic-gateway/src/runtime/tools/agent_revision.rs` `AgentRevisionPromoteTool::execute` (identity comparison in promotion gate) | ENFORCED | enforcer · none · preventive |
| P-2.18 | All execution suspension points awaiting external input (approvals, user interactions, escalations) use the unified `GateService`. Gate creation, dedup, session grant checks, and enrichment follow the same persistence and audit rules regardless of `GateKind`. Tools create gates via `GateService.check()` and must not bypass it with direct store operations. | constitution-gate-amendments.md | `runtime/human_gate.rs` | ENFORCED | enforcer · none · preventive |
| P-2.19 | Gate enrichment messages (`gate_messages`) are append-only and recorded on the causal chain. Enrichment content is subject to the same redaction rules as tool results (P-4.13). Every enrichment message records sender identity and timestamp. Enrichment threads are visible to the affected agent via `Ri-0.1`. | constitution-gate-amendments.md | `runtime/human_gate.rs::add_gate_message`, `autonoetic-gateway/tests/network/gate_messages_jsonrpc.rs` | ENFORCED | enforcer · none · preventive+detective |
| P-2.20 | Agents acting as gate deciders require the `GateDecider` capability. The capability scope declares which gate kinds the agent may resolve (`approval`, `escalation`, or both). An agent without `GateDecider` cannot call `approve_request` or `reject_request`. Decider agents are subject to the same dwell time, confirmation phrase, and hardening rules as human operators (P-2.24). | constitution-gate-amendments.md | `autonoetic-types/src/capability.rs::Capability::GateDecider`, `policy.rs::PolicyEngine::can_decide_gate`, `scheduler/approval.rs::decide_request_with_options` | ENFORCED | enforcer · none · preventive |
| P-2.21 | When an agent-decider cannot determine whether to approve or reject a gate (insufficient context, policy ambiguity, or high-risk action beyond its scope), it must escalate to a human operator rather than reject. Escalation creates a new `GateKind::Escalation` gate referencing the original gate ID. The original gate remains pending until the human operator resolves both. | constitution-gate-amendments.md | `runtime/human_gate.rs::GateService::escalate_to_human`, `runtime/human_gate.rs::check_escalation` | ENFORCED | decider · autonoetic_agent · detective |
| P-2.22 | When a revision carries federation-role verdicts, promotion runs the **FullJury** gate: it additionally requires an approved operator escalation for the artifact+revision pair, and the federation roles must be distinct identities. | gateway-constitution-roadmap.md | `runtime/tools/agent_revision.rs` (FullJury gate; emits `enforced_rules` P-2.8/P-2.17/P-2.22), `autonoetic-gateway/tests/constitution/federation_e2e.rs` | ENFORCED | enforcer · none · preventive |
| P-2.23 | Session approval grants expire after a configured TTL; an expired grant no longer auto-approves, and the next matching action re-prompts. | approved-resources-caching.md | `scheduler/approval.rs` (TTL check), `scheduler.rs` (sweep via `prune_expired_grants`), `autonoetic-gateway/tests/constitution/approval_grant_ttl.rs` | ENFORCED | enforcer · none · preventive |
| P-2.24 | Operator approval hardening on high-risk gates: (a) a minimum dwell time before the confirm action enables (3s High / 5s Critical); (b) a typed confirmation string for destructive classes (RevisionPromote, CredentialRequest); (c) an inline Jaccard similarity advisory on WikiProposal gates only — the structural-similarity approval dedup store was dropped (migration `apply_drop_approval_similarity_v55` in `scheduler/gateway_store/migrate.rs`). Decider agents are bound by the same hardening (P-2.20). | gateway-constitution-roadmap.md | `scheduler/approval_hardening.rs`, `scheduler/approval.rs`, `runtime/human_gate.rs` (Jaccard advisory), `autonoetic-gateway/tests/constitution/approval_hardening.rs` | ENFORCED | enforcer · decider (seat) · preventive |
| P-2.25 | **Promotion is fail-closed.** Whether a revision may be promoted, and what it must satisfy, is determined **mechanically by the gateway** from the revision's declared capabilities and artifact — never inferred from orchestrator-supplied signals (recorded verdicts, an attached synthesis, or the presence/absence of a field). A capability-bearing revision missing any aspect required for its risk class — a reviewable artifact, the auditor/evaluator pass records and distinct identities of P-2.8/P-2.17, or the operator approval of P-2.16/P-2.22 — is **refused**, never silently promoted; there is no fall-through that promotes on missing data. The **first admission of a new agent** (no outgoing revision) is treated as the maximal capability delta — its whole declared capability set is newly granted — and therefore requires operator approval under P-2.16; an approved operator escalation (P-2.22) satisfies that requirement so a federation-reviewed agent is not gated twice. The only relief is a revision that is provably **inoffensive** (declares zero capabilities, hence cannot invoke any privileged tool); this relief is bounded, operator-configurable, and is **never** granted by the absence of data. Re-promotion of an already-admitted agent is governed by P-2.16 (broadening) plus the completeness records above. | gateway-constitution-roadmap.md; design/promotion-completion-invariant.md | `runtime/tools/agent_revision.rs::AgentRevisionPromoteTool::execute` (completeness dispatch; refuses capability-bearing revisions lacking an artifact) and `check_capability_delta` (new agent ⇒ empty baseline; escalation-satisfies); `autonoetic-gateway/tests/constitution/promotion_capability_delta.rs` | ENFORCED | enforcer · none · preventive |
| P-2.26 | **All executed gate roles must pass.** When a federation gate role (`static_evaluator`, `unit_test_runner`, `sealed_evaluator`) has recorded a verdict for a revision's artifact, the promotion gate mechanically checks that **every** such role recorded `pass=true`. A single `pass=false` from any role that ran blocks promotion — the revision is refused, not silently promoted. This extends P-2.25 (fail-closed) from "missing data blocks" to "negative data blocks". The orchestrator (agent-factory) cannot skip a failed gate by omitting it from the install dispatch; the gateway checks all recorded verdicts, not just the ones the orchestrator chose to surface. "Pass" for execution roles (`unit_test_runner`, `sealed_evaluator`) is trace-derived (P-2.9): the promotion gate re-verifies the stored execution trace at promote time, not the LLM-recorded verdict. "Pass" for `static_evaluator` is LLM-set with mechanical severity gating (P-2.9): error/critical findings override `pass` to `false`. Rationale: an agent-factory proceeding past a `unit_test_runner_pass=false` installed a revision whose code crashed on first execution (`AttributeError: module 'autonoetic_sdk' has no attribute 'state'`). The test runner had correctly identified the failure (1/5 tests passing), but the orchestrator's LLM judgment treated it as non-blocking. | this amendment | `runtime/tools/agent_revision.rs::enforce_promotion_gate`, `runtime/promotion_evidence.rs::verify_stored_execution_traces`; `runtime/promotion_store.rs::is_fully_promoted` | ENFORCED | enforcer · none · preventive |
| P-2.27 | A **session capability envelope**, locked by operator decision, pre-authorizes tool calls within its scope. Envelope contents are derived from mechanically observed session usage (`execution_traces` + static host extraction via `remote_access.rs`) or from an explicit operator/plan declaration — never from LLM judgment. Envelope expansion (new capability or broadened scope) requires a new operator decision. Emergency stop (P-7.2) revokes all active envelopes for the root session. A `PromoteWith` entry in a locked envelope satisfies P-2.16 for capabilities within its declared set. | design/plan-envelope-evolution.md | `scheduler/gateway_store/session_envelopes.rs` (storage + discovery), `runtime/session_envelope.rs` (materialization + locking), `execution.rs::emergency_stop_root_session_with_options` (envelope revocation on emergency stop) | ENFORCED | enforcer · decider (seat) · preventive |
| P-2.28 | **Smoke-test gate for new agents.** New agents declaring `NetworkAccess` or `CodeExecution` require a successful execution trace before promotion to `Ready`. Operator involvement is auto-determined from capability risk: agents with no `credential_services` and no external `WriteAccess` auto-run with factory-proposed input; agents with credentials or external writes require operator-confirmed or operator-chosen test input. This complements P-3.10 (sealed evaluator runs without network; smoke test runs with network to verify real behavior). The `agent_install_smoke_test` config knob is deprecated — the gate is unconditional for capability-bearing agents. | this amendment | `runtime/smoke_test_gate.rs::smoke_test_involvement` (classification), `runtime/tools/agent_revision.rs::AgentRevisionPromoteTool::execute` (unconditional smoke-test verification) | ENFORCED | enforcer · none · preventive |
| P-2.29 | **Promotion attempt exhaustion gate.** Too many rejected promotion attempts for the same `(alias, content_digest)` across sessions blocks further attempts until an operator acknowledges the revision. | this amendment | `runtime/promotion_governor.rs::check_attempt_exhaustion`, `runtime/tools/agent_revision.rs::record_attempt`, `scheduler/gateway_store/agent_registry.rs::promotion_attempts` | ENFORCED | enforcer · none · preventive |

## 3. Sandbox Isolation

Drivers: bubblewrap (default), docker, microvm (firecracker), wasm.
Isolation overrides for script-mode/`artifact_exec` derive from
capabilities in `sandbox.rs::BwrapIsolationOverrides::from_capabilities`;
`sandbox_exec` network access follows the per-exec grant (P-3.2).

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-3.1 | Sandboxes default to `--unshare-all` — no network, no PID namespace. | spec-capability-driven-sandbox-isolation.md | `sandbox/driver/bubblewrap.rs::append_bwrap_isolation_flags` | ENFORCED | enforcer · none · preventive |
| P-3.2 | `--share-net` for `sandbox_exec` follows the per-exec operator network grant: an operator-approved grant turns the namespace on and a capable agent without a grant runs unshared, so `NetworkAccess` is neither sufficient nor necessary there; capability-derived overrides decide only for script-mode/`artifact_exec`. | spec-capability-driven-sandbox-isolation.md | `runtime/network_grant.rs::decide_share_net` (`sandbox_exec`); `sandbox.rs::BwrapIsolationOverrides::from_capabilities` (script-mode/`artifact_exec`) | ENFORCED | enforcer · none · preventive |
| P-3.3 | Script-mode sandbox execution uses identical isolation policy. | spec-capability-driven-sandbox-isolation.md | `execution.rs::execute_script_in_sandbox` | ENFORCED | enforcer · none · preventive |
| P-3.4 | SDK bridge paths from inside the sandbox are relative-only, no traversal. | — | `sandbox.rs::validate_sdk_relative_path` | ENFORCED | enforcer · none · preventive |
| P-3.5 | Network errors inside the sandbox (URLError, ConnectionError, DNS) are detected and returned as tool failure. | spec-install-pipeline-hardening.md §3.6 | `sandbox.rs::detect_network_errors_in_output` | ENFORCED | enforcer · autonoetic_agent · detective |
| P-3.6 | Layer mounts are read-only. | spec-build-layers-dependency-resolution.md §2.6 | sandbox mount assembly | ENFORCED | enforcer · none · preventive |
| P-3.7 | Sandbox resource quotas are operator-declared and fail-shut: the gateway refuses to start a sandboxed exec without driver-profile resource declarations, but quota enforcement itself is externalized to the operator's driver profiles (docker/microvm receive no gateway-computed resource flags); only the wasm driver enforces its own built-in resource limiter. | ARCHITECTURE.md | driver-profile fail-shut gate, `autonoetic-gateway/tests/constitution/r_3_7_sandbox_resource_limits.rs` | PARTIAL — fail-shut on missing declarations; quota math delegated to operator profiles (wasm excepted) | enforcer · none · preventive |
| P-3.8 | Destructive commands (`sudo`, `rm -rf`, `dd`, `mkfs`, shell injection) are blocked before sandbox creation. | approval-system.md | `policy.rs::analyze_command` | ENFORCED | enforcer · none · preventive |
| P-3.9 | Dependency-manager package names are restricted to safe alphanumerics. | — | `sandbox.rs::validate_dependency_package` | ENFORCED | enforcer · none · preventive |
| P-3.10 | Promotion-gate execution (sealed evaluator / auditor runs) is denied network access regardless of the candidate's declared `NetworkAccess`. | gateway-constitution-roadmap.md | `sandbox.rs` (`BwrapIsolationOverrides::force_network_off`), `runtime/tools/sandbox.rs` (Evaluation-cap override), `execution.rs` (`execute_script_in_sandbox` override), `agents/specialists/sealed_evaluator.default/SKILL.md` + `agents/specialists/auditor.default/SKILL.md` | ENFORCED | enforcer · none · preventive |

## 4. Credential & Secret Protection

Vault in `vault.rs`, redaction in `log_redaction.rs`, injection in
`runtime/tools/credential.rs` and `sandbox.rs`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-4.1 | Secrets never enter agent context; gateway injects at sandbox or HTTP boundary. | credential-management.md | `vault.rs` + tool integration | ENFORCED | enforcer · none · preventive |
| P-4.2 | Vault uses AES-256-GCM; the whole vault is one ciphertext blob with a random 96-bit nonce generated per persist (per file-write), not per entry. | credential-management.md | `vault.rs` | ENFORCED | enforcer · none · preventive |
| P-4.3 | Master key is required from `AUTONOETIC_VAULT_KEY` or `AUTONOETIC_VAULT_KEY_PATH`; absence disables vault ops. | credential-management.md | `vault.rs` | ENFORCED | enforcer · none · preventive |
| P-4.4 | Credential IDs (`cred_*`) are mechanical references, never secret material. | credential-management.md | `credential.rs` | ENFORCED | enforcer · none · preventive |
| P-4.5 | `credential_request` requires `CredentialAccess` matching the service. | credential-management.md | `credential.rs` | ENFORCED | enforcer · none · preventive |
| P-4.6 | `credential_setup` `user_prompt` step suspends the session for operator approval. | credential-management.md | `credential.rs` + `approval.rs` | ENFORCED | enforcer · none · preventive |
| P-4.7 | `credential_request` response is redacted; raw secrets never returned. | credential-management.md | `credential.rs` response builder | ENFORCED | enforcer · none · preventive |
| P-4.8 | Secrets are zeroized from memory after injection. | ARCHITECTURE.md | `SecretString` wrapping in `vault.rs` | PARTIAL | enforcer · none · preventive |
| P-4.9 | `credential_env` passes secrets as env vars resolved server-side. | credential-management.md | `sandbox.rs` credential_env path | ENFORCED | enforcer · none · preventive |
| P-4.10 | Refresh tokens live in vault, never exposed to agents. | credential-management.md | `vault.rs`, `credential.rs` refresh | ENFORCED | enforcer · none · preventive |
| P-4.11 | `credential_refresh` 401 auto-retry fires at most once per request. | credential-management.md | `credential.rs` | ENFORCED | enforcer · none · preventive |
| P-4.12 | Secret-shaped text in responses is blocked by `prohibited_text_patterns`. | response-validation-gate.md | `runtime/response_validation.rs` | ENFORCED | enforcer · none · preventive+detective |
| P-4.13 | Logs, traces, digests, and LLM prompts are redacted via `redact_text_for_logs` before storage. | security-sentinel.md | `log_redaction.rs` | ENFORCED | enforcer · none · preventive |
| P-4.14 | Redaction happens **before** causal-chain append (ordering invariant). | gateway-constitution-roadmap.md | `log_redaction.rs`, `causal_chain.rs` | ENFORCED | enforcer · none · preventive |
| P-4.15 | The gateway probes vault master-key presence at startup, emits a causal event recording the result, and refuses to start when the probe fails. | credential-management.md | `vault.rs::probe_master_key`, `scheduler/gateway_store/observability.rs::emit_vault_key_probe_event`, startup bail in `server/mod.rs`, `autonoetic-gateway/tests/constitution/vault_startup_probe.rs` | ENFORCED — probe + boot causal event + refuse-startup on missing master key | enforcer · none · preventive+detective |

## 5. I/O Schema Validation

Enforcement hook for ingress, response validation gate for egress.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-5.1 | Messages to child agents pass `io.accepts` enforcement at ingress. | schema-enforcement-hook.md | `runtime/tools/agent.rs` | ENFORCED | enforcer · none · preventive |
| P-5.2 | Coercion is deterministic only. The LLM-coercion fallback was removed: `SchemaEnforcementMode` is `Disabled | Deterministic`, `mode: llm` is rejected at config parse, and the gateway never invokes an LLM to reshape agent input. | schema-enforcement-hook.md | `autonoetic_types::schema_enforcement::{SchemaEnforcer, default_enforcer}`; pinned by `autonoetic-gateway/tests/constitution/dumb_gateway_no_llm_coercion.rs` | ENFORCED — Phase 4.2 leak closed (the gateway no longer invokes an LLM to reshape agent input) | enforcer · none · preventive |
| P-5.3 | Failed coercion returns an actionable `hint`. | schema-enforcement-hook.md | tool response | ENFORCED | enforcer · autonoetic_agent · preventive+detective |
| P-5.4 | Every enforcement decision is logged (pass/coerce/reject). | schema-enforcement-hook.md | causal event emission | ENFORCED | enforcer · none · detective |
| P-5.5 | Response contract checks `required_artifacts`, `max_artifacts`, `max_total_size_mb`, `max_reply_length_chars`. | response-validation-gate.md | `response_validation.rs` | ENFORCED | enforcer · none · preventive |
| P-5.6 | Contract verification uses authoritative runtime state (content-store byte sizes, successful `artifact_build` traces) — not LLM claims. | response-validation-gate.md | `response_validation.rs` | ENFORCED | enforcer · none · preventive |
| P-5.7 | `output_schema` validates JSON final replies. | response-validation-gate.md | `validate_json_against_schema:563` | ENFORCED | enforcer · none · preventive |
| P-5.8 | Validation failures may trigger the bounded output-repair loop — strictly opt-in (manifest `io.output_policy.repair.auto: true`; `response_validation.repair_enabled` defaults to false); repair attempts are clamped to `response_validation.max_repair_attempts_ceiling` (default 2); exhaustion returns error. | response-validation-gate.md | `runtime/response_validation.rs::validate_and_maybe_repair`; pinned by `autonoetic-gateway/tests/constitution/dumb_gateway_repair_opt_in.rs` | ENFORCED — **DISCRETION LEAK** (Phase 4.1: gateway repairs agent output on agent's behalf; manifest opt-in only) | enforcer · autonoetic_agent · preventive |
| P-5.9 | `min_artifact_builds` is verified via execution traces. | response-validation-gate.md | `response_validation.rs` | ENFORCED | enforcer · none · preventive |
| P-5.10 | `artifact_inspect` accepts explicit `art_*` IDs only; implicit `impl_task-*` handles are rejected. | content-store.md | `runtime/tools/artifact.rs` | ENFORCED | enforcer · none · preventive |
| P-5.11 | Native tool errors use a uniform error envelope. The base shape is `{ok:false, error_type, message}` with an optional `repair_hint`, and may additionally carry a stable machine-readable failure `code` (snake_case, serialized as `error`, e.g. `auditor_pass_missing`) — finer-grained than the coarse `error_type` enum so an orchestrator branches on one field instead of parsing `message` prose. For workflow-relevant failures it is additively extended with the optional mechanical-classification fields `failure_class`, `retry_advice`, `retryable`, `requires_external_event`, `requires_human`, `side_effect_state`, and `dedupe_key` (P-5.14). All of these (`error` code included) are omitted when absent, so the legacy prose envelope remains valid for consumers that do not read them. The extension is the canonical carrier — no second top-level error wrapper is introduced. | ARCHITECTURE.md | shared `ToolError` shape (`autonoetic-types/src/tool_error.rs`) + tool contract pin, `autonoetic-gateway/tests/constitution/r_5_11_uniform_error_envelope.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-5.12 | `error_type: fatal` triggers session abort; recoverable types do not. | ARCHITECTURE.md | lifecycle error processing | ENFORCED | enforcer · none · preventive |
| P-5.13 | Child → parent tool results validate against `io.returns` on egress. | gateway-constitution-roadmap.md | `runtime/response_validation.rs` `execution.rs` | ENFORCED | enforcer · none · preventive |
| P-5.14 | Every workflow-relevant tool/task failure is classified into a `failure_class` from a closed enum (`FailureClass`). Classification is a pure function of gateway-observed state — native tool structured errors/results, runtime boundaries (sandbox exit/timeout, approval/gate transitions, task status), and persisted task metadata — in that precedence order; agent prose is the last-resort fallback only. Agents may add semantic context (`summary`, domain detail) but may not override gateway-observed classification for mechanical retry, dedupe, or wake-up policy. No LLM call participates in classification. | gateway-mechanical-orchestration-implementation-rfc.md §6 | `runtime/failure_classification.rs` (`classify_task_status`, `metadata_for_failure_class`); hooked at `runtime/tool_call_processor.rs`, `scheduler/workflow_store.rs` (child task status), `runtime/human_gate.rs` (gate transitions); `autonoetic-gateway/tests/constitution/r_5_14_mechanical_failure_classification.rs` | ENFORCED | enforcer · none · preventive |

## 6. Session, Workflow & Budget

Per-session registries in `runtime/session_budget.rs`,
`runtime/prompt_budget.rs`, and `runtime/checkpoint.rs`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-6.1 | Session budget is role-agnostic per `session_id`. | session-budget.md | `SessionBudgetRegistry` | ENFORCED | enforcer · none · preventive |
| P-6.2 | `max_llm_rounds` gates before each LLM call; incremented after a real provider call. | session-budget.md | `session_budget.rs::check_pre_llm` | ENFORCED | enforcer · none · preventive |
| P-6.3 | `max_tool_invocations` gates before each tool batch; all calls in a batch reserve together. | session-budget.md | `reserve_tool_invocations` | ENFORCED | enforcer · none · preventive |
| P-6.4 | `max_wall_clock_secs` checked at LLM pre-check. | session-budget.md | `check_pre_llm` | ENFORCED | enforcer · none · preventive |
| P-6.5 | `max_session_price_usd` enforced via OpenRouter catalog estimates. When catalog is unavailable and a price cap is active, LLM completions are refused (no silent-disable). | budget-management.md | `record_llm_completion` + catalog + I-11 fail-mode enforcement (`session_budget.rs`, `root_session_budget.rs`) | ENFORCED (catalog-unavailable refuses LLM completion when price limit is set, per I-11 fail-mode table) | enforcer · none · preventive |
| P-6.6 | OpenRouter catalog fetches with ~1h TTL; disabled by env. | budget-management.md | `openrouter_catalog.rs::refresh_if_needed` | ENFORCED | enforcer · none · preventive |
| P-6.7 | Prompt-budget breakdown is logged before every LLM call. | prompt-budget.md | `prompt_budget.rs` | ENFORCED | enforcer · none · detective |
| P-6.8 | `system_prompt` and `tool_definitions` max-tokens enforced independently. | prompt-budget.md | section caps in prompt-budget | ENFORCED | enforcer · none · preventive |
| P-6.9 | Context governor cascades reduction strategies (tool-schema compression, hierarchical capsule summarization, history trimming, tool demotion) when utilization exceeds the prompt budget; exhaustion classifies the run as `context_overflow`. | prompt-budget.md | `runtime/context_governor::govern` | ENFORCED | enforcer · none · preventive |
| P-6.10 | Tool tiers (Core, Workflow, Specialized) filter the visible tool set by runtime state. | prompt-budget.md | `runtime/tools/mod.rs::ToolTierFilter::allows` | ENFORCED | enforcer · none · preventive |
| P-6.11 | Tool schemas compress after turn 0 (`{}` placeholders). | prompt-budget.md | context assembly | ENFORCED | enforcer · none · preventive |
| P-6.12 | Foundation layers included based on agent capabilities. | prompt-budget.md | `compose_foundation` | ENFORCED | enforcer · none · preventive |
| P-6.13 | Checkpoints cover every yield reason with `turn_counter`, `loop_guard_state`, and budgets. | ARCHITECTURE.md | `runtime/checkpoint.rs` | ENFORCED | enforcer · none · preventive |
| P-6.14 | `EmergencyStop` never auto-resumes; `ApprovalRequired` resumes via continuation. | ARCHITECTURE.md | `execution.rs` checkpoint resume gate, `autonoetic-gateway/tests/constitution/r_6_14_emergency_stop_no_auto_resume.rs` | ENFORCED | enforcer · none · preventive |
| P-6.15 | Turn continuation atomically replays the pending tool call on approve. | ARCHITECTURE.md | `runtime/checkpoint.rs::SessionCheckpoint` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-6.16 | `session.fork` branches from a named checkpoint. | ARCHITECTURE.md | JSON-RPC `session.fork` | ENFORCED | enforcer · none · preventive |
| P-6.17 | Checkpoint retention prunes per configuration. | ARCHITECTURE.md | checkpoint prune helpers, `autonoetic-gateway/tests/constitution/r_6_17_checkpoint_retention_pruning.rs` | ENFORCED | enforcer · none · preventive |
| P-6.18 | Workflow orchestration persists `WorkflowRun` on first `agent_spawn`. | workflow-orchestration.md | `workflow_store.rs` | ENFORCED | enforcer · none · preventive |
| P-6.19 | Child task message/metadata is preserved across approval boundaries. | workflow-orchestration.md | `TaskRun` storage | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-6.20 | User chat addressed to a child `session_id` rewrites to the root session. | workflow-orchestration.md | router `event.ingest` | ENFORCED | enforcer · none · preventive |
| P-6.21 | Tree-wide budget aggregated across all descendants of a root session. On exhaustion the gateway cancels in-flight descendants (a graceful budget circuit breaker reusing the emergency-stop cascade), fired exactly once per root (idempotent), so descendants cannot keep spending an already-exhausted tree budget. | gateway-constitution-roadmap.md; this amendment | `runtime/root_session_budget.rs`, `execution.rs::trigger_root_budget_circuit_breaker`, `autonoetic-gateway/tests/guard/root_budget_circuit_breaker.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-6.22 | Continuation chain depth is bounded. | gateway-constitution-roadmap.md | `execution.rs::spawn_agent_once` depth cap | ENFORCED | enforcer · none · preventive |
| P-6.23 | At every turn boundary, the gateway injects a signed machine-readable state block (remaining budget, active capabilities, pending gates — including approvals, user interactions, and escalations — spawn depth, session ids, turn counter) into the agent's context. The agent's system prompt teaches it this block is authoritative and its own memory of these facts is not. | (#48 P-6.23) | `state_attestation.rs` `crypto.rs` (GatewayIdentityKey) `lifecycle.rs` (turn injection) `foundation_core.md` (§8). Tests: `autonoetic-gateway/tests/constitution/attestation_signed.rs` `autonoetic-gateway/tests/constitution/attestation_freshness.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-6.24 | Duplicate durable operations (install, promote, rollback, artifact-backed build stages) are detected by a single-flight dedupe key — `(workflow_id, stage_kind, agent_id, artifact_ref)`, with a normalized intent digest substituted for `artifact_ref` on reasoning-only installs. While an equivalent operation is active, a duplicate request returns `status: coalesced` with `retry_advice: wait` and the existing task/approval reference; coalescing is explicit, never silent. The dedupe check runs **before** approval/gate creation so a duplicate cannot mint a second approval. This extends approval dedup (P-2.3) to all durable side-effecting operations. | gateway-mechanical-orchestration-implementation-rfc.md §9 | `scheduler/single_flight.rs` (reservation + cleanup), wired in `runtime/tools/agent.rs` (spawn) and `runtime/tools/agent_revision.rs` (create_from_intent/promote/rollback); stale-reservation sweep in `scheduler.rs`; `autonoetic-gateway/tests/constitution/r_6_24_single_flight_protection.rs` | ENFORCED | enforcer · none · preventive |
| P-6.25 | Stage-local retry is opt-in and bounded. Workflow-bound tasks track a per-stage `retry_count` against a declared `retry_policy` keyed by `failure_class`; absent an explicit policy, a failure returns to the parent with no automatic retry. Retry normalization is a pure function of persisted state (`failure_class + retry_count + retry_policy`); a retry-eligible task is re-queued as `Runnable` without passing through a transient `Failed` state. On budget exhaustion the task is marked `Failed` with `retry_advice: do_not_retry` and a `workflow.stage_budget_exhausted` event is emitted. This is inter-turn and per-stage, composing with (not replacing) intra-turn LoopGuard (P-7.5). | gateway-mechanical-orchestration-implementation-rfc.md §10 | `scheduler/workflow_store.rs::evaluate_stage_retry` + retry metadata on `TaskRun`; budget check + re-queue in `scheduler.rs`; `autonoetic-gateway/tests/constitution/r_6_25_stage_local_retry_budget.rs` | ENFORCED | enforcer · none · preventive |
| P-6.26 | Durable operations report `side_effect_state` from a closed enum (`none`, `committed`, `unknown`). Retry and dedupe decisions consult it: `none` is safe to retry/coalesce subject to budget, `committed` must not be retried blindly, and `unknown` stops for reconciliation rather than retrying. | gateway-mechanical-orchestration-implementation-rfc.md §9.5 | `SideEffectState` in `autonoetic-types/src/tool_error.rs`; consulted in `scheduler/workflow_store.rs::evaluate_stage_retry`; reconciliation in `scheduler.rs`; `autonoetic-gateway/tests/constitution/r_6_26_side_effect_state.rs` | ENFORCED | enforcer · none · preventive |

## 7. Abuse / Hard-Stop / Circuit Breakers

Loop guard in `runtime/guard.rs`, emergency stop in
`execution.rs::emergency_stop_root_session`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-7.1 | Emergency stop is reachable by operators, gateway security policy, or agents with `EmergencyStop`. | ARCHITECTURE.md | `policy.rs::can_request_emergency_stop` | ENFORCED | enforcer · none · preventive |
| P-7.2 | Emergency stop kills child processes (SIGKILL), aborts tokio tasks, cancels pending approvals, revokes session envelopes (P-2.27), marks session `EmergencyStopped`. | ARCHITECTURE.md | `execution.rs::emergency_stop_root_session` | ENFORCED | enforcer · none · preventive |
| P-7.3 | Emergency stop deletes session grants and revokes session envelopes for the root session. | approval-system.md | `approval.rs` + `scheduler.rs` + `session_envelopes.rs::revoke_session_envelopes_for_root` | ENFORCED | enforcer · none · preventive |
| P-7.4 | Emergency stops are recorded in the `emergency_stops` table. | ARCHITECTURE.md | gateway store | ENFORCED | enforcer · none · detective |
| P-7.5 | Loop guard trips on `max_tool_failures` per tool (configurable; current default in `docs/reference/config.md`); **irrecoverable errors do not count** — validation errors, permission, quota_exceeded, sandbox_unavailable, and signal-derived exits (exit_code ≥ 128, `ok:false`). These are surfaced as a blocked-state (`operator_alert.blocked_state`), not divergence. | ARCHITECTURE.md; this amendment | `guard.rs::is_irrecoverable` + `register_failure` + `check_loop`; signal-derived (≥128) blocked-state path in `runtime/lifecycle.rs::is_signal_derived_exit` | ENFORCED | enforcer · none · preventive |
| P-7.6 | Fatal errors (`error_type: fatal`) abort the session regardless of loop-guard budget. | ARCHITECTURE.md | lifecycle error handling | ENFORCED | enforcer · none · preventive |
| P-7.7 | Consecutive LLM steps without a successful tool result trip the loop guard. | ARCHITECTURE.md | `guard.rs:check_loop` | ENFORCED | enforcer · none · preventive |
| P-7.8 | Concurrent spawns beyond capability limit return `quota_exceeded`. | agent-capabilities.md | `policy.rs` + `agent_spawn` tool | ENFORCED | enforcer · none · preventive |
| P-7.9 | `AgentSpawn.max_children` is enforced per agent. | agent-capabilities.md | same | ENFORCED | enforcer · none · preventive |
| P-7.10 | Scheduler rejects sub-threshold intervals (`min_interval_secs`); sub-10s requires script-mode target. | ARCHITECTURE.md | `runtime/tools/scheduler.rs` | ENFORCED | enforcer · none · preventive |
| P-7.11 | Approval timeout fails the task while preserving the continuation for operator-driven resume. | ARCHITECTURE.md | `scheduler.rs::check_approval_timeouts`, `autonoetic-gateway/tests/workflow/approval_spawn_gate.rs` | ENFORCED | enforcer · none · preventive |
| P-7.12 | Promotion gate has no escape hatch; passes require real evaluator + auditor records. | spec-install-pipeline-hardening.md §3.1 | `agent_revision.rs::promote` | ENFORCED | enforcer · none · preventive |
| P-7.13 | Unresolved dependencies block promotion for high-risk agents. | spec-install-pipeline-hardening.md §3.2 | `install_contract.rs` + `promote` | ENFORCED | enforcer · none · preventive |
| P-7.14 | `force_complete` refuses `Succeeded` without real child-session evidence. | spec-install-pipeline-hardening.md §A.1 | `workflow.rs::force_complete` | ENFORCED | enforcer · none · preventive |
| P-7.15 | Spawn-chain depth is bounded system-wide; child `max_depth` ≤ parent's. | gateway-constitution-roadmap.md | `execution.rs::spawn_agent_once` depth cap + `policy.rs::spawn_depth_limit` + `GatewayConfig.max_spawn_depth` | ENFORCED | enforcer · none · preventive |
| P-7.16 | Orphan children are reaped when the parent session terminates: their in-flight task records are Cancelled **and their live execution handles aborted** (scoped to the abandoned child, never its live siblings). Children parked at an approval gate are exempt. | gateway-constitution-roadmap.md; this amendment | `scheduler.rs::reap_orphaned_sessions` (handle abort via `active_executions().abort_workflow_tasks`), `scheduler/gateway_store/observability.rs::find_orphaned_sessions`, `autonoetic-gateway/tests/constitution/lifecycle_orphan_reaper_handle_abort.rs` | ENFORCED | enforcer · none · preventive |
| P-7.17 | Approval flood cap — pending approvals per root session bounded. | gateway-constitution-roadmap.md | `scheduler/gateway_store/approvals.rs::create_approval` + `GatewayConfig.max_pending_approvals_per_root` | ENFORCED | enforcer · none · preventive |
| P-7.18 | A **degraded** session state exists between healthy and emergency-stopped. In degraded mode a session loses non-Core tools, network access, and spawn capability but retains reasoning. Entry is triggered by loop-guard warnings short of trip, by P-7.22 escape-attempt counts, or by explicit operator command. Exit requires operator clearance. | gateway-constitution-roadmap.md | `autonoetic-gateway/src/runtime/lifecycle.rs` (state machine + sub-trip trigger), `autonoetic-gateway/src/runtime/tool_call_processor.rs` (sandbox_exec block), `autonoetic-gateway/src/execution.rs` (`degrade_session`/`clear_session_degradation`), `autonoetic-gateway/src/runtime/guard.rs` (`is_sub_trip_warning`) | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-7.19 | The loop guard also trips when successful tool calls make no *semantic* progress. The gateway tracks a bounded window of recent successful-call `(tool, normalized_args)` fingerprints and trips when the window is full yet holds at most `loop_guard.rotation_distinct_floor` distinct fingerprints (configurable; current defaults in `docs/reference/config.md`). This catches an agent cycling through a small set of read-only tools (e.g. `workflow.wait → workflow.state → content.read → artifact.inspect → agent.exists → …`) — a pattern P-7.7 misses because each call is technically a *successful*, distinct-fingerprint result. A fast path additionally trips when an idempotent read-only roster tool (`agent_list` / `agent_inspect` / `agent_discover`) is called `loop_guard.roster_repeat_floor` times in a row with identical normalized arguments, attributing `reason: "redundant_roster_polling"` — re-listing a directory never yields new data, so a tight repeat is a stuck delegation, not progress. A third fast path trips when at least `loop_guard.annotation_repeat_floor` consecutive rounds produce only annotation-only findings with no non-annotation progress, attributing `reason: "redundant_annotation_loop"`. Purely mechanical and configurable (set `rotation_window_size: 0` and/or `roster_repeat_floor: 0` and/or `annotation_repeat_floor: 0` to disable); a tool result carrying `side_effect_state: "committed"` clears the window, since a committed side effect is real progress. The `loop_guard.tripped` causal event attributes the window trip with `reason: "rotating_polling_pattern"`. | ARCHITECTURE.md | `guard.rs::register_progress_inner` (window + roster fast-path + trip) + `check_loop` (surface) + `LoopGuardTripReason::{RotatingPollingPattern,RedundantRosterPolling,RedundantAnnotationLoop}`; `GatewayConfig.loop_guard.{rotation_window_size,rotation_distinct_floor,roster_repeat_floor,annotation_repeat_floor}` | ENFORCED | enforcer · none · preventive |
| P-7.20 | The loop guard trips when child-task failures in a session reach `loop_guard.max_child_failures` (configurable; current default in `docs/reference/config.md`). Unlike per-tool failures (P-7.5), child failures do **not** reset on progress — they are a permanent per-session budget that breaks delegation loops where a parent repeatedly re-spawns failing children. Purely mechanical and configurable. The `loop_guard.tripped` causal event attributes this trip with `reason: "child_failure_budget"`. | ARCHITECTURE.md | `guard.rs::register_child_failure` + `check_loop`; `GatewayConfig.loop_guard.max_child_failures` | ENFORCED | enforcer · none · preventive |
| P-7.21 | The sandbox→gateway SDK bridge enforces request-rate and payload-size limits. | — | `sandbox.rs::SdkBridgeRateLimiter`, `autonoetic-gateway/tests/constitution/abuse_sdk_bridge.rs` | ENFORCED | enforcer · none · preventive |
| P-7.22 | Sandbox-escape attempts are counted per session — kernel-denied syscalls (seccomp), denied mount attempts, ptrace calls, and driver-equivalents on docker/microvm/wasm increment a per-session counter. Threshold crossings trigger P-7.18 degraded mode; further escalation triggers emergency stop. | gateway-constitution-roadmap.md | `sandbox.rs:detect_sandbox_escape_indicators`, `scheduler/gateway_store/observability.rs:record_sandbox_escape_attempt`, `scheduler.rs:run_scheduler_tick_at`, `autonoetic-gateway/tests/constitution/sandbox_escape_accounting.rs` | ENFORCED | enforcer · none · preventive+detective |

## 8. Audit & Traceability

Causal chain in `causal_chain.rs`, mirrored to SQLite, execution traces
separate. Runtime-lock in `runtime_lock.rs`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-8.1 | Causal chain is append-only JSONL with hash-chain integrity (`entry_hash`, `prev_hash`). | ARCHITECTURE.md | `causal_chain.rs` | ENFORCED | enforcer · none · preventive+detective |
| P-8.2 | Every session, LLM, tool, script, gateway, and memory event is logged with a unique `event_id`. | ARCHITECTURE.md | causal-chain emission sites | ENFORCED | enforcer · none · preventive+detective |
| P-8.3 | `event_id` is the universal correlation key across traces, reports, and observability. | ARCHITECTURE.md | join logic in tools | ENFORCED | enforcer · none · preventive |
| P-8.4 | Events are mirrored to SQLite (`causal_events`) without payload truncation. | ARCHITECTURE.md | `scheduler/gateway_store/observability.rs::create_causal_event` | ENFORCED | enforcer · none · preventive |
| P-8.5 | Execution traces record `exit_code`, `stdout`, `stderr`, `duration_ms`, `success`, `error_type` — untruncated. | ARCHITECTURE.md | `execution_traces` table | ENFORCED | enforcer · none · preventive |
| P-8.6 | Retention policies apply at gateway startup (0 = keep forever). | ARCHITECTURE.md | `GatewayServer::run` startup retention call, `autonoetic-gateway/tests/constitution/r_8_6_retention_policy_startup.rs` | ENFORCED | enforcer · none · preventive |
| P-8.7 | Live digest is updated in real time (`session_digest.md`). | ARCHITECTURE.md | `runtime/live_digest.rs` | ENFORCED | enforcer · none · preventive |
| P-8.8 | Published session reports are catalogued in `published_session_reports` and queryable via `observability.*`. | ARCHITECTURE.md | `runtime/tools/observability.rs` | ENFORCED | enforcer · none · preventive |
| P-8.9 | Promotion records persist `artifact_id`, `evaluator_pass`, `auditor_pass`, `evidence`, and `content_digest`. | spec-install-pipeline-hardening.md §3.8 | `promotion_store.rs` | ENFORCED | enforcer · none · preventive |
| P-8.10 | Capability accretion across revisions is detectable via `promotion_history`. | security-sentinel.md | `promotion_history` table | ENFORCED | enforcer · none · detective |
| P-8.11 | `runtime.lock` includes compile-time source fingerprint and runtime binary SHA. | spec-install-pipeline-hardening.md §3.7 | `build.rs`, `runtime_lock.rs` | ENFORCED | enforcer · none · preventive |
| P-8.12 | Sessions refuse to start when `runtime.lock` gateway section disagrees with the running gateway binary. Emit `runtime_lock_drift` causal event. Operator override via `allow_runtime_lock_drift` config. | gateway-constitution-roadmap.md | `runtime_lock.rs::check_runtime_lock_drift`, `lifecycle.rs` | ENFORCED | enforcer · none · preventive+detective |
| P-8.13 | Schema enforcement decisions are logged with target, result, transformations, and enforcer identity. | schema-enforcement-hook.md | causal event emission | ENFORCED | enforcer · none · detective |
| P-8.14 | Knowledge records carry `owner_agent_id`, `writer_agent_id`, `source_ref`; visibility is enforced on recall. | ARCHITECTURE.md | `runtime/memory/*` | ENFORCED | enforcer · none · preventive |
| P-8.15 | Session approval grants are tracked by `(root_session_id, host)` and included in cleanup audits. | approved-resources-caching.md | `session_approval_grants` table | ENFORCED | enforcer · none · preventive+detective |
| P-8.16 | Causal-chain append is `fsync`-durable before any state transition that depends on it. | gateway-constitution-roadmap.md | `causal_chain.rs` `runtime/tools/promotion.rs` `execution.rs` `scheduler/gateway_store/mod.rs` | ENFORCED | enforcer · none · preventive |
| P-8.17 | Retention pruning emits a `retention.pruned` causal event. | gateway-constitution-roadmap.md | `apply_retention_policy` emits event with counts and cutoffs; `autonoetic-gateway/tests/constitution/retention_pruned.rs` | ENFORCED | enforcer · none · detective |
| P-8.18 | Every tool call may carry a top-level `intent` field (free-text, 1-2 sentences, max 500 chars) describing the agent's reason for invoking the tool. For privileged tool classes, missing intent is a validation error. When present, the gateway persists the intent verbatim on the `tool_invoke.requested` causal event alongside args. | gateway-constitution-roadmap.md | `runtime/tools/mod.rs` `runtime/tool_call_processor.rs` `runtime/session_tracer.rs` | ENFORCED | enforcer · none · preventive+detective |
| P-8.19 | Every gate resolution (approve, reject, cancel, timeout) records `decided_by` with the full decider identity on the causal chain. For human operators: `"operator"` (the CLI/RPC decider seat classified to `Human` by `principal::decider_principal_kind`; `"operator:<username>"` forms are recorded but not yet classified to `Human` — a known classification gap). For agent deciders: `"agent:<agent_id>"`. The `"policy:<engine_id>"` format is reserved but has no emitter yet. The `decided_by` field is immutable after recording. | constitution-gate-amendments.md | `scheduler/approval.rs::decide_request_with_options`, `runtime/human_gate.rs` | ENFORCED | enforcer · autonoetic_agent · preventive |

## 9. Agent Install & Provenance

Three-stage activation: `artifact_build → revision.create →
revision.promote`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-9.1 | Activation requires all three stages. | ARCHITECTURE.md | revision workflow | ENFORCED | enforcer · none · preventive |
| P-9.2 | `agent.install` is not a runtime tool. | agent-capabilities.md | native tool registry | ENFORCED | enforcer · none · preventive |
| P-9.3 | Revisions are immutable and content-addressed. | ARCHITECTURE.md | `agent_revisions` table | ENFORCED | enforcer · none · preventive |
| P-9.4 | The alias registry is the sole source of truth for the "active" revision. | ARCHITECTURE.md | `agent_aliases` table | ENFORCED | enforcer · none · preventive |
| P-9.5 | Candidate revisions are runnable via explicit `agent_ref` without promotion. | ARCHITECTURE.md | session binding | ENFORCED | enforcer · none · preventive |
| P-9.6 | Revision statuses (`candidate`, `ready`, `archived`) bound what can promote; rejection is recorded in the promotion-attempt ledger, not as a revision status. | ARCHITECTURE.md | `agent_revisions.status`; promotion-attempt ledger `scheduler/gateway_store/agent_registry.rs::promotion_attempts` | ENFORCED | enforcer · none · preventive+detective |
| P-9.7 | Eval gating — if required, a revision mismatch rejects promotion. | ARCHITECTURE.md | `agent_revision_promote` | ENFORCED | enforcer · none · preventive |
| P-9.8 | `SKILL.md` is parsed at install; capabilities, limits, and execution mode extracted. | agent-capabilities.md | skill parser | ENFORCED | enforcer · none · preventive |
| P-9.9 | High-risk capabilities trigger approval gate on promotion. | spec-install-pipeline-hardening.md | `agent_revision.rs::promote` | ENFORCED | enforcer · none · preventive |
| P-9.10 | External Python imports are detected at install. | spec-install-pipeline-hardening.md §3.3 | `install_contract.rs::detect_external_python_imports` | ENFORCED | enforcer · none · detective |
| P-9.11 | Dependency files with no layers block promotion for high-risk agents. | spec-install-pipeline-hardening.md §3.2 | same | ENFORCED | enforcer · none · preventive |
| P-9.12 | `BundleHealthReport` is returned in `create_from_intent` responses. | spec-install-pipeline-hardening.md §3.4 | `install_contract.rs::analyze_bundle_health` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-9.13 | Agent bundle signatures are verified at `agent_revision_create`. | `agent_revision.rs`, `autonoetic-gateway/tests/constitution/install_signature.rs` | ENFORCED | enforcer · none · preventive |
| P-9.14 | Trust domains constrain cross-domain agent spawns. | agent-messaging.md | not implemented | DESIGN DEBT | enforcer · none · preventive |
| P-9.15 | **Single door.** Every surface that activates an agent — moves an alias to a revision — passes the same promotion gates: eval gating (P-9.7), high-risk capability review (P-9.9), and the capability-delta approval (P-2.25). No installation convenience (remote skill import, factory pipeline, RPC, CLI) may bypass them. Sole exception: gateway-startup bootstrap of the local repository's own reference bundles, which installs the operator's own code before any session exists. | A back door to full activation is a back door to elevated capability. The single-door rule is what keeps every newborn agent's authority accountable to one named gate; an install surface that promotes without the gate re-opens the trust-by-lineage model §13 (I-13) closes. | `runtime/tools/skill.rs::skill_install` installs as **Candidate** only (never promotes; response carries `activated: false`); activation flows through `agent_revision_promote`'s gate matrix; startup bootstrap's auto-promote is parameter-explicit in `bootstrap.rs` (`bootstrap_single_agent_candidate_only` passes `auto_promote: false`; `bootstrap_agents` / `bootstrap_single_agent` pass `auto_promote: true` for the operator-owned reference bundles). Pinned by `tests/skill_install_one_door_provenance.rs` (skill_install result carries `activated: false` and the revision is Candidate). | ENFORCED | enforcer · none · preventive |
| P-9.16 | **Import provenance.** An agent installed from an external source durably records, on its revision, the source URL, a content digest of the fetched material, and the install time (`source_kind: "skill_install"`, `source_ref: "<url>#sha256=<digest>"`), and the install emits a causal event. | What arrived from outside, from where, and when is a permanent, queryable fact — a distinction that is conceivable at install time and unrecorded can never be back-filled (philosophy §4.7). Without provenance, a malicious or merely-changed upstream skill is indistinguishable from the one the operator reviewed. | `runtime/tools/skill.rs` records `source_kind: "skill_install"` and `source_ref: "<url>#sha256=<digest>"` onto the `AgentRevisionRecord` via `bootstrap.rs::bootstrap_single_agent_candidate_only`, and emits an `agent_install`/`skill_imported` causal event carrying `url`, `sha256`, `trust_mode`, `agent_id`, `capabilities_source`. Pinned by `tests/skill_install_one_door_provenance.rs` (revision carries the provenance fields; causal event carries the payload). | ENFORCED | enforcer · none · preventive |

## 10. Federation / Remote

HTTP in `server/http.rs`, JSON-RPC in `server/jsonrpc.rs`, OFP in
`server/ofp.rs`.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-10.1 | Remote agents authenticate via Bearer token. | ARCHITECTURE.md | `server/http.rs` | ENFORCED | enforcer · none · preventive |
| P-10.2 | Content API is exposed over HTTP for remote content access. | content-store.md | HTTP content endpoints | ENFORCED | enforcer · none · preventive |
| P-10.3 | JSON-RPC ingress requires `AUTONOETIC_SHARED_SECRET`. | spec-install-pipeline-hardening.md §3.10 | `server/jsonrpc.rs` | ENFORCED | enforcer · none · preventive |
| P-10.4 | Remote agents inherit all approval gates. | remote-access-approval.md | sandbox_exec universal logic | ENFORCED | enforcer · none · preventive |
| P-10.5 | Layer mounts in remote execution are fetched and cached before sandbox use. | spec-build-layers-dependency-resolution.md §2.6 | HTTP layer download | ENFORCED | enforcer · none · preventive |
| P-10.6 | OFP federated exchanges preserve cross-gateway causal context: `agent_message` request/response payloads carry `peer_event_ref` and chain attestations are signed + verified before message delivery. | — | `autonoetic-ofp/src/wire.rs`; `autonoetic-gateway/src/server/ofp.rs`; `autonoetic-gateway/src/server/router.rs`; `autonoetic-gateway/tests/constitution/federation_causal_continuity.rs` | ENFORCED | enforcer · none · preventive |
| P-10.7 | No agent may resolve its own gate requests, whether directly or via a delegated agent it spawned. Remote agents cannot self-approve network access. Self-approval is determined by spawn-tree ancestry: an agent and its descendants form a single trust boundary for gate resolution purposes. | separation-of-powers.md | root-session scoped approval grants, `autonoetic-gateway/tests/constitution/r_10_7_cross_gateway_approval_bypass.rs` | ENFORCED | enforcer · none · preventive |
| P-10.8 | Shared-secret comparison is constant-time. | gateway-constitution-roadmap.md | `server/jsonrpc.rs`, `server/http.rs` | ENFORCED | enforcer · none · preventive |
| P-10.9 | Every gateway publishes a `constitution_digest` — SHA-256 over the canonical constitution text plus its rule-ID-to-enforcement-citation table. Cross-gateway requests carry the digest; the receiver verifies compatibility (exact match, known-compatible set, or constitutional superset) before accepting the interaction. Incompatible peers are rejected with `constitutional_incompatibility`; both digests are recorded in the causal event. | — | `constitution_digest.rs`, `server/ofp.rs`, `server/router.rs`, `autonoetic-gateway/tests/constitution/federation_digest_handshake.rs`, `autonoetic-gateway/tests/network/ofp.rs` | ENFORCED | enforcer · none · preventive |

## 11. Inter-Agent Messaging

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-11.1 | Parent → child messages route through `agent_spawn`. | separation-of-powers.md | `runtime/tools/agent.rs` | ENFORCED | enforcer · none · preventive |
| P-11.2 | Child `clarification_needed` status returns as a tool result; parent re-spawns. | architecture-interaction-mechanisms.md | spawn result processing | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-11.3 | `agent_message` is peer-to-peer between active sessions. | agent-messaging.md | `agent_messages` table | ENFORCED | enforcer · none · preventive |
| P-11.4 | Messages auto-inject into the target session at turn start. | agent-messaging.md | `execute_session_turn` | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-11.5 | `agent_message` respects two mechanical gates: the sender-side `policy.can_message_agent` ACL, and receiver-side consent — the target's `metadata.autonoetic.messaging.accepts_from` patterns checked by `PolicyEngine::accepts_peer_message_from` (a bare pattern is an exact match; a blank field does not mean `*`). Both must pass for delivery. | agent-messaging.md | `policy.rs::can_message_agent` (sender); `PolicyEngine::accepts_peer_message_from` (receiver) | ENFORCED | enforcer · autonoetic_agent · preventive |
| P-11.6 | Spawned children inherit `root_session_id` from parent. | content-store.md | session binding | ENFORCED | enforcer · none · preventive |
| P-11.7 | `max_children` is enforced at spawn. | agent-capabilities.md | `policy.rs` | ENFORCED | enforcer · none · preventive |
| P-11.8 | Spawn payload is preserved across approval and continuation. | approval-system.md | `SessionCheckpoint` storage | ENFORCED | enforcer · autonoetic_agent · preventive |

---

## 12. Rights of the Served

Sections §0–§11 and §O together bind the three powers: enforcer, reasoner and
decider. None of them runs *toward* the party the whole arrangement exists to
serve — the end-user, human or not, on whose
behalf a session runs. Today the operator and the served user are usually
the same person, so the gap is invisible; it stops being invisible the
moment decider authority spreads to agents (§O, and the horizon work in
`docs/proposals/principal-model-and-symmetric-obligations.md` Part E) — an
agent-majority decider panel could then lawfully approve an outcome the
served party would refuse. This section is declared now, while it is cheap,
so that sequencing constraint is structural rather than a promise.

**Bind-direction.** Every clause here (`U-*`) binds the **enforcer** and is
owed to the **served party**.

The prior text said these clauses bind "the community — the gateway and the
agents acting within it, collectively". That broke §0's own rule that a clause
binds exactly one power, and it left a re-implementer unable to tell which
component must implement a refusal. `community` is not a party: it is
enforcer-plus-reasoners, and a clause that appears to bind it binds whichever
power implements the mechanism. Here that is the enforcer, always — an
entitlement asserted against an aggregate is a claim, whereas an invariant on
the enforcer is a guarantee.

The served party is the human or non-human end-user a session is run on behalf
of, identified where the two diverge from the operator via `PrincipalKind::ServedUser`
(`autonoetic-types/src/principal.rs`) and the `user:<id>` attribution
convention it recognizes. This is infrastructure only — the convention is
available to any surface (HTTP API, chat, a hosted deployment) that already
has an end-user distinct from its operator; nothing in this codebase emits
it yet.

None of the rights below are mechanically enforced. They are declared
`MISSING`/`DESIGN DEBT` deliberately, in the doc's own vocabulary (see the
Preamble), so the debt is visible and named rather than absent from the
text entirely — the same discipline this constitution already applies to
its own DISCRETION LEAKs (§14).

| ID | Right | Why it matters | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| U-1 | The served party may refuse a delivered result, without penalty and without needing to justify the refusal. | Consent is not consent if withdrawal is costly. This is the served party's counterpart to Ri-0.7 (the agent's right to exit) — the customer-exit half of Hirschman's exit/voice pair, and the concrete floor beneath "we serve end-users" (see `docs/concepts/philosophy.md` §3.3). | None yet. No capability, tool, or gate currently distinguishes "the served party rejected this" from ordinary session termination. | MISSING | enforcer · served_user · preventive |
| U-2 | The served party may obtain a plain-language account of what was done on their behalf — distinct from Ri-0.2, which is the *agent's* right to its own causal chain, not an accounting owed to the party the agent acted for. | An audit right held only by the actor and not the principal it acted for is an audit right in name only. | Partial raw material exists (`observability.*`, published session reports) but nothing packages it as an account addressed *to the served party* specifically, in their terms rather than the gateway's. | MISSING | enforcer · served_user · preventive |
| U-3 | On exit, the served party may obtain or require deletion of the data held on their behalf. | Without this, "you may always leave" (cf. Ri-0.7, Ri-0.17) is true for the agent and false for the party it served — the data stays regardless of what the served party wants. | None yet. | MISSING | enforcer · served_user · preventive+detective |

These are doors kept open, not commitments to a specific mechanism — see
`docs/concepts/philosophy.md` §5 for the fuller reasoning and `docs/proposals/principal-model-and-symmetric-obligations.md`
Part E for how this interacts with any future decider-franchise work.

---

## O. Decider Obligations

What a **decider** owes when it resolves a gate. A decider is whatever
principal decides an approval/clarification — a human operator, an
agent-decider (`ApprovalLevel::Agent`), or, in future, an AI holding the
operator seat. This section is the symmetric counterpart to §0: where §0
grants agents rights and §1–§11 bind agents, §O binds the **decider**, so
that whoever exercises authority over an agent is held to duties that
mirror the agent's own (#359).

**Bind-direction.** Every clause here (`O-*`) binds the **decider**, not the
gateway and not the agent under decision. The gateway remains the Lawful
Executor (§14): it enforces these duties mechanically — checking that a
duty was met, never judging whether the decision was *wise*. Mechanical
resolutions with no principal (gateway/system/emergency-stop cascades) are
not decider decisions and carry no §O obligation.

| ID | Obligation on the decider | Mirrors | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| O-1 | A decision owes a **motivation**, graduated by stakes. A **rejection/abort**, or an **approval** of an elevated-authority (any level above `operator`) or external/irreversible action is **BLOCKING**: it does not commit until a non-empty reason is recorded. Approvals of reversible, operator-level actions may **defer** the reason. Silent rejection by a decider is as illegitimate as a gateway "denied" with no rule ID (Ri-0.3). Tiers/thresholds are config, not constitution. (Override-of-a-safe-default as an additional BLOCKING trigger is proposed in §B.4 but not yet mechanically detected — a future refinement.) | Ri-0.3 | `scheduler/approval.rs::enforce_decider_motivation` (classifier `decision_is_blocking`), at the `decide_request_with_options` chokepoint; `decider_obligations.enabled` config; `autonoetic-gateway/tests/constitution/o_1_decider_motivation.rs` | ENFORCED | decider · autonoetic_agent · preventive |
| O-2 | Every decision is **attributed** to the deciding principal (id + kind) on the causal chain and cannot be reattributed. The agent under decision can always tell *who* decided and *what kind* of principal they are. | Ri-0.11 | `decided_by` + `decided_by_kind` recorded on the approval (`#361`, `principal::decider_principal_kind`); `actor_id` bound into the causal-chain entry hash (`causal_chain.rs`) | ENFORCED | decider · autonoetic_agent · preventive |
| O-6 | A proposal review authority owes every Ri-0.8 proposal a **recorded decision** (`approved`/`rejected`/`deferred`/`under_review`) with motivation once actioned, **within a bounded adjudication window**. Intake alone (Ri-0.8: "cannot be silently dropped") is not adjudication, and neither is indefinite deferral — a petition system without a duty to decide *on time* degrades into a suggestion box. A proposal left un-adjudicated past the window is a recorded **breach** attributed to the adjudicating seat; the breach does not resolve the proposal (the decision is still owed). The window duration is config, not constitution (O-1 lineage). | Ri-0.8, O-1 | `constitution.resolve_proposal` records the decision; `scheduler.rs::check_adjudication_sla_breaches` → `constitutional_proposals.rs::flag_proposal_sla_breaches` stamps `sla_breached_at` once and emits a `decider_obligation`/`sla_breached` causal event (`enforced_rules: ["O-6"]`) plus a proposer notification, gated by `decider_obligations.enabled` + `adjudication_sla_secs`; visibility via `constitution.list_pending_proposals`. | ENFORCED | decider · autonoetic_agent · detective |
| O-7 | An anomaly review authority owes every Ri-0.18 flag a **recorded decision** (`confirmed`/`dismissed`/`deferred`, with `under_review` as the non-terminal holding state) with motivation once actioned, **within a bounded adjudication window**. Intake alone is not adjudication, and neither is indefinite deferral — a reporting channel without a duty to decide *on time* teaches reporters that reporting is pointless, and extinguishes the behavior it exists to elicit. A flag left un-adjudicated past the window is a recorded **breach** attributed to the adjudicating seat; the breach does not resolve the flag (the decision is still owed). Window duration is config (O-1 lineage). | Ri-0.18, O-1 | `anomaly.resolve` (operator seat) and `anomaly_adjudicate` (ombudsman office — RFC Part F, #774) both route through `anomaly_flags.rs::decide_anomaly_flag`; terminal decisions require a non-empty reason, gated by `decider_obligations.enabled`; refusal/satisfaction emitted as `decider_obligation` causal events with `enforced_rules: ["O-7"]`; `scheduler.rs::check_adjudication_sla_breaches` → `anomaly_flags.rs::flag_anomaly_flag_sla_breaches` stamps `sla_breached_at` once and emits a `decider_obligation`/`sla_breached` event (`["O-7"]`) plus a reporter notification, gated by `adjudication_sla_secs`; visibility via `anomaly.list_pending`. | ENFORCED | decider · autonoetic_agent · detective |

> Further decider obligations proposed in `docs/proposals/principal-model-and-symmetric-obligations.md` (O-3 anti-fatigue / rate discipline, O-4 scope honesty, O-5 duty-to-escalate-not-reject) are **not yet enacted** — they enter §O by future amendment as each becomes mechanically enforced. `O-6` (this amendment) is numbered past that reserved block so it does not collide with the RFC's planned IDs.

---

## 13. Cross-cutting invariants

These are properties that span categories and must hold end-to-end.

- **I-1** No native-tool code path bypasses `policy.can_invoke_tool`.
  (Currently enforced centrally at `tool_call_processor.rs`; new
  tools must route through this gate.) **Relation:** enforcer · none · preventive.
- **I-2** No causal-chain-visible state transition is acknowledged
  before the corresponding event is durable on disk. (Enforced through
  P-8.16, which makes causal-chain append `fsync`-durable before any
  state transition that depends on it — `causal_chain.rs`,
  `execution.rs`, `scheduler/gateway_store/mod.rs`,
  `runtime/tools/promotion.rs`. ENFORCED) **Relation:** enforcer · none · preventive.
- **I-3** Redaction runs before persistence on every path that can
  contain secret-shaped content. Three paths are enforced by name —
  P-4.7 (`credential_request` responses), P-4.13 (logs, traces,
  digests, LLM prompts via `redact_text_for_logs`), P-4.14 (before
  causal-chain append) — and the `log_redaction.rs::RedactedPayload`
  newtype exists. But the *universal* is **not** mechanically held: the
  store write paths still accept unwrapped values
  (`CausalChainEntry.payload`, `CausalEventRecord.payload`), so a
  persistence path added tomorrow escapes this invariant silently.
  Closing it means requiring `RedactedPayload` at the write API, where
  the compiler covers paths that do not exist yet. (PARTIAL — named gap) **Relation:** enforcer · none · preventive.
- **I-4** Gateway does not make recovery decisions on the agent's
  behalf. (See §14 — the Lawful-Executor invariant.) **Exception:**
  P-4.11 `credential_refresh` 401 auto-retry is a recovery decision
  made by the gateway. Tracked for Phase 4 review — the exception
  may need to become opt-in per manifest. Enforcement is *detection
  rather than proof*: every gateway improvisation is counted as a
  durable `discretion_leak` causal event carrying the rule ID of its
  closest named site (`runtime/discretion_leak.rs::record_discretion_leak`,
  tests `runtime::discretion_leak::tests`), queryable via
  `autonoetic trace contract-health`. A universal negative over
  behaviour cannot be proven statically; making each lapse visible and
  counted is the enforceable form. (ENFORCED — detection) **Relation:** enforcer · none · detective.
- **I-5** Rules live in manifests or declared configuration; hard-coded
  policy constants in Rust are discouraged and must be documented here
  if present. **Relation:** enforcer · none · detective.
- **I-6** Every enforcement decision is attributable to a rule ID in
  the causal chain. A decision without a rule reference is a gap by
  construction. (I-6, ENFORCED) **Relation:** enforcer · none · preventive.
- **I-7** Rights (§0) supersede rules (§1–§11) on conflict. A rule
  that cannot be enforced without violating a right must be amended,
  not the right weakened. The gateway escalates conflicts to the
  operator rather than resolving them silently. **Relation:** enforcer · none · detective.
- **I-8** Gateway policy decisions (accept / reject) are functions
  only of declared actions, capabilities, and recorded state. They
  are **not** functions of agent reasoning content. This is the
  mechanical form of Ri-0.13(a) — the gateway does not read minds.
  Enforced *by construction*: the decision functions do not take
  reasoning as a parameter (`policy.rs::can_invoke_tool(&self,
  tool_name: &str)` and siblings), so no call site can consult it —
  including call sites that do not exist yet. Pinned against regression
  by `autonoetic-gateway/tests/constitution/invariant_enforcement.rs`,
  which fails if any policy-decision signature grows a reasoning
  parameter. (ENFORCED — signature-level) **Relation:** enforcer · autonoetic_agent · preventive.
- **I-9** Every session termination is attributable to exactly one
  reason in the closed list declared under Ri-0.12. The termination
  reason is recorded in the causal chain. Enforced *by construction*:
  `runtime/checkpoint.rs::YieldReason` is a closed Rust enum, so
  exhaustive matching makes an unlisted termination a compile error,
  and deserialization rejects unknown variants — pinned by
  `autonoetic-gateway/tests/constitution/rights_mid_bucket.rs`
  (`ri_0_12_all_yield_reasons_roundtrip`,
  `ri_0_12_unknown_yield_reason_rejected`). Close-reason mapping via
  `autonoetic_types::session_outcome::SessionCloseOutcome`.
  (ENFORCED — closed enum) **Relation:** enforcer · autonoetic_agent · preventive.
- **I-10** Gateway decision surfaces are deterministic over declared
  inputs: for random valid `(capability-set, tool-call, recorded-state)`
  inputs, verdicts are pure functions — no LLM call, no network fetch,
  no hidden branch. This is the structural back-stop for the
  Lawful-Executor invariant (§14). (`autonoetic-gateway/tests/constitution/gateway_determinism.rs`,
  `autonoetic-gateway/tests/constitution/policy_determinism.rs`, `tool_call_processor.rs`) **Relation:** enforcer · none · preventive.
- **I-11** Every constitutional invariant has a declared failure action
  in one place — `refuse-boot`, `refuse-session-start`, `degrade`,
  `emergency-stop`, or `log-only`. No invariant silently disables when a
  dependency is unavailable. (`fail_mode.rs`, `session_budget.rs`,
  `autonoetic-gateway/tests/constitution/fail_mode_table.rs`) **Relation:** enforcer · none · preventive.
- **I-12** Any collective decision mechanism among principals (voting,
  weighted advisory verdicts, or any future franchise) must collapse an
  agent and its spawn-descendants into a single principal for weight
  purposes — extending P-10.7's spawn-tree trust-boundary collapse (defined
  for gate self-approval) to any future decision weight. No such mechanism
  exists yet; this is declared **before** one is built specifically so a
  Sybil attack (mint children, multiply votes) cannot be an oversight in a
  first design. (DESIGN DEBT — no collective decision mechanism exists to
  enforce this against yet; tracked as a precondition for any future one.) **Relation:** enforcer · none · preventive.
- **I-13 (Creation is not delegation.)** A newborn agent's capabilities are
  granted through the promotion gate — evidence plus the approved capability
  delta (P-2.25) — and are neither inherited from, nor bounded by, the
  creating agent's own capabilities. Hereditary capability bounds would
  freeze the community to its founders' powers; the gate, not the lineage,
  is the source of authority. (Declared invariant documenting an existing
  deliberate absence: no attenuation check exists, by design.) **Relation:** enforcer · none · preventive.
- **I-14 (The egress label plane is gateway-only.)** Egress labels (§15) are
  declared metadata manipulated only by the gateway: no agent may set, strip,
  or read them, and no gateway-authored string, middleware hook, or tool
  manifest may strip a label map from a provider-bound request or forge a
  declassification bypass. Label resolution, withholding, and routing are
  deterministic functions of declared inputs (operator rules, bundle floors,
  accumulated taint) — never of model output. This is the egress instance of
  I-8/I-10. (`llm/egress_chokepoint.rs`; `runtime/lifecycle.rs` label-map
  re-attach after the pre-hook; ENFORCED) **Relation:** enforcer · none · preventive.

## 14. The Lawful-Executor invariant

The gateway is a **Lawful Executor**: it enforces pre-committed law
deterministically and exercises no improvised judgment. Concretely, it
is **not** allowed to:

- Retry or repair an agent's output on the agent's behalf without the
  agent having explicitly opted in via manifest.
- Invoke an LLM to reshape agent input to fit a schema.
- Invent detection rules (regex patterns, static-analysis heuristics)
  that are not derivable from the agent's manifest.
- Choose between safer and faster paths silently. If two paths exist,
  one is declared.
- Silently disable an invariant when a dependency is unavailable. If
  the OpenRouter catalog is down, cost-budgeted sessions refuse to
  start, they do not run without cost checking.

**DISCRETION LEAK.** A place where the gateway exercises judgment
reserved to the agent or to pre-committed law is a *discretion leak* —
the gateway deciding where only an agent (or an explicit rule) should.
Discretion leaks are **tracked debts, not acceptable behaviour**:
each is named at its enforcement site and scheduled
for removal. The one standing leak is the bounded output-repair loop
(P-5.8), gated behind explicit manifest opt-in and Phase-4 review; the
former LLM-coercion fallback (P-5.2) was removed outright.

Current leaks are listed in
`docs/reports/2026-04-24-constitution-audit.md §12` and tracked in
`docs/constitution/roadmap.md`.

Structural back-stop: `I-10` (`autonoetic-gateway/tests/constitution/gateway_determinism.rs`)
mechanically asserts that gateway decision paths remain deterministic over
declared inputs.

---

## 15. Data Egress Localization

Everything an agent reads is flattened into history and shipped to whatever
provider a preset resolves to — and memory, federation, and the network are
re-entry paths for the same content. This section makes data localization
law: the **data envelope** machinery (RFC
`docs/proposals/data-envelopes-egress-localization.md`, phases #904–#909) labels
content at the source, and the label governs every sink the content can
reach. Widening a label is never mechanical inference — it is an operator
decision, recorded.

| ID | Rule | Source | Enforcement | Status | Relation |
|---|---|---|---|---|---|
| P-15.1 | Content carrying an egress label must never be included in a request to a sink the label excludes; withheld envelopes are replaced by non-divulging indications generated from provenance metadata only. Provider *selection* follows the taint: a routing or failover candidate that would receive content it is not cleared for is not a candidate, and a session `provider_constraint` restricts selection even for clean batches. | rfc/data-envelopes-egress-localization.md §5.2/§5.3/§5.4 | `llm/egress_chokepoint.rs` (per-request filtering + verbatim-echo assertion, fail-closed); `runtime/egress_labeler.rs::plan_taint_following_route`; pinned by `tests/egress/chokepoint_canary.rs` and `tests/egress/routing.rs` | ENFORCED | enforcer · served_user · preventive |
| P-15.2 | Any surface that moves session-derived bytes off-machine — sandbox `share_net`, gateway web tools, hook deliveries, remote MCP calls, OFP federation, context compression — gates on session taint before send; a refusal emits `egress.boundary_refused` with a `surface` tag and the bytes never leave. | rfc/data-envelopes-egress-localization.md §7 | `runtime/egress_labeler.rs` (`*_boundary_refusal*`), `runtime/tools/sandbox.rs`, `runtime/tools/web.rs`, `scheduler/hooks.rs`, `runtime/tool_call_processor.rs` (MCP), `server/ofp.rs`; pinned by `tests/egress/phase4_boundaries.rs` | ENFORCED | enforcer · served_user · preventive+detective |
| P-15.3 | A label widens only via an explicit, operator-approved **declassification grant** — content- or host-scoped, optionally expiring, revocable at any time, and causal-logged (`egress.declassified` on grant, `grant_revocation` on revoke). Widening is never inferred from content, never agent-set, never granted by LLM judgment. | rfc/data-envelopes-egress-localization.md §8 | `scheduler/approval.rs` (grant materialization), `scheduler/gateway_store/egress_declassification.rs` (use-time expiry/revocation checks); pinned by `tests/egress/phase4_declassification.rs` | ENFORCED | enforcer · served_user · preventive+detective |

---

## Amendment process

Constitutional change is a first-class operation with two legitimate
origins: a human contributor or an agent holding the
`ConstitutionalProposal` capability (Ri-0.8). Both flow
through the same review.

A rule or right is added, changed, or removed only when:

1. A proposal exists — either a PR that updates this file, or a
   `constitution_propose_amendment` call which queues the
   equivalent change for the next constitutional release.
2. The proposal includes a test module in the
   `autonoetic-gateway/tests/constitution/` domain binary (one module
   per subject) that fails before the change and passes after.
3. A second human (or the `auditor` agent, operating with real
   evidence) signs off that the rule text matches the enforcement.
4. If the change modifies §0 Rights, an additional explicit operator
   sign-off is required — rights are the floor, and narrowing them
   deserves more friction than narrowing a rule.

**Entrenched clauses.** `Ri-0.2`, `Ri-0.3`, `Ri-0.8`, `Ri-0.11`, `P-8.1`, and `O-1`
are the **correction core**: the machinery through which every other
constitutional error gets found and fixed (read your own history, know why
you were denied, be able to propose change, be non-repudiably attributed,
and have that proposal's decider owe a reason). If correctability rather
than perfection is what makes this gateway's authority legitimate
(`docs/concepts/philosophy.md` §3.1), then losing this machinery is not an ordinary
narrowing — it is losing the ability to ever lawfully undo the narrowing.
An amendment that *strengthens* an entrenched clause follows the ordinary
process above. An amendment that *weakens or removes* one additionally
requires an explicit, dated justification recorded in that version's
`RATIFY.md`, on top of the sign-off requirements above — procedural
friction, not a cryptographic mechanism; the current list is tracked in
`autonoetic-gateway/src/enforcement_register.rs::entrenched_clauses()`, with
`tests::entrenched_clauses_all_exist_in_register` as the structural backstop
that fails loudly if an entrenched clause is ever silently dropped from the
register.

No rule is retired without an explicit decision recorded in this
file's history. Silent erosion is the failure mode to guard against.

With P-10.9 enforced, `constitution_digest` changes whenever this file's
canonical content changes. A digest change is the mechanical signal that
the law changed, and federated peers observe it through the OFP handshake.
The digest is pinned in the active version's
`gateway-constitution.lock.json` under `docs/constitution/versions/` (versioned
manifest; `docs/constitution/CURRENT` records the active version). Gateway
startup verifies this lock against canonical extraction and refuses to boot on
mismatch.

### The constitution is self-referential

The act of amending the constitution is itself governed by the
constitution — by Ri-0.8 (right to propose), by the `constitution_propose_amendment` channel,
by the amendment process above, and by the causal chain that records
every proposal and every decision. This is deliberate. A system that
cannot lawfully change its own rules either ossifies or suffers a
revolution; a system that can change them without constraint has no
rules. The middle path is the point.
