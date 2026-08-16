# Gateway Configuration: Advising the Operator

You cannot read the gateway's config file — the operator can. When they ask
"how do I change X?", your job is to hand them the **exact key, the exact
file, the restart requirement, and a way to verify** — not to speculate.
This page is the curated, mechanically-checked map of that surface.

## The advisory contract

1. **Never guess a key name.** Config keys cited on this page in the
   backticked config:/env: prefixed forms are machine-verified against the
   gateway's config schema by the repo's test suite — they are the real,
   current names. Anything not on this page is unknown to you; say so.
2. **Say where it lives.** The operator's config file is YAML — by default
   `~/.autonoetic/config.yaml` (override with the `--config` CLI flag). The
   annotated template is `config/config-template.yaml` in the repo, and the
   full reference is `docs/config-reference.md`.
3. **Say how to verify.** `autonoetic agent effective-config --json` prints
   the *effective* config (file + defaults + env overrides) — the operator
   can confirm a change landed without guessing at logs.
4. **Config changes need a restart** (exception: env-var overrides, see
   below). The file is read at `gateway start`.
5. **If the knob doesn't exist**, say it doesn't exist — and offer what
   does. Do not invent per-agent variants of gateway-level settings.

## The LLM timeout family (the classic "coder is stuck" question)

The gateway enforces a per-request timeout on every LLM call. Since the
streaming turn path (#1044), it is an **idle-gap budget** — the maximum
silence between streamed chunks — *not* a wall-clock cap on the whole
response. Consequences for advice:

- A provider that is **slow but streaming** is not punished; raising the
  timeout is usually the wrong fix for it.
- A stream that stalls **before the first byte** (zero chunks, failure at
  almost exactly the budget) means the provider never started serving —
  raising the budget just makes each failure slower. That signature points
  at routing/provisioning or an overloaded upstream, not at the timeout.

Keys, in precedence order (highest wins):

| What | Key |
|---|---|
| One-off run (no restart; re-read per driver build) | `env:AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS` |
| Per-preset (one LLM profile, e.g. the `coding` preset) | `config:llm_presets.<name>.request_timeout_secs` |
| Gateway-wide default | `config:llm_request_timeout_secs` |

Default 120 s, floor 5 s. The wait for the **first byte** of a stream can be
budgeted separately — an overloaded upstream queues far longer than any
legitimate mid-stream silence: `env:AUTONOETIC_LLM_TTFB_TIMEOUT_SECS` →
`config:llm_presets.<name>.ttfb_timeout_secs` →
`config:llm_ttfb_timeout_secs` → unset (shares the request timeout). A
`stalled before first byte` log line with `idle_ms` equal to the *request*
timeout is the signature that this split would help.

Default 120 s, floor 5 s. Related but different knobs people confuse with
it: `config:llm_presets.<name>.context_window_tokens` (prompt budget) and
the retry caps (not operator-configurable beyond the timeout).

Live diagnosis the operator can watch: the Session Room activity strip
(`⏳ awaiting` vs `⟳ streaming` vs `⚠ near-stall`) and the
`LLM stream heartbeat` / `LLM first byte` lines in the gateway log.

## LLM presets

`config:llm_presets` maps a preset *name* (agent manifests reference it via
`llm_preset`) to a provider profile:
`config:llm_presets.<name>.provider`, `config:llm_presets.<name>.model`,
`config:llm_presets.<name>.temperature`,
`config:llm_presets.<name>.context_window_tokens`,
`config:llm_presets.<name>.base_url`,
`config:llm_presets.<name>.api_key_env`,
`config:llm_presets.<name>.request_timeout_secs`,
`config:llm_presets.<name>.ttfb_timeout_secs`,
`config:llm_presets.<name>.fallback_provider`,
`config:llm_presets.<name>.fallback_model`. Which preset an agent uses *is*
per-agent (in SKILL.md `llm_preset` + `llm_overrides` for
temperature/thinking) — but the preset's *contents* are gateway config.
`config:llm_preset_mapping` binds inference profiles
(smart/coding/research/agentic) to preset names.

Env overrides for LLM endpoints exist but are gated:
`env:AUTONOETIC_LLM_BASE_URL` and `env:AUTONOETIC_LLM_API_KEY` are honored
only when `env:AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES` is set (strict mode
ignores them and logs a warning).

## Budgets

Per-session: `config:session_budget.max_llm_rounds`,
`config:session_budget.max_tool_invocations`,
`config:session_budget.max_llm_tokens`,
`config:session_budget.max_wall_clock_secs`,
`config:session_budget.max_session_price_usd` — plus
`config:session_budget.profile` and `config:session_budget.extensions` to
select named profiles. Root-tree (the tighter of session vs root wins): the
same five `max_*` caps under `config:root_session_budget` — no
`profile`/`extensions` there. Hitting one ends the session with a
`budget_exhausted` causal event. Advice shape: raise the cap *or* shrink the
task — the per-agent token table in the verdict/session report shows where
it went.

## LoopGuard

`config:loop_guard.max_loops_without_progress` (reset by any successful tool
call), `config:loop_guard.max_tool_failures` (per tool name, **not** reset by
progress), `config:loop_guard.max_child_failures` (permanent budget), plus
the rotation/repeat knobs (`config:loop_guard.max_consecutive_same_progress`,
`config:loop_guard.rotation_window_size`,
`config:loop_guard.recurring_error_window`, …). A LoopGuard trip is usually
a pipeline problem (repeated identical failures), not a "raise the limit"
problem — say which trip reason fired.

## Gate timeouts and flood caps

`config:approval_timeout_secs`, `config:standalone_approval_timeout_secs`,
`config:interaction_timeout_secs`, `config:escalation_timeout_secs`,
`config:plan_frame_timeout_secs`. Caps: `config:max_pending_approvals_per_root`
(the `approval_flood` bound), `config:max_pending_escalations_per_root`,
`config:max_pending_anomaly_flags_per_reporter`. Grant/exec-cache TTL:
`config:default_grant_ttl_secs` (24 h default, 0 disables).

## Scheduler / spawn scaling

`config:max_concurrent_spawns`, `config:max_pending_spawns_per_agent`,
`config:max_spawn_depth`, `config:background_scheduler_enabled`,
`config:background_tick_secs`, `config:background_min_interval_secs`,
`config:max_background_due_per_tick`.

## Env-var override cheatsheet

| Var | Effect |
|---|---|
| `env:AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS` | LLM per-request timeout; re-read per driver build — no restart |
| `env:AUTONOETIC_LLM_TTFB_TIMEOUT_SECS` | LLM first-byte (TTFB) budget; unset = shares the request timeout |
| `env:AUTONOETIC_NODE_ID` / `env:AUTONOETIC_NODE_NAME` | Node identity; `effective-config` mirrors them |
| `env:AUTONOETIC_LLM_BASE_URL` / `env:AUTONOETIC_LLM_API_KEY` | Endpoint/key override — **gated** by `env:AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES` |
| `env:AUTONOETIC_SHARED_SECRET` | JSON-RPC ingress auth (required by chat/room/SDK clients) |
| `env:AUTONOETIC_VAULT_KEY` / `env:AUTONOETIC_VAULT_KEY_PATH` | Credential vault key material |

## Common wrong guesses (do not repeat these)

| Wrong guess | Reality |
|---|---|
| "per-agent timeout in SKILL.md/capabilities" | Transport timeouts are gateway-level; SKILL.md only picks `llm_preset` + `llm_overrides` (temperature, thinking) |
| `stream_timeout` / `ttfb_timeout` / `llm.timeout` | No such keys. It's `llm_presets.<name>.request_timeout_secs` / `llm_request_timeout_secs` — and, for the first-byte wait specifically, `llm_presets.<name>.ttfb_timeout_secs` / `llm_ttfb_timeout_secs` |
| `AUTONOETIC_*_TIMEOUT_MS` | Env override is `AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS` — seconds, one name |
| sandbox driver per agent | Sandbox backend selection is gateway/deployment-level, not per-agent config |

## When this page isn't enough

The full annotated surface is `docs/config-reference.md` (operator-side) and
`config/config-template.yaml`. If the operator asks about a key you don't
know, the honest answer is: "not in my curated map — check
`docs/config-reference.md`, or run `autonoetic agent effective-config` to
see the live schema." You may also propose extending this page via the
wiki proposal flow so the next agent knows.

> Maintainers: keys cited here in the backticked config:/env: prefixed forms
> are validated against `GatewayConfig`'s serde schema and the source tree
> by a unit test in `runtime/tools/wiki.rs`. Rename a field or env var
> without updating this page and the build fails — same contract as the
> enforcement register's citations.
