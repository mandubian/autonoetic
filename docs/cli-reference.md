# Autonoetic CLI Reference

The Autonoetic CLI (`autonoetic`) provides commands for managing the gateway, agents, traces, and integrations.

## Global Options

| Option | Description |
|--------|-------------|
| `-c, --config <PATH>` | Path to config file (default: `~/.autonoetic/config.yaml`) |
| `--non-interactive` | Disables all prompts (for CI/CD) |

## Commands

---

## Run (Quick Start)

### `autonoetic run`

One-command start: detects LLM providers, generates config, bootstraps agents, starts gateway, and opens chat. Ideal for first-time users.

```bash
autonoetic run
autonoetic run --agent-id researcher.default
autonoetic run --session-id my-session
```

| Option | Description |
|--------|-------------|
| `--agent-id <ID>` | Target agent (default: `planner.default`) |
| `--session-id <ID>` | Resume an existing session |

On first run, interactively prompts for:
1. **LLM provider** — auto-detects API keys and local servers
2. **Model** — fetches the provider's catalog
3. **Persona** — optional "about yourself" text for agent personalization

---

## Gateway

Manage the Gateway lifecycle and configuration.

### `autonoetic gateway start`

Starts the Gateway daemon.

```bash
autonoetic gateway start
autonoetic gateway start --daemon
autonoetic gateway start --port 8080
```

| Option | Description |
|--------|-------------|
| `-d, --daemon` | Run in background as daemon |
| `--port <PORT>` | Override default HTTP/TCP port |
| `--tls` | Force TLS on OFP federation port |

### `autonoetic gateway stop`

Gracefully stops a background Gateway daemon.

```bash
autonoetic gateway stop
```

### `autonoetic gateway status`

Shows Gateway health and loaded policies.

```bash
autonoetic gateway status
autonoetic gateway status --json
```

| Option | Description |
|--------|-------------|
| `--json` | Emit machine-readable JSON output |

### `autonoetic gateway approvals`

Manage background approval requests.

```bash
# List pending approvals
autonoetic gateway approvals list

# Approve a request
autonoetic gateway approvals approve <request_id> --reason "Approved"

# Reject a request  
autonoetic gateway approvals reject <request_id> --reason "Not needed"
```

### `autonoetic gateway escalations`

Manage federation escalation messages for promotion review.

```bash
# List pending escalations
autonoetic gateway escalations list

# Inspect a specific escalation
autonoetic gateway escalations inspect <escalation_id>

# Resolve an escalation (operator decision)
autonoetic gateway escalations resolve <escalation_id> --status approved --decided-by "operator" --reason "All clear"
```

| Option | Description |
|--------|-------------|
| `--status <STATUS>` | Decision: `approved` or `rejected` (resolve, required) |
| `--decided-by <ID>` | Decider identity (resolve, required) |
| `--reason <TEXT>` | Decision reason (resolve, required) |

### `autonoetic gateway grants`

Manage session approval grants.

```bash
# List all grants
autonoetic gateway grants list

# Revoke a grant
autonoetic gateway grants revoke --grant-id <grant_id>
autonoetic gateway grants revoke --root-session <id> --host api.example.com
```

---

## Agent

Manage Autonoetic agents.

### `autonoetic agent init`

Scaffolds a new agent directory with LLM configuration.

```bash
autonoetic agent init my-agent --template coder
autonoetic agent init my-agent --preset coding
autonoetic agent init my-agent --provider anthropic --model claude-sonnet-4-20250514
```

| Argument | Description |
|----------|-------------|
| `agent_id` | Agent ID to create |

| Option | Description |
|--------|-------------|
| `-t, --template` | Template (planner, researcher, coder, auditor, generic) |
| `--preset` | Named LLM preset from config |
| `--provider` | LLM provider override |
| `--model` | LLM model override |

### `autonoetic agent presets`

Lists available LLM presets and template-to-preset mappings.

```bash
autonoetic agent presets
```

### `autonoetic agent run`

Boots an agent and connects to the Gateway.

```bash
# Run with initial message
autonoetic agent run my-agent "Hello"

# Interactive chat mode
autonoetic agent run my-agent --interactive

# Headless mode
autonoetic agent run my-agent --headless
```

| Argument | Description |
|----------|-------------|
| `agent_id` | Agent ID to run |
| `message` | Initial message (optional) |

| Option | Description |
|--------|-------------|
| `-i, --interactive` | Persistent chat loop |
| `--headless` | Boot without user interaction |
| `--record-network` | Record HTTP traffic for fixture-based evaluation |

### `autonoetic agent list`

Lists all local agents registered with the Gateway.

```bash
autonoetic agent list
```

### `autonoetic agent bootstrap`

Bootstraps runtime agents from reference bundles.

```bash
autonoetic agent bootstrap
autonoetic agent bootstrap --from /path/to/bundles
autonoetic agent bootstrap --overwrite
```

| Option | Description |
|--------|-------------|
| `-f, --from` | Path to reference bundles root |
| `-o, --overwrite` | Overwrite existing agents |

### `autonoetic agent credential`

Manage vault-stored credentials.

```bash
# Store a secret (prompts for value if --from-env and --value omitted)
autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY

# Read secret from environment variable
autonoetic agent credential put --service github --secret-name GITHUB_TOKEN --from-env GITHUB_TOKEN

# With all options
autonoetic agent credential put \
  --service openweathermap \
  --secret-name OPENWEATHER_API_KEY \
  --from-env OPENWEATHER_API_KEY \
  --credential-id cred_weather \
  --inject-as env:OPENWEATHER_API_KEY \
  --allowed-hosts api.openweathermap.org

# List all credentials
autonoetic agent credential list

# List credentials for a service
autonoetic agent credential list --service openweathermap

# Remove a credential
autonoetic agent credential rm cred_abc123
```

| Subcommand | Description |
|------------|-------------|
| `put` | Store secret in vault + create credential record |
| `list` | List credential metadata (never secret values) |
| `rm` | Remove credential and its secret |

---

## Recording

Record real HTTP traffic during agent execution for sealed evaluation replay.

### `autonoetic recording start`

Start a recording session for an agent.

```bash
autonoetic recording start --agent-id myagent.default --duration 5m
autonoetic recording start --agent-id myagent.default --max-requests 50
```

| Option | Description |
|--------|-------------|
| `--agent-id <ID>` | Agent to record (required) |
| `--duration <DURATION>` | Recording duration (e.g., `5m`, `1h`) |
| `--max-requests <N>` | Max requests before stopping |

### `autonoetic recording list`

List recorded fixture sets.

```bash
autonoetic recording list
autonoetic recording list --agent-id myagent.default
```

### `autonoetic recording inspect`

Inspect a fixture set.

```bash
autonoetic recording inspect <fixture_set_ref>
```

---

## Eval Sealed

Run sealed-network evaluation against recorded fixtures.

```bash
autonoetic eval sealed --artifact-ref ar.xxx --fixture-set fs.yyy
autonoetic eval sealed --artifact-ref ar.xxx --fixture-set fs.yyy --agent-id sealed_evaluator.default
```

| Option | Description |
|--------|-------------|
| `--artifact-ref <REF>` | Artifact to evaluate (required) |
| `--fixture-set <REF>` | Fixture set for replay (required) |
| `--agent-id <ID>` | Evaluator agent (default: `sealed_evaluator.default`) |

---

## Review

Inspect post-promotion review results.

```bash
# Show review status for all agents
autonoetic review status

# Show review status for a specific agent
autonoetic review status --agent-id myagent.default

# Inspect a specific review
autonoetic review inspect <review_id>

# Show review history
autonoetic review history --agent-id myagent.default --limit 10
```

| Subcommand | Description |
|------------|-------------|
| `status` | Show current review status per agent |
| `inspect` | Full detail for a specific review |
| `history` | Historical review results |

---

## Chat

Chat with an agent through the Gateway JSON-RPC ingress.

```bash
# Chat with default agent
autonoetic chat

# Target specific agent
autonoetic chat researcher.default

# With specific session
autonoetic chat researcher.default --session-id my-session
```

| Argument | Description |
|----------|-------------|
| `agent_id` | Target agent ID (optional) |

| Option | Description |
|--------|-------------|
| `--sender-id` | Stable sender identity |
| `--channel-id` | Stable channel identity |
| `--session-id` | Stable conversation ID |
| `--test-mode` | Suppress prompts for scripted tests |

**Slash commands during chat:**

| Command | Description |
|---------|-------------|
| `/session` | Show known sessions and open the session picker |
| `/session new [name]` | Create a new session |
| `/session switch <id>` | Switch to an existing session |
| `/status` | Show current session details |
| `/why [request_id]` | Explain why an approval was triggered (constitutional rules) |
| `/policy <text>` | Send governance intent to `governance-author.default` for constitutional proposals |
| `/persona [text]` | Show or set user persona (persists to `persona.md`, applies to new sessions) |
| `/curate [focus notes]` | Run memory curation on the current session now; optional focus notes steer the curator |
| `/crystallize [what worked]` | Make a tactic from this session reusable; the crystallizer picks instruction / wrapper / new skill |
| `/skills` | Standing view of proposed skill work: verdicts, recorded decisions, and Candidate revisions awaiting promotion |
| `/cancel` | Leave the current picker/prompt |
| `/quit` or `/exit` | Exit chat |
| `/help` | Show all commands |

---

## Trace

Inspect causal chain traces for debugging and audit.

### `autonoetic trace sessions`

List known sessions across agent traces.

```bash
autonoetic trace sessions
autonoetic trace sessions --agent planner.default
autonoetic trace sessions --json
```

| Option | Description |
|--------|-------------|
| `--agent` | Restrict to specific agent |
| `--json` | Machine-readable JSON output |

### `autonoetic trace show`

Show all events for one session.

```bash
autonoetic trace show session-123
autonoetic trace show session-123 --agent planner.default
autonoetic trace show session-123 --json
```

| Argument | Description |
|----------|-------------|
| `session_id` | Session identifier |

| Option | Description |
|--------|-------------|
| `--agent` | Restrict to specific agent |
| `--json` | Machine-readable JSON output |

### `autonoetic trace event`

Show one specific event by log ID.

```bash
autonoetic trace event log-123
autonoetic trace event log-123 --agent planner.default
autonoetic trace event log-123 --json
```

| Argument | Description |
|----------|-------------|
| `log_id` | Event/log identifier |

| Option | Description |
|--------|-------------|
| `--agent` | Restrict to specific agent |
| `--json` | Machine-readable JSON output |

### `autonoetic trace rebuild`

Rebuild unified session timeline from gateway + agent causal logs.

```bash
autonoetic trace rebuild session-123
autonoetic trace rebuild session-123 --agent planner.default
autonoetic trace rebuild session-123 --json
autonoetic trace rebuild session-123 --skip-checks
```

| Argument | Description |
|----------|-------------|
| `session_id` | Session identifier |

| Option | Description |
|--------|-------------|
| `--agent` | Restrict to specific agent |
| `--json` | Machine-readable JSON output |
| `--skip-checks` | Skip integrity checks |

### `autonoetic trace follow`

Follow session events in real-time as they happen.

```bash
autonoetic trace follow session-123
autonoetic trace follow session-123 --agent planner.default
autonoetic trace follow session-123 --json
```

| Argument | Description |
|----------|-------------|
| `session_id` | Session identifier |

| Option | Description |
|--------|-------------|
| `--agent` | Restrict to specific agent |
| `--json` | Machine-readable JSON output |

Press `Ctrl+C` to stop following.

### `autonoetic trace fork`

Fork a session from a snapshot to explore alternative paths.

```bash
autonoetic trace fork session-123
autonoetic trace fork session-123 --message "Try a different approach"
autonoetic trace fork session-123 --at-turn 3 --interactive
autonoetic trace fork session-123 --new-session-id my-fork --agent researcher.default --json
```

| Argument | Description |
|----------|-------------|
| `session_id` | Source session ID to fork from |

| Option | Description |
|--------|-------------|
| `--message` | Branch message to append (e.g., "Try a different approach") |
| `--new-session-id` | New session ID (auto-generated if not provided) |
| `--at-turn` | Fork from specific turn number (default: latest) |
| `--agent` | Target agent ID (defaults to source agent) |
| `--interactive` | Start interactive chat session after forking |
| `--json` | Machine-readable JSON output |

The fork writes a fresh checkpoint under the new session id, so the branch is
immediately **runnable** — sending it a message resumes from the fork point
with the full branch-point context. Checkpoints exist only at yield points
(hibernation, approval, budget, escalation…), not at every turn, so `--at-turn`
can only target a turn that has a checkpoint; otherwise it errors and lists the
forkable turns. The same branch can be created from the Session Room with
`/fork [--at-turn N] [message]` (it forks the current session and switches to
the new one). See `docs/session-forking.md` for the full mechanism, including
the timeline-mirroring design choice (copy vs reuse-by-reference).

### `autonoetic trace history`

Show conversation history for a session.

```bash
autonoetic trace history session-123
autonoetic trace history session-123 --agent planner.default
autonoetic trace history session-123 --json
```

| Argument | Description |
|----------|-------------|
| `session_id` | Session identifier |

| Option | Description |
|--------|-------------|
| `--agent` | Restrict to specific agent |
| `--json` | Machine-readable JSON output |

---

## Session

Inspect session outcomes and export session archives.

### `autonoetic session show`

Print the `SessionOutcome` row for a session as JSON.

```bash
autonoetic session show session-123
```

| Argument | Description |
|----------|-------------|
| `session_id` | Session identifier |

### `autonoetic session rate`

Attach an operator rating to a session's `SessionOutcome` row.

```bash
autonoetic session rate session-123 --thumbs-up
autonoetic session rate session-123 --thumbs-down --note "reason..."
```

| Argument | Description |
|----------|-------------|
| `session_id` | Session identifier |

| Option | Description |
|--------|-------------|
| `--thumbs-up` | Positive rating |
| `--thumbs-down` | Negative rating |
| `--note` | Optional note (max 500 chars) |

### `autonoetic session export`

Export a full session (root session tree) to a single file or a structured archive directory.

```bash
# Single-file export (default: <session-id>.room.md)
autonoetic session export session-123
autonoetic session export session-123 --format json --output session-123.json

# Structured archive directory
autonoetic session export session-123 --output-dir ./archives
```

| Argument | Description |
|----------|-------------|
| `session_id` | Session identifier |

| Option | Description |
|--------|-------------|
| `-o, --output` | Output file path (mutually exclusive with `--output-dir`) |
| `-f, --format` | Export format: `room` (default), `room-raw`, `json` |
| `--with-checkpoints` | Include full checkpoint files (large) |
| `--min-altitude` | Drop `detail` events: `normal`, `attention`, or `error` |
| `--output-dir` | Export structured archive directory containing `wiki/`, `json`, and `MANIFEST.json` |

---

## Skill

Manage AgentSkills.io ecosystem and skills.

### `autonoetic skill install`

Downloads and installs an AgentSkills.io compliant bundle.

```bash
autonoetic skill install https://github.com/user/repo
autonoetic skill install my-skill --agent researcher.default
```

| Argument | Description |
|----------|-------------|
| `url_or_id` | GitHub URL or Skill ID |

| Option | Description |
|--------|-------------|
| `-a, --agent` | Target agent ID |

### `autonoetic skill uninstall`

Removes a skill from an agent's capability list.

```bash
autonoetic skill uninstall my-skill --agent researcher.default
```

| Argument | Description |
|----------|-------------|
| `skill_name` | Name of skill to uninstall |

| Option | Description |
|--------|-------------|
| `-a, --agent` | Target agent ID (required) |

---

## Federate

Manage federation and cluster connections.

### `autonoetic federate join`

Connects the local Gateway to a remote peer via OFP.

```bash
autonoetic federate join peer.example.com:9000
```

| Argument | Description |
|----------|-------------|
| `peer_address` | Remote peer address |

### `autonoetic federate list`

Outputs the local PeerRegistry.

```bash
autonoetic federate list
```

---

## MCP

Manage MCP (Model Context Protocol) integrations.

### `autonoetic mcp add`

Registers a local MCP server with the Gateway.

```bash
# Stdio transport
autonoetic mcp add my-server --command "npx"

# SSE transport
autonoetic mcp add my-server --sse-url http://localhost:3000
```

| Argument | Description |
|----------|-------------|
| `server_name` | MCP server name |

| Option | Description |
|--------|-------------|
| `-c, --command` | Subprocess command (stdio transport) |
| `--sse-url` | SSE endpoint URL |
| `args` | Command arguments (last) |

### `autonoetic mcp expose`

Runs the Gateway as an MCP Server on stdio.

```bash
autonoetic mcp expose researcher.default
```

| Argument | Description |
|----------|-------------|
| `agent_id` | Agent ID to expose |

---

## Examples

### Basic Workflow

```bash
# Start gateway
autonoetic gateway start --daemon

# Create an agent
autonoetic agent init my-researcher --template researcher

# Bootstrap reference agents
autonoetic agent bootstrap

# Chat with agent
autonoetic chat my-researcher

# Check trace
autonoetic trace sessions
autonoetic trace show session-123
```

### Background Processing

```bash
# Start gateway
autonoetic gateway start --daemon

# Check pending approvals
autonoetic gateway approvals list

# Approve a request
autonoetic gateway approvals approve req-456 --reason "Approved for execution"

# Follow session in real-time
autonoetic trace follow session-789
```

### Federation

```bash
# Join a peer
autonoetic federate join peer.example.com:9000

# List peers
autonoetic federate list
```

---

## Exit Codes

| Code | Description |
|------|-------------|
| 0 | Success |
| 1 | Error (missing config, invalid arguments, etc.) |
| 130 | Interrupted (Ctrl+C) |
