# Design: Operator Live File Comments

Status: **Proposed**
Scope: file-anchored, **next-turn**, **comment-only** (no operator editing).
Related: extends Phases 5–6 of `docs/design/human-agent-artifact-collaboration-plan.md`
(`workbench.ask` / `workbench.open`); builds on the t=0 content tree pane (PR #496).

## Context

The Session Room already lets an operator **watch** what an agent is producing and
**talk** to the session — but the two are disconnected:

- **Read.** The content tree pane (`autonoetic/src/cli/room/tui.rs`, keys `c`/`o`) shows
  the live session drafts an agent wrote via `content_write`/`content_patch`, backed by
  the `content.list` / `content.read` RPCs (`autonoetic-gateway/src/router.rs`). It is
  read-only and on-demand.
- **Talk.** The operator composes a freeform message (`i`) that is posted via
  `event.ingest` (`async_mode: true`) and delivered to the agent at the **next turn
  boundary** — attributed Human/Operator on the canonical timeline
  (`operator_message_event`, `runtime/session_timeline.rs`).

What's missing is the link between them: when an operator reads a draft and spots a
problem ("you hardcoded a secret on line 12 of `config.yaml`"), there is no way to
attach that remark to **the file (and version) they were looking at** and have the agent
reliably see it in context. Today they must hand-type the filename into a freeform
message, and the comment is not anchored to a content version, so it silently rots as
the agent rewrites the file.

This design adds **operator comments anchored to a live content file**, delivered to the
owning agent at its next turn. It deliberately stays **comment-only**: the operator
*proposes* an observation; the agent decides what to do. Operators who want to **edit**
already have the workbench projection + reconcile flow (`artifact_project` →
`runtime/workbenches/*/source/` → `workbench_reconcile`); this feature is the
lighter-weight oversight loop, not a co-editing surface.

## Goals

- Let an operator attach a comment to a **named content file** in a live session, with an
  **optional line hint**.
- Deliver the comment to the **owning agent at its next turn**, framed with the file name,
  the **content version** the operator was viewing, and the line hint.
- Record the comment as a first-class **timeline event** (`operator.comment`) with proper
  principal/role attribution, so every channel (room, Discord, future) sees it.
- Preserve **Separation of Powers**: a comment is a *proposal/observation*, never a
  mutation of agent state.

## Non-goals (this iteration)

- **No operator editing of live drafts.** (Use the existing workbench flow.)
- **No mid-turn interruption.** Comments are queued and delivered at the next turn
  boundary, exactly like `event.ingest` chat today. (Mid-turn steering is a separate,
  larger change — turn cancellation/resumption.)
- **No line re-mapping across versions.** The line hint is best-effort and may drift; we
  surface drift rather than silently re-anchor.
- **No VS Code extension / FUSE mount.** A read view in a real editor (Tier 1 below) is a
  complementary, separable enhancement; this doc is the comment loop (Tier 2).

## UX flow

1. Operator opens the content tree (`c`), selects a file, opens it (`o`) in the viewer
   (`ContentView`, today `{ name, content, scroll }`; this design adds the resolved
   `handle`, which `content.read` already returns — see the TUI section).
2. Operator presses **`m`** (comment) → a compose panel opens (reusing `ComposeInput`).
3. Optional: operator types a line hint (e.g. `12` or `12-14`) in a small field; default
   is whole-file.
4. Operator writes the comment body and submits.
5. The TUI calls `content.comment`. A status line confirms `✓ commented`.
6. The comment appears on the timeline immediately (`operator.comment`, Attention
   altitude) and is delivered to the owning agent at its next turn.

## Data model

A comment is an event, not stored state. Anchor identity reuses the content store's
existing keys (`name` + content `handle`, the SHA-256 the operator was viewing).

```jsonc
// operator.comment timeline payload (and the next-turn delivery framing)
{
  "comment_id": "cmt_<short>",
  "name": "config.yaml",            // session content name
  "handle": "sha256:abc…",          // the VERSION the operator commented on (anchor)
  "current_handle": "sha256:def…",  // the name's current version at comment time
  "drifted": true,                  // current_handle != handle → file already moved on
  "line_start": 12,                 // optional, 1-based, best-effort
  "line_end": 14,                   // optional
  "body": "you hardcoded a secret here"
}
```

- **Anchor = (`name`, `handle`).** The `handle` is captured from the `content.read`
  response the operator was viewing, so the comment binds to a concrete immutable version
  even though the *name* is a moving pointer.
- **Drift.** Drift is computed **once, at comment time**: the handler resolves `name` →
  `current_handle` and sets `drifted = (handle != current_handle)`. Both handles and the
  flag are stored in the single `operator.comment` event and reused verbatim in the
  next-turn framing — there is no second, delivery-time event. (Because comment and
  delivery are effectively back-to-back in a live session, comment-time drift is the
  relevant signal; a file that changes again between emit and the agent's turn is the
  agent's own write, which it already knows about.) We do **not** re-map line numbers.

## Gateway: new RPC `content.comment`

A dedicated method (not an overloaded `event.ingest`) so the anchor schema is validated
and the comment is unambiguously typed.

```jsonc
// request
{
  "session_id": "<root or child session id>",
  "name": "config.yaml",
  "handle": "sha256:abc…",     // optional; the version being viewed. Omitted → anchor to current.
  "line_start": 12,            // optional
  "line_end": 14,              // optional
  "body": "you hardcoded a secret here",
  "commented_by": "operator"   // optional, default "operator"
}
// response
{ "ok": true, "comment_id": "cmt_…", "name": "config.yaml",
  "handle": "sha256:abc…", "drifted": false }
```

Handler behavior (mirrors `content.read` resolution + `event.ingest` delivery):

1. Open `ContentStore`; resolve `name` → `current_handle`
   (`resolve_name_or_handle_to_handle`). If the name doesn't resolve, return a JSON-RPC
   server error (`-32000`, `content.comment resolve failed: …`) — same not-found
   convention as `content.read`.
2. Anchor `handle` = the provided handle, else `current_handle`.
   `drifted = (handle != current_handle)`.
3. Validate `body` non-empty; `line_end >= line_start` when both present.
4. Emit an `operator.comment` timeline event (see below).
5. **Enqueue next-turn delivery** to the agent that owns `session_id`, reusing the
   existing async `event.ingest` path: synthesize a framed operator turn (the agent
   receives it in conversation history at its next `spawn_agent_once`).
6. Return the ack.

Delivery framing handed to the agent (next turn):

```
Operator comment on file `config.yaml` (version sha256:abc… , lines 12–14):
> you hardcoded a secret here
[note: you have rewritten this file since (now sha256:def…); re-read the current
 version before acting on the line numbers.]   ← only when drifted
```

## Timeline event: `operator.comment`

A new event type alongside `operator.message`
(`autonoetic-gateway/src/runtime/session_timeline.rs`):

- **Attribution:** `principal = Principal::human(commented_by)` (default "operator"),
  `role = SessionRole::Operator` — same model as `operator_message_event`.
- **Altitude:** base **`Attention`** (above the `Normal` default). An anchored comment is
  the operator flagging something they want addressed, so it ranks with operator gates
  rather than ordinary narrative — it stays visible even in stricter, attention-and-above
  views, and sorts as something the agent must act on. (A future `severity` field could
  modulate this; out of scope here.)
- **Payload:** the data-model object above (redacted body, like `operator.message`).
- Written to `live_digest_events` only (presentation timeline), consistent with how
  operator messages are handled today; the immutable content versions referenced by the
  comment are already in the evidence/content store.

## Agent uptake (guidance)

Plumbing delivers the comment; a **guidance block** makes the agent treat it as an
operator observation it must engage with — reusing the guidance-block mechanism
(`docs/internals/prompt/composition.md`). Doctrine, roughly:

> An operator comment anchored to a file is a high-signal observation. Acknowledge it,
> then either address it (and say how) or explain why you are not. If the comment is
> marked drifted, re-read the current file version before responding.

No new enforcement — this is presentation/guidance, consistent with the prose-first,
agent-decides philosophy.

## TUI changes (`autonoetic/src/cli/room/tui.rs`)

- `ContentView` must retain the **`handle`** from the `content.read` response (today it
  keeps `{ name, content, scroll }`). Add `handle` so the comment can anchor to the exact
  version viewed.
- In `ContentView`, bind **`m`** → open `ComposeInput` for the comment body, plus a small
  optional line-hint field; submit calls a new `send_comment()` (sibling of
  `send_message()`) that issues `content.comment`.
- Status line: `✓ commented` / `✗ <error>`.

## Anchor drift — the one real subtlety

The file version moves under the comment between *compose* and *delivery* (the agent may
have written a newer version in the meantime). We handle this deterministically:

- The comment **binds to the handle viewed**, so it is never silently mis-attributed.
- At delivery we compare to the **current** handle and, on mismatch, mark `drifted` and
  hand the agent both handles + an instruction to re-read.
- Line numbers are explicitly **best-effort**; on drift the agent is told not to trust
  them. This avoids a fragile line-remapping engine for v1.

## Tier 1 (complementary, separable): live read in a real editor

The original motivation included opening live files in an editor like VS Code. That is a
**read** concern and is orthogonal to comments: a `content.project --live` (or a room
action) that materializes the current session drafts into a real directory
(`runtime/sessions/<id>/live/`) the operator opens with `code <dir>`, refreshed on
change. It reuses the content store and the existing workbench-projection pattern. Tracked
as a separate, optional issue; **not required** for the comment loop, which works fully
from the TUI viewer.

## Phasing

1. **Gateway core** — `content.comment` RPC + `operator.comment` timeline event +
   next-turn delivery via the existing async path.
2. **Agent uptake** — guidance block + anchor-drift framing; tests that a delivered
   comment lands in the agent's next-turn context.
3. **TUI** — `ContentView` retains `handle`; `m` to compose; `send_comment()`.
4. **(Optional) Tier 1** — live read projection to an external editor.

## Test plan

- `content.comment` resolves `name`, anchors to viewed `handle`, computes `drifted`,
  rejects empty body / inverted line range, and errors (`-32000`) on an unknown name.
- An `operator.comment` row appears on the timeline at `Attention` with Operator
  attribution.
- The comment is delivered into the owning agent's next-turn context, with the drift note
  present iff the file changed.
- TUI round-trip: open file → `m` → submit → `✓ commented`, comment visible on timeline.

## Open questions

1. **Altitude:** fixed `Attention`, or a `severity` knob (note/warn) chosen by the
   operator? (Proposed: fixed `Attention` for v1.)
2. **Targeting in multi-agent sessions:** deliver to the agent that *wrote* the file
   (by content provenance) vs. the session's active agent? (Proposed: the active/owning
   agent of `session_id`, matching `event.ingest`; revisit if provenance routing is
   needed.)
3. **Acknowledgement surface:** should the agent's response to a comment be a typed
   `comment.resolved`-style timeline event, or just prose on the timeline? (Proposed:
   prose for v1; type it later if threads are needed.)
