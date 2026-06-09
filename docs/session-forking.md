# Session Forking

This document describes how a session is forked from a past turn so an operator
or agent can **backtrack to an earlier state and try a different approach**.

## Overview

Forking creates a new, independent session that starts from the conversation
state of a source session **as of a chosen turn**. It serves two purposes:

- **A test/debug tool** — backtrack to a decision point and explore an
  alternative path without disturbing the original session.
- **The substrate for counterfactual self-evolution** — an agent can fork at
  turn *N*, run "what if I had done X instead", and compare outcomes via the
  causal chain.

Every fork is a fully runnable session: sending it a message resumes execution
from the fork point with the full branch-point context.

## What a fork is built from

A fork is derived from a **checkpoint**. Checkpoints are universal session
snapshots written at every *yield point* (hibernation, approval, budget
exhaustion, user input, escalation, emergency stop). Each captures the full LLM
conversation history, the turn counter, loop-guard state, budgets, and
reproducibility metadata, and is stored at:

```
.gateway/checkpoints/<session_id>/<turn_id>.checkpoint.json
```

Turn ids are zero-padded to a fixed width — `turn-000003` — via the single
shared helper `checkpoint::turn_id_for(turn)`. Everything that needs to address
a checkpoint by turn number (the lifecycle that writes them, `trace fork`, the
`session.fork` RPC, the timeline mirror) uses this helper so the format cannot
drift.

### Yield-point granularity

Checkpoints exist **only at yield points**, not at every turn. You can therefore
fork only at a turn that has a checkpoint. A request for a turn with no
checkpoint is rejected with an error that lists the forkable turns. This is a
deliberate efficiency choice — checkpointing every turn would multiply storage
and I/O for little benefit.

## How a fork is made runnable

`SessionFork::fork_from_checkpoint` does three things:

1. Clones the source checkpoint's history (appending the optional branch
   message) and writes it to the content store under the name `session_history`.
   That blob is content-addressed (SHA-256), so identical history dedupes
   automatically. This blob feeds the **UI** (chat/trace history hydration).
2. **Writes a `Hibernation` checkpoint under the new session id**, with any
   inherited approval/pending-tool state stripped. This is what makes the fork
   runnable: the execution engine seeds a session's LLM context from its latest
   checkpoint — *not* from the `session_history` content name — so without this
   step the branch would resume from a blank history and the fork point would be
   lost. Because the yield reason is `Hibernation` (a normal, auto-resumable
   point), the next message to the fork resumes cleanly with the full
   branch-point context.
3. Mirrors the source timeline into the fork (see below).

The forked session gets a new, independent `root_session_id` (e.g.
`fork-ab12cd34`). It does **not** inherit the parent's root id — each branch is a
self-contained root. (A lineage/children view linking a fork back to its parent
would be a future enhancement.)

## Timeline mirroring

The Session Room renders a session's timeline from the `live_digest_events`
table, filtered strictly by `root_session_id`. A fork writes a checkpoint but no
`live_digest_events` of its own, so without further action the room would show
an **empty timeline** until the branch first runs and emits events.

To avoid that, `GatewayStore::clone_timeline_for_fork` copies the source
session's timeline rows into the fork's `root_session_id`, **up to and including
the fork turn**, with fresh `event_id`s and preserved timestamps/attribution.
The cutoff is the end-of-fork-turn timestamp (the latest event whose `turn_id`
is `<= turn-00000N`), which also pulls in interleaved turn-less rows (operator
messages, session start) that belong before the branch point.

### Design choice: copy rather than reuse-by-reference

The timeline is **copied** into new rows in the existing `live_digest_events`
table rather than reused in place from the parent. No new table is introduced,
and the LLM history itself is reused (content-addressed); only the timeline rows
are duplicated.

The considered alternative was *reuse-by-reference*: store a
`(parent_root_session_id, fork_turn)` lineage pointer and have the timeline
reader UNION the parent's rows (`turn <= fork_turn`) with the fork's own rows,
never copying. We chose **copy** deliberately:

| | Copy (chosen) | Reuse-by-reference |
|---|---|---|
| Storage | duplicates timeline rows per fork | zero duplication |
| Timeline read path | unchanged, simple | UNION + lineage lookup, hotter path |
| Fork-of-fork | just works | needs a recursive lineage walk |
| Cursor pagination | trivial (one root) | spans multiple roots, fiddly ordering |
| Parent pruned later (retention) | **fork unaffected, self-contained** | fork loses its prefix |

The decisive factors were keeping the *hottest* read path (timeline render)
simple and making a fork **durable and self-contained** — a branch must not lose
its inherited history when the parent's `live_digest_events` are pruned by the
configurable retention policy. The cost is bounded row duplication (timeline
rows are small and capped at the fork turn). If duplication ever becomes a
concern, reuse-by-reference remains a viable evolution.

## Surfaces

Forking is exposed three ways, all backed by the same mechanism:

### CLI

```bash
autonoetic trace fork <session_id>                       # fork from the latest checkpoint
autonoetic trace fork <session_id> --at-turn 5           # fork from a specific turn
autonoetic trace fork <session_id> --message "try B"     # append a branch message
autonoetic trace fork <session_id> --at-turn 5 --interactive
```

### JSON-RPC

`session.fork` parameters: `source_session_id` (required), optional
`at_turn`, `branch_message`, `new_session_id`, `target_agent_id`. Returns
`new_session_id`, `fork_turn`, `history_handle`, `message_count`, and
`mirrored_events` (rows copied by the timeline mirror).

### Session Room

- `/fork [--at-turn N] [message]` — fork the current session and switch the room
  to the new branch.
- `F` on a selected timeline row — fork from that row's turn and switch to it.

After forking, the room switches to the branch and shows its mirrored history;
send a message to continue the branch from the fork point.

## Related

- `docs/cli-reference.md` — full `autonoetic trace fork` reference
- `docs/content-store.md` — content-addressed `session_history` storage
- `docs/session-room.md` / `docs/session-room-architecture.md` — the timeline UI
- Checkpoints and the causal chain are described in `docs/ARCHITECTURE.md`
