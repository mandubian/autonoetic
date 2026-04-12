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

Call `scheduler.cron.create` with:
- `message` (required): The prompt sent to the target agent on each trigger
- `schedule_expr` (required): Cron expression or natural-language phrase
- `target_agent_id` (optional): Defaults to the calling agent
- `metadata` (optional): Arbitrary JSON attached to the job

### Supported Schedule Expressions

**Natural-language phrases:**
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
| `min_interval_secs` | 60 | Minimum interval between triggers |
| `max_per_root` | 50 | Maximum jobs per root session |
| `max_due_per_tick` | 16 | Maximum due jobs processed per scheduler tick |

Configure in `config.yaml`:

```yaml
scheduled_jobs:
  min_interval_secs: 120
  max_per_root: 25
  max_due_per_tick: 8
```

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

Scheduled job execution uses the same workflow execution paths as `agent.spawn`. Any sandbox operations requiring approval (network access, code execution) still go through the standard approval flow. The scheduler does **not** bypass any security gates.

## Best Practices

1. **Use descriptive messages**: The message is the full prompt the agent receives. Include context about what triggered the run.
2. **Prefer explicit cron for complex schedules**: Natural-language parsing is constrained to common patterns. Use cron syntax for anything non-standard.
3. **Monitor via `scheduler.cron.list`**: Periodically check job status and `last_error` fields.
4. **Pause, don't cancel, for temporary stops**: Cancelled jobs are permanent. Use pause for temporary suspension.
5. **Set reasonable intervals**: The minimum interval is 60 seconds by default. Very frequent schedules consume scheduler resources.

## Limitations (v1)

- **UTC only**: No timezone support beyond UTC
- **No second-resolution**: Minimum granularity is 1 minute
- **No cross-agent mutation**: You cannot modify another agent's jobs
- **No natural-language ambiguity resolution**: Ambiguous phrases are rejected with guidance
