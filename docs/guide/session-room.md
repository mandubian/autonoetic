# The Session Room

> Looking for *how it works* rather than how to use it? See the
> [Session Room architecture](../internals/session/room.md).

The **Session Room** is a live, channel-agnostic view of everything happening in
a session — every actor (the planner, specialists, the divergence sentinel, you)
appears in one timeline, like a chat room. From it you can **watch** a session
unfold, **resolve** approvals and clarifications, and **send messages** into the
session.

It is a thin **client of the gateway API** — it never touches the database
directly. The same timeline and the same actions are what future channels
(Discord, WhatsApp, …) will surface; the terminal UI is just the first one.

> The Session Room is the successor to `autonoetic chat`. `chat` still works; the
> room adds a richer, importance-ranked timeline and a uniform multi-actor view.

## Prerequisites

The room talks to a running gateway over JSON-RPC, so:

1. **Start the gateway** (in another terminal):
   ```bash
   cargo run -p autonoetic -- gateway start
   ```
2. **Set the ingress secret** — the same value the gateway uses:
   ```bash
   export AUTONOETIC_SHARED_SECRET="<your-secret>"
   ```
   If it's unset, the room exits with a message telling you so.

You also need a **root session id** to view. Any session you've started (e.g. via
`autonoetic chat` or `autonoetic run`) has one; `autonoetic trace list` shows
recent sessions.

## Command

```bash
autonoetic room <ROOT_SESSION_ID> [OPTIONS]
```

| Option | Default | Meaning |
|---|---|---|
| `<ROOT_SESSION_ID>` | — | The session whose timeline to show (required). |
| `--min-altitude <LEVEL>` | `normal` | Lowest importance to show: `detail` \| `normal` \| `attention` \| `error`. |
| `--follow` | off | Non-interactive: tail the timeline live (like `tail -f`) until `Ctrl+C`. |
| `--tui` | off | Launch the **interactive** full-screen shell (scroll, drill-down, resolve gates, send messages). |
| `--limit <N>` | `200` | Max rows fetched per read. |

Global flags also apply: `--config <path>`, `--log-level <level>`,
`--non-interactive`.

### Three ways to run it

```bash
# 1. One-shot snapshot — print the timeline at/above the floor and exit.
autonoetic room session-abc123

# 2. Live tail — keep printing new activity until Ctrl+C (read-only).
autonoetic room session-abc123 --follow

# 3. Interactive shell — scroll, inspect, approve/answer, and chat.
autonoetic room session-abc123 --tui
```

To see everything, including routine plumbing (turn starts, tool requests):

```bash
autonoetic room session-abc123 --tui --min-altitude detail
```

## Reading the timeline

Each line is one event: `‹glyph› [‹actor›] ‹summary›`.

**Importance glyphs** (the altitude of the event):

| Glyph | Altitude | Examples |
|---|---|---|
| `·` | detail | turn start/end, LLM rounds, tool requests, workbench bookkeeping |
| `▸` | normal | tool completed, session start, **your messages** |
| `⚠` | attention | approval requested, a clarification (`user.ask`), plan proposed, a sentinel divergence, an operator file comment |
| `✗` | error | LLM/tool failures |

The `--min-altitude` floor (and the `a` key in the TUI) hides anything below the
chosen level. Long runs of routine `·` events are **collapsed** into a single
`⟨N routine events …⟩` row (toggle with `s`).

**Actor labels** show who acted, by seat, with a marker when the occupant isn't a
normal autonoetic agent:

- `🧑 operator` — you (a human).
- `🌐 coder·claude-code` — a foreign agent (e.g. an external CLI tool).
- `sentinel`, `planner`, `coder`, … — autonoetic agents, by their seat.

## Interactive shell (`--tui`) keys

### Navigation & view
| Key | Action |
|---|---|
| `j` / `↓` | Move selection down (stops auto-follow) |
| `k` / `↑` | Move selection up |
| `g` / `Home` | Jump to the top |
| `G` / `End` | Jump to the bottom and resume following newest |
| `a` | Cycle the altitude floor (detail → normal → attention → error → …) |
| `s` | Toggle squashing of routine `·` runs |
| `Enter` | Drill into the selected row — show its full detail (metadata, refs, payload). `Enter`/`Esc` closes it. |
| `q` / `Ctrl+C` | Quit (press twice within 3s) |
| `Esc` | Close the detail pane / content view / search if open — **never** arms destructive actions |
| `p` | **Pause** the session: parks the running turn at the next tool boundary (cooperative — the checkpoint is saved, no work is lost). Press `p` again to cancel a not-yet-applied pause; once parked, send a message (`i`) to resume. |
| `X` | Emergency stop — press **twice** within 2s (or `/estop`). Hard abort: kills the turn and sandbox children; the session must be forked to continue. Esc never triggers this. |

### Acting on the session
| Key | Available when | Action |
|---|---|---|
| `y` | an **approval** is selected (`⚠ approval requested`) | Approve — opens a one-line **motivation** prompt |
| `n` | an **approval** is selected | Reject — opens a one-line motivation prompt |
| `r` | a **clarification** is selected (`⚠ asks: …`) | Reply to the question |
| `i` | any time | **Compose a message** to send into the session |

### Watching & commenting on live content
The agent's in-progress files (drafts written via `content_write`/`content_patch`)
are visible from the first turn — before any artifact is built.

| Key | Available when | Action |
|---|---|---|
| `c` | any time | Toggle the **live content tree** — every draft name the session has produced |
| `o` | the content tree is open | **Open** the selected draft in the in-room read-only viewer |
| `O` | the content pane is up | **Open in your editor** — project the live drafts to a real folder and launch it (see [Reading live code in your editor](#reading-live-code-in-your-editor)) |
| `m` | a draft is open in the viewer | **Comment** on this file (see [Commenting on live files](#commenting-on-live-files)) |

### Input prompts (approve / reject / reply / message)
When a prompt is open at the bottom of the screen:

| Key | Action |
|---|---|
| *(type)* | Edit the text buffer |
| `Backspace` | Delete a character |
| `Enter` | Submit |
| `Esc` | Cancel (closes the prompt, sends nothing) |

For a **clarification with pre-digested choices**, the prompt lists them as
`[1] … · [2] …`:

- Type the **number** of a choice and press `Enter` to pick it (works for any
  count, e.g. `12`).
- For a pure-choice question (no free text) with ≤ 9 options, a single number key
  selects instantly.
- If the question allows free text, just type your answer instead.

**Motivation** on approvals is optional unless the constitution's decider
obligations require it for that decision; the gateway records who decided and why.

## Sending messages

Press `i`, type your message, and `Enter`. The message is sent into the session
over the gateway (the same path `chat` uses), and **your line appears in the
timeline** as `▸ [🧑 operator] …`, followed by the agent's response as it streams
in. Sending is asynchronous — the UI never freezes while the agent works.

> v1 attaches to an **existing** session. Start a brand-new session with
> `autonoetic chat` or `autonoetic run`, then open the room on it.

## Reading live code in your editor

The in-room viewer (`o`) is markdown-oriented and fine for prose, but for code or
multi-file drafts you'll want a real editor. Press **`O`** (from the content tree
or a draft) to **project the session's current drafts to a real folder** and open
it:

- The gateway writes the live drafts to `…/runtime/sessions/<id>/live/` — a
  **read-only snapshot**, rebuilt each time you press `O` so it reflects the
  current versions (renamed/deleted drafts drop out).
- The room then best-effort launches a GUI editor (`$VISUAL`, else `code` /
  `codium`). Either way the status line shows the folder path, so you can open it
  manually if no launcher is found.
- The folder lives **on the gateway host**. When the room runs on that same
  machine (the usual local-dev setup) the editor opens directly; against a remote
  gateway, use the printed path over your own mount/SSH.

This is **read-only** — editing these files does not change what the agent is
doing. To make changes the agent picks up, use the workbench flow
(`artifact_project` → edit → `workbench_reconcile`); to flag an issue without
editing, leave a comment (below).

## Commenting on live files

When you're reviewing a draft and spot an issue, you can attach a comment to
**that file** instead of describing it in a free-form message:

1. Press `c` to open the content tree, select the file, and `o` to open it.
2. Press `m` to comment. Type your remark and press `Enter`.
3. Optionally prefix the body with a **line hint** — `L12: …` for a single line or
   `L12-14: …` for a range. (A malformed or reversed hint is ignored and the whole
   text is kept as the comment.)

The comment is **anchored to the exact version** you were viewing, appears on the
timeline as `⚠ [🧑 operator] commented on ‹file›`, and is delivered to the agent
**at its next turn** — it does not interrupt the current one. If the agent has
already written a newer version of that file, the room confirms with
`✓ commented (file changed since — agent will re-read)` and the agent is told to
re-read before acting on the line numbers.

Comments are **observations, not edits**: they never change the agent's files —
the agent decides how to respond. To make changes yourself, use the workbench
flow (`artifact_project` → edit → `workbench_reconcile`) instead.

## How it relates to `chat` and `trace`

- `autonoetic chat <agent>` — start/continue a conversation (also how you create a
  fresh session).
- `autonoetic room <session>` — the live, multi-actor view of a session, with
  gate resolution and messaging. Read-only without `--tui`.
- `autonoetic trace …` — after-the-fact inspection (causal chain, contract
  health, execution traces).

## Troubleshooting

- **“Missing AUTONOETIC_SHARED_SECRET …”** — export the same secret the gateway
  was started with.
- **“cannot reach gateway at 127.0.0.1:PORT”** — the gateway isn't running, or
  is on a different port; check `autonoetic gateway status` and your `config.yaml`.
- **Empty / “(no activity …)”** — nothing has happened at or above the floor for
  that session. Lower it with `--min-altitude detail`, or check the session id.
- **A gate won't accept my answer** — an empty answer is rejected; a question
  that requires a choice won't take free text (type the option number instead).
