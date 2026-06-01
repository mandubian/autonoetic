# Separation of Powers: Agents Reason, Gateway Decides

## Core Principle

**Agents are pure reasoners. They propose "what should happen." The gateway is the sole authority that decides "what actually happens."**

Every critical decision — resource allocation, secret access, inter-agent communication, scheduling, approval gates — lives in the gateway. The agent reasons, plans, delegates, and evolves, but it never touches anything sensitive or scarce directly.

This gives Autonoetic its two key properties at once: **powerful autonomous reasoning** (the agent can do anything it can propose) and **constrained execution** (the gateway enforces boundaries the agent cannot bypass).

```
Agent (low-privilege):           Gateway (high-privilege):
┌─────────────────────┐          ┌──────────────────────┐
│  READ:               │          │  MANAGES:             │
│  - SKILL.md          │          │  - Secrets/Vault      │
│  - state/task.md     │          │  - Network sockets    │
│  - skill catalog     │          │  - Filesystem writes  │
│  - memory            │          │  - Agent spawning     │
│  - causal chain      │          │  - Approval gates     │
│                      │          │  - Capability grants  │
│  PROPOSES:           │          │  - Backpressure       │
│  - "run this skill"  │          │                       │
│  - "spawn agent X"   │          │  EXECUTES:            │
│  - "share memory"    │          │  - Sandboxed scripts  │
│  - "schedule task"   │  ──→    │  - Tool invocations   │
│                      │  ←──    │  - API calls          │
│  RECEIVES:           │          │  - Resource allocation│
│  - tool results      │          │                       │
│  - structured errors │          │  AUDITS:              │
│  - memory summaries  │          │  - Causal chain       │
│                      │          │  - Policy violations  │
│  NO ACCESS TO:       │          │  - Spend tracking     │
│  - Raw secrets       │          │                       │
│  - Network directly  │          │  AUTHORIZES:          │
│  - Other agents      │          │  - Inter-agent comms  │
│  - Scheduling        │          │  - Secret injection   │
│  - Approvals         │          │  - Capability grants  │
└─────────────────────┘          └──────────────────────┘
```

---

## Approval Execution Boundary

Approval-gated tool calls are a concrete example of separation-of-powers:

1. **Agent proposes** a privileged action (for example `sandbox_exec` with remote access, or `agent_revision_promote`).
2. **Gateway enforces** the approval gate and records a pending request.
3. **Operator decides** approve/reject.
4. **Gateway executes** the approved action for workflow-bound continuations and returns the real tool result to the resumed turn.

The agent never receives direct authority from approval itself. Approval authorizes the **gateway's execution path**, not a privilege escalation inside the agent runtime.

For non-workflow sessions, the gateway may deliver a durable approval notification and the agent can retry with a validated reference. In both paths, the gateway remains the authority.

---

## Delegation

**Agent proposes** which agent to spawn, when, and with what instructions.

**Gateway decides** whether it's allowed, whether resources are available, and actually creates the process.

### Agent side

The agent's `SKILL.md` declares delegation capabilities:

```yaml
metadata:
  capabilities:
    - agent_spawn
  spawn_policy:
    allowed_targets:
      - researcher.default
      - coder.default
```

The agent's reasoning loop decides when delegation is needed:

```
Goal: "Build a competitive analysis report"

Agent thinks:
  1. I need research → propose: spawn researcher.default
  2. I need code     → propose: spawn coder.default

Agent calls: gateway.agent_spawn(target="researcher.default", instructions="...")
```

### Gateway side

The gateway receives the proposal and checks every boundary:

```
Gateway decides:
  - Does this agent have agent_spawn capability?        ✓
  - Is researcher.default an allowed target?             ✓
  - Is concurrency budget available?                     ✓
  - Is target agent manifest valid?                      ✓
  → EXECUTES: spawns researcher, returns handle
```

The gateway also enforces backpressure (max concurrent spawns, per-agent queue limits) and logs the spawn to the causal chain for audit.

---

## Reevaluation

**Agent proposes** what to do when woken and how often to be woken.

**Gateway controls** the clock, deduplication, and whether the agent is allowed background wakes at all.

### Agent side

The agent's `SKILL.md` declares a reevaluation schedule:

```yaml
metadata:
  background:
    enabled: true
    schedule: "every 20 minutes"
    purpose: "check pending approvals and retry failed tasks"
```

The agent's reasoning loop handles the tick signal:

```
Gateway fires: { signal: "tick", timestamp: "..." }

Agent reads: state/reevaluation.json
Agent thinks:
  - "I have pending_approval_123 from 2 hours ago"
  - "My last scrape task failed with timeout"
  - Proposed action: gateway.approval_status("pending_approval_123")
  - Proposed action: agent_spawn(researcher.default, "retry scrape X")
```

### Gateway side

The gateway owns the scheduler. It fires `tick` signals, deduplicates overlapping wakes, respects backpressure, and logs every wake reason to the causal chain. The agent never sets timers or manages scheduling.

---

## Secrets

**Agent requests** that a secret be injected for a specific tool.

**Gateway decides** whether the agent is authorized, injects the secret as an ephemeral environment variable, and the agent never sees the value.

### The Ephemeral Injection Pattern

This is the critical security boundary. The LLM never sees raw secret values. The agent's state never contains them. The gateway is the sole custodian.

### Agent side

The agent declares which secrets its skills need:

```yaml
metadata:
  capabilities:
    - secrets.get
  declared_secrets:
    - GITHUB_TOKEN
```

The agent requests authorization:

```
Agent thinks: "I need to call the GitHub API"
Agent proposes: gateway.secrets.request("GITHUB_TOKEN", for_tool="github_search")
```

### Gateway side

```
Gateway decides:
  - Does this agent have secrets.get capability?         ✓
  - Is GITHUB_TOKEN in declared_secrets?                 ✓
  - Is tool "github_search" authorized for this secret?  ✓
  → RESULT: "approved"
```

The agent receives only a boolean approval. When the gateway executes the sandboxed skill, it injects `GITHUB_TOKEN=ghp_...` as an ephemeral env var. The secret exists only in the sandbox process memory for the duration of execution.

---

## Knowledge visibility (Tier 2)

**Agent proposes** durable facts via `knowledge_store` — including **who may read** them — using **`visibility`**, not a separate share API.

**Gateway enforces** visibility, session binding, scope policy, retention/expiry, and provenance.

### Visibility modes

| Value | Meaning |
|-------|---------|
| `private` | Only the owning/writing agent can read. |
| `session` (default on store) | Any agent whose tool execution shares the same **session id** (same root workflow) can read. |
| `global` | Any agent in any session can read. |

### Agent side

```
Agent thinks: "The planner should see this finding"
Agent proposes: knowledge_store(
  id="research_findings",
  content="…",
  scope="project_X",
  visibility="session"   // default; omitted is fine when a session is active
)
```

To widen access later (e.g. private → session → global), the agent calls **`knowledge_store` again** with the same `id` and updated `visibility`.

### Gateway side

```
Gateway decides:
  - Is this agent allowed to write Tier 2 / this scope?              ✓
  - For visibility "session": is there a non-empty session context?   ✓
  - On read: does the reader's session match the row (or private/global rules)? ✓
  - Has the row expired per retention?                                ✓
  → EXECUTES: upsert memory row, audit/provenance as today
```

The agent chooses visibility **horizons**; the gateway decides **policy** and **readability** on every recall/search.

---

## The Vocabulary of Proposals

The agent doesn't call functions — it proposes **intent verbs** that the gateway interprets, validates, and executes:

| Verb | Agent Says | Gateway Does |
|---|---|---|
| `execute` | "Run skill X with these params" | Validates capability, spawns sandbox, injects secrets, returns result |
| `spawn` | "Create agent Y with these instructions" | Validates policy, allocates resources, starts agent |
| `store` (knowledge) | "Persist fact Z with visibility V" | Validates policy, binds session/global/private, upserts row |
| `schedule` | "Wake me every N minutes" | Registers with scheduler, deduplicates |
| `recall` | "Get memory matching query Q" | Searches tier2, applies ACL filters, returns summaries |
| `request` | "I need approval for capability C" | Enqueues approval, returns status |

Every verb is a gateway-enforced boundary. The agent proposes; the gateway decides and executes.

---

## Agent Architecture

The agent is a reasoning loop with a capabilities vocabulary but no execution authority:

```
┌─────────────────────────────────────────────────┐
│                 Agent Runtime                     │
│                                                   │
│  ┌───────────┐    ┌──────────┐    ┌───────────┐  │
│  │ SKILL.md  │───→│ Reasoning │───→│ Proposals │  │
│  │ (persona  │    │   Loop    │    │ (intent   │  │
│  │  + rules) │    │           │    │  verbs)   │  │
│  └───────────┘    └────┬─────┘    └─────┬─────┘  │
│                        │                │         │
│  ┌───────────┐    ┌────▼─────┐    ┌─────▼─────┐  │
│  │  Memory   │───→│ Context  │───→│ LLM Call  │  │
│  │ (state/ + │    │ Assembly │    │ (provider │  │
│  │  tier2)   │    │          │    │  agnostic)│  │
│  └───────────┘    └──────────┘    └───────────┘  │
│                                                   │
│  CAN ONLY:              CANNOT:                   │
│  - Read skills          - Access secrets directly │
│  - Read/write state/    - Make network calls      │
│  - Read memory          - Spawn processes         │
│  - Propose actions      - Set schedules           │
│  - Request capabilities - Share memory directly   │
│  - Receive results      - Bypass approval gates   │
└─────────────────────────────────────────────────┘
                        │
                        │ Proposals (JSON-RPC)
                        ▼
┌─────────────────────────────────────────────────┐
│                  Gateway                          │
│                                                   │
│  Receives proposals → Checks policy → Executes   │
│  Enforces capability boundaries                  │
│  Manages all resources and secrets               │
│  Logs everything to causal chain                 │
└─────────────────────────────────────────────────┘
```

---

## Why This Works

**The agent is powerful but constrained.** It can reason, plan, delegate, and evolve — but it can't leak secrets, spam resources, or bypass policy. The LLM has maximum reasoning freedom within an execution cage it cannot escape.

**The gateway is simple but authoritative.** It doesn't understand delegation patterns, reevaluation heuristics, or knowledge evolution workflows. It just validates proposals against policy and executes them. This keeps it generic: completely different agent architectures can use the same gateway with different composition patterns — swarms, consensus-based delegation, ML-driven reevaluation — without changing a line of gateway code.

**The autonoetic properties emerge from agent composition of gateway primitives**, not from the gateway hardcoding orchestration patterns. A planner agent composes `agent_spawn` + `task.board` into delegation. A background agent composes `scheduler.interval` + `agent.state` into reevaluation. A coder agent composes `skill.store` + `approval.queue` into evolution.

The gateway "ensures" autonoetic behavior by making it **possible and auditable**, not by making it **prescriptive**.

---

## Integrity Guarantees

### HMAC-Signed Continuations

When an agent's turn is suspended at an approval boundary, the gateway writes a **signed continuation file** to disk. The file contains an HMAC-SHA256 signature over the serialized turn state, computed with a per-gateway key derived from `continuation_key` (or `node_id` as fallback).

On resume:
1. The signature is verified before deserialization
2. If the signature is invalid (tampered file), the gateway:
   - Rejects the continuation
   - Emits a causal event
   - Cancels the bound approval
3. The `pending_action` on the continuation is compared to the stored approval action (action-equality check) to detect TOCTOU swaps

### Threat Model

| In scope | Out of scope |
|----------|-------------|
| **Malicious agent**: LLM attempting to bypass approval gates, tamper with continuation files, or escalate privileges | **Local filesystem attacker with vault key**: an attacker who has compromised the host and obtained the `continuation_key` or vault master key can forge continuations or decrypt secrets |
| **Agent retry attacks**: re-submitting near-identical approval requests (detected by similarity scoring) | **Kernel-level compromise**: root-level access to the host |
| **Continuation tampering**: modifying `.gateway/continuations/<task_id>.json` on disk (detected by HMAC) | **Supply-chain attacks on LLM providers** |
| **Approval fatigue**: operator approving without review (mitigated by similarity annotations, stats, and diff surface) | |
| **Subtle capability misuse**: an agent using declared capabilities in unexpected ways (detected by auditor/evaluator agents during promotion, not by the gateway alone) | |

The security boundary is **gateway vs agent**, not **gateway vs host**. The gateway trusts the local filesystem and its own key material; it does not trust any data produced or influenced by the agent.

The gateway is not expected to catch every possible violation — no enforcement layer can anticipate every creative misuse of a declared capability. Instead, the system is designed so that **detection is collective**:

- **Auditor and evaluator agents** review artifacts before promotion. They catch behavioral anomalies the gateway cannot detect mechanically.
- **Static analysis** (`RemoteAccessAnalyzer`, `artifact_prepare`) flags network access patterns in source code even when the agent did not declare them.
- **Similarity scoring** on approval requests detects when an agent is flooding or re-trying near-identical actions.
- **The causal chain** makes every action attributable and reconstructable — an operator or agent can audit any session after the fact.
- **Emergency stop** provides a circuit breaker to kill an entire root session when misbehavior is detected.

Security of all is the responsibility of all. The gateway enforces hard boundaries; the ecosystem of agents watches for subtle violations; the causal chain ensures nothing is silent.

### Continuation Cleanup

Continuation files are cleaned up in these scenarios:
- **Successful resume** after approval (normal path)
- **Reject, cancel, or withdraw** — the file is deleted immediately
- **Gateway startup** — a reaper scans for orphaned files whose bound approval is terminal or missing
- **Emergency stop** — all continuation files for the root session are deleted
- **Workflow task cancellation** — the file is deleted

This prevents stale continuation files from accumulating on disk after crashes or interrupted sessions.
