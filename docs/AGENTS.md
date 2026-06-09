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
| **Registrar** | `registration.default` | Onboards services via `credential_setup(skill_url)` — keeps secrets vault-side |
| **Discovery** | `discovery.default` | Finds installed non-foundational agents that match a task intent |

### Evolution Roles

| Role | Agent ID | Purpose |
|------|----------|---------|
| **Installer** | `specialized_builder.default` | Installs new durable agents (revision create + promote) |
| **Factory** | `agent-factory.default` | Owns full agent creation pipeline end-to-end |
| **Adapter** | `agent-adapter.default` | Generates wrapper agents for I/O gaps |
| **Curator** | `memory-curator.default` | Distills durable learnings |
| **Steward** | `evolution-steward.default` | Decides skill promotion |

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
      - type: "ToolInvoke"
        allowed: ["content.", "knowledge.", "agent."]
      - type: "MemoryWrite"
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
| `metadata.autonoetic.validation` | No | `"soft"` (LLM) or `"strict"` (script) |

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
| **Tool Access** | Which tools/commands can be invoked | `SandboxFunctions`, `CodeExecution` |
| **Storage Access** | Which paths/scopes can be read/written | `ReadAccess`, `WriteAccess` |
| **Privilege Escalation** | Operations that escape sandbox/agent boundaries | `NetworkAccess`, `AgentSpawn` |

### Available Capabilities

| Capability | Fields | Description |
|------------|--------|-------------|
| `SandboxFunctions` | `allowed: [string]` | MCP tool access by prefix (e.g., `web.*`, `sandbox.*`) |
| `ReadAccess` | `scopes: [string]` | Read access to content, memory, knowledge (includes search) |
| `WriteAccess` | `scopes: [string]` | Write access to content, memory, knowledge (includes `knowledge_store`) |
| `NetworkAccess` | `hosts: [string]` | HTTP/network access to specific hosts |
| `CodeExecution` | `patterns: [string]` | Execute scripts/commands in sandbox |
| `AgentSpawn` | `max_children: number` | Create child agent sessions |
| `AgentMessage` | `patterns: [string]` | Send messages to other agents |
| `BackgroundReevaluation` | `min_interval_secs: number, allow_reasoning: boolean` | Periodic wake-ups for background processing |
| `SchedulerAccess` | `patterns: [string]` | Create, list, pause, resume, cancel scheduled cron jobs (e.g., `scheduler.cron.*`) |
| `SkillInstall` | `allowed_sources: [string]` | Fetch a remote SKILL.md and install it as a new local agent via `skill_install`. Use `["*"]` for any source, or specific hosts like `["agentskills.io"]`. |

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
| `AgentSpawn` | Creating new agent sessions |

### Scoping

Capabilities use pattern-based scoping:
- `*` = wildcard (all access)
- `self.*` = own agent's state
- `skills/*` = installed skills directory
- `scripts/*` = scripts directory
- `api.*` = API-related state

### Adding New Capabilities

Capabilities are defined in `autonoetic-types/src/capability.rs` as a Rust enum. To add a new capability:

1. Add a variant to the `Capability` enum
2. Implement the policy check in `policy.rs`
3. Gate the relevant tool(s) in `is_available()` 
4. Update this documentation

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

### Self-Awareness (`self_describe`)

*Autonoetic* consciousness is self-knowing across time. `self_describe` makes that a first-class, single-call capability for the **calling** agent, rather than something assembled from scattered tools. It answers:

- **who am I** — identity + persona
- **what may I do** — declared capabilities + allowed tool tiers
- **what am I guaranteed** — your **rights** under the constitution (the Bill of Rights), surfaced front-line: these *bind the gateway* on your behalf, not granted at discretion
- **what have I done** — where your history lives (`digest_query`, `observability_*`)
- **how do I evolve** — the revision, skill-promotion, and (capability-permitting) constitutional-amendment paths open to you

It takes no arguments, reports only your own self and the public constitution, and is **always available** — an agent always has the standing to know itself. Rights are sourced from the enforcement register (`docs/constitution/enforcement-register.md`), so they stay in sync with what the gateway actually upholds.

`self_describe` is **Core tier** (like `constitution_read`) so it is visible to every agent including child sessions. **Use `self_describe` to inspect yourself; use `agent_inspect` only for *other* agents** — do not `agent_inspect` your own id.

### Skill Install Tool

| Tool | Signature | Description |
|------|-----------|-------------|
| `skill_install` | `(url: string, agent_id: string, trust_mode?: string) → result` | Fetch a remote SKILL.md, write it to `agents_dir`, and immediately bootstrap + promote it as a new local agent. Requires `SkillInstall` capability. |

**Parameters:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `url` | Yes | Full URL to a SKILL.md file, e.g. `https://agentskills.io/skills/web-researcher/SKILL.md` |
| `agent_id` | Yes | ID for the new agent. May only contain ASCII letters, digits, `.`, `-`, `_`. |
| `trust_mode` | No | How to treat imported capabilities: `generous` (keep as-is), `strict` (add approval gate, **default**), `audit` (drop to read-only + approval). |

**Trust mode behavior:**

| Mode | Capabilities applied | When to use |
|------|----------------------|-------------|
| `generous` | Use capabilities declared in remote SKILL.md (minimal defaults if none declared) | Trusted internal sources |
| `strict` | Preserve declared capabilities + add `ApprovalQueue` for all actions | Default for third-party skills |
| `audit` | `ReadAccess(self.*)` + `ApprovalQueue` only — declared capabilities ignored | Untrusted or high-risk skills |

**Return value:**
```json
{
  "ok": true,
  "agent_id": "web-researcher.default",
  "trust_mode": "strict",
  "activated": true,
  "message": "Skill installed and promoted as agent 'web-researcher.default'"
}
```

**Security model:**
- The installing agent must declare `SkillInstall` in its capabilities with `allowed_sources` matching the URL host.
- The gateway policy engine enforces this before any HTTP request is made.
- No remote code is executed during install — the SKILL.md is parsed and written to disk, then bootstrapped like any local agent.
- `strict` mode (the default) ensures the new agent cannot take any privileged action without an approval gate, limiting blast radius from untrusted skills.

### Revision Tools

| Tool | Signature | Description |
|------|-----------|-------------|
| `agent_revision_create` | `(artifact_ref: string, agent_id: string, ...) → revision` | Low-level strict artifact path (expects manifest/lock already present in artifact) |
| `agent_revision_create_from_intent` | `(agent_id, artifact_ref, instructions, description, capabilities, ...) → revision` | Preferred path: create immutable revision from semantic intent while gateway canonicalizes `SKILL.md` metadata and `runtime.lock` |
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

### Promotion Tools

| Tool | Signature | Description |
|------|-----------|-------------|
| `promotion_record` | `(artifact_ref: string, ...) → record` | Record evaluator/auditor evidence for promotion. Canonical `content_digest` binding is gateway-owned and attached during revision create/promote. |
| `promotion_query` | `(scope: string, ...) → [records]` | Query promotion records |

### Execution Tools (Same-Session Debugging)

For searching raw tool execution traces within sessions. Returns stdout, stderr, exit codes, duration — the low-level debugging surface.

| Tool | Signature | Description |
|------|-----------|-------------|
| `execution_search` | `(tool_name?, success?, error_type?, command_pattern?, agent_id?, session_id?, limit?) → traces` | Search raw execution traces by tool name, success/failure, error type, command pattern, or agent. Returns full execution detail. For cross-session discovery of summaries, use `observability_search`. |

### Artifact Execution Tool (Transient Runs)

For running built artifact entrypoints in a sandbox with artifact-aware analysis and approval reuse. The tool analyzes the artifact's source files (not the shell command string) and binds approval reuse to the artifact identity.

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

**Input schema contract:**
- The agent author declares `io.accepts` (and optionally `io.returns`) in the manifest to describe the input the script expects. The gateway exposes this schema through `agent.describe` so callers (including the planner) can translate natural-language intent into matching fields before calling `agent_spawn`.
- When `io.accepts` is present, the gateway validates the caller's `message` at spawn time. On mismatch the call is rejected with a structured tool error that includes `expected_schema`, per-field errors, and a repair hint — the calling LLM reads this and retries with a corrected payload.
- When `io.accepts` is absent, the `message` is passed through unchanged. The author is responsible for parsing it inside the script.
- There is no auto-generated default schema; the shape of the input is entirely the author's choice.

### Reasoning Agent Requirements

```yaml
execution_mode: "reasoning"  # or omit (default)
llm_config:
  provider: "openai"
  model: "gpt-4o"
  temperature: 0.1
```

---

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
         - type: "MemoryWrite"
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
         - type: "ToolInvoke"
           allowed: ["content.", "knowledge."]
         - type: "MemoryWrite"
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

An agent with `SkillInstall` capability can pull a SKILL.md from a URL and register it as a live local agent in a single step — no CLI intervention required.

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
2. Fetches the remote SKILL.md over HTTPS (15 s timeout).
3. Parses frontmatter with `SkillParser`; applies the requested `trust_mode` to the capability set.
4. Writes `SKILL.md` + a fresh `runtime.lock` into `agents_dir/web-researcher-default/`.
5. Calls `bootstrap_single_agent()` — computes the content digest, creates a revision, auto-promotes to Active.
6. Returns `{ ok, agent_id, trust_mode, activated }`.

The installed agent is immediately available for `agent_spawn` calls. No separate `autonoetic agent bootstrap` step is needed.

**Compared to `credential_setup(skill_url)`:**

| | `credential_setup(skill_url)` | `skill_install` |
|---|---|---|
| **Purpose** | Onboard API credentials from a service's SKILL.md | Install the skill itself as a runnable agent |
| **Output** | `credential_id` stored in vault | New agent directory + active revision |
| **Secrets** | Extracted and vault-stored | Not applicable |
| **User interaction** | May pause for API keys | None |
| **Capability required** | `CredentialAccess` + `NetworkAccess` | `SkillInstall` |

Both can be used together: `registration.default` handles `credential_setup` to onboard the API key, while `skill_install` installs the agent that will use it.

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

**Promotion evidence binding (high-risk capabilities):**

- For revisions declaring `NetworkAccess`, `CodeExecution`, or `AgentSpawn`, promotion requires evaluator and auditor pass records (legacy gate) or federation verdicts + approved operator escalation (FullJury gate).
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
    "required_capabilities": ["NetConnect"]
})
# Returns ranked list with scores
```
