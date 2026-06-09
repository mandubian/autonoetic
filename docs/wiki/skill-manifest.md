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
    llm_config:
      provider: "openai"
      model: "gpt-4o"
      temperature: 0.1
    capabilities:
      - type: "MemoryWrite"
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
| `metadata.autonoetic.llm_config` | Reasoning mode | LLM provider/model config |
| `metadata.autonoetic.capabilities` | No | Permission declarations |
| `metadata.autonoetic.execution_mode` | No | `"reasoning"` (default) or `"script"` |
| `metadata.autonoetic.script_entry` | Script mode | Entry script path |
| `metadata.autonoetic.io` | No | JSON Schema for input/output |
| `metadata.autonoetic.validation` | No | `"soft"` (LLM) or `"strict"` (script) |

## Script vs Reasoning Mode

| Mode | LLM Required | Use For |
|------|-------------|---------|
| `reasoning` | Yes | Complex judgment, research, ambiguous requirements |
| `script` | No | Deterministic API calls, data transforms, simple lookups |

## Extended Instructions (`<!-- extended -->`)

Place `<!-- extended -->` on its own line to split the body:
- **Before marker**: Always injected into system prompt
- **After marker**: Available on-demand via `resolve({"ref": "extended_instructions"})`

Use this to keep the core prompt lean while providing detailed reference material.

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
