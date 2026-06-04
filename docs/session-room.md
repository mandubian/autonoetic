# The Session Room

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
| `⚠` | attention | approval requested, a clarification (`user.ask`), plan proposed, a sentinel divergence |
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
| `q` / `Ctrl+C` | Quit |
| `Esc` | Close the detail pane if open, otherwise quit |

### Acting on the session
| Key | Available when | Action |
|---|---|---|
| `y` | an **approval** is selected (`⚠ approval requested`) | Approve — opens a one-line **motivation** prompt |
| `n` | an **approval** is selected | Reject — opens a one-line motivation prompt |
| `r` | a **clarification** is selected (`⚠ asks: …`) | Reply to the question |
| `i` | any time | **Compose a message** to send into the session |

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
