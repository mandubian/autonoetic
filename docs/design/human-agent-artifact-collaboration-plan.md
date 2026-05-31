# Human-Agent Artifact Collaboration Plan

**Status:** Draft RFC. Not implemented.

**Core proposal:** add a first-class **PlanFrame** plus an **Operator
Workbench Projection** so humans and agents can co-construct artifacts and
agents without forcing every handoff through planner-only orchestration.

**Refs:**
- `docs/workflow-orchestration.md` — durable workflows, task runs, child wake-ups.
- `docs/planner-principles.md` — planner proposes, gateway executes.
- `docs/cognitive-capsule.md` — portable artifact/agent closure.
- `docs/design/human-gate-unification-plan.md` — unified human suspension gates.
- `docs/design/operator-approval-inspection-plan.md` — code visibility during approval.

---

## 1. Problem statement

Today the planner is the main orchestration surface:

1. User asks for an artifact or agent.
2. Planner decomposes the work.
3. Planner calls specialists such as `coder.default`.
4. Specialists produce artifacts.
5. Planner/auditor/evaluator loops try to inspect everything.

This is safe and automatable, but it is not always the best human experience.
For many artifacts, especially code agents, the operator wants to:

- See the actual files in an editor or IDE.
- Make small corrections directly instead of explaining them back to an agent.
- Decide that some validations are not worth running for this iteration.
- Keep the original plan visible as the shared frame of reference.
- Resume agent work after human edits without losing provenance.

The current flow treats the human mostly as an approver at gates. The proposed
flow treats the human as a **co-builder**: the planner still coordinates, the
gateway still enforces safety boundaries, but the artifact can be projected into
a human-editable workspace and reconciled back into immutable artifact history.

The important shift:

> The operator should be able to enter the construction loop, not merely approve
> the construction loop from outside it.

---

## 2. Design goals

1. **Human-editable artifacts without mutating immutable storage.** Artifacts
   stay content-addressed and immutable. Human edits happen in a projection
   workspace, then reconcile into a new artifact revision.
2. **Plan-first collaboration.** The planner proposes a concrete plan and gets
   user approval before heavy delegation. The approved plan remains available
   throughout the workflow.
3. **Selective validation with explicit accountability.** Operators can waive
   tedious advisory checks such as unit tests in low-risk cases, but waivers are
   recorded. Mechanical safety gates remain non-waivable unless policy explicitly
   allows it.
4. **Provenance for mixed authorship.** Every artifact revision records whether
   changes came from an agent, the operator, or both.
5. **No IDE lock-in.** The gateway exposes a workspace path and metadata; clients
   may open VS Code, JetBrains, `$EDITOR`, a web IDE, or nothing.
6. **Composable with capsules.** A workbench can be exported as a capsule when
   the operator wants to move the construction context across machines.

---

## 3. Proposed primitives

### 3.1 PlanFrame

A **PlanFrame** is a durable, versioned plan owned by a workflow. It is not just
text in the chat transcript. It is structured background state that agents and
clients can inspect throughout the session.

PlanFrame should extend the existing workflow system, not create a parallel
orchestrator. The workflow remains the operational lifecycle; the PlanFrame is
the approved intent and validation contract attached to that workflow.

The workflow should point at the active plan by reference:

```rust
pub struct WorkflowRun {
    // existing fields omitted
    pub active_plan_ref: Option<PlanRef>,
}

pub struct PlanRef {
    pub plan_id: String,
    pub version: u32,
}
```

This indirection matters. Plans can be amended and versioned independently while
the workflow keeps a stable pointer to the currently active approved version.
Embedding the full plan inside `WorkflowRun` would couple plan evolution to the
workflow row and create an awkward denormalized state blob.

Conceptual schema:

```rust
pub struct PlanFrame {
    pub plan_id: String,
    pub workflow_id: String,
    pub root_session_id: String,
    pub title: String,
    pub objective: String,
    pub status: PlanStatus,
    pub version: u32,
    pub steps: Vec<PlanStep>,
    pub validation_policy: ValidationPolicy,
    pub approved_by: Option<String>,
    pub approved_at: Option<String>,
    pub created_by_agent_id: String,
    pub updated_at: String,
}

pub enum PlanStatus {
    Draft,
    AwaitingApproval,
    Approved,
    Superseded,
    Completed,
    Cancelled,
}

pub struct PlanStep {
    pub step_id: String,
    pub title: String,
    pub owner: StepOwner,
    pub status: StepStatus,
    pub depends_on: Vec<String>,
    pub task_ids: Vec<String>,
    pub artifact_refs: Vec<String>,
    pub notes: Option<String>,
}

pub enum StepOwner {
    Planner,
    Agent(String),
    Operator,
    Shared,
}
```

`PlanStep` and `TaskRun` are intentionally many-to-many in practice. A plan step
may spawn no tasks, one task, or several parallel tasks. A single task may also
contribute evidence for multiple plan steps. For the MVP, the canonical forward
links live on `PlanStep.task_ids`; avoid adding a reverse `plan_step_id` to
`TaskRun` until there is a clear query need. If a reverse link is later needed,
it should be `plan_step_ids: Vec<String>`, not a 1:1 field.

The planner uses the PlanFrame as the enduring contract:

- "Here is what we intend to build."
- "Here is who is doing which part."
- "Here is which validation is required, recommended, or waived."
- "Here is what changed since the user approved the plan."

### 3.2 Operator Workbench Projection

An **Operator Workbench Projection** is the operation that takes an immutable or
portable source and creates a mutable directory for human editing.

Projection vocabulary:

- **Projection source** — the thing being projected: an artifact, agent
  revision, or capsule.
- **Projection target** — always a workbench.
- **Projection result** — a workbench directory containing editable files plus
  metadata about the source it came from.
- **Reconciliation result** — a new immutable artifact or agent revision created
  from the edited workbench.

In other words: **artifacts, agent revisions, and capsules are projected into a
workbench; the workbench itself is not projected.** The workbench is the editable
target created by projection.

For the first implementation, `artifact.project` is the primary operation.
`capsule.project` and `agent.project` can be added later as convenience wrappers
that unpack their source into the same workbench model.

Example path:

```text
.gateway/workbenches/wb-a1b2c3/
├── WORKBENCH.md
├── planframe.json
├── source/
│   ├── SKILL.md
│   ├── runtime.lock
│   └── ...
├── .autonoetic/
│   ├── projection.json
│   ├── base_artifact.json
│   ├── validation-policy.json
│   └── provenance.log
```

The workbench is intentionally **not** the artifact store. It is an editable
working copy with a known base digest and source identity.

Projection metadata records:

```rust
pub struct WorkbenchProjection {
    pub workbench_id: String,
    pub workflow_id: String,
    pub plan_id: String,
    pub base_artifact_ref: Option<String>,
    pub base_capsule_ref: Option<String>,
    pub workspace_path: String,
    pub status: WorkbenchStatus,
    pub created_for_operator: Option<String>,
    pub created_at: String,
    pub last_reconciled_at: Option<String>,
}
```

When an artifact is projected, the gateway can tell the user:

```text
Artifact projected to:
  .gateway/workbenches/wb-a1b2c3/source

Open it with:
  code .gateway/workbenches/wb-a1b2c3/source
```

The CLI may optionally offer `--open`, but the core gateway should only create
the workbench and return the path. Launching an editor is a client concern.

### 3.3 Reconciliation

Reconciliation turns the mutable workbench back into immutable artifact history.

Conceptual commands:

```bash
autonoetic artifact project <artifact-ref> --workflow <wf-id>
autonoetic workbench status <workbench-id>
autonoetic workbench diff <workbench-id>
autonoetic workbench reconcile <workbench-id> --message "operator edits"
```

Reconcile performs:

1. Read base artifact digest from `.autonoetic/base_artifact.json`.
2. Compute file diff from the projected `source/` tree.
3. Classify authorship:
   - `agent_generated` for base files.
   - `operator_modified` for human edits.
   - `agent_modified_after_operator` for later agent changes on top.
4. Create a new immutable artifact revision.
5. Attach provenance and PlanFrame step updates.
6. Trigger the configured validation policy for the new revision.

This gives the human a normal editing loop without breaking the content-addressed
artifact model.

---

## 4. Collaboration lifecycle

### Phase 0 — Plan first

Before starting substantial artifact construction, the planner creates a draft
PlanFrame.

If no workflow exists yet, `planframe.propose` should call the same
`ensure_workflow_for_root_session` path used by `agent_spawn`. This creates an
empty workflow early, before any child tasks exist. The workflow can remain
`Active`; draft/approval state belongs on `PlanFrame.status`, not on a new
`WorkflowRunStatus::DraftPlan` variant.

Planner output should include:

- Objective and acceptance criteria.
- Proposed agent roles.
- Concrete steps.
- Expected artifacts.
- Validation policy.
- Which checks are required versus skippable.
- Known tradeoffs.

The user can:

- Approve the plan.
- Ask for changes.
- Mark steps as human-owned.
- Pre-authorize advisory validation waivers.

The gateway records approval as a human gate event:

```text
plan.approved(plan_id, version, operator_id)
```

### Phase 1 — Agent construction

The planner delegates to specialists as today, but every child task receives the
PlanFrame as background context.

Agents must treat it as a shared frame:

- Use current approved plan version.
- Report completed steps against `step_id`.
- Propose amendments when reality diverges.
- Avoid silently expanding scope.

If an agent discovers the plan is wrong, it proposes a plan amendment instead of
wandering off-road.

### Phase 2 — Projection to human

At any useful checkpoint, the planner or operator can request that an artifact
be projected into a workbench:

```text
artifact.project(artifact_ref, mode = "workbench")
```

Recommended projection points:

- After initial coder output.
- Before expensive validation.
- Before packaging/installing an agent.
- After auditor findings that are easier to fix manually than through prompts.

The planner should proactively offer projection when:

- The artifact is code-heavy.
- The next audit/test cycle is expensive.
- The needed change is small and local.
- The operator has asked to inspect or modify code.

### Phase 3 — Human edit window

The workflow enters a cooperative pause without adding new lifecycle statuses.
Use the existing workflow/task state machine and attach workbench detail as
metadata/events:

```text
WorkflowRun.status = Active | WaitingChildren | Resumable
TaskRun.status = Paused
pause_reason = "workbench"
```

If no child task is currently blocked by the edit window, the workflow can remain
`Active` with an open workbench event. If a delegated task is waiting for the
operator's edits before it can continue, that task uses the existing `Paused`
status and records the workbench ID in task metadata or workflow events.

The chat/client shows:

- Workbench path.
- Current PlanFrame summary.
- Diff status.
- Suggested files to inspect.
- Available next actions:
  - "Continue with agent"
  - "Reconcile my edits"
  - "Run selected checks"
  - "Waive selected advisory checks"
  - "Revise plan"

This is a different mental mode from approval. It is a construction pause, not a
yes/no gate.

### Phase 4 — Reconcile and resume

After edits, the operator runs reconcile or clicks "Resume".

The gateway:

1. Builds a new artifact revision from the workbench.
2. Produces a diff summary.
3. Updates provenance.
4. Applies validation policy.
5. Wakes the planner with structured context:

```json
{
  "event": "workbench_reconciled",
  "workbench_id": "wb-a1b2c3",
  "new_artifact_ref": "art-...",
  "changed_files": ["SKILL.md", "src/main.rs"],
  "operator_modified": true,
  "waived_validations": ["unit_tests"],
  "required_validations_remaining": ["static_security_review"]
}
```

The planner then continues from the same PlanFrame instead of rediscovering the
project state from chat.

---

## 5. Validation policy and audit skipping

The system should support skipping some checks, but not by pretending the checks
passed.

### 5.1 Validation classes

Proposed validation classes:

| Class | Examples | Waivable by operator? |
|---|---|---|
| Mechanical safety gate | capability enforcement, approval gates, secret redaction, constitution invariants | No |
| Security review | auditor static review, remote-access classification, credential-flow review | Usually no; configurable only for dev gateways |
| Correctness check | unit tests, integration tests, fixture evaluation | Yes, with explicit waiver |
| Quality check | style review, refactor suggestions, docs polish | Yes |
| Packaging check | dependency lock, runtime closure, capsule export validation | Depends on target environment |

The sharp edge: a skipped unit test is not a pass. It is a human decision to
continue with reduced confidence.

### 5.2 Waiver record

Every waived validation creates a durable record:

```rust
pub struct ValidationWaiver {
    pub waiver_id: String,
    pub workflow_id: String,
    pub plan_id: String,
    pub artifact_ref: String,
    pub validation_id: String,
    pub validation_class: ValidationClass,
    pub waived_by: String,
    pub reason: String,
    pub risk_acknowledged: bool,
    pub expires_at: Option<String>,
    pub created_at: String,
}
```

The promotion record should show:

```json
{
  "validation": "unit_tests",
  "status": "waived",
  "waived_by": "operator",
  "reason": "Small prompt-only SKILL.md edit; no executable code changed"
}
```

This gives operators the speed they want while preserving audit truth.

### 5.3 Planner behavior

The planner should be allowed to propose skipping tedious checks, but not decide
unilaterally.

Good planner behavior:

> "This change only touches `SKILL.md`. I recommend waiving unit tests and
> keeping static review before install."

Bad planner behavior:

> "Tests are probably unnecessary, so I skipped them."

The operator owns waiver authority. The gateway records it.

---

## 6. PlanFrame as persistent project memory

The PlanFrame should outlive a single turn. For longer work, it should behave
like lightweight project state.

### 6.1 Where it lives

Store PlanFrames under workflow state first:

```text
.gateway/scheduler/workflows/<workflow_id>/plans/<plan_id>.json
```

`WorkflowRun` stores only `active_plan_ref: Option<PlanRef>`. The active plan
reference points to the plan ID and version currently governing the workflow.
Plan amendments create a new plan version and update the workflow pointer after
approval.

Later, support project-scoped PlanFrames:

```text
.gateway/projects/<project_id>/plans/<plan_id>.json
```

Workflow scope is enough for the first implementation. Project scope is the path
to multi-day agent construction, but it should reuse the same plan reference
model rather than replacing workflow ownership.

### 6.2 How agents receive it

On every child spawn, the gateway injects a compact PlanFrame summary into the
turn-start context:

```text
[planframe]
plan_id: plan-a1b2
version: 3
status: approved
current_step: implement-agent-skill
operator_owned_steps: review-generated-code
validation_policy: static_review required, unit_tests advisory
```

Agents can request full plan details through a read-only tool:

```text
planframe.get(plan_id)
```

Updates require either:

- Planner authority for execution status updates.
- Operator approval for substantive plan amendments.
- Gateway authority for mechanical status transitions.

### 6.3 Amendments

Plans should change, but changes should be explicit.

An amendment records:

- Old version.
- New version.
- Diff summary.
- Reason.
- Proposed by agent/operator.
- Approved by operator if scope, validation, or risk changes.

This prevents the common failure mode where the planner starts with "build a
small helper" and gradually drifts into "install a new networked agent" without a
fresh human decision.

---

## 7. UX proposal

### 7.1 Chat

When a plan is proposed:

```text
Plan proposed: Build `rss-summarizer.default`

1. Draft SKILL.md and capability manifest
2. Generate helper script
3. Project artifact for your review
4. Static security review
5. Package and install

Validation:
- Required: static security review, capability check
- Advisory: unit tests, style review

Approve plan? [approve] [revise] [take over step]
```

When an artifact is projected into a workbench:

```text
Workbench ready:
  .gateway/workbenches/wb-a1b2c3/source

Suggested next move:
  code .gateway/workbenches/wb-a1b2c3/source

After editing:
  autonoetic workbench reconcile wb-a1b2c3
```

### 7.2 CLI

Candidate commands:

```bash
autonoetic plan show <plan-id>
autonoetic plan approve <plan-id>
autonoetic plan amend <plan-id> --from-file plan.json

autonoetic artifact project <artifact-ref> --workflow <workflow-id>
autonoetic capsule project <capsule-ref> --workflow <workflow-id>

autonoetic workbench status <workbench-id>
autonoetic workbench diff <workbench-id>
autonoetic workbench reconcile <workbench-id>
autonoetic workbench waive <workbench-id> unit_tests --reason "operator inspected manually"
```

### 7.3 IDE/editor integration

Do not hard-code IDE assumptions into the gateway.

Recommended layers:

1. Gateway returns projection metadata and path.
2. CLI optionally opens an editor using:
   - `$AUTONOETIC_EDITOR`
   - `$EDITOR`
   - `code` if explicitly requested with `--open code`
3. Rich clients can use the same JSON-RPC metadata to open their own IDE.

This avoids turning editor launch into a privileged gateway behavior.

### 7.4 Smooth handoff tooling

The core UX rule:

> The Chat TUI is the cockpit; the IDE/editor is the work surface.

The operator should never need to reconstruct the workflow state from memory.
Every surface should show the current plan step, workbench state, changed files,
validation posture, and the next safe actions.

#### Persistent workbench card

When an artifact is projected, the Chat TUI shows a persistent card:

```text
Workbench wb-a1b2 · Plan step 2/5 · 3 files changed
[open editor] [show diff] [checkpoint] [reconcile] [ask agent] [waive tests]
```

The card updates as the workbench changes and remains visible until the workbench
is reconciled, discarded, or archived.

#### One-command edit mode

```bash
autonoetic workbench open <workbench-id>
```

This command should:

1. Create a checkpoint before editing.
2. Open the configured editor or IDE.
3. Start a file watcher for changed files.
4. Keep the Chat TUI in `waiting for operator edits` mode.
5. On editor close, offer: `diff`, `checkpoint`, `reconcile`, `ask agent`, or
   `keep editing`.

The gateway should observe edits but never auto-reconcile them.

#### File watcher and gentle warnings

While the workbench is open, the gateway can surface lightweight warnings:

- Capability manifest changed.
- `runtime.lock` changed.
- Credential or remote-access code changed.
- Entrypoint file deleted or renamed.
- Generated artifact I/O contract may have changed.

Warnings are advisory until reconciliation. Required gates still happen after
reconcile.

#### Ask-agent from the workbench

The operator can ask the orchestrator or a specialist about the workbench:

```bash
autonoetic workbench ask <workbench-id> "Does this change break the agent contract?"
```

Initial implementation can send the current diff plus PlanFrame summary to the
selected orchestrator. Later, editor integrations can expose this as a side panel
or "ask about selection" command.

#### Reconcile wizard

`workbench.reconcile` should behave like a small wizard, not a blind write:

```text
Changed files:
- SKILL.md
- src/client.py

Semantic summary:
- Timeout changed from 30s to 60s.
- No capability changes detected.
- Agent I/O contract appears unchanged.

Recommended next actions:
[reconcile] [checkpoint first] [ask agent] [run static review] [cancel]
```

After reconciliation, the gateway wakes the orchestrator with the raw diff ref,
changed files, semantic summary, and validation state.

#### Validation picklist

After reconcile, show validations as a picklist:

```text
Recommended validations:
[x] Static security review required
[ ] Unit tests advisory
[ ] Style review advisory

Waive unit tests? reason: __________________
```

Skipping a check creates a waiver record; it never records a skipped check as a
pass.

#### Return-to-agent button

The smoothest handoff is a single TUI action:

```text
[return to agent]
```

It should:

1. Check whether there are unreconciled edits.
2. Offer checkpoint/reconcile if needed.
3. Generate the semantic change summary.
4. Attach diff refs and validation state.
5. Wake the selected orchestrator with structured context.

This keeps the operator out of JSON/tool details while preserving the gateway's
audit trail.

---

## 8. API sketch

### 8.1 New tools

Read-only / low-risk:

- `planframe.get`
- `planframe.list`
- `workbench.status`
- `workbench.diff`
- `workbench.checkpoints`

Mutation / gated:

- `planframe.propose`
- `planframe.amend`
- `planframe.approve`
- `artifact.project`
- `capsule.project`
- `workbench.open`
- `workbench.checkpoint`
- `workbench.checkout`
- `workbench.ask`
- `workbench.reconcile`
- `validation.waive`

### 8.2 Gate integration

Use `GateService` for:

- Plan approval.
- Plan amendment approval.
- Validation waiver approval.
- Human edit pause/resume.

This avoids inventing a third suspension mechanism. A workbench pause is a human
gate with richer state, not a special lifecycle exception.

### 8.3 Policy integration

Add capability gates:

- `ArtifactProjection` — create workbench projections.
- `PlanMutation` — propose/amend PlanFrames.
- `ValidationWaiver` — request waivers.
- `WorkbenchReconcile` — ingest mutable workspace into artifact store.

Planner may propose and request. Gateway enforces who can approve.

---

## 9. Security and safety model

### 9.1 Immutable base, mutable overlay

The artifact store remains immutable. Human edits happen in a mutable overlay.
Reconciliation creates a new artifact revision. This preserves:

- Reproducibility.
- Diffability.
- Provenance.
- Rollback.

### 9.2 Workbench path safety

Workbench creation must defend against:

- Symlink escape from `source/`.
- Path traversal in artifact file names.
- Secret files accidentally included during reconcile.
- Hidden editor files (`.env`, swap files, local caches) being captured.

Default reconcile should ignore:

```text
.git/
.env
*.swp
*.tmp
node_modules/
target/
__pycache__/
```

Operators can override with explicit include rules.

### 9.3 Human edits are not automatically trusted

Human-authored code can still be unsafe. Reconciliation records authorship; it
does not bypass required safety checks.

This is the key policy:

> Human edit authority is construction authority, not automatic execution
> authority.

### 9.4 Waivers are constrained

Waivers should be:

- Explicit.
- Scoped to one artifact revision.
- Time-bounded where useful.
- Visible in promotion records.
- Blocked for mechanical safety gates.

---

## 10. Implementation plan

### Phase 1 — PlanFrame MVP

- Add `PlanFrame`, `PlanRef`, `PlanStep`, and `ValidationPolicy` types.
- Add `WorkflowRun.active_plan_ref: Option<PlanRef>`.
- Persist PlanFrames under workflow state, versioned independently from
  `WorkflowRun`.
- Add planner-facing `planframe.propose` and read-only `planframe.get`.
- Have `planframe.propose` call `ensure_workflow_for_root_session` so planning
  can create an empty workflow before first `agent_spawn`.
- Add operator approval for plan version 1 through `GateService`.
- Inject compact approved PlanFrame summaries into child task context.
- Keep Phase 1 free of workbench-specific fields such as `workbench_ids`; those
  belong to the projection/reconcile phases once the lifecycle is concrete.

Acceptance criteria:

- Planner can propose a plan before delegation.
- User can approve or reject the plan.
- Empty workflow with draft plan survives restart and can later receive tasks.
- Regression test: `planframe.propose` creates an empty workflow, then a later
  `agent_spawn` attaches normally without unexpected status transitions or lost
  `active_plan_ref`.
- Child agents receive the approved plan summary.
- Plan status survives gateway restart.

### Phase 2 — Workbench projection

- Add `artifact.project` to project an artifact into
  `.gateway/workbenches/<id>/source`.
- Write projection metadata under `.autonoetic/`.
- Add `workbench.status`, `workbench.diff`, `workbench.checkpoint`,
  `workbench.checkpoints`, and `workbench.checkout`.
- Create automatic checkpoints on projection and before reconcile.
- Return path metadata through CLI/chat/JSON-RPC.
- Add the persistent Chat TUI workbench card with `open editor`, `show diff`,
  `checkpoint`, and `ask agent` actions.
- Add path-safety tests for traversal and symlink escape.

Acceptance criteria:

- Existing artifact can be projected into a mutable directory.
- Operator can inspect and modify files with any editor.
- Gateway can show diff against the base artifact.
- Operator can checkpoint and restore the workbench without git.
- Projection does not mutate the original artifact.

### Phase 3 — Reconcile into artifact revision

- Add `workbench.reconcile`.
- Create a new immutable artifact revision from edited source.
- Record provenance and changed-file summary.
- Generate a semantic change summary before waking the planner/orchestrator,
  using the configured lightweight/compression preset.
- Wake planner with `workbench_reconciled` structured context.
- Update PlanFrame step artifact refs.

Acceptance criteria:

- Human edits become a new artifact revision.
- Planner resumes with the new artifact ref.
- Planner receives raw diff ref, changed files, semantic summary, and
  validation state.
- Provenance distinguishes operator-modified files.

### Phase 4 — Validation waivers

- Add `ValidationPolicy` classes.
- Add `validation.waive` gated by operator approval.
- Render waived validations in promotion records and traces.
- Prevent waivers for mechanical safety gates.
- Add the TUI validation picklist after reconcile.

Acceptance criteria:

- Operator can waive unit tests for a specific artifact revision.
- Waiver is visible in workflow trace and promotion record.
- Required safety checks still block promotion/execution.

### Phase 5 — Project-scoped PlanFrames and capsules

- Promote PlanFrames from workflow-scoped to project-scoped where requested.
- Support `capsule.project` into a workbench.
- Support exporting a reconciled workbench plus PlanFrame as a capsule draft.
- Use PlanFrame objective and active steps as a relevance lens for context
  compression / State Capsule generation.

Acceptance criteria:

- Multi-session project can reuse the same approved plan.
- Capsule import can create a workbench for human review.
- Workbench state can be moved between machines without losing provenance.
- Context compression preserves plan-advancing decisions, artifact refs,
  operator decisions, and waivers more strongly than abandoned detours.

### Phase 6 — Return-to-agent flow

- Add `workbench.open` as one-command edit mode.
- Add `workbench.ask` for operator questions about current workbench state.
- Add file watcher warnings for risky changes.
- Add `return to agent` TUI action that checks for unreconciled edits, offers
  checkpoint/reconcile, then wakes the selected orchestrator.

Acceptance criteria:

- Operator can move from Chat TUI → editor → reconcile → orchestrator wake-up
  without manually copying paths, diffs, artifact refs, or JSON payloads.
- File watcher warnings identify capability, credential, remote-access,
  runtime-lock, and entrypoint changes before reconcile.
- `return to agent` refuses to silently drop local edits.

---

## 11. Planner policy changes

Update planner instructions with these principles:

1. **Propose before building** when work is multi-step, expensive, or installable.
2. **Offer projection** when human inspection would be cheaper than another
   agent loop.
3. **Treat the PlanFrame as the shared contract**, not as disposable chat text.
4. **Ask before waiving validation**; never silently skip checks.
5. **Prefer small reconciliations** after human edits so diffs stay reviewable.
6. **Amend the plan when scope changes** instead of drifting.

Suggested planner phrase:

> "I can run another coder/auditor loop, or I can project this artifact into a
> workbench so you can edit it directly. Since this looks like a small local
> change, I recommend projection, then static review only."

That is the desired posture: proactive, practical, and still accountable.

---

## 12. Open questions

1. **What counts as a project?** Workflow-scoped PlanFrames are straightforward.
   Project-scoped PlanFrames need a project identity model.
2. **How should operator edits be detected?** File diff is enough for MVP.
   Later, editor integration could mark intentional edits more precisely.
3. **Should workbenches be inside `.gateway/` or user-visible project dirs?**
   `.gateway/workbenches` is safer and easier to clean. User-visible dirs are
   nicer for IDE ergonomics.
4. **How long do workbenches live?** Decision before Phase 2: reconciled
   workbenches may follow workflow-retention cleanup, but unreconciled workbenches
   are never deleted without explicit operator confirmation. If the workflow
   completes while a workbench has local edits, the gateway should warn and keep
   the workbench until the operator reconciles, discards, or archives it.
5. **Can agents edit the same workbench concurrently?** Recommendation: no for
   MVP. Reconcile human edits first, then let agents work from the new artifact.
6. **How much validation can be skipped in dev mode?** Recommend a gateway config
   profile: `strict`, `standard`, `dev`. Only `dev` permits waiving security
   review, and promotion/install surfaces the waiver loudly.

---

## 13. Non-goals

- Not live collaborative editing between an IDE and an agent in the same file.
- Not mutable artifact storage.
- Not a way for humans to bypass capability enforcement.
- Not automatic trust of human-written code.
- Not a replacement for auditor or evaluator agents.
- Not a requirement that every tiny task starts with a formal plan.

---

## 14. Recommended first slice

The PlanFrame MVP in Phase 1 ships first and can be released independently. It
does not require projection, reconcile, or validation waivers.

Build the smallest useful loop:

1. Planner proposes a PlanFrame and asks for approval.
2. Coder produces an artifact.
3. Gateway projects artifact to `.gateway/workbenches/<id>/source`.
4. Operator edits in their IDE.
5. Gateway reconciles edits into a new artifact revision.
6. Operator waives unit tests with a reason.
7. Static review still runs before install/promotion.

This slice changes the feel of the system immediately: Autonoetic becomes less
"agents disappear and return with an artifact" and more "agents and human share
a workbench, with the gateway keeping the ledger honest."
