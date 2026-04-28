# Agent Capabilities Reference

## Overview

This document describes the capability system used by Autonoetic agents. Capabilities define what tools and resources an agent can access.

## Capability Types

| Capability | Required Fields | Purpose |
|------------|-----------------|---------|
| `SandboxFunctions` | `allowed: [string]` | Access to MCP tools by prefix |
| `ReadAccess` | `scopes: [string]` | Read content, memory, knowledge |
| `WriteAccess` | `scopes: [string]` | Write content, memory, knowledge |
| `CodeExecution` | `patterns: [string]` | Execute scripts in sandbox |
| `NetworkAccess` | `hosts: [string]` | Make HTTP requests |
| `AgentSpawn` | `max_children: integer` | Create child agent sessions |
| `AgentMessage` | `patterns: [string]` | Send messages to other agents |

## Tool Capability Mapping

### Content Tools
| Tool | Requires Capability | Notes |
|------|---------------------|-------|
| `content_read` | `ReadAccess` | Read from content store |
| `content_write` | `WriteAccess` | Write to content store with visibility |

### Artifact Tools
| Tool | Requires Capability | Notes |
|------|---------------------|-------|
| `artifact_build` | `WriteAccess` | Build immutable artifact from session content |
| `artifact_inspect` | `ReadAccess` | Inspect artifact files and metadata |
| `artifact_resolve_ref` | `ReadAccess` | Resolve scoped artifact refs to canonical artifact IDs |
| `artifact_prepare` | `CodeExecution` | Preflight for artifact execution (approval + credentials) |
| `artifact_exec` | `CodeExecution` | Execute artifact entrypoint with artifact-bound approval reuse |

### Agent Tools
| Tool | Requires Capability | Notes |
|------|---------------------|-------|
| `agent_spawn` | `AgentSpawn` | Spawn child agent sessions |
| `agent_exists` | `SandboxFunctions: ["agent."]` | Check if agent exists |
| `agent_discover` | `SandboxFunctions: ["agent."]` | Discover available agents |

### Revision & Activation Tools
| Tool | Requires Capability | Notes |
|------|---------------------|-------|
| `agent_revision_create` | `AgentRevision` | Create immutable revision from an AgentBundle artifact |
| `agent_revision_list` | `AgentRevision` | List revisions for an agent |
| `agent_revision_inspect` | `AgentRevision` | Inspect revision metadata and status |
| `agent_revision_promote` | `AgentRevision` | Move alias to a revision (activates it) |
| `agent_revision_rollback` | `AgentRevision` | Roll alias back to a prior revision |
| `agent_revision_diff` | `AgentRevision` | File-level diff between two revisions |

> **Note:** `agent.install` has been removed from the runtime tool surface. The only path to activate a new logical agent is: `artifact_build` → `agent_revision_create` → `agent_revision_promote`.

### Knowledge Tools
| Tool | Requires Capability | Notes |
|------|---------------------|-------|
| `knowledge_recall` | `ReadAccess` | Recall stored knowledge (visibility + session + expiry enforced) |
| `knowledge_store` | `WriteAccess` | Store or update knowledge; `visibility` (`session` default) and `retention` |
| `knowledge_search` | `ReadAccess` | Search knowledge base |
| `knowledge_search_by_tags` | `ReadAccess` | Tag-AND search in a scope |
| `digest_query` | `ReadAccess` | Post-session narrative / digest |

### Sandbox Tools
| Tool | Requires Capability | Notes |
|------|---------------------|-------|
| `sandbox_exec` | `CodeExecution` | Execute scripts with patterns |

## Important: SandboxFunctions vs Native Tools

**Common misconception**: `SandboxFunctions` with prefix `"content."` grants access to `content_read`, `content_write`, etc.

**Reality**: `SandboxFunctions` is for **MCP (Model Context Protocol) tools only**. Native content tools require `ReadAccess` and `WriteAccess` capabilities.

```
❌ WRONG:
  capabilities:
    - type: "SandboxFunctions"
      allowed: ["content."]  # This does NOT grant content_read access!

✅ CORRECT:
  capabilities:
    - type: "ReadAccess"
      scopes: ["self.*"]
    - type: "WriteAccess"
      scopes: ["self.*"]
```

## Agent Capability Matrix

### Standard Agents

| Agent | ReadAccess | WriteAccess | CodeExecution | NetworkAccess | AgentSpawn | AgentRevision |
|-------|------------|-------------|---------------|---------------|------------|--------------|
| **planner.default** | ✅ | ✅ | ❌ | ❌ | ✅ (10) | ❌ Delegates to specialized_builder |
| **specialized_builder.default** | ✅ | ✅ | ❌ | ❌ | ✅ (5) | ✅ **EXCLUSIVE** |
| **coder.default** | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ |
| **researcher.default** | ✅ | ✅ | ❌ | ✅ | ❌ | ❌ |
| **architect.default** | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ |
| **debugger.default** | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ |
| **auditor.default** | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ |
| **agent-adapter.default** | ✅ | ✅ | ✅ | ❌ | ✅ (5) | ❌ Must delegate |

### Capability Details by Agent

#### planner.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge.", "agent."]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*"]
  - type: "AgentSpawn"
    max_children: 10
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]
```
- **Role**: Front-door lead agent, interprets goals, delegates to specialists
- **Cannot**: Execute code, install agents directly, make HTTP requests

#### specialized_builder.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge.", "agent."]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*", "agents/*"]
  - type: "AgentSpawn"
    max_children: 5
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*", "agents/*"]
  - type: "AgentRevision"
    patterns: ["*"]
```
- **Role**: **EXCLUSIVE** agent activator — only agent that calls `agent_revision_create` + `agent_revision_promote`
- **Cannot**: Execute code, make HTTP requests

#### coder.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge.", "sandbox."]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*", "scripts/*"]
  - type: "CodeExecution"
    patterns: ["python3 scripts/*", "node *", "bash *"]
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*", "scripts/*"]
```
- **Role**: Code production, sandboxed execution
- **Cannot**: Install agents, make HTTP requests directly

#### researcher.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge.", "web.", "mcp_"]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*"]
  - type: "NetworkAccess"
    hosts: ["*"]
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]
```
- **Role**: Evidence gathering, web research
- **Cannot**: Execute code, install agents

#### architect.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge."]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*"]
  - type: "CodeExecution"
    patterns: ["python3 scripts/*"]
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]
```
- **Role**: System design, prototyping
- **Cannot**: Install agents, make HTTP requests

#### debugger.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge.", "sandbox."]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*"]
  - type: "CodeExecution"
    patterns: ["python3 scripts/*", "node *", "bash *"]
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]
```
- **Role**: Root cause analysis, targeted fixes
- **Cannot**: Install agents, make HTTP requests

#### auditor.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge."]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*"]
  - type: "CodeExecution"
    patterns: ["python3 scripts/*"]
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]
```
- **Role**: Correctness review, reproducibility verification
- **Cannot**: Install agents, make HTTP requests

#### agent-adapter.default
```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["knowledge.", "sandbox."]
  - type: "ReadAccess"
    scopes: ["self.*", "skills/*"]
  - type: "CodeExecution"
    patterns: ["python3 scripts/*"]
  - type: "AgentSpawn"
    max_children: 5
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]
```
- **Role**: Generate wrapper agents for I/O bridging
- **Note**: Must delegate agent installation to specialized_builder

## Delegation Model

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              PLANNER (Lead)                                 │
│  • Interprets goals                                                         │
│  • Delegates to specialists                                                 │
│  • Synthesizes responses                                                    │
│  • CANNOT install agents or execute code                                    │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                ┌───────────────────┼───────────────────┐
                ▼                   ▼                   ▼
┌───────────────────────┐ ┌───────────────────────┐ ┌───────────────────────┐
│  SPECIALIZED_BUILDER  │ │      CODER            │ │     RESEARCHER        │
│  • Creates revisions  │ │  • Writes code        │ │  • Gathers evidence   │
│  • ONLY agent that    │ │  • Executes in sandbox│ │  • Has network access │
│    can promote agents │ │  • Tests before return│ │  • Cites sources      │
└───────────────────────┘ └───────────────────────┘ └───────────────────────┘
                                    │
                ┌───────────────────┼───────────────────┐
                ▼                   ▼                   ▼
┌───────────────────────┐ ┌───────────────────────┐ ┌───────────────────────┐
│     ARCHITECT         │ │      DEBUGGER         │ │      AUDITOR          │
│  • Designs systems    │ │  • Root cause analysis│ │  • Review & audit     │
│  • Creates prototypes │ │  • Targeted fixes     │ │  • Verify results     │
│  • Documents decisions│ │  • Reproduces issues  │ │  • Risk assessment    │
└───────────────────────┘ └───────────────────────┘ └───────────────────────┘
```

## Capability Format Examples

### NetworkAccess (requires `hosts` field)
```json
{"type": "NetworkAccess", "hosts": ["api.example.com"]}
{"type": "NetworkAccess", "hosts": ["*"]}  // Allow all hosts
```

### SandboxFunctions (for MCP tools)
```json
{"type": "SandboxFunctions", "allowed": ["web.", "content_read"]}
```

### CodeExecution (script patterns)
```json
{"type": "CodeExecution", "patterns": ["python3 scripts/*", "node *"]}
```

### ReadAccess/WriteAccess (scopes)
```json
{"type": "ReadAccess", "scopes": ["self.*", "shared.*"]}
{"type": "WriteAccess", "scopes": ["self.*", "agents/*"]}
```

### AgentSpawn (max children)
```json
{"type": "AgentSpawn", "max_children": 5}
```

## Common Mistakes

1. **Using `SandboxFunctions: ["content."]` for content access**
   - Wrong: Use `ReadAccess`/`WriteAccess` instead

2. **Adding extra fields to capabilities**
   - Wrong: `{"type": "NetworkAccess", "description": "...", "hosts": [...]}`
   - Correct: `{"type": "NetworkAccess", "hosts": [...]}`

3. **Planner trying to activate an agent directly**
   - Wrong: Planner calls `agent_revision_create` or `agent_revision_promote`
   - Correct: Planner delegates to `specialized_builder.default` via `agent_spawn`

4. **Bypassing eval gating for promotion**
   - `agent_revision_promote` can require a passed `required_eval_run_id`
   - Pass `eval_run` output to `specialized_builder` so it can include it in the promote call

## See Also

- [AGENTS.md](AGENTS.md) — agent model, lifecycle, tool surface
- [ARCHITECTURE.md](ARCHITECTURE.md) — revision & activation model, gateway internals
