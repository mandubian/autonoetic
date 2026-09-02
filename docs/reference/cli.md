# Autonoetic CLI Reference

> Complete reference for the `autonoetic` command-line interface.

## Quick Start

```bash
# Fastest path: one command does everything (config, bootstrap, gateway, chat)
autonoetic run

# Or the manual decomposed workflow:
autonoetic agent bootstrap --from ./agents/ --overwrite
autonoetic gateway start --port 8080 --config config.yaml
autonoetic chat --agent planner.default

# Inspect traces
autonoetic trace sessions
autonoetic trace show <session_id>
```

## Global Options

Accepted by every subcommand.

| Option | Description |
|--------|-------------|
| `--config <PATH>` | Path to a custom `config.yaml` or `policy.yaml` (default: `~/.autonoetic/`) |
| `--log-level <LEVEL>` | Override the gateway log level: `trace`, `debug`, `info`, `warn`, `error` |
| `--non-interactive` | Disable all prompts — required for CI |
| `-h, --help` | Print help |
| `-V, --version` | Print version |

There is no `-c` short form for `--config`.

---

## `autonoetic run`

One-command start for new users. Detects available LLM providers, generates
config, bootstraps agents, starts the gateway in-process, and opens chat —
all without requiring the user to understand the decomposed commands.

```bash
autonoetic run [OPTIONS] [AGENT_ID]

Options:
  -c, --collaborative   Use planner.collaborative (PlanFrame, workbench, /wb, /return)
  --session-id <ID>     Resume an existing session
  --resume              Resume the most recent session
  --overwrite           Re-copy and re-bootstrap reference agents
  --refresh-models      Interactively select a new provider/model
```

**Interactive setup (first run only):**

1. Detects available providers from environment variables (`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `MOONSHOT_API_KEY`, `KIMI_CODE_API_KEY`, etc.) and probes local servers (Ollama, LM Studio, vLLM, llama.cpp).
2. Presents a numbered menu to pick a provider.
3. Fetches the provider's model catalog and lets you pick a model.
4. Optionally prompts for a user persona ("Tell me about yourself").
5. Writes `~/.autonoetic/config.yaml` and `~/.autonoetic/persona.md`.
6. Bootstraps agents and starts gateway + chat.

Subsequent runs skip setup and go straight to chat.

**Collaborative mode** (`--collaborative` or `-c`):

```bash
autonoetic run -c
```

Uses `planner.collaborative` instead of `planner.default`. This agent has the `PlanFrameAccess` capability, which enables the full collaboration lifecycle: PlanFrame proposals, workbench projection, reconciliation, semantic summaries, validation waivers, and the `/return` handoff. The TUI will show workbench status and `/wb` commands when a workbench is active.

You can also select any agent by ID as a positional argument:

```bash
autonoetic run planner.collaborative
```

---

## Gateway Commands

### `autonoetic gateway start`

Start the gateway daemon (JSON-RPC + OFP + HTTP listeners).

```bash
autonoetic gateway start [OPTIONS]

Options:
  --port <PORT>        Gateway JSON-RPC port (default: from config)
  --config <PATH>      Path to a config.yaml (see reference/config.md)
```

**Environment variables:**
- `AUTONOETIC_SHARED_SECRET` — Required auth token for HTTP API and local JSON-RPC ingress
- `AUTONOETIC_LLM_BASE_URL` — Override LLM provider URL (ignored unless `AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES=true`)
- `AUTONOETIC_LLM_API_KEY` — Override LLM API key (ignored unless `AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES=true`)

### `autonoetic gateway stop`

Stop the running gateway daemon.

### `autonoetic gateway status`

Show gateway status including connected agents, MCP servers, and scheduler state.

```bash
autonoetic gateway status [--json]
```

### `autonoetic gateway preflight`

Probe host capabilities without starting the gateway: which sandbox tiers
(bubblewrap, docker, microvm, wasm) and language toolchains (python, javascript)
are runnable on this host/build. The same summary is logged at `gateway start`.
Exits non-zero when no sandbox tier is runnable at all.

```bash
autonoetic gateway preflight [--json]
```

### `autonoetic gateway constitution show`

Show the active constitution: version, canonical digest, signer, enforcement
counts, and a one-line gloss for every clause (`P-*` rules bind the agent,
`Ri-*` rights bind the gateway). A `✓` marks clauses the gateway mechanically
enforces. This is the same lightweight view clients get from the
`constitution.get` JSON-RPC method; pass `--include-text` for the full markdown.

```bash
autonoetic gateway constitution show [--include-text] [--json]
```

### `autonoetic gateway approvals`

Manage pending approval requests for `agent_revision_promote` and `sandbox_exec` actions.

```bash
autonoetic gateway approvals list [--json]
autonoetic gateway approvals approve <request_id> [--reason TEXT]
autonoetic gateway approvals reject <request_id> [--reason TEXT]
```

**Secret values** for credential-prompt approvals: the inline `--secret KEY=VALUE`
form puts the secret in the process list and shell history. Prefer `--secret-stdin`
or `--secret-file` (KEY=VALUE lines, `#` comments skipped):

```bash
autonoetic gateway approvals approve apr-xxxxxxxx --secret-stdin < secrets.env
autonoetic gateway approvals approve apr-xxxxxxxx --secret-file secrets.env
```

**Confirm phrases (R++4)**: the `list` output shows the phrase an approval's
class requires (e.g. `RevisionPromote`), so resolving a gate never depends on
failing the approve once to learn the phrase. `--json` includes it as
`confirm_phrase`.

**Unified pending view (#722):** to see *everything* awaiting you for one root
session — approvals, user interactions, escalations, and plans — in a single
oldest-first list, each annotated with the command that resolves it:

```bash
autonoetic gateway pending --root-session <root_session_id> [--json]
```

This is the CLI form of the `operator.pending` RPC, so a headless operator no
longer has to poll four separate command families.

**Approval ID format:** Short IDs like `apr-db51b7ad` (12 chars). LLMs won't truncate these.

**List output** shows each approval with its kind and details:

| Column | Description |
|--------|-------------|
| REQUEST ID | Short approval ID (`apr-xxxxxxxx`) |
| AGENT | Agent that requested the approval |
| KIND | Action type (`sandbox_exec` or `revision_promote`) |
| DETAILS | Command being executed (sandbox_exec) or revision being promoted (revision_promote) |

Example:
```
REQUEST ID                            AGENT                KIND              DETAILS
apr-3458926a                          specialized_bui…     revision_promote  promote: weather.default rev-abc123
apr-9e6420c1                          evaluator.defau…     sandbox_exec      exec: python3 -c "import requests; print(…
```

**Auto-execute:** After approval, the gateway automatically resumes the suspended session - no agent retry needed.

**Deduplication:** The gateway prevents duplicate approval requests. If an approval is already pending (or already approved) for the same operation, the existing request ID is returned instead of creating a new one.

### `autonoetic gateway deciders`

Appoint, list, or revoke a **run-scoped gate decider** — "name the night
watch". An unattended run otherwise stops at every gate needing a verdict; an
appointment seats an agent that already holds the `GateDecider` capability
(P-2.20) for one run, so those gates get a motivated ruling instead of parking
until morning.

```bash
autonoetic gateway deciders appoint --agent <agent_id> --scope <root_session_id> \
    [--kind approval] [--kind escalation] [--ceiling standard|high] \
    [--expires-at <rfc3339>] [--max-gates <n>] [--appointed-by <principal>] [--json]
autonoetic gateway deciders list [--root-session <root_session_id>] [--include-revoked] [--json]
autonoetic gateway deciders revoke <appointment_id> [--reason TEXT] [--revoked-by <principal>]
```

**The bundled occupant is `nightwatch.default`.** It holds `GateDecider { kinds: ["approval"] }` and nothing else — no network, no exec, no spawn. It reads a gate card and returns a verdict, and every additional capability would be a way for the seat to do something other than decide. It is declared as a system agent with **no schedule**: the night watch is woken by a gate opening in a scope it was appointed to, not by a clock, which also means no run can spawn it.

**It runs on the `decider` preset**, which must be a *fixed* preset. Appointing an agent on a routing preset is refused: a routing preset chooses a model per call, so the same seat would be served by different models on different gates and "which model produced this verdict" would have no answer — which would make the calibration evidence meaningless. If the preset cannot be resolved at all, the appointment is refused then rather than failing at 3am.

**The seat gets its own session.** The gateway creates a **top-level** session for the appointment, owned by the appointee with the appointing operator as its principal. Because `root_session_id` is the first segment of a session id, a top-level session is outside the run for budget, emergency stop, session approval grants, content-visibility propagation and R-10.7 — none of which needed a special case written for the decider. The run cannot starve, stop, or feed its own judge.

**An appointment never widens capabilities.** The appointee must already hold
`GateDecider` covering every kind named, checked against its promoted revision.
Appointing an agent that lacks the capability — or holds it for `approval` but
is appointed for `escalation` — is refused, not silently narrowed.

**Advisory only, for now.** Phase 1 records the agent's verdict but still parks
the gate for you. Both verdicts land on the record, and the agreement between
them is what a later binding appointment will be justified by — judgment layers
earn authority from evidence, not from assertion.

**Risk ceiling.** Gates are classified `standard`, `high`, or `critical` from
their action. The ceiling is the highest class the decider may rule on;
anything above parks for you. Two things worth knowing before choosing:

- A `sandbox_exec` with detected network hosts is **high**, not standard — so a
  `standard` ceiling decides very little in practice.
- `critical` gates (agent promotion, credential registration) are **not
  appointable at all**, and are refused when you try rather than sitting above
  a ceiling a later edit could raise. Promotion and secret delivery should not
  become delegable by one gesture.

**The appointment pins a revision, not just a name.** What gets seated is the
agent's *promoted revision* at the moment you appoint — instructions,
capabilities, and model. Promoting a new revision of the same agent does not
retroactively change what an existing appointment seated, and calibration
evidence gathered under one revision does not silently carry to another. If you
change the night watch's model, re-appoint.

**Expiry is two independent clocks.** `--expires-at` is wall-clock;
`--max-gates` is a count; whichever is reached first ends the appointment. An
appointment with neither is a standing grant, and `list` labels it as one
rather than leaving the column blank.

**Revocation** takes effect on the next gate. Verdicts already attributed stay
attributed — a ruling that was lawful when made does not become unlawful when
the seat is vacated.

Example — seat a night watch for an overnight run, network gates included,
expiring at 8am:

```bash
autonoetic gateway deciders appoint \
    --agent nightwatch.default \
    --scope root-3f2a91c4 \
    --kind approval \
    --ceiling high \
    --expires-at 2026-08-29T08:00:00Z
```

`list` without `--root-session` shows active appointments across all runs,
which is how an appointment still pointing at a finished run becomes visible:

```
APPOINTMENT                              AGENT                  SCOPE                        CEILING   MODE       STATE
apt_9c1e...                              nightwatch.default     root-3f2a91c4                high      advisory   active
```

### `autonoetic gateway approvals interactive`

Interactive TUI for reviewing, approving, and rejecting pending approval requests.

```bash
autonoetic gateway approvals interactive
```

**Keyboard controls:**

| Key | Action |
|-----|--------|
| `↑` / `k` | Move selection up |
| `↓` / `j` | Move selection down |
| `a` | Approve selected request |
| `r` | Reject selected request |
| `R` | Refresh approval list |
| `?` | Ask a question about the selected approval |
| `q` / `Esc` | Quit (also cancels a question in progress) |

The TUI displays:
- **Top**: List of pending approvals with request ID, agent, kind, and details (command for sandbox_exec, revision_id for revision_promote)
- **Middle**: Detail panel showing the selected approval's full information (session, reason, command, dependencies, capabilities, detected hosts). In Q&A mode, shows the answer to your question.
- **Bottom**: Status bar (or question input prompt when `?` is pressed)

**Q&A mode**: Press `?` to ask a question about the selected approval. The detail panel switches to show the answer. Press `Esc` to return to normal navigation. Example questions: `what URL?`, `show me the code`, `what dependencies?`, `why does this need approval?`

### `autonoetic gateway approvals ask`

Ask a natural-language question about a specific approval request from the command line.

```bash
autonoetic gateway approvals ask <request_id> "<question>"
```

Answers common questions about any approval request (pending or decided) by inspecting its stored fields:

| Question topic | Example questions |
|---|---|
| URLs / hosts | `"what URL?"`, `"which hosts?"`, `"what endpoint?"` |
| Code / command | `"what code will run?"`, `"show me the command"` |
| Dependencies | `"what packages?"`, `"what dependencies?"` |
| Agent / session | `"which agent?"`, `"what session?"` |
| Reason | `"why does this need approval?"`, `"what is the purpose?"` |
| Capabilities | `"what capabilities?"`, `"what permissions?"` |
| History | `"is this similar to a previous approval?"` |

**Examples:**

```bash
# What URL will the code access?
autonoetic gateway approvals ask apr-9e6420c1 "what URL will it access?"

# Show the full command
autonoetic gateway approvals ask apr-9e6420c1 "show me the code"

# Why is this waiting for approval?
autonoetic gateway approvals ask apr-9e6420c1 "why does this need approval?"

# What new capabilities are being added?
autonoetic gateway approvals ask apr-3458926a "what capabilities are being added?"
```

The answer is extracted directly from the stored approval record — no LLM is required.

```bash
autonoetic gateway approvals list [--json]
autonoetic gateway approvals approve <request_id> [--reason TEXT]
autonoetic gateway approvals reject <request_id> [--reason TEXT]
```

**Approval ID format:** Short IDs like `apr-db51b7ad` (12 chars). LLMs won't truncate these.

**Auto-execute:** After approval, the gateway automatically completes the install - no agent retry needed.

---

### `autonoetic gateway cron`

Inspect and operate the gateway's scheduled cron jobs.

```
autonoetic gateway cron list [--status <s>] [--owner <agent_id>] [--root-session <id>] [--limit <n>] [--json]
autonoetic gateway cron trigger <job_id> [--force]
```

- `list` — list scheduled cron jobs and their root sessions. Optional filters: `--status` (`active`/`paused`/`cancelled`), `--owner` (owning agent id), `--root-session` (root session id); `--limit` (default 100) and `--json` for machine-readable output.
- `trigger <job_id>` — fire a scheduled job **now** on the running gateway, bypassing its schedule (calls the live `scheduled_jobs.trigger` JSON-RPC method). Get the `job_id` from `cron list`. Returns `{status: "triggered"}` or `{status: "trigger_skipped"}` when an in-flight fire of the same job is already running. `--force` skips that in-flight guard and fires regardless.

Requires a running gateway (the scheduler and target agent run in-process).

### `autonoetic gateway system-agents`

Inspect and run the built-in system agents (curator, evolution orchestrator, …).

```
autonoetic gateway system-agents list
autonoetic gateway system-agents run <agent_id>
```

- `list` — list declared system agents and their status.
- `run <agent_id>` — manually trigger a system agent's run now, bypassing its schedule (same live-fire path as `cron trigger`).

---

## Agent Commands

### `autonoetic agent init`

Scaffold a new agent directory with role-specific LLM configuration.

```bash
autonoetic agent init <name> [OPTIONS]

Options:
  --template <TEMPLATE>   Template (planner, researcher, coder, auditor, generic)
  --preset <PRESET>       Named LLM preset from config (e.g., agentic, coding, fast)
  --provider <PROVIDER>   LLM provider override (openai, anthropic, gemini, openrouter)
  --model <MODEL>         LLM model override (gpt-4o, claude-sonnet-4-20250514)
```

**Examples:**

```bash
# Use template-specific default LLM (planner → claude, coder → claude)
autonoetic agent init my_coder --template coder

# Use a named preset from config
autonoetic agent init my_agent --preset coding

# Override LLM directly
autonoetic agent init my_agent --provider anthropic --model claude-sonnet-4-20250514
```

Creates:
- `SKILL.md` with manifest frontmatter and LLM config
- canonical `runtime.lock` scaffold (`gateway`/`sdk`/`sandbox` + empty agent-owned sections)
- `state/`, `history/`, `skills/`, `scripts/` directories

### `autonoetic agent presets`

List available LLM presets and template mappings.

```bash
autonoetic agent presets
```

### `autonoetic agent init-config`

Creates a default `config.yaml` with LLM presets and template mappings.

```bash
# Create config.yaml in current directory
autonoetic agent init-config

# Create at specific location
autonoetic agent init-config --output /path/to/config.yaml

# Overwrite existing config
autonoetic agent init-config --overwrite
```

The generated config includes:
- Gateway settings (ports, limits)
- LLM presets (agentic, coding, research, fallback)
- Template-to-preset mappings for role-specific LLM selection

### `autonoetic agent run`

Execute an agent directly (without gateway ingress).

```bash
autonoetic agent run <agent_id> [OPTIONS]

Options:
  --config <PATH>       Gateway config path
  --interactive         Interactive stdin chat loop
  --config FILE         Agent config for runtime
```

### `autonoetic agent list`

List all installed agents.

```bash
autonoetic agent list [--agents-dir PATH] [--json]
```

### `autonoetic agent bootstrap`

Seed reference agent bundles into the runtime agents directory.

```bash
autonoetic agent bootstrap [--from PATH] [--overwrite]
```

### `autonoetic agent credential put`

Store a secret in the encrypted vault and register a credential record.

```bash
# Interactive prompt (masked input)
autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY

# From environment variable
autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY --from-env OPENWEATHER_API_KEY

# Direct value
autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY --value "your-key"
```

| Option | Description |
|--------|-------------|
| `--service <SERVICE>` | Service name (required) |
| `--secret-name <NAME>` | Vault key for the secret (required) |
| `--from-env <VAR>` | Read secret from environment variable |
| `--value <VALUE>` | Provide secret directly |
| `--credential-id <ID>` | Credential ID (auto-generated if omitted) |
| `--inject-as <METHOD>` | Injection method (e.g., `env:API_KEY`, `bearer`) |
| `--allowed-hosts <HOSTS>` | Hosts this credential may be used with |
| `--expires-at <TIMESTAMP>` | ISO 8601 expiry timestamp |

### `autonoetic agent credential list`

List registered credentials (metadata only, never secret values).

```bash
autonoetic agent credential list [--service SVC] [--json]
```

### `autonoetic agent credential rm`

Remove a credential and its secret from the vault.

```bash
autonoetic agent credential rm <credential_id>
```

### `autonoetic agent seed`

Seed an alias to a specific revision for deterministic setup and tests.

```bash
autonoetic agent seed <agent_id> <revision_id> [--promotion-id <ID>] [--reason <TEXT>] [--json]
```

Notes:
- This performs alias movement using the same atomic alias update + promotion-history write used by promotion flows.
- Use `--promotion-id` in integration tests when deterministic promotion lineage identifiers are required.
- This is a setup convenience path; it does not expose eval-gating flags.
- Activation remains explicit: `artifact -> revision create -> promote`.

### `autonoetic agent revision list`

List revisions and their statuses, newest first.

```bash
autonoetic agent revision list [--agent-id <ID>] [--status <candidate|ready|archived|rejected>] [--limit <N>] [--json]
```

Notes:
- `--status candidate` is the operator's usual question: what the promotion gate is
  holding. A Candidate exists but is not active — `agent list` will not show it,
  which is why a stalled install used to be invisible from the CLI.
- The status match is case-insensitive; records store `Candidate`, you can type
  `candidate`.
- When the list is clipped by `--limit`, the count that was dropped is printed —
  a truncated list that reads as complete is how the thing waiting on you gets
  missed.

### `autonoetic agent revision inspect`

Inspect one revision: metadata, execution closure, and the full record.

```bash
autonoetic agent revision inspect <revision_id|agent_ref> [--json]
```

Notes:
- Accepts a full `rev_sha256:…` id or an agent ref (`agent@rev_<short>`); anything
  that is not a revision id is resolved as a ref.
- The human output ends with the full JSON record: this is the command an operator
  reaches for when deciding on a Candidate, and a summary that silently omitted a
  field would be worse than verbose.

### `autonoetic agent revision create`

Create an immutable revision from an `agent_bundle` artifact.

```bash
autonoetic agent revision create <agent_id> <artifact_id> [--base-revision-id <REV>] [--summary <TEXT>] [--json]
```

Notes:
- This path expects the artifact to already contain `SKILL.md` and the referenced `runtime.lock`.
- The gateway validates shape, scaffolds gateway-owned lock fields, and stores canonicalized lock bytes.
- For schema help from inside agent tooling, use the runtime tool `agent_revision_schema`.
- For intent-driven installs (gateway renders canonical manifest/lock), agents can use `agent_revision_create_from_intent`.

### `autonoetic agent revision promote`

Promote a revision to the active alias target.

```bash
autonoetic agent revision promote <agent_id> <revision_id> [--reason <TEXT>] [--required-eval-run-id <RUN>] [--json]
```

### `seed` vs `revision promote`

- Use `agent revision promote` for normal lifecycle movement; it supports governance/eval gating with `--required-eval-run-id`.
- Use `agent seed` for deterministic initialization/bootstrap flows (especially integration tests).
- Both commands move (or create) the alias target and emit promotion history.

### `autonoetic agent alias list`

List alias bindings and currently active revision targets.

```bash
autonoetic agent alias list [--agent-id <ID>] [--json]
```

### `autonoetic agent alias inspect`

Inspect one alias and its active revision details.

```bash
autonoetic agent alias inspect <alias_id> [--json]
```

### `autonoetic agent alias suspend`

Suspend an agent: block new sessions while leaving in-flight sessions running.
Read-only resolution (evaluation/diff) stays available, so an operator can keep
inspecting a suspended agent to decide whether to lift it. Use when an already
promoted agent is found to hold too much capability — pair with
`session.envelope.revoke` to also strip its auto-approved grants.

```bash
autonoetic agent alias suspend <alias_id> [--reason <TEXT>] [--by <WHO>] [--json]
```

- Idempotent: suspending an already-suspended agent reports no change.
- Re-promotion lifts the suspension automatically — unless the promotion was
  pre-authorized by a locked session envelope.

### `autonoetic agent alias unsuspend`

Lift a suspension so the agent can start new sessions again.

```bash
autonoetic agent alias unsuspend <alias_id> [--json]
```

> Over the gateway API these map to the `agent.suspend` / `agent.unsuspend`
> JSON-RPC methods.

### `autonoetic agent promotion-history`

Inspect durable promote/rollback history.

```bash
autonoetic agent promotion-history [--agent-id <ID>] [--json]
```

---

## Chat Command

Connect to an agent via terminal chat (routes through gateway `event.ingest`):

```bash
autonoetic chat [OPTIONS]

Options:
  --agent <ID>           Target agent ID (default: implicit routing)
  --session-id <ID>      Session identifier (auto-generated if omitted)
  --sender-id <ID>       Sender identifier (default: "terminal")
  --channel-id <ID>      Channel identifier (default: "terminal")
```

Requires `AUTONOETIC_SHARED_SECRET` in the environment so chat requests can authenticate to gateway JSON-RPC ingress.

**Explicit routing required:** `--agent` must specify a registered agent ID. The gateway requires an explicit `target_agent_id` and has no fallback lead.

**Session persistence:** `--session-id` enables multi-turn conversations with context retention.

**Commands during chat:**
- `/session` — Show known sessions in the TUI and open the session picker
- `/session new [name]` — Create a new session, optionally naming it explicitly
- `/session switch <id>` — Switch to an existing session
- `/status` — Show current session info
- `/wb` or `/wb status` — Show active workbench status (collaborative mode)
- `/wb diff` — Show diff of active workbench against base artifact
- `/wb reconcile` — Reconcile active workbench edits into new artifact
- `/wb discard` — Discard active workbench without reconciling
- `/return [--force] [note]` — Hand active workbench back to the orchestrator
- `/why [request_id]` — Explain why an approval was triggered (shows constitutional rules)
- `/policy <text>` — Route a governance request to `governance-author.default` (constitutional proposals)
- `/persona [text]` — Show or set user persona (persists to `persona.md`)
- `/cancel` — Leave the current session picker/prompt
- `/exit` or `/quit` — Exit chat
- `/help` — Show available chat commands

Workbench and `/return` commands are available when the active agent has `PlanFrameAccess` (e.g. `planner.collaborative`). See [Human-Agent Collaboration](../guide/human-agent-collaboration.md) for the full lifecycle.

---

## Room Command

The **Session Room** is a live, channel-agnostic, importance-ranked view of a
session — every actor (planner, specialists, sentinel, you) in one timeline. From
it you can watch a session, resolve approvals/clarifications, and send messages.
Like `chat`, it's a gateway API client and needs `AUTONOETIC_SHARED_SECRET`.

```bash
autonoetic room <ROOT_SESSION_ID> [OPTIONS]

Options:
  --min-altitude <LEVEL>  Lowest importance to show: detail | normal | attention | error | story  (default: normal)
  --follow                Tail the timeline live until Ctrl+C (read-only)
  --tui                   Interactive shell: scroll, drill-down, resolve gates, send messages
  --limit <N>             Max rows fetched per read (default: 200)
```

```bash
autonoetic room session-abc123                      # one-shot snapshot
autonoetic room session-abc123 --follow             # live tail (read-only)
autonoetic room session-abc123 --tui                # interactive
```

Interactive keys (abridged): `j`/`k` scroll · `a` altitude floor · `s` squash ·
`Enter` drill-down · `y`/`n` approve/reject · `r` reply to a question · `i`
message the session · `q` quit. Full guide, glyphs, and gate/answer details:
**[The Session Room](../guide/session-room.md)**.

---

## Trace Commands

### `autonoetic trace sessions`

List all sessions with causal chain activity.

```bash
autonoetic trace sessions [--agent <ID>] [--json]
```

### `autonoetic trace show`

View a session's timeline of events.

```bash
autonoetic trace show <session_id> [--agent <ID>] [--json]
```

### `autonoetic trace event`

View a specific causal chain entry.

```bash
autonoetic trace event <log_id> [--json]
```

### `autonoetic trace rebuild`

Reconstruct a unified timeline from gateway + agent causal logs.

```bash
autonoetic trace rebuild <session_id> [--json]
```

### `autonoetic trace follow`

Watch session events in real-time.

```bash
autonoetic trace follow <session_id> [--agent <ID>] [--json]
```

Press Ctrl+C to stop following.

### `autonoetic trace fork`

Fork a session from a checkpoint to explore alternative paths.

```bash
autonoetic trace fork <session_id> [OPTIONS]

Options:
  --at-turn <N>         Fork from specific turn (default: latest)
  --message <TEXT>       Branch prompt (e.g., "try a different approach")
  --agent <ID>          Fork into a different agent
  --interactive         Start chat after forking
```

### `autonoetic trace history`

View the conversation history of a session.

```bash
autonoetic trace history <session_id> [--json]
```

### `autonoetic trace contract-health`

Standing **contract-health** view: how often each constitutional clause
(principle/right) has been enforced. Reads the `enforced_rules` carried on
causal events and attributes each `P-x.y` / `Ri-x.y` rule/right ID to its owning
clause via the enforcement register. Rule IDs not present in the register are
reported as `unattributed` (a visible coverage-gap signal). See
[Contract Health](../internals/session/lifecycle.md#contract-health).

```bash
autonoetic trace contract-health [--since <RFC3339>] [--json]
```

- `--since` — only count enforcement events at or after this RFC3339 timestamp.
- `--json` — machine-readable output (`by_clause` with clause/count/title/binds, plus `unattributed`).

---

## Session Commands

Inspect and export session-level state.

### `autonoetic session show`

Print the `SessionOutcome` row for a session as JSON. The row is created when the session closes.

```bash
autonoetic session show <session_id>
```

### `autonoetic session rate`

Attach an operator rating (`thumbs-up` or `thumbs-down`) to a session's `SessionOutcome` row.

```bash
autonoetic session rate <session_id> --thumbs-up [--note "..."]
autonoetic session rate <session_id> --thumbs-down [--note "..."]
```

- `--note` is capped at 500 characters.

### `autonoetic session export`

Export a full session (root session tree) into a single human-readable archive or a structured archive directory.

```bash
# Single-file export (default: <session-id>.room.md in current directory)
autonoetic session export <session_id>
autonoetic session export <session_id> --format json --output report.json

# Structured archive directory with wiki-style Markdown + JSON + manifest
autonoetic session export <session_id> --output-dir ./archives
```

Options:

| Option | Description |
|--------|-------------|
| `-o, --output <PATH>` | Output path for a single-file export. Defaults to `<session-id>.room.md` (or `.json`). Mutually exclusive with `--output-dir`. |
| `-f, --format <FORMAT>` | Export format: `room` (default), `room-raw`, or `json`. |
| `--with-checkpoints` | Include full checkpoint files (message history) in the export. Checkpoints can be large. Implied when `--output-dir` is used. |
| `--min-altitude <LEVEL>` | Drop `detail` events when set to `normal`, `attention`, or `error`. |
| `--output-dir <DIR>` | Export into a structured archive directory instead of a single file. Layout: `<DIR>/<constitution-version>-<lock-digest-short>/<session-id>/` containing `wiki/`, `<session-id>.json`, and `MANIFEST.json`. Mutually exclusive with `--output`. |

The archive directory is cleaned on re-export for the same session, so stale pages are removed.

### `autonoetic session egress-policy`

Declare, show, or clear a session's **egress policy** — the session-scoped half of the egress source rules (see [`docs/reference/config.md`](config.md#egress-data-localization) and [RFC: data envelopes](../proposals/data-envelopes-egress-localization.md) §5.4). This is the "for this room, these sources stay local" rung: name the private sources for one session without editing gateway config.

```bash
# Emails stay local for this session; the sandbox reading ~/mail/** too.
autonoetic session egress-policy set <session_id> \
  --rule 'email.*=local_only' \
  --rule 'sandbox.exec:~/mail/**=local_only'

autonoetic session egress-policy show <session_id>
autonoetic session egress-policy clear <session_id>
```

| Option (`set`) | Description |
|--------|-------------|
| `--rule <SOURCE[:PATH]=LABEL>` | Repeatable. `SOURCE` is a tool-name pattern (dotted or snake_case, `*` suffix allowed); `PATH` narrows to a filesystem glob; `LABEL` is `unrestricted`, `local_only`, or `no_remote_model`. |
| `--default-label <LABEL>` | Session default for content no rule labels. Can only *restrict* the gateway-wide default. |
| `--set-by <WHO>` | Attribution recorded on the declaration (default `operator:cli`). |

Session rules are **added** to the operator-global `egress.rules`, and because label resolution is an intersection they can only restrict — a session can never loosen standing policy. The policy is keyed by the **root** session (children inherit it) and is deleted when that session closes or is emergency-stopped. `set` replaces any previous declaration.

The equivalent JSON-RPC methods are `session.egress_policy.set` / `.get` / `.clear`.

---

## Capsule Commands

Export, import, verify, and inspect Cognitive Capsules — portable, signed, revision-pinned agent snapshots. See [`docs/guide/cognitive-capsule.md`](../guide/cognitive-capsule.md).

Agent-initiated callers can use the equivalent `capsule.export` / `capsule.import` tools, gated by the `CapsuleExport` capability.

### `autonoetic capsule export`

Package an agent revision into a `.capsule.tar.zst` archive.

```bash
autonoetic capsule export <agent_id> \
  [--mode thin|hermetic|replay|headless]   # default: thin
  [--revision <rev_id>]                    # default: current alias target
  [--include-memory[=true|false]]          # include redacted memory snapshot;
                                           # omit to defer to config.capsule.include_memory_by_default
  [--sign[=true|false]]                    # sign with the gateway key;
                                           # omit to defer to config.capsule.auto_sign
  [--output <path>]                        # default: <agent_id>.capsule.tar.zst
  [--session-id <id>]                      # required for --mode replay
  [--root-session-id <id>]                 # required for --mode headless
  [--json]
```

Replay mode bundles the latest `SessionCheckpoint` for `--session-id`; Headless mode bundles all scheduled jobs under `--root-session-id`. Importers recreate jobs with prefixed IDs and lay the checkpoint into the receiving gateway's `runtime/checkpoints/` so the scheduler resume path can pick it up.

### `autonoetic capsule import`

Import a capsule onto this gateway. Use `--dry-run` to validate without persisting.

```bash
autonoetic capsule import <path> \
  [--verify-signature]                     # require + verify a trusted signature
  [--activate]                             # bind the alias to the imported revision
  [--dry-run]                              # validate only
  [--trust-domain local|partner|foreign]   # constrained set; clap rejects other values; platform compatibility only enforced when != local
  [--memory-conflict keep-local|overwrite-local]   # default: keep-local
  [--json]
```

### `autonoetic capsule verify`

Validate the manifest schema, the canonical digest, and the signature (when present) against the configured `capsule.trusted_signers`. Exits **non-zero** when the signature is `Mismatch`, `UntrustedSigner`, or `Malformed`. Unsigned capsules (`Absent`) pass.

```bash
autonoetic capsule verify <path> [--json]
```

### `autonoetic capsule inspect`

Print a summary of the `capsule.json` manifest — IDs, mode, included content, signature, provenance.

```bash
autonoetic capsule inspect <path> [--json]
```

---

## Skill Commands

### `autonoetic skill install`

Install a skill from a local directory.

```bash
autonoetic skill install <path>
```

### `autonoetic skill uninstall`

Remove an installed skill.

```bash
autonoetic skill uninstall <name>
```

---

## Federate Commands

### `autonoetic federate join`

Connect to a remote Autonoetic gateway for federation.

```bash
autonoetic federate join <host:port> [--name <NAME>]
```

### `autonoetic federate list`

List connected federation peers.

```bash
autonoetic federate list [--json]
```

---

## MCP Commands

### `autonoetic mcp add`

Register an MCP server for tool discovery.

```bash
autonoetic mcp add <name> [--command <CMD>] [--args <ARGS>] [--transport stdio|sse]
```

### `autonoetic mcp expose`

Run the gateway as an MCP server for external clients.

```bash
autonoetic mcp expose [--port <PORT>]
```

---

## Security Commands

Security sentinel: status, findings, triage, and red-team pattern proposals.
Behaviour is described in [`../internals/security-sentinel.md`](../internals/security-sentinel.md).

### `autonoetic security status`

Sentinel health: finding counts, triage backlog, last sweep time.

```bash
autonoetic security status
```

### `autonoetic security findings`

List security findings.

```bash
autonoetic security findings --severity critical
autonoetic security findings --finding-type credential_leak --json
```

| Option | Description |
|--------|-------------|
| `--severity <SEVERITY>` | Filter by severity: `critical`, `warning`, `info` |
| `--finding-type <TYPE>` | Filter by type, e.g. `credential_leak`, `sandbox_escape_attempt` |
| `--triage <TRIAGE>` | Filter by triage state: `pending`, `true_positive`, `false_positive`, `benign`, `deferred` |
| `--limit <N>` | Maximum findings to show (default: 50) |
| `--json` | Output as JSON |

### `autonoetic security triage` / `bulk-triage`

Record a triage decision on one finding, or on every pending finding matching a
filter.

```bash
autonoetic security triage <finding_id> --decision false_positive
autonoetic security bulk-triage --finding-type credential_leak --decision benign
```

### `autonoetic security patterns` / `pattern-accept` / `pattern-reject`

List red-team attack-pattern proposals and accept or reject them.

```bash
autonoetic security patterns
autonoetic security pattern-accept <proposal_id>
autonoetic security pattern-reject <proposal_id>
```

---

## Recording Commands

Record real HTTP traffic during agent execution, for sealed evaluation replay.

> **Requires a running gateway (#1119):** `recording list/inspect/delete/cancel`
> and `eval sealed` speak JSON-RPC (`recording.*`) rather than reading
> `gateway.db` directly. `recording start` runs an agent in-process, as before.
> Capsule commands stay offline by design.

### `autonoetic recording start`

```bash
autonoetic recording start --agent-id myagent.default --duration 5m
autonoetic recording start --agent-id myagent.default --max-requests 50
```

| Option | Description |
|--------|-------------|
| `--agent-id <ID>` | Agent to record (required) |
| `--duration <DURATION>` | Recording duration, e.g. `5m`, `1h` |
| `--max-requests <N>` | Stop after this many requests |

### `autonoetic recording list` / `inspect`

```bash
autonoetic recording list
autonoetic recording list --agent-id myagent.default
autonoetic recording inspect <fixture_set_ref>
```

---

## Eval Commands

Sealed-network evaluation against recorded fixtures.

### `autonoetic eval sealed`

```bash
autonoetic eval sealed --artifact-ref ar.xxx --fixture-set fs.yyy
autonoetic eval sealed --artifact-ref ar.xxx --fixture-set fs.yyy --agent-id sealed_evaluator.default
```

| Option | Description |
|--------|-------------|
| `--artifact-ref <REF>` | Artifact to evaluate (required) |
| `--fixture-set <REF>` | Fixture set to replay (required) |
| `--agent-id <ID>` | Evaluator agent (default: `sealed_evaluator.default`) |

---

## Review Commands

Post-promotion review results. Design: [`post-promotion-review.md`](../proposals/post-promotion-review.md).

```bash
autonoetic review status
autonoetic review status --agent-id myagent.default
autonoetic review inspect <review_id>
autonoetic review history --agent-id myagent.default --limit 10
```

| Subcommand | Description |
|------------|-------------|
| `status` | Current review status per agent |
| `inspect` | Full detail for one review |
| `history` | Historical review results |

---

## Watchdog Command

Run the divergence watchdog against one session — a second-opinion judge on
whether a session is going off the rails.

```bash
autonoetic watchdog <session_id>
```

| Argument | Description |
|----------|-------------|
| `<SESSION_ID>` | Target session to review (required) |

---

## Sentinel Experiment Command

Run the watchdog validation experiment (sentinel P4) over a labeled corpus and
write a confusion-matrix report.

```bash
autonoetic sentinel-experiment --corpus corpus.yaml
autonoetic sentinel-experiment --corpus corpus.yaml --no-tools --skip-watchdog
```

| Option | Description |
|--------|-------------|
| `--corpus <PATH>` | Labeled corpus YAML (required) — format in [`../design/divergence-sentinel-validation.md`](../proposals/divergence-sentinel-validation.md) |
| `--output <PATH>` | Report path (default: the corpus path with a `.results.md` extension, next to the corpus) |
| `--skip-watchdog` | Use cached watchdog replies in the corpus instead of re-spending LLM tokens |
| `--no-tools` | Run the watchdog tool-free (`watchdog-fast.default`, empty tool registry) — roughly an order of magnitude cheaper, and produces no side-effect rows on the target session |

> The watchdog has live tools that write to the gateway store, so running this
> against real sessions creates real side effects (planner notices, operator
> escalations). The harness reports the deltas in a Side-Effect Summary so they
> can be cleaned up. `--no-tools` avoids them entirely.

---

## Improve Command

Run the self-improvement loop: diagnose, propose, validate, deploy. See
[`self-improvement-loop.md`](../proposals/self-improvement-loop.md).

### `autonoetic improve run`

Exactly one session selector is required.

```bash
autonoetic improve run --session session-abc123
autonoetic improve run --last-sessions 10 --agent myagent.default
autonoetic improve run --since 2026-08-01 --agent myagent.default --dry-run
```

| Option | Description |
|--------|-------------|
| `--session <ID>` | Single session to improve from |
| `--last-sessions <N>` | The N most recent sessions for this agent |
| `--since <DATE>` | Sessions since this date (RFC3339 or `YYYY-MM-DD`) |
| `--agent <ID>` | Agent ID — required with `--last-sessions` or `--since` |
| `--dry-run` | Diagnose and propose, but stop before A/B replay |
| `--no-prompt` | Refuse to deploy; print the comparison report path instead |
| `--propose-code-fix` | File a GitHub issue with code-level findings from failed sessions. Requires an authenticated `gh` CLI; use with `--session` |

---

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Configuration error |
| 3 | Agent not found |
| 4 | Permission denied |
| 5 | Network/connectivity error |
| 6 | Invalid arguments |

---

## Common Workflows

### Start Gateway and Chat

```bash
# Start gateway in background
autonoetic gateway start --port 8080 &

# Chat with planner (implicit routing)
autonoetic chat
```

### Debug a Session

```bash
# List recent sessions
autonoetic trace sessions

# View session timeline
autonoetic trace show session-abc123

# Follow live events
autonoetic trace follow session-abc123

# View specific entry
autonoetic trace event causal-log-42 --json
```

### Approve Revision Promotion

```bash
# List pending approvals
autonoetic gateway approvals list

# Approve a specific request
autonoetic gateway approvals approve c19a8a50-d6c8-4c5f-aa3c-6ba119751b11 \
  --reason "Weather agent revision looks safe"
```

### Fork and Explore

```bash
# Fork from turn 5 with alternative approach
autonoetic trace fork session-abc123 --at-turn 5 \
  --message "Try a simpler implementation" --interactive
```

### Bootstrap Reference Agents

```bash
# Create config first (required for bootstrap). The gateway parses YAML —
# a TOML-style `port = 8080` will not load.
cat > config.yaml << 'EOF'
port: 8080
agents_dir: ./agents
EOF

# Bootstrap all reference bundles
autonoetic agent bootstrap --from ./agents/ --overwrite

# Start and verify
autonoetic gateway start --config config.yaml
autonoetic agent list
```
