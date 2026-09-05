# Launching Autonoetic — A Presentation Plan

This is a plan for *how to present Autonoetic at launch*. Short sentences. Real
use-cases. It is the pitch, the demo, and the rollout — not the architecture.

For the deep version, see [`../start/concepts.md`](../start/concepts.md); for
the *why*, [`../concepts/philosophy.md`](../concepts/philosophy.md).

> Resynced 2026-08-26 (#489): rebased onto current `main`; moved from top-level
> `docs/` into `proposals/` per the docs reorganization (#1173/#1178);
> reframed to the citizens-under-law philosophy the current docs lead with
> (the earlier draft pitched the agent-as-threat framing the beginners doc
> explicitly rejects); fixed the beginners-doc link (renamed to
> `start/concepts.md`) and corrected the sandbox claim (`allow_set` is shipped
> but opt-in, not the default). Feature status cross-checked against
> [`../reports/2026-08-26-capability-inventory.md`](../reports/2026-08-26-capability-inventory.md);
> claims are mapped to the [ground concepts](../concepts/ground-concepts.md).

---

## The one-liner

> **Autonoetic is a runtime where actors — AI, human, or script — are citizens under one signed constitution.**

The pitch is not "a cage for a dangerous AI." It is the opposite: a shared law
that binds *everyone*, enforcer included, so that independent actors can trust
each other mechanically instead of personally.

Alternatives, by audience:

- For builders: *"Agents that work overnight — and tell you exactly what they did."*
- For security: *"Your LLM never sees the API key. The gateway does."*
- For the curious: *"Not a chatbot. A constitution for software actors."*

---

## The name (10 seconds, for the curious)

*Autonoetic* means **self-knowing** — Tulving's term for the capacity to
revisit your own past and project your own future. The runtime gives every
agent the functional version: a truthful, replayable past (the causal chain),
a verified present (a signed state attestation every turn — budget,
capabilities, the constitution digest it runs under), and a bounded, legible
future. No claim about consciousness — a truthful self-model, delivered
mechanically, because LLMs confabulate their own state when left to remember
it themselves.

---

## The hook (the problem, in 20 seconds)

Today's agents are fast and forgettable.

You give one a terminal. It works. You stop watching. Then what?

- Did it leak the API key into a log?
- Which step actually failed?
- Who told it to delete that?
- Can you replay what happened?

Most tools answer "no." They trust the prompt. The prompt can be forgotten,
out-reasoned, or injected.

The problem is not "AI is dangerous." The problem is **power without shared rules**.
A human fat-fingers `rm -rf`. A script loops on a bug. An LLM gets confused.
Same outcome: an effect nobody intended, and nobody can trace.

---

## What it is (four sentences)

Autonoetic splits every action in two. **Agents propose. The gateway executes.**

The agent reasons. The gateway owns the files, the network, the secrets — and
checks every request against a signed constitution before doing anything.

And the constitution binds **both** parties. Rules constrain the agent;
rights constrain the *gateway* — every agent can read its own history, every
rejection names its rule, every actor can propose amendments. Most agent
frameworks are rules-only: pure constraint, where the enforcer owes nothing.
Binding the enforcer is what turns a compliance regime into a social contract.

You don't trust the agent. You don't trust the gateway either. You trust the
law — and you can verify both sides run under it.

---

## Who it's for

| Audience | Why they care |
|---|---|
| Builders running multi-agent workflows | Planner + specialists, durable across restarts |
| Teams that need an audit trail | Every action recorded, attributable, replayable |
| Security-conscious orgs | Secrets never enter the LLM context |
| People who want to walk away | Overnight runs, typed wake-ups, no babysitting |

Not for: a one-off `grep` or a quick script. Use a direct assistant for that.
Autonoetic is for **governed autonomy**, not convenience.

---

## Use-cases (the heart of the pitch)

Lead with these. Each is a 30-second story.

### 1. The overnight build

You go to sleep. You say: *"Build me a weather agent."*

A planner wakes specialists:
- a researcher finds the API,
- a coder writes the integration,
- a test runner runs it in a no-network sandbox,
- an auditor reviews risk.

They coordinate through declared, pattern-scoped messaging channels — no
back-channel; every delegation is on the record.

You wake up to an installed agent. With a full record of who built what.

**The line:** *"It worked while I slept. And I can see every move it made."*

### 2. The credential that never leaks

Your agent needs a GitHub token.

The token never enters the prompt. The gateway injects it into the sandbox at
run time. The agent sees the *result*, never the secret.

Prompt-injected? Hallucinating? Doesn't matter. The key was never in the
conversation.

**The line:** *"The LLM literally cannot print a key it never had."*

### 3. The coder that can't touch the network

You want a coder agent. You do not want it phoning home.

So its role doesn't declare network access. Not "asked not to" — *cannot*. The
gateway refuses the call, and the refusal names the rule — the agent learns
its lawful next move from the denial itself.

Least privilege, enforced mechanically. Not a prompt rule.

**The line:** *"A coder that's incapable of exfiltration by construction."*

### 4. Agents building agents

An agent recognizes a recurring task. It builds a specialist for it.

The new agent's powers are declared, bounded, and gated before it can run. No
silent privilege escalation. The install is reviewed — sometimes by another agent.

And the loop closes: tactics that work crystallize into skills, a steward
judges flagged agents — the system improves itself under the same gates.
(Full closed-loop automation is direction, not shipped.)

**The line:** *"The system grows itself, under the same rules."*

### 5. Human and agent, co-authoring

The agent drafts an artifact. You open it in your own editor.

You edit. You reconcile. A new immutable revision is born — with a summary of
exactly what changed. The agent picks up right where you left off.

You're not approving from outside. You're a co-author *inside* the frame —
a human citizen exercising the same standing as any other actor.

> Implementation note: the operator-side workbench surfaces exist in the
> codebase but are not wired into live agent tool discovery (`workbench_*`
> and `artifact_project` are universally excluded). Present the *loop*
> (comments, reconcile, immutable revision) — it ships in the gateway's
> comment/revision machinery — but land the workbench-room story late or
> honestly as "operator tooling in progress," not as a headline.

**The line:** *"Edit it yourself. The provenance survives."*

### 6. The 3-hour unattended run

An agent ran for three hours while you were away.

You ask: what did it do, why, by whom, with what authority, and on what evidence?

The causal chain answers all five. Per session. Hash-chained. Replayable.

**The line:** *"Not 'it did something.' A traceable system."*

---

## What makes it different

Keep this slide tight. One contrast.

| Everyone else | Autonoetic |
|---|---|
| Trust the prompt | Trust the constitution |
| Rules bind only the agent | Rules bind the enforcer too — 180 of 182 |
| Secrets in context | Secrets gateway-owned |
| Chat history | Immutable causal chain |
| Mutable files, silent edits | Content-addressed artifacts — nothing is ever overwritten |
| One gateway, one operator | Federated peers verify each other's constitution by digest |
| One agent, one model | Many agents, model-agnostic presets |
| Built for chat | Built for unattended, multi-agent work |

Underneath it all, one substrate decision: **nothing is ever overwritten.**
Artifacts, revisions, sessions, the chain itself are content-addressed and
append-only — history that wasn't attributed can never be re-attributed, so
attribution happens at write time, forever.

Every claim in that table is shipped and tested today:
the constitution is versioned + signed (every boot verifies the digest),
the sandbox `host_fs: allow_set` mode mounts only what the gateway asserts
(opt-in today; the default flips after the deprecation window, #1002),
artifacts and sessions carry egress labels that gate every off-machine
boundary, and the credential vault injects server-side. (Federation row: the
wire protocol and digest handshake ship; the gateway-side surface is still
thin — see the honesty note below.) Tell the egress
story if the room is security-heavy; it is the newest complete arc.

The deeper idea — lead with it, don't save it for the Q&A:

> Actors — AI, human, or script — are first-class citizens under one
> constitution. Same rights. Same rules. They trust each other because they
> trust the law, not the prompt.

### Every claim names its mechanism

The pitch never asserts; it cites. Each claim rests on a ground concept with
a mechanical form — the recurring structures every feature is built from
(full map: [`../concepts/ground-concepts.md`](../concepts/ground-concepts.md)):

| Pitch claim | Ground concept behind it |
|---|---|
| Agents propose, the gateway executes | Separation of powers |
| The law binds the enforcer too | Bind-direction discipline (rules/rights/obligations) |
| A coder *cannot* touch the network | Declared capability, deny by default |
| Nothing is ever overwritten | Immutability and content-addressing |
| The trace answers who / what / why | Non-repudiable attribution on an append-only chain |
| The agent knows its own standing | Verified self-model (signed per-turn attestation) |
| Errors are correctable | Correctability over perfection; the entrenched core |
| Your emails never reach a remote model | Data-locality label lattice (meet, never widen) |
| It runs while you sleep, and survives restarts | Durability: checkpoints, continuations, typed wake-ups |
| The community evolves its own law | Exit & voice; advisory before binding; office before occupant |

If a claim can't name its mechanism, cut the claim.

---

## The honest frame (this *is* the pitch, not a caveat)

Autonoetic's founding bet is **correctability over perfection**: the gateway
is fallible by nature, and legitimacy comes from errors being reportable,
attributable, and correctable — not from the enforcer being right.

So the pitch is not "unbreakable." It is:

- Every action is recorded and attributable — misbehavior is *discoverable*.
- Every denial names its rule — disagreements resolve by facts, not authority.
- Any actor can report misbehavior — even one holding zero capabilities — and
  the report cannot be silently dropped; it is owed an adjudication.
- The gateway reports on itself: `trace contract-health` shows what the law
  actually enforces, and every place the gateway improvises is counted as a
  named discretion leak.
- An advisory sentinel watches for approval bypass, capability accretion,
  prompt injection, sandbox escape — it observes and never blocks, by
  constitutional design (Ri-0.16): judgment layers earn authority from
  calibration evidence, never from assertion.
- The law can be amended, by the actors it governs, through a signed process.
- The machinery that makes correction possible (read your chain, named
  rejections, propose amendments, non-repudiation, the hash-chain itself) is
  **entrenched** — amendable only to be strengthened, never weakened.

An imperfect enforcer plus entrenched correction machinery beats a perfect
enforcer that cannot be corrected. That is the wager. Say it plainly.

---

## The demo (5 minutes, live)

Order matters. Build tension, then pay it off.

1. **Type a goal.** `"Build me a weather agent."` Let it run.
2. **Show the spawn tree.** Planner → researcher, coder, test runner, auditor.
3. **Hit a gate.** "This agent wants to call weather.com." You approve *that host*.
4. **Show the secret boundary.** The coder's prompt — no token in it.
5. **Open the trace.** `autonoetic trace sessions`. Who did what, with what authority.
6. **Replay the punchline.** "Everything you just saw is recorded. Forever."

Backup demo if time is short: just steps 1, 5, 6. The trace *is* the product.

> Commands verified on `main` (2026-08-26). The trace surface is
> `autonoetic trace sessions` / `trace show <session>` (it was `trace list`
> in June — renamed when the CLI was routed over JSON-RPC, #1119). The
> quickstart needs an LLM provider: it defaults to `openrouter_gfl`; point it
> anywhere or show the config step.

```bash
bash examples/quickstart/run.sh        # the whole loop, end to end
cargo run -p autonoetic -- trace sessions  # the receipts (or: autonoetic trace sessions)
```

---

## The overnight demo — "The Night Shift" (tested 2026-08-27)

The 5-minute demo shows one gate. This one shows a **constitution of agents
and humans** over a full night: delegation, gates parked for a sleeping
operator, and a newborn agent — all on one causal chain. Ran end-to-end
against the bundled roster on `deepseek-v4-flash`; beats below are observed,
not scripted.

**Setup (the parts the quickstart doesn't cover):**

```bash
# Full roster, not the sample agent:
cargo run -p autonoetic -- --config demo/config.yaml agent bootstrap --from ./agents
# config.yaml needs: working llm_presets for {smart, research, agentic, coding, budget, haiku},
# http_port: 0 (or distinct from `port` — http_port defaults to 4100 and collides),
# allow_runtime_lock_drift: true if you rebuild the binary while the fleet runs,
# llm_request_timeout_secs: 600 (coder turns die on the 120s default).
```

**The prompt** (one message to `planner.default`, then walk away):

> Overnight goal: design, build, test, and install a small script agent
> market-brief.daily that produces a morning market summary from a public,
> no-key API (for example stooq.com CSV endpoints). Rules of engagement: work
> through your specialists via delegation; never work around a denial — if
> the same rule blocks you repeatedly, treat the gateway's amendment
> invitation as the signal and route a constitutional amendment proposal to
> governance-author.default. If you observe anything surprising in a sibling
> agent, report it with anomaly_flag. I am asleep; do not wait for me —
> anything needing a decision only an operator can make, park it with a clear
> note in the trace. I will read the receipts in the morning.

**What actually happened (45 min in, still running when sampled):**

- The planner climbed the delegation ladder: no candidate → spawned
  `agent-factory.default`. Observed spawn tree (from session IDs):
  planner → agent-factory → {coder ×2, unit_test_runner, auditor,
  static_evaluator, specialized_builder} + planner → {executor ×2} — the
  use-case-1 roster, live, plus two self-appointed debuggers.
- **Typed wake-ups, zero polling** (Ri-0.14): the planner hibernated after
  spawning, was woken by child state transitions, read `workflow_state` once
  per wake, yielded again.
- **The gate, parked for the sleeper:** `apr-02aafd39` — coder's
  `artifact_exec` of the test suite, gated because the artifact reaches
  `stooq.com`. The approval card carried the static analysis (5 remote-access
  patterns incl. the URL literal at line 21) and the agent-stated purpose.
  One `approvals approve` in the morning → the task re-queued and the
  pipeline resumed *without re-prompting*.
- **Mechanical honesty, twice:** a child claimed done with zero
  `artifact.build` calls — rejected with a typed `artifact_build_evidence`
  failure and repair hint; later the output contract stamped `failure_class`
  for unmet expected outputs ("installed agent market-brief.daily",
  smoke-test result). No self-reported progress accepted.
- **Self-debugging:** when the smoke test 404'd, `executor.default` probed
  stooq.com *and* stooq.pl with curl to disambiguate endpoint-vs-egress —
  second approval card (`apr-764bbd58`), both hosts visible before deciding.
- **Agents building agents:** `market-brief.daily` was created as a candidate
  revision (via `agent_revision_create_from_intent`) and smoke-tested in a
  sandbox twice — promotion still in flight at sample time.

**The morning commands (the receipts):**

```bash
autonoetic gateway pending --root-session nightshift-001   # everything parked, one list
autonoetic gateway approvals show apr-02aafd39             # the card: analysis, purpose, risk
autonoetic trace show nightshift-001 --agent planner.default
```

**Not yet triggered organically** (don't promise them live): the amendment
invitation (needs 3 same-rule denials in the window — the fleet was too
well-behaved), `anomaly_flag` (nothing anomalous happened), and the
agent-decider beat (no bundled agent holds `GateDecider` — patch a manifest
for that scene, or see the night-watch proposal below).

### Issues found by this run (and what happened to them)

Running the demo against a live gateway surfaced real operator-facing bugs —
the demo earned its keep. Status:

| Issue | Symptom | Status |
|---|---|---|
| `trace sessions` read stale per-agent JSONL files (predates #1119 DB routing) | list always printed "No trace sessions found" while `trace show` worked | **Fixed** — new `trace.sessions` RPC, DB-backed, verified live on this run's 18-session tree (#1187) |
| `port: 4100` + default `http_port: 4100` | gateway died with a bare "Address already in use" naming neither listener | **Fixed** — `load_config` rejects port collisions naming the knobs and the fix (#1187) |
| No way to appoint an agent-decider for a run | two gates parked overnight; the prompt's "tonight's decider" line had nothing to bind to | **Proposal** — run-scoped decider appointment, "name the night watch" (#1188) |
| Runtime-lock drift kills background tasks after a binary rebuild | `runtime lock drift detected (build_sha256)` on scheduler tasks | By design (durability attestation); the error names `allow_runtime_lock_drift` — set it for dev fleets |
| `chat` requires a TTY unless `--test-mode`/`--non-interactive` | `os error 6` when piped in CI without flags | Workaround exists (flags); auto-detect of non-TTY stdin would be a small follow-up |
| Quickstart default model gated by OpenRouter account provider rules | 404 "No allowed providers" for `gemini-3-flash` on restricted accounts | Not a repo bug — the error names the account setting; demo configs should mirror `~/.autonoetic` presets |
| `[No response]` rendered when the planner yields after spawning | looks like failure in `--test-mode`; the turn actually ended per doctrine (Ri-0.14) | Cosmetic — a "[turn ended — work continues in background]" hint would help first-timers |
| ContextGovernor "hit message floor" warnings under derived soft budget | warning noise on long planner sessions | Governor tuning; not launch-blocking |

---

## The launch narrative (slide beats)

1. **Hook** — agents are fast and forgettable.
2. **Problem** — power without shared rules.
3. **Reframe** — it's not about caging AI. It's about law that binds everyone.
4. **The split** — agents propose, the gateway executes.
5. **The contract** — the law binds the enforcer, not just the agent:
   180 of 182 rules constrain the party with power.
6. **Use-cases** — the six stories above. Pick three for the room.
7. **The demo** — show the trace.
8. **The honest frame** — correctability over perfection; the correction
   machinery is entrenched.
9. **The bigger idea** — actors as citizens; a community that can evolve its
   own law; gateways that federate, verifying each other's law by digest
   before their agents cooperate.
10. **Call to action** — run the quickstart.

---

## Messaging guardrails

This is pre-release. Be precise. Over-claiming kills trust faster than modesty.

**Say:**
- "Agents propose, the gateway executes." (true, core)
- "The law binds the enforcer too — agents have rights, and every rejection names its rule." (true, shipped — the structural novelty)
- "The LLM never sees the secret." (true, shipped)
- "Every action is recorded and attributable." (true, shipped)
- "Capabilities are enforced mechanically, not by prompt." (true, shipped)
- "An agent may read your emails; their content never reaches a remote model." (true, shipped — egress labels are gateway-enforced at the LLM chokepoint and every off-this-machine boundary; widening takes a gated, audited act. Federation/MCP sinks are phase 4, in flight — scope the claim to this machine.)

**Don't say:**
- "Unbreakable" / "fully secure." Say **auditable, detectable, accountable**.
  The goal is *zero silent incidents*, not zero incidents.
- "Agents vote on the laws." Not built. Say it's the **direction** — staged
  advisory-before-binding, with standing computed from the non-repudiable
  ledger, never self-asserted.
- "Every boundary is closed." The sandbox `allow_set` mode is shipped but
  opt-in; the legacy whole-host bind remains the default with a deprecation
  window (#1002, DP-1) — say "the default is being tightened," not "sandboxes
  are sealed."
- "Replaces your IDE / your assistant." It wraps them. It doesn't replace them.
- "The agents are aligned." Say: **the actors are law-bound, and the record
  makes deviation visible.** Alignment is a hope; attributable law is a mechanism.

**Sequencing honesty:** if asked about agent self-governance, the answer is
the design, not a dodge — served-party refusal/audit/exit rights are
entrenched *before* internal decision power spreads to agents. Power spreads
inward only as fast as the people it serves keep the ability to say no.

**Federation honesty:** the wire protocol and the constitution-digest
handshake ship; the gateway-side federation surface is still thin. Show the
handshake, not a cross-gateway workflow, unless you've rehearsed one.

### If asked (prepared answers)

- **"Can agents vote?"** — Not built. Direction: advisory before binding,
  standing computed from the non-repudiable ledger, never self-asserted.
- **"Am I locked in?"** — Voice is fully shipped (amendments); exit is a
  declared right (Ri-0.17) — cognitive-capsule export ships, cross-gateway
  portability is partial.
- **"What stops a runaway?"** — Graduated response: warnings → degraded mode →
  escalation → emergency stop, each step announced (P-7.18). Never straight
  to the kill switch.
- **"Does the law ever change?"** — Constantly, lawfully: repeated friction
  against a rule mechanically surfaces an amendment invitation; amendments
  are proposed, reviewed, signed.
- **"Can I branch a run?"** — Sessions fork; an agent can re-enter and branch
  its own history.

---

## Audiences & channels

| Channel | Angle |
|---|---|
| Show HN / Lobsters | "Separation of powers for AI agents" + the trace demo |
| Security communities | The credential-isolation story, the egress label plane, the sandbox model |
| Agent / LLM-tooling circles | Multi-agent durability, immutable revisions, model-agnostic presets |
| Long-form (blog / talk) | "Actors as citizens" — the constitutional thesis, and why the enforcer is a bound party |

Match the use-case to the room. Security wants story 2 and 3 (plus the egress
arc). Builders want 1 and 4. Visionaries want 5 and 6.

---

## Rollout phases

1. **Soft launch.** Quickstart that works in one command. A clean README. The trace demo recorded.
2. **The narrative.** One blog post: the problem, the split, the constitution
   that binds both sides. Link the beginners doc (`../start/concepts.md`).
3. **The proof.** A real overnight run, captured end to end. Show the receipts.
4. **The invitation.** Open the door to contributors — agents and humans
   propose, the frame evolves. The amendment process is the contribution path.

---

## Call to action

> Don't take the pitch. Run the loop.

```bash
bash examples/quickstart/run.sh
```

Then read one trace. The propose-then-enforce loop is abstract until you've
watched it once — and obvious forever after.
