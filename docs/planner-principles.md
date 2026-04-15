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

The planner lacks `NetworkAccess`, `CredentialAccess`, and `CodeExecution`. Any action requiring those must be delegated. This is not a workaround — it is the design. The planner is a pure reasoner; specialists are the executors.

This principle replaces the "MUST delegate" table. The table was derivable from the planner's capability set; this principle explains why the table exists.

### 3. Secrets never reach LLM context

Any flow involving API keys or tokens must route through `credential.setup` / `credential.request`. The gateway owns the vault. A script that calls a registration API directly prints the secret into LLM context — that is the security anti-pattern `registration.default` exists to prevent.

This principle was the root cause of the security routing bug that triggered this refactor: the planner was sending registration tasks to `coder.default`, which wrote Python scripts that logged secrets into the chat. The principle makes the invariant explicit and durable.

### 4. Reuse state, never recompute

On resume after any interruption, `workflow.state` is the first call — always. The `reuse_guards` flags are mechanical truth. If `has_coder_artifact: true`, do not re-spawn coder. Respecting them is not optional; violating them creates retry loops, wasted compute, and inconsistent workflow state.

### 5. Sequential dependencies are sequential

If B uses A's output, they cannot be parallelized. Only tasks with no data dependency between them may use `async=true`. Agent creation, post-research integration, and credential flows are always sequential chains.

### 6. Artifact IDs come from structured results

Never type artifact IDs from memory. Copy from `artifact.build`, `artifact.resolve_ref`, or child `result_summary`. Run `artifact.inspect(artifact_id)` as a preflight before spawning any dependent child. Wrong artifact IDs create avoidable retry loops.

---

## What Was Removed (and Where It Went)

| Removed from planner | Moved to |
|---|---|
| Full agent creation pipeline (architect → coder → packager → eval/audit → install) | `agent-factory.default` |
| Promotion gate decision matrix | `agent-factory.default` |
| Post-coder dependency check | `agent-factory.default` |
| Agent installation message templates | `agent-factory.default` |
| Service registration / credential.setup loop | `registration.default` |
| "Why registration is not a coder task" prose | Principle 3 |
| Enumerated error-type-to-action table | Failure handling as principles |
| Exhaustive "MUST delegate" and "CANNOT do directly" lists | Foundational agent directory + Principle 2 |

The planner's SKILL.md went from ~530 lines to ~230 lines (−57%) with no loss of coverage — only scope reduction to what the planner genuinely owns.

---

## The Foundational Agent Directory

The planner knows these eleven agents by name. Each entry is one line: the role, when to use it, and the core capability that makes it the right choice.

| Agent | Use when | Core capability |
|---|---|---|
| `researcher.default` | Web/evidence gathering, fetching URLs | NetworkAccess |
| `coder.default` | Executable code, scripts | CodeExecution |
| `architect.default` | Multi-file design, structural breakdown | — (design-only) |
| `evaluator.default` | Behavioral validation, test execution | CodeExecution |
| `auditor.default` | Security review, static analysis | — (analysis-only) |
| `packager.default` | Dependency installation for code agents | NetworkAccess (deps) |
| `specialized_builder.default` | Final agent install (revision create + promote) | AgentRevision |
| `debugger.default` | Root cause analysis when things fail repeatedly | CodeExecution |
| `registration.default` | Service onboarding via credential.setup | CredentialAccess |
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

4. One-shot executable code
   → coder.default

5. Debugging
   → debugger.default

6. Recurring/scheduled task
   → agent-factory.default → scheduler.cron.create

7. Pure prose, analysis, knowledge lookup
   → handle directly

8. Structural design
   → architect.default

9. Unknown intent (no foundational agent clearly fits)
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
registration.default → credential.setup(skill_url)   # HTTP happens gateway-side
  → gateway stores secret in vault
  → returns suspended_for_user_input if user input needed
registration.default → user.ask(question)
registration.default → credential.setup(credential_id, resume_vars)  # resumes
  → ok: true, credential_id returned
```

The LLM never sees the secret at any step. This is Principle 3 operationalized.

The security guarantee comes from `registration.default`'s capability declaration:
- `CredentialAccess: ["*"]` — can call `credential.setup` and `credential.request`
- `NetworkAccess: ["*"]` — can reach any host for the setup flow

The planner lacks both. It cannot accidentally perform registration even if misconfigured. Capability enforcement is mechanical (Principle 1).
