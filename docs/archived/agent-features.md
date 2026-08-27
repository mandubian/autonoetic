> **Archived — merged.** Its unique material (middleware hooks, background
> scheduling) is now in [`AGENTS.md`](../AGENTS.md); the rest duplicated
> `AGENTS.md` (manifest, capabilities), `ARCHITECTURE.md` (execution modes,
> I/O schemas, memory tiers, disclosure policy) and
> [`internals/gateway.md`](../internals/gateway.md). Kept as the design
> record; not source of truth.

# Agent Features Reference

> **⚠ PARTIALLY SUPERSEDED (2026-07-11).** The canonical reference for
> agent manifest fields, capabilities, I/O schemas, execution modes, and
> the agent lifecycle is now `docs/AGENTS.md`. This doc is kept because it
> still carries more detail on a few narrow topics (middleware hooks,
> disclosure policy, background scheduling internals) that `AGENTS.md`
> summarizes. For anything else, prefer `docs/AGENTS.md` — when the two
> disagree, `AGENTS.md` wins (it is the one kept in sync with the
> `Capability` enum and the constitution).

This document describes all agent features available in the Autonoetic gateway runtime.

## Table of Contents

- [Agent Manifest](#agent-manifest)
- [Execution Modes](#execution-modes)
- [Capabilities](#capabilities)
- [I/O Schemas](#io-schemas)
- [Middleware Hooks](#middleware-hooks)
- [Background Scheduling](#background-scheduling)
- [Disclosure Policy](#disclosure-policy)
- [Memory System](#memory-system)

---

## Agent Manifest

Every agent is defined by a `SKILL.md` file containing YAML frontmatter and a Markdown body. The frontmatter defines the agent's identity, capabilities, and behavior.

Install-time ownership split:
- Agent-owned: markdown instruction body + semantic intent fields.
- Gateway-owned: canonical metadata shape and gateway/runtime lock closure fields.
- Use `agent_revision_schema` to inspect the current install contract at runtime.

### Minimal Example

```yaml
---
name: "my-agent"
description: "A simple reasoning agent"
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
      id: "my-agent"
      name: "My Agent"
      description: "A simple reasoning agent"
    llm_config:
      provider: "openai"
      model: "gpt-4o"
      temperature: 0.0
    capabilities:
      - type: "ReadAccess"
        scopes: ["*"]
---
# My Agent Instructions

You are a helpful assistant.
```

### Manifest Fields (Canonical Target)

| Field | Required | Description |
|-------|----------|-------------|
| `version` | Yes | Manifest version (use `"1.0"`) |
| `runtime` | Yes | Runtime declaration block (canonical target) |
| `agent` | Yes | Agent identity (id, name, description) |
| `capabilities` | No | List of granted capabilities |
| `llm_config` | No | LLM provider configuration |
| `limits` | No | Resource limits |
| `execution_mode` | No | `reasoning` (default) or `script` |
| `script_entry` | No | Script path for script mode |
| `io` | No | I/O schema contract |
| `middleware` | No | Pre/post processing hooks |
| `background` | No | Background scheduling policy |
| `disclosure` | No | Output disclosure policy |

The parser accepts both native top-level Autonoetic shape and metadata-wrapped shape (`metadata.autonoetic.*`) during install/import compatibility.

---

## Execution Modes

Autonoetic supports two execution modes for agents: **reasoning** (default) and **script**.

### Reasoning Mode (default)

Agents use the full LLM lifecycle: the gateway builds a system prompt, sends it to the configured LLM, processes tool calls, and manages conversation history. This is the default mode and requires `llm_config`.

```yaml
execution_mode: reasoning  # optional, this is the default
llm_config:
  provider: "openai"
  model: "gpt-4o"
```

### Script Mode

Script agents execute directly in the sandbox without LLM involvement. The gateway runs the specified script with the ingress payload as input and returns the script's stdout as the reply.

**Key properties:**
- Fast execution (no LLM latency)
- Deterministic (same input → same output)
- Cheap (no token usage)
- Limited to simple data retrieval and transforms

**Requirements:**
- Must set `execution_mode: script`
- Must set `script_entry` pointing to the script
- `llm_config` is not required

```yaml
execution_mode: script
script_entry: scripts/weather.py
```

**Script input:** The script receives the normalized task payload via `AUTONOETIC_INPUT_PATH` (primary) and `AUTONOETIC_INPUT` (compatibility). When delegation metadata exists, it is exposed separately via `AUTONOETIC_META_PATH` and `AUTONOETIC_META`. When `script_input_mode: stdin` (default), the normalized payload is also written to the script's stdin. When `script_input_mode: args`, the normalized payload is passed as the first positional CLI argument ($1).

Prefer the injected SDK helper over direct environment parsing:

```python
from autonoetic_sdk import load_invocation

invocation = load_invocation()
task = invocation.input
metadata = invocation.metadata
```

This keeps runtime input and delegation metadata separate while still allowing local CLI fallbacks when the script runs outside the gateway.

**Script output:** stdout is captured and returned as the agent reply.

**Example script:**
```python
from autonoetic_sdk import load_input
import json

input_data = load_input("")
print(json.dumps({"result": input_data}))
```

**Input schema contract:**
- The agent author declares `io.accepts` (and optionally `io.returns`) in the manifest. The gateway surfaces these through `agent_list` so callers can shape `message` correctly before calling `agent_spawn`.
- When `io.accepts` is present, the gateway parses the caller's `message` and validates it against the schema. On mismatch, `agent_spawn` returns `{ "ok": false, "error": "schema_validation_failed", "expected_schema": ..., "fields_with_errors": [...], "hint": ... }` — the calling LLM reads this and retries with a corrected payload. Type coercion (default values, type defaults for required fields) is applied silently.
- When `io.returns` is present, the gateway validates the child agent's final reply before returning the `SpawnResult` to the caller. Output-policy checks (reply length, prohibited patterns, artifact constraints, repair policy) are declared separately under `io.output_policy`. Mismatches are rejected at the gateway boundary and recorded as contract events so drift is visible in traces.
- When `io.accepts` is absent, `message` is passed through unchanged. The script is responsible for parsing it.
- The gateway does **not** invent a default schema: `create_from_intent` without an explicit `io` installs the agent with `io: None`.

**When to use script mode:**
- API data retrieval (weather, stock prices, status checks)
- Simple data transforms
- Deterministic operations without reasoning needed

**When to use reasoning mode:**
- Tasks requiring judgment or interpretation
- Multi-step reasoning
- Ambiguous user intents
- Tasks requiring tool use chains

### Fast Path

When `execution_mode: script`, the gateway bypasses the full LLM lifecycle:
1. Loads the agent manifest
2. Validates the script entry exists
3. Executes the script directly in sandbox
4. Returns stdout as reply
5. Logs causal events (`script.started`, `script.completed`, `script.failed`)

No LLM request is made, no history is managed, and no tool calls are processed.

---

## Capabilities

Capabilities control what agents can do. The gateway enforces these at runtime.

### Available Capabilities

| Capability | Description |
|------------|-------------|
| `ReadAccess` | Read from memory scopes |
| `WriteAccess` | Write to memory scopes |
| `CodeExecution` | Execute shell commands |
| `SandboxFunctions` | Invoke MCP tools |
| `AgentSpawn` | Spawn child agents |
| `AgentMessage` | Message other agents |
| `BackgroundReevaluation` | Schedule background tasks |
| `NetworkAccess` | Make network requests |
| `SandboxExec` | Execute in sandbox |

### Capability Examples

```yaml
capabilities:
  - type: "ReadAccess"
    scopes: ["*"]
  - type: "WriteAccess"
    scopes: ["self.*", "shared.*"]
  - type: "AgentSpawn"
    max_children: 5
  - type: "CodeExecution"
    patterns: ["cargo test *", "python *"]
```

---

## I/O Schemas

Agents can declare input and output schemas for mechanical validation and adapter-based composition.

### Example

```yaml
io:
  accepts:
    type: object
    required:
      - query
    properties:
      query:
        type: string
  returns:
    type: object
    properties:
      findings:
        type: array
      summary:
        type: string
```

**Purpose:**
- Validates incoming payloads against the accepts schema
- Enables adapter agents to generate wrappers for schema mismatch
- Exposed in `agent_discover` for planner routing decisions

---

## Middleware Hooks

Agents can declare pre-processing and post-processing hooks that run in the sandbox.

### Example

```yaml
middleware:
  pre_process: "python3 scripts/normalize.py"
  post_process: "python3 scripts/format.py"
```

**Pre-process hook:**
- Runs on user input before passing to LLM
- Can set `skip_llm: true` in JSON output to bypass LLM entirely
- Useful for input normalization or short-circuit responses

**Post-process hook:**
- Runs on LLM output before returning to user
- Can transform or filter the response

---

## Background Scheduling

Agents can run in the background on a schedule.

### Example

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

### Background Modes

| Mode | Description |
|------|-------------|
| `deterministic` | Execute pending scheduled actions directly (no LLM) |
| `reasoning` | Full LLM-driven execution for each wake |

### Wake Predicates

Only two wake predicates are active. All others have been removed.

| Predicate | Description |
|-----------|-------------|
| `timer` | Wake on interval timer |
| `approval_resolved` | Wake when a pending approval is granted or rejected |

---

## Disclosure Policy

Controls what agent replies can contain from sensitive sources.

### Example

```yaml
disclosure:
  default_class: "public"
  path_overrides:
    - path: "secrets/*"
      class: "secret"
      action: "withhold"
```

### Disclosure Classes

| Class | Description |
|-------|-------------|
| `public` | Can be returned verbatim |
| `internal` | Can be returned within gateway |
| `confidential` | Summarized only |
| `secret` | Never returned verbatim |

---

## Memory System

Autonoetic provides two memory tiers:

### Tier 1 (Working Memory)

- Local state files in agent directory (`state/`)
- For deterministic checkpoint and operational continuity
- Restart-safe, per-agent only

### Tier 2 (Durable Memory)

- Gateway-managed SQLite storage (`gateway.db` / `memories`)
- Cross-session and cross-agent recall with **visibility** (`private`, `session`, `global`) — sharing is `knowledge_store` + `visibility`, not a separate tool
- Optional **retention** TTL on stored rows
- Full provenance tracking

### Memory tools (gateway LLM / JSON-RPC)

| Tool | Description |
|------|-------------|
| `knowledge_store` | Store or upsert a durable fact (`visibility`, `retention`, tags, …) |
| `knowledge_recall` | Retrieve by id if visible |
| `knowledge_search` | Search by scope, content, and/or AND-matched tags |

Tier 1 working files still map to SDK helpers / `content.*` for session files under `state/`.

---

## Example: Script Agent (Weather)

```yaml
---
name: "weather.default"
description: "Fetches weather data for a city"
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
      id: "weather.default"
      name: "Weather Agent"
      description: "Fetches weather data for a city"
    execution_mode: script
    script_entry: scripts/fetch_weather.py
    capabilities: []
---
# Weather Agent
This agent runs as a script and returns weather data.
```

## Example: Reasoning Agent (Researcher)

```yaml
---
name: "researcher.default"
description: "Researches topics and returns findings"
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
      id: "researcher.default"
      name: "Researcher Default"
      description: "Researches topics and returns findings"
    llm_config:
      provider: "openai"
      model: "gpt-4o"
      temperature: 0.2
    capabilities:
      - type: "ReadAccess"
        scopes: ["*"]
      - type: "WriteAccess"
        scopes: ["self.*"]
    io:
      accepts:
        type: object
        required:
          - query
      returns:
        type: object
        properties:
          findings:
            type: array
          summary:
            type: string
---
# Researcher Agent
You research topics and return structured findings.
```
