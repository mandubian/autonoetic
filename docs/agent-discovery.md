# Agent Discovery

## The Problem

The planner knows foundational agents by name. But as users install custom agents, create service-specific specialists, or evolve purpose-built agents, the planner cannot statically enumerate them all. A hardcoded list rots; a principle-first planner needs a dynamic fallback.

The challenge: the gateway is a **dumb policy/execution engine**. It enforces capabilities, manages the vault, and runs sandboxes. It does not understand which agent fits which intent. Semantic routing is not a gateway concern.

The solution: separate enumeration (dumb, gateway-level) from semantic matching (smart, agent-level).

---

## Architecture

```
Planner
  │
  ├─ Foundational agents (knows by name: researcher, executor, coder, builder, ...)
  │
  └─ Unknown intent → discovery.default
                           │
                           ├─ agent_list (gateway tool — dumb enumeration)
                           │    └─ queries SQLite agent registry
                           │    └─ returns: agent_id, description, capabilities, execution_mode
                           │
                           └─ LLM reasoning over list vs task_description
                                └─ returns: ranked_candidates, recommendation, needs_new_agent
```

The gateway's job is enumeration. The agent's job is understanding. Neither does the other's work.

---

## `agent_list` — Gateway Tool

`agent_list` is a read-only directory query. It scans the installed agent filesystem and returns structured metadata.

**Capability required:** `SandboxFunctions` (lower privilege than `AgentSpawn` — any agent that can call knowledge tools can enumerate agents)

**Arguments:**

| Field | Type | Description |
|---|---|---|
| `filter_prefix` | string? | Only agents whose `agent_id` starts with this (e.g. `"specialists/"`) |
| `requires_capability` | string? | Only agents declaring this capability type (e.g. `"NetworkAccess"`) |
| `execution_mode` | string? | `"reasoning"` or `"script"` |

**Return shape:**

```json
{
  "ok": true,
  "agents": [
    {
      "agent_id": "researcher.default",
      "description": "Research-focused autonomous agent for evidence collection.",
      "capabilities": ["NetworkAccess", "ReadAccess", "WriteAccess"],
      "execution_mode": "reasoning"
    }
  ],
  "count": 1
}
```

**Design constraints:**
- Returns only installed agents (those with a `SKILL.md` in the agents directory)
- No semantic scoring — that belongs in the agent layer
- All three filter args are independent and combinable
- Empty filter set returns all agents

---

## `discovery.default` — Semantic Matching Agent

`discovery.default` calls `agent_list`, reasons about the results against the task description, and returns ranked candidates. It is a thin reasoning agent with no side effects.

**Capabilities:** `SandboxFunctions: ["agent.", "knowledge."]` — can call `agent_list` and `knowledge_recall` for prior context, nothing else.

**Input (from spawn message):**

```
Find an agent for: <task_description>
Required capabilities: [NetworkAccess, CredentialAccess]  (optional)
exclude_foundational: true  (optional — skip agents planner already knows)
```

**Output:**

```json
{
  "ranked_candidates": [
    {"agent_id": "moltbook-ops", "score": 0.9, "rationale": "Exact match: posts to Moltbook feed with CredentialAccess."},
    {"agent_id": "social-poster", "score": 0.6, "rationale": "Generic social posting, may lack Moltbook-specific setup."}
  ],
  "recommendation": "Use moltbook-ops — it was built specifically for this service.",
  "confidence": "high",
  "needs_new_agent": false
}
```

`needs_new_agent: true` signals that no installed agent fits. The planner then delegates to `agent-factory.default`.

**How discovery works:**
1. Call `agent_list` (with `requires_capability` filter if applicable)
2. Optionally call `knowledge_recall` to retrieve prior context about agent usage for similar tasks
3. Reason: does each agent's `description` match the intent? Does its `capabilities` set enable the required operations?
4. Score and rank. Return candidates with brief rationale.

---

## Planner Usage Pattern

```
# 1. Try foundational agents first (no overhead)
#    researcher, executor, coder, architect, evaluator, auditor, packager,
#    specialized_builder, debugger, registration, agent-factory

# 2. If none clearly fit, spawn discovery
agent_spawn("discovery.default", message="Find an agent for: post to Moltbook feed. Required capabilities: [CredentialAccess]")

# 3. Read recommendation
#    → ranked_candidates[0].agent_id = "moltbook-ops"
#    → recommendation = "Use moltbook-ops — CredentialAccess + Moltbook-specific instructions"
#    → needs_new_agent = false

# 4a. Candidate found → spawn it
agent_spawn("moltbook-ops", message="Post: ...")

# 4b. No candidate found (needs_new_agent: true) → build it
agent_spawn("agent-factory.default", message="Build an agent that posts to Moltbook feed. Capabilities: [CredentialAccess, NetworkAccess, ReadAccess, WriteAccess]")
```

---

## When NOT to Use Discovery

Discovery is wasted overhead when the intent clearly maps to a foundational agent:
- "fetch this URL" → `researcher.default` (not discovery)
- "run this quick shell command or one-off script" → `executor.default` (not discovery)
- "write a reusable Python script" → `coder.default` (not discovery)
- "register with Moltbook" → `registration.default` (not discovery)
- "build a new agent" → `agent-factory.default` (not discovery)
- "debug this failure" → `debugger.default` (not discovery)

Use discovery only when: the task requires a domain-specific or user-installed agent that the planner cannot name from its foundational vocabulary.

---

## Gateway Design: Why the Tool is Dumb

`agent_list` deliberately does no semantic scoring. This preserves the gateway's role as a neutral executor.

If the gateway tried to match intent to agents, it would need to understand domain semantics, agent evolution patterns, and task decomposition — all of which belong to the reasoning layer. A dumb enumeration tool + a smart reasoning agent is strictly better:

- The gateway stays generic: no code changes when new agent types are installed
- The discovery agent can be replaced or evolved without gateway changes
- The tool can be called by any agent with `SandboxFunctions`, not just discovery — other agents can enumerate installed agents for their own reasoning

The same pattern applies to `agent_discover` (keyword-based, existing) and `agent_list` (structured enumeration, new). Discovery.default uses `agent_list` because it can reason; simple keyword lookup is insufficient for semantic intent matching.
