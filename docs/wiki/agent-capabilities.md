# Agent Capabilities and Permissions

## Overview

Every agent declares its required capabilities in the SKILL.md frontmatter. The gateway validates all tool calls against these declarations — an agent cannot perform any action it has not explicitly declared.

## Capability Categories

| Category | Purpose | Examples |
|----------|---------|---------|
| **Tool Access** | Which tools can be invoked | `ToolInvoke`, `SandboxFunctions` |
| **Storage Access** | Which paths/scopes can be read/written | `ReadAccess`, `WriteAccess` |
| **Privilege Escalation** | Operations that cross boundaries | `NetworkAccess`, `AgentSpawn` |

## Available Capabilities

| Capability | Fields | Description |
|------------|--------|-------------|
| `ToolInvoke` | `allowed: [string]` | Tool access by prefix (e.g., `web.*`, `sandbox.*`) |
| `ReadAccess` | `scopes: [string]` | Read access to content, memory, knowledge |
| `WriteAccess` | `scopes: [string]` | Write access to content, memory, knowledge |
| `NetworkAccess` | `hosts: [string]` | HTTP/network access to specific hosts. The gateway validates declared hosts against URL literals detected in the artifact source during revision creation. |

## Host Validation

When creating a revision via `agent_revision_create_from_intent`, the gateway extracts hosts from URL literals (e.g. `https://api.example.com/...`) and IP addresses found in the artifact source files. The revision is rejected if any detected host is not covered by the declared `NetworkAccess.hosts` list.

- Declare the exact hostnames your code calls.
- A bare domain like `example.com` covers itself and all subdomains.
- `*.example.com` covers subdomains but not `example.com` itself.
- Use `hosts: ["*"]` only for genuine open-web agents that cannot enumerate targets upfront (e.g. researcher, web-search).
| `CodeExecution` | `patterns: [string]` | Execute scripts/commands in sandbox |
| `AgentSpawn` | `max_children: number` | Create child agent sessions |
| `AgentMessage` | `patterns: [string]` | Send messages to other agents |
| `BackgroundReevaluation` | `min_interval_secs`, `allow_reasoning` | Periodic wake-ups |
| `SchedulerAccess` | `patterns: [string]` | Create/manage cron jobs |
| `SkillInstall` | `allowed_sources: [string]` | Fetch and install remote SKILL.md |
| `MemoryWrite` | `scopes: [string]` | Write to memory scopes |
| `PlanFrameAccess` | *(none)* | Access workbench tools (planner only) |

## Scoping

Capabilities use pattern-based scoping:
- `*` = wildcard (all access)
- `self.*` = own agent's state
- `skills/*` = installed skills directory
- `scripts/*` = scripts directory

## What Gates What

| Capability | Gates These Tools |
|------------|------------------|
| `ReadAccess` | `resolve`, `artifact_inspect`, `memory.read`, `knowledge_recall`, `knowledge_search` |
| `WriteAccess` | `content_write`, `artifact_build`, `memory.write`, `knowledge_store` |
| `NetworkAccess` | `web_fetch`, `web_search`, `web_call` |
| `CodeExecution` | `sandbox_exec` |
| `AgentSpawn` | `agent_spawn` |

## Example Declaration

```yaml
capabilities:
  - type: "ToolInvoke"
    allowed: ["content_", "knowledge_", "agent_"]
  - type: "MemoryWrite"
    scopes: ["self.*", "skills/*"]
  - type: "AgentSpawn"
    max_children: 10
  - type: "NetworkAccess"
    hosts: ["api.openweathermap.org"]
```
