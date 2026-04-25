# Scheduled Tasks (Cron) Guide

Operators and agents can register recurring tasks that trigger at specified intervals. The gateway's background scheduler handles durable persistence, atomic triggering, and error recovery.

## Quick Start

### Create a Scheduled Job

```json
{
  "message": "Check for new emails and summarize any urgent ones",
  "schedule_expr": "every 30 minutes",
  "target_agent_id": "researcher.default"
}
```

Call `scheduler_cron_create` with:
- `message` (required): The prompt sent to the target agent on each trigger
- `schedule_expr` (required): Cron expression or natural-language phrase
- `target_agent_id` (optional): Defaults to the calling agent
- `metadata` (optional): Arbitrary JSON attached to the job

### Supported Schedule Expressions

**Natural-language phrases:**
- `every N seconds` — e.g., `every 10 seconds`
- `every N minutes` — e.g., `every 5 minutes`
- `every N hours` — e.g., `every 2 hours`
- `every day at HH:MM` — e.g., `every day at 09:00`
- `every <weekday> at HH:MM` — e.g., `every monday at 14:30`

**Explicit cron (5 fields: minute hour day-of-month month day-of-week):**
- `*/5 * * * *` — Every 5 minutes
- `0 9 * * *` — Daily at 9:00 AM UTC
- `0 14 * * 1` — Every Monday at 2:30 PM UTC
- `0 0 1 * *` — First day of every month at midnight

All schedules are evaluated in **UTC**.

### List, Pause, Resume, Cancel

```json
// List my jobs
{ "status": "active" }

// Pause a job
{ "job_id": "sj-abc123" }

// Resume a paused job
{ "job_id": "sj-abc123" }

// Cancel permanently
{ "job_id": "sj-abc123" }
```

## Capability Requirement

Agents need `SchedulerAccess` capability to use `scheduler.cron.*` tools:

```yaml
capabilities:
  - type: "SchedulerAccess"
    patterns: ["scheduler.cron.*"]
```

## Ownership and Isolation

- Jobs are **always owned by the calling agent** — you cannot create jobs on behalf of other agents
- You can only **list, pause, resume, or cancel jobs you own**
- Each job is scoped to a root session; the `max_per_root` guardrail (default: 50) limits jobs per session

## Guardrails

| Guardrail | Default | Description |
|-----------|---------|-------------|
| `min_interval_secs` | 1 | Minimum interval between triggers |
| Sub-10s target policy | enforced | Intervals below 10 seconds require `execution_mode: script` on target agent |
| `max_per_root` | 50 | Maximum jobs per root session |
| `max_due_per_tick` | 16 | Maximum due jobs processed per scheduler tick |

Configure in `config.yaml`:

```yaml
scheduled_jobs:
  min_interval_secs: 10
  max_per_root: 25
  max_due_per_tick: 8
```

For tighter cadence, set `background_tick_secs: 1` in gateway config to reduce trigger latency.

## How Triggering Works

1. The background scheduler tick (every 5 seconds by default) loads jobs where `next_run_at <= now`
2. Each due job is **atomically claimed and advanced** — the `next_run_at` is updated to the next occurrence in the same database transaction, preventing duplicate triggers
3. A workflow task is enqueued for the target agent with the job's message
4. The agent executes via the standard async workflow path, including all approval gates

### Error Handling

If enqueueing a task fails (e.g., workflow error), the job:
- Records the error in `last_error`
- Advances `next_run_at` by 60 seconds (backoff)
- Remains active for the next tick

## Approval Preservation

Scheduled job execution uses the same workflow execution paths as `agent_spawn`. Any sandbox operations requiring approval (network access, code execution) still go through the standard approval flow. The scheduler does **not** bypass any security gates.

## Best Practices

1. **Use descriptive messages**: The message is the full prompt the agent receives. Include context about what triggered the run.
2. **Prefer explicit cron for complex schedules**: Natural-language parsing is constrained to common patterns. Use cron syntax for anything non-standard.
3. **Monitor via `scheduler_cron_list`**: Periodically check job status and `last_error` fields.
4. **Pause, don't cancel, for temporary stops**: Cancelled jobs are permanent. Use pause for temporary suspension.
5. **Set reasonable intervals**: The minimum interval is 1 second by default, but very frequent schedules consume scheduler resources.
6. **Use script mode for high frequency**: If you need intervals below 10 seconds, target an agent with `execution_mode: script`.

## Limitations (v1)

- **UTC only**: No timezone support beyond UTC
- **No cross-agent mutation**: You cannot modify another agent's jobs
- **No natural-language ambiguity resolution**: Ambiguous phrases are rejected with guidance

---

## System Agents

System agents are **declared in config** and **auto-scheduled on gateway startup**. They provide an operator-managed way to run recurring background agents without requiring any agent to self-register its own cron job.

### Declaration

In `config.yaml`:

```yaml
system_agents:
  - agent_id: evolution-orchestrator.default
    schedule: "0 */4 * * *"
    message: "Run evolution analysis cycle"
    enabled: true

  - agent_id: memory-curator.default
    # no schedule = one-shot (not auto-scheduled, but registered for manual control)
    enabled: true
```

| Field | Required | Description |
|-------|----------|-------------|
| `agent_id` | Yes | Agent ID (must be bootstrapped and promoted) |
| `schedule` | No | Cron expression. If absent, agent is registered but not auto-scheduled |
| `message` | No | Prompt sent on each trigger. Defaults to `"Scheduled run for <agent_id>"` |
| `enabled` | No | `true` (default) or `false`. Disabled agents are skipped during reconciliation |

### Startup Reconciliation

When the gateway starts, it reconciles each declared system agent:

1. Checks if the agent exists (bootstrapped and promoted)
2. Checks if an **active** cron job targeting that agent already exists
3. If no active job and `schedule` is set → creates one (owned by `"system"`)
4. If agent is disabled or missing → logs and skips

Reconciliation is **idempotent** — safe to run multiple times. Existing jobs are never modified or duplicated.

### CLI Control

```bash
# List declared system agents and their status
autonoetic gateway system-agents list

# JSON output
autonoetic gateway system-agents list --json

# Manually trigger reconciliation (create missing cron jobs)
autonoetic gateway system-agents bootstrap

# Manually trigger a specific agent run
autonoetic gateway system-agents run evolution-orchestrator.default
```

Output for `list`:

```
AGENT_ID                                 SCHEDULE             ENABLED JOB        NEXT_RUN
evolution-orchestrator.default           0 */4 * * *          true    sj-sys-abc 2026-04-20T08:00:00Z
memory-curator.default                   (oneshot)            true    -          -
```

Bootstrap output:

```
+ evolution-orchestrator.default Cron job created: 0 */4 * * * (next run: 2026-04-20T08:00:00Z)
o memory-curator.default No schedule declared (one-shot agent)
```

Icons: `+` created, `=` already exists, `-` disabled, `!` missing agent, `o` no schedule, `x` failed.

### How System Agent Jobs Differ from Agent-Created Jobs

| Aspect | System Agent Job | Agent-Created Job |
|--------|-----------------|-------------------|
| **Owner** | `"system"` | Calling agent ID |
| **Trigger** | Gateway startup reconciliation | `scheduler_cron_create` tool call |
| **Creation** | Config-driven, idempotent | Runtime, agent-initiated |
| **Control** | CLI `system-agents bootstrap` | `scheduler_cron_pause/resume/cancel` |
| **Persistence** | Same `scheduled_jobs` table | Same `scheduled_jobs` table |

Both use the same execution path — the background scheduler picks up due jobs, enqueues workflow tasks, and runs agents through the standard approval gates. System agent jobs have no special privileges.

### Example: Evolution Pipeline

```yaml
system_agents:
  - agent_id: evolution-orchestrator.default
    schedule: "0 */4 * * *"
    message: "Run evolution analysis cycle"
```

On gateway startup:
1. Reconciliation finds no active cron for `evolution-orchestrator.default`
2. Creates a system-owned cron job: `0 */4 * * *` (every 4 hours)
3. Every 4 hours, the scheduler wakes the orchestrator
4. Orchestrator spawns curator → steward → factory pipeline
5. Results surface as admin proposals and knowledge entries
