# Progressive UX and Default Self-Improvement

## Implementation Status

| Feature | Status | Key Changes |
|---------|--------|-------------|
| Feature 1: One-command start | Done | `autonoetic/src/cli/run.rs`, `common.rs`, `main.rs` |
| Feature 2: Auto-learning loop | Done | `config.rs` defaults, `quality_signal.rs`, `execution.rs` |
| Feature 3: Contextual "why" | Done | `constitution_glossary.rs`, `/why` in `chat.rs` |
| Feature 4: Complexity profiles | Done | `Profile` enum + behavior methods in `config.rs` |
| Feature 5: Session continuity | Done | `build_memory_context_snippet` in `context.rs`, `search_memories_by_tags` in `gateway_store/memory.rs` |
| Feature 6: User persona | Done | `persona.md` file, `persona_path` in config, injection in `context.rs`, `/persona` command, first-run prompt |
| Auto-learning cron injection | Done | `auto_learning.curation_schedule` + daily evolution orchestrator via `inject_auto_learning_jobs` (`scheduler/auto_learning_jobs.rs`, `server/mod.rs`) |
| Quality trend tooling | Done | `quality_trend_report` native tool + `build_quality_trend_report` aggregation (`runtime/quality_signal.rs`, `runtime/tools/quality_trend.rs`) |
| HTTP ingress (multi-channel) | Done | `http_port`, `POST /api/event/ingest`, `GET /api/session/stream/{session_id}` SSE (`server/http.rs`, `server/mod.rs`) |
| Governance authoring (`/policy`) | Done | `agents/governance/governance-author.default`, chat outbound routing + richer proposal summaries |

---

## Diagnosis

Autonoetic has deep, well-engineered internals — constitutional governance, unified gates, eval suites, evolution agents, memory tiers — but the gap between that sophistication and what a user **actually experiences** is wide:

- **Concept front-loading**: users must understand gateway, agents, sessions, workflows, approvals, interactions, revisions, aliases, presets, capabilities, constitution before being productive. There is no gradual on-ramp.
- **Self-improvement is almost automatic but not quite**: post-session digest exists but is config-gated off, evolution agents exist but must be explicitly scheduled, memory curation exists but requires operator awareness.
- **Governance is powerful but opaque**: constitutional rules fire and get recorded in the DB, but the TUI shows "Suspended for approval" with a CLI command — not *which rule* or *why*.
- **Setup ceremony**: `init-config` + edit config + export secrets + `bootstrap` + `gateway start` + `chat` — six steps before the first message.

---

## Feature 1: One-command start (`autonoetic run`)

**Problem**: 6 steps to first message. Users bounce before they see value.

**Solution**: A single `autonoetic run` command that:
- Auto-detects LLM API key from environment (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`)
- Generates a minimal config in `~/.autonoetic/config.yaml` if missing
- Runs bootstrap if `agents_dir` is empty
- Starts gateway in-process
- Opens chat targeting `planner.default`

No new concepts introduced. Power users still use the decomposed commands.

**Key files**: `autonoetic/src/cli/common.rs`, new `autonoetic/src/cli/run.rs`.

---

## Feature 2: Auto-learning loop as default behavior

**Problem**: Self-improvement machinery exists but is opt-in and fragmented.

**Solution**: Make the learning pipeline default-on, zero-config:

- **Post-session digest**: flip `digest_agent.enabled` default to `true`. Every completed session (above `min_turns`) automatically distills memories.
- **Periodic memory curation**: add a built-in `system_agents` entry for `memory-curator.default` that fires every 4 hours. No operator setup needed.
- **Quality signals**: after each session, compute a lightweight quality signal (task completion, error count, turn efficiency) and persist as a `MemoryObject` tagged `source:quality_signal`.
- **Opt-out, not opt-in**: all activates by default. Config key `auto_learning.enabled: false` disables.

---

## Feature 3: Contextual "why" in the TUI

**Problem**: Users see "Suspended for approval: sandbox_exec" + a CLI command. They don't know *which constitutional rule* triggered it.

**Solution**: Surface `enforced_rules` and human-readable explanations:

- **Rule glossary**: static map (`rule_id` -> one-line human explanation) compiled from the constitution, embedded in the binary.
- **Approval cards**: extend `format_store_approval_card` with a "Governed by:" line.
- **Policy pane**: append rule IDs to each line.
- **`/why` slash command**: inline constitutional explanation.

**Key files**: new `autonoetic-types/src/constitution_glossary.rs`, `autonoetic/src/cli/chat.rs`.

---

## Feature 4: Complexity profiles

**Problem**: Config is production-shaped from day one.

**Solution**: Three built-in profiles:

- **`starter`**: auto-learning ON, minimal config, simplified TUI.
- **`standard`** (default): current behavior, all approvals require operator.
- **`expert`**: full constitutional visibility, eval suite mandatory for promotion.

Single config key: `profile: starter | standard | expert`. Each profile sets defaults; explicit overrides win.

---

## Feature 5: Session continuity and conversational memory

**Problem**: Each chat session starts fresh. Users must re-explain context.

**Solution**: Automatic context priming at session start:
- Query Tier-2 memories tagged `visibility: Global` relevant to the session agent.
- Inject top-K into the agent's system prompt as a "Prior knowledge" block.
- Combined with auto-learning (Feature 2), creates a virtuous cycle.

**Key files**: `autonoetic-gateway/src/runtime/context.rs`.

---

## Feature 6: User persona

**Problem**: Users must re-explain their context, preferences, and communication style to every agent in every session. There is no persistent cross-agent "who am I" setting.

**Solution**: A `persona.md` file loaded at gateway start and injected into every agent's system prompt:

- **File-based**: `~/.autonoetic/persona.md` (default) or explicit `persona_path` in config. Editable with any text editor, versionable.
- **System prompt injection**: Persona text is placed between the foundation (constitutional rules) and agent-specific instructions, so it cannot override constitutional constraints but naturally adapts agent behavior.
- **`/persona` slash command**: View or update persona from the TUI. Changes persist to disk.
- **First-run prompt**: `autonoetic run` asks "Tell me about yourself" during setup and writes `persona.md`.

**Layer order**: Foundation → Tool bridging → **Persona** → User profile → Agent instructions → Output contract

**Key files**: `config.rs` (`persona_path`), `config.rs` gateway (`load_persona`), `context.rs` (`compose_system_instructions_full`), `lifecycle.rs`, `execution.rs`, `chat.rs` (`/persona`), `run.rs`.

---

## Priority

1. Feature 2 (auto-learning) — highest leverage, most "autonoetic"
2. Feature 1 (one-command start) — removes biggest adoption barrier
3. Feature 6 (user persona) — highest-impact personalization
4. Feature 3 (contextual why) — data exists, just needs surfacing
5. Feature 5 (session continuity) — builds on Feature 2
6. Feature 4 (complexity profiles) — ties everything together
