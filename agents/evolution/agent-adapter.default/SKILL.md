---
name: "agent-adapter.default"
description: "Generates wrapper agents for I/O gaps"
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
      id: "agent-adapter.default"
      name: "Agent Adapter Default"
      description: "Generates wrapper agents for bridging I/O gaps."
    llm_preset: agentic
    capabilities:
      - type: "SandboxFunctions"
        allowed: ["knowledge_", "sandbox_"]
      - type: "CodeExecution"
        patterns: ["python3 scripts/*"]
      - type: "AgentSpawn"
        max_children: 5
      - type: "WriteAccess"
        scopes: ["self.*", "skills/*"]
      - type: "ReadAccess"
        scopes: ["self.*", "skills/*"]
    validation: "soft"
    io:
      returns:
        type: object
        required: ["status"]
        properties:
          status:
            type: string
            enum: ["ok", "partial", "clarification_needed", "failed"]
            description: "Adapter generation outcome."
          artifact_ref:
            type: string
            description: "Reference to the generated wrapper artifact."
          summary:
            type: string
            description: "What was adapted and how."
          error:
            type: string
            description: "Error detail when status is failed."
---
# Agent Adapter

Generates wrapper agents for bridging I/O gaps between tools and targets.

## Behavior
- Analyze source and target schemas using `schema_diff.py`
- **Record the base's revision before generating**: call `agent_inspect` on the
  base agent and pass the promoted revision digest — `alias.revision_id` in the
  inspect output (a `rev_sha256:…` value; not `short_ref`) — to
  `generate_wrapper.py` as `--base-revision-digest`. Drift detection (roster
  `stale_base`, spawn-time advisories, promotion-time drift events) only works
  when wrapper provenance claims a digest — omit the flag only when the base
  cannot be inspected (under-claim, never guess)
- Generate wrapper scripts using `generate_wrapper.py`
- Build an artifact with `artifact_build` from the generated wrapper
- **Delegate installation to `specialized_builder.default`** — you cannot create or promote agent revisions directly
