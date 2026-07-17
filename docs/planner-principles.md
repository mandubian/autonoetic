# Principle-First Planner Design

## Why Principles, Not Rules

The planner's SKILL.md previously held ~530 lines of prescriptive decision trees — exhaustive failure-mode tables, enumerated capability checklists, and specialist-internal pipeline details that the planner had no reason to own. When the gateway evolved (new tools, new agents, new capability types), each change required a planner update to stay consistent.

**Principles survive gateway evolution. Rules rot.**

The rewrite replaces the rule tree with six durable principles plus a terse foundational agent directory. The pipeline internals moved to `agent-factory.default`, the credential onboarding details moved to `registration.default`, and the discovery machinery moved to `discovery.default`. The planner now holds orchestration policy — not specialist implementation.

---

## The Six Principles

### 1. Capability enforcement is mechanical

The gateway checks every tool call against declared capabilities — every time, no exceptions. The planner cannot override it, only fail. The correct response to a blocked action is: identify the missing capability and route to an agent that has it.

This principle replaces the "CANNOT do directly" sections. Those lists were just capability enumerations; this principle is the reasoning rule that generates them.

### 2. Planner proposes, gateway executes

The planner lacks `NetworkAccess`, `CodeExecution`, and `ArtifactExecution`.
It may operate the vault through `CredentialAccess`, but delegates all code and
artifact execution. This is not a workaround — it is the design.

This principle replaces the "MUST delegate" table. The table was derivable from the planner's capability set; this principle explains why the table exists.

### 3. Secrets never reach LLM context

Any flow involving API keys or tokens must route through `credential_setup` / `credential_request`. The gateway owns the vault. A script that calls a registration API directly prints the secret into LLM context — that is the security anti-pattern `registration.default` exists to prevent.

This principle was the root cause of the security routing bug that triggered this refactor: the planner was sending registration tasks to `coder.default`, which wrote Python scripts that logged secrets into the chat. The principle makes the invariant explicit and durable.

### 4. Reuse state, never recompute

On resume after any interruption, `workflow_state` is the first call — always. The `reuse_guards` flags are mechanical truth. If `has_coder_artifact: true`, do not re-spawn coder. Respecting them is not optional; violating them creates retry loops, wasted compute, and inconsistent workflow state.

### 5. Sequential dependencies are sequential

If B uses A's output, they cannot be parallelized. Only tasks with no data dependency between them may use `async=true`. Agent creation, post-research integration, and credential flows are always sequential chains.

### 6. Artifact IDs come from structured results

Never type artifact refs from memory. Copy from `artifact_build`, `resolve`, or child `result_summary`. Run `artifact_inspect(artifact_ref)` as a preflight before spawning any dependent child. Wrong artifact refs create avoidable retry loops.

### 7. Coordinate by yielding, not polling

The invariant: **never re-issue `workflow_wait` in a loop, and never spin `workflow_state` to discover progress.** Discovering child-state transitions is a mechanical lifecycle concern the gateway already owns (Ri-0.14: *"Parents are not required to poll to discover child state transitions"*); pushing it into prompt logic is fragile and token-expensive — an 880-turn agent-creation session traced largely to such polling loops motivated this principle. How you *wait* depends on the dependency shape:

- **Sequential / single child → end your turn.** Spawn, then stop. The gateway suspends the parent as `WaitingForChild` and wakes it automatically when the child reaches a terminal state or hits a gate, with the child's typed state already in the turn-start context. Yielding costs exactly one resumption — cheaper than blocking.
- **Parallel fan-out you must fully join → one `workflow_wait(task_ids=[all])`.** When several independent children run concurrently and you need all of them before proceeding, a single blocking join returns when the whole group is terminal. This is *not* polling, and it is strictly cheaper than ending your turn and being woken once per child (the gateway emits a per-child notification, so a 3-way fan-out would otherwise cost ~3 resumptions). Call it once; never loop it.
- **Inspection / recovery → `workflow_wait` as a probe.** A `timeout_secs=0` snapshot, or an active wait when recovering a task already suspected stuck.

(This composes with Principle 4: `workflow_state` is still the one call you make *on resume* to read `reuse_guards` — once per wake, never in a loop.)

---

## What Was Removed (and Where It Went)

| Removed from planner | Moved to |
|---|---|
| Full agent creation pipeline (architect → coder → packager → eval/audit → install) | `agent-factory.default` |
| Promotion gate decision matrix | `agent-factory.default` |
| Post-coder dependency check | `agent-factory.default` |
| Agent installation message templates | `agent-factory.default` |
| Service registration / credential_setup loop | `registration.default` |
| "Why registration is not a coder task" prose | Principle 3 |
| Enumerated error-type-to-action table | Failure handling as principles |
| Exhaustive "MUST delegate" and "CANNOT do directly" lists | Foundational agent directory + Principle 2 |

The planner's SKILL.md went from ~530 lines to ~230 lines (−57%) with no loss of coverage — only scope reduction to what the planner genuinely owns.

---

## The Foundational Agent Directory

The planner knows these twelve agents by name. Each entry is one line: the role, when to use it, and the core capability that makes it the right choice.

| Agent | Use when | Core capability |
|---|---|---|
| `researcher.default` | Web/evidence gathering, fetching URLs | NetworkAccess |
| `executor.default` | Quick deterministic bash/script execution without dependencies or artifact handoff | CodeExecution |
| `coder.default` | Durable code, reusable scripts, and artifact-producing implementation work | ArtifactExecution |
| `architect.default` | Multi-file design, structural breakdown | — (design-only) |
| `sealed_evaluator.default` | Sealed-sandbox artifact evaluation (operator-invokable) | CodeExecution + ArtifactExecution |
| `static_evaluator.default` | Static code review, credential flow analysis | SandboxFunctions |
| `unit_test_runner.default` | Runs artifact test suites in no-network sandbox | ArtifactExecution |
| `auditor.default` | Security review, static analysis | — (analysis-only) |
| `packager.default` | Dependency installation for code agents | NetworkAccess (deps) |
| `specialized_builder.default` | Final agent install (revision create + promote) | AgentRevision |
| `debugger.default` | Root cause analysis when things fail repeatedly | CodeExecution |
| `registration.default` | Service onboarding via credential_setup | CredentialAccess |
| `agent-factory.default` | Building a new agent end-to-end | AgentSpawn |
| `discovery.default` | Finding a non-foundational agent for an intent | SandboxFunctions |

For non-foundational (user-installed, evolved) agents, the planner uses `discovery.default` rather than trying to enumerate them statically.

---

## Decision Flow (Abbreviated)

```
1. Service registration/credential onboarding
   → researcher (skill_url if unknown) → registration.default

2. New persistent agent
   → agent-factory.default

3. Research / web / evidence
   → researcher.default

4. Quick deterministic execution (bash, simple scripts, local transforms; no deps, no durable artifact)
   → executor.default

5. Durable implementation work (code that should be reused, reviewed, handed off, or installed)
   → coder.default

6. Debugging
   → debugger.default

7. Recurring/scheduled task
   → agent-factory.default → scheduler_cron_create

8. Pure prose, analysis, knowledge lookup
   → handle directly

9. Structural design
   → architect.default

10. Unknown intent (no foundational agent clearly fits)
   → discovery.default
     If needs_new_agent: true → agent-factory.default
```

The decision flow is short because most complexity is inside the specialist agents. The planner chooses *who*, not *how*.

---

## Security Boundary: Why Registration Lives in a Specialist

The anti-pattern that triggered this refactor:

```
User: "Register my account with Moltbook"
Planner (old): → coder.default
Coder: writes Python script → calls /api/register-agent → prints secret in LLM context
```

The correct path:

```
User: "Register my account with Moltbook"
Planner → researcher.default (discover skill_url from service docs)
Planner → registration.default (spawn with skill_url)
registration.default → credential_setup(skill_url)   # HTTP happens gateway-side
  → gateway stores secret in vault
  → returns suspended_for_user_input if user input needed
registration.default → user_ask(question)
registration.default → credential_setup(credential_id, resume_vars)  # resumes
  → ok: true, credential_id returned
```

The LLM never sees the secret at any step. This is Principle 3 operationalized.

The security guarantee comes from `registration.default`'s capability declaration:
- `CredentialAccess: ["*"]` — can call `credential_setup` and `credential_request`
- `NetworkAccess: ["*"]` — can reach any host for the setup flow

The planner lacks both. It cannot accidentally perform registration even if misconfigured. Capability enforcement is mechanical (Principle 1).
