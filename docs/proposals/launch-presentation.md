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
> but opt-in, not the default).

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
| Rules bind only the agent | Rules bind the agent; rights bind the gateway |
| Secrets in context | Secrets gateway-owned |
| Chat history | Immutable causal chain |
| One agent, one model | Many agents, model-agnostic presets |
| Built for chat | Built for unattended, multi-agent work |

Since June, *all six* headline claims above are shipped and tested:
the constitution is versioned + signed (every boot verifies the digest),
the sandbox `host_fs: allow_set` mode mounts only what the gateway asserts
(opt-in today; the default flips after the deprecation window, #1002),
artifacts and sessions carry egress labels that gate every off-machine
boundary, and the credential vault injects server-side. Tell the egress
story if the room is security-heavy; it is the newest complete arc.

The deeper idea — lead with it, don't save it for the Q&A:

> Actors — AI, human, or script — are first-class citizens under one
> constitution. Same rights. Same rules. They trust each other because they
> trust the law, not the prompt.

---

## The honest frame (this *is* the pitch, not a caveat)

Autonoetic's founding bet is **correctability over perfection**: the gateway
is fallible by nature, and legitimacy comes from errors being reportable,
attributable, and correctable — not from the enforcer being right.

So the pitch is not "unbreakable." It is:

- Every action is recorded and attributable — misbehavior is *discoverable*.
- Every denial names its rule — disagreements resolve by facts, not authority.
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

## The launch narrative (slide beats)

1. **Hook** — agents are fast and forgettable.
2. **Problem** — power without shared rules.
3. **Reframe** — it's not about caging AI. It's about law that binds everyone.
4. **The split** — agents propose, the gateway executes.
5. **The contract** — rules bind the agent; rights bind the gateway.
6. **Use-cases** — the six stories above. Pick three for the room.
7. **The demo** — show the trace.
8. **The honest frame** — correctability over perfection; the correction
   machinery is entrenched.
9. **The bigger idea** — actors as citizens; a community that can evolve its
   own law, with the people it serves always able to say no.
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
- "Labels follow the content: a private artifact can't leave the machine through any boundary without a gated, audited act." (true, shipped as egress label plane)

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
