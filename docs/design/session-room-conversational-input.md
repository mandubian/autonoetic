# Session Room — Full Conversational Input (operator messages)

Status: **proposed** (2026-06-04) · Part of the Session Room program (#363) ·
Follows interaction polish (#404).

## Problem

The Session Room (`autonoetic/src/cli/room/`) is today a **read + gate-resolve**
surface: it tails the canonical timeline and resolves approvals/clarifications,
but the operator **cannot send a free-form message** into the session. To start
or steer a conversation you still drop back to `chat.rs`. The goal is to make the
room a true interaction surface so a session can be driven entirely from it — and,
because the room is just one channel, so the same path works for Discord/WhatsApp
later.

## How operator input works today (findings)

- **Ingress is `event.ingest`** (newline JSON-RPC over TCP, `AUTONOETIC_SHARED_SECRET`)
  — the *same transport* the room's `RoomClient` already uses. `chat.rs` submits
  every operator turn as `event.ingest { event_type: "chat", message, session_id,
  target_agent_id?, metadata }` (`router.rs:583`). The handler already special-cases
  `event_type == "chat"` (e.g. rerouting to an active workflow-child session,
  `router.rs:632`).
- **Session continuity**: the first message to a fresh `session_id` spawns; reusing
  the same `session_id` continues (`is_message` flag → `can_message_agent` policy).
- **Blocking vs async**: `event.ingest` blocks on the agent turn by default;
  `async_mode: true` enqueues and returns immediately.
- **The gap**: the operator's own message is pushed into the in-memory conversation
  history (`execution.rs:3698`) and persisted to the content store, but **never
  written to `live_digest_events`**. So a timeline reader (the room) sees the
  agent's side but not the operator's. This is the foundational thing to fix.

## Design

Two slices. Slice A is gateway-side and benefits every channel; Slice B is the
room UI.

### Slice A — operator messages on the canonical timeline (gateway)

In the `event.ingest` handler, for operator-originated chat, emit a
`live_digest_events` row **before** dispatching to the agent (so the operator's
line lands in the feed ahead of the agent's response, preserving conversational
order):

- `event_type`: `operator.message`
- `principal` / `role`: derive — when there is **no `source_agent_id`** (a human
  via room/chat), attribute to **`Human` + `Operator` seat**; when an agent
  originated it, attribute to that agent. (v1 keys on `event_type == "chat"` with
  no `source_agent_id` ⇒ operator. Foreign-channel attribution via `metadata`
  sender is a follow-up.)
- `altitude`: `Normal` (visible at the default floor; it's first-class conversation,
  not plumbing).
- `payload`: `{ "message": <redacted text> }` (reuse `redact_text_for_logs`, as the
  `user.ask.pending` producer does).
- `refs`: none required for v1.

`render::summarize` gets an arm for `operator.message` (show the one-lined text;
the actor label already renders 🧑/seat). This makes the operator visible in the
room **and** in any future channel — no per-channel work.

> Why gateway-side, not in the room: the timeline is the canonical, channel-neutral
> spine (#390). Writing the operator event once in the gateway means the room,
> Discord, and `chat.rs`-as-timeline all show the operator identically. A channel
> writing its own "I sent this" row would fragment the source of truth.

### Slice B — compose & send from the room (TUI)

- A **compose** key (proposed `i` = "input", mirroring the gate-capture UX already
  in `tui.rs`) opens a text buffer in the footer; `Enter` sends, `Esc` cancels.
- Send via `RoomClient` → `event.ingest { event_type: "chat", message, session_id:
  <root_session_id>, async_mode: true, metadata: { source: "session_room" } }`.
  **Async** so the sync TUI loop never blocks on a full agent turn; the operator's
  line (Slice A) and the agent's response then stream in through normal polling.
- Status line reflects the outcome (`✓ sent` / `✗ <error>`); on a mid-turn/busy
  rejection, surface it and keep the buffer so it can be resent.
- Targeting v1: send to the room's `root_session_id` (omit `target_agent_id` →
  gateway resolves the lead). The existing chat reroute logic handles workflow
  children.

### Non-goals (v1)

- Starting a **brand-new** session from the room (attach-to-existing only; fresh
  sessions stay with `chat`/CLI).
- Rich/multi-line composer, history scrollback editing, attachments.
- Per-occupant attribution for non-human channel senders (Slice A keys on the
  operator case; richer attribution is a follow-up when the Discord bridge lands).

## Open questions

1. **Mid-turn sends** — does `async_mode: true` enqueue or reject while the agent
   is mid-turn? Confirm and pick the UX (queue silently vs. "agent busy, resend").
2. **Event-type name** — `operator.message` (Operator-seat framing) vs. a neutral
   `chat.message`. Leaning `operator.message` to match the two-axis model (seat =
   Operator, principal = Human *or* AI).
3. **Echo timing** — emit the operator event synchronously in the handler before
   enqueueing the turn (chosen), so order is deterministic even under async.

## Test plan

- Gateway: `event.ingest { event_type: "chat" }` writes one `operator.message`
  timeline row attributed to Human/Operator; agent-originated ingest does not
  mis-attribute. Integration test over the router (shared-env pattern).
- Render: `operator.message` renders one-lined with the 🧑 operator label.
- Room TUI: compose→send issues an async `event.ingest`; unit-test the param
  shape and the busy/again handling (no live agent required).
- Live smoke: drive a real session end-to-end from the room.
