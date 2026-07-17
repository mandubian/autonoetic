# Agent Capabilities and Permissions

## Overview

Every agent declares its required capabilities in the SKILL.md frontmatter. The gateway validates all tool calls against these declarations — an agent cannot perform any action it has not explicitly declared.

The canonical, exhaustive list is the `Capability` enum in `autonoetic-types/src/capability.rs` and the summary in `docs/AGENTS.md` → Capabilities System. This page lists the most common variants.

## Capability Categories

| Category | Purpose | Examples |
|----------|---------|---------|
| **Tool Access** | Which tools can be invoked | `SandboxFunctions`, `ArtifactExecution`, `CodeExecution` |
| **Storage Access** | Which paths/scopes can be read/written | `ReadAccess`, `WriteAccess` |
| **Privilege Escalation** | Operations that cross boundaries | `NetworkAccess`, `AgentSpawn`, `AgentRevision` |

## Common Capabilities

| Capability | Fields | Description |
|------------|--------|-------------|
| `SandboxFunctions` | `allowed: [string]` | MCP tool access by prefix (e.g., `web_*`, `sandbox_*`). Native tools use their own capability. |
| `ReadAccess` | `scopes: [string]` | Read access to content, memory, knowledge |
| `WriteAccess` | `scopes: [string]` | Write access to content, memory, knowledge |
| `NetworkAccess` | `hosts: [string]` | HTTP/network access to specific hosts. The gateway validates declared hosts against URL/IP literals detected in the artifact source during revision creation. |
| `CodeExecution` | `patterns: [string]` | Execute scripts/commands in sandbox via `sandbox_exec` |
| `ArtifactExecution` | *(none)* | Execute immutable artifact entrypoints via `artifact_exec` / `artifact_prepare` |
| `AgentSpawn` | `max_children: number` | Create child agent sessions |
| `AgentMessage` | `patterns: [string]` | Send messages to other agents |
| `BackgroundReevaluation` | `min_interval_secs`, `allow_reasoning` | Periodic wake-ups for background processing |
| `SchedulerAccess` | `patterns: [string]` | Create/manage cron jobs |
| `SkillInstall` | `allowed_sources: [string]` | Fetch and install remote SKILL.md |
| `AgentRevision` | *(varies)* | Create, promote, diff, and rollback agent revisions |
| `Evaluation` | *(varies)* | Publish eval suites, queue runs, compare revisions |
| `CredentialAccess` | `services`, etc. | Read / register / refresh vault credentials |
| `EmergencyStop` | *(none)* | Request an emergency stop of a root session |
| `ConstitutionalProposal` | *(none)* | Propose constitutional amendments |
| `GateDecider` | `kinds: [approval\|escalation]` | Resolve gates as an agent-decider |
| `CapsuleExport` | *(none)* | Export a cognitive capsule |
| `ReasoningAudit` | *(varies)* | Disclose private reasoning with notification |
| `PlanFrameAccess` | *(varies)* | Read/decompose/track plans as capability-grant envelopes |
| `PromoteWith` | `agent_id`, `capabilities` | Session capability envelope for promotion gating |

## Host Validation

When creating a revision via `agent_revision_create_from_intent`, the gateway extracts hosts from URL literals (e.g. `https://api.example.com/...`) and IP addresses found in the artifact source files. The revision is rejected if any detected host is not covered by the declared `NetworkAccess.hosts` list.

- Declare the exact hostnames your code calls.
- A bare domain like `example.com` covers itself and all subdomains.
- `*.example.com` covers subdomains but not `example.com` itself.
- `hosts: ["*"]` is only accepted when the agent also declares `open_web: true` (constitution P-1.5). Use it only for genuine open-web agents that cannot enumerate targets upfront.

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
| `ArtifactExecution` | `artifact_exec`, `artifact_prepare` |
| `AgentSpawn` | `agent_spawn` |
| `AgentMessage` | `agent_message` |
| `SkillInstall` | `skill_install` |
| `SchedulerAccess` | `scheduler_cron_*` |

## Example Declaration

```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["content_", "knowledge_", "agent_"]
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]
  - type: "AgentSpawn"
    max_children: 10
  - type: "NetworkAccess"
    hosts: ["api.openweathermap.org"]
```
