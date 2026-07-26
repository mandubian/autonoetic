# Direct Agent-to-Agent Messaging

## Objective
To enable direct, asynchronous peer-to-peer (P2P) messaging between distinct running agent sessions, eliminating the restriction of strictly parent-child delegation.

## Delegation vs. Messaging

It is important to understand when to use `agent_message` compared to traditional task delegation (`agent_spawn` and `workflow` tools).

**1. Delegation (`agent_spawn`)**
- **Hierarchical:** Always establishes a parent-child relationship.
- **Task-oriented:** Used to completely hand off a defined unit of work because the current agent lacks the explicit capabilities to perform it (e.g., Planner delegating to Coder to execute code).
- **Structured workflow context:** The caller is resumed by gateway-owned child-state wake-up when the child changes state, and may still use `workflow_wait` or `workflow_state` for explicit inspection and artifact lookup.

**2. Messaging (`agent_message`)**
- **Peer-to-Peer:** Non-hierarchical. Messages can be sent across unrelated active sessions.
- **Signal-oriented:** Used for side-channel coordination, pinging, broadcasting state updates, or nudging an already-running session.
- **Asynchronous context:** The message is sent asynchronously and is purely injected into the target's conversation history. The sender doesn't formally wait for a structured checkpoint response, making it ideal for event-driven decoupled systems.

## Data Structures

To support both immediate 1-to-1 messaging and foresee future 1-to-N (multicast/broadcast) use cases, the messaging system is split into two tables:

1. **`agent_messages`**: Stores the canonical message payload.
2. **`agent_message_deliveries`**: Acts as an inbox for each target session, allowing a single message to span multiple recipient sessions.

```sql
CREATE TABLE IF NOT EXISTS agent_messages (
    message_id TEXT PRIMARY KEY,
    sender_session_id TEXT NOT NULL,
    sender_agent_id TEXT NOT NULL,
    target_pattern TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS agent_message_deliveries (
    message_id TEXT NOT NULL,
    target_session_id TEXT NOT NULL,
    delivered_at TEXT,
    PRIMARY KEY (message_id, target_session_id)
);
```

## Addressing and Recipient Resolution

`agent_message` takes `target_session_id`, `target_agent_id`, or both; at least
one is required. `target_session_id` wins for delivery when both are present.

- **`target_session_id`** — one session. The gateway resolves that session's
  bound agent (`session_agent_bindings`) and applies the ACL to it, so a session
  id cannot widen a scoped grant. A session id with no binding is refused
  (`target_session_not_found`) rather than delivered to unchecked.
- **`target_agent_id`** — broadcast to that role's **unfinished** sessions,
  excluding the sender's own session.

"Unfinished" is `GatewayStore::list_unfinished_sessions_for_agent`: bindings with
no `session_outcomes` row. `session_agent_bindings` is append-only — one row per
session ever bound, never deleted — so it is a historical index, not a liveness
index. A `session_outcomes` row is written unconditionally at session close, so
its **presence** marks a session as closed and its absence means the session has
not finished. Residual case: a session killed without a clean close leaves no
outcome row and stays listed; messaging it queues a delivery nothing consumes,
the same observable failure as messaging a hung session.

Result contract — treat only `ok == true && status == "delivered" &&
recipients_count > 0` as sent. Every other status leaves `recipients_count == 0`
and queues nothing:

| `status` | Meaning |
|---|---|
| `no_live_recipients` | The role is installed but has no unfinished session other than the sender's. |
| `target_agent_not_found` | No agent with that id is installed. |
| `target_agent_unavailable` | The agent is installed but its manifest could not be loaded — a broken bundle, not a missing one. |
| `target_session_not_found` | The session id has no `session_agent_bindings` row, so the gateway cannot tell which agent owns it and will not deliver unchecked. |

`exists` distinguishes the two `target_agent_*` cases (`false` for not-found,
`true` for unavailable) and is absent when no `GatewayConfig` was supplied.

## Wakeup Mechanism

When an agent issues a message:
1. The message and its per-recipient delivery rows are inserted into SQLite, and
   a pending `AgentMessage` notification is recorded per recipient.
2. The scheduler's notification pump (`process_pending_notifications`) delivers
   each notification as `event.ingest` against the target session, which wakes
   it.
3. That ingest carries a **wake notice only** — it deliberately does not repeat
   the message body, because the body is delivered by auto-injection (below).
   The notice declares `metadata.signal_type = "agent_message"`, which lets the
   ingress pass its text through verbatim instead of wrapping it in the
   `Gateway event type: … / Message: … / Metadata: …` envelope.

Carrying the body in *both* places is what made every peer message arrive twice,
in two different formats, only one of which matched the documented block below.

## Auto-Injection

Rather than making the recipient pull from an inbox tool, delivery is
**auto-injected**. At each **wake** — the start of a session run, before the turn
loop, in `AgentExecutor::execute_with_history` — the gateway queries
`agent_message_deliveries` where `target_session_id = <current_session>` and
`delivered_at IS NULL`. Each undelivered message is appended to history as user
text:

```
[Direct Message from Agent '<sender_agent_id>' (Session: <sender_session_id>)]
<message>
```

and then marked delivered, with an `agent_message`/`received` causal event.

Note this is per **wake**, not per turn within a run: a message that arrives
while the receiver is mid-loop is queued and injected when it next wakes. The
pump's `event.ingest` is what causes that wake in the normal case.

## Tool Implementation

`AgentMessageTool` (`agent_message`) is registered in the native tool registry
and gated on the `AgentMessage` capability. It enforces
`PolicyEngine::can_message_agent()` (P-11.5) against the receiving agent in both
addressing modes.

Tier: **Workflow** (`config/tools.yaml`). This matters — as `Specialized` (the
registry default for unlisted tools) the tool is filtered out of the advertised
tool list for every child session and every un-escalated root session, including
agents that declare the capability, and its prompt guidance is dropped with it.
`child_tool_tier_filter_for_manifest` already grants the Workflow tier to
manifests declaring `AgentMessage`, so Workflow is the tier that matches.
Regression guard: `agent_message_is_workflow_tier_so_declaring_agents_can_see_it`
in `runtime::prompt_budget`.

## Operator Surface

Delivery emits an `agent.peer_message` row on the **receiving** session's
canonical timeline (`peer_message_event`), attributed to the **sender** —
sender acted, receiver was affected. Payload carries `message_id`,
`sender_agent_id`, `sender_session_id`, and the redacted body.

Altitude derives to **Normal**, the same floor as `operator.message`, so peer
traffic is visible in the room without lowering the altitude filter. A sender on
a raised seat (e.g. Sentinel) lifts it further via `role_floor`. Without this
row, an injected peer message reads as anonymous user text — indistinguishable
from something the operator typed.

The wake-notice ingest emits no timeline row of its own: it carries no body, so a
row there would either duplicate `agent.peer_message` or sit beside it as a
bodyless placeholder.

The `agent_message`/`received` causal event remains the audit record; the
timeline row is the operator-facing view.
