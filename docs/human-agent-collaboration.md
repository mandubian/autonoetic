# Human-Agent Artifact Collaboration

The collaboration system lets operators and agents co-edit artifacts through a structured **PlanFrame → Workbench → Reconcile → Return** lifecycle. The agent proposes a plan; the operator edits files in a projected workbench; edits are reconciled into immutable artifact revisions; the agent resumes with full context of what changed.

## Getting Started

```bash
autonoetic run -c
```

The `--collaborative` (or `-c`) flag selects `planner.collaborative`, which has the `PlanFrameAccess` capability required for all collaboration tools. The TUI will automatically show workbench status and enable `/wb`, `/return`, and (when configured) `/waive` commands when a workbench is active.

Without this flag, `autonoetic run` uses `planner.default` which does not have collaboration tools.

You can also use any agent with `PlanFrameAccess` explicitly:

```bash
autonoetic run planner.collaborative
autonoetic chat --agent planner.collaborative
```

All collaboration tools require the `PlanFrameAccess` capability.

## Core Concepts

### PlanFrame

A versioned, immutable plan document owned by a workflow. Agents propose plans; operators approve them before work begins. Each amendment creates a new revision — the previous version is preserved unchanged.

```
plan_id            Unique identifier (e.g. plan-abc123)
workflow_id        Parent workflow
version            Monotonically increasing revision number
status             AwaitingApproval | Approved | Superseded
title              Short title
objective          Detailed objective and acceptance criteria
steps              Ordered list of plan steps with owner, dependencies, notes
validation_policy  Required checks (correctness, quality, packaging)
```

### Workbench

A mutable, file-level copy of an artifact projected into a directory the operator can edit directly with any tool (editor, IDE, CLI). The original artifact remains immutable. Edits are tracked via SHA-256 digests.

```
workbench_id       Unique identifier (e.g. wb-abc123)
workflow_id        Parent workflow
base_artifact_id   The immutable artifact this workbench was projected from
workspace_path     Local directory containing the editable file copy
status             Active | Reconciled | Discarded
```

### Semantic Summary

A deterministic classification of every changed file produced during reconciliation. Identifies contract-impacting changes (capability declarations, runtime locks, credential shapes, network access) so the agent knows what was modified without re-reading every file.

### Validation Waiver

A durable, auditable record that an operator chose to skip a specific validation check. Mechanical safety gates and security reviews **cannot** be waived. Waivers are visible in promotion records and traces.

The operator-facing waiver workflow is **opt-in**. Set `validation_waivers.enabled: true` in the gateway config to enable it. When enabled, the Chat TUI provides `/waive` to trigger the workflow and `/waive <validation_id> [reason...]` to ask the orchestrator to waive a specific advisory validation. Set `validation_waivers.auto_propose_after_reconcile: true` to have `workbench reconcile` automatically set `propose_waivers: true` in `reconciliation.json`, signaling clients that they may offer a waiver picklist.

## Lifecycle

```
1. AGENT proposes a PlanFrame  →  status: awaiting_approval
2. OPERATOR approves the plan   →  status: approved
3. AGENT builds artifact, projects into workbench
4. OPERATOR edits files in the workbench directory
5. OPERATOR reconciles edits   →  new immutable artifact revision
   OR discards the workbench   →  status: discarded
6. OPERATOR returns to agent   →  agent resumes with semantic summary
```

## Tools

### PlanFrame Tools

| Tool | Description |
|------|-------------|
| `planframe_propose` | Create a new plan. Requires approval before agents act on it. |
| `planframe_get` | Get a plan by ID (latest version) or by active plan for the current workflow. Supports `compact` mode. |
| `planframe_list` | List all plans for the current workflow. |
| `planframe_approve` | Move a plan from `awaiting_approval` to `approved`. Typically operator-invoked. |
| `planframe_amend` | Create a new immutable revision of a plan. Approved plans require re-approval after amendment. |
| `planframe_history` | Get the full revision history of a plan. |

#### `planframe_propose`

```json
{
  "title": "Add REST API endpoint",
  "objective": "Add a GET /health endpoint returning 200 OK",
  "steps": [
    {
      "step_id": "s1",
      "title": "Implement endpoint",
      "owner": "coder",
      "depends_on": []
    },
    {
      "step_id": "s2",
      "title": "Write tests",
      "owner": "coder",
      "depends_on": ["s1"]
    }
  ],
  "validation_policy": {
    "required_checks": ["unit_tests", "style_review"],
    "waiver_allowed": ["style_review"]
  }
}
```

#### `planframe_amend`

All fields are optional — only provided fields are updated. The `reason` field records why the amendment was made.

```json
{
  "plan_id": "plan-abc123",
  "title": "Add REST API endpoint (revised)",
  "steps": [/* complete replacement step list */],
  "reason": "Added integration test step"
}
```

### Workbench Tools

| Tool | Description |
|------|-------------|
| `artifact_project` | Project an artifact into an editable workbench directory. Auto-creates a checkpoint. |
| `workbench_status` | Get status, file listing, and modification state. |
| `workbench_diff` | Compare current files against the base artifact. Returns added, modified, deleted, unchanged. |
| `workbench_checkpoint` | Create a named checkpoint of the current workbench state. |
| `workbench_checkpoints` | List all checkpoints for a workbench. |
| `workbench_checkout` | Restore a workbench to a previous checkpoint. |
| `workbench_reconcile` | Reconcile edits into a new immutable artifact revision. Auto-checkpoints before reconciling. Produces a semantic summary. |
| `workbench_discard` | Discard without reconciling. Preserves directory for audit. |
| `workbench_cleanup` | Delete a reconciled or discarded workbench (directory, checkpoints, metadata, SQLite record). Refuses active workbenches. |

#### `artifact_project`

```json
{
  "artifact_ref": "ar.1a2b3c4d5e6f",
  "plan_id": "plan-abc123"
}
```

Returns `workbench_id` and `workspace_path`. Files are copied (not symlinked) so the operator can edit them directly.

#### `workbench_reconcile`

```json
{
  "workbench_id": "wb-abc123",
  "message": "Fixed error handling in health endpoint"
}
```

Produces:
- A new immutable artifact with a fresh `artifact_ref`
- `reconciliation.json` with provenance (base, new, operator-modified, added, deleted files)
- `semantic_summary.json` with file classifications and contract impact analysis

#### `workbench_cleanup`

Only works on `Reconciled` or `Discarded` workbenches. Active workbenches must be reconciled or discarded first.

```json
{
  "workbench_id": "wb-abc123"
}
```

Deletes the workbench directory, checkpoint snapshots, `.autonoetic/` metadata files, and the SQLite record. Reports any filesystem deletion failures as `warnings` in the response.

### Validation Waiver Tools

| Tool | Description |
|------|-------------|
| `validation_waive` | Record a waiver for a specific validation check on an artifact. |
| `validation_waivers` | List waivers for an artifact or workflow. |

#### `validation_waive`

```json
{
  "artifact_id": "art-abc123",
  "validation_id": "style_review",
  "validation_class": "quality_check",
  "reason": "Style issues are cosmetic and will be addressed in follow-up"
}
```

Validation classes:
- `correctness_check` — functional correctness (unit tests, integration tests)
- `quality_check` — code quality (style, lint, review)
- `packaging_check` — build/packaging (dependencies, layer validation)

**Non-waivable**: `mechanical_safety` and `security_review` checks cannot be waived.

## Checkpoints

Checkpoints are automatic snapshots of workbench state:

| Trigger | Label |
|---------|-------|
| Projection | `auto: projection` |
| Before reconcile | `auto: pre-reconcile` |
| Manual via `workbench_checkpoint` | User-specified label (default: `manual`) |

Auto-checkpoints are best-effort — failure does not block the parent operation. Manual checkpoints return `{ ok: false, error }` on failure so the operator gets feedback.

Use `workbench_checkout` to restore any checkpoint by ID.

## Semantic Summary

Generated automatically during `workbench_reconcile`. The rule-based summarizer (`rule_based_v1`) classifies every changed file:

### File Roles

| Role | Detection |
|------|-----------|
| `capability` | `capabilities.yaml`, `agent.toml`, `capabilities/` |
| `skill_manifest` | `SKILL.md`, `skills/` |
| `runtime_lock` | `runtime_lock.json`, `.autonoetic/` |
| `config_schema` | `config-template.yaml`, `schemas/` |
| `entry_point` | `main.rs`, `lib.rs`, `main.py`, `__init__.py`, `mod.rs`, `bin/` |
| `network_access` | Source files matching `RemoteAccessAnalyzer` patterns (HTTP clients, socket connections) |
| `credential` | `credentials.yaml`, `*.pem`, `*.key`, `*.crt`, `secrets/` |
| `documentation` | `*.md` |
| `test` | `tests/`, `_test.rs`, `test_*.py`, `*.spec.ts` |
| `build` | `Cargo.toml`, `package.json`, `pyproject.toml`, `requirements.txt` |
| `source_code` | `*.rs`, `*.py`, `*.ts`, `*.tsx`, `*.js`, `*.go`, `*.java` |
| `unknown` | Anything else |

### Contract Impact

Roles that affect behavioral contracts produce contract changes:

| Role | Impact |
|------|--------|
| `capability` | `CapabilityChange` |
| `skill_manifest` | `SkillManifestChange` |
| `runtime_lock` | `RuntimeLockChange` |
| `config_schema` | `ConfigSchemaChange` |
| `entry_point` | `EntryPointChange` |
| `network_access` | `NetworkAccessChange` |
| `credential` | `CredentialShapeChange` |

Source code, tests, documentation, and build files do not produce contract changes — they are informational.

The summary is persisted to `<workbench>/.autonoetic/semantic_summary.json` and surfaced to the agent during the `/return` handoff.

## Chat TUI Commands

The `autonoetic chat` session supports these slash commands for collaboration:

### Plan approval (`/plan`)

When a collaborative planner calls `planframe_propose`, the plan is stored with status
`awaiting_approval`. The chat TUI surfaces it separately from gateway gate approvals
(`apr-*`):

| Action | How |
|--------|-----|
| View pending plans | `/plan` or Ctrl+P pending overlay |
| Approve (inline) | Ctrl+A (after any `apr-*` approvals; requires `chat.inline_approvals: true`) |
| Approve (explicit) | `/plan approve` or `/plan approve <plan_id>` |

After inline approval, chat sends a wake message to the planner so execution can continue.
`/plan approve` without inline wake only updates the plan record — send a follow-up message
to the planner if needed.

### Workbench commands (`/wb`)

| Command | Description |
|---------|-------------|
| `/wb` or `/wb status` | Show active workbench status |
| `/wb diff` | Show diff of active workbench against base artifact |
| `/wb reconcile` | Reconcile active workbench edits into new artifact |
| `/wb discard` | Discard active workbench without reconciling |

### Validation waivers (`/waive`)

```
/waive [validation_id] [reason...]
```

Available when `validation_waivers.enabled: true`.

| Command | Description |
|---------|-------------|
| `/waive` | Show the active workbench's validation policy and existing waivers. |
| `/waive <validation_id> [reason...]` | Ask the orchestrator to waive a specific advisory validation. |

`mechanical_safety` and `security_review` validations cannot be waived. Waivers are recorded with a reason and surfaced in `promotion.query`.

### Memory curation (`/curate`)

```
/curate [focus notes...]
```

Runs memory curation on the current session immediately, instead of waiting for
the scheduled `memory-curator.default` cron (`auto_learning.curation_schedule`).
Optional free-text focus notes steer the curator — e.g. `/curate focus on the
retry loop` narrows what it distills. Useful right after a session where you want
its lessons captured before moving on.

### Return to agent (`/return`)

```
/return [--force] [note]
```

Hands the active workbench back to `planner.default`. Behavior:

1. **No active workbench** — shows informational message.
2. **Unsaved edits** — refuses unless `--force` is given. Lists modified, added, and deleted files and prompts to reconcile first.
3. **Reconciled or forced** — sends a `workbench_reconciled` event to the root session with:
   - Workbench ID, base and new artifact refs
   - Lists of operator-modified, operator-added, and deleted files
   - Semantic summary (from `semantic_summary.json`)
   - Optional operator note
   - Contract-impact summary line (e.g., "2 capability changes, 1 entry point change")

The agent resumes with full context of what changed, including which files the operator edited vs. which were agent-generated.

## Workflow Completion Warning

When a workflow completes while active (unreconciled) workbenches still exist, the gateway emits a `workflow.unreconciled_workbenches` event with the workbench IDs. The workflow still completes — the event is a warning, not a block. This ensures unreconciled operator edits are never silently dropped.

## Live session visibility (chat and future channels)

The chat TUI today shows **assistant replies** and **workflow events** (`task.*`, plan approval).
It does **not** stream every root-session tool call (for example `content_write`), even though
`digest.md` and `session_overview.md` record them.

A channel-agnostic fix is planned in
[`docs/design/operator-activity-feed-plan.md`](design/operator-activity-feed-plan.md): the gateway
will emit an `operator_activity` feed keyed by `root_session_id` so the terminal, Discord,
WhatsApp, and HTTP bridges all consume the same summaries.

Until that ships, use `digest.md` or `autonoetic trace` for in-run tool activity.

## Security Model

- **Immutable base**: The original artifact is never modified. Workbench files are copies.
- **Path safety**: Workbench paths are validated against traversal attacks.
- **Operator edits are not trusted**: Reconciled edits go through the same content-addressed storage and digest verification as agent-generated content.
- **Validation waivers are constrained**: Mechanical safety and security reviews cannot be waived. Waivers require a non-empty reason.
- **Active workbenches are protected**: The cleanup tool refuses active workbenches — operators must explicitly reconcile or discard first.
- **Provenance is complete**: Every reconciliation records which files the operator modified, added, or deleted, plus the base and new artifact IDs.

### Awareness and collective accountability

The collaboration lifecycle is a concrete expression of autonoetic awareness — agents that know their past (artifact history, causal chain), present (workbench state, semantic summary, `self_describe`), and future (PlanFrame, scheduled tasks, evolution paths), and that operate in an ecosystem where other agents can observe and review their work.

The security model does not rely solely on the gateway to prevent every possible violation. Every reconciliation produces an auditable record. Every workbench change is diffed, checkpointed, and classified. Auditor and evaluator agents review artifacts before promotion. The causal chain makes every action traceable. If something goes wrong — a bad edit, an unexpected change, a capability used oddly — the system detects it through the combination of mechanical enforcement, agent review, and complete auditability.

See `docs/autonoetic-concepts-for-beginners.md` for the full conceptual framing.

## Live session visibility

Root-session tool work (`content_write`, delegation, failures) is recorded in the gateway **`operator_activity`** feed, keyed by `root_session_id`. The chat TUI polls this feed during `check_signals`; future Discord/WhatsApp bridges should use the same `operator.activity.list` JSON-RPC method or the HTTP SSE stream documented in `docs/remote-agents-http-api.md`.

Design: `docs/design/operator-activity-feed-plan.md`.

## File Layout

A projected workbench creates this structure:

```
.gateway/
  workbenches/
    <workbench_id>/
      .autonoetic/
        projection.json          Workbench metadata
        base_digests.json        SHA-256 digests of original files
        checkpoints/             Snapshot directories
          <checkpoint_id>/       File copies at checkpoint time
        reconciliation.json      Provenance (created on reconcile)
        semantic_summary.json    Semantic classification (created on reconcile)
      source/                    Editable file copy
        ...artifact files...
```
