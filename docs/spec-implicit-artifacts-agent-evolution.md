# Spec: Implicit Artifacts and Agent Evolution

**Status:** Draft
**Author:** Architecture Review
**Date:** 2026-03-23
**Updated:** 2026-03-28 — Added `workflow.state` for structured resume; clarified implicit-vs-explicit boundary

---

## 1. Problem Statement

### 1.1 Content Access Failures

In multi-agent workflows, the planner frequently fails to access outputs from child agents:

1. **Researcher** fetches data but doesn't persist it → planner can't find it
2. **Architect** writes content with auto-generated aliases (`e1bcea62`) → planner can't guess the name
3. **Planner** hallucinates content names (`weather_result`, `design_doc`) that don't exist
4. **Planner** confuses task IDs (`task-94c19ac6`) with content handles

### 1.2 No Recovery Path

When agents get stuck, they:
- Retry the same failing action
- Hallucinate solutions
- Produce low-quality output with missing data
- **Cannot ask for help** in a structured way

### 1.3 Static Agents

Agents are immutable, but this prevents:
- Learning from failures
- Adding capabilities without forking
- Composing behaviors
- Incremental improvement

---

## 2. Solution Overview

### 2.1 Implicit Artifacts

Every completed task automatically produces an implicit artifact containing:
- Agent's final LLM response (summary)
- Key tool outputs (configurable)
- Metadata (agent_id, task_id, timestamps)

### 2.2 Structured Workflow State

The `workflow.state` tool exposes compact, structured workflow facts so agents can resume deterministically without re-inferring state from conversation history:
- Current workflow step
- Completed tasks with artifact IDs
- Pending approvals
- Active tasks
- Reuse guards (has_coder_artifact, has_evaluator_result, etc.)
- Resume hint (one-line guidance)

### 2.3 Escalation System

Agents can escalate when stuck:
- **Level 1**: Reasoning LLM (o1-style, deeper analysis)
- **Level 2**: Specialist agent (domain expertise)
- **Level 3**: Human (full authority)

### 2.4 Agent Evolution

Agents can be enhanced through:
- **Composition**: Combine multiple agents
- **Enhancement**: Inherit and extend an existing agent
- **Hooks**: Add pre/post execution behaviors
- **Capability Injection**: Add tools and instructions

---

## 3. Implicit vs Explicit Artifacts

### 3.1 Two Classes of Artifacts

The system distinguishes between two artifact classes with different use cases:

| Aspect | Implicit Artifacts | Explicit Artifacts |
|--------|-------------------|-------------------|
| **Created by** | Gateway (automatic, on task completion) | Agent (via `artifact.build`) |
| **Purpose** | Parent-child output handoff | Specialist boundary / review / install |
| **When to use** | Ordinary agent collaboration | Evaluation, audit, installation |
| **Access pattern** | `workflow.wait` output field, `workflow.state` completed_tasks | `artifact.inspect`, `artifact.build` |
| **Persistence** | TTL-based (default 24h) | Permanent (until deleted) |
| **Who needs it** | All agents | Evaluators, auditors, specialized_builder |

### 3.2 Design Principle

**Ordinary agents should think in terms of implicit outputs first.** Explicit artifacts are specialist-boundary objects that should not be a universal cognitive burden.

- Planner consuming researcher output → use `workflow.wait` implicit output
- Evaluator validating coder output → use `artifact.inspect` on explicit artifact
- Specialized_builder installing → use explicit artifact_id from promotion gate

---

## 4. Implicit Artifacts

### 4.1 Definition

An **implicit artifact** is automatically created by the gateway when a task completes. It captures the task's outputs for cross-session access.

```
Task completes → Gateway creates impl_{task_id} → Available to parent session
```

### 4.2 Content Structure

```json
{
  "artifact_id": "impl_task-94c19ac6",
  "artifact_type": "implicit",
  "task_id": "task-94c19ac6",
  "agent_id": "researcher.default",
  "session_id": "demo-session-1/researcher.default-c4545f82",
  "parent_session": "demo-session-1",
  "created_at": "2026-03-23T13:41:33Z",
  "expires_at": "2026-03-24T13:41:33Z",  // 24h TTL by default
  "summary": "Fetched current weather for Paris using Open-Meteo API...",
  "content": {
    "llm_response": "The current weather in Paris is...",
    "tool_outputs": [
      {
        "tool": "web.fetch",
        "summary": "Open-Meteo API response",
        "content_ref": "sha256:abc123..."  // Reference, not inline
      }
    ]
  },
  "metadata": {
    "tokens_used": 1234,
    "duration_ms": 45000,
    "stop_reason": "EndTurn"
  }
}
```

### 4.3 Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│  Task Spawned                                                │
│       ↓                                                      │
│  Agent Executes                                              │
│       ↓                                                      │
│  Task Completes (success/failure)                            │
│       ↓                                                      │
│  Gateway creates impl_{task_id}                              │
│       ↓                                                      │
│  Parent session can access via workflow.wait or workflow.state │
│       ↓                                                      │
│  TTL expires (default 24h) → garbage collected               │
└─────────────────────────────────────────────────────────────┘
```

### 4.4 Access Patterns

#### Via workflow.wait Response

```json
// workflow.wait returns:
{
  "task_id": "task-94c19ac6",
  "status": "completed",
  "output": {
    "artifact_id": "impl_task-94c19ac6",
    "summary": "Fetched current weather for Paris..."
  }
}
```

#### Via workflow.state (Recommended for Resume)

```json
// workflow.state returns structured facts:
{
  "workflow_status": "active",
  "completed_tasks": [
    {
      "task_id": "task-94c19ac6",
      "agent_id": "researcher.default",
      "status": "succeeded",
      "result_summary": "Fetched current weather for Paris..."
    }
  ],
  "reuse_guards": {
    "has_coder_artifact": false,
    "has_evaluator_result": false,
    "pending_approvals": false,
    "active_tasks_running": false
  },
  "resume_hint": "some_tasks_done — check completed_tasks for next step"
}
```

#### Via content.read

```json
content.read({
  "name_or_handle": "impl_task-94c19ac6"
})
```

#### Via artifact.inspect

```json
artifact.inspect({
  "artifact_id": "impl_task-94c19ac6"
})
```

### 4.5 Configuration

Implicit artifact behavior is configurable at gateway level:

```yaml
# gateway config
implicit_artifacts:
  enabled: true
  ttl_hours: 24
  include_tool_outputs: true
  max_tool_output_size_kb: 64
  excluded_tools: ["sandbox.exec"]  // Don't capture large/verbose outputs
```

### 4.6 Enhanced Error Messages

When `content.read` fails for a name that looks like a guessed name:

```json
{
  "error_type": "resource",
  "message": "Content 'weather_result' not found",
  "hint": "Use workflow.wait or workflow.state to get stable output handles from completed child tasks, then use content.read with the artifact_id from the output field.",
  "available_artifacts": [
    {"artifact_id": "impl_task-94c19ac6", "from": "researcher.default", "summary": "Fetched weather..."},
    {"artifact_id": "impl_task-fb261586", "from": "architect.default", "summary": "Design document..."}
  ]
}
```
Task completes → Gateway creates impl_{task_id} → Available to parent session
```

### 3.2 Content Structure

```json
{
  "artifact_id": "impl_task-94c19ac6",
  "artifact_type": "implicit",
  "task_id": "task-94c19ac6",
  "agent_id": "researcher.default",
  "session_id": "demo-session-1/researcher.default-c4545f82",
  "parent_session": "demo-session-1",
  "created_at": "2026-03-23T13:41:33Z",
  "expires_at": "2026-03-24T13:41:33Z",  // 24h TTL by default
  "summary": "Fetched current weather for Paris using Open-Meteo API...",
  "content": {
    "llm_response": "The current weather in Paris is...",
    "tool_outputs": [
      {
        "tool": "web.fetch",
        "summary": "Open-Meteo API response",
        "content_ref": "sha256:abc123..."  // Reference, not inline
      }
    ]
  },
  "metadata": {
    "tokens_used": 1234,
    "duration_ms": 45000,
    "stop_reason": "EndTurn"
  }
}
```

### 3.3 Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│  Task Spawned                                                │
│       ↓                                                      │
│  Agent Executes                                              │
│       ↓                                                      │
│  Task Completes (success/failure)                            │
│       ↓                                                      │
│  Gateway creates impl_{task_id}                              │
│       ↓                                                      │
│  Parent session can access via workflow.outputs or content   │
│       ↓                                                      │
│  TTL expires (default 24h) → garbage collected               │
└─────────────────────────────────────────────────────────────┘
```

### 3.4 Access Patterns

#### Via workflow.wait Response

```json
// workflow.wait returns:
{
  "task_id": "task-94c19ac6",
  "status": "completed",
  "output": {
    "artifact_id": "impl_task-94c19ac6",
    "summary": "Fetched current weather for Paris..."
  }
}
```

#### Via content.read

```json
content.read({
  "name_or_handle": "impl_task-94c19ac6"
})
```

#### Via artifact.inspect

```json
artifact.inspect({
  "artifact_id": "impl_task-94c19ac6"
})
```

### 3.5 Configuration

Implicit artifact behavior is configurable at gateway level:

```yaml
# gateway config
implicit_artifacts:
  enabled: true
  ttl_hours: 24
  include_tool_outputs: true
  max_tool_output_size_kb: 64
  excluded_tools: ["sandbox.exec"]  # Don't capture large/verbose outputs
```

### 3.6 Enhanced Error Messages

When `content.read` fails for a name that looks like a guessed name:

```json
{
  "error_type": "resource",
  "message": "Content 'weather_result' not found",
  "hint": "Did you mean one of these implicit artifacts from your workflow?",
  "available_artifacts": [
    {"artifact_id": "impl_task-94c19ac6", "from": "researcher.default", "summary": "Fetched weather..."},
    {"artifact_id": "impl_task-fb261586", "from": "architect.default", "summary": "Design document..."}
  ]
}
```

---

## 4. Escalation System

### 4.1 Overview

When an agent cannot proceed, it should escalate rather than guess or fail silently.

```
Self-retry → Escalate to reasoning LLM → Escalate to specialist → Escalate to human
   (free)         (medium cost)             (medium cost)          (high cost)
```

### 4.2 Escalation Tool

```json
{
  "name": "session.escalate",
  "description": "Request help when stuck. Use this when you've tried reasonable approaches but cannot proceed correctly.",
  "parameters": {
    "type": "object",
    "properties": {
      "reason": {
        "type": "string",
        "description": "Clear explanation of why you're stuck"
      },
      "context": {
        "type": "string",
        "description": "Relevant context: what you tried, what failed, error messages"
      },
      "target": {
        "type": "string",
        "enum": ["reasoning_llm", "specialist", "human"],
        "default": "reasoning_llm",
        "description": "Who to ask for help"
      },
      "urgency": {
        "type": "string",
        "enum": ["low", "medium", "high"],
        "default": "medium"
      },
      "suggested_actions": {
        "type": "array",
        "items": {"type": "string"},
        "description": "Possible next steps you're considering (helps target respond better)"
      }
    },
    "required": ["reason", "context"]
  }
}
```

### 4.3 Escalation Targets

#### Level 1: Reasoning LLM

- **When**: Complex analysis needed, multiple possible paths
- **Model**: Configurable per gateway (e.g., o1-preview, claude with extended thinking)
- **Cost**: Medium
- **Latency**: Medium (may take 10-60 seconds)

```json
// Agent calls:
session.escalate({
  "reason": "Cannot determine correct artifact ID from workflow.wait response",
  "context": "workflow.wait returned task-94c19ac6 completed but I don't see an output_artifact field",
  "target": "reasoning_llm",
  "suggested_actions": [
    "Try content.read with task ID",
    "Ask user for clarification",
    "Check if output is in a different field"
  ]
})

// Gateway responds:
{
  "analysis": "The workflow.wait response structure includes an 'output' object with 'artifact_id'. Use: content.read({'name_or_handle': 'impl_task-94c19ac6'})",
  "confidence": "high",
  "alternative": "If that fails, the task output may be in the workflow.events for this task."
}
```

#### Level 2: Specialist Agent

- **When**: Domain-specific expertise needed
- **Agent**: Configurable per escalation (or gateway default)
- **Cost**: Medium
- **Latency**: Medium

```json
session.escalate({
  "reason": "Need to understand security implications of this code pattern",
  "context": "Code uses subprocess with user input, not sure if it's safe",
  "target": "specialist",
  "specialist_agent": "auditor.default"  // Optional override
})
```

#### Level 3: Human

- **When**: Authorization needed, or all automated paths exhausted
- **Blocking**: Yes, workflow pauses until response
- **Cost**: High
- **Latency**: Variable (minutes to hours)

```json
session.escalate({
  "reason": "Multiple approaches possible, need business decision",
  "context": "Can implement as (A) simple but limited, or (B) complex but flexible",
  "target": "human",
  "urgency": "medium"
})
```

### 4.4 Escalation Capability Declaration

Agents declare escalation capability in their SKILL.md:

```yaml
capabilities:
  - type: Escalation
    targets:
      - type: reasoning_llm
        model: "o1-preview"
        max_per_session: 5
      - type: specialist
        default_agent: "helper.default"
      - type: human
        trigger: "auto"  # or "manual" only
```

### 4.5 Escalation Tracking

Gateway tracks escalations to prevent loops:

```json
{
  "session_id": "demo-session-1",
  "escalations": [
    {
      "escalation_id": "esc-001",
      "timestamp": "2026-03-23T13:45:00Z",
      "target": "reasoning_llm",
      "reason": "Cannot find artifact",
      "resolution": "Provided correct artifact ID",
      "outcome": "resolved"
    }
  ],
  "escalation_count": 1,
  "max_allowed": 10  // Per session
}
```

---

## 5. Agent Evolution

### 5.0 Existing Evolution Infrastructure

The system already has an **agent-adapter.default** that generates wrapper agents for bridging I/O gaps:

```
agent-adapter.default
├── Analyzes source and target schemas (schema_diff.py)
├── Generates wrapper scripts (generate_wrapper.py)
└── Delegates installation to specialized_builder.default
```

This is a form of agent evolution already in production. The new evolution mechanisms should integrate with and extend this pattern.

**Evolution Roles Hierarchy:**

| Role | Capability |
|------|------------|
| `agent-adapter.default` | Generates I/O adapters (wrappers for schema translation) |
| `specialized_builder.default` | Installs agents from artifacts, creates enhanced agents |
| `evolution-steward.default` | (Future) Orchestrates evolution, detects improvement opportunities |

### 5.1 Design Principles

1. **Base agents remain immutable** - Original agent definitions don't change
2. **Evolution creates new agents** - Enhanced versions are new agent IDs
3. **Composition over modification** - Prefer combining agents over editing them
4. **Explicit inheritance** - Clear lineage from base to enhanced

### 5.2 Evolution Mechanisms

#### 5.2.1 Enhancement (Inheritance)

Create a new agent that extends a base agent:

```yaml
# agents/researcher.with-persistence/SKILL.md
---
base: researcher.default
description: "Researcher that always persists results"
---

## Additional Instructions

After completing research:
1. Write all findings to content store with semantic name
2. Return the content name/handle in your response
3. If writing fails, escalate before returning

## Overrides

The following instructions from base are overridden:
- Replace "Return results in your response" with "Return results AND persist to content store"
```

Gateway merges at load time:
```yaml
# Effective SKILL.md (computed)
---
name: researcher.with-persistence
description: Researcher that always persists results
base: researcher.default
capabilities: [NetworkAccess, ReadAccess, WriteAccess, Escalation]
---

[Base SKILL.md content]

## Additional Instructions

After completing research:
1. Write all findings to content store with semantic name
...
```

#### 5.2.2 Hooks (Pre/Post Wrappers)

Add behaviors that run before/after the agent:

```yaml
# agents/researcher.logged/SKILL.md
---
base: researcher.default
hooks:
  pre:
    - type: log
      message: "Starting research task"
    - type: validate
      check: "input.query is not empty"
  post:
    - type: persist
      target: "content_store"
      name: "research_output_{timestamp}"
    - type: log
      message: "Research complete"
---
```

**Hook Types:**

| Hook | Pre/Post | Description |
|------|----------|-------------|
| `log` | Both | Write to session log |
| `validate` | Pre | Check input conditions |
| `persist` | Post | Save output to content store |
| `transform` | Post | Modify output format |
| `notify` | Post | Send notification |
| `retry` | Post | Retry on specific failures |

#### 5.2.3 Composition

Combine multiple agents into one:

```yaml
# agents/researcher.audited/SKILL.md
---
name: researcher.audited
composition:
  sequence:
    - agent: researcher.default
    - agent: auditor.default
      input: "$previous.output"
      condition: "$previous.status == 'success'"
---
```

Execution flow:
```
Input → researcher.default → Output 1 → auditor.default → Final Output
```

#### 5.2.4 Capability Injection

Add tools and instructions without full rewrite:

```yaml
# agents/researcher.with-escalation/SKILL.md
---
base: researcher.default
injections:
  tools:
    - session.escalate
    - session.request_clarification
  instructions:
    - "When web.fetch fails after 2 retries, escalate to reasoning_llm"
    - "If results seem incomplete, ask user for clarification"
---
```

### 5.3 Evolution Tool

New tool for creating enhanced agents:

```json
{
  "name": "agent.enhance",
  "description": "Create an enhanced version of an existing agent. Only available to evolution roles.",
  "parameters": {
    "type": "object",
    "properties": {
      "base_agent": {
        "type": "string",
        "description": "Agent ID to enhance (e.g., 'researcher.default')"
      },
      "new_agent_id": {
        "type": "string",
        "description": "ID for the enhanced agent (e.g., 'researcher.with-persistence')"
      },
      "description": {
        "type": "string",
        "description": "Description of what this enhancement adds"
      },
      "enhancements": {
        "type": "object",
        "properties": {
          "additional_instructions": {
            "type": "array",
            "items": {"type": "string"}
          },
          "hooks": {
            "type": "object",
            "properties": {
              "pre": {"type": "array"},
              "post": {"type": "array"}
            }
          },
          "additional_tools": {
            "type": "array",
            "items": {"type": "string"}
          },
          "additional_capabilities": {
            "type": "array",
            "items": {"type": "object"}
          }
        }
      },
      "reason": {
        "type": "string",
        "description": "Why this enhancement is needed (for audit trail)"
      }
    },
    "required": ["base_agent", "new_agent_id", "enhancements", "reason"]
  }
}
```

### 5.4 Evolution Workflow

```
1. Agent fails repeatedly in a pattern
2. Log/monitoring detects pattern
3. Suggestion generated: "researcher.default could benefit from persistence"
4. Human reviews suggestion
5. If approved: specialized_builder creates researcher.with-persistence
6. Future tasks can use enhanced agent
7. Metrics track: is enhanced version better?
```

### 5.5 Lineage Tracking

Every evolved agent tracks its lineage:

```json
{
  "agent_id": "researcher.with-persistence",
  "lineage": {
    "base": "researcher.default",
    "enhancements": [
      {
        "version": 1,
        "created_at": "2026-03-23T14:00:00Z",
        "created_by": "specialized_builder.default",
        "session_id": "demo-session-1",
        "reason": "Researcher was not persisting outputs, causing downstream failures"
      }
    ]
  }
}
```

---

## 6. Integration Points

### 6.1 Gateway Changes

| Component | Change |
|-----------|--------|
| Task completion | Create implicit artifact |
| workflow.wait | Include output artifact in response |
| content.read | Enhanced error messages with hints |
| Tool registry | Add session.escalate tool |
| Agent loader | Support base inheritance, hooks, injections |
| Session metadata | Track escalation count |

### 6.2 SKILL.md Updates

| Agent | Update |
|-------|--------|
| planner.default | Use artifact IDs from workflow.wait, don't guess names |
| researcher.default | Consider persisting outputs, use escalation when stuck |
| architect.default | Write content with semantic names |
| specialized_builder.default | Support agent.enhance tool |
| **All agents** | **Resumption handling guidance** (see below) |

#### Resumption Handling Guidance (Add to All Agents)

```markdown
## Handling Resumption After Hibernation

Sometimes tool calls require approval. When this happens:

1. **You will hibernate** - this is normal behavior
2. **When you wake**, your tool result will have `resumed: true`
3. **This is a continuation** - your previous tool call completed
4. **DO NOT restart your task** - continue from where you left off

### How to Resume

Look at your conversation history to understand where you were:
- What was your goal?
- What step were you on?
- What's the next step?

**Never start over when resumed - always continue your plan.**
```

### 6.3 Configuration

```yaml
# Gateway config additions
implicit_artifacts:
  enabled: true
  ttl_hours: 24
  include_tool_outputs: true

escalation:
  enabled: true
  reasoning_model: "o1-preview"
  default_specialist: "helper.default"
  max_per_session: 10

evolution:
  enabled: true
  allowed_roles: ["specialized_builder.default", "evolution-steward.default"]
  require_approval: true
```

---

## 7. Migration Path

### Phase 1: Implicit Artifacts (Week 1-2)

1. Implement implicit artifact creation on task completion
2. Update workflow.wait to include artifact reference
3. Add enhanced error messages to content.read
4. Update planner SKILL.md to use artifact IDs

### Phase 2: Escalation (Week 2-3)

1. Implement session.escalate tool
2. Add escalation capability to SKILL.md schema
3. Update key agents with escalation guidance
4. Add escalation tracking to session metadata

### Phase 3: Agent Evolution (Week 3-5)

1. Implement base inheritance in agent loader
2. Implement hooks system
3. Implement agent.enhance tool
4. Create first enhanced agents based on observed failures

---

## 8. Open Questions

1. **Implicit artifact TTL**: Is 24h appropriate? Should it be configurable per agent type?

2. **Escalation costs**: Should escalation costs be attributed to the session? To the agent?

3. **Evolution approval**: Should agent.enhance require human approval always, or only for certain changes?

4. **Hook execution order**: When multiple hooks are defined, what's the execution order? How to handle hook failures?

5. **Lineage depth**: How many levels of inheritance should we support? What happens if base is deleted?

---

## 9. Success Metrics

| Metric | Target |
|--------|--------|
| Content read failures | 80% reduction |
| Escalation usage | 20% of sessions use at least once |
| Time to resolution (with escalation) | 50% faster than retry loops |
| Enhanced agent adoption | 30% of tasks use enhanced versions within 1 month |
| Agent quality (evaluator pass rate) | 15% improvement for enhanced agents |

---

## Appendix A: Example Workflows

### A.1 Before (Current State)

```
planner spawns researcher (async)
planner spawns architect (async)
planner calls workflow.wait
  → returns: {"status": "completed", "task_id": "task-94c19ac6"}
planner tries content.read("weather_result")
  → FAIL: not found
planner guesses another name, fails again
planner proceeds with missing data
  → Low quality output
```

### A.2 After (With This Spec)

```
planner spawns researcher (async)
researcher completes → gateway creates impl_task-94c19ac6
planner spawns architect (async)
architect completes → gateway creates impl_task-fb261586
planner calls workflow.wait
  → returns: {
       "task_id": "task-94c19ac6",
       "status": "completed",
       "output": {
         "artifact_id": "impl_task-94c19ac6",
         "summary": "Fetched weather for Paris: 15°C, partly cloudy"
       }
     }
planner uses artifact_id from response
planner calls content.read("impl_task-94c19ac6")
  → SUCCESS
planner has all data needed
  → High quality output
```

### A.3 With Escalation

```
planner spawns researcher (async)
researcher gets stuck (API returns unexpected format)
researcher calls session.escalate({
  target: "reasoning_llm",
  reason: "API response format doesn't match expected schema"
})
reasoning_llm analyzes, provides parsing guidance
researcher uses guidance, succeeds
  → gateway creates impl_task-94c19ac6
```

### A.4 With Agent Evolution

```
[Session 1]
researcher.default runs, forgets to persist output
planner fails to find output
escalation recovers, but inefficient

[System]
Failure pattern detected: "researcher.default not persisting outputs"
Suggestion: "Enhance researcher with post-hook to persist"

[Session 2]
Human approves enhancement
specialized_builder creates researcher.with-persistence

[Session 3+]
planner spawns researcher.with-persistence
researcher completes → output automatically persisted
planner finds output immediately
  → No failures, no escalation needed
```

---

## Implementation Status (2026-03-23)

### ✅ Implemented

#### 3. Implicit Artifacts
- **Creation**: `create_implicit_artifact()` in `workflow_store.rs:672-719`
- **Trigger**: Automatic when task transitions to `Succeeded` status
- **Format**: `impl_{task_id}` content with `artifact_id`, `summary`, `created_at`
- **Storage**: Session-visible in parent session via `ContentStore`
- **Access**: Via `workflow.wait` response → `output.artifact_id`

#### 3.6 Enhanced Error Messages
- **Tool**: `ContentReadTool` enhanced in `tools.rs:2100`
- **Detection**: Checks for guessed names (non-SHA256) on failure
- **Hints**: `find_available_artifacts()` provides usage suggestions
- **Response**: `{ error_type, hint, available_artifacts }`

#### 4.2 Escalation Tool
- **Tool**: `SessionEscalateTool` in `tools.rs:3368`
- **Targets**:
  - `reasoning_llm`: Returns structured analysis (stub, extensible)
  - `specialist`: Suggests appropriate specialist agents
  - `human`: Returns guidance for user interaction (NOT integrated with chat yet)
- **Usage**: `session.escalate({ reason, context, target, urgency, suggested_actions })`

### 🔄 Partial Implementation

#### 4.3 Escalation Tracking
- **Spec calls for**: Escalation count, outcome tracking
- **Current**: Basic tool response, no persistent tracking
- **Future**: Add escalation tracking to prevent loops

#### 5. Agent Evolution
- **Status**: Not implemented (framework exists, no evolution mechanisms yet)
- **Capabilities required**: Composition, enhancement, hooks, capability injection

### 📋 Related Documentation

- **Workflow Events**: `docs/workflow-orchestration.md` → "Workflow Event Types"
- **Approval Flow**: `docs/spec-artifact-dedup-approval-improvements.md`
- **Chat CLI**: Real-time workflow event polling with approval visibility
