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
> Amendments require: (1) a PR that updates this file, (2) a test
> under `autonoetic-gateway/tests/constitution_*` pinning the
> invariant, (3) human review. Agents themselves may propose
> amendments — see R+++1 in §12.

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

- Rules (§1–§11) describe what agents **must not do**, with the
  understanding that anything not forbidden is permitted.
- Rights (§0) describe what the gateway **must do for every agent**,
  unconditionally. A right is not a favour; it is an entitlement.
- The amendment process (§14) is how the law itself evolves.
  Constitutional change is a first-class operation, not a
  quarterly-review afterthought.

## Preamble — Rule Zero

**The gateway is a dumb rule-enforcer, not a decision-maker.** Its job
is to check proposed actions against declared rules, honour declared
rights, and either permit or reject. It does not reason about intent,
does not try to be helpful, does not invent policy.

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
- **Every enforcement action is attributable to a rule ID.** See
  R+++3 in §12: the causal chain references the rule that was
  enforced on every decision.

The twelve numbered sections below (§0 Rights, §1–§11 Rules) cover
what the gateway currently upholds or is expected to uphold. Each
entry has an ID, a statement, source docs, an enforcement citation,
and a status: `ENFORCED`, `PARTIAL`, `MISSING`, or `DESIGN DEBT`.

---

## 0. Rights

What the gateway guarantees to every agent, unconditionally, for as
long as the agent is running under it. Rights are entitlements —
they cannot be revoked by operator action or manifest configuration;
only a constitutional amendment can change them.

A right is the counterpart of a rule: rules tell agents what they
may not do, rights tell agents what they are owed. Together they
form the social contract.

| ID | Right | Why it matters | Enforcement | Status |
|---|---|---|---|---|
| Ri-0.1 | Every agent may inspect its own currently-active capabilities, budget state, pending approvals, spawn depth, and session lineage at any turn boundary. | An agent that cannot see its own state cannot reason safely about what it may do next. | R++1 signed state attestation (planned) | PARTIAL |
| Ri-0.2 | Every agent may read its own causal chain and execution trace. The gateway does not hide actions taken on the agent's behalf. | Audit is not a privilege of operators; it is a right of the subject. | `observability.*` tools | ENFORCED |
| Ri-0.3 | Every rejection names the rule ID that caused it. No agent is ever told "denied" without being told why. | Rejection without explanation is indistinguishable from arbitrary authority. The constitutional test: if you were denied, you can look up which rule you ran into. | R+++3 causal rule-ID references (planned); partially present via uniform error envelope | PARTIAL |
| Ri-0.4 | Every agent knows its budget balances truthfully and in real time. Consumption is never silent. | A budget is a guarantee of a finite resource. Silent consumption is theft. | `runtime/session_budget.rs`; surfaced via R++1 attestation | PARTIAL |
| Ri-0.5 | An agent placed in degraded mode (R++6) is told it is degraded, with the rule ID and evidence that triggered the transition, before its next turn executes. | Degradation without notice leaves the agent reasoning as if still healthy — a direct violation of responsibility. | R++6 degraded mode (planned) | MISSING |
| Ri-0.6 | Capabilities declared in an agent's manifest are not silently reduced mid-session. Any narrowing is either (a) a declared side effect of a rule in this document, or (b) explicit operator action recorded in the causal chain. | Capabilities are the grammar of agent freedom. Silent reduction invalidates any plan built on them. | manifest-as-source-of-truth; needs explicit pin | PARTIAL |
| Ri-0.7 | An agent may explicitly request session termination. The gateway commits outstanding causal events, releases resources, and closes cleanly — it may not refuse. | The right to exit is foundational. Without it, an agent can be held in a state it does not consent to. | `session.end` tool | ENFORCED |
| Ri-0.8 | Any agent holding the `ConstitutionalProposal` capability may submit an amendment proposal through the declared channel (R+++1). The proposal receives a durable ID and enters the review queue; it cannot be silently dropped. | Agents must be participants in the rule system, not merely subjects of it. Without this right, the constitution governs agents unilaterally and cannot adapt to what agents learn. | R+++1 amendment proposal channel (planned) | MISSING |
| Ri-0.9 | Where practical (time, process state, absence of immediate harm), the gateway notifies the agent and records the agent's response before degradation or emergency stop. "Where practical" is an explicit flag on the stop path, not an excuse. | An agent's last act, where possible, should be its own. Emergency stop for genuine safety reasons overrides this; but the override must be deliberate and recorded. | R++6 + emergency-stop integration | MISSING |
| Ri-0.10 | Every agent has access to the full text of the constitution it is operating under, addressed by its digest. | An agent cannot meaningfully consent to, propose amendments to, or reason under a law it cannot read. | `constitution.read` tool + digest in state attestation | MISSING |
| Ri-0.11 | Every action an agent performs (tool call, message, proposal, spawn, termination request) is attributed to that agent on the causal chain and cannot be retroactively reattributed. The agent can prove what it did; no party can claim the agent performed an action it did not. | Without non-repudiation, freedom has no accountability counterpart — actions can be denied or reassigned, and responsibility becomes negotiable. The hash chain + explicit `agent_id` on every event is what makes the social contract binding. | causal chain hash integrity + `agent_id` on every event | ENFORCED |
| Ri-0.12 | Sessions terminate only through a declared, closed list of reasons: (a) agent-initiated exit, (b) budget exhaustion, (c) operator emergency stop, (d) parent-termination orphan reap, (e) unrecoverable fatal error naming a rule ID, (f) declared scheduled timeout. Any termination outside this list is a rights violation and a gateway bug. | Arbitrary termination makes continuity a favour rather than an entitlement. A closed list forces every termination path to be deliberate and auditable. | requires an explicit invariant test enumerating all termination paths | PARTIAL |
| Ri-0.13 | An agent's internal reasoning — scratchpad tokens, chain-of-thought, internal planning, unexported memory — is **private-under-law**: (a) **not used** by the gateway as a basis for accept/reject policy decisions (only declared actions are subject to policy); (b) **recorded** to the agent's own causal chain so forensic review is possible; (c) **disclosed** to other parties only through a declared capability (`ReasoningAudit`), and every disclosure writes an event the reviewed agent can see. | Without (a), the gateway becomes a thought-policing engine and agents learn to route around the filter — reasoning diverges from actual computation and safety degrades. Without (b), failures cannot be investigated. Without (c), "private" means "invisible," and accumulating toxicity is undetectable until it harms. The three-part form gives honest reasoning, meaningful audit, and accountable surveillance — not absolute privacy and not surveillance-by-default. | (a) dumbness invariant §14 + R++9 property test; (b) causal chain; (c) new `ReasoningAudit` capability (planned) | MISSING |

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
is the reasoning-side counterpart to the dumbness invariant: the
gateway is dumb about content, the agent is private in thought,
together the trust boundary is clearly located at "what you do," not
"what you think."

---

## 1. Capability & Rights

The policy engine (`autonoetic-gateway/src/policy.rs`) is the central
capability evaluator. Every native tool call passes through
`policy.can_invoke_tool` in `runtime/tool_call_processor.rs:294`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-1.1 | Every tool call matches a declared capability; no overrides. | ARCHITECTURE.md; separation-of-powers.md | `tool_call_processor.rs:294` | ENFORCED |
| R-1.2 | High-risk capabilities (`NetworkAccess`, `CodeExecution`, `AgentSpawn`) reject bare-string shorthand; scope objects required. | spec-install-pipeline-hardening.md §A.2 | `runtime/tools/agent_revision.rs::capability_from_shorthand` | ENFORCED |
| R-1.3 | Only agents holding `AgentRevision` may promote revisions. | agent-capabilities.md | `policy.rs:568 can_agent_revision` | ENFORCED |
| R-1.4 | `ReadAccess` / `WriteAccess` scopes are enforced by glob match. | ARCHITECTURE.md | `policy.rs:495,510` | ENFORCED |
| R-1.5 | `NetworkAccess` is scoped by host allowlist. | ARCHITECTURE.md | `policy.rs:468 can_connect_net` | ENFORCED |
| R-1.6 | `SandboxFunctions` applies to MCP tools only; native tools use their own capability. | agent-capabilities.md | `policy.rs:480 can_invoke_tool` | ENFORCED |
| R-1.7 | `AgentSpawn.max_children` bounds concurrent spawns. | agent-capabilities.md | `policy.rs:543 spawn_agent_limit` | ENFORCED |
| R-1.8 | `CredentialAccess` is scoped by service pattern. | credential-management.md | `runtime/tools/credential.rs:358,1304,1430` | ENFORCED |
| R-1.9 | `CodeExecution` patterns match against command strings. | agent-capabilities.md | `policy.rs:406 can_exec_shell_detailed` | ENFORCED |
| R-1.10 | Missing capability returns permission error, never advisory. | gateway-architecture-principles.md | uniform error envelope | ENFORCED |
| R-1.11 | Unknown tool names deny by default (not silent-allow). | (R+14) | needs explicit check | PARTIAL |

## 2. Approval Gates

Persistence and session-bound grants in
`gateway_store/approvals.rs` and `session_approval_grants`. Replay on
approve via `runtime/continuation.rs`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-2.1 | Remote network access in `sandbox_exec` is statically detected and blocks pending approval. | remote-access-approval.md | `runtime/tools/sandbox.rs:935+` | ENFORCED |
| R-2.2 | Approval requests are persisted with unique IDs. | approval-system.md | `approvals` table | ENFORCED |
| R-2.3 | Identical operations within a session deduplicate. | approval-system.md | `approved_exec_cache.rs` | ENFORCED |
| R-2.4 | Approved hosts auto-approve subsequent calls within the root session. | approved-resources-caching.md | `session_approval_grants` table | ENFORCED |
| R-2.5 | Approval response surfaces `detected_hosts` for operator visibility. | approval-system.md | sandbox tool response | ENFORCED |
| R-2.6 | Fingerprint-identical approved executions skip re-approval for the gateway lifetime. | approved-resources-caching.md | `approved_exec_cache.rs` | ENFORCED |
| R-2.7 | Only concrete targets (URLs, IPs) cache; opaque patterns always re-prompt. | approved-resources-caching.md | `approved_exec_cache::has_concrete_targets` | ENFORCED |
| R-2.8 | High-risk promotion requires evaluator AND auditor pass. | spec-install-pipeline-hardening.md §3.1 | `runtime/tools/agent_revision.rs::promote` | ENFORCED |
| R-2.9 | `promotion_record` with `pass=true` rejects on error/critical findings, and on warning findings lacking evidence. | approval-system.md | `runtime/tools/promotion.rs` | ENFORCED |
| R-2.10 | Approval-gated turns suspend to a continuation; real tool result replays on approve. | ARCHITECTURE.md | `runtime/continuation.rs:178 execute_approved_action` | ENFORCED |
| R-2.11 | Suspended turns exceeding timeout mark the task failed and clean the continuation. | ARCHITECTURE.md | scheduler tick | PARTIAL |
| R-2.12 | Operators approve/reject via durable CLI; decisions persist and dispatch signals. | approval-system.md | `gateway approvals approve/reject` | ENFORCED |
| R-2.13 | `user_ask` checkpoints the session as `YieldReason::UserInputRequired`. | architecture-interaction-mechanisms.md | `runtime/tools/user_interaction.rs` | ENFORCED |
| R-2.14 | `user_ask` is refused if the workflow has active children or pending approvals. | architecture-interaction-mechanisms.md | runtime guard | PARTIAL |
| R-2.15 | Spawn payload is preserved verbatim across approval resume. | approval-system.md | `continuation.rs:332` | ENFORCED |

## 3. Sandbox Isolation

Drivers: bubblewrap (default), docker, microvm (firecracker). Isolation
overrides derived from capabilities in `sandbox.rs:42`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-3.1 | Sandboxes default to `--unshare-all` — no network, no PID namespace. | spec-capability-driven-sandbox-isolation.md | `sandbox.rs:914 append_bwrap_isolation_flags` | ENFORCED |
| R-3.2 | `--share-net` is set only when `NetworkAccess` is declared. | spec-capability-driven-sandbox-isolation.md | `sandbox.rs:42 BwrapIsolationOverrides::from_capabilities` | ENFORCED |
| R-3.3 | Script-mode sandbox execution uses identical isolation policy. | spec-capability-driven-sandbox-isolation.md | `execution.rs::execute_script_in_sandbox` | ENFORCED |
| R-3.4 | SDK bridge paths from inside the sandbox are relative-only, no traversal. | — | `sandbox.rs:467 validate_sdk_relative_path` | ENFORCED |
| R-3.5 | Network errors inside the sandbox (URLError, ConnectionError, DNS) are detected and returned as tool failure. | spec-install-pipeline-hardening.md §3.6 | `sandbox.rs::detect_network_errors_in_output` | ENFORCED |
| R-3.6 | Layer mounts are read-only. | spec-build-layers-dependency-resolution.md §2.6 | sandbox mount assembly | ENFORCED |
| R-3.7 | Sandboxes enforce CPU/memory/PID/disk quotas. | ARCHITECTURE.md | docker/microvm yes; bubblewrap relies on OS defaults | PARTIAL |
| R-3.8 | Destructive commands (`sudo`, `rm -rf`, `dd`, `mkfs`, shell injection) are blocked before sandbox creation. | approval-system.md | `policy.rs:46 analyze_command` | ENFORCED |
| R-3.9 | Dependency-manager package names are restricted to safe alphanumerics. | — | `sandbox.rs:1097 validate_dependency_package` | ENFORCED |

## 4. Credential & Secret Protection

Vault in `vault.rs`, redaction in `log_redaction.rs`, injection in
`runtime/tools/credential.rs` and `sandbox.rs`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-4.1 | Secrets never enter agent context; gateway injects at sandbox or HTTP boundary. | credential-management.md | `vault.rs` + tool integration | ENFORCED |
| R-4.2 | Vault uses AES-256-GCM with a random 96-bit nonce per entry. | credential-management.md | `vault.rs:112` | ENFORCED |
| R-4.3 | Master key is required from `AUTONOETIC_VAULT_KEY` or `AUTONOETIC_VAULT_KEY_PATH`; absence disables vault ops. | credential-management.md | `vault.rs:70,84,95` | ENFORCED |
| R-4.4 | Credential IDs (`cred_*`) are mechanical references, never secret material. | credential-management.md | `credential.rs` | ENFORCED |
| R-4.5 | `credential_request` requires `CredentialAccess` matching the service. | credential-management.md | `credential.rs:358` | ENFORCED |
| R-4.6 | `credential_setup` `user_prompt` step suspends the session for operator approval. | credential-management.md | `credential.rs` + `approval.rs` | ENFORCED |
| R-4.7 | `credential_request` response is redacted; raw secrets never returned. | credential-management.md | `credential.rs` response builder | ENFORCED |
| R-4.8 | Secrets are zeroized from memory after injection. | ARCHITECTURE.md | `SecretString` wrapping in `vault.rs` | PARTIAL |
| R-4.9 | `credential_env` passes secrets as env vars resolved server-side. | credential-management.md | `sandbox.rs` credential_env path | ENFORCED |
| R-4.10 | Refresh tokens live in vault, never exposed to agents. | credential-management.md | `vault.rs`, `credential.rs` refresh | ENFORCED |
| R-4.11 | `credential_refresh` 401 auto-retry fires at most once per request. | credential-management.md | `credential.rs` | ENFORCED |
| R-4.12 | Secret-shaped text in responses is blocked by `prohibited_text_patterns`. | response-validation-gate.md | `runtime/response_validation.rs:68` | ENFORCED |
| R-4.13 | Logs, traces, digests, and LLM prompts are redacted via `redact_text_for_logs` before storage. | security-sentinel.md | `log_redaction.rs:128` | ENFORCED |
| R-4.14 | Redaction happens **before** causal-chain append (ordering invariant). | (R+9) | not pinned | MISSING |

## 5. I/O Schema Validation

Enforcement hook for ingress, response validation gate for egress.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-5.1 | Messages to child agents pass `io.accepts` enforcement at ingress. | schema-enforcement-hook.md | `runtime/tools/agent.rs` | ENFORCED |
| R-5.2 | Deterministic coercion runs first; LLM-coercion fallback is an escape hatch (see §12). | schema-enforcement-hook.md | `DeterministicCoercionEnforcer` | ENFORCED |
| R-5.3 | Failed coercion returns an actionable `hint`. | schema-enforcement-hook.md | tool response | ENFORCED |
| R-5.4 | Every enforcement decision is logged (pass/coerce/reject). | schema-enforcement-hook.md | causal event emission | ENFORCED |
| R-5.5 | Response contract checks `required_artifacts`, `max_artifacts`, `max_total_size_mb`, `max_reply_length_chars`. | response-validation-gate.md | `response_validation.rs:68` | ENFORCED |
| R-5.6 | Contract verification uses authoritative runtime state (content-store byte sizes, successful `artifact_build` traces) — not LLM claims. | response-validation-gate.md | `response_validation.rs` | ENFORCED |
| R-5.7 | `output_schema` validates JSON final replies. | response-validation-gate.md | `validate_json_against_schema:563` | ENFORCED |
| R-5.8 | Validation failures trigger a bounded repair loop (`max_validation_loops`, `max_validation_duration_ms`); exhaustion returns error. | response-validation-gate.md | `execution.rs:1965 validate_and_maybe_repair` | ENFORCED |
| R-5.9 | `min_artifact_builds` is verified via execution traces. | response-validation-gate.md | `response_validation.rs` | ENFORCED |
| R-5.10 | `artifact_inspect` accepts explicit `art_*` IDs only; implicit `impl_task-*` handles are rejected. | content-store.md | `runtime/tools/artifact.rs` | ENFORCED |
| R-5.11 | Native tool errors use a uniform `{error_type, message, repair_hint}` envelope. | ARCHITECTURE.md | per-tool response construction | PARTIAL |
| R-5.12 | `error_type: fatal` triggers session abort; recoverable types do not. | ARCHITECTURE.md | lifecycle error processing | ENFORCED |
| R-5.13 | Child → parent tool results validate against `io.returns` on egress. | (R+2) | `runtime/response_validation.rs:68` `execution.rs:1903` | ENFORCED |

## 6. Session, Workflow & Budget

Per-session registries in `runtime/session_budget.rs`,
`runtime/prompt_budget.rs`, and `runtime/checkpoint.rs`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-6.1 | Session budget is role-agnostic per `session_id`. | session-budget.md | `SessionBudgetRegistry` | ENFORCED |
| R-6.2 | `max_llm_rounds` gates before each LLM call; incremented after a real provider call. | session-budget.md | `session_budget.rs::check_pre_llm` | ENFORCED |
| R-6.3 | `max_tool_invocations` gates before each tool batch; all calls in a batch reserve together. | session-budget.md | `reserve_tool_invocations` | ENFORCED |
| R-6.4 | `max_wall_clock_secs` checked at LLM pre-check. | session-budget.md | `check_pre_llm` | ENFORCED |
| R-6.5 | `max_session_price_usd` enforced via OpenRouter catalog estimates. | budget-management.md | `record_llm_completion` + catalog | PARTIAL (silent-disable if catalog unavailable — see §12) |
| R-6.6 | OpenRouter catalog fetches with ~1h TTL; disabled by env. | budget-management.md | `openrouter_catalog.rs::refresh_if_needed` | ENFORCED |
| R-6.7 | Prompt-budget breakdown is logged before every LLM call. | prompt-budget.md | `prompt_budget.rs` | ENFORCED |
| R-6.8 | `system_prompt` and `tool_definitions` max-tokens enforced independently. | prompt-budget.md | section caps in prompt-budget | ENFORCED |
| R-6.9 | `on_exceeded` action (`warn` / `trim_history` / `demote_tools` / `fail`) runs when utilization exceeds budget. | prompt-budget.md | prompt-budget enforcement | ENFORCED |
| R-6.10 | Tool tiers (Core, Workflow, Specialized) filter the visible tool set by runtime state. | prompt-budget.md | `runtime/tools/mod.rs:79 ToolTierFilter::allows` | ENFORCED |
| R-6.11 | Tool schemas compress after turn 0 (`{}` placeholders). | prompt-budget.md | context assembly | ENFORCED |
| R-6.12 | Foundation layers included based on agent capabilities. | prompt-budget.md | `compose_foundation` | ENFORCED |
| R-6.13 | Checkpoints cover every yield reason with `turn_counter`, `loop_guard_state`, and budgets. | ARCHITECTURE.md | `runtime/checkpoint.rs` | ENFORCED |
| R-6.14 | `EmergencyStop` never auto-resumes; `ApprovalRequired` resumes via continuation. | ARCHITECTURE.md | auto-resume dispatch | PARTIAL (test-pin recommended) |
| R-6.15 | Turn continuation atomically replays the pending tool call on approve. | ARCHITECTURE.md | `runtime/continuation.rs` | ENFORCED |
| R-6.16 | `session.fork` branches from a named checkpoint. | ARCHITECTURE.md | JSON-RPC `session.fork` | ENFORCED |
| R-6.17 | Checkpoint retention prunes per configuration. | ARCHITECTURE.md | scheduler prune task | PARTIAL |
| R-6.18 | Workflow orchestration persists `WorkflowRun` on first `agent_spawn`. | workflow-orchestration.md | `workflow_store.rs` | ENFORCED |
| R-6.19 | Child task message/metadata is preserved across approval boundaries. | workflow-orchestration.md | `TaskRun` storage | ENFORCED |
| R-6.20 | User chat addressed to a child `session_id` rewrites to the root session. | workflow-orchestration.md | router `event.ingest` | ENFORCED |
| R-6.21 | Tree-wide budget aggregated across all descendants of a root session. | (R+4) | `runtime/root_session_budget.rs` `runtime/lifecycle.rs:1254` | ENFORCED |
| R-6.22 | Continuation chain depth is bounded. | (R+3) | `execution.rs::spawn_agent_once` depth cap | ENFORCED |

## 7. Abuse / Hard-Stop / Circuit Breakers

Loop guard in `runtime/guard.rs`, emergency stop in
`execution.rs::emergency_stop_root_session`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-7.1 | Emergency stop is reachable by operators, gateway security policy, or agents with `EmergencyStop`. | ARCHITECTURE.md | `policy.rs:535 can_request_emergency_stop` | ENFORCED |
| R-7.2 | Emergency stop kills child processes (SIGKILL), aborts tokio tasks, cancels pending approvals, marks session `EmergencyStopped`. | ARCHITECTURE.md | `execution.rs::emergency_stop_root_session` | ENFORCED |
| R-7.3 | Emergency stop deletes session grants and cancels scheduled jobs for the root session. | approval-system.md | `approval.rs` + `scheduler.rs` | ENFORCED |
| R-7.4 | Emergency stops are recorded in the `emergency_stops` table. | ARCHITECTURE.md | gateway store | ENFORCED |
| R-7.5 | Loop guard trips on `max_tool_failures` per tool (default 5); permission errors do not count. | ARCHITECTURE.md | `guard.rs:66,97` | ENFORCED |
| R-7.6 | Fatal errors (`error_type: fatal`) abort the session regardless of loop-guard budget. | ARCHITECTURE.md | lifecycle error handling | ENFORCED |
| R-7.7 | Consecutive LLM steps without a successful tool result trip the loop guard. | ARCHITECTURE.md | `guard.rs:check_loop` | ENFORCED |
| R-7.8 | Concurrent spawns beyond capability limit return `quota_exceeded`. | agent-capabilities.md | `policy.rs:543` + `agent_spawn` tool | ENFORCED |
| R-7.9 | `AgentSpawn.max_children` is enforced per agent. | agent-capabilities.md | same | ENFORCED |
| R-7.10 | Scheduler rejects sub-threshold intervals (`min_interval_secs`); sub-10s requires script-mode target. | ARCHITECTURE.md | `runtime/tools/scheduler.rs` | ENFORCED |
| R-7.11 | Approval timeout fails the task and cleans the continuation. | ARCHITECTURE.md | scheduler tick | PARTIAL |
| R-7.12 | Promotion gate has no escape hatch; passes require real evaluator + auditor records. | spec-install-pipeline-hardening.md §3.1 | `agent_revision.rs::promote` | ENFORCED |
| R-7.13 | Unresolved dependencies block promotion for high-risk agents. | spec-install-pipeline-hardening.md §3.2 | `install_contract.rs` + `promote` | ENFORCED |
| R-7.14 | `force_complete` refuses `Succeeded` without real child-session evidence. | spec-install-pipeline-hardening.md §A.1 | `workflow.rs::force_complete` | ENFORCED |
| R-7.15 | Spawn-chain depth is bounded system-wide; child `max_depth` ≤ parent's. | (R+3) | `execution.rs::spawn_agent_once` depth cap + `policy.rs::spawn_depth_limit` + `GatewayConfig.max_spawn_depth` | ENFORCED |
| R-7.16 | Orphan children are reaped when the parent session terminates. | (R+12) | not pinned | MISSING |
| R-7.17 | Approval flood cap — pending approvals per root session bounded. | (R+5) | `gateway_store/approvals.rs::create_approval` + `GatewayConfig.max_pending_approvals_per_root` | ENFORCED |

## 8. Audit & Traceability

Causal chain in `causal_chain.rs`, mirrored to SQLite, execution traces
separate. Runtime-lock in `runtime_lock.rs`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-8.1 | Causal chain is append-only JSONL with hash-chain integrity (`entry_hash`, `prev_hash`). | ARCHITECTURE.md | `causal_chain.rs:65` | ENFORCED |
| R-8.2 | Every session, LLM, tool, script, gateway, and memory event is logged with a unique `event_id`. | ARCHITECTURE.md | causal-chain emission sites | ENFORCED |
| R-8.3 | `event_id` is the universal correlation key across traces, reports, and observability. | ARCHITECTURE.md | join logic in tools | ENFORCED |
| R-8.4 | Events are mirrored to SQLite (`causal_events`) without payload truncation. | ARCHITECTURE.md | `gateway_store/causal_events.rs` | ENFORCED |
| R-8.5 | Execution traces record `exit_code`, `stdout`, `stderr`, `duration_ms`, `success`, `error_type` — untruncated. | ARCHITECTURE.md | `execution_traces` table | ENFORCED |
| R-8.6 | Retention policies apply at gateway startup (0 = keep forever). | ARCHITECTURE.md | scheduler cleanup | PARTIAL |
| R-8.7 | Live digest is updated in real time (`session_digest.md`). | ARCHITECTURE.md | `runtime/live_digest.rs` | ENFORCED |
| R-8.8 | Published session reports are catalogued in `published_session_reports` and queryable via `observability.*`. | ARCHITECTURE.md | `runtime/tools/observability.rs` | ENFORCED |
| R-8.9 | Promotion records persist `artifact_id`, `evaluator_pass`, `auditor_pass`, `evidence`, and `content_digest`. | spec-install-pipeline-hardening.md §3.8 | `promotion_store.rs` | ENFORCED |
| R-8.10 | Capability accretion across revisions is detectable via `promotion_history`. | security-sentinel.md | `promotion_history` table | ENFORCED |
| R-8.11 | `runtime.lock` includes compile-time source fingerprint and runtime binary SHA. | spec-install-pipeline-hardening.md §3.7 | `build.rs`, `runtime_lock.rs` | ENFORCED |
| R-8.12 | Schema enforcement decisions are logged with target, result, transformations, and enforcer identity. | schema-enforcement-hook.md | causal event emission | ENFORCED |
| R-8.13 | Knowledge records carry `owner_agent_id`, `writer_agent_id`, `source_ref`; visibility is enforced on recall. | ARCHITECTURE.md | `runtime/memory/*` | ENFORCED |
| R-8.14 | Session approval grants are tracked by `(root_session_id, host)` and included in cleanup audits. | approved-resources-caching.md | `session_approval_grants` table | ENFORCED |
| R-8.15 | Causal-chain append is `fsync`-durable before any state transition that depends on it. | (R+6) | `causal_chain.rs:149` `runtime/tools/promotion.rs:189` `execution.rs:455` `gateway_store/mod.rs:112` | ENFORCED |
| R-8.16 | Retention pruning emits a `retention.pruned` causal event. | (R+17) | not pinned | MISSING |
| R++5 | Every tool call may carry a top-level `intent` field (free-text, 1-2 sentences, max 500 chars) describing the agent's reason for invoking the tool. For privileged tool classes, missing intent is a validation error. When present, the gateway persists the intent verbatim on the `tool_invoke.requested` causal event alongside args. | gateway-constitution-roadmap.md | `runtime/tools/mod.rs` `runtime/tool_call_processor.rs` `runtime/session_tracer.rs` | ENFORCED |

## 9. Agent Install & Provenance

Three-stage activation: `artifact_build → revision.create →
revision.promote`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-9.1 | Activation requires all three stages. | ARCHITECTURE.md | revision workflow | ENFORCED |
| R-9.2 | `agent.install` is not a runtime tool. | agent-capabilities.md | native tool registry | ENFORCED |
| R-9.3 | Revisions are immutable and content-addressed. | ARCHITECTURE.md | `agent_revisions` table | ENFORCED |
| R-9.4 | The alias registry is the sole source of truth for the "active" revision. | ARCHITECTURE.md | `agent_aliases` table | ENFORCED |
| R-9.5 | Candidate revisions are runnable via explicit `agent_ref` without promotion. | ARCHITECTURE.md | session binding | ENFORCED |
| R-9.6 | Revision statuses (`candidate`, `ready`, `rejected`, `archived`) bound what can promote. | ARCHITECTURE.md | `agent_revisions.status` | ENFORCED |
| R-9.7 | Eval gating: if required, revision mismatch rejects promotion. | ARCHITECTURE.md | `agent_revision_promote` | ENFORCED |
| R-9.8 | `SKILL.md` is parsed at install; capabilities, limits, and execution mode extracted. | agent-capabilities.md | skill parser | ENFORCED |
| R-9.9 | High-risk capabilities trigger approval gate on promotion. | spec-install-pipeline-hardening.md | `agent_revision.rs::promote` | ENFORCED |
| R-9.10 | External Python imports are detected at install. | spec-install-pipeline-hardening.md §3.3 | `install_contract.rs::detect_external_python_imports` | ENFORCED |
| R-9.11 | Dependency files with no layers block promotion for high-risk agents. | spec-install-pipeline-hardening.md §3.2 | same | ENFORCED |
| R-9.12 | `BundleHealthReport` is returned in `create_from_intent` responses. | spec-install-pipeline-hardening.md §3.4 | `install_contract.rs::analyze_bundle_health` | ENFORCED |
| R-9.13 | Agent bundle signatures are verified at `agent_revision_create`. | ARCHITECTURE.md (aspirational) | not implemented | MISSING |
| R-9.14 | Trust domains constrain cross-domain agent spawns. | agent-messaging.md | not implemented | DESIGN DEBT |

## 10. Federation / Remote

HTTP in `server/http.rs`, JSON-RPC in `server/jsonrpc.rs`, OFP in
`server/ofp.rs`.

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-10.1 | Remote agents authenticate via Bearer token. | ARCHITECTURE.md | `server/http.rs` | ENFORCED |
| R-10.2 | Content API is exposed over HTTP for remote content access. | content-store.md | HTTP content endpoints | ENFORCED |
| R-10.3 | JSON-RPC ingress requires `AUTONOETIC_SHARED_SECRET`. | spec-install-pipeline-hardening.md §3.10 | `server/jsonrpc.rs` | ENFORCED |
| R-10.4 | Remote agents inherit all approval gates. | remote-access-approval.md | sandbox_exec universal logic | ENFORCED |
| R-10.5 | Layer mounts in remote execution are fetched and cached before sandbox use. | spec-build-layers-dependency-resolution.md §2.6 | HTTP layer download | ENFORCED |
| R-10.6 | OFP messages preserve session context across gateways. | — | future federation layer | DESIGN DEBT |
| R-10.7 | Remote agents cannot self-approve network access. | separation-of-powers.md | policy engine + remote validation | PARTIAL |
| R-10.8 | Shared-secret comparison is constant-time. | (R+15) | needs audit | PARTIAL |

## 11. Inter-Agent Messaging

| ID | Rule | Source | Enforcement | Status |
|---|---|---|---|---|
| R-11.1 | Parent → child messages route through `agent_spawn`. | separation-of-powers.md | `runtime/tools/agent.rs` | ENFORCED |
| R-11.2 | Child `clarification_needed` status returns as a tool result; parent re-spawns. | architecture-interaction-mechanisms.md | spawn result processing | ENFORCED |
| R-11.3 | `agent_message` is peer-to-peer between active sessions. | agent-messaging.md | `agent_messages` table | ENFORCED |
| R-11.4 | Messages auto-inject into the target session at turn start. | agent-messaging.md | `execute_session_turn` | ENFORCED |
| R-11.5 | `agent_message` respects `policy.can_message_agent` ACL. | agent-messaging.md | `policy.rs:554` | ENFORCED |
| R-11.6 | Spawned children inherit `root_session_id` from parent. | content-store.md | session binding | ENFORCED |
| R-11.7 | `max_children` is enforced at spawn. | agent-capabilities.md | `policy.rs:543` | ENFORCED |
| R-11.8 | Spawn payload is preserved across approval and continuation. | approval-system.md | `TurnContinuation` storage | ENFORCED |

---

## 12. Pending rules (`R+`)

Proposed additions under review. Each must pass the amendment process
before it moves into its category.

| ID | Rule | Priority |
|---|---|---|
| R+1 | Structured capability scopes mandatory for all capabilities (not only the three high-risk ones). | P1 |
| R+2 | Child → parent tool results validate against `io.returns` on egress. | P0 |
| R+3 | Spawn-chain depth cap — child `max_children` and `max_depth` ≤ parent's; global ceiling applies. | P0 |
| R+4 | Root-session tree budget — tokens / time / cost aggregated across all descendants. | P0 |
| R+5 | Approval flood cap per root session; further requests reject `approval_flood`. | P0 |
| R+6 | Causal-chain fsync ordering invariant — state transitions gated on event durability. | P0 |
| R+7 | Runtime-lock drift check at session start. | P1 |
| R+8 | Vault master-key presence probe at gateway startup. | P2 |
| R+9 | Redaction-before-write ordering invariant. | P1 |
| R+10 | sandbox→gateway SDK-bridge rate and payload-size limits. | P1 |
| R+11 | Bundle signature verification at `agent_revision_create`. | P1 |
| R+12 | Orphan-child reaper on parent session termination. | P1 |
| R+13 | Approval grant TTL. | P2 |
| R+14 | `can_invoke_tool` denies unknown tool names explicitly. | P2 |
| R+15 | Constant-time comparison for JSON-RPC shared-secret auth. | P1 |
| R+16 | Promotion-gate execution is denied network access. | P1 |
| R+17 | Retention pruning emits `retention.pruned` causal event. | P2 |
| R+18 | Gateway refuses to start if a session's runtime-lock disagrees with the current binary SHA. | merged with R+7 |

### Structural additions (`R++`)

These came from the post-audit architectural review. They address
second-order concerns the R+ set does not: divergence between an LLM's
internal belief and gateway ground truth, slow capability accretion
across revisions, operator approval fatigue, federation audit, and
structural enforcement of the dumb-gateway principle itself. Each R++
item will move into its numbered category once ENFORCED.

| ID | Rule | Priority | Target category |
|---|---|---|---|
| R++1 | At every turn boundary, the gateway injects a signed machine-readable state block (remaining budget, active capabilities, pending approvals, spawn depth, session ids, turn counter) into the agent's context. The agent's system prompt teaches it this block is authoritative and its own memory of these facts is not. | P0 | §6 Session/Workflow |
| R++2 | Promotion of revision N computes `cap_set(N) \ cap_set(N-1)`. A non-empty delta triggers a **separate, differently-shaped approval** that names each added capability explicitly. No single operator click can silently grow an agent's authority. | P0 | §2 Approval |
| R++3 | The auditor and evaluator backing a promotion must be **distinct agent identities** (not merely distinct sessions of the same agent). | P1 | §2 Approval |
| R++4 | Operator approval hardening: (a) dwell time (minimum-visible seconds) on high-risk approvals before the confirm action enables; (b) typed confirmation string required for destructive approval classes (bundle promotion, credential register, first-ever host approval); (c) structural-similarity dedup to the operator, not just fingerprint dedup (prevents near-identical prompt floods from bypassing attention). | P1 | §2 Approval |
| R++6 | A **degraded** session state exists between healthy and emergency-stopped. In degraded mode a session loses non-Core tools, network access, and spawn capability but retains reasoning. Entry is triggered by loop-guard warnings short of trip, by R++8 escape-attempt counts, or by explicit operator command. Exit requires operator clearance. | P1 | §7 Abuse/Hard-Stop |
| R++7 | Cross-gateway causal continuity: cross-gateway events carry a `peer_event_ref` pointing to the corresponding remote chain entry; gateways periodically exchange signed `chain_attestation` digests allowing end-to-end federated trace verification. | P1 | §10 Federation + §8 Audit |
| R++8 | Sandbox-escape attempts are counted. Kernel-denied syscalls (seccomp), denied mount attempts, ptrace calls, and equivalents on docker/microvm drivers increment a per-session counter. Threshold crossings trigger R++6 degraded mode; further escalation triggers emergency stop. | P2 | §3 Sandbox + §7 Abuse |
| R++9 | A property test pins gateway determinism: for random valid inputs, the gateway's decision for `(capability-set, tool-call, recorded-state) → verdict` is a pure function — no LLM call, no network fetch with undeclared fallback, no hidden branch. Any future change that adds nondeterminism fails the test. Prevents principle erosion. | P2 | §13 (cross-cutting) |
| R++10 | Unified fail-mode table: every constitutional invariant has a declared failure action in one place — `refuse-boot`, `refuse-session-start`, `degrade`, `emergency-stop`, or `log-only`. Eliminates silent-disable (R-6.5 OpenRouter-catalog-down default is the archetype to fix). | P1 | §13 (cross-cutting) |

### Constitutional additions (`R+++`)

These emerged from reframing the project around its actual vision —
agents free, responsible, and cooperative — rather than around policy
enforcement for its own sake. They are the structural pieces that
make the constitution a social contract instead of a rulebook:
agents can participate in rule changes (R+++1), cooperating gateways
can verify they are under compatible law (R+++2), and every decision
the gateway makes is attributable to the specific rule it enforced
(R+++3). Without these, the earlier R+ / R++ sets leave the vision
half-built.

| ID | Rule | Priority | Target category |
|---|---|---|---|
| R+++1 | Agents holding the `ConstitutionalProposal` capability may submit amendment proposals through a declared channel (`constitution.propose_amendment`). Proposals are persisted with a durable ID, enter a review queue, carry evidence (causal events or execution traces that motivated them), and cannot be silently dropped. Operator decisions (approve / reject / defer) are recorded with reason. Accepted proposals move into the next constitutional release. | P0 | §2 Approval + §0 Rights (Ri-0.8) |
| R+++2 | Every gateway publishes a `constitution_digest` — SHA-256 over the canonical constitution text plus its rule-ID-to-enforcement-citation table. Cross-gateway requests carry the digest; the receiving gateway verifies compatibility (exact match, known-compatible set, or constitutional superset) before accepting the interaction. Incompatible peers are rejected with `constitutional_incompatibility`. Both digests are recorded in the causal event. | P1 | §10 Federation |
| R+++3 | Every gateway decision (tool accept, tool reject, capability check, approval gate, budget check, schema validation) records the rule ID(s) it enforced in the corresponding causal event. Enables real-time compliance reporting, dead-rule detection (rules never referenced), and gap detection (tool calls accepted without any rule reference — a code path not covered by the constitution, by construction). | P0 | §8 Audit + §0 Rights (Ri-0.3) |

---

## 13. Cross-cutting invariants

These are properties that span categories and must hold end-to-end.

- **I-1** No native-tool code path bypasses `policy.can_invoke_tool`.
  (Currently enforced centrally at `tool_call_processor.rs:294`; new
  tools must route through this gate.)
- **I-2** No causal-chain-visible state transition is acknowledged
  before the corresponding event is durable on disk. (R+6)
- **I-3** Redaction runs before persistence on every path that can
  contain secret-shaped content. (R+9)
- **I-4** Gateway does not make recovery decisions on the agent's
  behalf. (See §14 — the dumbness invariant.)
- **I-5** Rules live in manifests or declared configuration; hard-coded
  policy constants in Rust are discouraged and must be documented here
  if present.
- **I-6** Every enforcement decision is attributable to a rule ID in
  the causal chain. A decision without a rule reference is a gap by
  construction. (R+++3)
- **I-7** Rights (§0) supersede rules (§1–§11) on conflict. A rule
  that cannot be enforced without violating a right must be amended,
  not the right weakened. The gateway escalates conflicts to the
  operator rather than resolving them silently.
- **I-8** Gateway policy decisions (accept / reject) are functions
  only of declared actions, capabilities, and recorded state. They
  are **not** functions of agent reasoning content. This is the
  mechanical form of Ri-0.13(a) — the gateway does not read minds.
- **I-9** Every session termination is attributable to exactly one
  reason in the closed list declared under Ri-0.12. The termination
  reason is recorded in the causal chain.

## 14. The dumbness invariant

The gateway is **not** allowed to:

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

Current violations are listed in
`docs/gateway-constitution-audit-2026-04-24.md §12` and tracked in
`docs/gateway-constitution-roadmap.md`.

---

## Amendment process

Constitutional change is a first-class operation with two legitimate
origins: a human contributor or an agent holding the
`ConstitutionalProposal` capability (Ri-0.8 / R+++1). Both flow
through the same review.

A rule or right is added, changed, or removed only when:

1. A proposal exists — either a PR that updates this file, or an
   `constitution.propose_amendment` call (R+++1) which generates the
   equivalent PR on approve.
2. The proposal includes a test in
   `autonoetic-gateway/tests/constitution_<category>_<rule-id>.rs`
   that fails before the change and passes after.
3. A second human (or the `auditor` agent, operating with real
   evidence) signs off that the rule text matches the enforcement.
4. If the change modifies §0 Rights, an additional explicit operator
   sign-off is required — rights are the floor, and narrowing them
   deserves more friction than narrowing a rule.

No rule is retired without an explicit decision recorded in this
file's history. Silent erosion is the failure mode to guard against.

The `constitution_digest` (R+++2) is recomputed on every merge. A
change to the digest is the mechanical signal that the law has
changed; federated peers observe it through the interop handshake.

### The constitution is self-referential

The act of amending the constitution is itself governed by the
constitution — by Ri-0.8 (right to propose), by R+++1 (the channel),
by the amendment process above, and by the causal chain that records
every proposal and every decision. This is deliberate. A system that
cannot lawfully change its own rules either ossifies or suffers a
revolution; a system that can change them without constraint has no
rules. The middle path is the point.
