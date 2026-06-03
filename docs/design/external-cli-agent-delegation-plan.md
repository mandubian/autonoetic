# External CLI Agent Delegation Plan

**Status:** Draft side-plan. Not implemented.

**Core proposal:** let Autonoetic delegate bounded implementation work inside a
workbench to existing local CLI agents such as Codex, Claude Code, or OpenCode,
while Autonoetic remains the control plane for plans, policy, provenance,
artifacts, approvals, and validation.

**Refs:**
- `docs/design/human-agent-artifact-collaboration-plan.md` — PlanFrame,
  workbench projection, reconcile, return-to-agent flow.
- `docs/workflow-orchestration.md` — workflow/task lifecycle and child wake-ups.
- `docs/separation-of-powers.md` — gateway-owned enforcement boundary.
- `docs/credential-management.md` — secrets stay in gateway vault.
- `docs/rfc/session-room-channel-agnostic-timeline.md` §5 — the Session Room
  **renders** this delegation: external-provider work surfaces as
  `ForeignAgent`-attributed timeline events carrying the provenance below.
  This plan governs the *mechanism* and authority boundary; the Session Room is
  only its presentation/attribution layer and must not relax these bounds.

---

## 1. Motivation

Autonoetic should not reimplement every coding-agent capability. The system's
value is the constitutional frame: durable intent, capability boundaries,
approval gates, artifact identity, provenance, validation, and operator control.

Existing CLI agents are often excellent at local implementation work. The right
architecture is to make them **bounded execution providers** inside an
Autonoetic-controlled workbench.

> Autonoetic owns the mission, ledger, safety boundary, and artifact lifecycle.
> External CLI agents may propose file changes inside a bounded workspace.

---

## 2. Design goals

1. **Use the best tool available.** Codex, Claude Code, OpenCode, shell scripts,
   and human editors can all be providers for a PlanFrame step.
2. **Keep policy authority in Autonoetic.** External agents cannot approve gates,
   bypass capability checks, read gateway secrets, or promote/install artifacts.
3. **Make delegation reversible.** Every external-agent run starts from a
   checkpoint and produces a diff/provenance record.
4. **Start interactive, then automate.** Interactive CLI delegation gives value
   immediately. Non-interactive delegation can ship later with tighter contracts.
5. **Provider-neutral interface.** Do not bake one vendor or one CLI protocol into
   workflow semantics.

---

## 3. Core model

```text
PlanFrame step
  → orchestrator selects execution provider
  → gateway creates/checkpoints workbench
  → external CLI agent works inside bounded cwd
  → gateway captures transcript, exit status, and diff
  → gateway reconciles/validates or returns to operator
  → orchestrator receives structured handoff context
```

External agents are not child Autonoetic agents in the capability sense. They are
local execution providers launched by the gateway or operator under a provider
profile.

---

## 4. Provider profiles

Define configured providers in `config.yaml` or a dedicated provider registry:

```yaml
external_agents:
  providers:
    - id: codex.local
      kind: cli_agent
      command: codex
      args: []
      modes: [interactive, non_interactive]
      default_sandbox: workspace_write_no_network
      transcript_capture: best_effort
      prompt_style: stdin

    - id: opencode.local
      kind: cli_agent
      command: opencode
      args: []
      modes: [interactive]
      default_sandbox: workspace_write_no_network
      transcript_capture: terminal_log

    - id: claude.local
      kind: cli_agent
      command: claude
      args: []
      modes: [interactive, non_interactive]
      default_sandbox: workspace_write_no_network
      transcript_capture: best_effort
```

Provider profile fields:

| Field | Meaning |
|---|---|
| `id` | Stable provider ID used in PlanFrame/tool calls. |
| `kind` | `cli_agent`, `script`, or future provider kinds. |
| `command` / `args` | Local executable and default args. |
| `modes` | Supported launch modes. |
| `default_sandbox` | Default sandbox/network/file policy for the provider run. |
| `transcript_capture` | How much terminal/session output can be recorded. |
| `prompt_style` | `stdin`, `argv`, `file`, or provider-specific adapter. |

Provider availability should be discoverable:

```bash
autonoetic external-agent list
autonoetic external-agent inspect codex.local
```

---

## 5. Delegation modes

### 5.1 Interactive mode — MVP

The operator stays in the loop and drives the external CLI agent directly.

```bash
autonoetic workbench delegate wb-a1b2 \
  --provider codex.local \
  --step implement-timeout-setting
```

The gateway should:

1. Create a workbench checkpoint.
2. Launch the configured CLI agent with `cwd = workbench/source`.
3. Pass a bounded task brief containing PlanFrame step, constraints, and local
   file paths.
4. Capture terminal output best-effort.
5. Watch files for changes.
6. On process exit, produce changed-file summary and offer reconcile/ask/checkpoint.

This mode avoids pretending Autonoetic can fully control third-party interactive
agents. It simply gives them a safe, well-described workspace.

### 5.2 Non-interactive mode — later

The orchestrator sends a bounded prompt to a provider and expects the process to
exit with proposed changes.

```bash
autonoetic workbench delegate wb-a1b2 \
  --provider codex.local \
  --step implement-timeout-setting \
  --non-interactive
```

Additional requirements:

- Timeout and cancellation.
- Exit-status classification.
- Prompt/result transcript capture.
- Strict no-secret prompt construction.
- Optional max changed files / max diff size.
- Retry policy owned by workflow, not provider.

Non-interactive delegation is powerful, but it should ship after the interactive
path proves the provider model.

---

## 6. Safety boundaries

External CLI agents may:

- Read and edit files inside the workbench source tree.
- Run local commands allowed by the provider sandbox.
- Produce proposed diffs.
- Explain or review code using local context.

External CLI agents may not:

- Receive gateway secrets or credential values.
- Approve their own gates.
- Promote, install, or publish artifacts directly.
- Mutate immutable artifact storage directly.
- Modify workflow/PlanFrame state except through gateway tools.
- Bypass reconciliation or required validation.

The core rule:

> External-agent output is an artifact mutation proposal, not an authority
> decision.

---

## 7. Workbench integration

External delegation should compose with workbench tooling:

- Automatic checkpoint before launch.
- File watcher warnings during execution.
- Changed-file summary after exit.
- Optional semantic diff summary before orchestrator wake-up.
- Reconcile wizard for converting edits into immutable artifact revisions.
- Provenance record naming the provider and launch mode.

Example provenance:

```json
{
  "source": "external_cli_agent",
  "provider_id": "codex.local",
  "mode": "interactive",
  "plan_id": "plan-a1b2",
  "step_id": "implement-timeout-setting",
  "checkpoint_before": "chk-003",
  "changed_files": ["src/client.py", "SKILL.md"],
  "transcript_ref": "content:sha256:..."
}
```

---

## 8. Orchestrator behavior

The collaborative orchestrator should choose external delegation when:

- The task is local code implementation or refactoring.
- A specialist CLI agent is clearly better suited than prompt-only delegation.
- The operator prefers a known local tool.
- The work is bounded to a workbench and can be validated afterward.

The orchestrator should not choose external delegation when:

- The task requires gateway-held credentials.
- The task is policy/approval decision-making.
- The task requires direct artifact promotion or installation.
- The external provider lacks an acceptable sandbox profile.

Planner phrasing:

> "This is a bounded local code edit. I can delegate it to `codex.local` inside
> the workbench, then reconcile and run static review. Autonoetic will keep the
> checkpoint, diff, and validation ledger."

---

## 9. API and CLI sketch

Read-only:

```bash
autonoetic external-agent list
autonoetic external-agent inspect <provider-id>
autonoetic workbench delegate-status <delegation-id>
```

Mutation:

```bash
autonoetic workbench delegate <workbench-id> \
  --provider <provider-id> \
  --step <plan-step-id> \
  [--interactive | --non-interactive]

autonoetic workbench delegate-cancel <delegation-id>
```

Tool/API names:

- `external_agent.list`
- `external_agent.inspect`
- `workbench.delegate`
- `workbench.delegate_status`
- `workbench.delegate_cancel`

---

## 10. Implementation phases

### Phase 1 — Provider registry and discovery

- Add provider config schema.
- Add provider availability checks.
- Add `external-agent list/inspect` CLI.
- Do not execute providers yet.

Acceptance criteria:

- Configured providers can be listed with command, modes, and availability.
- Missing executables are reported clearly.

### Phase 2 — Interactive workbench delegation

- Add `workbench.delegate --interactive`.
- Create checkpoint before launch.
- Launch provider with `cwd = workbench/source`.
- Capture terminal output best-effort.
- Watch changed files.
- On exit, show diff/checkpoint/reconcile options.

Acceptance criteria:

- Operator can launch Codex/OpenCode/Claude Code inside a workbench.
- Provider edits remain confined to workbench source.
- Gateway records provider provenance and changed files.

### Phase 3 — TUI cockpit integration

- Add workbench card action: `delegate to external agent`.
- Show running delegation status.
- Surface risky-change warnings while provider runs.
- Offer `return to agent` after provider exit.

Acceptance criteria:

- Operator can choose provider from Chat TUI.
- TUI shows provider run state and changed files.
- No manual path copying is required.

### Phase 4 — Non-interactive delegation

- Add bounded prompt brief generation.
- Add timeout/cancel support.
- Capture transcript and exit classification.
- Add diff-size and changed-file caps.

Acceptance criteria:

- Orchestrator can delegate bounded work non-interactively.
- Failed provider runs classify cleanly without corrupting workbench state.
- Required validation/reconcile still gates artifact use.

### Phase 5 — Provider selection policy

- Add orchestrator heuristics for choosing human editor vs Autonoetic specialist
  vs external CLI provider.
- Add config allow/deny list per provider and per workflow profile.
- Add trace events for provider choice rationale.

Acceptance criteria:

- Orchestrator can explain why it chose a provider.
- Operators can disable providers globally or per session.
- Provider choice is visible in workflow trace.

---

## 11. Open questions

1. **Transcript capture depth.** Some CLIs use TUI/full-screen behavior. MVP can
   capture terminal logs best-effort and rely on file diffs as canonical output.
2. **Provider sandboxing.** Should external CLIs run under the same sandbox
   machinery as `sandbox.exec`, or a narrower local process wrapper first?
3. **Network policy.** Default should be no network unless the operator explicitly
   enables it for a provider run.
4. **Secrets in local files.** Workbench reconcile should keep ignoring `.env` and
   known secret-bearing files unless explicitly included.
5. **Provider installation.** Autonoetic should discover local providers, not
   install third-party CLIs in MVP.
6. **Concurrent providers.** Recommendation: one active external provider per
   workbench in MVP.

---

## 12. Non-goals

- Not replacing Autonoetic agents.
- Not trusting external CLI agents as policy authorities.
- Not giving external agents direct access to credentials.
- Not making Autonoetic depend on any single CLI agent vendor.
- Not requiring non-interactive automation for MVP.
