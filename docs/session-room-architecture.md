# Session Room — Architecture

This is the design companion to the [Session Room user guide](session-room.md).
It explains *how* the room works: the canonical timeline, the three event
streams, the channel-as-client boundary, and the two-axis actor model. The
broader design rationale lives in
[`docs/rfc/session-room-channel-agnostic-timeline.md`](rfc/session-room-channel-agnostic-timeline.md).

## The idea in one paragraph

A session is a multi-actor collaboration — a planner, specialists, the divergence
sentinel, external tools, and you. The **Session Room** presents that as one
**channel-agnostic timeline**: every actor appears as a participant, like a chat
room. The gateway owns a single **canonical timeline** and decides what matters;
**channels** (the terminal TUI today; Discord/WhatsApp tomorrow) are thin
**clients of the gateway API** that render it and relay your actions back. They
never touch the database. This keeps the hard parts — merging, importance,
attribution — in one place, fixed once for every channel.

## The three event streams

The gateway records session activity at three levels of abstraction. The room
reads the **middle** one.

| Stream | Table | Role | Granularity |
|---|---|---|---|
| Operator activity | `operator_activity` | Thin attention projection (notifications) | Coarse — only attention-worthy items |
| **Canonical timeline** | **`live_digest_events`** | **The Session Room spine** | Rich — every meaningful event, importance-ranked |
| Causal chain | `causal_events` / `causal_chain.jsonl` | Tamper-evident audit substrate | Firehose — everything, for forensics |

`live_digest_events` is deliberately the spine: it's a *digest built for live
presentation*, not the raw firehose (which would overwhelm a channel) nor the
thin notification projection (which drops the narrative). Drill-down can descend
into the causal chain; the timeline itself stays readable.

## The canonical timeline (`live_digest_events`)

A single, append-only, hash-orderable log per root session. Migration **v46**
(`session_timeline`) extended the table in place with attribution + importance
columns: `principal_kind`, `principal_id`, `role`, `altitude`, `refs_json`.

- **Writer / type:** `runtime::session_timeline::build_timeline_event` constructs
  a `LiveDigestEventRecord` with consistent `event_id` / `node_id` /
  attribution. Producers call `store.create_live_digest_event`.
- **Reader:** `GatewayStore::list_session_timeline`
  (`scheduler/gateway_store/session_timeline.rs`) — cursor-paginated on
  `(created_at, event_id)`, filtered by a minimum altitude, mapping rows to the
  shared `SessionTimelineEntry` type. Rows written before v46 (NULL attribution)
  fall back to sensible defaults (altitude → Normal, principal → the source
  agent, role derived from its id), so old sessions still render.
- **Append-only:** events are never mutated; a *resolution* is a new event
  (e.g. `approval.approved`), not an edit. This respects the constitution's
  real-time / append-only digest rule (P-8.7).

### Event types (what producers emit)

| Event | Emitted by | Default altitude |
|---|---|---|
| `session.start` | session tracer | Normal |
| `turn.start` / `turn.end` / `llm.round` / `llm.retry` | session tracer | Detail |
| `tool.requested` | session tracer | Detail |
| `tool.completed` | session tracer | Normal |
| `agent.message` | session tracer (LLM completion text) | Normal |
| `agent.reasoning` | session tracer (extended thinking) | Detail |
| `operator.message` | router (`event.ingest` chat path) | Normal |
| `user.ask.pending` | `user_ask` tool | Attention |
| `approval.pending` | human gate | Attention |
| `approval.{approved,rejected,cancelled}` | approvals store (decision chokepoint) | Normal |
| `plan.pending` / `plan.approved` | plan-frame tool | Attention / Normal |
| `divergence.intervention` | lifecycle (sentinel) | Attention |
| `workbench.{created,reconciled,discarded}` | workbench tool | Detail |
| `llm.request_failed` | session tracer | Error |

`agent.message` + `operator.message` are what make the room read as a
*conversation*: both sides' prose on the timeline, not just mechanical markers.
Free text is **redacted then hard-capped** before it's written to a row; full
content stays in the evidence store.

## Importance: the altitude model

Every event has an **altitude** — its at-a-glance importance — computed
gateway-side so it is identical for all channels.

```
Detail  <  Normal  <  Attention  <  Error
  ·          ▸          ⚠            ✗
```

```
altitude(event) = max( base_altitude(event_type), role_floor(role) )
```

- `base_altitude` maps the event type (mechanics → Detail, progress → Normal,
  gates → Attention, failures → Error).
- `role_floor` lets a **seat raise, never lower** the floor: a `Sentinel`-seat
  event is at least `Attention`, so a divergence/security intervention can't be
  buried — even a Sentinel's `agent.reasoning` surfaces. `role_floor` defaults
  live in code and are config-tunable (we don't pin tunables in the
  constitution).

Channels apply a **display floor** (`--min-altitude`, or the `a` key) as a *view*
filter — the data is unchanged. The TUI also **coalesces** runs of consecutive
`Detail` events into one collapsed row so routine plumbing doesn't flood the
view.

## Who acted: the two-axis actor model

Every event is attributed on two independent axes (`autonoetic-types`:
`principal.rs`, `session_timeline.rs`):

- **Principal — WHO** (`PrincipalKind`): `Human`, `AutonoeticAgent`, `Script`,
  or `ForeignAgent { provider }` (an external CLI/AI, e.g. `claude-code`).
- **Seat — WHICH ROLE** (`SessionRole`): `Operator`, `Planner`, `Specialist`,
  `Sentinel`, `Curator`, `Auditor`, `Tool`, `ExternalSurface`, `Runtime`.

The two axes are orthogonal on purpose. The **Operator seat is
occupant-agnostic**: a human fills it today; an AI could tomorrow. Obligations
(e.g. motivating a decision, §O) attach to the *seat's decision*, not the
occupant's kind — so accountability is symmetric by construction. The model can't
tell a human decider from an AI decider except as a display marker (`🧑`).

Attribution is derived, not invented: `derive_role(agent_id)` maps an agent id to
its seat; `decider_seat(decided_by)` maps a recorded decider string to a
`(principal, seat)`. The divergence **Sentinel** is a *participant* in the room,
not system chrome — it speaks and intervenes like any actor. The only non-actor
is the executor function itself; when the runtime must speak it does so through
the `Runtime` seat (hidable, Detail floor), so the room stays uniform.

## Channels are API clients (Separation of Powers)

The gateway is the high-privilege executor and sole owner of state. A channel is
a low-privilege **client of the gateway API** — it never reads `gateway.db`
directly. This is the same boundary the rest of the system enforces between
agents and the gateway, applied to presentation surfaces.

### Read path

- **`session.timeline.list`** JSON-RPC — the cursor-paginated timeline, floor-
  filtered. (router.rs)
- **`GET /api/session/timeline/stream/{root_session_id}`** — an SSE stream
  (cursor bootstrap + live tail) for push delivery. (server/http.rs)

### Write path (relaying your actions)

- **`approvals.approve` / `approvals.reject`** — resolve an approval gate (records
  the decider kind and an optional motivation, §O).
- **`interaction.resolve_and_answer`** — answer a `user.ask` (free text *or*
  `answer_option_id` for a pre-digested choice).
- **`event.ingest`** (`event_type: "chat"`) — send a free-form message into the
  session; the gateway also writes the `operator.message` timeline row, so both
  sides of the conversation appear.

### The render core and the `Channel` trait

`autonoetic/src/cli/room/`:

- **`render.rs`** — a **pure, channel-neutral** core: `SessionTimelineEntry` →
  one line (`summarize`, `render_line`), `coalesce`/`coalesce_indexed` (squash +
  drill-down mapping), `format_detail`, `one_line` (hard-cap flattening). No I/O.
- **`channel.rs`** — the `Channel` trait (`kind`, `format_row`, `gate_prompt`)
  over that core, plus the channel-neutral gate primitives (`GateRef`,
  `GateKind`, `GateAction`). `CliChannel` (viewer) and `TuiChannel` (interactive
  shell) are the first impls; a Discord bridge is the next.
- **`client.rs`** — `RoomClient`: newline-delimited JSON-RPC over TCP to
  `127.0.0.1:{port}`, authenticated with `AUTONOETIC_SHARED_SECRET`.
- **`tui.rs`** — the ratatui shell. Its sync event loop bridges to the async RPC
  client with `block_in_place` + `Handle::block_on` (the CLI runs on a
  multi-thread runtime).

### Binding external conversations

For off-host channels, **`operator_channel_bindings`** (migration **v48**) maps
`(channel, external_id) → root_session_id` so a Discord thread / WhatsApp chat
survives reconnects and routes replies back as `Operator`-seat events. Channels
reach it over **`channel.bind` / `channel.resolve`** — again, API, not store.

## Conversational gates

Approvals, clarifications, plan approvals, and divergence checks unify into one
primitive: an *addressed ask between participants*, resolved in-flow (not modal).

- The asking side proposes **pre-digested choices** so the operator can resolve in
  one tap; free text stays available when the question allows it. The choices ride
  on the timeline event payload, so every channel renders them without an extra
  round-trip.
- Decisions are recorded with the **decider's kind** (`decided_by_kind`, migration
  **v47**) and an optional **motivation**. Under the constitution's decider
  obligations (§O), the gateway checks *that* a reason exists by the required time
  for blocking/irreversible decisions — it never judges the reason's quality
  (Lawful Executor). Accountability is symmetric: humans owe motivations the way
  agents do.

## End-to-end data flow

```
Agent turn                              Operator action (room)
   │ session_tracer / gates / tools         │  approvals.* · interaction.* · event.ingest
   ▼                                         ▼
build_timeline_event ──► live_digest_events ◄── gateway executes (unblocks agent,
   (append-only, attributed, altitude)          records decision + new events)
              │
              ▼
   list_session_timeline  ──(JSON-RPC / SSE)──►  Channel (RoomClient)
              (cursor, floor)                        │ render.rs + Channel trait
                                                      ▼
                                              terminal TUI / Discord / …
```

A turn streams in as `turn.start → agent.reasoning → agent.message → tool.* →
turn.end`; a gate appears as `*.pending` and is closed by a new `*.approved` /
answer event. Your messages enter via `event.ingest` and appear as
`operator.message`. The channel just polls/streams the timeline and renders.

## Build phases (history)

- **P1** — canonical timeline: extend `live_digest_events` + the reader + the
  producers.
- **P2** — greenfield TUI renderer (altitude dial, squash, drill-down,
  conversational gates).
- **P3** — gateway timeline API + channel-as-client migration + the `Channel`
  trait + `operator_channel_bindings` (+ Discord bridge, in progress).
- **P4** — digest narrative enrichment (agent message/reasoning on the timeline;
  failures link their preceding action chain).
- **P5** — federation: `ForeignAgent` actors and an IDE renderer (layered on the
  external-CLI-agent delegation work, #343).
- **P6** — documentation (this doc + the user guide).

## Source map

| Concern | Location |
|---|---|
| Timeline types (`Altitude`, `SessionRole`, `SessionTimelineEntry`) | `autonoetic-types/src/session_timeline.rs` |
| Principal types | `autonoetic-types/src/principal.rs` |
| Event builder, altitude, role derivation | `autonoetic-gateway/src/runtime/session_timeline.rs` |
| Producers (turns, agent narrative, failures) | `autonoetic-gateway/src/runtime/session_tracer.rs` |
| Timeline reader | `autonoetic-gateway/src/scheduler/gateway_store/session_timeline.rs` |
| Channel bindings store | `autonoetic-gateway/src/scheduler/gateway_store/channel_bindings.rs` |
| RPC methods | `autonoetic-gateway/src/router.rs` |
| SSE stream | `autonoetic-gateway/src/server/http.rs` |
| Render core / channels / client / TUI | `autonoetic/src/cli/room/` |
| Schema migrations (v45–v48) | `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs` |
