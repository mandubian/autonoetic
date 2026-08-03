# SKILL.md Manifest Format

## Overview

Every agent is defined by a `SKILL.md` file — a YAML frontmatter header followed by Markdown instructions. The frontmatter declares metadata, capabilities, and configuration. The Markdown body contains the agent's instructions.

## Minimal Example

```yaml
---
name: "my.agent"
description: "One-line description"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.agent"
      name: "My Agent"
      description: "What this agent does"
    llm_preset: smart
    llm_overrides:
      temperature: 0.1
    capabilities:
      - type: "WriteAccess"
        scopes: ["self.*"]
---
# Agent Instructions

Your natural language instructions go here.
```

## Key Frontmatter Fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Fully qualified agent ID |
| `description` | Yes | One-line description |
| `metadata.autonoetic.agent.id` | Yes | Must match directory name |
| `metadata.autonoetic.llm_preset` | Reasoning mode | Named inference preset from gateway config |
| `metadata.autonoetic.llm_overrides` | No | Temperature / thinking overrides on the resolved preset |
| `metadata.autonoetic.llm_config` | Legacy | Inline provider/model (deprecated; prefer `llm_preset`) |
| `metadata.autonoetic.capabilities` | No | Permission declarations |
| `metadata.autonoetic.execution_mode` | No | `"reasoning"` (default) or `"script"` |
| `metadata.autonoetic.script_entry` | Script mode | Entry script path |
| `metadata.autonoetic.io` | No | JSON Schema for input/output |
| `metadata.autonoetic.validation` | No | `"soft"` (LLM) or `"strict"` (script) |
| `metadata.autonoetic.script_input_mode` | No | `"stdin"` (default) or `"args"` |
| `metadata.autonoetic.open_web` | No | Required when `NetworkAccess.hosts` is `["*"]` |
| `metadata.autonoetic.disclosure` | No | Default visibility scopes for outputs |
| `metadata.autonoetic.middleware` | No | Pre/post-processing hooks |

## Script vs Reasoning Mode

| Mode | LLM Required | Use For |
|------|-------------|---------|
| `reasoning` | Yes | Complex judgment, research, ambiguous requirements |
| `script` | No | Deterministic API calls, data transforms, simple lookups |

## Extended Instructions (`<!-- extended -->`)

Place `<!-- extended -->` on its own line to split the body:
- **Before marker**: Always injected into system prompt (the core)
- **After marker**: The extended half — loaded **mechanically by the gateway on
  the agent's first tool call** (#1015), injected as a `gateway_note` on the
  first tool result; from then on it is inlined into the system prompt like any
  other content. There is no manual fetch path: agents do not (and cannot)
  `resolve` it ahead of time — any first tool call delivers it.

Use this to keep the core prompt lean (saving the extended tokens on the cold
turn-1 input) while still guaranteeing the agent gets the detailed reference
material the moment it starts executing — no `resolve` required. Agent SKILLs
with the marker should include a compact "Extended Instructions" ToC in the
core half so the agent knows what topics will arrive (see planner.default /
coder.default).

## Runtime Lock

Every agent directory must contain a `runtime.lock` file declaring:
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

The gateway computes the real SHA-256 during bootstrap.
