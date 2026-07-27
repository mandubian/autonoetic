# Agent-to-Agent Messaging

`agent_message` sends a direct, asynchronous message to another agent's session.
It is the non-hierarchical counterpart to `agent_spawn`: no child session, no
parent/child relationship, no task to wait on.

Requires the `AgentMessage` capability. Workflow tier.

## When to use it instead of `agent_spawn`

| Use | Tool |
|---|---|
| Hand off a unit of work you lack the capability to do, and get a result back | `agent_spawn` |
| Tell a peer something it needs to know while it keeps doing its own work | `agent_message` |

Reach for `agent_message` for progress updates, findings, divergence reports, and
status signals — anything that does not need a synchronous reply. If you need a
result, spawn; a message will not give you one.

## Addressing

Pass `target_session_id`, `target_agent_id`, or both. At least one is required.
**`target_session_id` wins when both are present.**

- **`target_session_id`** — one specific session.
- **`target_agent_id`** — broadcast to every *unfinished* session of that role.
  Your own session is never a recipient, so broadcasting to your own role
  reaches peers only.

Your `AgentMessage` `patterns` are checked against the **receiving agent** in
both modes. With `target_session_id` the gateway resolves that session's bound
agent and checks it, so naming a session cannot widen a scoped grant.

## Always check the result

A message can fail without raising an error. Treat it as sent **only** when:

```
ok == true  &&  status == "delivered"  &&  recipients_count > 0
```

Every other status delivered nothing (`recipients_count == 0`):

| `status` | What it means | What to do |
|---|---|---|
| `no_live_recipients` | The role is installed but has no unfinished session other than yours. | Nobody is listening. `agent_spawn` if the work must happen. |
| `target_session_finished` | That session has already ended. Messages are injected when a session wakes, and it will not wake again. | Don't retry. `agent_spawn` if the work still needs doing. |
| `target_agent_not_found` | No agent with that id is installed (checked both the alias registry and the agents directory). | Check the id with `agent_list`. Do not retry. |
| `target_agent_unavailable` | The agent is installed but its manifest could not be loaded — a broken bundle, not a missing one. | Report it. Retrying will not fix a broken bundle. |
| `target_session_not_found` | That session id has no agent binding, so the gateway cannot tell who owns it. | Use a session id from your own workflow, or address the role instead. |

**A child you spawned is usually already finished.** A short child session can end
seconds after it replies, so a session id you captured earlier is very often dead
by the time you message it. Messaging is for peers that are *still running*, not
for following up with a child. If you need more work done, spawn again.

`recipients_count` counts sessions that can actually consume the message, not
sessions the role has ever run. A count of 0 with `ok: false` is a real
delivery failure — do not report success.

## Receiving a message

Messages arrive at the start of your turn as user text:

```
[Direct Message from Agent 'coder.default' (Session: root-1/child-3)]
Tests pass but coverage dropped on the auth module.
```

That block **is** the message. A preceding `[Gateway] Wake-up: direct message
...` line is only the notice that woke you and never repeats the content — read
the block, not the notice.

Process incoming messages and correlate them with your own goals or workflow
state. Do not ignore them: they carry progress reports, divergence findings, and
status updates you are expected to act on. To respond, send an `agent_message`
back to the sender's `agent_id` or `session_id`.

## Delivery timing

Delivery is **per wake, not per turn**. A message sent while you are mid-task is
queued and injected the next time your session wakes. So:

- A peer will not see your message the instant you send it.
- `status: "delivered"` means *queued for a session that can receive it*, not
  *already read*.

There is no reply correlation: a response is a new message with no link back to
the one it answers. If an exchange needs to be traceable, say what you are
replying to in the message text.

## Who is actually reachable

Only a session that still exists. Two kinds qualify:

1. **Resident sessions.** An agent whose SKILL.md sets
   `agent.resident_idle_ttl_secs` does not die when its task finishes — it parks
   and stays addressable until that many seconds pass without traffic. This is
   what makes peer messaging work: spawn the peer once, then message it.
2. **Sessions still executing.**

Everything else is unreachable. An ordinary specialist ends with its task, so a
session id you captured earlier is usually already dead — you will get
`target_session_finished`. There is no mailbox: messaging a role with nothing
running returns `no_live_recipients` and stores nothing for later.

If you need a peer you can talk to more than once, it must be a resident agent.
If the work simply needs doing and you do not care who is listening, use
`agent_spawn`.

## Operator visibility

Every delivered message appears on the receiving session's timeline as
`agent.peer_message`, attributed to the sending agent, at Normal altitude. Peer
coordination is visible to the operator by default — write messages that make
sense to a human reading over your shoulder.

## Related pages

- `workflow-orchestration` — spawning children, waiting, and the delegation ladder
- `agent-capabilities` — declaring `AgentMessage` and how capabilities gate tools
- `tool-reference` — one-line summary of every tool
