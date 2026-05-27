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

## Wakeup Mechanism

When an agent issues a message:
1. The message and its corresponding delivery records are inserted into SQLite.
2. The Gateway signals the target session(s) via the existing JSON-RPC signaling loop (`event.ingest` with an `agent_message` payload).
3. If the target session is sleeping or waiting on a background timer, the receipt of the `event.ingest` message will act as an immediate wakeup signal.

## Auto-Injection

Instead of forcing the recipient agent to explicitly pull messages via an inbox tool, the system will feature **auto-injection**:
At the beginning of each execution turn (`execute_session_turn` within `AgentExecutor`), the Gateway queries `agent_message_deliveries` where `target_session_id = <current_session>` and `delivered_at IS NULL`.
Any undelivered messages are injected directly into the LLM system/user context as synthetic events (e.g., `[Async Message from Agent X]: "..."`) and marked as delivered.

## Tool Implementation
The `AgentMessageTool` (`agent_message`) will be officially implemented in the native Registry, accepting `target_session_id` and `message_payload`. The tool enforces the existing `PolicyEngine::can_message_agent()` rules before allowing dispatch.
