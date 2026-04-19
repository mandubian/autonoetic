# Autonoetic CLI Reference

> Complete reference for the `autonoetic` command-line interface.

## Quick Start

```bash
# Bootstrap reference agents and start gateway
autonoetic agent bootstrap --from ./agents/ --overwrite
autonoetic gateway start --port 8080 --config gateway.toml

# Chat with an agent
autonoetic chat --agent planner.default

# Inspect traces
autonoetic trace sessions
autonoetic trace show <session_id>
```

## Global Options

| Option | Description |
|--------|-------------|
| `-c, --config <PATH>` | Path to gateway.toml config file |
| `--non-interactive` | Disable interactive prompts |

---

## Gateway Commands

### `autonoetic gateway start`

Start the gateway daemon (JSON-RPC + OFP + HTTP listeners).

```bash
autonoetic gateway start [OPTIONS]

Options:
  --port <PORT>        Gateway JSON-RPC port (default: from config)
  --config <PATH>      Path to gateway.toml
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

### `autonoetic gateway approvals`

Manage pending approval requests for `agent.revision.promote` and `sandbox.exec` actions.

```bash
autonoetic gateway approvals list [--json]
autonoetic gateway approvals approve <request_id> [--reason TEXT]
autonoetic gateway approvals reject <request_id> [--reason TEXT]
```

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
| `q` / `Esc` | Quit |

The TUI displays:
- **Top**: List of pending approvals with request ID, agent, kind, and details (command for sandbox_exec, revision_id for revision_promote)
- **Middle**: Detail panel showing the selected approval's full information (session, reason, command, dependencies, capabilities, detected hosts)
- **Bottom**: Status bar showing the result of the last action

```bash
autonoetic gateway approvals list [--json]
autonoetic gateway approvals approve <request_id> [--reason TEXT]
autonoetic gateway approvals reject <request_id> [--reason TEXT]
```

**Approval ID format:** Short IDs like `apr-db51b7ad` (12 chars). LLMs won't truncate these.

**Auto-execute:** After approval, the gateway automatically completes the install - no agent retry needed.

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

### `autonoetic agent revision create`

Create an immutable revision from an `agent_bundle` artifact.

```bash
autonoetic agent revision create <agent_id> <artifact_id> [--base-revision-id <REV>] [--summary <TEXT>] [--json]
```

Notes:
- This path expects the artifact to already contain `SKILL.md` and the referenced `runtime.lock`.
- The gateway validates shape, scaffolds gateway-owned lock fields, and stores canonicalized lock bytes.
- For schema help from inside agent tooling, use the runtime tool `agent.revision.schema`.
- For intent-driven installs (gateway renders canonical manifest/lock), agents can use `agent.revision.create_from_intent`.

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
- `/exit` or `/quit` — Exit chat
- `/status` — Show current session info

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
# Create config first (required for bootstrap)
cat > gateway.toml << 'EOF'
port = 8080
agents_dir = "./agents"
EOF

# Bootstrap all reference bundles
autonoetic agent bootstrap --from ./agents/ --overwrite

# Start and verify
autonoetic gateway start --config gateway.toml
autonoetic agent list
```
