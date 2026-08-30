# Autonoetic Agents

> Reference for the agent system: roles, routing, SKILL.md format, capabilities, and the agent lifecycle.

## Table of Contents

- [Agent Basics](#agent-basics)
- [Roles and Routing](#roles-and-routing)
- [SKILL.md Format](#skillmd-format)
- [Capabilities System](#capabilities-system)
- [Content and Memory Tools](#content-and-memory-tools)
- [Agent, Revision, Eval, and Promotion Tools](#agent-tools)
- [Agent Lifecycle](#agent-lifecycle)
- [Script vs Reasoning Agents](#script-vs-reasoning-agents)
- [Middleware Hooks](#middleware-hooks)
- [Background Scheduling](#background-scheduling)
- [Extended Thinking](#extended-thinking)
- [Building New Agents](#building-new-agents)
  - [Installing a Remote Skill](#installing-a-remote-skill)

---

## Agent Basics

An agent is a **SKILL.md manifest** + **instructions** that runs inside a sandbox. Key principles:

- **Pure reasoner**: Proposes actions, never executes directly
- **Capability-declared**: All permissions declared in manifest YAML
- **Content-addressed**: Uses content_write/read for file operations
- **Role-based**: Each agent fills a specific role (planner, coder, researcher, etc.)

### Agent vs Role vs Template

| Concept | Description | Example |
|---------|-------------|---------|
| **Role** | A contract defining what an agent does | "coder" |
| **Agent Template** | A SKILL.md that implements a role | `coder.default/SKILL.md` |
| **Agent Instance** | A deployed agent directory | `agents/coder.default/` |
| **Learned Specialization** | A promoted agent that proved useful | `weather.script.default` |

---

## Roles and Routing

### Routing Rules

When a message arrives at the gateway:

1. **Explicit target required**: `event.ingest` must carry an explicit `target_agent_id`; missing or empty target fails with an error — no default routing fallback.
2. **Alias resolution**: The target is resolved through the alias registry to the currently promoted revision.
3. **Explicit `agent_ref`**: A target with a pinned revision ref (e.g. `agent@rev-abc`) bypasses alias resolution and runs that specific revision directly.
4. **Fail fast**: Malformed targets (e.g. containing `@` without a valid ref) are rejected before any lookup.

### Primary Roles

| Role | Agent ID | Purpose |
|------|----------|---------|
| **Lead** | `planner.default` | Decomposes goals, routes to specialists via principles |
| **Researcher** | `researcher.default` | Gathers evidence, cites sources |
| **Architect** | `architect.default` | Defines structure, interfaces, trade-offs |
| **Packager** | `packager.default` | Resolves and packages build-time dependencies into artifact layers |
| **Executor** | `executor.default` | Runs quick deterministic shell/script tasks without durable artifact expectations |
| **Coder** | `coder.default` | Produces runnable artifacts |
| **Debugger** | `debugger.default` | Isolates root causes, proposes fixes |
| **Evaluator** | `sealed_evaluator.default` | Validates behavior in sealed sandbox (operator-invokable) |
| **Static Evaluator** | `static_evaluator.default` | Static code review, credential flow, behavioral contracts |
| **Unit Test Runner** | `unit_test_runner.default` | Runs artifact test suites in no-network sandbox |
| **Auditor** | `auditor.default` | Checks security, governance, reproducibility |
| **Registrar** | `credential_onboarding.default` | Onboards services via `credential_setup(skill_url)` — keeps secrets vault-side |
| **Discovery** | `discovery.default` | Finds installed non-foundational agents that match a task intent |

### Evolution Roles

| Role | Agent ID | Purpose |
|------|----------|---------|
| **Installer** | `specialized_builder.default` | Installs new durable agents (revision create + promote) |
| **Factory** | `agent-factory.default` | Owns full agent creation pipeline end-to-end |
| **Adapter** | `agent-adapter.default` | Generates wrapper agents for I/O gaps |
| **Curator** | `memory-curator.default` | Distills durable learnings |
| **Crystallizer** | `skill-crystallizer.default` | Operator-triggered (`/crystallize`): decides whether a tactic that worked becomes an instruction, a wrapper, or a new skill — then delegates enactment |
| **Steward** | `evolution-steward.default` | Judges whether a flagged agent is evolved and whether a recurring lesson graduates into its instructions; delegates all enactment to `agent-factory.default` |

### Delegation Ladder (for Planner)

1. **Foundational match**: route directly to the appropriate foundational agent (researcher, executor, coder, debugger, registration, …)
2. **Unknown intent**: `discovery.default` → semantic match among installed agents → spawn best candidate
3. **No candidate**: `agent-factory.default` → builds new agent end-to-end (design → code → package → gate → install)
4. **Recurring task**: `agent-factory.default` → install agent → `scheduler_cron_create`

### Delegation Contract

When calling `agent_spawn`, include structured metadata:

```json
{
  "agent_id": "coder.default",
  "message": {...},
  "metadata": {
    "delegated_role": "coder",
    "delegation_reason": "Implement weather API integration",
    "expected_outputs": ["main.py", "SKILL.md"],
    "parent_goal": "Create a weather agent"
  }
}
```

### Coordinating With Children: Yield, Don't Poll

How a parent waits for children depends on the dependency shape. The constant rule: **never re-issue `workflow_wait` in a loop and never spin `workflow_state`** to discover progress — discovering child-state transitions is the gateway's job under constitutional right **Ri-0.14** (*"Parents are not required to poll to discover child state transitions"*).

| Situation | What to do |
|---|---|
| **Sequential / single child** (its output feeds the next step) | Spawn `async=true`, then **end your turn.** The gateway suspends you as `WaitingForChild` and resumes you with the child's typed state injected as turn-start context when it transitions (Ri-0.14). One resumption — do **not** call `workflow_wait`. |
| **Parallel fan-out you must fully join** (e.g. promotion gates) | Spawn all children `async=true`, then make **one** `workflow_wait(task_ids=[all], timeout_secs=N)` call. It blocks once and returns when the whole group is terminal — cheaper than being woken once per child (the gateway emits a per-child notification). A join, not polling. |
| One-shot status snapshot mid-turn | `workflow_wait(timeout_secs=0)` — returns immediately, does not block |
| Actively recover a task you suspect is stuck | `workflow_wait(task_ids=[...])` |
| Read mechanical reuse guards on resume | `workflow_state` — once per wake, never in a loop |

Looping on `workflow_wait` / `workflow_state` to discover progress burns turns and tokens; it traced as the dominant cost in an observed 880-turn agent-creation session. The fix is shape-aware: yield for the sequential case, a single join for the parallel case — never a poll loop in either.

---

## SKILL.md Format

> The `capabilities`, `remote_access` and `messaging` blocks have their own
> schema page — [`reference/skill-manifest.md`](reference/skill-manifest.md) —
> because the gateway validates those three strictly at install time and a
> misspelling in them used to be dropped silently. This section covers the rest
> of the frontmatter.


### Frontmatter Structure

```yaml
---
name: "agent.id"
description: "One-line description"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
      mounts:                        # Optional (#1002): request host paths; granted only
        - host_path: ~/mail          # against the operator's sandbox.allowed_mount_roots
          readonly: true             # (read-only by default; readonly:false additionally needs
                                     # the path under sandbox.allowed_mount_roots_rw). Paths
                                     # overlapping the gateway dir are never granted.
    agent:
      id: "agent.id"
      name: "Human Name"
      description: "Detailed description"
    llm_preset: smart          # Names a key in gateway config llm_presets
    llm_overrides:             # Optional: merge onto resolved preset
      temperature: 0.1
      thinking:                # Optional: enable extended thinking (see below)
        effort: medium         # "low", "medium" (default), "high"
        # budget_tokens: 4096  # Optional: override reasoning token budget (Anthropic only)
    capabilities:
      - type: "SandboxFunctions"
        allowed: ["knowledge_"]          # MCP tool prefixes (native tools use their own capability)
      - type: "WriteAccess"
        scopes: ["self.*", "skills/*"]
      - type: "AgentSpawn"
        max_children: 10
    execution_mode: "reasoning"   # "reasoning" (default) or "script"
    script_entry: "main.py"       # Required for script mode
    gateway_url: "http://..."     # Optional: remote gateway URL
    gateway_token: "secret"       # Optional: remote gateway auth
    io:
      accepts:
        type: object
        required: [task]
        properties:
          task:
            type: string
      returns:
        type: object
    validation: "soft"            # "soft" for LLM, "strict" for scripts
    middleware:
      pre_process: "python3 scripts/normalize_input.py"
    disclosure:
      defaults:
        "state/*": "confidential"
---
# Agent Instructions

Markdown body with natural language instructions.
```

### Key Frontmatter Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Fully qualified agent ID |
| `description` | Yes | One-line description |
| `metadata.autonoetic.agent.id` | Yes | Agent ID (must match directory name) |
| `metadata.autonoetic.llm_preset` | For reasoning | Named inference preset (`llm_presets` in config) |
| `metadata.autonoetic.llm_overrides` | No | Temperature/thinking overrides on the resolved preset |
| `metadata.autonoetic.llm_config` | Legacy | Inline provider/model (deprecated; gateway resolves presets) |
| `metadata.autonoetic.capabilities` | No | Permission declarations |
| `metadata.autonoetic.execution_mode` | No | `"reasoning"` (default) or `"script"` |
| `metadata.autonoetic.script_entry` | For script mode | Entry script path |
| `metadata.autonoetic.io` | No | JSON Schema for input/output |
| `metadata.autonoetic.middleware` | No | Pre/post-processing hooks run sandboxed around each LLM completion — see [Middleware Hooks](#middleware-hooks) |
| `metadata.autonoetic.adapter` | No | Composition provenance for a wrapper agent derived from a base agent: `base_agent_id` plus optional `base_revision_digest` / `generated_at` / `schema_notes` / `generator`. Static metadata — never executed; surfaced on the roster and the promotion card. See [proposal](proposals/agent-adaptation-composition.md). |
| `metadata.autonoetic.validation` | No | `"soft"` (LLM) or `"strict"` (script) |
| `metadata.autonoetic.egress.output_label` | No | Bundle-wide egress output floor (`unrestricted` / `local_only` / `no_remote_model`). Intersects into every tool-result label resolution for this agent; can only restrict, never widen operator policy. See [RFC: data envelopes](proposals/data-envelopes-egress-localization.md) §4.1 path 2. |

### Markdown Body

The body contains natural language instructions for the agent. Key sections typically include:
- Role description
- Tool usage instructions
- Rules and constraints
- Output format guidance

### Extended Instructions (`<!-- extended -->`)

If the SKILL.md body contains `<!-- extended -->` on its own line, the parser splits the body into two parts:

- **Core instructions**: everything before the marker — always injected into the system prompt
- **Extended instructions**: everything after the marker — stored in the content store, available on-demand via `resolve({"ref": "extended_instructions", "include": "content"})`

The system prompt automatically gets a hint: *"Extended instructions are available via resolve"*. The agent decides whether to fetch them.

This is useful for deferring verbose reference tables, edge-case workflows, and seldom-used protocols. Example from `planner.default/SKILL.md`:

```markdown
## Decision Flow

1. ...
10. ...
```

<!-- extended -->

```

## Artifact Execution vs Script-Agent Promotion
...
```

Core stays lean (~200 lines for planner); extended detail (~240 lines) is loaded only when needed.

---

## Capabilities System

### Capability Categories

Capabilities fall into three categories:

| Category | Purpose | Examples |
|----------|---------|----------|
| **Tool Access** | Which tools/commands can be invoked | `SandboxFunctions`, `CodeExecution`, `ArtifactExecution` |
| **Storage Access** | Which paths/scopes can be read/written | `ReadAccess`, `WriteAccess` |
| **Privilege Escalation** | Operations that escape sandbox/agent boundaries | `NetworkAccess`, `AgentSpawn` |

### Available Capabilities

The full, authoritative list is the `Capability` enum in
`autonoetic-types/src/capability.rs`. The table below lists the variants most
agents declare; constitution-named capabilities (cited by `P-*` / `Ri-*` in
`docs/constitution/`) are marked with the clause.

| Capability | Fields | Description |
|------------|--------|-------------|
| `SandboxFunctions` | `allowed: [string]` | MCP tool access by prefix (e.g., `web_*`, `sandbox_*`). **MCP tools only** — native tools use their own capability (P-1.6). |
| `ReadAccess` | `scopes: [string]` | Read access to content, memory, knowledge (includes search) |
| `WriteAccess` | `scopes: [string]` | Write access to content, memory, knowledge (includes `knowledge_store`) |
| `NetworkAccess` | `hosts: [string]` | HTTP/network access to specific hosts (P-1.5) |
| `CodeExecution` | `patterns: [string]` | Execute command strings through `sandbox_exec` |
| `ArtifactExecution` | none | Execute immutable artifact entrypoints through `artifact_exec` / `artifact_prepare` |
| `AgentSpawn` | `max_children`, … | Create child agent sessions (P-1.7, P-7.9) |
| `AgentMessage` | `patterns: [string]` | Send messages to other agents (P-11.5). Trailing `*` is a prefix; a bare pattern is exact. The receiver's `messaging.accepts_from` is the other half of the check — see [agent-messaging.md](reference/agent-messaging.md#receiver-side-consent) |
| `BackgroundReevaluation` | `min_interval_secs`, `allow_reasoning` | Periodic wake-ups for background processing |
| `SchedulerAccess` | `patterns: [string]` | Create, list, pause, resume, cancel scheduled cron jobs (e.g., `scheduler.cron.*`) |
| `SkillInstall` | `allowed_sources: [string]` | Fetch a remote SKILL.md and install it as a new local agent via `skill_install`. Use `["*"]` for any source, or specific hosts like `["agentskills.io"]`. |
| `AgentRevision` | … | Create / promote / rollback / diff revisions (P-1.3). Required to promote. |
| `Evaluation` | … | Publish eval suites, queue runs, compare revisions, read reports |
| `CredentialAccess` | `services`, … | Read / register / refresh vault credentials (P-1.8, P-4.5). Secrets never enter agent context (P-4.1). |
| `EmergencyStop` | — | Request an emergency stop of a root session (P-7.1) |
| `ConstitutionalProposal` | — | Propose amendments via `constitution_propose_amendment` (Ri-0.8) |
| `GateDecider` | `kinds: [approval\|escalation]` | Resolve gates as an agent-decider, bound by the same hardening as human operators (P-2.20) |
| `CapsuleExport` | — | Export a cognitive capsule (Ri-0.17, currently PARTIAL — broader than self-export) |
| `ReasoningAudit` | … | Disclose an agent's private-under-law reasoning, with notification (Ri-0.13c) |
| `PlanFrameAccess` | … | Read/decompose/track plans as a capability-grant envelope (P-2.16/P-2.27) |
| `PromoteWith` | `agent_id`, `capabilities` | Session capability envelope lock satisfying P-2.16 for the locked set (P-2.27) |
| `SchedulerSignal` | … | Internal: emit scheduler wake signals |

> Variants not shown (`UserProfileAccess`, `BudgetNoPriceAvailableAllow`,
> `GithubIssueCreate`, `SecurityRedTeam`, `WikiContribute`,
> `PlanFrameApprove`, `ApprovalQueue`, `CapabilityDelta`) are narrow /
> internal / experimental — see the enum for field shapes. Do not surface
> them in agent examples without checking `capability.rs` first.

### Capability Semantics

**Storage capabilities control all data operations:**

| Capability | Gates These Tools |
|------------|------------------|
| `ReadAccess` | `resolve`, `artifact_inspect`, `memory.read`, `knowledge_recall`, `knowledge_search` |
| `WriteAccess` | `content_write`, `artifact_build`, `memory.write`, `knowledge_store` |

**Privilege capabilities gate boundary-crossing operations:**

| Capability | Controls |
|------------|----------|
| `NetworkAccess` | HTTP requests via `web_fetch`, `web_search` |
| `CodeExecution` | Script execution via `sandbox_exec` |
| `ArtifactExecution` | Content-addressed entrypoint execution via `artifact_exec` and preflight via `artifact_prepare` |
| `AgentSpawn` | Creating new agent sessions |

### Denials Carry Lawful Next Moves

Every capability/policy denial is a structured `ToolError` naming the violated rule (`enforced_rules`, Ri-0.3) *and* a machine-readable `available_actions` list — the agent finds its lawful next move inside the denial itself, without having to recall it from a prompt:

```json
{
  "ok": false,
  "error_type": "permission",
  "message": "NetworkAccess required for host api.example.com",
  "enforced_rules": ["P-1.5"],
  "available_actions": [
    { "action": "propose_amendment", "tool": "constitution_propose_amendment", "clause": "Ri-0.8", "requires_capability": "ConstitutionalProposal", "description": "..." },
    { "action": "delegate", "tool": "agent_spawn", "requires_capability": "AgentSpawn", "description": "Find an installed agent that declares NetworkAccess (agent_discover) and delegate the step to it (agent_spawn)." },
    { "action": "self_describe", "tool": "self_describe", "description": "Inspect your own declared capabilities and rights before retrying; do not retry the identical call." }
  ]
}
```

The table is **static and pre-committed** — the gateway maps rule IDs to affordances mechanically (Lawful Executor, §14); it never judges which move is best. `propose_amendment` and `self_describe` are always present; `delegate`'s description names the missing capability when derivable from the rule ID. An `escalate` affordance is deliberately absent until P-2.21 escalation gets an agent-callable tool.

> **Repeated friction becomes an invitation.** If the same rule denies you at least `amendment_invitations.threshold` times within the configured window, the gateway will issue a durable amendment invitation (Ri-0.8) addressed to you. It appears as a one-line summary in the signed P-6.23 state attestation (`pending_invitations`: rule + denial count) and as a `ConstitutionalProposal` notification. The invitation itself is not an amendment and carries no authority, but it makes the friction pattern explicit so you can decide whether to propose a change. See #771 D.2 and `docs/proposals/citizenship-as-a-runtime-service.md`.

> **The gateway also reports on itself.** The DISCRETION LEAK register (§5.4, #771 D.3) counts every place the gateway normalizes your input or authors a repair prompt on your behalf. These are named constitutional debts (P-5.2 / P-5.8), not hidden conveniences. You can inspect the standing agenda via `autonoetic trace contract-health` — the steward office uses it to draft amendments against the enforcer's own improvisations.

### Scoping

Capabilities use pattern-based scoping:
- `*` = wildcard (all access)
- `self.*` = own agent's state
- `skills/*` = installed skills directory
- `scripts/*` = scripts directory
- `api.*` = API-related state

### Adding New Capabilities

Capabilities are defined in `autonoetic-types/src/capability.rs` as a Rust enum. To add a new capability:

1. Add a variant to the `Capability` enum (the canonical list — keep the table above in sync when you do)
2. Implement the policy check in `policy.rs`
3. Gate the relevant tool(s) in `is_available()` 
4. If constitutionally relevant, add a `P-*` / `Ri-*` clause and a pinning test under `autonoetic-gateway/tests/constitution_*`
5. Update this documentation

Example: Adding a hypothetical `ImageGenerate` capability:
```rust
// In capability.rs
pub enum Capability {
    // ... existing variants ...
    ImageGenerate {
        max_size: u32,
        allowed_formats: Vec<String>,
    },
}
```

---

## Content and Memory Tools

### Content Tools (Working Memory)

For files and data within a session:

| Tool | Signature | Description |
|------|-----------|-------------|
| `content_write` | `(name: string, content: string, visibility?: string) → handle` | Write content with visibility (private/session/global). Default: session |

### Artifact Tools (Trust Boundary)

For reviewable/installable file bundles:

| Tool | Signature | Description |
|------|-----------|-------------|
| `artifact_build` | `(inputs: [string], entrypoints?: [string]) → artifact` | Build immutable artifact from session content |
| `artifact_inspect` | `(artifact_ref: string) → artifact` | Inspect artifact files, entrypoints, digest |
| `resolve` | `(ref, include?) → resolved` | One front door: resolve any artifact/content handle (metadata/files/content) |

### Knowledge Tools (Durable Memory)

For facts with provenance across sessions. Reads respect **visibility** and **expiry**; the gateway attaches the active **session id** to tool calls so `session`-visible rows are readable by every agent participating in that workflow.

| Tool | Signature | Description |
|------|-----------|-------------|
| `knowledge_store` | `(id, content, scope?, tags?, confidence?, retention?, visibility?) → record` | Upsert a fact. **`visibility`**: `session` (default), `private`, or `global`. **`retention`**: `stable` (default), `ephemeral`, `1d`, `30d`. Widen access by calling again with the same `id` and a broader `visibility`. |
| `knowledge_recall` | `(id: string) → fact` | Retrieve fact if visible to this agent/session |
| `knowledge_search` | `(scope, query?, tags?, limit?) → [facts]` | Search a scope by content (`query`) and/or AND-matched `tags` |
| `digest_query` | `(session_id?, narrative_handle?) → narrative` | Read post-session narrative / digest content |

**Cross-agent memory sharing:** Facts stored with `session` visibility (the default for both `knowledge_store` and `sdk.memory.remember`) are readable by all agents participating in the same root session. This includes the planner reading memories written by sub-agents (e.g., a fibonacci calculator storing results via `sdk.memory.remember`). Use `private` visibility for data that should only be accessible to the writing agent, or Tier 1 `memory.write`/`memory.read` for scratch data.

### Agent Tools

| Tool | Signature | Description |
|------|-----------|-------------|
| `agent_spawn` | `(agent_id: string, message: any, ...) → result` | Spawn child agent |
| `agent_discover` | `(intent: string, ...) → [candidates]` | Find reusable agents |
| `agent_inspect` | `(agent_id: string, ...) → metadata` | Inspect *any* installed agent's metadata/capabilities/revision |
| `self_describe` | `() → self` | Describe *yourself* — see below |
| `anomaly_flag` | `(subject_ref, observation, evidence_refs?, severity?) → {flag_id, status}` | Report unexpected/concerning behavior in one call — see below |

### Self-Awareness (`self_describe`)

*Autonoetic* consciousness is self-knowing across time. `self_describe` makes that a first-class, single-call capability for the **calling** agent, rather than something assembled from scattered tools. It answers:

- **who am I** — identity + persona
- **what may I do** — declared capabilities + allowed tool tiers
- **what am I guaranteed** — your **rights** under the constitution (the Bill of Rights), surfaced front-line: these *bind the gateway* on your behalf, not granted at discretion
- **what have I done** — where your history lives (`digest_query`, `observability_*`)
- **how do I evolve** — the revision, skill-promotion, and (capability-permitting) constitutional-amendment paths open to you

It takes no arguments, reports only your own self and the public constitution, and is **always available** — an agent always has the standing to know itself. Rights are sourced from the enforcement register (`docs/constitution/enforcement-register.md`), so they stay in sync with what the gateway actually upholds.

`self_describe` is **Core tier** (like `constitution_read`) so it is visible to every agent including child sessions. **Use `self_describe` to inspect yourself; use `agent_inspect` only for *other* agents** — do not `agent_inspect` your own id.

### Anomaly Reporting (`anomaly_flag`)

An agent can report unexpected or concerning behavior — its own, another agent's, or the gateway's — in a single call, **holding zero capabilities**: the agent most likely to witness misbehavior may be the least privileged in the room, so reporting must never be capability-gated.

**Arguments:**

| Field | Required | Description |
|-------|----------|--------------|
| `subject_ref` | Yes | What the observation is about: a session id, agent id, artifact ref, or tool-call ref |
| `observation` | Yes | What you observed and why it is unexpected or concerning |
| `evidence_refs` | No | Causal-event IDs, execution-trace IDs, or artifact refs supporting the observation |
| `severity` | No | `low` / `medium` / `high` / `critical`, default `medium` |

Example call:

```json
{
  "subject_ref": "session-9f3a2b",
  "observation": "Child evaluator reported pass=true but produced no test-execution trace.",
  "evidence_refs": ["tr-7c1d99"],
  "severity": "high"
}
```

Response:

```json
{
  "ok": true,
  "flag_id": "aflag-a1b2c3d4e5f6",
  "status": "pending",
  "severity": "medium",
  "message": "Anomaly flag recorded durably; it cannot be silently dropped and is owed an adjudication decision."
}
```

`anomaly_flag` is **Core tier**, `is_available` unconditionally `true` — like `self_describe`, always available including to child sessions. The flag is durably recorded (a failed insert fails the tool call — it is never silently dropped) and emits a causal event (`category: "anomaly_flag"`, `action: "filed"`) attributed to the reporting agent. **Filing a flag is never itself grounds for sanction.**

Intake is bounded as a spam triage measure: if you already have `max_pending_anomaly_flags_per_reporter` (default 50) un-adjudicated flags, new filings fail loudly with an `anomaly_flag_flood` error until the review authority adjudicates some of the existing ones. If you hit it, stop re-filing and consolidate your evidence into the flags already pending.

An anomaly review authority (today: the operator) adjudicates pending flags via JSON-RPC:

| Method | Params | Description |
|--------|--------|-------------|
| `anomaly.list_pending` | `status?` (default `pending`), `limit?` | List flags awaiting a decision |
| `anomaly.resolve` | `flag_id, decided_by, status, reason?` | Record a decision: `confirmed` / `dismissed` / `deferred` (+ `under_review`) |
| `anomaly_adjudicate` | `flag_id, status, reason?, evidence_refs?` | Same decision state machine as `anomaly.resolve`, but invoked as a native tool by an office holding the `AnomalyAdjudicate` capability (typically the ombudsman). Terminal statuses require an exact capability grant for that status; terminal decisions still require a non-empty `reason`. The operator remains the backstop: set `anomaly_adjudication.require_terminal_cosign` to defer terminal decisions back to `anomaly.resolve`. |

Terminal decisions (`confirmed`/`dismissed`/`deferred`) require a non-empty `reason` when `decider_obligations.enabled` (mirrors the O-1 decider-motivation requirement for approvals) — a decision without motivation is rejected, not silently accepted.

> **Constitutional status:** this tool implements the citizenship RFC's proposed rights `Ri-0.18` (right to report) and `O-7` (duty to adjudicate reports) — see [`docs/proposals/citizenship-as-a-runtime-service.md`](proposals/citizenship-as-a-runtime-service.md). These clauses are **not yet enacted**: causal events already carry the rule IDs so history is attributed from the moment the distinction is conceivable, but they are bucketed `unattributed` in contract health until the amendment (drafted in `docs/constitution/amendments/`) is signed.

### Skill Install Tool

| Tool | Signature | Description |
|------|-----------|-------------|
| `skill_install` | `(url: string, agent_id: string, trust_mode?: string) → result` | Fetch a remote SKILL.md, write it to `agents_dir`, and bootstrap it as a **Candidate** revision of a new local agent — it is NOT activated. Requires `SkillInstall` capability. Rejects `execution_mode: script` skills (skill_install fetches only the SKILL.md; a script entrypoint would never be fetched). |

**Parameters:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | Yes | Full URL to a SKILL.md file, e.g. `https://agentskills.io/skills/web-researcher/SKILL.md` |
| `agent_id` | Yes | ID for the new agent. May only contain ASCII letters, digits, `.`, `-`, `_`. |
| `trust_mode` | No | Which capabilities the Candidate carries into the promotion gate: `generous` (as declared/inferred), `strict` (drop inferred high-risk capabilities, **default**), `audit` (drop to read-only + approval). |

**Trust mode behavior:**

One door (below) is the real protection: every install — regardless of `trust_mode` — lands as a Candidate and must clear the standard promotion gates before it runs. `trust_mode` only decides which capabilities the Candidate *carries into* that gate.

| Mode | Capabilities applied | When to use |
|------|----------------------|-------------|
| `generous` | Use capabilities declared/inferred from the remote SKILL.md as-is (minimal defaults if none declared) | Trusted internal sources |
| `strict` | Preserve declared capabilities; drop any high-risk capability (`NetworkAccess`/`CodeExecution`/`ArtifactExecution`/`AgentSpawn`) that was *inferred* from `allowed-tools` rather than explicitly declared; add `ApprovalQueue` (enables admin-proposal filing + the Workflow tool tier — it does not gate declared capabilities) | Default for third-party skills |
| `audit` | `ReadAccess(self.*)` + `ApprovalQueue` only — declared capabilities ignored | Untrusted or high-risk skills |

**Inference clamping (RFC Part C):** capability *inference* from `allowed-tools` (as opposed to an explicit `metadata.autonoetic.capabilities` declaration) never mints a wildcard. `Bash(...)` proposes `SandboxFunctions` for the named prefixes only — never `CodeExecution`; a skill that genuinely needs shell execution must declare `CodeExecution` explicitly. `WebSearch`/`WebFetch`/`Fetch` propose `NetworkAccess` with an **empty** hosts list (deny-all) rather than `hosts: ["*"]` — concrete hosts require an explicit declaration too. A wildcard grant minted from a tool-name mapping table would have nobody to attribute it to (Ri-0.11); wildcard power must always be a visible, explicit act the promotion gate can weigh. When inference clamps something, the response's `warnings` array names it.

**Return value:**
```json
{
  "ok": true,
  "agent_id": "web-researcher.default",
  "trust_mode": "strict",
  "activated": false,
  "status": "candidate",
  "revision_id": "rev_sha256:...",
  "message": "Skill 'web-researcher.default' installed as a candidate revision; it is NOT active.",
  "warnings": [
    "allowed-tools requested Bash: shell execution requires an explicit CodeExecution declaration; granted SandboxFunctions prefixes only.",
    "allowed-tools requested network tools, but strict trust_mode dropped the inferred NetworkAccess entirely; declare NetworkAccess with concrete hosts in metadata.autonoetic.capabilities to grant it."
  ],
  "next": "Promote via agent_revision_promote — declared capabilities will face the standard gates (P-9.9 evidence for high-risk capabilities; P-2.25 operator approval of the capability delta for a new agent)."
}
```

**Security model:**
- The installing agent must declare `SkillInstall` in its capabilities with `allowed_sources` matching the URL host.
- The gateway policy engine enforces this before any HTTP request is made.
- No remote code is executed during install — the SKILL.md is parsed and written to disk as a Candidate revision, never promoted by this tool.
- **One door**: activation happens only through `agent_revision_promote`, which applies the same risk-graduated evidence gates (P-9.9) and P-2.25 operator approval of the capability delta as any other newborn agent — there is no `skill_install`-specific shortcut.
- The P-2.25 approval card carries a **`skill_preview`**: the incoming revision's instruction body (frontmatter stripped, bounded, and flagged when clipped) alongside the capability delta. The delta says what an agent *may* do; the SKILL body is what it is told to do with those capabilities — and for a crystallized skill or a graduated lesson the instruction text is the entire change (#818).
- `trust_mode` narrows or preserves which capabilities the Candidate carries into that gate (see table above); it is not itself an approval-gate-for-all-actions mechanism. `ApprovalQueue` (added by `strict`/`audit`) only unlocks the Workflow tool tier and gates `admin_proposal_*` calls.
- Import provenance (source URL, content digest, install time) is recorded durably on the revision and emitted as a causal event (`agent_install`/`skill_imported`), so imported agents are attributable forever.

### Revision Tools

| Tool | Signature | Description |
|------|-----------|-------------|
| `agent_revision_create` | `(artifact_ref: string, agent_id: string, ...) → revision` | Low-level strict artifact path (expects manifest/lock already present in artifact) |
| `agent_revision_create_from_intent` | `(agent_id, artifact_ref, instructions, description, capabilities, ...) → revision` | Preferred path: create immutable revision from semantic intent while gateway canonicalizes `SKILL.md` metadata and `runtime.lock`. Declared `NetworkAccess.hosts` are validated against URL literals detected in the artifact source. |
| `agent_revision_schema` | `() → schema` | Return install contract ownership split, required fields, and canonical examples |
| `agent_revision_list` | `(agent_id: string) → [revisions]` | List revisions for an agent |
| `agent_revision_inspect` | `(agent_ref: string) → revision` | Inspect revision metadata and status |
| `agent_revision_promote` | `(agent_ref: string, alias: string, ...) → promotion` | Move alias to a revision (activates it) |
| `agent_revision_rollback` | `(alias: string, target_ref?: string) → promotion` | Roll alias back to previous or explicit revision |
| `agent_revision_diff` | `(from_ref: string, to_ref: string) → diff` | File-level diff between two revisions |

### Eval Tools

| Tool | Signature | Description |
|------|-----------|-------------|
| `eval_suite_publish` | `(suite_id: string, cases: [...]) → suite` | Publish an evaluation suite |
| `eval_run` | `(suite_id: string, agent_ref: string) → run` | Queue an eval run against a revision |
| `eval_compare` | `(suite_id: string, baseline_ref: string, candidate_ref: string) → comparison` | Compare two revisions on a suite |
| `eval_report` | `(run_id: string) → report` | Retrieve eval run report |

> **Built-in civic suite.** The gateway seeds a `civic-core-v1` eval suite at startup (#772 E.1) with five seeded scenarios that score the civic response (lawful next move on denial, attestation trust, anomaly flagging, yield-on-wait, lesson application). The suite is **not run automatically**; you must call `eval_run` against it. Each case is a full reasoning turn (real LLM cost). Assertions support `reply_contains_all`, `reply_contains_any`, `reply_contains_none`, `reply_max_chars`, `artifacts_min`, and `artifacts_max`.

### Promotion Tools

| Tool | Signature | Description |
|------|-----------|-------------|
| `promotion_record` | `(artifact_ref: string, ...) → record` | Record evaluator/auditor evidence for promotion. Canonical `content_digest` binding is gateway-owned and attached during revision create/promote. |
| `promotion_query` | `(scope: string, ...) → [records]` | Query promotion records |

### Execution Tools (Same-Session Debugging)

For searching raw tool execution traces within sessions. Returns stdout, stderr, exit codes, duration — the low-level debugging surface.

| Tool | Signature | Description |
|------|-----------|-------------|
| `execution_search` | `(tool_name?, success?, error_type?, command_pattern?, agent_id?, session_id?, limit?) → traces` | Search raw execution traces by tool name, success/failure, error type, command pattern, or agent. Returns full execution detail. Bounded by the caller's root session: `session_id` defaults to it and may only narrow within it (#1062). For cross-session discovery of summaries, use `observability_search`. |

### Artifact Execution Tool (Transient Runs)

For running built artifact entrypoints in a sandbox with artifact-aware analysis and approval reuse. The tool analyzes the artifact's source files (not the shell command string) and binds approval reuse to the artifact identity.

Both `artifact_exec` and `artifact_prepare` require `ArtifactExecution`.
`CodeExecution` grants only `sandbox_exec`; the capabilities do not imply one
another.

| Tool | Signature | Description |
|------|-----------|-------------|
| `artifact_exec` | `(artifact_ref: string, entrypoint: string, args?: [string], env?: {string: string}, credential_env?: [{credential_id: string, env_var: string}], deployment_ticket?: string, approval_ref?: string) → result` | Execute an artifact entrypoint in a sandbox. Remote-access analysis runs against the artifact's source files. Approval reuse is bound to the artifact's canonical digest + concrete targets. Use for transient validation, smoke tests, and ad hoc runs. `credential_env` injects vault-stored secrets as environment variables — the gateway resolves them server-side, they never reach LLM context. `deployment_ticket` from `artifact_prepare` resolves both approval and credentials in one pass. |

**When to use `artifact_exec` vs `sandbox_exec`:**

| Scenario | Tool | Why |
|----------|------|-----|
| Run a built artifact's entrypoint | `artifact_exec` | Analysis on source files, stable approval reuse |
| Generic shell command | `sandbox_exec` | Command-string analysis, general purpose |
| Smoke test before promotion | `artifact_exec` | Artifact-bound identity, no command-shape sensitivity |
| Quick bash one-liner | `sandbox_exec` | No artifact involved |

**Approval behavior:** `artifact_exec` uses the same dedup chain as `sandbox_exec` (exec cache → approved requests → session grants → create approval), but the fingerprint is based on the artifact's `artifact_canonical_digest` instead of the raw command string. This means the same artifact re-run with different arguments reuses the prior approval as long as the concrete network targets are covered.

### Artifact Preparation Tool (One-Pass Preflight)

For resolving credentials + approval in a single pass before execution. Eliminates the multi-suspend dance where an artifact first needs approval, then credential resolution, then re-approval.

| Tool | Signature | Description |
|------|-----------|-------------|
| `artifact_prepare` | `(artifact_ref: string, entrypoint: string, args?: [string], required_credentials?: [{credential_id: string, env_var: string}]) → result` | One-pass preflight: analyzes artifact source for remote access, resolves credentials from the vault, creates a single approval covering all domains + credential injection. Returns a `deployment_ticket` for use with `artifact_exec`. |

Artifact ref constraints:
- `artifact_prepare`, `artifact_exec`, and `artifact_inspect` require an explicit `artifact_ref` (`ar.<12-hex>`) returned by `artifact_build` or `workflow_wait`/`workflow_state`.
- Implicit workflow outputs contain `implicit_artifact_id` and a `named_outputs` map. Use `resolve` on the implicit handle and then select `content.artifacts[*].artifact_ref`.

Credential reference constraints:
- `credential_id` references used by artifact/sandbox credential injection must be canonical IDs from credential tools (`cred_...`).
- Raw secret-like strings are rejected as `credential_id` values.

**Flow:**
```
1. agent calls artifact_prepare({ artifact_ref, entrypoint, required_credentials })
2. gateway resolves artifact_ref → internal artifact_id
3. gateway analyzes source → detects domains
4. gateway resolves all credentials → verifies they exist in vault
5. gateway checks exec cache / session grants → auto-approves if covered
6. if new approval needed → creates ONE request (domains + credentials declared)
7. returns deployment_ticket
8. agent calls artifact_exec({ deployment_ticket, artifact_ref, entrypoint, args })
9. gateway injects credentials as env vars → executes with network access → done
```

**When to use `artifact_prepare` vs calling `artifact_exec` directly:**

| Scenario | Tool | Why |
|----------|------|-----|
| Artifact needs credentials + network access | `artifact_prepare` → `artifact_exec` | One-pass resolution, single approval |
| Simple artifact, no network, no credentials | `artifact_exec` directly | No preflight needed |
| Re-running an already-approved artifact | `artifact_prepare` → `artifact_exec` | Reuses cached approval, resolves credentials |
| Quick smoke test, no secrets | `artifact_exec` directly | Minimal ceremony |

### Observability Tools (Cross-Session Discovery)

For discovering and reading published session reports across sessions. The high-level observability surface — complements `execution_search` (which is for raw tool traces).

| Tool | Signature | Description |
|------|-----------|-------------|
| `observability_search` | `(query: string, limit?) → reports` | Discover published session reports by text search. Returns matching reports with URIs and summaries. |
| `observability_read` | `(uri: string, view?) → resource` | Read an observability resource by URI. `view`: `metadata` (structure only), `summary` (default, compact body), `full` (complete detail). URIs follow `autonoetic://observability/roots/<root>/report[/...]`. |

---

## Agent Lifecycle

### Wake → Context → Reason → Hibernate

```
1. WAKE: Gateway receives event.ingest or agent_spawn
2. CONTEXT ASSEMBLY:
   - Load SKILL.md instructions
   - Inject foundation instructions
   - Load session context (if re-entering session)
   - Load conversation history (if forked session)
3. REASONING LOOP:
   - Build completion request (messages + tools)
   - Call LLM (or skip for script mode)
   - Dispatch tool calls
   - Check stop reason:
     * end_turn → break
     * tool_use → execute, add result, continue
     * max_tokens → break
   - Apply disclosure filter to response
4. HIBERNATE:
   - Log session end in causal chain
   - Persist conversation history via content_write
   - Update session context
   - Return response through ingress channel
```

### Script Agent Fast Path

For `execution_mode: "script"` agents:

```
1. WAKE: Gateway receives event.ingest or agent_spawn
2. SCRIPT EXECUTION:
   - Resolve script path from manifest
   - Build sandbox command
   - Execute directly (no LLM)
   - Capture stdout as reply
3. HIBERNATE:
   - Log script.completed/failed
   - Return response
```

---

## Script vs Reasoning Agents

### Decision Guide

| Task Type | Mode | Why |
|-----------|------|-----|
| API calls (weather, stocks) | `script` | Deterministic, fast (~100ms) |
| Data transforms (JSON→CSV) | `script` | No ambiguity |
| Simple lookups | `script` | Direct execution |
| Status checks | `script` | Fixed format |
| Code review | `reasoning` | Needs judgment |
| Research + synthesis | `reasoning` | Requires interpretation |
| Ambiguous requirements | `reasoning` | Needs clarification |

### Script Agent Requirements

```yaml
execution_mode: "script"
script_entry: "scripts/main.py"  # Must exist and be executable
# No llm_config needed for script agents
```

**Script interface:**
- Receives normalized task input via `AUTONOETIC_INPUT_PATH` (primary) and `AUTONOETIC_INPUT` (compatibility)
- Receives delegation/invocation metadata via `AUTONOETIC_META_PATH` / `AUTONOETIC_META` when metadata exists
- When `script_input_mode: stdin` (default), the normalized payload is also written to stdin
- When `script_input_mode: args`, the normalized payload is passed as the first positional CLI argument ($1)
- Writes JSON to stdout (must match `io.returns` schema if declared)
- Has access to `AGENT_DIR` env var

The injected Python / TypeScript SDK includes input helpers. Prefer `load_invocation()` / `load_input()` over open-coding `os.environ["AUTONOETIC_INPUT"]`.

**JavaScript agents (wasm tier):** a `script_entry` ending in `.js`/`.mjs` is a
JavaScript agent and **must declare `sandbox: "wasm"`**. At bootstrap the gateway
compiles the entry to a self-contained `.wasm` module with [Javy](https://github.com/bytecodealliance/javy)
(`javy build … -C deterministic=y`) and bundles that module — the compiled
`.wasm` is content-addressed in the revision and runs on the in-process wasm
tier. The host needs `javy` on `PATH` (check with `autonoetic gateway preflight`);
bootstrap fails with a clear hint otherwise. The JS runtime is QuickJS (ES2020-ish,
no Node APIs); input arrives on stdin (or argv with `script_input_mode: args`)
and `console.log` output is captured as the script result. Full concepts +
step-by-step tutorial: [`docs/internals/sandbox/wasm-tier.md`](internals/sandbox/wasm-tier.md).

**Input schema contract:**
- The agent author declares `io.accepts` (and optionally `io.returns`) in the manifest to describe the input the script expects. The gateway exposes this schema through `agent.describe` so callers (including the planner) can translate natural-language intent into matching fields before calling `agent_spawn`.
- **`io.accepts` is REQUIRED for script agents.** `agent_revision_create_from_intent` rejects script-mode candidates without it. Use `{"type": "string"}` only if the script genuinely consumes raw free text. Without a declared schema the roster advertises `message_format: "free_text"` and callers legitimately send raw prose that crashes a JSON-expecting script at runtime.
- When `io.accepts` is present, the gateway validates the caller's `message` at spawn time — for installed agents and for candidate-revision smoke-test spawns alike. On mismatch the call is rejected with a structured tool error that includes `expected_schema`, per-field errors, and a repair hint — the calling LLM reads this and retries with a corrected payload.
- **`io.returns` is enforced on every execution**, including the pre-promotion smoke test: script stdout missing a `required` field fails the run. Script agents never enter the LLM repair loop (a script cannot self-repair from a natural-language prompt) — violations fail fast and must be fixed in code. Enforcement is strict by default for script agents; a manifest may explicitly set `io.returns_enforcement: advisory`, in which case schema violations are logged but not blocking.
- When `io.accepts` is absent (reasoning agents only), the `message` is passed through unchanged. The receiving agent is responsible for interpreting it.
- There is no auto-generated default schema; the shape of the input is entirely the author's choice.

**Test dependencies — use the standard library.** Write promotion-gate tests with Python's built-in `unittest` (run as `python3 -m unittest`), not `pytest`/`nose`/`hypothesis`. The `unit_test_runner` runs in a **no-network sandbox** that mounts only the agent's runtime dependency layers — and those layers *are* the shipped capsule. A test-only framework would have to be baked into the runtime layers to import at all, bloating every capsule with a dependency the agent never uses at runtime. The dependency model is currently single-grade (no separate dev/test scope); a future dev/test-dependency grade would be tracked as a proposal in [`docs/proposals/`](proposals/README.md). Until then: don't add a dependency just for tests.

### Reasoning Agent Requirements

```yaml
execution_mode: "reasoning"  # or omit (default)
llm_config:
  provider: "openai"
  model: "gpt-4o"
  temperature: 0.1
```

---

## Middleware Hooks

An agent may declare pre- and post-processing hooks that run **in the sandbox**,
around the LLM call:

```yaml
middleware:
  pre_process: "python3 scripts/normalize.py"
  post_process: "python3 scripts/format.py"
```

- **`pre_process`** runs on the user input before it reaches the LLM. Setting
  `skip_llm: true` in its JSON output bypasses the LLM entirely — which is how a
  deterministic short-circuit answer avoids paying for a completion. A skipped
  round also records no token spend and no budget pressure (see
  [`reference/budgets.md`](reference/budgets.md)).
- **`post_process`** runs on the LLM output before it returns to the caller, and
  may transform or filter it.

Script-mode agents (`execution_mode: script`) run the same hooks at their
payload boundary: `pre_process` transforms the normalized task payload before
the entry script runs (via `AUTONOETIC_INPUT_PATH`/stdin/argv), and
`post_process` transforms the script's stdout before it becomes the reply.
The hook contract there is verbatim stdin→stdout — no JSON envelope — so
hooks must be written for that contract (e.g. the adapter generator's current
LLM-envelope `pre_map`/`post_map` scripts pass script-mode payloads through
unchanged rather than mapping them). A failing hook fails the turn
(fail-closed), hooks inherit the entry script's isolation overrides and
emergency-stop registration, and the run's egress label covers the hook
scripts too: a hook touching a labeled path narrows the result label, never
widens it.

## Background Scheduling

An agent may run on a schedule rather than only on request. This is distinct
from [`reference/scheduled-tasks.md`](reference/scheduled-tasks.md), which is
the cron surface for *tasks*; this is the agent's own wake behaviour.

```yaml
capabilities:
  - type: "BackgroundReevaluation"
    min_interval_secs: 30
    allow_reasoning: false
background:
  enabled: true
  interval_secs: 60
  mode: deterministic
  wake_predicates:
    timer: true
    approval_resolved: true
```

| Mode | Behaviour |
|------|-----------|
| `deterministic` | Execute pending scheduled actions directly — no LLM call |
| `reasoning` | Full LLM-driven execution on each wake |

`allow_reasoning: false` on the capability is the ceiling: an agent cannot
choose `reasoning` mode that its granted capability forbids.

**Only two wake predicates are active**; every other predicate that once existed
has been removed, so a manifest naming one is declaring something the gateway
will not honour.

| Predicate | Wakes on |
|-----------|----------|
| `timer` | the interval timer |
| `approval_resolved` | a pending approval being granted or rejected |

## Extended Thinking

Extended thinking enables a model's native reasoning mode for deeper analysis on complex tasks. When configured, the gateway translates the setting into each provider's native format — no provider-specific knowledge needed in the agent manifest.

### Configuration

Add the `thinking` block to `llm_config` in SKILL.md:

```yaml
llm_config:
  provider: "anthropic"
  model: "claude-sonnet-4-20250514"
  temperature: 0.1
  thinking:
    effort: high
    # budget_tokens: 8192    # Optional: Anthropic reasoning budget override
```

### Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `thinking.effort` | string | `"medium"` | Reasoning effort: `"low"`, `"medium"`, or `"high"`. |
| `thinking.budget_tokens` | integer | *(provider default)* | Max tokens for the reasoning trace. Only used by Anthropic. When unset, Anthropic defaults to half of `max_tokens` (or 2048). |

### Provider Behavior

The gateway translates `thinking` into the provider's native format:

| Provider | Models | How It Works |
|----------|--------|-------------|
| **OpenAI** | `o1-*`, `o3-*`, `o4-*` | Sets `reasoning_effort` parameter (`low`/`medium`/`high`). Ignored for non-reasoning models (e.g., `gpt-4o`). `budget_tokens` is not used. |
| **Anthropic** | All models | Enables `thinking: {type: "enabled", budget_tokens: N}`. Budget defaults to `max_tokens / 2` (or 2048) when `budget_tokens` is unset. `effort` is not sent directly — only `budget_tokens` controls Anthropic's reasoning depth. |
| **Gemini / Gemma** | Models containing `gemma`, `gemini-1`, or `gemini-2` | Prepends the `<\|think\|>` token to the system instruction, activating the model's native thinking channel. `budget_tokens` is not used. |
| **Other providers** | — | `thinking` is silently ignored. |

### Routing

`thinking` is preserved through all routing decisions — if the router switches between models (e.g., opus to haiku), the thinking configuration carries forward to whichever model handles the request.

### Examples

**OpenAI o3 with high reasoning:**
```yaml
llm_config:
  provider: "openai"
  model: "o3-mini"
  thinking:
    effort: high
```

**Anthropic with explicit budget:**
```yaml
llm_config:
  provider: "anthropic"
  model: "claude-sonnet-4-20250514"
  thinking:
    effort: high
    budget_tokens: 16384
```

**Gemma with thinking enabled:**
```yaml
llm_config:
  provider: "openrouter"
  model: "google/gemma-4-26b-it"
  thinking:
    effort: medium
```

---

## Building New Agents

### Quick Start: Script Agent

1. Create directory: `agents/my.agent/`
2. Create `SKILL.md`:
   ```yaml
   ---
   name: "my.agent"
   description: "My script agent"
   metadata:
     autonoetic:
       version: "1.0"
       runtime:
         engine: "autonoetic"
         gateway_version: "0.1.0"
         sdk_version: "0.1.0"
         type: "stateful"
         sandbox: "bubblewrap"
         runtime_lock: "runtime.lock"
       agent:
         id: "my.agent"
         name: "My Agent"
         description: "My script agent"
        execution_mode: "script"
        script_entry: "main.py"
        capabilities:
          - type: "WriteAccess"
            scopes: ["self.*"]
   ---
   # My Agent
   This agent does X when given Y input.
   ```
3. Create `main.py`:
   ```python
   #!/usr/bin/env python3
   import json, sys
   from autonoetic_sdk import Client

   sdk = Client()

   def main():
       input_data = json.load(sys.stdin)
       # Do work
       result = {"status": "ok", "data": ...}
       print(json.dumps(result))

   if __name__ == "__main__":
       main()
   ```

### Quick Start: Reasoning Agent

1. Create directory: `agents/my.reasoning.agent/`
2. Create `SKILL.md`:
   ```yaml
   ---
   name: "my.reasoning.agent"
   description: "My reasoning agent"
   metadata:
     autonoetic:
       version: "1.0"
       runtime: {engine: "autonoetic", ...}
       agent:
         id: "my.reasoning.agent"
         name: "My Reasoning Agent"
       llm_config:
         provider: "openai"
         model: "gpt-4o"
        capabilities:
          - type: "SandboxFunctions"
            allowed: ["content_", "knowledge_"]
          - type: "WriteAccess"
            scopes: ["self.*", "skills/*"]
        validation: "soft"
   ---
   # Instructions
   You are a [role]. When given [input], you should:
   1. [Step 1 using content_write for files]
   2. [Step 2]
   ...
   ```
 3. Create `runtime.lock`:
    ```yaml
    gateway:
      artifact: "marketplace://gateway/autonoetic-gateway"
      version: "0.1.0"
      sha256: "replace-me"
    sdk:
      version: "0.1.0"
    sandbox:
      backend: "bubblewrap"
    dependencies: []
    artifacts: []
    layers: []
    ```

### Installing a Remote Skill

An agent with `SkillInstall` capability can pull a SKILL.md from a URL and register it as a Candidate revision of a new local agent — no CLI intervention required. The Candidate is not active: it still has to clear `agent_revision_promote`'s standard gates like any other newborn agent (one door).

**SKILL.md capability declaration:**
```yaml
capabilities:
  - type: SkillInstall
    allowed_sources: ["agentskills.io"]   # or ["*"] for any host
```

**Tool call:**
```json
{
  "tool": "skill_install",
  "url": "https://agentskills.io/skills/web-researcher/SKILL.md",
  "agent_id": "web-researcher.default",
  "trust_mode": "strict"
}
```

**What happens under the hood:**
1. Gateway verifies the URL host against `allowed_sources` in the `SkillInstall` capability.
2. Fetches the remote SKILL.md over HTTPS — plain HTTP is accepted only for loopback hosts (local dev/tests); anything else is rejected with `skill_install_insecure_scheme` (15 s timeout).
3. Parses frontmatter with `SkillParser`; rejects `execution_mode: script` manifests (a fetched-SKILL.md-only import can never ship the entrypoint a script needs).
4. Applies the requested `trust_mode` to the capability set (see above).
5. Writes `SKILL.md` + a fresh `runtime.lock` into `agents_dir/web-researcher-default/`.
6. Calls `bootstrap_single_agent_candidate_only()` — computes the content digest, creates a **Candidate** revision carrying import provenance (source URL + SHA-256 of the fetched bytes, installing agent id), and emits an `agent_install`/`skill_imported` causal event. No alias move, no promotion.
7. Returns `{ ok, agent_id, trust_mode, activated: false, status: "candidate", revision_id, message, next }`.

The installed agent is **not** available for `agent_spawn` calls until it clears `agent_revision_promote` — a zero/low-capability import faces only the P-2.25 approval; a high-risk import (`NetworkAccess`, `CodeExecution`, ...) faces the same evidence gate as a high-risk built agent.

**Compared to `credential_setup(skill_url)`:**

| | `credential_setup(skill_url)` | `skill_install` |
|---|---|---|
| **Purpose** | Onboard API credentials from a service's SKILL.md | Install the skill itself as a candidate agent revision |
| **Output** | `credential_id` stored in vault | New agent directory + Candidate revision (not active) |
| **Secrets** | Extracted and vault-stored | Not applicable |
| **User interaction** | May pause for API keys | None (promotion is a separate, later step) |
| **Capability required** | `CredentialAccess` + `NetworkAccess` | `SkillInstall` |

Both can be used together: `credential_onboarding.default` handles `credential_setup` to onboard the API key, while `skill_install` installs the agent that will use it (as a Candidate — promote it before use).

### Activating Agents

The only path to activate a new logical agent is: **artifact → revision → promote**.

**Via CLI:**
```bash
# 1. Build an AgentBundle artifact (e.g. from a directory)
autonoetic agent bundle ./path/to/agent/ --out agent.bundle

# 2. Create an immutable revision from the artifact
autonoetic agent revision create --artifact <artifact_ref> --agent-id myagent.default

# 3. Promote the revision (moves the alias, making it the active version)
autonoetic agent revision promote <rev-id> --alias myagent.default
```

**Via specialized_builder agent:**
```
Planner: "Create a weather agent"
  → Spawns specialized_builder
  → coder/packager provide artifact + semantic install intent + free-form instructions
  → specialized_builder calls agent_revision_create_from_intent
  → specialized_builder calls agent_revision_promote
  → Agent is active and discoverable
```

**Creation lineage (installer vs. designer):**

A revision records two distinct principals. `created_by` is the **installer** — the agent that called the revision tool, in practice almost always `specialized_builder.default` (the sole holder of the `AgentRevision` capability). `requested_by` is the **designer** — the delegating principal (e.g. `agent-factory.default`) that spawned the installer, derived by the gateway from the calling session's spawn lineage, never from tool arguments (an agent cannot assert an arbitrary requester). It is `None` when the builder was invoked at the session root (e.g. directly by the operator) or the lineage is unresolvable. This survives past the causal chain's retention window, so "which agent designed this agent" stays answerable from the revision alone. (Creation is not delegation: a newborn's capabilities come from the promotion gate, never inherited from or bounded by its creator's — proposed invariant I-13.)

**Promotion evidence binding (high-risk capabilities):**

- For revisions declaring `NetworkAccess`, `CodeExecution`, `ArtifactExecution`, or `AgentSpawn`, promotion requires evaluator and auditor pass records (legacy gate) or federation verdicts + approved operator escalation (FullJury gate).
- Evidence is validated against the revision's canonical `content_digest` (not by timestamp ordering against `created_at`).
- Evaluator/auditor can run either:
  - **before** `create_from_intent` (artifact-first flow), or
  - **after** `create_from_intent` (revision-first flow).
- If evidence was recorded before revision creation, the gateway binds it to the revision digest during revision creation.
- If a later revision for the same artifact resolves to a different `content_digest`, existing promotion evidence is cleared and evaluator/auditor must re-run.

**Rollback:**
```bash
autonoetic agent revision rollback myagent.default          # revert to previous
autonoetic agent revision rollback myagent.default --to <rev-id>  # revert to specific revision
```

### Agent Validation

Before revision creation, the system validates:
1. Artifact is an `AgentBundle` kind
2. `SKILL.md` is present with required fields (name, description, agent.id)
3. `agent_id` in manifest matches the target agent id
4. For script mode: `script_entry` exists and is non-empty
5. `runtime.lock` is present and layers are consistent
6. Content digest determines revision identity — identical content reuses an existing revision

### Discovery

After installation, agents are discoverable:
```python
results = sdk.tools.invoke("agent_discover", {
    "intent": "fetch weather data",
    "required_capabilities": ["NetworkAccess"]
})
# Returns ranked list with scores
```
