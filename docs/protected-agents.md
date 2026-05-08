# Protected Agents & Manual Recovery

This document covers two safety mechanisms for critical agents:

1. **Protected-agent promotion gate** — mechanical eval-run requirement for programmatic promotion
2. **Manual bootstrap recovery** — the operator procedure when the normal pipeline is untrusted

## The Recursive Trust Problem

`agent-factory.default` is the canonical path for creating and evolving agents. The pipeline is:

```
planner → agent-factory → specialized_builder → agent_revision_promote
```

This pipeline cannot be used to improve agent-factory itself without a circular trust dependency: a regressed agent-factory is exactly the agent that cannot be trusted to fix itself.

The same problem applies to other evolution-tier agents: `specialized_builder.default`, `evolution-orchestrator.default`, `memory-curator.default`, `evolution-steward.default`.

## Protected-Agent Promotion Gate

### How It Works

The gateway enforces a mechanical gate for agents listed in `protected_agents`:

```yaml
protected_agents:
  enabled: true
  agents:
    - agent-factory.default
    - specialized_builder.default
    - evolution-orchestrator.default
```

When a protected agent is promoted via `agent_revision_promote`:

1. **Eval evidence required**: `required_eval_run_id` must be provided and must reference a **passed** eval run for the exact revision being promoted.
2. **Standard gates still fire**: capability-delta (R++2), artifact promotion, sentinel pre-promotion gate.
3. **No eval run = blocked**: the tool returns `protected_agent_requires_eval_run` with a repair hint.

### Disabling the Gate

For development environments, set `protected_agents.enabled: false` or omit agents from the list. The gate is opt-in per agent ID.

## Manual Bootstrap Recovery

When the normal pipeline is untrusted (agent-factory regressed, eval suite unavailable, or the gateway is in emergency stop), operators can bypass the programmatic path and directly manipulate the agent revision system.

### When to Use This Procedure

- Agent-factory produces broken agents after a SKILL.md change
- The evolution pipeline is stuck in a failure loop
- The eval suite is unavailable and you need to restore a known-good agent
- Emergency stop was triggered and you need to restore critical agents

### Prerequisites

- CLI access on the gateway machine
- The known-good SKILL.md content (from git history, backup, or manual edit)
- Gateway must be running (or startable)

### Step-by-Step Procedure

#### 1. Identify the known-good revision

```bash
# List revision history for the agent
autonoetic agent revision list agent-factory.default

# Inspect the currently-active revision
autonoetic agent revision inspect agent-factory.default
```

If the current revision is broken, find the previous one from the promotion history. Each promotion records the previous revision ID.

#### 2. Rollback to the known-good revision

```bash
# Rollback to the immediately previous revision
autonoetic agent revision rollback agent-factory.default

# Or rollback to a specific known-good revision
autonoetic agent revision rollback agent-factory.default --to <revision-id>
```

Rollback is the **preferred path** — it's atomic, reversible, and recorded in the causal chain.

#### 3. If rollback is insufficient: direct edit + revision create

If the agent needs changes (not just a rollback), edit the SKILL.md directly:

```bash
# 1. Edit the agent's SKILL.md directly
vim agents/evolution/agent-factory.default/SKILL.md

# 2. Re-bootstrap the agent (creates a new revision from the edited files)
autonoetic agent bootstrap --agent-id agent-factory.default

# 3. The new revision is auto-promoted to active
```

`agent bootstrap` computes the content digest, creates a revision record, signs it with the gateway identity key, and atomically promotes it — bypassing the eval-run requirement since it's a CLI-initiated operation.

#### 4. If bootstrap is unavailable: manual revision creation

In extreme cases (broken `bootstrap` command, corrupted store):

```bash
# 1. Edit the SKILL.md
vim agents/evolution/agent-factory.default/SKILL.md

# 2. Create a revision from the agent directory
autonoetic agent revision create \
  --agent-id agent-factory.default \
  --from-dir agents/evolution/agent-factory.default

# 3. Promote (CLI does not enforce the protected-agent gate)
autonoetic agent revision promote <new-revision-id> --alias agent-factory.default
```

The CLI `revision promote` command does **not** enforce the protected-agent eval gate — it's a direct operator action, equivalent to the manual alias pin.

#### 5. Verify the fix

```bash
# Test the restored agent with a simple task
autonoetic agent spawn agent-factory.default --message "List your capabilities"

# Check the alias points to the right revision
autonoetic agent revision inspect agent-factory.default
```

### Warnings

- **Never edit files in `.gateway/revisions/` directly**. Always use the CLI commands. The content-addressed store computes digests from file contents; editing files in place corrupts the digest chain.
- **Record the reason**. Always pass `--reason` to rollback/promote commands so the causal chain documents why the recovery happened.
- **Test before trusting**. After recovery, run a simple task through the restored agent before resuming normal operations.
- **Re-enable the protected-agent gate** after recovery if you disabled it.

### Recovery from Complete Gateway Failure

If the gateway itself cannot start:

1. The agent data is in `<agents_dir>/.gateway/` — this directory is self-contained (SQLite store + revision files).
2. Back up `.gateway/` before any manual intervention.
3. Start the gateway with a minimal config pointing to the same `agents_dir`.
4. Follow the procedure above.

## Protected Agents in the Evolution Pipeline

The evolution-orchestrator already maintains an **exempt agents** list that prevents the automated evolution pipeline from modifying critical agents:

```
planner.default, coder.default, evaluator.default, auditor.default,
specialized_builder.default, agent-factory.default,
evolution-orchestrator.default, memory-curator.default,
evolution-steward.default, agent-adapter.default
```

This list is enforced by the orchestrator's instructions (not gateway code). The protected-agent gate provides a **mechanical** backstop: even if the exempt list is bypassed, the eval-run requirement blocks silent promotion.

## Future Work

- **Golden eval suites**: Pre-defined eval suites for each protected agent that serve as the canonical regression test. Any new revision must pass the golden suite.
- **Factory of last resort**: A minimal, frozen, gateway-shipped creator that can rebuild any specialist including agent-factory itself. Never runs in normal operation; activated only when the normal pipeline is untrusted.
- **Per-agent protected config**: Override eval suite requirements, minimum pass thresholds, and rollback targets per protected agent.
