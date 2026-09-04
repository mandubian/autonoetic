# Archived examples

Runnable examples that no longer run against a current gateway. Kept as a
record of the flow they demonstrated, not as something to execute.

| Example | Demonstrated | Why it stopped working |
|---|---|---|
| [`specialized_builder/`](specialized_builder) | The agent-birth flow — an agent building and installing another agent | Depended on the since-removed `agent.install` tool (`P-9.2`: installation is not a runtime tool), and on GNU-only `find -printf` |
| [`tiered_memory_probe/`](tiered_memory_probe) | Tiered memory reads across sessions | Same `agent.install` dependency |

The live version of the builder flow is the agent bundle at
[`../../agents/evolution/specialized_builder.default/SKILL.md`](../../agents/evolution/specialized_builder.default/SKILL.md),
which uses the revision pipeline (`content_write` → `artifact_build` →
`agent_revision_create_from_intent` → `agent_revision_promote`). For an example
that does run, see [`../quickstart/README.md`](../quickstart/README.md).
