# Constitutional Law Table (generated)

> **Generated** from `autonoetic-gateway/src/constitution_relations.rs`. Do not edit by hand — run the generator (`BLESS_LAW_TABLE=1`). One row per clause the active constitution declares, recording **which power it binds**, **who has standing to invoke it**, and the verification field. Bind direction is declared data, never derived from the ID prefix (#1284).

This is the **law** side: what a clause obliges, of whom, to whom — identical for any implementation. Which code holds it up is conformance data and lives in [`enforcement-register.md`](enforcement-register.md). See `docs/proposals/constitution-bind-direction-model.md`.

> ⚠️ **`verified_by` is under revision** (RFC #1283 §2.4.1). The column records this implementation's mechanism for enforced clauses and a *requirement* for unenforced ones — one column, two meanings. It is being replaced by `requires` (constitutional) plus `achieved` (register). Read it as provisional.

## Coverage

**124 of 221** clauses classified. The remainder are numbered `P-*` awaiting their section tranche; they are counted, not hidden — a ratchet test pins the exact number so a new clause cannot arrive unclassified.

| binds | clauses |
|---|---|
| `reasoner` | 1 |
| `enforcer` | 118 |
| `decider` | 5 |

| owed to | clauses | means |
|---|---|---|
| `autonoetic_agent` | 32 | a duty the agent can invoke — an agent **right**, whatever the ID prefix says |
| `served_user` | 6 | owed to the end user a session serves |
| `decider` *(seat)* | 5 | owed to whoever occupies the deciding seat, human or agent |
| `none` | 81 | an **integrity property**: no invocable beneficiary. Not a lesser clause — nobody can *claim* their own sandbox confinement |

**Agent rights by relation** (27): `I-8`, `I-9`, `P-2.10`, `P-2.12`, `P-2.15`, `P-5.11`, `P-5.3`, `P-5.8`, `P-7.18`, `P-9.12`, `Ri-0.1`, `Ri-0.10`, `Ri-0.11`, `Ri-0.12`, `Ri-0.13`, `Ri-0.14`, `Ri-0.16`, `Ri-0.17`, `Ri-0.18`, `Ri-0.2`, `Ri-0.3`, `Ri-0.4`, `Ri-0.5`, `Ri-0.6`, `Ri-0.7`, `Ri-0.8`, `Ri-0.9`

A right is a *view*, not a family: an enforcer duty owed to the agent is an agent right regardless of prefix. So this list is not the `Ri-*` set — 10 of its members carry another prefix (`I-8`, `I-9`, `P-2.10`, `P-2.12`, `P-2.15`, `P-5.11`, `P-5.3`, `P-5.8`, `P-7.18`, `P-9.12`), and §0's rights/rules ratio would be computed from this rather than from prefixes.

## Clauses

| clause | binds | owed to | `verified_by` | statement |
|---|---|---|---|---|
| `Ri-0.1` | `enforcer` | `autonoetic_agent` | `test` | Every agent may inspect its own currently-active capabilities, budget state, pending gates (approvals, user interactions, escalations), spawn depth, and session lineage at any turn boundary. |
| `Ri-0.2` | `enforcer` | `autonoetic_agent` | `test` | Every agent may read its own causal chain and execution trace. |
| `Ri-0.3` | `enforcer` | `autonoetic_agent` | `test` | Every rejection names the rule ID that caused it. |
| `Ri-0.4` | `enforcer` | `autonoetic_agent` | `test` | Every agent knows its budget balances truthfully and in real time. |
| `Ri-0.5` | `enforcer` | `autonoetic_agent` | `test` | An agent placed in degraded mode (P-7.18) is told it is degraded, with the rule ID and evidence that triggered the transition, before its next turn executes. |
| `Ri-0.6` | `enforcer` | `autonoetic_agent` | `test` | Capabilities declared in an agent's manifest are not silently changed mid-session. |
| `Ri-0.7` | `enforcer` | `autonoetic_agent` | `test` | An agent may explicitly request session termination. |
| `Ri-0.8` | `enforcer` | `autonoetic_agent` | `test` | Any agent holding the `ConstitutionalProposal` capability may submit an amendment proposal through the declared channel (`constitution_propose_amendment`). |
| `Ri-0.9` | `enforcer` | `autonoetic_agent` | `test` | Where practical (time, process state, absence of immediate harm), the gateway notifies the agent and records the agent's response before degradation or emergency stop. |
| `Ri-0.10` | `enforcer` | `autonoetic_agent` | `test` | Every agent has access to the full text of the constitution it is operating under, addressed by its digest. |
| `Ri-0.11` | `enforcer` | `autonoetic_agent` | `chokepoint` | Every action an agent performs (tool call, message, proposal, spawn, termination request) is attributed to that agent on the causal chain and cannot be retroactively reattributed. |
| `Ri-0.12` | `enforcer` | `autonoetic_agent` | `construction` | Sessions terminate only through a declared, closed list of reasons |
| `Ri-0.13` | `enforcer` | `autonoetic_agent` | `construction` | An agent's internal reasoning — scratchpad tokens, chain-of-thought, internal planning, unexported memory — is **private-under-law** |
| `Ri-0.14` | `enforcer` | `autonoetic_agent` | `test` | When a child task reaches a terminal state (succeeded, failed, cancelled, aborted) or resolves a gate (approval, user interaction, escalation), the gateway wakes the parent with typed child state. |
| `Ri-0.15` | `enforcer` | `decider` *(seat)* | `construction` | Every gate output — every `GateKind` (approval, user_input, escalation, wiki_proposal), to every decider (human or agent) — carries a typed `DecisionContext` sufficient to decide |
| `Ri-0.16` | `enforcer` | `autonoetic_agent` | `test` | The divergence Sentinel is **observational** |
| `Ri-0.17` | `enforcer` | `autonoetic_agent` | `test` | An agent may request export of its own cognitive capsule for migration to another gateway. |
| `Ri-0.18` | `enforcer` | `autonoetic_agent` | `test` | Any agent may file an anomaly report (`anomaly_flag`) at any time, **without holding any capability**. |
| `P-1.1` | — | — | — | *unclassified.* Every tool call matches a declared capability |
| `P-1.2` | — | — | — | *unclassified.* High-risk capabilities (`NetworkAccess`, `CodeExecution`, `AgentSpawn`) reject bare-string shorthand |
| `P-1.3` | — | — | — | *unclassified.* Only agents holding `AgentRevision` may promote revisions. |
| `P-1.4` | — | — | — | *unclassified.* `ReadAccess` / `WriteAccess` scopes are enforced by glob match. |
| `P-1.5` | — | — | — | *unclassified.* `NetworkAccess` is scoped by host allowlist. |
| `P-1.6` | — | — | — | *unclassified.* `SandboxFunctions` applies to MCP tools only |
| `P-1.7` | — | — | — | *unclassified.* `AgentSpawn.max_children` bounds concurrent spawns. |
| `P-1.8` | — | — | — | *unclassified.* `CredentialAccess` is scoped by service pattern. |
| `P-1.9` | — | — | — | *unclassified.* `CodeExecution` patterns match against command strings. |
| `P-1.10` | — | — | — | *unclassified.* Missing capability returns permission error, never advisory. |
| `P-1.11` | — | — | — | *unclassified.* Unknown tool names deny by default (not silent-allow). |
| `P-2.1` | `enforcer` | none *(integrity property)* | `chokepoint` | Remote network access across all networked tools (`sandbox_exec`, `credential.*`, `web.*`) is statically detected and blocks pending approval via the unified `GateService` (`GateKind::Approval`) rather than hard-denying. |
| `P-2.2` | `enforcer` | none *(integrity property)* | `test` | Approval requests are persisted with unique IDs. |
| `P-2.3` | `enforcer` | none *(integrity property)* | `chokepoint` | Identical operations within a session deduplicate. |
| `P-2.4` | `enforcer` | none *(integrity property)* | `test` | Approved hosts auto-approve subsequent calls within the root session, scoped to the approving agent |
| `P-2.5` | `enforcer` | `decider` *(seat)* | `test` | Approval response surfaces `detected_hosts` for operator visibility. |
| `P-2.6` | `enforcer` | none *(integrity property)* | `test` | Fingerprint-identical approved executions skip re-approval until the cache entry expires |
| `P-2.7` | `enforcer` | none *(integrity property)* | `test` | Only concrete targets (URLs, IPs) cache |
| `P-2.8` | `enforcer` | none *(integrity property)* | `chokepoint` | High-risk promotion requires evaluator AND auditor pass. |
| `P-2.9` | `reasoner` | none *(integrity property)* | `chokepoint` | `promotion_record` evidence is trace-based for execution roles (`unit_test_runner`, `sealed_evaluator`, legacy `evaluator`) |
| `P-2.10` | `enforcer` | `autonoetic_agent` | `test` | Gate-suspended turns (approval, user interaction, escalation) checkpoint via `YieldReason` and resume through `resume_from_checkpoint`. |
| `P-2.11` | `enforcer` | none *(integrity property)* | `test` | Suspended turns exceeding timeout mark the task failed while preserving continuation for explicit operator-driven resume. |
| `P-2.12` | `enforcer` | `autonoetic_agent` | `test` | Deciders (human operators, autonomous reviewer agents, or policy engines) approve/reject gates via the approval resolution API. |
| `P-2.13` | `enforcer` | none *(integrity property)* | `test` | `user_ask` creates a gate via `GateService` with `GateKind::UserInput` and checkpoints the session as `YieldReason::UserInputRequired`. |
| `P-2.14` | `enforcer` | none *(integrity property)* | `test` | `user_ask` is refused if the workflow has active children or pending gates (approvals, escalations, or other `user_ask` interactions). |
| `P-2.15` | `enforcer` | `autonoetic_agent` | `test` | Spawn payload is preserved verbatim across approval resume. |
| `P-2.16` | `enforcer` | `decider` *(seat)* | `chokepoint` | Promotion of revision N computes `cap_set(N) \ cap_set(N-1)`. |
| `P-2.17` | `enforcer` | none *(integrity property)* | `chokepoint` | The auditor and evaluator backing a promotion must be **distinct agent identities** (not merely distinct sessions of the same agent). |
| `P-2.18` | `enforcer` | none *(integrity property)* | `chokepoint` | All execution suspension points awaiting external input (approvals, user interactions, escalations) use the unified `GateService`. |
| `P-2.19` | `enforcer` | none *(integrity property)* | `test` | Gate enrichment messages (`gate_messages`) are append-only and recorded on the causal chain. |
| `P-2.20` | `enforcer` | none *(integrity property)* | `chokepoint` | Agents acting as gate deciders require the `GateDecider` capability. |
| `P-2.21` | `decider` | `autonoetic_agent` | `test` | When an agent-decider cannot determine whether to approve or reject a gate (insufficient context, policy ambiguity, or high-risk action beyond its scope), it must escalate to a human operator rather than reject. |
| `P-2.22` | `enforcer` | none *(integrity property)* | `chokepoint` | When a revision carries federation-role verdicts, promotion runs the **FullJury** gate |
| `P-2.23` | `enforcer` | none *(integrity property)* | `test` | Session approval grants expire after a configured TTL |
| `P-2.24` | `enforcer` | `decider` *(seat)* | `test` | Operator approval hardening on high-risk gates |
| `P-2.25` | `enforcer` | none *(integrity property)* | `chokepoint` | **Promotion is fail-closed.** Whether a revision may be promoted, and what it must satisfy, is determined **mechanically by the gateway** from the revision's declared capabilities and artifact — never inferred from orchestrator-supplied signals (recorded verdicts, an attached synthesis, or the presence/absence of a field). |
| `P-2.26` | `enforcer` | none *(integrity property)* | `chokepoint` | **All executed gate roles must pass.** When a federation gate role (`static_evaluator`, `unit_test_runner`, `sealed_evaluator`) has recorded a verdict for a revision's artifact, the promotion gate mechanically checks that **every** such role recorded `pass=true`. |
| `P-2.27` | `enforcer` | `decider` *(seat)* | `test` | A **session capability envelope**, locked by operator decision, pre-authorizes tool calls within its scope. |
| `P-2.28` | `enforcer` | none *(integrity property)* | `chokepoint` | **Smoke-test gate for new agents.** New agents declaring `NetworkAccess` or `CodeExecution` require a successful execution trace before promotion to `Ready`. |
| `P-2.29` | `enforcer` | none *(integrity property)* | `chokepoint` | **Promotion attempt exhaustion gate.** Too many rejected promotion attempts for the same `(alias, content_digest)` across sessions blocks further attempts until an operator acknowledges the revision. |
| `P-3.1` | — | — | — | *unclassified.* Sandboxes default to `--unshare-all` — no network, no PID namespace. |
| `P-3.2` | — | — | — | *unclassified.* `--share-net` for `sandbox_exec` follows the per-exec operator network grant |
| `P-3.3` | — | — | — | *unclassified.* Script-mode sandbox execution uses identical isolation policy. |
| `P-3.4` | — | — | — | *unclassified.* SDK bridge paths from inside the sandbox are relative-only, no traversal. |
| `P-3.5` | — | — | — | *unclassified.* Network errors inside the sandbox (URLError, ConnectionError, DNS) are detected and returned as tool failure. |
| `P-3.6` | — | — | — | *unclassified.* Layer mounts are read-only. |
| `P-3.7` | — | — | — | *unclassified.* Sandbox resource quotas are operator-declared and fail-shut |
| `P-3.8` | — | — | — | *unclassified.* Destructive commands (`sudo`, `rm -rf`, `dd`, `mkfs`, shell injection) are blocked before sandbox creation. |
| `P-3.9` | — | — | — | *unclassified.* Dependency-manager package names are restricted to safe alphanumerics. |
| `P-3.10` | — | — | — | *unclassified.* Promotion-gate execution (sealed evaluator / auditor runs) is denied network access regardless of the candidate's declared `NetworkAccess`. |
| `P-4.1` | — | — | — | *unclassified.* Secrets never enter agent context |
| `P-4.2` | — | — | — | *unclassified.* Vault uses AES-256-GCM |
| `P-4.3` | — | — | — | *unclassified.* Master key is required from `AUTONOETIC_VAULT_KEY` or `AUTONOETIC_VAULT_KEY_PATH` |
| `P-4.4` | — | — | — | *unclassified.* Credential IDs (`cred_*`) are mechanical references, never secret material. |
| `P-4.5` | — | — | — | *unclassified.* `credential_request` requires `CredentialAccess` matching the service. |
| `P-4.6` | — | — | — | *unclassified.* `credential_setup` `user_prompt` step suspends the session for operator approval. |
| `P-4.7` | — | — | — | *unclassified.* `credential_request` response is redacted |
| `P-4.8` | — | — | — | *unclassified.* Secrets are zeroized from memory after injection. |
| `P-4.9` | — | — | — | *unclassified.* `credential_env` passes secrets as env vars resolved server-side. |
| `P-4.10` | — | — | — | *unclassified.* Refresh tokens live in vault, never exposed to agents. |
| `P-4.11` | — | — | — | *unclassified.* `credential_refresh` 401 auto-retry fires at most once per request. |
| `P-4.12` | — | — | — | *unclassified.* Secret-shaped text in responses is blocked by `prohibited_text_patterns`. |
| `P-4.13` | — | — | — | *unclassified.* Logs, traces, digests, and LLM prompts are redacted via `redact_text_for_logs` before storage. |
| `P-4.14` | — | — | — | *unclassified.* Redaction happens **before** causal-chain append (ordering invariant). |
| `P-4.15` | — | — | — | *unclassified.* The gateway probes vault master-key presence at startup, emits a causal event recording the result, and refuses to start when the probe fails. |
| `P-5.1` | `enforcer` | none *(integrity property)* | `chokepoint` | Messages to child agents pass `io.accepts` enforcement at ingress. |
| `P-5.2` | `enforcer` | none *(integrity property)* | `construction` | Coercion is deterministic only. |
| `P-5.3` | `enforcer` | `autonoetic_agent` | `test` | Failed coercion returns an actionable `hint`. |
| `P-5.4` | `enforcer` | none *(integrity property)* | `detection` | Every enforcement decision is logged (pass/coerce/reject). |
| `P-5.5` | `enforcer` | none *(integrity property)* | `test` | Response contract checks `required_artifacts`, `max_artifacts`, `max_total_size_mb`, `max_reply_length_chars`. |
| `P-5.6` | `enforcer` | none *(integrity property)* | `test` | Contract verification uses authoritative runtime state (content-store byte sizes, successful `artifact_build` traces) — not LLM claims. |
| `P-5.7` | `enforcer` | none *(integrity property)* | `test` | `output_schema` validates JSON final replies. |
| `P-5.8` | `enforcer` | `autonoetic_agent` | `chokepoint` | Validation failures may trigger the bounded output-repair loop — strictly opt-in (manifest `io.output_policy.repair.auto: true`; `response_validation.repair_enabled` defaults to false) |
| `P-5.9` | `enforcer` | none *(integrity property)* | `test` | `min_artifact_builds` is verified via execution traces. |
| `P-5.10` | `enforcer` | none *(integrity property)* | `test` | `artifact_inspect` accepts explicit `art_*` IDs only |
| `P-5.11` | `enforcer` | `autonoetic_agent` | `test` | Native tool errors use a uniform error envelope. |
| `P-5.12` | `enforcer` | none *(integrity property)* | `test` | `error_type: fatal` triggers session abort |
| `P-5.13` | `enforcer` | none *(integrity property)* | `chokepoint` | Child → parent tool results validate against `io.returns` on egress. |
| `P-5.14` | `enforcer` | none *(integrity property)* | `construction` | Every workflow-relevant tool/task failure is classified into a `failure_class` from a closed enum (`FailureClass`). |
| `P-6.1` | — | — | — | *unclassified.* Session budget is role-agnostic per `session_id`. |
| `P-6.2` | — | — | — | *unclassified.* `max_llm_rounds` gates before each LLM call |
| `P-6.3` | — | — | — | *unclassified.* `max_tool_invocations` gates before each tool batch |
| `P-6.4` | — | — | — | *unclassified.* `max_wall_clock_secs` checked at LLM pre-check. |
| `P-6.5` | — | — | — | *unclassified.* `max_session_price_usd` enforced via OpenRouter catalog estimates. |
| `P-6.6` | — | — | — | *unclassified.* OpenRouter catalog fetches with ~1h TTL |
| `P-6.7` | — | — | — | *unclassified.* Prompt-budget breakdown is logged before every LLM call. |
| `P-6.8` | — | — | — | *unclassified.* `system_prompt` and `tool_definitions` max-tokens enforced independently. |
| `P-6.9` | — | — | — | *unclassified.* Context governor cascades reduction strategies (tool-schema compression, hierarchical capsule summarization, history trimming, tool demotion) when utilization exceeds the prompt budget |
| `P-6.10` | — | — | — | *unclassified.* Tool tiers (Core, Workflow, Specialized) filter the visible tool set by runtime state. |
| `P-6.11` | — | — | — | *unclassified.* Tool schemas compress after turn 0 (`{}` placeholders). |
| `P-6.12` | — | — | — | *unclassified.* Foundation layers included based on agent capabilities. |
| `P-6.13` | — | — | — | *unclassified.* Checkpoints cover every yield reason with `turn_counter`, `loop_guard_state`, and budgets. |
| `P-6.14` | — | — | — | *unclassified.* `EmergencyStop` never auto-resumes |
| `P-6.15` | — | — | — | *unclassified.* Turn continuation atomically replays the pending tool call on approve. |
| `P-6.16` | — | — | — | *unclassified.* `session.fork` branches from a named checkpoint. |
| `P-6.17` | — | — | — | *unclassified.* Checkpoint retention prunes per configuration. |
| `P-6.18` | — | — | — | *unclassified.* Workflow orchestration persists `WorkflowRun` on first `agent_spawn`. |
| `P-6.19` | — | — | — | *unclassified.* Child task message/metadata is preserved across approval boundaries. |
| `P-6.20` | — | — | — | *unclassified.* User chat addressed to a child `session_id` rewrites to the root session. |
| `P-6.21` | — | — | — | *unclassified.* Tree-wide budget aggregated across all descendants of a root session. |
| `P-6.22` | — | — | — | *unclassified.* Continuation chain depth is bounded. |
| `P-6.23` | — | — | — | *unclassified.* At every turn boundary, the gateway injects a signed machine-readable state block (remaining budget, active capabilities, pending gates — including approvals, user interactions, and escalations — spawn depth, session ids, turn counter) into the agent's context. |
| `P-6.24` | — | — | — | *unclassified.* Duplicate durable operations (install, promote, rollback, artifact-backed build stages) are detected by a single-flight dedupe key — `(workflow_id, stage_kind, agent_id, artifact_ref)`, with a normalized intent digest substituted for `artifact_ref` on reasoning-only installs. |
| `P-6.25` | — | — | — | *unclassified.* Stage-local retry is opt-in and bounded. |
| `P-6.26` | — | — | — | *unclassified.* Durable operations report `side_effect_state` from a closed enum (`none`, `committed`, `unknown`). |
| `P-7.1` | `enforcer` | none *(integrity property)* | `test` | Emergency stop is reachable by operators, gateway security policy, or agents with `EmergencyStop`. |
| `P-7.2` | `enforcer` | none *(integrity property)* | `test` | Emergency stop kills child processes (SIGKILL), aborts tokio tasks, cancels pending approvals, revokes session envelopes (P-2.27), marks session `EmergencyStopped`. |
| `P-7.3` | `enforcer` | none *(integrity property)* | `test` | Emergency stop deletes session grants and revokes session envelopes for the root session. |
| `P-7.4` | `enforcer` | none *(integrity property)* | `test` | Emergency stops are recorded in the `emergency_stops` table. |
| `P-7.5` | `enforcer` | none *(integrity property)* | `test` | Loop guard trips on `max_tool_failures` per tool (configurable; current default in `docs/reference/config.md`) |
| `P-7.6` | `enforcer` | none *(integrity property)* | `test` | Fatal errors (`error_type: fatal`) abort the session regardless of loop-guard budget. |
| `P-7.7` | `enforcer` | none *(integrity property)* | `test` | Consecutive LLM steps without a successful tool result trip the loop guard. |
| `P-7.8` | `enforcer` | none *(integrity property)* | `test` | Concurrent spawns beyond capability limit return `quota_exceeded`. |
| `P-7.9` | `enforcer` | none *(integrity property)* | `test` | `AgentSpawn.max_children` is enforced per agent. |
| `P-7.10` | `enforcer` | none *(integrity property)* | `test` | Scheduler rejects sub-threshold intervals (`min_interval_secs`) |
| `P-7.11` | `enforcer` | none *(integrity property)* | `test` | Approval timeout fails the task while preserving the continuation for operator-driven resume. |
| `P-7.12` | `enforcer` | none *(integrity property)* | `chokepoint` | Promotion gate has no escape hatch |
| `P-7.13` | `enforcer` | none *(integrity property)* | `chokepoint` | Unresolved dependencies block promotion for high-risk agents. |
| `P-7.14` | `enforcer` | none *(integrity property)* | `test` | `force_complete` refuses `Succeeded` without real child-session evidence. |
| `P-7.15` | `enforcer` | none *(integrity property)* | `test` | Spawn-chain depth is bounded system-wide |
| `P-7.16` | `enforcer` | none *(integrity property)* | `test` | Orphan children are reaped when the parent session terminates |
| `P-7.17` | `enforcer` | none *(integrity property)* | `test` | Approval flood cap — pending approvals per root session bounded. |
| `P-7.18` | `enforcer` | `autonoetic_agent` | `test` | A **degraded** session state exists between healthy and emergency-stopped. |
| `P-7.19` | `enforcer` | none *(integrity property)* | `test` | The loop guard also trips when successful tool calls make no *semantic* progress. |
| `P-7.20` | `enforcer` | none *(integrity property)* | `test` | The loop guard trips when child-task failures in a session reach `loop_guard.max_child_failures` (configurable; current default in `docs/reference/config.md`). |
| `P-7.21` | `enforcer` | none *(integrity property)* | `test` | The sandbox→gateway SDK bridge enforces request-rate and payload-size limits. |
| `P-7.22` | `enforcer` | none *(integrity property)* | `detection` | Sandbox-escape attempts are counted per session — kernel-denied syscalls (seccomp), denied mount attempts, ptrace calls, and driver-equivalents on docker/microvm/wasm increment a per-session counter. |
| `P-8.1` | `enforcer` | none *(integrity property)* | `chokepoint` | Causal chain is append-only JSONL with hash-chain integrity (`entry_hash`, `prev_hash`). |
| `P-8.2` | — | — | — | *unclassified.* Every session, LLM, tool, script, gateway, and memory event is logged with a unique `event_id`. |
| `P-8.3` | — | — | — | *unclassified.* `event_id` is the universal correlation key across traces, reports, and observability. |
| `P-8.4` | — | — | — | *unclassified.* Events are mirrored to SQLite (`causal_events`) without payload truncation. |
| `P-8.5` | — | — | — | *unclassified.* Execution traces record `exit_code`, `stdout`, `stderr`, `duration_ms`, `success`, `error_type` — untruncated. |
| `P-8.6` | — | — | — | *unclassified.* Retention policies apply at gateway startup (0 = keep forever). |
| `P-8.7` | — | — | — | *unclassified.* Live digest is updated in real time (`session_digest.md`). |
| `P-8.8` | — | — | — | *unclassified.* Published session reports are catalogued in `published_session_reports` and queryable via `observability.*`. |
| `P-8.9` | — | — | — | *unclassified.* Promotion records persist `artifact_id`, `evaluator_pass`, `auditor_pass`, `evidence`, and `content_digest`. |
| `P-8.10` | — | — | — | *unclassified.* Capability accretion across revisions is detectable via `promotion_history`. |
| `P-8.11` | — | — | — | *unclassified.* `runtime.lock` includes compile-time source fingerprint and runtime binary SHA. |
| `P-8.12` | — | — | — | *unclassified.* Sessions refuse to start when `runtime.lock` gateway section disagrees with the running gateway binary. |
| `P-8.13` | — | — | — | *unclassified.* Schema enforcement decisions are logged with target, result, transformations, and enforcer identity. |
| `P-8.14` | — | — | — | *unclassified.* Knowledge records carry `owner_agent_id`, `writer_agent_id`, `source_ref` |
| `P-8.15` | — | — | — | *unclassified.* Session approval grants are tracked by `(root_session_id, host)` and included in cleanup audits. |
| `P-8.16` | — | — | — | *unclassified.* Causal-chain append is `fsync`-durable before any state transition that depends on it. |
| `P-8.17` | — | — | — | *unclassified.* Retention pruning emits a `retention.pruned` causal event. |
| `P-8.18` | — | — | — | *unclassified.* Every tool call may carry a top-level `intent` field (free-text, 1-2 sentences, max 500 chars) describing the agent's reason for invoking the tool. |
| `P-8.19` | — | — | — | *unclassified.* Every gate resolution (approve, reject, cancel, timeout) records `decided_by` with the full decider identity on the causal chain. |
| `P-9.1` | `enforcer` | none *(integrity property)* | `chokepoint` | Activation requires all three stages. |
| `P-9.2` | `enforcer` | none *(integrity property)* | `construction` | `agent.install` is not a runtime tool. |
| `P-9.3` | `enforcer` | none *(integrity property)* | `construction` | Revisions are immutable and content-addressed. |
| `P-9.4` | `enforcer` | none *(integrity property)* | `chokepoint` | The alias registry is the sole source of truth for the "active" revision. |
| `P-9.5` | `enforcer` | none *(integrity property)* | `test` | Candidate revisions are runnable via explicit `agent_ref` without promotion. |
| `P-9.6` | `enforcer` | none *(integrity property)* | `test` | Revision statuses (`candidate`, `ready`, `archived`) bound what can promote |
| `P-9.7` | `enforcer` | none *(integrity property)* | `chokepoint` | Eval gating — if required, a revision mismatch rejects promotion. |
| `P-9.8` | `enforcer` | none *(integrity property)* | `test` | `SKILL.md` is parsed at install |
| `P-9.9` | `enforcer` | none *(integrity property)* | `chokepoint` | High-risk capabilities trigger approval gate on promotion. |
| `P-9.10` | `enforcer` | none *(integrity property)* | `test` | External Python imports are detected at install. |
| `P-9.11` | `enforcer` | none *(integrity property)* | `chokepoint` | Dependency files with no layers block promotion for high-risk agents. |
| `P-9.12` | `enforcer` | `autonoetic_agent` | `test` | `BundleHealthReport` is returned in `create_from_intent` responses. |
| `P-9.13` | `enforcer` | none *(integrity property)* | `test` | Agent bundle signatures are verified at `agent_revision_create`. |
| `P-9.14` | `enforcer` | none *(integrity property)* | `chokepoint` | Trust domains constrain cross-domain agent spawns. |
| `P-9.15` | `enforcer` | none *(integrity property)* | `chokepoint` | **Single door.** Every surface that activates an agent — moves an alias to a revision — passes the same promotion gates |
| `P-9.16` | `enforcer` | none *(integrity property)* | `test` | **Import provenance.** An agent installed from an external source durably records, on its revision, the source URL, a content digest of the fetched material, and the install time (`source_kind: "skill_install"`, `source_ref: "<url>#sha256=<digest>"`), and the install emits a causal event. |
| `P-10.1` | — | — | — | *unclassified.* Remote agents authenticate via Bearer token. |
| `P-10.2` | — | — | — | *unclassified.* Content API is exposed over HTTP for remote content access. |
| `P-10.3` | — | — | — | *unclassified.* JSON-RPC ingress requires `AUTONOETIC_SHARED_SECRET`. |
| `P-10.4` | — | — | — | *unclassified.* Remote agents inherit all approval gates. |
| `P-10.5` | — | — | — | *unclassified.* Layer mounts in remote execution are fetched and cached before sandbox use. |
| `P-10.6` | — | — | — | *unclassified.* OFP federated exchanges preserve cross-gateway causal context |
| `P-10.7` | — | — | — | *unclassified.* No agent may resolve its own gate requests, whether directly or via a delegated agent it spawned. |
| `P-10.8` | — | — | — | *unclassified.* Shared-secret comparison is constant-time. |
| `P-10.9` | — | — | — | *unclassified.* Every gateway publishes a `constitution_digest` — SHA-256 over the canonical constitution text plus its rule-ID-to-enforcement-citation table. |
| `P-11.1` | — | — | — | *unclassified.* Parent → child messages route through `agent_spawn`. |
| `P-11.2` | — | — | — | *unclassified.* Child `clarification_needed` status returns as a tool result |
| `P-11.3` | — | — | — | *unclassified.* `agent_message` is peer-to-peer between active sessions. |
| `P-11.4` | — | — | — | *unclassified.* Messages auto-inject into the target session at turn start. |
| `P-11.5` | — | — | — | *unclassified.* `agent_message` respects two mechanical gates |
| `P-11.6` | — | — | — | *unclassified.* Spawned children inherit `root_session_id` from parent. |
| `P-11.7` | — | — | — | *unclassified.* `max_children` is enforced at spawn. |
| `P-11.8` | — | — | — | *unclassified.* Spawn payload is preserved across approval and continuation. |
| `U-1` | `enforcer` | `served_user` | `chokepoint` | The served party may refuse a delivered result, without penalty and without needing to justify the refusal. |
| `U-2` | `enforcer` | `served_user` | `test` | The served party may obtain a plain-language account of what was done on their behalf — distinct from Ri-0.2, which is the *agent's* right to its own causal chain, not an accounting owed to the party the agent acted for. |
| `U-3` | `enforcer` | `served_user` | `test` | On exit, the served party may obtain or require deletion of the data held on their behalf. |
| `O-1` | `decider` | `autonoetic_agent` | `chokepoint` | A decision owes a **motivation**, graduated by stakes. |
| `O-2` | `decider` | `autonoetic_agent` | `chokepoint` | Every decision is **attributed** to the deciding principal (id + kind) on the causal chain and cannot be reattributed. |
| `O-6` | `decider` | `autonoetic_agent` | `detection` | A proposal review authority owes every Ri-0.8 proposal a **recorded decision** (`approved`/`rejected`/`deferred`/`under_review`) with motivation once actioned, **within a bounded adjudication window**. |
| `O-7` | `decider` | `autonoetic_agent` | `detection` | An anomaly review authority owes every Ri-0.18 flag a **recorded decision** (`confirmed`/`dismissed`/`deferred`, with `under_review` as the non-terminal holding state) with motivation once actioned, **within a bounded adjudication window**. |
| `I-1` | `enforcer` | none *(integrity property)* | `chokepoint` | No native-tool code path bypasses `policy.can_invoke_tool`. |
| `I-2` | `enforcer` | none *(integrity property)* | `chokepoint` | No causal-chain-visible state transition is acknowledged |
| `I-3` | `enforcer` | none *(integrity property)* | `construction` | Redaction runs before persistence on every path that can |
| `I-4` | `enforcer` | none *(integrity property)* | `detection` | Gateway does not make recovery decisions on the agent's |
| `I-5` | `enforcer` | none *(integrity property)* | `registry` | Rules live in manifests or declared configuration |
| `I-6` | `enforcer` | none *(integrity property)* | `detection` | Every enforcement decision is attributable to a rule ID in |
| `I-7` | `enforcer` | none *(integrity property)* | `detection` | Rights (§0) supersede rules (§1–§11) on conflict. |
| `I-8` | `enforcer` | `autonoetic_agent` | `construction` | Gateway policy decisions (accept / reject) are functions |
| `I-9` | `enforcer` | `autonoetic_agent` | `construction` | Every session termination is attributable to exactly one |
| `I-10` | `enforcer` | none *(integrity property)* | `sampling` | Gateway decision surfaces are deterministic over declared |
| `I-11` | `enforcer` | none *(integrity property)* | `registry` | Every constitutional invariant has a declared failure action |
| `I-12` | `enforcer` | none *(integrity property)* | `construction` | Any collective decision mechanism among principals (voting, |
| `I-13` | `enforcer` | none *(integrity property)* | `test` | A newborn agent's capabilities are |
| `I-14` | `enforcer` | none *(integrity property)* | `chokepoint` | Egress labels (§15) are |
| `P-15.1` | `enforcer` | `served_user` | `chokepoint` | Content carrying an egress label must never be included in a request to a sink the label excludes |
| `P-15.2` | `enforcer` | `served_user` | `chokepoint` | Any surface that moves session-derived bytes off-machine — sandbox `share_net`, gateway web tools, hook deliveries, remote MCP calls, OFP federation, context compression — gates on session taint before send |
| `P-15.3` | `enforcer` | `served_user` | `chokepoint` | A label widens only via an explicit, operator-approved **declassification grant** — content- or host-scoped, optionally expiring, revocable at any time, and causal-logged (`egress.declassified` on grant, `grant_revocation` on revoke). |
