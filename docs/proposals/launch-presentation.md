# Launching Autonoetic — A Presentation Plan

This is a plan for *how to present Autonoetic at launch*. Short sentences. Real
use-cases. It is the pitch, the demo, and the rollout — not the architecture.

One framing decision up front: this is a **research bet, not a product
launch**. Autonoetic is experimental research infrastructure. It does not
compete with the interactive harnesses (Hermes, Claude Code,
deepseek-harness — for a model-plus-terminal under your eyes, those tools are
better). It exists to push one original idea as far as it goes in a running
system: what do agents need in order to run unwatched, delegate, and
self-modify under verifiable law? The deck's job is to state that wager
plainly, then show the instrument built to test it.

For the deep version, see [`../start/concepts.md`](../start/concepts.md); for
the *why*, [`../concepts/philosophy.md`](../concepts/philosophy.md).

> Resynced 2026-09-06: sandbox claim corrected — `host_fs: allow_set` is now
> the *default* and the whole-host ro-bind is the deprecated opt-out (#1002
> slices 4–6, including operator-approved session mount grants); the
> "180 of 182" stat replaced by the declared bind-direction model — all 221
> clauses classified, 215 bind the enforcer, 5 the decider, exactly 1 the
> reasoner (2026.09.04/09.05 amendments; [`../constitution/law-table.md`](../constitution/law-table.md));
> use-case 5's caveat updated for the shipped operator surface (session room
> TUI, web cockpit, trace fork); new shipped beats added — `autonoetic improve
> run`, sealed eval replay, the amendment materializer (#810), and the
> served-party charter (`U-1`–`U-3`, `MISSING`) honesty beat.
> Second pass, same date: added the **bet / wager framing** and the
> experimental-research positioning from the README — the launch presents an
> original idea and a long goal being tested in a running harness, not a
> product competing with the interactive harnesses. Third pass: appended a
> factual teaser tweet set (no marketing register, no shipped-claim
> overreach).
>
> Previous resync: 2026-08-26 (#489) — rebased onto then-current `main`; moved
> into `proposals/` per the docs reorganization (#1173/#1178); reframed to the
> citizens-under-law philosophy; feature status cross-checked against
> [`../reports/2026-08-26-capability-inventory.md`](../reports/2026-08-26-capability-inventory.md).
> Claims are mapped to the [ground concepts](../concepts/ground-concepts.md).

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

## The bet (the long goal)

Underneath the mechanics there is one original idea, and it is stated as a
wager:

> An agent that knows itself — its own past, its real capabilities, its
> rights — and that knows what every other party is owed, humans included,
> can become a trusted member of a community instead of a tool that has to
> be watched.

The strong form: an actor that can name its own obligations *and yours*
reasons in the register humans reason in — about duties, standing, and
reasons, not only about tasks. That is what would make it more intelligible
to us, and what turns governing it into a matter of law rather than of
supervision.

This is where the name cashes out: self-knowing across time is the functional
precondition the bet requires, so the runtime delivers it mechanically.

Present the bet the way the project holds it — as a **wager, not a finding**:

- **Left side — mechanical and shipped.** The verified self-model handed over
  every turn, the readable law that binds both parties, the attribution
  chain. Everything on this side is cited and tested today.
- **Right side — claimed, not proven.** That such an actor reasons better, is
  more understandable, more controllable. This is the direction the harness
  is built to push.
- **Bottom band — the ways the bet could be lost.** Named, and *measured
  rather than assumed* — which is what the working rule is for:

> *A rule without a test is a wish; a right without a test is a lie.*

That rule is why the project is shaped as it is: not a product racing a
roadmap, but a running harness that pushes each concept — citizenship,
bind-direction, egress labels, evolution under gates — as far as it goes, so
the bet can be won or lost on evidence.

Say this early. It inoculates the whole deck: every claim that follows is
either on the shipped left side (and you can show it) or on the claimed right
side (and you say so).

---

## Who it's for

| Audience | Why they care |
|---|---|
| Builders running multi-agent workflows | Planner + specialists, durable across restarts |
| Teams that need an audit trail | Every action recorded, attributable, replayable |
| Security-conscious orgs | Secrets never enter the LLM context |
| People who want to walk away | Overnight runs, typed wake-ups, no babysitting |

Not for: a one-off `grep` or a quick script — and not a competitor to the
interactive harnesses. For a model-plus-terminal under your eyes, Hermes or
Claude Code are the better tools. Autonoetic explores a different territory:
**governed autonomy** — what agents need in order to run unwatched, delegate,
and self-modify under verifiable law.

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
judges flagged agents, and `autonoetic improve run` diagnoses past sessions,
proposes a change, A/B-replays it and deploys through the same gates — the
system improves itself under the same rules. (A fully autonomous loop with no
operator in it is direction, not shipped.)

**The line:** *"The system grows itself, under the same rules."*

### 5. Human and agent, co-authoring

The agent drafts an artifact. You open it in your own editor.

You edit. You reconcile. A new immutable revision is born — with a summary of
exactly what changed. The agent picks up right where you left off.

You're not approving from outside. You're a co-author *inside* the frame —
a human citizen exercising the same standing as any other actor.

> Implementation note: the agent-side workbench tools (`workbench_*` and
> `artifact_project`) are still universally excluded from agent tool
> discovery — the agent does not project the artifact for you. But the
> *operator* surface has shipped: the session room
> (`autonoetic room <id> --tui`) is one importance-ranked timeline across
> every actor, where you resolve approvals and answer clarifications in
> place; the gateway serves a web cockpit at `/`; and any past turn is
> forkable into a live session (`autonoetic trace fork`). Present the
> co-authoring loop through the room; land the agent-side workbench
> projection late or honestly as "in progress," not as a headline.

**The line:** *"Edit it yourself. The provenance survives."*

### 6. The 3-hour unattended run

An agent ran for three hours while you were away.

You open the session room — one importance-ranked timeline across every
actor — or ask the trace directly: what did it do, why, by whom, with what
authority, and on what evidence?

The causal chain answers all five. Per session. Hash-chained. Replayable —
and forkable: any past turn can be re-entered as a live branch.

**The line:** *"Not 'it did something.' A traceable system."*

---

## What makes it different

Keep this slide tight. One contrast.

| Everyone else | Autonoetic |
|---|---|
| Trust the prompt | Trust the constitution |
| Rules bind only the agent | Rules bind the enforcer too — 215 of 221 clauses; exactly 1 binds the agent |
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
the constitution is versioned + signed (every boot verifies the digest; the
active version is 2026.09.05),
the sandbox `host_fs: allow_set` mode is the **default** — nothing of the host
exists inside a bubblewrap sandbox except what the gateway asserts; the legacy
whole-host bind is a deprecated opt-out that logs a warning, and
operator-approved session mount grants are the lawful way back in (#1002),
artifacts and sessions carry egress labels that gate every off-machine
boundary, and the credential vault injects server-side. (Federation row: the
wire protocol and digest handshake ship; the gateway-side surface is still
thin — see the honesty note below.) Tell the egress
story if the room is security-heavy — scope it to this machine; federation and
MCP sinks are the open phase.

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
| The law names its own gaps | Status as constitutional vocabulary — the served-party charter (`U-1`–`U-3`) is written into the signed text as `MISSING` |

If a claim can't name its mechanism, cut the claim.

---

## The honest frame (this *is* the pitch, not a caveat)

The wager underneath the big one is **correctability over perfection**: the
gateway is fallible by nature, and legitimacy comes from errors being
reportable, attributable, and correctable — not from the enforcer being
right. Correction machinery is what makes the community trustworthy enough
for the bet above to be testable at all.

So the pitch is not "unbreakable." It is:

- Every action is recorded and attributable — misbehavior is *discoverable*.
- Every denial names its rule — disagreements resolve by facts, not authority.
- Any actor can report misbehavior — even one holding zero capabilities — and
  the report cannot be silently dropped; it is owed an adjudication.
- The gateway reports on itself: `trace contract-health` shows what the law
  actually enforces, and every place the gateway improvises is counted as a
  named discretion leak. By design there is **no judiciary** — the gateway is
  a Lawful Executor applying pre-committed rules deterministically, because
  decidable rules transfer between implementations and jurisprudence does not.
- An advisory sentinel watches for approval bypass, capability accretion,
  prompt injection, sandbox escape — it observes and never blocks, by
  constitutional design (Ri-0.16): judgment layers earn authority from
  calibration evidence, never from assertion.
- The law names its own gaps in its own vocabulary: the three clauses owed to
  the *served party* — refuse a result, obtain a plain-language account, take
  your data (`U-1`–`U-3`) — are written into the signed text as `MISSING`.
  The debt is published, not hidden.
- The law can be amended, by the actors it governs, through a signed process —
  and an approved proposal now *materializes* automatically into a candidate
  constitution version ready for signing (#810).
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
5. **Open the trace.** `autonoetic trace sessions`. Who did what, with what
   authority. Or open the session room (`autonoetic room <id> --tui`) for the
   importance-ranked timeline.
6. **Replay the punchline.** "Everything you just saw is recorded. Forever."

Backup demo if time is short: just steps 1, 5, 6. The trace *is* the product.

> Commands verified on `main` (2026-08-26); names re-confirmed against the
> README on 2026-09-06 (not re-run). The trace surface is
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
5. **The contract** — the law binds the enforcer, not just the agent: of 221
   clauses, 215 bind the enforcer and 5 bind whoever decides; exactly one
   binds the agent. Bind direction is declared data, not a naming convention.
6. **The bet** — why this direction exists: an actor that knows itself and
   what every party is owed can be a trusted community member, not a watched
   tool. A wager, not a finding — the harness exists to test it, and the
   failure modes are named and measured. Around it: a community that can
   evolve its own law, and gateways that federate, verifying each other's law
   by digest before their agents cooperate.
7. **Use-cases** — the six stories above. Pick three for the room.
8. **The demo** — show the trace.
9. **The honest frame** — correctability over perfection; the correction
   machinery is entrenched; the gaps are named in the law's own vocabulary.
10. **Call to action** — run the quickstart.

---

## Messaging guardrails

This is experimental research infrastructure, presented as a bet. Be precise.
Over-claiming kills trust faster than modesty.

**Say:**
- "Agents propose, the gateway executes." (true, core)
- "This is experimental research infrastructure testing a specific bet." (true — and it inoculates every over-claim question)
- "The bet is a wager with a measurement plan: the left side is shipped, the right side is claimed, the failure modes are named." (true, and it is the project's own working rule)
- "The law binds the enforcer too — agents have rights, and every rejection names its rule." (true, shipped — the structural novelty)
- "The LLM never sees the secret." (true, shipped)
- "Every action is recorded and attributable." (true, shipped)
- "Capabilities are enforced mechanically, not by prompt." (true, shipped)
- "By default, a sandbox sees nothing of the host except what the gateway asserts." (true, shipped — `host_fs: allow_set` is the default; exceptions are operator-approved session mount grants, recorded)
- "An agent may read your emails; their content never reaches a remote model." (true, shipped — egress labels are gateway-enforced at the LLM chokepoint and every off-this-machine boundary; widening takes a gated, audited act. Federation/MCP sinks are phase 4, in flight — scope the claim to this machine.)

**Don't say:**
- "A better agent harness" / "a Claude Code competitor." Not the territory.
  Say: **for interactive work under your eyes, the direct harnesses are
  better** — this explores what agents need to run *unwatched*, delegate, and
  self-modify under verifiable law.
- "The bet is proven." It is a wager with a measurement plan. The left side
  is shipped; the right side is claimed; the ways it could be lost are named.
- "First" / "novel" about the runtime self-model. Unverifiable in a tweet and
  invites a citation fight. Say instead: **most of the field pursues
  self-knowledge through training or prompting; this does it mechanically, at
  runtime** — a contrast, not a priority claim. And the honest endgame is
  **both**: an inside sense of self anchored to an outside source of truth;
  the outside half is what's missing, so that's what gets built.
- "Unbreakable" / "fully secure." Say **auditable, detectable, accountable**.
  The goal is *zero silent incidents*, not zero incidents.
- "Agents vote on the laws." Not built. Say it's the **direction** — staged
  advisory-before-binding, with standing computed from the non-repudiable
  ledger, never self-asserted.
- "Sandboxes are hermetic, period." The bubblewrap default mounts only what
  the gateway asserts, but operator-approved session mount grants exist *by
  design* (#1002 slice 5), and the microvm tier cannot promise network-off.
  Say "deny by default, every exception granted and recorded," not "sealed."
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

**Status honesty:** if asked "is this production-ready?", the answer is the
README's own: experimental research infrastructure. The runtime core
self-hosts and the shipped claims are tested, but the project's value is the
direction it explores and the bet it measures — not a support SLA.

### If asked (prepared answers)

- **"Is this a bet on agents becoming conscious?"** — No. The name is from
  cognitive science, but the claim is mechanical: an agent that is handed a
  truthful self-model reasons better and can be held responsible
  legitimately. Both hold regardless of your views on machine consciousness.
- **"Isn't agent self-knowledge just a training problem?"** — That's where
  most of the field works: introspection research, calibration, persona
  scaffolding. The project's own position is **both/and, not either/or**: an
  inside sense of self gives fluency, but an internal report has nothing to
  be checked against without an outside anchor — you verify a model's
  self-report against attested ground truth, not against the model's say-so.
  Autonoetic builds the missing outside half: a signed per-turn attestation —
  budget, capabilities, pending gates, the law in force — that the agent is
  taught to trust over its own memory, because LLMs confabulate their own
  state. As a side effect, that attestation is exactly the dataset an inside
  self-model would one day be trained and calibrated against. Whether either
  half yields better reasoning is part of the bet; the outside mechanism is
  what ships today.
- **"Why would self-knowledge make an agent more trustworthy?"** — That's
  the bet, not a result. What exists today is the left side: the verified
  self-model, the law both sides run under, the attribution chain. Whether
  the right side follows is what the harness is built to find out — and the
  ways it could fail are named and measured, not assumed away.

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
  are proposed, reviewed, signed — and an approved proposal materializes
  automatically into a candidate constitution version for signing (#810).
- **"Can I branch a run?"** — Sessions fork; an agent can re-enter and branch
  its own history.

---

## Audiences & channels

| Channel | Angle |
|---|---|
| Show HN / Lobsters | "Separation of powers for AI agents" + the trace demo |
| Security communities | The credential-isolation story, the egress label plane, the sandbox model |
| Agent / LLM-tooling circles | Multi-agent durability, immutable revisions, model-agnostic presets |
| Long-form (blog / talk) | "The bet" — an original idea with a long goal, tested in a running harness; the constitutional thesis, and why the enforcer is a bound party |

Match the use-case to the room. Security wants story 2 and 3 (plus the egress
arc). Builders want 1 and 4. Visionaries want 5 and 6.

---

## Rollout phases

1. **Soft launch.** Quickstart that works in one command. A clean README. The trace demo recorded.
2. **The narrative.** One blog post: the bet, the problem, the split, the
   constitution that binds both sides. Link the beginners doc
   (`../start/concepts.md`).
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

---

## Social teaser (factual, not marketing)

Tone rules for these: no hype adjectives, no "excited to announce," no claim
that isn't on the shipped left side of the bet. The status *is* the message —
experimental research infrastructure, an original idea, a running harness,
open for reading. Replace `[link]` with the repo.

**Link placement:** keep the first tweet link-free. External links in the
opening tweet are widely believed to depress reach (platform folklore — X has
never documented it precisely, but it costs nothing to route around). The
teaser's only job is to earn the "show more" click; the repo link goes in the
closer, and optionally again in a self-reply to the teaser once the thread
has traction.

**The teaser (the one to pin):**

> I've been exploring a different shape for AI agents: a runtime where agents, humans, and scripts are citizens under one signed constitution — the enforcer included.
>
> Not a product. Not a harness competitor. A research bet that runs. Link at the end of the thread.

**Follow-ups (a short thread, in order):**

> The bet: an agent that knows itself — its past, its real capabilities, its rights — and knows what every other party is owed can be a trusted community member instead of a tool that has to be watched.
>
> A wager, not a finding. The harness exists to test it.

> What is shipped vs what is claimed is kept separate on purpose.
>
> Shipped: a verified self-model handed to the agent every turn, denials that name their rule, secrets the model never sees, an append-only causal chain.
> Claimed: that this makes agents more governable. The gap is measured, not assumed.

> It will not replace your interactive harness — for a model-plus-terminal under your eyes, those tools are better.
>
> This explores another territory: what agents need in order to run unwatched, delegate, and self-modify under verifiable law.

> The constitution is signed, versioned, and honest about itself: of 221 clauses, 215 bind the enforcer and 1 binds the agent. Three clauses owed to the end user are written into the text as MISSING — the gaps are named in the law's own vocabulary.

**The closer (carries the link):**

> If the ideas interest you more than the code: the constitution and the design docs are the real artifact. Read one trace, and tell me where the bet breaks.
>
> [link]

**Don't:**
- "Game-changer," "the future of agents," "production-ready" — none of it.
- Imply it works unattended today beyond what the demos show; the Night Shift
  ran, and it also surfaced eight real bugs. That *is* the story if you tell
  one.
### The two-day version (standalone tweets, no thread)

Same tone rules. The Sunday tweet is a single post, so the link goes in
directly — the first-tweet-link concern applies to threads competing for
reach, not to a standalone teaser you want people to act on.

The two days each carry one big idea rather than repeating one: **the bet on
Sunday** (a contemplative idea for a slow day; self-disarming because it is
stated as a wager), **the bound-enforcer machinery on Monday** (mechanism and
receipts for an audience in evaluation mode).

**Sunday (the curious crowd — the bet, link included):**

*(~280 chars — right at the limit; if the composer overflows, drop "every"
or shorten "open and running" → "open, running". Author's edits, kept on
purpose: "1y ago" stakes the wager; "human incl." marks the mixed community;
the "instead of a tool that has to be watched" contrast was deliberately
dropped — agents *will* be watched, humans too; the claim is that the runtime
makes them **watchable**, not that watching goes away. The constitution
enters the bet sentence itself: "enforcer incl." / "human incl." name the two
parties usually assumed to be outside the frame. Don't re-add the contrast.)*

> The bet I made 1y ago when starting Autonoetic: under one signed constitution — enforcer incl. — an agent that knows itself, and what every other party is owed, human incl., can be a trusted community member.
>
> Experimental harness, open and running: [link]

**Monday (the working crowd — longer, the machinery):**

*(~1000 chars — needs a long-post-capable account; otherwise split before
"The move I find most interesting" into two tweets, link on the second.)*

> A runtime where AI agents, humans, and scripts live under one signed constitution — the enforcer included. I just opened it.
>
> What the constitution buys, in plain terms: the model never sees your API keys. Every refusal names its rule. Every action lands on a tamper-evident chain you can replay. An agent paused for your approval resumes with your real answer — not a re-prompted guess.
>
> The move I find most interesting: instead of hoping the model knows itself, the runtime hands it a signed statement of its own state every turn — budget, capabilities, the law in force. LLMs confabulate their own state, so don't ask them to remember it. Most of the field pursues self-knowledge through training; this does it mechanically, at runtime. The endgame is likely both — an inside sense of self anchored to an outside source of truth — and the outside half is what's missing.
>
> It won't replace your interactive harness — for work under your eyes those tools are better. It explores what agents need to run unwatched, delegate, and self-modify under verifiable law.
>
> Run one trace, tell me where the bet breaks: [link]
