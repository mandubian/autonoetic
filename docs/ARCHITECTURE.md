# Autonoetic Architecture

> Autonoetic: from Greek *autonoetikos* (αὐτονοητικός) — "self-aware, having insight into one's own mental processes."

Autonoetic is a Rust-first runtime for autonomous, self-evolving AI agents with formal governance. It replaces the heavier CCOS architecture with a leaner design that separates reasoning from execution.

## Table of Contents

- [Core Design Principle](#core-design-principle)
- [System Components](#system-components)
- [Data Flow](#data-flow)
- [Security Model](#security-model)
- [Execution Modes](#execution-modes)
- [Agent I/O Schemas](#agent-io-schemas)
- [Memory Architecture](#memory-architecture)
- [Revision & Activation Model](#revision--activation-model)
- [Content Storage](#content-storage)
- [Causal Chain](#causal-chain)
- [Session Checkpoints, Continuations, and Forks](#session-checkpoints-continuations-and-forks)
- [Queryable Event Store](#queryable-event-store)
- [Contract Health](#contract-health)
- [Live Digest](#live-digest)
- [Observability Surface](#observability-surface)
- [Hook System](#hook-system)
- [Unified Gateway Database](#unified-gateway-database)
- [Emergency Stop](#emergency-stop)
- [Human Escalation](#human-escalation)
- [Promotion Federation](#promotion-federation)
- [Recording Mode](#recording-mode)
- [Post-Promotion Review](#post-promotion-review)
- [Scheduled Tasks](#scheduled-tasks)
- [Fast Scheduler Sidecar](#fast-scheduler-sidecar)
- [Promotion Safety Governor](#promotion-safety-governor)
- [Curator Decision Journal](#curator-decision-journal)
- [Error Taxonomy](#error-taxonomy)
- [Design Principles](#design-principles)

---

## Core Design Principle

**Separation of Powers**: Agents are pure reasoners. The gateway is the sole authority for execution.

This is governed by three fundamental rules:
1. **Rule Zero: Rules cannot be overridden.** Not by agents, not by planners, not by parameters. If a rule exists, it applies equally to all agents without exception. 
2. **Safety is mechanical.** LLM decisions are advisory. Safety-critical invariants must be mechanically enforced by the gateway's deterministic guardrails.
3. **Gateway is a narrow rule enforcer.** It analyzes, gates, and explains why something was refused — but never routes or makes workflow decisions itself.

```
┌─────────────────────────────────────────────────────────┐
│                     Agent (Low Privilege)                │
│                                                         │
│  ┌─────────────┐   ┌─────────────┐   ┌─────────────┐   │
│  │  Reasoning  │ → │  Proposals  │ → │   Review    │   │
│  │   (LLM)     │   │  (Intents)  │   │  (Results)  │   │
│  └─────────────┘   └─────────────┘   └─────────────┘   │
│         │                │                   │          │
│         └────────────────┼───────────────────┘          │
│                          ▼                              │
│              Intent / Proposal Verbs:                   │
│         execute, spawn, share, schedule, recall         │
└──────────────────────────┬──────────────────────────────┘
                           │ JSON-RPC / HTTP
                           ▼
┌─────────────────────────────────────────────────────────┐
│                   Gateway (High Privilege)               │
│                                                         │
│  ┌─────────┐  ┌──────────┐  ┌──────────┐  ┌─────────┐  │
│  │ Policy  │  │Execution │  │  Audit   │  │  Secret │  │
│  │ Engine  │→ │  Engine  │→ │  Logger  │  │  Store  │  │
│  └─────────┘  └──────────┘  └──────────┘  └─────────┘  │
│         │              │              │              │   │
│         ▼              ▼              ▼              ▼   │
│  Capability      Sandbox        Causal          Vault   │
│  Validation     Execution       Chain         Injection  │
└─────────────────────────────────────────────────────────┘
```

**Why this matters:**
- Agents cannot access secrets, filesystems, or networks directly
- Gateway mechanically validates every proposal against capabilities and strict safety policy
- All execution is logged to an immutable audit trail
- Agents can be replaced; governance and safety are permanently enforced

## System Components

### Gateway

The gateway is the security boundary and execution engine. It is a **narrow rule enforcer** that applies mechanical guardrails, but does NOT contain domain-specific business logic or routing orchestration.

| Component | Responsibility |
|-----------|---------------|
| **JSON-RPC Router** | Accepts `event.ingest` and `agent_spawn` requests |
| **Policy Engine** | Validates capabilities, ACLs, and disclosure rules |
| **Execution Service** | Spawns agent sessions, manages lifecycle |
| **Layer Store** | Content-addressed storage for compressed directory trees (artifact dependencies) |
| **Content Store** | SHA-256 content-addressable storage for artifacts |
| **Causal Chain** | Append-only JSONL audit log with hash-chain integrity |
| **Scheduler** | Manages background reevaluation cadence; wake predicates: `Timer` and `ApprovalResolved` |
| **Fast Scheduler** | Parallel low-latency loop for sub-second interval jobs (disabled by default) |
| **Sandbox Runner** | Executes scripts via bubblewrap, docker, or microvm |
| **Secret Vault** | Injects secrets ephemerally, never exposes to agents |
| **HTTP API** | REST endpoints for remote agent content access |

### Agent

An agent is a SKILL.md manifest + instructions that runs inside a sandbox. Agents propose actions; the gateway executes them.

Key characteristics:
- **Pure reasoner**: Makes decisions, but cannot execute
- **Text-native**: Agent and workflow state are plain text/JSON files, prioritizing transparency over database opacity.
  - *Note:* The Gateway uses an embedded SQLite database (`gateway.db`) purely for fast-moving transactional data: approvals, notifications, and workflow events.
- **Capability-declared**: All permissions declared in manifest
- **Role-based**: Each agent fills a specific role in the system

### SDK

The `autonoetic_sdk` package (Python/TypeScript) provides the agent's view of the gateway:

| Transport | Use Case |
|-----------|----------|
| Unix socket | Local agents (same machine as gateway) |
| HTTP/REST | Remote agents (different machine) |

---

## Data Flow

### Standard Request Flow

```
1. User message arrives via JSON-RPC or HTTP
2. Gateway requires explicit target_agent_id — missing or malformed target fails immediately
3. Gateway resolves target through alias registry → pinned revision directory
   (explicit agent_ref bypasses alias lookup; runs that revision directly)
4. Gateway creates a session binding: stores requested_target, alias_id, revision_id, runtime_lock_hash
5. Gateway spawns agent session from the pinned revision directory + runtime closure
6. Agent reasoning loop:
   a. Gateway picks an egress-eligible provider for the turn's batch taint
      (taint-following routing) and assembles context
   b. LLM chokepoint filters the outbound request — content whose label
      excludes the target provider's sink is replaced by an indication
      (see Egress Localization under Security Model)
   c. LLM processes context + instructions
   d. LLM emits tool calls (content_write, agent_spawn, etc.)
   e. Gateway validates and executes tools; tool results are labeled at commit
   f. Tool results returned to LLM
   g. Loop until EndTurn
7. Agent response returned through ingress channel
8. All actions logged to causal chain
```

### Content Storage Flow

```
Agent: content_write("main.py", script_content)
  ↓
Gateway: 1. Compute SHA-256 hash
         2. Store blob at .gateway/content/sha256/ab/c123...
         3. Update session manifest: {"main.py": "sha256:abc123"}
         4. Return handle to agent

Agent: resolve("main.py")
  ↓
Gateway: 1. Resolve name → handle from session manifest
         2. Fetch blob from content store
         3. Return content
```

### Artifact Creation Flow

```
1. Coder writes files via content_write()
2. Coder writes SKILL.md with YAML frontmatter
3. Gateway detects SKILL.md, extracts metadata
4. On agent_spawn completion:
   - All session content bundled into artifact
   - Artifact metadata from SKILL.md frontmatter
   - Structured artifact added to spawn response
5. Planner receives artifacts in spawn response
6. Specialized_builder reads artifacts via resolve()
```

### Build Layer Flow

```
1. Packager agent (with network access) runs:
   sandbox_exec({
     "command": "pip install -r /tmp/requirements.txt --target /tmp/venv",
     "capture_paths": [
       {"path": "/tmp/venv", "mount_as": "/opt/venv"}
     ]
   })

2. Gateway captures directory as layer:
   - LayerStore.create_from_dir() → compresses venv/ as tar.zst
   - Computes SHA-256 digest
   - Stores at .gateway/layers/layer_{digest}/
   - Returns captured_layer with layer_id, name, mount_path, digest

3. Packager builds layered artifact:
   artifact_build({
     "inputs": ["main.py", "requirements.txt"],
     "layers": [
       {
         "layer_id": "layer_abc123...",
         "name": "python-deps",
         "mount_path": "/opt/venv",
         "digest": "sha256:..."
       }
     ]
   })

4. Artifact includes layer metadata:
   - Deterministic ID includes file handles + layer digests
   - Manifest stores layers array
   - Evaluator receives layered artifact

5. Evaluator runs with layered artifact:
   sandbox_exec({
     "artifact_id": "art_xxxxxx",
     "command": "python3 /tmp/main.py"
   })
   ↓
   Gateway:
   - Extracts layer to temp dir via LayerStore.extract_to()
   - Mounts at /opt/venv inside sandbox
   - Imports work immediately (no pip install needed)
```

**Key Benefits:**
- Dependencies installed once at build time
- Same layer deduplicated across artifacts (by digest)
- Network access limited to build phase
- Evaluator runs in network-isolated sandbox


---

## Security Model

### Security Sentinel

A dedicated system-tier agent audits the gateway's own state for security issues. See [`docs/security-sentinel.md`](security-sentinel.md) for design, phased build plan, and the three hard problems (recursive trust, prompt injection against the auditor, and calibration).

The sentinel lives in `agents/system/` — a new tier parallel to `lead/`, `specialists/`, and `evolution/`. Placing it in a distinct tier signals that it must be visibly harder to silently revise than evolution-tier agents.

Key design decisions:
- **Read-only capability profile**: no `NetworkAccess`, no `CodeExecution`, no write access to privileged surfaces.
- **Append-only findings**: the `security_findings` SQLite table is insert-only; only triage state may be updated after insert.
- **Frozen baseline**: `security_sentinel_baseline.default` runs alongside the current sentinel on every sweep and never promotes automatically.
- **Deterministic first**: Phase 1 checks (credential regex, capability accretion SQL, approval bypass, sandbox escape) produce `critical` findings without LLM involvement. LLM-judgment findings (Phase 2) land at `warning` by default.

### Observability Redaction

Two cooperating mechanisms control how sensitive data flows through the gateway:

- **`ViewerClass` (Agent / Operator / Admin)** controls *what an observability or approval reader sees* based on who they are. Agents reading `execution_search` get trace metadata only — no stdout, no commands, no arguments, no result. Agents reading `approval_summary` for `WriteFile` see the path but not content; for `CredentialRequest` they see only `credential_id`/`url`/`method` (headers / body / payload blanked). One known gap (issue #158, fixed by PR #160): `SandboxExec.command` is currently preserved verbatim for the Agent class. Operators see structural fields with secret-named JSON keys redacted; non-JSON strings get precise in-place masking via `redact_embedded_secrets`. Admins see the raw record. Class is selected at the call site.
- **`DisclosureClass` (Public / Restricted)** controls *what an LLM may quote in its assistant reply*. Per-content classification configured via `DisclosurePolicy` rules in `SKILL.md`.

Together with **P-4.14** (the redaction-before-write invariant enforced by `RedactedPayload`), these form three layers: P-4.14 keeps secrets out of the causal chain at write time; `ViewerClass` strips fields per consumer at read time; `DisclosureClass` filters the LLM's reply.

Redaction primitives are centralised in `autonoetic-types/src/redaction.rs`. Per-record field-by-field tables, call-site conventions, and the threat model are documented in [`docs/observability-redaction.md`](observability-redaction.md).

### Capability-Based Access Control

All external interactions require declared capabilities:

| Capability | Grants Access To |
|------------|-----------------|
| `ReadAccess` | Reading agent state files |
| `WriteAccess` | Writing agent state files |
| `SandboxFunctions` | Invoking MCP and native tools |
| `CodeExecution` | Running shell commands in sandbox |
| `AgentSpawn` | Spawning child agents |
| `AgentMessage` | Messaging other agents |
| `NetworkAccess` | Network access (with host allowlist) |
| `BackgroundReevaluation` | Scheduled background wakes |

### Capability Scoping

Capabilities use pattern-based scoping:

```yaml
capabilities:
  - type: "WriteAccess"
    scopes: ["self.*", "skills/*"]  # Can only write to own dir and skills
  - type: "NetworkAccess"
    hosts: ["api.open-meteo.com"]   # Can only reach specific hosts
```

### Sandbox Filesystem Isolation

The sandbox boundary is **capability-driven for network** (`NetworkAccess` →
`--share-net`; no capability → `--unshare-all`, netns off) but, for the default
**bubblewrap** driver, the **filesystem boundary is currently read-only over the
whole host** (`--ro-bind / /`). Docker and wasm tiers mount only the agent
workspace; microvm is operator-dependent. The ro-bind of `/` means a sandboxed
process running with `CodeExecution`/`SandboxFunctions` can *read* any host file
readable by the gateway's UID — including, in principle, the gateway config and
gateway directory.

As a **stopgap** (tracked by #1002, which will replace it with an explicit mount
allow-set), the gateway masks gateway-internal secrets inside every bubblewrap
sandbox (`sandbox.rs::bwrap_deny_path_flags`), layered over the ro-bind of `/`:

| Masked path (under `<agents_dir>/.gateway`) | Mechanism | Why |
|---|---|---|
| `vault.key`, `vault.enc.json` | `/dev/null` overlay | credential vault master key + encrypted blob |
| `gateway.db`, `gateway.db-shm`, `gateway.db-wal` | `/dev/null` overlay | session/approval/causal SQLite DB |
| `state_attestation.ed25519` | `/dev/null` overlay | Ed25519 identity signing key |
| `sessions/`, `scheduler/`, `checkpoints/`, `history/`, `logs/`, `revisions/` | empty `--tmpfs` | transcripts, approvals, runtime state |

The operator **config file** is likewise masked at its actual load path
(registered via `sandbox::init_sandbox_host_deny_paths` at gateway startup), so
provider/endpoint config, `continuation_key`, and trusted-signer material are
not readable from inside the sandbox. The `sdk/` subtree is deliberately *left
accessible* because the sandbox resolves its `PYTHONPATH` from
`<gateway_dir>/sdk`; the constitution is public agent-readable law and is not
masked.

This closes the worst read paths today, but it is a **deny-list**, not an
allow-list — #1002 is the durable fix (curated mount allow-set + declared custom
mounts). New secrets added under `.gateway` must be added to
`BWRAP_GATEWAY_SENSITIVE_FILES`/`_DIRS` or they will be reachable until #1002
lands.

### Secret Injection

Secrets are never exposed to agents directly:
1. Agent requests secret via `secrets.get("api_key")`
2. Gateway validates agent has access to requested secret
3. Gateway injects secret as environment variable for sandbox execution
4. Secret is zeroized after execution

### Disclosure Policy

Reply governance controls what the agent can tell the user:

| Class | Behavior | Example |
|-------|----------|---------|
| `public` | Verbatim | Public API responses |
| `internal` | Summary only | Internal state, session context |
| `confidential` | Redacted | Memory contents, tool outputs |
| `secret` | Never disclosed | Vault secrets, API keys |

`DisclosureClass` governs the **inward** direction (what the assistant may
repeat to the user/viewers). Its **outward** complement — where content may
flow to a provider, a peer gateway, or the network — is the egress label plane
below.

### Egress Localization (Data Envelopes)

A gateway-enforced **label plane** bounds where content may leave the machine
(full design: `docs/rfc/data-envelopes-egress-localization.md`). Where
credentials are protected by *never entering context*, this generalizes the
property to ordinary content — "an agent may read my emails, but their content
must never reach a remote model."

- **Envelopes & labels.** Content born at a boundary (tool result, LLM
  response, memory recall) carries an `EgressLabel` — a set of allowed *sinks*
  (`LocalModel`, `RemoteModel`, `LocalAgent`, `FederatedAgent`, `Network`,
  `MemoryPersist`, `UserReply`). Labels are **declared metadata the gateway
  alone manipulates** — never model-inferred (Lawful-Executor). Derivation only
  ever *restricts* (lattice meet / `intersect`); the sole widening path is
  operator-approved declassification.
- **Where labels are born.** Operator source rules (`egress.rules`, e.g.
  `sandbox.exec:~/mail/** → local_only`), a bundle-declared floor
  (`metadata.autonoetic.egress.output_label`), argument taint (a tool called
  with a tainted argument produces tainted output), and the LLM-response label
  (a completion inherits the intersection of the envelopes it actually saw).
- **The chokepoint.** Before every provider call a policy-wrapping `LlmDriver`
  maps the target preset's `egress_class` to a `Sink` and, for each message
  whose label excludes it, substitutes a non-divulging *indication*
  (`[withheld: 1× email.read result — policy local_only]`). A verbatim
  outbound-content assertion is a tripwire against echo-exfil. This applies
  identically to the failover chain.
- **Taint-following routing.** Per completion, the intersection of the labels
  added since the last completion decides which presets are *eligible*; a
  tainted batch routes to a cleared (local) preset and the failover chain is
  filtered the same way. No eligible preset ⇒ the turn refuses with a path
  forward, never a silent downgrade.
- **Durability & propagation.** Labels serialize into checkpoints (survive
  suspend/resume/fork), survive history transforms (message ids, §3.4), gate
  per-label-band context compression (a tainted band never compresses on a
  remote preset), and cross session boundaries onto spawn-return values and
  `agent_message` payloads (closing the `LocalAgent` hole).
- **Traceability.** Every decision emits a content-free causal event
  (`egress.envelope_labeled` / `envelope_withheld` / `request_filtered` /
  `provider_selected` / `boundary_refused` / `declassified`); "what left my
  machine at turn N, and why did it run on this provider?" is answerable from
  the causal chain alone (`gateway egress audit <session>`).
- **Phase 4 boundary surfaces (#909).** Non-LLM egress paths gate on session
  taint before bytes leave. Each refusal emits `egress.boundary_refused` with
  a `surface` tag; operator widening emits `egress.declassified`. Widening from
  an ordinary network approval is **host-scoped** (one grant per approved
  host, revocable via `gateway grants revoke --host`); session-wide
  declassification requires an explicit `EgressDeclassify` approval:

  | Surface | Gate |
  |---------|------|
  | `sandbox` | `share_net` requires declassification covering every detected host when taint excludes `Network`; `Unresolved` targets hard-refuse |
  | `web` | `web_fetch` / `web_search` / `web_call` before outbound HTTP |
  | `hooks` | `http.callback` deliveries |
  | `mcp` | Remote SSE `tools/call` — session gate + argument-carried taint |
  | `ofp` | Outbound `AgentMessage` withhold; inbound missing label fail-closed |
  | `compression` | Tainted bands never summarize on remote presets |

  The **data-owner compartment** pattern (resident agent + `local_only` +
  `agent_message` replies) is documented in
  [`docs/egress-data-owner-compartment.md`](egress-data-owner-compartment.md).

---

## Execution Modes

### Reasoning Mode (Default)

Full LLM-driven loop for tasks requiring judgment:

```
Context → LLM → Tool Calls → Execute → Results → LLM → ... → Response
```

- Uses configured LLM provider/model
- Iterates until EndTurn or loop limit
- Supports all tool types
- Higher latency (~2s per turn), higher cost

### Script Mode (Deterministic)

Direct sandbox execution, no LLM:

```
Input → Script → Output → Return
```

- Executes declared script directly in sandbox
- No LLM call, no iteration
- Fast (~100ms), free, deterministic
- For API calls, data transforms, lookups

**Decision guide:**
| Task Type | Mode | Reason |
|-----------|------|--------|
| API calls (weather, stocks) | `script` | Deterministic, fast |
| Data transforms | `script` | No ambiguity |
| Code review | `reasoning` | Needs judgment |
| Research | `reasoning` | Requires synthesis |

---

## Agent I/O Schemas

Agents declare optional `io.accepts` (input) and `io.returns` (output) JSON Schema fragments in their SKILL.md frontmatter. The gateway enforces these mechanically — it never auto-generates, infers, or modifies schemas.

### Enforcement behavior

| Schema present? | Ingress (`agent.spawn`) | Egress (agent reply) |
|---|---|---|
| `io.accepts` absent | Message passes through unvalidated | — |
| `io.accepts` present | Validates caller's message; rejects with `expected_schema` + repair hint | — |
| `io.returns` absent | — | No output validation (fail-open) |
| `io.returns` present | — | Validates reply against schema; repair retry if enabled |

### Design choices and rationale

**Schema authorship is LLM-owned.** The gateway is a Lawful Executor — it stores and validates schemas verbatim, never creates them. Agent-factory and specialized_builder include `io` in their install intent delegation so newly created agents carry schemas from birth.

**Asymmetric guidance: `io.returns` encouraged, `io.accepts` discouraged for reasoning agents.** Reasoning agents receive natural-language messages from the planner; over-constraining the input schema blocks valid callers with validation errors. Script agents with structured CLI arguments benefit from `io.accepts`.

**Minimal-but-real schemas.** LLMs are unreliable at producing deterministic JSON shapes. The design mitigates this with three rules:

1. **Only require what's always there.** Typically just `status: string` for semi-structured agents, or 2-3 fields for highly structured agents (evaluators, auditor). Never require fields that are sometimes absent.

2. **Use broad types, not narrow ones.** `string` not `enum`, `object` not a specific sub-shape, `array` not `array<SpecificType>`. This avoids type mismatches without losing structural signal.

3. **Extra properties are always allowed.** The schema declares the *minimum* contract, not the maximum. LLM-generated extra fields are harmless and expected.

**Three schema tiers by agent output predictability:**

| Tier | Schema shape | When to use | Examples |
|---|---|---|---|
| Structured | `required: [field1, field2]`, specific properties | Output is deterministic, always the same shape | Evaluator, auditor, sentinel, curator, discovery |
| Semi-structured | `required: ["status"]`, optional properties | Output carries a status plus variable extras | Agent-factory, specialized_builder, coder, packager |
| Variable | `{type: "object"}` with no required fields | Output is too context-dependent for any fixed fields | Planner, researcher, debugger, executor |

---

## Memory Architecture

### Tier 1: Working Memory (Content Storage)

Agent-local files for per-tick determinism:

```
.agent_dir/
├── state/           # Checkpoint files (task.md, scratchpad.md, handoff.md)
├── history/         # Causal chain logs
└── skills/          # Installed skills
```

**Tools:** `content_write`, `resolve`, `artifact_build`, `artifact_inspect`

Content uses root-session visibility. Default is `session` (collaborative within root). Use `visibility: "private"` for scratch work. Artifacts are the mandatory boundary for review/install/execution.

### Tier 2: Durable Memory (Knowledge)

Gateway-managed facts with provenance:

**Tools:** `knowledge_store`, `knowledge_recall`, `knowledge_search`, `digest_query`

Sharing is done by **storing (or re-storing) with the right visibility** — there is no separate share tool. To expose a fact to collaborators in the same root workflow, use `knowledge_store` with default `visibility: "session"` (or call `knowledge_store` again with the same `id` to widen visibility).

| Field | Description |
|-------|-------------|
| `memory_id` | Unique identifier |
| `scope` | Namespace for organizing and policy scoping |
| `owner_agent_id` | Agent that owns this fact |
| `writer_agent_id` | Agent that wrote this fact |
| `source_ref` | Session/turn reference for traceability |
| `content` | The actual fact |
| `content_hash` | SHA-256 for integrity |
| `visibility` | `private` (owner/writer only), `session` (same `session_id` as stored on the record), or `global` (any agent) |
| `expires_at` | Optional TTL from `retention` on store (`stable`, `ephemeral`, `1d`, `30d`) |

---

## Revision & Activation Model

Agents run from **immutable revisions**, not from mutable authoring directories. The activation path is always:

```
artifact_build()  →  agent_revision_create()  →  agent_revision_promote()
      ↓                        ↓                           ↓
  AgentBundle            revision stored,            alias moves to
  artifact in            content-addressed,          new revision;
  content store          status: candidate           status: ready
```

### Key Invariants

- A session always executes from a **pinned revision directory** + a **pinned runtime closure** (hashed `runtime.lock`).
- The **alias registry** is the sole source of truth for which revision is "active" for a logical agent.
- `agent_revision_promote` is the only way to change what alias lookup resolves to.
- Running sessions remain pinned to their revision; promotion does not affect them.
- Candidate revisions are runnable via explicit `agent_ref` (e.g. for eval) without being promoted.
- `agent.install` is **not** part of the runtime tool surface; seeding is always via revision + promote.

### Revision Statuses

| Status | Meaning |
|--------|---------|
| `candidate` | Created, not yet promoted; runnable by explicit ref |
| `ready` | Promoted at least once; currently active or previously active |
| `rejected` | Failed eval gate; cannot be promoted |
| `archived` | Superseded; kept for rollback and audit |

### Eval Gating

Promotion can be gated by a passed eval run:

```
eval_suite_publish()  →  eval_run(suite, agent_ref)  →  agent_revision_promote(required_eval_run_id=...)
```

If the eval run's subject revision does not match the promote target, the promote is rejected.

---

## Content Storage

Content-addressable storage that works locally and remotely:

```
.gateway/
├── content/sha256/ab/c123...   # Immutable content blobs
├── sessions/<session_id>/
│   ├── manifest.json            # name → handle mappings
│   └── artifacts.json           # Artifact metadata
├── layers/                      # Compressed dependency directories
│   ├── index.json             # digest → layer_id mapping
│   └── layer_{id}/
│       ├── manifest.json       # Layer metadata (layer_id, digest, file_count, size_bytes, created_at)
│       └── contents.tar.zst  # Compressed tarball of directory
└── knowledge.db                 # Tier 2 durable facts
```

### Key Properties

- **Content-addressed**: SHA-256 handles, natural deduplication
- **Session-scoped**: Files named within a session with visibility control
- **Cross-session**: `session` visibility makes content visible under same root
- **Cross-agent**: Siblings see each other's session-visible content
- **Remote-accessible**: HTTP API for distributed agents

### Remote Agents

Remote agents use the HTTP Content API instead of Unix sockets:

```
┌──────────────┐    HTTP/REST    ┌──────────────┐
│ Remote Agent │ ◄─────────────► │   Gateway    │
│              │  Bearer token   │              │
└──────────────┘                 └──────────────┘
```

Configuration via manifest or environment:
```yaml
metadata:
  autonoetic:
    gateway_url: "http://gateway:8080"
    gateway_token: "secret"
```

---

## Causal Chain

All actions are logged to an append-only JSONL audit trail:

```
.gateway/history/causal_chain.jsonl
agent_dir/history/causal_chain.jsonl
```

### Entry Structure

```json
{
  "event_id": "uuid-v4",
  "session_id": "session-123",
  "turn_id": "turn-abc",
  "event_seq": 42,
  "category": "tool",
  "action": "requested",
  "timestamp": "2026-03-15T10:30:00Z",
  "payload": {"tool_name": "content_write", ...},
  "entry_hash": "sha256:...",
  "prev_hash": "sha256:..."
}
```

The `event_id` is the universal correlation key: execution traces, session reports, and the observability surface all join back to the causal chain via this field.

### Key Events

| Category | Actions | Description |
|----------|---------|-------------|
| `session` | `start`, `end` | Session lifecycle |
| `llm` | `requested`, `completed` | LLM completion calls |
| `tool` | `requested`, `completed`, `failed` | Tool execution |
| `script` | `started`, `completed`, `failed` | Script agent execution |
| `gateway` | `event.ingest.requested`, `.completed` | Ingress events |
| `memory` | `history.persisted`, `session.forked` | Session checkpointing |

### Trace Commands

```bash
autonoetic trace sessions              # List active sessions
autonoetic trace show <session_id>     # View session timeline
autonoetic trace event <log_id>        # View specific entry
autonoetic trace rebuild <session_id>  # Reconstruct unified timeline
autonoetic trace follow <session_id>   # Watch live events
autonoetic trace fork <session_id>     # Fork from checkpoint
autonoetic trace history <session_id>  # View conversation history
```

---

## Session Checkpoints, Continuations, and Forks

Three interrelated mechanisms enable restarting sessions from a given step:

| Mechanism | Purpose | Storage |
|-----------|---------|---------|
| **Checkpoint** | Universal snapshot at every yield point | `.gateway/checkpoints/{session_id}/{turn_id}.checkpoint.json` |
| **Turn Continuation** | Suspend/resume at approval boundaries | `.gateway/continuations/{task_id}.json` |
| **Session Fork** | Branch a new session from any checkpoint | Copies checkpoint history to a new session |

### Checkpoints

Universal execution snapshots saved at every yield point for crash recovery and session forking.

#### Checkpoint Structure

```json
{
  "session_id": "session-123",
  "turn_id": "turn-042",
  "turn_counter": 42,
  "history": [...],                    // Full conversation history
  "yield_reason": "Hibernation",       // Why execution stopped
  "loop_guard_state": {...},           // Failure tracking state
  "agent_id": "coder.default",
  "workflow_id": "wf-abc",
  "runtime_lock_hash": "sha256:...",
  "constitution_version": "2026.06.05",
  "constitution_digest": "sha256:...",
  "llm_config_snapshot": {...},
  "tool_registry_version": "...",
  "content_store_refs": [...],
  "pending_tool_state": {...},
  "llm_rounds_consumed": 3,
  "tool_invocations_consumed": 12,
  "tokens_consumed": 4500,
  "estimated_cost_usd": 0.04,
  "created_at": "2026-03-15T10:30:00Z"
}
```

#### Yield Reasons

| Reason | Trigger | Auto-Resume? |
|--------|---------|--------------|
| `Hibernation` | EndTurn / StopSequence between turns | Yes |
| `BudgetExhausted` | Session budget depleted | Yes (after budget reset) |
| `ApprovalRequired` | Tool needs approval gate | Via signed checkpoint |
| `UserInputRequired` | `user_ask` pending answer | Yes (when answered) |
| `EmergencyStop` | Operator circuit breaker | **No** (blocks auto-resume) |
| `MaxTurnsReached` | Loop guard limit | Yes |
| `ManualStop` | Operator/user interrupt | Yes |
| `Error` | Recoverable error | Yes |

#### Checkpoint Management

```bash
# List all checkpoints for a session
autonoetic trace checkpoints <session_id>

# View checkpoint details (via the JSON-RPC API or inspecting files)
ls .gateway/checkpoints/<session_id>/
```

Checkpoints are pruned automatically (default: keep last N per session).

### Turn Suspension (Approval-Gated Turns)

When a tool call requires operator approval, the turn is **suspended to a signed session checkpoint** rather than failing or retrying with synthetic prompts. On approval, execution resumes seamlessly with real tool results.

#### Suspension Flow

1. Agent requests a privileged tool call (e.g., `agent_revision_promote`, `sandbox_exec` on a new resource)
2. Gateway evaluates policy → approval required
3. Gateway creates an `ApprovalRequest` in SQLite
4. Gateway checkpoints the session with `YieldReason::ApprovalRequired` (HMAC-signed)
5. Turn execution pauses; approval request is emitted

#### Checkpoint Structure

Approval suspension is stored as a `SessionCheckpoint` under `.gateway/checkpoints/<session_id>/<turn_id>.checkpoint.json`. The checkpoint is HMAC-SHA256 signed and includes the full conversation history, the pending tool call, remaining tool calls in the batch, and loop-guard state.

#### Resume Flow

1. Operator approves (or rejects) the approval request
2. Gateway applies the decision through `apply_decision`
3. The scheduler wakes the session from checkpoint
4. For `sandbox_exec` approvals: gateway records session approval grants for the detected hosts (enabling auto-approval of subsequent calls to the same hosts within this root session)
5. Gateway injects `approval_ref` into the suspended tool call and resumes the reasoning loop
6. The agent re-issues the tool call with `approval_ref`; the gateway executes it normally and injects the real tool result into conversation history
7. Gateway executes any remaining tool calls from the original batch
8. Checkpoint is deleted after successful resume

### Auto-Resume Behavior

When a session is re-entered (e.g., gateway restart, new event for an existing session), the gateway checks for the latest checkpoint and evaluates whether to auto-resume:

| Yield Reason | Auto-Resume Condition |
|--------------|----------------------|
| `Hibernation` | Always |
| `BudgetExhausted` | Budget available again |
| `MaxTurnsReached` | Always |
| `ManualStop` | Always |
| `Error` | Always |
| `UserInputRequired` | Interaction status is `Answered` |
| `ApprovalRequired` | Via turn continuation (approval resolved) |
| `EmergencyStop` | **Never** — requires manual re-activation |

### Session Forking

Create a new session that starts from the conversation state at any checkpoint, optionally with a branch message for exploring alternative paths.

#### CLI

```bash
# Fork from latest checkpoint
autonoetic trace fork session-123

# Fork from a specific turn
autonoetic trace fork session-123 --at-turn 5

# Fork with a branch message (try a different approach)
autonoetic trace fork session-123 --at-turn 5 --message "try a different approach"

# Fork into a different agent
autonoetic trace fork session-123 --agent researcher.default

# Fork and immediately start chatting
autonoetic trace fork session-123 --at-turn 5 --interactive

# Machine-readable output
autonoetic trace fork session-123 --json
```

#### JSON-RPC API

Method: `session.fork`

```json
{
  "source_session_id": "session-123",
  "branch_message": "optional: try a different approach",
  "new_session_id": "optional: custom-id (auto-generated if omitted)",
  "target_agent_id": "optional: fork into a different agent"
}
```

Response:

```json
{
  "new_session_id": "fork-xxxx",
  "source_session_id": "session-123",
  "fork_turn": 42,
  "history_handle": "sha256:...",
  "message_count": 5
}
```

#### How Forking Works

1. Loads the checkpoint's conversation history from the content store
2. Generates a new session ID (`fork-{uuid}`) or uses the provided one
3. Optionally appends a branch message to the history
4. Stores the history under the new session ID
5. Returns fork metadata (new session ID, source, fork turn, history handle)

Forks can themselves be forked (multi-level branching). Forking fails if no checkpoint exists for the source session.

---

## Queryable Event Store

Causal chain events are mirrored to SQLite for agent learning queries.

### Tables

**`causal_events`** — Queryable mirror of causal chain JSONL:

| Column | Description |
|--------|-------------|
| `event_id` | UUID matching JSONL log_id |
| `agent_id`, `session_id`, `turn_id` | Context |
| `category` | tool_invoke, llm, lifecycle, memory... |
| `action` | requested, completed, failure... |
| `status` | SUCCESS, ERROR, DENIED |
| `enforced_rules` | JSON array of constitutional rule/right IDs this event enforced (default placeholder `R+++3` when none) |
| `target` | Tool name, model name, etc. |
| `payload` | Full JSON (not truncated) |
| `timestamp` | RFC3339 |

#### Principle-aware enforcement events

Enforcement events carry the `P-x.y` / `Ri-x.y` rule/right IDs they enforce in
`enforced_rules`, and (for richer events like `loop_guard.tripped`) the
resolved owning **clause** in the payload. The `enforcement_register`
reverse-maps a `P-x.y` / `Ri-x.y` ID to its owning principle or right,
so breaches correlate by **constitutional clause**, not by ad-hoc rule
strings. See [Contract Health](#contract-health) below.

**`execution_traces`** — Full code execution results:

| Column | Description |
|--------|-------------|
| `trace_id` | UUID |
| `event_id` | Joins to `causal_events.event_id` — the universal correlation key |
| `tool_name` | sandbox_exec, agent_revision_promote... |
| `command` | The executed command |
| `exit_code` | Process exit code |
| `stdout`, `stderr` | Full output (not truncated) |
| `duration_ms` | Execution wall time |
| `success` | Boolean |
| `error_type` | compilation, runtime, permission, validation, resource, conflict, quota_exceeded, not_found, timeout |

### Agent Learning Tools

**`execution_search`** — Query past executions:
```json
{
  "tool_name": "sandbox_exec",
  "success": false,
  "error_type": "compilation",
  "command_pattern": "%client.rs%",
  "limit": 5
}
```

**`knowledge_search`** (with `tags`) — AND-match tagged memories:
```json
{
  "tags": ["type:error_lesson", "domain:http"],
  "limit": 10
}
```

---

## Contract Health

Trust-through-predictability holds only if breaches are detected and corrected —
so "report and correct" is a peer of "constrain." The contract-health view is
the standing tally behind that half of the loop: how often each constitutional
clause (principle/right) has actually been enforced.

It reads the `enforced_rules` carried on `causal_events`, attributes each
`P-x.y` / `Ri-x.y` rule/right ID to its owning clause via the
`enforcement_register` (`clause_of_rule`), and tallies occurrences per clause.
The `R+++3` event-attribution placeholder is skipped (every event carries it by
default); rule IDs not present in the register surface as `unattributed`, so
coverage gaps stay visible rather than silently dropped.

- **Code**: `GatewayStore::contract_health(since)` →
  `enforcement_register::ContractHealth { by_clause, unattributed }`
- **CLI**: `autonoetic trace contract-health [--since <RFC3339>] [--json]`

This is the foundation for principle-aware sentinel correlation; see
`docs/design/divergence-sentinel-design.md`.

---

## Live Digest

Real-time session narrative replacing the flat timeline.md.

### Storage

```
.gateway/sessions/{session_id}/digest.md
```

### Structure

```markdown
# Session Digest: {session_id}
Agent: {agent_id} | Started: {timestamp}

---

## Turn 1 — {timestamp}
**Action:** Called `sandbox_exec` with `python3 tests/run_all.py`
**Result:** 12 tests passed, 1 failed
**Reasoning:** Running full test suite first.

## Turn 2 — {timestamp}
**Action:** Edited `src/http/client.rs`
**Error:** Compilation failed — missing `Send` bound
**Fix:** Added `+ Send` to trait bound
**Artifact:** Modified `src/http/client.rs` (art_8f2a)
```

### Tools

- **`digest_annotate`** — Agent adds reasoning/decision notes
- **`digest_query`** — Search past session digests

### Session Room

The **canonical timeline** (`live_digest_events`) built from the live digest is
the spine of the **Session Room** — a channel-agnostic, importance-ranked,
multi-actor view of a session that channels (the terminal TUI, and external
bridges) consume as gateway API clients. See
[Session Room — Architecture](session-room-architecture.md) and the
[user guide](session-room.md).

---

## Session Read Cache

A per-session, in-memory result cache for **pure read tools** memoizes deterministic reads so an agent that re-reads the same handle across turns does not re-execute the tool or re-inject identical content into the transcript. It lives on `GatewayStore` (`session_read_cache`) keyed by exact `session_id`, and is consulted in `ToolCallProcessor::execute_tool_call` *before* dispatch.

| Tool | Policy | Invalidated by |
|---|---|---|
| `resolve` | Cache forever in-session (content-addressed) | never |
| `agent_inspect` | Cache | `skill_install`, `agent_revision_create`, `agent_revision_create_from_intent`, `agent_revision_promote`, `agent_revision_rollback` |
| `artifact_inspect` | Cache | `artifact_build` |

Properties:

- **Keyed by exact session id**, not root — a cached `resolve` result is never served to a sibling session, preserving per-session content visibility.
- **Wraps only the raw `registry.execute` output**; disclosure registration and secret redaction still run on every hit, so caching is transparent to those invariants.
- **Bounded + size-guarded**: per-session LRU of 128 entries; results over 1 MiB are never stored.
- **Invalidation is coarse but correct**: a mutating tool clears the affected tag class (`AgentExistence` / `ArtifactMetadata`) across *all* session caches, so a child session's promote invalidates the parent's `agent_inspect` cache. `resolve` is never invalidated.
- **Audited**: a cache hit emits a `tool_call.cache_hit` causal event (and the normal execution trace still records), so the causal chain shows every logical tool call.

Grounding: extends the determinism-skip principle of P-2.6 / P-2.7 (approved-execution caching) to pure reads, where the safety argument is stronger — there is no side effect to skip.

---

## Observability Surface

The observability surface lets agents discover and inspect session reports across sessions. It is built on top of the causal chain (the authoritative spine) and is complementary to `execution_search` (which searches raw tool traces within a session).

### Architecture

1. **Session reports** are written to the session directory during execution (JSON, markdown, HTML)
2. On session close, the **hook system** fires a `session.closed` event
3. The `publish_report` hook action reads the report, writes it to the content store, and registers it in the `published_session_reports` catalog
4. Agents use `observability_search` to discover reports and `observability_read` to fetch them by URI

### URI Scheme

All observability resources use `autonoetic://observability/roots/<root>/...`:

| URI | Resource |
|-----|----------|
| `.../report` | Full session report |
| `.../report/overview` | Compact overview (status, counts) |
| `.../report/agents` | Agent list |

### Report Links

Every node in the JSON report includes a `links` object with URI backlinks:

- **Agent nodes** → self, session, causal, traces
- **Timeline events** (when `event_id` present) → self, session, causal
- **Error items** (when `event_id` present) → self, session, causal
- **Approval items** → self, session, causal

---

## Hook System

The gateway has a configurable hook system that replaces hardcoded reactive behaviors. Hooks bind gateway events to actions:

```yaml
hooks:
  - on: "session.closed"
    action: "publish_report"
    async: true
  - on: "approval.resolved"
    action: "deliver_signal"
    async: true
```

### Available Events

| Event | Trigger |
|-------|---------|
| `session.closed` | Session finishes normally |
| `session.suspended` | Session suspends for approval or user input |
| `approval.resolved` | An approval request is approved, rejected, or cancelled |
| `approval.requested` | A new approval request is created |
| `workflow.join.satisfied` | A workflow join condition is met |
| `artifact.created` | A new artifact is built |
| `agent.promoted` | An agent revision is promoted |
| `emergency_stop` | Emergency stop is triggered |
| `policy.decision` | After a `causal_events` row insert matching hook filters (DENIED/ERROR, or SUCCESS with a non-baseline rule such as not only `R+++3`) — observer-only |

### Available Actions

| Action | Description |
|--------|-------------|
| `publish_report` | Reads session report, writes to content store, registers in catalog |
| `deliver_signal` | Delivers a signal to a waiting session (approval, workflow join) |
| `agent_spawn` | *(reserved)* Spawns an agent in response to an event |
| `http.callback` | Sends an HMAC-signed HTTP POST to an allowlisted external URL |

### Hook Dispatch

- **Async hooks** return immediately; the action runs in a background tokio task
- **Sync hooks** block the triggering operation until the action completes
- Failed hooks log a warning but do not fail the triggering operation

### Constitutional observability (`policy.decision`)

Hooks with `on: "policy.decision"` run **after** a matching row is inserted into `causal_events`. They are **observer-only**: they cannot change allow/deny outcomes; the gateway has already persisted the audit row.

**When this event is emitted**

| Causal `status` | `enforced_rules` | Emit? |
|-----------------|------------------|--------|
| `DENIED` or `ERROR` (any casing) | any | yes |
| `SUCCESS` | contains at least one rule **other than** `R+++3` | yes |
| `SUCCESS` | only `R+++3` | no (avoids noise on routine successes) |
| other (e.g. `active`) | — | no |

**`message_template` placeholders** (also present on `HookContext` where applicable): `{{event}}` → `policy.decision`; plus `{{root_session_id}}`, `{{session_id}}`, `{{agent_id}}`, `{{event_id}}`, `{{rule_ids}}`, `{{primary_rule_id}}`, `{{decision}}`, `{{status}}`, `{{category}}`, `{{action}}`, `{{target}}`, `{{reason}}`, `{{turn_id}}`, `{{source}}`.

**Example: spawn a dedicated observer agent** when a policy-relevant causal row is written. The target agent must exist under `agents_dir`. `agent.spawn` hooks **must** use `async: true`. Use `allowed_agents` so only your observer can be spawned from this hook.

```yaml
hooks:
  - on: "policy.decision"
    action: "agent.spawn"
    async: true
    params:
      agent_id: "constitutional-observer.default"
      message_template: |
        Causal policy event: {{event}}
        root={{root_session_id}} session={{session_id}} agent={{agent_id}}
        status={{status}} decision={{decision}} primary_rule={{primary_rule_id}} rules={{rule_ids}}
        category={{category}} action={{action}} target={{target}} event_id={{event_id}} turn={{turn_id}}
        reason={{reason}}
    allowed_agents:
      - constitutional-observer.default
```

The spawned run uses a dedicated session id (`hook-spawn-…`) under the same **root** session as the triggering context so tree-wide limits and emergency stop still apply.

---

## Unified Gateway Database

All transactional state in a single SQLite database:

```
.gateway/gateway.db
├── schema_migrations      # Ordered schema version tracking
│
│   ── Revision & Activation ──
├── agent_revisions        # Immutable revision records (content_digest, status, materialization path)
├── agent_aliases          # Mutable alias → revision pointer (one per logical agent)
├── session_agent_bindings # Per-session pinned revision + runtime_lock_hash
├── promotion_history      # Audit trail of alias movements (promote/rollback)
│
│   ── Evaluation ──
├── eval_suites            # Published suite definitions with case specs
├── eval_runs              # Queued/running/completed eval runs
├── eval_case_results      # Per-case outputs, scores, and failure details
│
│   ── Workflow & Approval ──
├── approvals              # Approval gates
├── user_interactions      # user_ask questions/answers
├── workflow_events        # Workflow event log
├── workflow_index         # Root session → workflow mapping
│
│   ── Execution & Audit ──
├── active_executions      # Running execution leases
├── emergency_stops        # Circuit breaker audit trail
├── causal_events          # Queryable mirror of causal chain JSONL
├── execution_traces       # Full execution results (stdout, stderr, exit_code)
├── live_digest_events     # Real-time session digest events
│
│   ── Memory & Artifacts ──
├── memories               # Tier 2 durable memory
├── memory_tags            # Tag index for knowledge_search tag filtering
├── artifact_refs          # Short ref → digest mapping
├── short_id_index         # LLM-friendly short IDs for revisions and runs
│
│   ── Observability & Hooks ──
├── published_session_reports      # Published report catalog (root_session_id → handles + metadata)
├── published_session_reports_fts  # FTS5 index for observability_search
└── hook_deliveries                # Hook dispatch tracking (idempotency + retry state)
```

### `user_ask` answers and gateway orchestration

`user_interactions` rows can store `workflow_id`, `task_id`, and `checkpoint_turn_id` when the question was raised from a workflow task (tool run context). **Adapters and CLIs should submit answers via** JSON-RPC `interaction.answer` or `interaction.resolve_and_answer` (or the shared in-process orchestrator used by the chat TUI) so paused workflow tasks and `UserInputRequired` checkpoints resume deterministically—not via SQLite writes alone.

See [`plan-channel-agnostic-interaction-answering.md`](./plan-channel-agnostic-interaction-answering.md).

### Retention Policy

Configured in gateway config:

```yaml
retention:
  execution_traces_days: 30   # 0 = forever
  causal_events_days: 90      # 0 = forever
```

Applied automatically on gateway startup.

---

## Emergency Stop

Root-session circuit breaker for operator intervention.

### Authorization

| Requester | Allowed |
|-----------|---------|
| User/Operator | ✓ |
| Gateway (security_policy) | ✓ |
| Agent with `EmergencyStop` capability | ✓ |
| Other agents | ✗ Permission Denied |

### Behavior

1. Persist stop request to `emergency_stops` table
2. Mark workflow `EmergencyStopping`
3. Kill sandbox child processes (SIGKILL)
4. Abort running tokio tasks
5. Cancel pending approvals and user interactions
6. Delete session approval grants (prevent post-stop auto-approval)
7. Write terminal checkpoint with `YieldReason::EmergencyStop`
8. Finalize status to `EmergencyStopped`

### CLI

```bash
autonoetic gateway emergency-stop <root_session_id> --reason "Security incident"
```

---

## Human Escalation

The gateway supports two escalation paths: **session escalation** (when an agent is stuck and needs human guidance during execution) and **federation escalation** (structured promotion review by the operator).

### Session Escalation (`session_escalate`)

### How Session Escalation Works

1. **Agent calls `session_escalate(target="human", reason, context, urgency, suggested_actions)`**
2. **Gateway creates `ApprovalRequest`** with `ScheduledAction::SessionEscalate` — this is a blocking approval, not advisory
3. **Lifecycle detects `escalation_required: true`** sentinel in the tool response, saves a checkpoint with `YieldReason::HumanEscalation`, and returns `TurnOutcome::Escalated`
4. **Agent session is suspended** — it cannot continue until the approval is resolved
5. **Operator approves** via `autonoetic gateway approve apr-xxx --reason "guidance note"`
6. **Gateway persists the operator's guidance** in the `decision_reason` column (separate from the agent's original `reason`)
7. **Session resumes from checkpoint** — the operator's guidance note is injected as a system message into the conversation history

### Session Escalation Authorization

| Target | Blocking? | Creates Approval? |
|--------|-----------|-------------------|
| `human` | Yes | Yes — `SessionEscalate` approval |
| `reasoning_llm` | No | No — advisory only |
| `specialist` | No | No — advisory only |

### Session Escalation Checkpoint Resume

On checkpoint resume for `HumanEscalation`:
- If approval is still pending → bail with "waiting for escalation approval"
- If rejected/cancelled → bail with "escalation was rejected"
- If approved → restore `LoopGuard` state, inject operator guidance as system message, resume execution

### Federation Escalation (`federation.escalate`)

Structured promotion review where the planner bundles verdicts from all federation roles into an `EscalationMessage` and submits them for operator decision:

1. **Planner collects verdicts** from `static_evaluator`, `unit_test_runner`, `auditor` via `promotion_query`
2. **Planner calls `federation.escalate`** with all role verdicts and a synthesis
3. **Gateway persists `EscalationMessage`** in the `escalations` SQLite table with status `Pending`
4. **Operator reviews** via `admin.escalation_list` / `admin.escalation_inspect`
5. **Operator decides** via `admin.escalation_resolve(Approved | Rejected)` — `decided_by` and `decision_reason` recorded
6. **Promotion gate (FullJury)** checks for an approved escalation before allowing promotion

The operator may request additional evaluation (e.g., sealed evaluation) and the planner re-escalates with the new verdict. Unlike session escalation, federation escalation does not suspend the planner — it is an asynchronous operator review.

### Design Rationale

This follows the **Separation of Powers** principle: the agent can request help, but only the gateway can unblock execution. The operator's guidance is mechanically injected — no agent interpretation or filtering.

In addition to session-level escalation, the gateway supports **federation escalation** for structured promotion review — see [Promotion Federation](#promotion-federation) below.

---

## Promotion Federation

The promotion gate uses a **federation of evaluation roles** — not a single evaluator — to produce promotion verdicts. Each role has a different methodology, produces independent verdicts, and the operator is the final arbiter.

### Federation Roles

| Role | Agent ID | Method | Network? |
|------|----------|--------|----------|
| **Auditor** | `auditor.default` | Security review, capability consistency, SKILL.md audit | No |
| **Static Evaluator** | `static_evaluator.default` | Static code review: correctness, credential flow, URL patterns, behavioral contracts | No |
| **Unit Test Runner** | `unit_test_runner.default` | Runs artifact's built-in test suite in a no-network sandbox | No |
| **Sealed Evaluator** | `sealed_evaluator.default` | Dynamic execution in sealed sandbox with fixture-proxied HTTP | Sealed proxy only |

The planner orchestrates federation: it inspects the artifact type, runs the `unit_test_runner` correctness gate first (for artifact-backed agents), then spawns the review roles (`auditor.default` + `static_evaluator.default`) in parallel, collects verdicts via `promotion_query`, and escalates the consolidated report to the operator.

### FullJury Gate

When federation roles (`static_evaluator` or `unit_test_runner`) have recorded verdicts for an artifact, the promotion gate mechanically enforces:

1. **Distinct identity** (P-2.17): each federation role's agent ID must differ from the revision proposer and from every other federation role
2. **Operator approval** (P-2.22): an approved `EscalationMessage` must exist for the artifact + revision pair
3. **Legacy compatibility**: artifacts without federation verdicts continue through the existing Full/AuditOnly gate

This is a fifth gate mode (`FullJury`) that activates on top of the legacy gate when federation verdicts are present. A compromised planner cannot bypass it — the gateway checks `has_federation_roles()` mechanically and refuses promotion without an approved escalation.

### EscalationMessage

The `EscalationMessage` type (`autonoetic-types/src/escalation.rs`) is a channel-agnostic structured payload that carries federation verdicts from the planner to the operator:

```
Planner collects verdicts → federation.escalate → EscalationMessage persisted
Operator reviews → admin.escalation_list / admin.escalation_inspect
Operator decides → admin.escalation_resolve(Approved | Rejected)
Gate checks escalation status → promote or reject
```

Key properties:
- **Capability-gated**: only agents with `AgentSpawn` can call `federation.escalate`
- **Dedup**: a second escalation for the same (artifact, revision) is rejected while a previous one is `Pending`
- **Audit trail**: `decided_by` and `decision_reason` recorded on resolution
- **Admin routes**: `admin.escalation_list`, `admin.escalation_inspect`, `admin.escalation_resolve`

### Unified pending-decisions view

An operator's outstanding decisions live in four separate tables — `approvals`,
`user_interactions`, `escalations`, and `plan_frames` — each with its own list
RPC and its own answer verb. The **`operator.pending`** JSON-RPC method
(`{root_session_id}`) is a single read-only aggregation over all four for one
root session, returning a normalized list (oldest-first) where each item carries
its `kind` (`approval` / `interaction` / `escalation` / `plan`), age, a one-line
summary, and an `answer` hint naming the RPC that resolves it (`approvals.approve`,
`interaction.answer`, `admin.escalation_resolve`, `planframes.approve`). This is
the server-side version of the mapping the room TUI applies client-side, so a
headless operator no longer has to poll four RPC families to see what is waiting.
(Issue #722 Stage 1; a coherent expiry policy and a CLI answer path are staged
follow-ups.)

---

## Recording Mode

The operator can run an agent with `--record-network` to capture real HTTP traffic as redacted fixture files. These fixtures then serve as reproducible baselines for sealed evaluation.

### How It Works

1. **Operator starts recording**: `autonoetic agent run <agent> --record-network --duration 5m`
2. **Gateway sets `sandbox_network: recording`**: the HTTP proxy intercepts outbound requests
3. **Proxy captures traffic**: each request/response pair is stored with mandatory redaction
4. **Redaction is mechanical**: the `redact_fixture` function strips credentials, Authorization headers, cookies, API keys, bearer tokens, and sensitive query parameters before storage
5. **Fixture sets are content-addressed**: each recording session produces a `FixtureSet` identified by SHA-256 digest, stored as an immutable artifact

### Redaction Policy

| Category | Fields redacted |
|----------|----------------|
| Request headers | `authorization`, `cookie`, `x-api-key`, `proxy-authorization` |
| Response headers | `set-cookie`, `www-authenticate`, `proxy-authenticate` |
| Query parameters | `token`, `api_key`, `apikey`, `secret`, `key`, `password`, `auth`, `signature`, `access_token`, `refresh_token` |
| Body (regex) | `bearer <token>` → `bearer [REDACTED]` |

### Sealed Evaluator Replay

Recorded fixture sets can be used for deterministic sealed evaluation:

```bash
autonoetic eval sealed --artifact-ref ar.xxx --fixture-set fs.yyy
```

The sealed evaluator replays the artifact against recorded traffic via the fixture proxy. Every HTTP call is intercepted and served from the fixture set — no live network access.

---

## Post-Promotion Review

After an agent is promoted and live, a background sentinel periodically reviews it for operational drift.

### Tier 1: Observability Review (all agents)

Runs daily for every promoted agent, regardless of whether fixtures exist:
- **Causal event trends**: tool failure rate, authorization denials, suspension count — compared against the previous period
- **Sentinel findings**: new security findings accumulated since last review
- **Thresholds**: tool-failure-rate > 1.5× → warning; > 3.0× → critical; auth denials doubled → critical; suspensions doubled → critical
- **Minor findings**: written to `security_findings` as advisory
- **Critical findings**: escalate to operator via `EscalationMessage` with `EscalationType::PostPromotionAnomaly`

### Tier 2: Fixture-Based Drift (deferred)

For agents with recorded fixture sets, the review additionally:
- Compares baseline (first recording) against current (latest recording) for endpoint drift
- Runs sealed evaluator replay of baseline fixtures against the current revision to detect regressions

Tier 2 is deferred pending wide adoption of the `--record-network` workflow.

### Safety

- **Advisory only**: the review never rolls back a revision automatically — the operator decides all remediation
- **Same escalation channel**: reuses the same `EscalationMessage` type and `admin.escalation_list` as federation escalations
- **Scheduled**: fires via the scheduler tick alongside sentinel sweeps; configurable interval

---

## Scheduled Tasks

The gateway provides first-class scheduled task support through `scheduler.cron.*` tools, allowing agents to register recurring work that is durably persisted and triggered by the background scheduler tick.

### Data Model

Scheduled jobs are stored in the `scheduled_jobs` SQLite table (schema v7) with the following key fields:

| Field | Description |
|-------|-------------|
| `job_id` | Primary key (e.g., `sj-<uuid>`) |
| `owner_agent_id` | The agent that created the job (bound to `manifest.agent.id` at creation) |
| `root_session_id` | Root session scope for the job |
| `target_agent_id` | Agent to trigger when the job fires |
| `message` | Prompt/message sent to the target agent |
| `cron_expr` | Normalized 5-field cron expression |
| `timezone` | Always `UTC` in v1 |
| `next_run_at` | RFC3339 timestamp of next trigger |
| `status` | `active`, `paused`, or `cancelled` |
| `generation` | Optimistic locking counter for atomic claim |

### Execution Model

1. **Scheduler tick** loads active jobs where `next_run_at <= now`
2. **Atomic claim-and-advance**: the job's `next_run_at` is advanced to the next occurrence in the same UPDATE that claims it, preventing duplicate triggers
3. **Workflow enqueue**: the job creates a `QueuedTaskRun` in a durable workflow, reusing the existing async task execution path
4. **Error backoff**: if enqueue fails, the job records `last_error` and advances `next_run_at` by 60 seconds

### Cron Expression Support

Both explicit cron and constrained natural-language phrases are supported:

| Pattern | Example | Normalized Cron |
|---------|---------|-----------------|
| Interval | `every 10 seconds` | `every 10 seconds` |
| Interval | `every 5 minutes` | `0/5 * * * *` |
| Interval | `every 2 hours` | `0 0/2 * * *` |
| Daily | `every day at 09:00` | `0 9 * * *` |
| Weekly | `every monday at 14:30` | `0 14 * * 1` |
| Explicit | `*/15 * * * *` | `*/15 * * * *` |

Second-resolution scheduling (`every N seconds`) is supported.

### Security Model

- **Ownership isolation**: agents can only list, pause, resume, or cancel jobs they own
- **Capability gating**: `SchedulerAccess` capability required for all `scheduler.cron.*` operations
- **Approval preservation**: scheduled task execution uses the same workflow paths, so sandbox approvals and remote-access checks still apply
- **Guardrails**: `min_interval_secs` (default 1) prevents abusive high-frequency schedules; `max_per_root` (default 50) caps jobs per root session; sub-10s intervals require script-mode targets

### Output Delivery

Scheduled task output is delivered directly to all connected chat terminals — no LLM involvement:

1. **Task execution**: The target agent runs (script or reasoning mode), stdout/reply is captured as `result_summary`
2. **Event emission**: A `task.completed` workflow event is emitted with `result_summary` in the payload
3. **CLI polling**: The terminal's `check_signals` loop (every 1s) discovers the new event
4. **Display**: For `sched-*` workflows, the output is shown as a `🔔` Signal message: `🔔 [12:15:07] joke-ticker: Why don't scientists trust atoms?`

The `WorkflowJoinSatisfied` signal is **not** sent to the planner for scheduled task completions. This avoids unnecessary LLM calls, token costs, and turn budget exhaustion — the planner has no meaningful role after setting up the schedule.

### Session Lifecycle

Scheduled jobs are **decoupled from session lifecycle** — they are gateway-level durable records that persist and fire independently of whether their root session is active, suspended, or closed.

| Session Event | Effect on Scheduled Jobs |
|---------------|-------------------------|
| **Normal session close** | No effect. Jobs remain `active` and continue firing on schedule. |
| **Session suspension** (e.g., pending approval) | No effect. Jobs fire independently. |
| **Session resume** (checkpoint respawn or turn continuation) | Jobs continue unaffected. The workflow run for the root session is not re-created on resume; it is loaded on demand when the first `agent_spawn` delegation occurs. |
| **Emergency stop** | All `active` jobs for the root session are cancelled via `cancel_scheduled_jobs_for_root()`. |

This means:
- Scheduled jobs **outlive individual session turns** — they are intended for long-running recurring work
- If a user closes a session and resumes later, cron jobs created in that session will have continued firing in the background
- Only `emergency_stop_root_session()` explicitly cancels jobs; normal `close_session()` does not

### Configuration

```yaml
scheduled_jobs:
  min_interval_secs: 1     # Minimum interval between triggers
  max_per_root: 50         # Max jobs per root session
  max_due_per_tick: 16     # Max due jobs processed per tick
```

### System Agents

System agents are **operator-declared background agents** that the gateway auto-schedules on startup. Unlike agent-created cron jobs, system agents are defined in `config.yaml` and reconciled idempotently.

```yaml
system_agents:
  - agent_id: evolution-orchestrator.default
    schedule: "0 */4 * * *"
    message: "Run evolution analysis cycle"
    enabled: true
```

On startup, the gateway checks each declared agent:
- If the agent exists and has a `schedule` but no active cron job → creates one (owned by `"system"`)
- If disabled or missing → logs and skips
- If an active job already exists → skips (idempotent)

CLI control: `autonoetic gateway system-agents list|bootstrap|run <agent_id>`

System agent jobs use the same `scheduled_jobs` table and execution path as agent-created jobs — no special privileges. See [Scheduled Tasks Guide](scheduled-tasks.md) for full documentation.

### Fast Scheduler Sidecar

For interval-style jobs (`every N seconds`), the canonical 5-second scheduler tick introduces unacceptable latency. The fast scheduler sidecar runs a parallel loop (default: 200 ms tick) that checks the same `scheduled_jobs` table but only considers interval-mode schedules (cron-style schedules stay on the canonical loop).

Key design decisions:
- **Same DB claim-and-advance**: both loops call `claim_and_advance_due_job`, so double-dispatch is impossible at the database level
- **Pre-filtering**: the fast loop filters out cron-expression jobs before counting (cron expressions are opaque strings the DB cannot parse)
- **Bounded work**: `max_due_per_tick` (default 64) caps the number of jobs claimed per tick
- **Disabled by default**: enable via `fast_scheduler.enabled: true` in config

---

## Promotion Safety Governor

Three soft gates enforced at `agent_revision_promote` time, protecting against promotion storms:

1. **Velocity gate**: limits the number of promotions per alias within a sliding time window (default: 3 per 24 hours). Prevents rapid-fire promote/rollback cycles.

2. **Flapping gate**: rejects promotions where the candidate revision was already promoted recently (scans the last N promotions, default 4). Detects oscillation between two revisions.

3. **Eval-regression gate**: halts promotion when the count of findings (errors, warnings) is strictly increasing across consecutive recent promotions. Requires N adjacent increases (default: 3) within a lookback window (default: 6 promotions).

All three gates are bypassable via `force: true` + `force_reason` (capped at 512 characters). Bypass emits a `governor.override` causal event for audit. The governor is disabled by default; enable via `promotion_governor.enabled: true`.

The governor runs after the existing capability-delta and sentinel gates but before the alias is moved. A TOCTOU race window exists between the velocity check and the actual promotion (the check reads promotion_history, then the promote writes it), but this is acceptable for a default-disabled, force-overridable soft gate.

---

## Curator Decision Journal

When response validation is enabled, the gateway parses the `decision_journal` array from agent output and persists one causal event per entry. This provides a durable, queryable audit trail for memory curation decisions (keep, drop, merge, etc.).

Each entry produces a `curator.decision` causal event with:
- `target`: the memory or resource the decision applies to (enables direct queries like "why was memory X dropped?")
- `action`: what was done
- `reason_code`: stable machine-readable code
- `reason_detail`, `metric_values`, `confidence`: optional structured context

A summary event (`decision_journal_recorded`) is also emitted per agent run, carrying the total entry count. Events are sequenced (0, 1, 2, ...) for deterministic batch ordering within a single run.

The category is configurable per agent type (defaults to `curator`), making the journal reusable for other decision-making agents beyond the memory curator. The gateway wires the `revision_id` from the session's agent binding into each event for full provenance tracking.

---

## Error Taxonomy

Native tools return structured error responses using a consistent envelope. When a tool call fails, the gateway returns a `ToolError` JSON object:

```json
{
  "ok": false,
  "error_type": "validation",
  "message": "schedule interval (5s) is below the minimum allowed (10s)",
  "repair_hint": "Use a less frequent schedule."
}
```

### Error Types

| Error Type | Meaning | Agent Can Recover By |
|------------|---------|---------------------|
| `validation` | Malformed input, missing required field, policy denial | Fixing the request, adjusting parameters |
| `permission` | Agent lacks required capability or scope | Requesting additional authorization, adjusting scope |
| `resource` | Missing file, unavailable service, rate limit | Retrying with backoff, using an alternative |
| `execution` | Tool ran but produced an unexpected result | Inspecting output, adjusting approach |
| `conflict` | Duplicate entry, state conflict, concurrent modification | Resolving the conflict, retrying |
| `quota_exceeded` | Budget exhausted, max attempts reached | Waiting, reducing usage, using alternative |
| `not_found` | Requested resource does not exist | Creating it, using an alternative |
| `timeout` | Operation exceeded its time limit | Retrying with backoff |
| `fatal` | Corrupted state, invariant violation, unsafe condition | **Not recoverable** — session should abort |

### LoopGuard Integration

The LoopGuard uses `error_type` to distinguish recoverable from non-recoverable failures:

- **Permission errors do NOT count against the tool failure budget** — the agent cannot fix them by retrying with different arguments, so counting them would unfairly exhaust the budget.
- **Fatal errors** indicate genuine invariant violations and should trigger immediate session abort.
- All other error types count normally against the per-tool failure budget (default: 5 failures).

#### Trip conditions

The guard's main independent trip conditions are below, each attributed on the `loop_guard.tripped` causal event with a stable `reason` code and the constitutional rule whose text describes it. Thresholds are configurable; current defaults live in `docs/config-reference.md`:

| `reason` | Condition | Rule |
|---|---|---|
| `tool_failure_budget` | A single tool exceeds `max_tool_failures` | P-7.5 |
| `no_meaningful_progress` | `current_loops` reaches `max_loops_without_progress` — consecutive LLM steps with no progress-resetting tool result | P-7.7 |
| `rotating_polling_pattern` | The last `rotation_window_size` successful calls hold ≤ `rotation_distinct_floor` distinct fingerprints — an agent cycling a small set of read-only tools without semantic progress. A result carrying `side_effect_state: "committed"` clears the window. | P-7.19 |
| `redundant_roster_polling` | An idempotent read-only roster tool (`agent_list` / `agent_inspect` / `agent_discover`) is called `roster_repeat_floor` times in a row with identical normalized arguments — a fast path that fires before the rotating-polling window fills, since re-listing a directory never yields new data. | P-7.19 |
| `child_failure_budget` | Child-task failures reach `max_child_failures`; does not reset on progress | P-7.20 |
| `repeated_irrecoverable_rejection` | The same `(tool, normalized-error)` irrecoverable rejection (`permission` / `quota_exceeded` / `sandbox_unavailable`) recurs `max_irrecoverable_repeats` times. These are excluded from `max_tool_failures` (retrying can't fix them), so the first occurrences are free; re-asking one already-answered gate (e.g. `agent_revision_promote` against a standing approval requirement) is the single-tool sibling of `no_meaningful_progress`. The count survives an approval suspend/resume. | P-7.7 |
| `repeated_spawn_identity` | A parent delegates to the same `agent_id` with the same structural identity (agent_id + expected_outputs hash + input digest) `max_spawn_identity_repeats` times. Catches the "declare success, produced nothing → respawn the same child" loop that the B.1 output-contract check (RFC #776 Part B.1) converts from silent failure to a typed `output_contract_unmet` failure — the B.4 spawn guard is the loop-safe backstop for that signal. Identity key is deliberately strict; the trip carries the attempt history to an escalation gate. | P-7.19 |

`no_meaningful_progress` (P-7.7) and `rotating_polling_pattern` (P-7.19) are complementary: P-7.7 catches the absence of progress-making results; P-7.19 catches *successful* results that nonetheless make no semantic progress (each distinct, so they reset P-7.7's counter). `redundant_roster_polling` is a narrow, faster sibling of `rotating_polling_pattern` for the specific case of repeated idempotent roster reads. `repeated_irrecoverable_rejection` (P-7.7) covers a gap the per-tool failure budget leaves open: gateway-side gate rejections deliberately don't burn that budget, so a single tool re-hammering one gate would otherwise loop unbounded. `repeated_spawn_identity` (P-7.19) lifts the same shape to the delegation layer: per-spawn success doesn't burn the per-tool budget, so a parent re-trying the same child against the same unmet contract would otherwise loop unbounded — the B.1/B.4 bundle (RFC #776) makes that loop loud at the cheapest detection point.

### Design Rule

All native tools must return either:
1. A success response (`ok: true`) with tool-specific data
2. A `ToolError` envelope (`ok: false`) with `error_type`, `message`, and optional `repair_hint`

Flow-control responses (e.g., `approval_required: true`, `suspended: true`) may use a custom JSON shape but should still include `error_type` when the response represents a failure condition.

---

## Design Principles

1. **Gateway as Lawful Executor**: deterministic enforcement, no improvised judgment — execute proposals, don't make decisions
2. **Agents as Pure Reasoners**: LLMs plan; gateway validates and acts
3. **Autonomy Through Composition**: Complex behavior emerges from simple primitives
4. **No Hardcoded Heuristics**: Business logic in SKILL.md, not platform code
5. **Spec-Driven, Not Code-Driven**: SKILL.md YAML frontmatter is the contract
6. **Pluggable Everything**: Sandbox drivers, LLM providers, capability handlers
7. **Immutable Audit Trail**: Every action logged, hash-chained, verifiable
8. **Content-Addressed Storage**: SHA-256 handles work locally and remotely
9. **Iterative Repair**: Errors are feedback, not failures; agents retry with corrections
10. **Two-Tier Validation**: Soft for LLMs (guidance), strict for scripts (enforcement)
