# Session Capability Envelope — Design

**Status:** Design — 2026-06-14 (revised)
**Builds on:** [`docs/design/operator-legibility.md`](operator-legibility.md) §6 (Pillar C),
[`docs/reference/capability-grants.md`](../reference/capability-grants.md)
**Proves out:** session `session-e9436118` (weather forecast agent build, 2026-06-13)

---

## 1. Problem

The plan-as-capability-grant feature (Pillar C, PR #499) materializes network
grants from plan step `agent_id` → declared `NetworkAccess.hosts`. The weather
session exposed three failure modes, **all of which fired simultaneously**:

1. **Plan steps carry `agent_id: null`.** `planner.collaborative` uses steps as
   a coordination checklist; the planner spawns agents by judgment, not by step
   reference. `materialize_plan_grants()` skips null `agent_id` → returns 0.

2. **Agents declare wildcards.** Even when `agent_id` is set, the agents that
   actually perform network calls (`researcher.default`) declare
   `NetworkAccess { hosts: ["*"] }`. Wildcards are intentionally skipped → 0.

3. **Promotion is a separate gate.** The session ended with a **promotion
   approval** (`apr-144ccf1e1c08418c`) asking the operator to acknowledge the
   new agent's capabilities — a *third* approval moment, disconnected from the
   plan approval that already happened 40 minutes earlier.

4. **`planner.default` has no PlanFrame at all.** The basic planner — the
   default front-door agent — doesn't use plans. It spawns specialists directly.
   Any envelope tied to `PlanFrame` is invisible to the majority of sessions.

## 2. Root cause

The current design tries to **derive** the envelope from static declarations
(agent capabilities, plan step agent_ids). But the real envelope is discovered
**through use**: the researcher calls `api.open-meteo.com`, the coder writes
code that talks to `geocoding-api.open-meteo.com`. The envelope lives in what
the session **actually did**, not in what was declared upfront.

Worse, tying the envelope to `PlanFrame` excludes `planner.default` entirely.
The envelope is a **session-level** concept — it should work regardless of
whether the planner uses structured plans.

## 3. Principle

> **Discover, don't declare. The envelope emerges from observed usage and is
> locked at contextual moments.**

The session is an iterative, try/fail process. The planner doesn't know upfront
what hosts it will need. But the gateway **records everything** — every
`sandbox_exec`, every `web_fetch`, every host touched. The envelope is
**accumulated from that history** and proposed for locking when the scope
crystallizes.

The operator's experience:

1. **Researcher** fetches weather from `api.open-meteo.com` (preapproved — it's
   the scout). The gateway records the host access.
2. Operator says **"make it an agent."** The gateway recognizes: this session
   has touched `api.open-meteo.com` + `geocoding-api.open-meteo.com`.
3. Gateway **proposes a lock**: "Pre-authorize these hosts + these capabilities
   for the rest of the session?" The operator sees exactly what was used —
   reality, not prediction.
4. Operator **approves**. The locked envelope materializes as grants.
5. **Coder, tester, promotion** all operate within the locked envelope — no
   further prompts unless the envelope expands.

A new approval appears only when:
- A tool call touches a host **outside** the locked envelope (envelope expansion
  prompt — just the delta).
- The operator explicitly requests to widen the envelope.
- A **new approval surface** emerges (new `Capability` variant — see §6).

## 4. The model: observe → propose → lock

Three phases, repeating throughout the session:

### Phase 1: Observe (passive, always running)

The gateway already records every tool call in `execution_traces` (including
full command strings) and every approval in the `approvals` table. The host
extraction is done by the existing static analyzer (`remote_access.rs`).

A new function queries observed usage on demand:

```rust
/// Hosts that this root session has actually touched, derived from
/// execution_traces + resolved approvals. No LLM judgment — pure query
/// over recorded history.
fn discover_observed_hosts(store: &GatewayStore, root_session_id: &str) -> Vec<String> {
    // 1. Scan execution_traces for sandbox_exec/web_fetch calls in this root session
    // 2. Run remote_access.rs host extraction on each command
    // 3. Union the results with hosts from resolved approvals
    // 4. Return sorted, deduplicated
}
```

No new table. No new recording. The data is already there — we just query it.

### Phase 2: Propose (at contextual moments)

The gateway surfaces observed usage and offers to lock it. **Trigger moments:**

| Trigger | What happens |
|---------|-------------|
| Plan proposal (`planframe_propose`) | Planner includes discovered hosts in the plan's `capability_envelope`; operator approves the plan → locks the envelope |
| "Make this an agent" (agent build flow) | Gateway attaches discovered hosts + expected capabilities to the promotion pre-auth proposal |
| Explicit operator request (`session.envelope.propose`) | Operator asks to see and lock what's been used |
| Approval creation | When a new approval is about to fire, the gateway checks: "you've already used these hosts — lock them to skip future prompts?" |
| Envelope expansion detected | Static analysis finds a host not in the locked envelope → propose expanding the lock |

### Phase 3: Lock (operator approves)

The operator reviews the proposed envelope (observed hosts + capability
projections) and approves. The lock materializes:

- `NetworkAccess { hosts }` → `session_approval_grants` rows (existing table)
- `PromoteWith { capabilities }` → promotion pre-authorization (checked in-memory at promotion time, see §8)
- Future capability types → their respective grant tables (dispatcher pattern, see §7)

After locking, tool calls within the envelope dedup silently against the
existing grant layer. The operator is not prompted again unless the envelope
expands.

## 5. Session-scoped, not plan-scoped

The envelope is a property of the **root session** (or workflow), not the plan.
This is the critical architectural decision:

- **Works for `planner.default`**: no plan needed. The envelope accumulates from
  observed usage and can be locked at any contextual moment.
- **Works for `planner.collaborative`**: the plan can **reference** the session
  envelope (include it in the proposal), but the envelope itself is session-level.
- **Survives plan cancellation**: if a plan is cancelled, the session envelope
  persists. A new plan can reuse it.

### Where the envelope lives

The locked envelope is a set of `session_approval_grants` rows with
`source_approval_id = 'session-envelope:<root_session_id>'`. This reuses the
existing grant infrastructure — no new table for network hosts.

For non-network capabilities (promotion pre-auth, future types), the envelope
is stored as a serialized `Vec<Capability>` on the workflow or a lightweight
`session_envelopes` table:

```sql
CREATE TABLE session_envelopes (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    root_session_id TEXT NOT NULL,
    capability_json TEXT NOT NULL,        -- serialized Capability
    source          TEXT NOT NULL,         -- 'discovered' | 'plan:<id>' | 'operator'
    observed_at     TEXT,                  -- when usage was first observed
    locked_at       TEXT,                  -- when operator approved (NULL = proposed, not yet locked)
    locked_by       TEXT,                  -- who approved
    plan_id         TEXT,                  -- if triggered by a plan approval
    created_at      TEXT NOT NULL
);
CREATE INDEX idx_session_envelopes_root ON session_envelopes(root_session_id);
```

Rows with `locked_at IS NULL` are **proposed** (observed but not yet approved).
Rows with `locked_at IS NOT NULL` are **active** (operator approved, materialized).

## 6. The generalization: `Vec<Capability>`

The envelope is a `Vec<Capability>` — the same enum agents already declare in
their manifests (`autonoetic-types/src/capability.rs`, 25 variants today). This
is open-ended by construction:

- **Network access** → `NetworkAccess { hosts: ["api.open-meteo.com"] }`
- **Promotion pre-auth** → `PromoteWith { agent_id, capabilities }` (new variant, see §8)
- **File writes** (future) → `WriteAccess { scopes: ["/tmp/weather_agent"] }`
- **Destructive ops** (future) → `DestructiveOps { patterns: ["rm /tmp/*"] }`
- **Resource budgets** (future) → `ResourceBudget { max_cpu_secs: 300 }`

When a new approval surface emerges, it enters as a new `Capability` variant.
The moment it exists: it participates in observation, can be proposed for
locking, diffs structurally, materializes via the dispatcher. No schema change
to the envelope concept.

### Why not `network_envelope`?

A hardcoded `network_envelope` field would need a new field for every new
surface (filesystem, destructive ops, resources, federation). `Vec<Capability>`
is one field, open-ended, reusing the type that already exists with its diff
logic (`compute_capability_delta`, `capability.rs:222`) and policy checks
(`policy.rs`).

## 7. Grant materialization (dispatcher)

When an envelope is locked, each capability in it materializes into the
appropriate grant type:

```rust
fn materialize_envelope(
    store: &GatewayStore,
    root_session_id: &str,
    envelope: &[Capability],
    locked_by: &str,
    source: &str,  // "discovered" | "plan:<id>" | "operator"
) -> usize {
    let mut count = 0;
    for cap in envelope {
        match cap {
            Capability::NetworkAccess { hosts } => {
                count += materialize_network_grant(store, root_session_id, hosts, locked_by, source);
            }
            Capability::PromoteWith { agent_id, capabilities } => {
                // Stored in session_envelopes, checked at promotion time (see §8).
                // No session_approval_grants row — promotion is one-shot, not recurring.
            }
            Capability::WriteAccess { scopes } => {
                // Future: filesystem scope grants.
            }
            // Adding a new grant type = adding a match arm here.
            _ => {}
        }
    }
    count
}
```

### Network grant (refactored from existing code)

```rust
fn materialize_network_grant(
    store: &GatewayStore,
    root_session_id: &str,
    hosts: &[String],
    locked_by: &str,
    source: &str,
) -> usize {
    let concrete: Vec<_> = hosts.iter()
        .filter(|h| !h.is_empty() && h.as_str() != "*")
        .collect();
    if concrete.is_empty() { return 0; }
    let targets: Vec<_> = concrete.iter()
        .map(|h| GrantTarget::ExactHost((**h).clone()))
        .collect();
    store.insert_session_grant(
        root_session_id,
        root_session_id,
        root_session_id,  // workflow-level, not agent-specific
        &GrantScope::RootSession,
        &targets,
        locked_by,
        &now_rfc3339(),
        Some(source),
        None,
    ).map(|_| 1).unwrap_or_else(|e| { tracing::warn!(error = %e, "grant failed"); 0 })
}
```

## 8. Promotion pre-authorization

### 8.1 The problem

First promotion of a new agent requires the operator to acknowledge **all**
declared capabilities (`agent_revision.rs:2277`). This is disconnected from
everything that came before — the operator already saw the hosts used, the
capabilities exercised, the artifact built. Then promotion asks again.

### 8.2 The solution: `PromoteWith` capability variant

```rust
pub enum Capability {
    // ... existing 25 variants ...

    /// Pre-authorizes promotion of an agent whose declared capabilities fall
    /// within this set. Checked at promotion time against the session envelope.
    /// If covered, promotion proceeds without re-prompting.
    PromoteWith {
        #[serde(default)]
        agent_id: String,           // empty = any agent created by this workflow
        capabilities: Vec<Capability>,
    },
}
```

### 8.3 Discovery-based promotion pre-auth

The `PromoteWith` entry is **proposed from observed session activity**, not
declared upfront:

1. The session has been building a weather agent.
2. The coder produced an artifact. Static analysis knows its capabilities.
3. When the operator (or planner) signals "ready for promotion", the gateway
   proposes: "Lock promotion pre-auth for `weather.forecast` with these
   capabilities: `[NetworkAccess{hosts}, CodeExecution, ReadAccess, WriteAccess]`?"
4. The capabilities are derived from the **artifact's actual declarations**
   (which the coder already wrote), cross-checked against the session envelope.
5. Operator approves → `PromoteWith` stored in `session_envelopes`.
6. At promotion time: artifact caps ⊆ `PromoteWith.capabilities` → proceed
   silently. Artifact caps ⊄ `PromoteWith.capabilities` → envelope expansion
   → re-approve the delta.

### 8.4 Promotion gate integration

```rust
// In agent_revision_promote, before creating the capability-ack approval:

if let Some(envelope) = session_envelope_for_root(store, root_session_id) {
    if let Some(preauth) = find_promote_with(&envelope, &args.agent_id) {
        if capability_set_covers(&preauth.capabilities, &current_capabilities) {
            // Pre-authorized. Skip the capability-ack approval.
            tracing::info!(target: "promotion", "pre-authorized by session envelope");
            // Proceed to eval/auditor gate.
        } else {
            // Artifact exceeds the pre-authorized set.
            // Fall through to existing approval creation — operator sees the delta.
        }
    }
}
```

`capability_set_covers` reuses the existing `capability_broadening()` logic
from `capability.rs:337` — the artifact is covered if none of its capabilities
are a broadening of the declared set.

## 9. Authorization layering

When a tool call happens:

```
permitted  = agent's own Capability covers the action          (floor — what the agent CAN do)
silent     = session_envelope (locked) covers the action        (pre-approved)
           OR session_approval_grants cover the action
           OR approved_exec_cache hit
otherwise  = create approval request (prompt the operator)
```

The agent capability is the **floor**. The locked envelope is the
**pre-authorization**. The gap between them is where per-call approval lives.

```
  Agent capability (floor):   NetworkAccess { hosts: ["*"] }       (researcher.default)
  Session envelope (locked):  NetworkAccess { hosts: ["api.open-meteo.com"] }
  ──────────────────────────────────────────────────────────────────────
  Tool call to api.open-meteo.com  → silent (within locked envelope)
  Tool call to evil.example.com    → prompt (within floor, outside envelope)
```

## 10. Envelope expansion

When a tool call touches something outside the locked envelope, the gateway:

1. **Checks** if the target was **observed** earlier in the session (maybe it
   was used before the lock but not included). If so, propose expanding the
   lock to include it.
2. **If not observed**, create an approval request for the new target. On
   approval, offer to add it to the locked envelope ("also pre-authorize this
   for the rest of the session?").

This makes every approval a **potential envelope expansion moment**. The
operator can always say "just this once" or "yes, for the whole session."

### Envelope diff (for plan-linked envelopes)

When a plan references the session envelope and the envelope changes, the
`PlanEnvelopeDiff` detects it. This reuses the existing
`compute_capability_delta()`:

```rust
pub struct PlanEnvelopeDiff {
    // ... existing fields (steps, validation, etc.) ...
    pub capability_delta: CapabilityDelta,  // NEW — reuses existing type
}

impl PlanEnvelopeDiff {
    pub fn requires_regate(&self) -> bool {
        // ... existing conditions ...
        || self.capability_delta.has_broadening()   // NEW
    }
}
```

`has_broadening()` (`capability.rs:217`) returns true when capabilities are
added or broadened. Asymmetric: expanding re-gates, narrowing doesn't.

## 11. The weather session, re-imagined

### With `planner.default` (no plan)

```
22:19  Operator: "what is the weather in paris?"
22:19  planner.default → spawns researcher.default
22:19  researcher runs curl https://api.open-meteo.com/...  (preapproved, scout)
       → gateway records host access in execution_traces
22:20  researcher returns weather data. Session observed: {api.open-meteo.com}

22:24  Operator: "make this a reusable agent"
22:24  planner → spawns architect, coder (no plan needed)
22:28  coder builds artifact using api.open-meteo.com + geocoding-api.open-meteo.com
       → gateway records both hosts
22:31  artifact built. Session observed: {api.open-meteo.com, geocoding-api.open-meteo.com}

22:32  Gateway proposes envelope lock:
       "This session has used: api.open-meteo.com, geocoding-api.open-meteo.com.
        The artifact declares: NetworkAccess, CodeExecution, ReadAccess, WriteAccess.
        Lock pre-authorization for the rest of the session?"
22:32  Operator approves → grants materialized, PromoteWith stored

22:35  executor tests the agent → network call covered by locked envelope → silent
22:37  evaluators/auditors run → no approval needed
23:04  promotion → artifact caps ⊆ PromoteWith → proceeds silently

Total operator interactions: 1 (the envelope lock)
```

### With `planner.collaborative` (plan-based)

Same flow, but the envelope lock happens at plan approval time. The plan
proposes `capability_envelope` populated from research output. The operator
approves the plan → envelope locks. Everything after is silent.

## 12. Design decisions

### D1: Session-scoped, not plan-scoped

The envelope belongs to the root session. The plan can reference it (include it
in the proposal), but the envelope persists across plan cancellations and works
without any plan at all. This makes it usable for `planner.default`.

### D2: Discovery-based, not declaration-based

The envelope is accumulated from observed usage (`execution_traces`), not
declared upfront. This eliminates the prediction problem (the planner doesn't
need to guess hosts) and grounds the envelope in reality (only hosts actually
touched are proposed).

### D3: Lock at contextual moments

The envelope is proposed for locking when the scope crystallizes: plan
proposal, artifact build completion, explicit operator request, or when a new
approval is about to fire. The operator is never interrupted for an envelope
lock unless there's a natural reason to ask.

### D4: `PromoteWith` is a pseudo-capability

Promotion pre-authorization is a `Capability::PromoteWith` variant stored in
`session_envelopes`, not a grant table row. Checked in-memory at promotion
time. This avoids a schema migration to `session_approval_grants` and keeps
the promotion logic self-contained.

### D5: Envelope expansion is always just-a-delta

When a tool call exceeds the locked envelope, the operator sees only the delta
(the new host or capability), not the whole envelope re-presented. If the
delta was previously observed, the gateway notes that ("you used this host
earlier at 22:19").

### D6: Wildcards are not materialized

Same as current code: `hosts: ["*"]` can't materialize to a concrete grant.
The envelope should carry concrete targets. Observed hosts are always concrete
(because they were extracted from actual commands).

### D7: PlanFrame can optionally carry an envelope

`PlanFrame.capability_envelope: Vec<Capability>` (serde default = empty) is an
optional way to declare an envelope at plan time. If present and non-empty, it
is the proposed envelope at plan approval. If empty, the session-level
discovery mechanism drives. Both paths coexist.

## 13. What is NOT generalized

- **Rate limits** — a budget, not an authorization. Different axis.
- **Temporal conditions** — a schedule constraint. Different axis.
- **Interaction policies** ("ask before deleting") — a per-action policy, not a
  pre-authorization.

These compose with the envelope but are orthogonal.

## 14. Migration path

| Step | Change | Risk | Benefits |
|------|--------|------|----------|
| 1 | Add `discover_observed_hosts()` — query execution_traces + host extraction | None — read-only query | Foundation for proposals |
| 2 | Add `session_envelopes` table (migration) | Low — new table, no existing data affected | Storage for locked envelopes |
| 3 | Add `materialize_envelope()` dispatcher | None — new function, not yet called | Grant materialization |
| 4 | Add envelope proposal at contextual moments (plan approval, artifact build) | Low — additive, doesn't change existing flows | Operator can lock observed usage |
| 5 | Add `PromoteWith` Capability variant | Low — new variant | Promotion pre-auth type |
| 6 | Wire promotion gate to check `session_envelopes` before creating approval | Medium — touches promotion hot path | Eliminates redundant promotion prompts |
| 7 | Add `PlanFrame.capability_envelope` (optional, serde default empty) | None — backward compatible | Plan-linked envelope for collaborative planner |
| 8 | Update planner guidance (both `.default` and `.collaborative`) to surface envelope proposals | Low — guidance only | LLM-driven lock triggers |

Steps 1–4 ship together (discovery + locking, no behavior change for existing
sessions). Steps 5–6 ship together (promotion pre-auth). Steps 7–8 are
enhancements that can ship independently.

## 15. Future surfaces (proof of generality)

| Future surface | Capability variant | Discovery source |
|---------------|-------------------|------------------|
| Destructive operations | `DestructiveOps { patterns }` | execution_traces (rm, delete commands) |
| Resource budgets | `ResourceBudget { max_cpu_secs }` | execution_traces (duration, resource usage) |
| Federation escalation | `FederateTo { gateways }` | session_escalate calls |
| External side effects | `SideEffects { patterns }` | execution_traces (POST, PUT, DELETE commands) |
| Data disclosure | `DataDisclosure { destinations }` | web_fetch/web_search targets |

Each one: new enum variant → observed from execution_traces → proposed for
locking → materialized via dispatcher → cross-checked at gates. No envelope
schema change. No new concept.

## 16. Relationship to existing work

- [`docs/design/operator-legibility.md`](operator-legibility.md) §6 — Pillar C
  original design (derive from agent capabilities). **Superseded** by this doc's
  discovery-based, session-scoped approach.
- [`docs/reference/capability-grants.md`](../reference/capability-grants.md) — shipped
  feature doc. The plan-linked derivation becomes a fallback; the
  session-scoped discovery is the primary mechanism.
- [`docs/archived/approval-system-hardening-plan.md`](../archived/approval-system-hardening-plan.md)
  — Phase 2 (scope model) is orthogonal and composes. The envelope uses the
  same `GrantScope`, `GrantTarget`, and `expires_at` infrastructure.
- [`docs/guide/remote-access-approval.md`](../guide/remote-access-approval.md) — the static
  analysis (`remote_access.rs`) becomes the **discovery engine** for the
  envelope, not just the approval trigger.

## 17. Open questions

1. **Auto-lock threshold.** Should the gateway auto-propose a lock after N
   observed usages of the same host, or always wait for a contextual trigger?
   Auto-proposal risks fatigue; contextual-only risks the operator never being
   asked.

2. **Envelope expiry.** Should locked envelopes expire (TTL) or last until
   session end? The existing `expires_at` on grants supports TTL; the question
   is whether to default to session-end or a configurable duration.

3. **Cross-session envelope reuse.** If the operator builds the same kind of
   agent in a later session, should the gateway propose reusing a prior
   envelope? (Probably not by default — each session is a fresh trust
   decision. But the operator might want a "template" mechanism.)

4. **Envelope visibility in the TUI.** The session-room timeline should show
   the locked envelope as a first-class object (what's pre-approved, what's
   been observed but not locked). This is a Pillar D (legibility) concern.
