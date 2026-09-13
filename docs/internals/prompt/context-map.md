# Context Map: what an agent's context actually holds, and what gets trimmed

**Measured:** 2026-09-13, post skill-trim sweep (#1329–#1331) and
anti-confabulation work (#1332, #1333). Numbers drift as doctrine and tool
schemas evolve — regenerate with:

```bash
cargo nextest run -p autonoetic-gateway -E 'test(prompt_composition_report)' \
  --success-output=immediate
```

The harness
(`autonoetic-gateway/tests/prompt/prompt_composition_budget.rs`) measures the
same layers the gateway composes; its ceilings are the ratchet that keeps this
map honest. Authoring rules live in
[`composition.md`](composition.md); budget strategies in
[`budget.md`](budget.md); the evidence behind the layer ordering in
[`burden-study.md`](burden-study.md).

---

## Two zones, two trim behaviors

An agent's context is one system message plus a conversation. The two halves
behave completely differently under budget pressure:

### Zone 1 — Standing system prompt (never trimmed)

Assembled per turn from fixed layers; byte-identical across turns of a session
except for the phase-earned tail, so the KV-cache prefix stays intact. Nothing
in this zone is ever evicted — this is the weight the composition ratchet
governs.

| Layer | Nature | Trim behavior |
|---|---|---|
| Tool schemas | JSON definition of every advertised tool (name, description, full input schema), tier-filtered per manifest | Never — the largest single layer on every agent |
| SKILL.md core | Doctrine before the `<!-- extended -->` marker: identity, principles, decision flows, foundational-agent tables | Never — turn 1 onward |
| SKILL.md extended | Doctrine after the marker (procedures, routing tables); announced by a `gateway_note` on the first tool result, inlined permanently from turn 2 | Permanent once loaded |
| Phase-earned sections | SKILL sections gated on phase facts (`phase(artifact_built)`, `phase(child_spawned)`, …) — evicted until the session earns them | Enters at the phase, then permanent |
| Foundation layers | Shared cross-agent doctrine (SDK reference, workflow/artifact/digest/script conventions) from `foundation_*.md`, manifest-selected | Never |
| Guidance | Gateway-authored standing blocks (yield discipline, spawn coordination, federation procedure), the procedure ones phase-gated | Never; phase-gated blocks enter at the phase |

### Zone 2 — Conversation (trimmed oldest-group-first)

Operator turns, assistant replies, tool results, wake notifications, and
turn-start signals. When the total exceeds the model's context window minus
Zone 1, the `trim_history` strategy drops **whole exchange groups** (an
assistant turn fused with its tool results — a call is never split from its
result) **from the front**, down to a floor of the 2 most recent groups; if
even that does not fit, it errors loudly instead of over-trimming.

Consequences worth internalizing:

- **The newest messages are the only trim-proof location.** Wake
  notifications, new tool results, and turn-start notes always land there.
- **Early turns are the first casualties.** On a small-context model the
  fixed Zone 1 consumes a large share of the window, so the task framing from
  turn 1 is evicted early (this is the mechanism behind the
  session-76a8d5c6 confabulation; postmortem lands with #1333).
- **Anything the agent needs long-term must not live only in early prose.**
  Gateway-observed truth is re-presented where possible (wake notifications
  carry `artifact_refs`; `reuse_guards` are re-derivable on demand) and
  verified at the boundary (`unknown_artifact_ref` claim verification).

---

## Measured map (2026-09-13 snapshot)

Characters; `~tok` at the harness's 4 ch/token estimate.

### planner.default (front-door lead)

| Layer | ch | ~tok |
|---|---:|---:|
| Tool schemas (33 tools) | 39,192 | 9,798 |
| SKILL.md core | 15,616 | 3,904 |
| SKILL.md extended | 14,280 | 3,570 |
| SKILL.md phase-earned (`artifact_built`) | 11,807 | 2,951 |
| Foundation layers | 13,112 | 3,278 |
| Guidance (pre-phase → all phases) | 7,738 → 9,521 | 1,934 → 2,380 |
| **Turn 1 / working / steady-state** | **75,658 / 89,938 / 103,528** | **18,914 / 22,484 / 25,882** |

### All measured agents (turn-1 / working / steady-state, ~tok)

| Agent | Tools | Turn 1 | Working | Steady |
|---|---:|---:|---:|---:|
| planner.collaborative | 40 | 23,328 | 23,486 | 26,215 |
| specialized_builder.default | 30 | 21,304 | 21,304 | 21,641 |
| agent-factory.default | 29 | 19,852 | 19,852 | 20,921 |
| planner.default | 33 | 18,914 | 22,484 | 25,882 |
| coder.default | 25 | 15,372 | 17,007 | 17,799 |
| credential_onboarding.default | 28 | 14,313 | 14,313 | 14,313 |
| unit_test_runner.default | 23 | 12,414 | 12,414 | 12,620 |

Phase-earned deferrals: coder 791 tok, planner 2,951 tok, planner.collaborative
2,283 tok, agent-factory 1,068 tok (all behind `artifact_built` /
`child_spawned`). "Working" is the modal turn for sessions that never build —
the gap between working and steady is what the phase gates buy.

### Reading the map

- **Tool schemas are 38–47% of every fixed prompt** — larger than any doctrine
  layer, by design (the burden study's lever ordering rests on this).
- **The conversation budget is what's left**: `context_window_tokens` minus the
  steady-state figure. On a 32k-window model, planner.default's ~25.9k tok
  fixed prompt leaves only ~6k tok of history — a dozen-ish exchanges — which
  is why small-model sessions experience trimming as aggressive. On a 128k
  window the same session never feels it.
- The conversation zone has no ratchet: it is bounded only by the window and
  the `trim_history` floor. Doctrine that must survive a session therefore
  belongs in Zone 1 (via the ratchet) or in gateway-re-presented turn-start
  material (wake notifications, guidance) — never only in early prose.
