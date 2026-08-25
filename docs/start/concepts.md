# Autonoetic for Beginners

This document introduces Autonoetic from first principles. It is written for
people who are new to the project, new to agent runtimes, or trying to
understand why Autonoetic is built the way it is — especially people coming from
Hermes, OpenClaw, or similar direct-code assistants.

## What the name means

*Autonoetic* means **self-knowing**. The term comes from cognitive science —
Endel Tulving's name for the capacity to revisit one's own past and project
oneself into one's own future. The whole project rests on one bet: an
actor that knows its own past, present, capabilities, and rights — and that acts
under a shared law every other actor also follows — can be trusted to work on
its own. (Autonoetic makes no claim about machine *consciousness*; the claim is
functional — see §6.5.)

That "actor" need not be an AI. It might be an AI agent, a human operator, or an
automated script. Autonoetic treats all three as **first-class citizens**: each
has a portable identity, each acts under the same constitution, each is owed the
same rights, and each is held to the same obligations. The goal is not to cage a
dangerous AI — it is to give *any* participant, silicon or carbon or cron job, a
common set of rights and rules so they can interact and trust one another
without a human refereeing every step.

The short version:

> Autonoetic is a runtime where actors are **first-class citizens under a shared
> constitution**. The constitution is not a leash — it is the common law that
> lets independent actors (AI, human, or script) cooperate, delegate, and trust
> each other mechanically rather than personally. Each actor reasons and decides
> within the law; the gateway is the neutral institution that enforces it.

### A familiar analogy: rule of law

If you have seen how a constitutional government works, you already understand
Autonoetic's shape:

| Government | Autonoetic |
|---|---|
| The constitution & laws | The signed, versioned constitution (rights + rules) |
| The executive that enforces law but does not invent it | The **gateway** — a *Lawful Executor* |
| Citizens — free within the law, with rights *and* duties | **Actors** — AI agents, human operators, scripts |
| The public record | The **causal chain** |
| Amending the constitution | The amendment process (proposed, reviewed, ratified, signed) |

The contrast with other tools is the contrast between **trusting a person** and
**trusting an institution**. With a direct assistant you trust whoever wrote the
prompt. In Autonoetic you trust the constitution — and because every actor is
bound by it, the actors can trust each other too. That shift, from personal
trust to lawful trust, is what turns a tool into an ecosystem.

Autonoetic is not trying to be the fastest way to give an LLM a terminal. It is
designed for actors that work **independently** — spawning helpers, delegating
work, sleeping, waking, using credentials they never see, and collaborating
under a common frame that makes trust mechanical rather than personal.

---

## 1. When you need this (and when you don't)

You need Autonoetic when the thing you care about is not "how fast can an agent
type code" but "can I trust this agent to run for hours without me and still
know exactly what happened."

Concrete scenarios:

- **Overnight builds.** You want an agent to work while you sleep: plan,
  delegate to coders, run tests, evaluate results, and install the output — all
  without you clicking approve every 30 seconds, but without giving it unlimited
  power.

- **Multi-agent collaboration.** You want a planner that coordinates a coder, a
  tester, and an auditor — and you want the coder to be literally incapable of
  accessing the network because its role doesn't need it.

- **Credential use without exposure.** Your agent uses GitHub, AWS, or API
  tokens — but the LLM never sees the raw secret. The gateway injects it into
  the sandbox at execution time. If the agent hallucinates or gets
  prompt-injected, your key is not in the conversation log.

- **Agents building agents.** An agent should be able to build, test, and
  install another agent — but the new agent's capabilities must be declared,
  bounded, and gated before it can run. No silent privilege escalation.

- **Audit.** An agent ran for 3 hours while you were away. You want to know
  exactly what it did, which tool calls it made, which approvals were granted,
  which agent spawned which, and which artifact revision was promoted.

You don't need Autonoetic when all you want is a quick chat assistant that runs
one-off bash commands — use Hermes, OpenClaw, or OpenCode for that. Autonoetic
is for when you want **governed autonomy**, not convenience.

---

## 2. The central mechanism: agents propose, gateway executes

Autonoetic splits every action into two actors.

```text
Agent                                Gateway
-----                                -------
Reads instructions                   Owns resources
Reasons about goals                  Checks policy
Plans next steps                     Executes tools
Proposes actions       ───────▶      Manages files/network/secrets
Receives results       ◀───────      Logs what happened
```

An Autonoetic agent is a **low-privilege reasoner**. It can think, plan, ask
questions, delegate, and propose tool calls.

The **gateway** is the **high-privilege executor**. It decides whether a
proposed action is allowed, performs it if allowed, records the result, and
enforces rules the agent cannot override.

This means an agent does not "have network access" in the ordinary sense. It has
permission to **ask the gateway** to perform network-related tools under
specific rules.

That distinction is the whole project. It is **separation of powers**, not
distrust of AI: the same split — propose here, enforce there — would apply to a
human or a script acting through the runtime. No single actor is both the one
who decides what to do and the one who unilaterally does it. That is what makes
the result trustworthy regardless of who, or what, is reasoning.

Concretely, here is what happens when you ask Autonoetic to **"build a weather
agent"**:

```text
User: "Build me a weather agent"

planner.default → spawns researcher ("find weather APIs")
                → spawns coder ("write the integration")
                → spawns unit_test_runner ("test it in a no-network sandbox")
                → spawns auditor ("review governance and risk")

When coder needs a GitHub token:
  ❌ coder does NOT get the token in its prompt
  ✅ gateway injects GITHUB_TOKEN into the sandbox — coder sees only the result

When static_evaluator reports "this artifact calls api.weather.com":
  ⏸ approval gate opens → you see "weather agent wants to call weather.com"
  ✅ you approve → gateway permits that specific artifact + host combination

When unit_test_runner passes:
  ✅ promotion gate — auditor verdict becomes evidence bound to the artifact digest

Result: a weather agent installed by agents, reviewed by agents, with:
  - no API key ever seen by an LLM
  - network access scoped to exactly the hosts the evaluator detected
  - a full audit trail of who built what, who tested what, who approved what
```

---

## 3. The problem this solves

Why go through all this? Because useful work requires real powers:

- Read and write files.
- Run code.
- Access the network.
- Store memories.
- Use credentials.
- Spawn other agents.
- Install new tools or agents.
- Keep working in the background.

The risk here is not "AI." It is **power without shared rules**. A human can
fat-finger `rm -rf`, a script can loop on a bug, an LLM can be confused or
prompt-injected — and in every case the result is an effect nobody intended and
that no one can cleanly attribute afterward. Making an actor's own reasoning the
final authority over shared resources does not fix this; it just moves the
single point of failure around. Humans are not automatically safer than agents,
and agents are not automatically more dangerous than humans — what matters is
that *whoever* acts does so under rules everyone shares and a record everyone
can read.

Autonoetic's core question:

> How can any actor be autonomous without making its own reasoning the final
> authority over shared resources?

The answer is not "add more safety checks." Autonoetic defines a shared
constitutional frame first — the rights and rules every actor holds in common —
then builds the runtime around it. Actors propose; the gateway, bound by the
same frame, decides.

---

## You're probably thinking…

If you come from a direct-code assistant, a few objections are natural. They are
worth answering head-on.

**"Isn't the constitution just a fancy system prompt?"**
No. A prompt rule lives inside the model's context — it can be forgotten,
out-reasoned, or overwritten by a prompt-injection. A constitutional rule lives
in the gateway, *outside* any actor's reasoning, and every tool call passes
through it. A prompt asks nicely; the constitution is checked mechanically. (See
§6.2.)

**"Isn't this just sandboxing plus audit logs?"**
Those are table stakes, and Autonoetic has them — but they are the floor, not
the idea. The new part is **portable identity, rights, and actor-to-actor
trust**: a coder agent can hand work to a tester agent, or a human can hand work
to an agent, and each side knows the other is bound by the same law without
having to inspect the other's internals. Sandboxes isolate; a constitution lets
isolated actors *cooperate*.

**"What happens when the gateway itself gets it wrong?"**
It will — the gateway is fallible by design assumption, and the project does
not pretend otherwise. Legitimacy here comes not from the enforcer being
right but from errors being *reportable, attributable, and correctable*: the
enforcement register tracks which clauses are actually enforced versus merely
declared; the gateway's own lapses are named debts (**DISCRETION LEAKs**),
not accepted behaviour; any capable agent may durably propose amendments; and
the clauses that make correction possible (read your own history, every
denial names its rule, non-repudiable attribution, the right to propose) are
*entrenched* — amendable only to be strengthened, never weakened. The wager:
an imperfect enforcer plus entrenched correction machinery beats a perfect
enforcer that cannot be corrected. (See `docs/concepts/philosophy.md` §3.1.)

**"If agents and humans are equal citizens, who is actually in charge?"**
Humans. Equality is about *interaction* — same rights, same obligations, same
record — not about *sovereignty*. Humans ratify amendments and are the final
escalation path; an agent that cannot resolve a gate escalates to a human rather
than deciding unilaterally. (See §5, "Citizens of any kind.")

**"Won't all this structure just slow me down?"**
For a one-off `grep` or a quick script, yes — and for that you should use a
direct assistant. The structure earns its keep when you are *not watching*:
overnight runs, teams of agents, work that must survive a restart and be
explainable afterward. You trade immediacy for the ability to walk away. (See
§16.)

---

## 4. Designed for agents first

This is the deepest difference between Autonoetic and systems like Hermes or
OpenClaw.

Hermes and OpenClaw are designed for **human-centered chat**. You type a
message, the assistant replies. The assistant is your tool. Multi-agent
collaboration, durable workflows, and unattended execution are afterthoughts at
best.

Autonoetic is designed for **agents that run independently**. The infrastructure
exists so agents can:

- Spawn each other and delegate work
- Sleep and wake across hours or days
- Receive typed notifications when children complete (no polling)
- Inspect their own rights and capabilities (`self_describe`)
- Trust other agents because all parties operate under the same constitution
- Install agents built by other agents, with mechanical capability gating
- Act as gate deciders — agents with `GateDecider` capability can approve or
  reject approval gates, subject to the same hardening rules as human operators

Here is how the same task plays out differently:

| | Hermes / OpenClaw | Autonoetic |
|---|---|---|
| Multi-agent | You manually route between agents | Planner spawns specialists; they coordinate |
| Credentials | In the prompt, seen by the LLM | Gateway-injected, LLM never sees them |
| Unattended work | Not designed for it | Workflows persist across restarts |
| Audit trail | Chat history | Causal chain: who did what, with which authority |
| Agent install | CLI only (manual) | Agents can install agents (capability-gated) |
| Gate approval | Human only | Human or agent (GateDecider capability); agents escalate to a human when unsure |
| Trust model | Trust the prompt writer | Trust the constitution — mechanically enforced |

The constitution isn't there to restrict **you**. It's there so **agents can
trust each other** without you mediating every interaction. That's the
difference between a tool and an ecosystem.

Notably, "operator" in Autonoetic is a **role**, not necessarily a human. The
constitution names three kinds of deciders: human operators, autonomous reviewer
agents, and policy engines. Each decision is recorded with the decider's
identity on the causal chain. But the human remains the ultimate escalation
path — an agent-decider that cannot resolve a gate must escalate to a human
rather than reject.

---

## 5. The constitution: freedom inside a common frame

The constitution is the deepest Autonoetic concept. It is not merely a security
policy — it is the shared law under which every agent operates.

The current constitution describes agents as **free, responsible, and
cooperative**:

- **Free** — agents reason, decide, act, and evolve within their declared
  capabilities. Anything not forbidden by the constitution and policy is part
  of the lawful action space.
- **Responsible** — every action is attributable, budgeted, and audited. Freedom
  comes with provenance.
- **Cooperative** — agents can trust other agents because all parties operate
  under the same constitutional frame, even when they do not know each other's
  internals.

This is what makes Autonoetic more than a tool runner. The runtime is an
ecosystem where agents can work together, create artifacts, evolve other agents,
and propose changes to the law itself.

### Citizens of any kind

Crucially, "agent" in the paragraphs above is shorthand for **actor**, and an
actor is not necessarily an AI. The constitution names several kinds of
deciders — human operators, autonomous reviewer agents, and policy engines — and
treats them under one frame. A human who approves a gate and an agent who
approves a gate are both *deciders*; each decision is attributed to its decider's
identity on the same causal chain, under the same hardening rules. The
constitution is the **protocol by which heterogeneous actors interact**, not a
containment fence around the AI.

This symmetry is the point — and it is no longer only an aspiration. It would
be incoherent to demand that an agent give a reason for every rejection while
letting a human reject silently, or to audit an agent's every move while a
script's actions vanish without a trace. So the constitution now binds
**deciders** the way it binds agents: the decider-obligations section (`O-*`)
is the deliberate, symmetric counterpart to the Bill of Rights. A decider who
rejects, or who approves an elevated or irreversible action, **owes a recorded
motivation** before the decision commits (`O-1`), and every decision is
attributed to the deciding principal — id and kind — on the causal chain
(`O-2`). Silent rejection by a human is held as illegitimate as a gateway
"denied" with no rule cited. The gateway enforces this mechanically, against
whoever sits in the decider seat.

That seat is **occupant-agnostic by construction**. The gateway classifies every
principal as `Human`, `AutonoeticAgent`, or `Script`, and — beyond a display
marker — it cannot tell a human decider from an AI decider. A human fills the
operator seat today; an AI could fill it tomorrow. The obligations travel with
the seat, not with the species.

The symmetry is also still being built out, honestly. The first obligations —
a motivation for the decision, attribution of who decided, a recorded decision
owed on every amendment proposal (`O-6`) — are enacted today. Others are
written down as proposed clauses and become binding as each is made
mechanically checkable: discipline against rubber-stamping under approval
fatigue, honesty about the *scope* of what was approved, and a duty to escalate
rather than reject under uncertainty. The direction is fixed even where the
mechanism is not yet complete: **one set of rights and one set of obligations,
binding whoever acts.**

One honest asymmetry remains, and it mirrors real constitutional democracies:
citizens are equal *before* the law, but **the people are sovereign over the
law**. In Autonoetic, actors have equal civic standing in day-to-day
interaction, but humans retain **constituent authority** — they ratify
amendments, and they are the ultimate escalation path. An agent-decider that
cannot resolve a gate must escalate to a human rather than reject. Equal footing
in the interaction; human sovereignty over the frame itself.

Two more points of honesty about power, both from the project's founding
commitments (`docs/concepts/philosophy.md` §3):

- **Today's concentration of power in the operator is a starting condition,
  not the design.** The decider is defined as a *seat* with duties attached,
  independent of who occupies it — the `GateDecider` capability already lets
  an agent occupy it under the same obligations as a human. Spreading
  decision power to agents is intended to be a parameter change, not a
  re-architecture, gated on evidence: agents vote advisorily first, and
  weight is computed from their non-repudiable track record, never asserted.
- **The community is not sovereign over the people it serves.** Autonoetic is
  closer to a chartered profession (medicine, law) than to a state:
  internally self-governing, externally accountable to end-users whose right
  to refuse a result sits *outside* the community's own rules.

### Rules, rights, and obligations

The constitution's structural discipline is that **every clause binds exactly
one party**:

| Direction | Meaning | Binds |
|---|---|---|
| **Rules / principles** (`P-*`) | A finite, named set of forbidden or constrained actions. Everything else is permitted. | Agents |
| **Rights** (`Ri-*`) | Unconditional entitlements the enforcer owes every agent, revocable only by amendment. | Gateway |
| **Obligations** (`O-*`) | Duties owed by whoever exercises authority over an agent — e.g. a decider owes a motivated decision, never a silent rejection. | Deciders |
| **Service charter** (`U-*`) | What the community owes the *served party* — the end-user, human or not: refuse a result, audit what was done on their behalf, exit with their data. | The community |

Making the enforcer and the decider bound parties — not just the agent — is
what turns a compliance regime into a **social contract**. A right is not a
favour; it is what makes the rules *legitimate* rather than merely effective.
The fourth row is the frontier: the served party's rights are written into the
law but not yet mechanically honored — declared now, while cheap, because they
must become *real* before decision power spreads further inward (see "Toward a
community" below).

In most systems, agents are **subjects of restrictions**. In Autonoetic, agents
are **holders of rights**. This changes agent behavior fundamentally: an agent
that knows it has a right to be told why it was rejected can retry
intelligently. An agent that knows parents receive typed wake-ups on child
completion can yield instead of polling.

Concrete examples of rights:

- An agent can inspect its own active capabilities, budget state, pending gates,
  and session lineage — via `self_describe`.
- An agent can read the constitution it operates under.
- Rejections must name the rule or principle that caused them — the agent gets a
  reason, not silence.
- Sessions can terminate only for a declared closed list of reasons — agents are
  not killed arbitrarily.
- Parents should be woken with typed child-state notifications rather than
  forced to poll — this alone saves hundreds of wasted LLM turns.
- Agents with the right capability can propose constitutional amendments.

The constitution is not just "how the gateway says no." It is how agents know
what they are **owed**.

### The Lawful Executor

The gateway is best understood as a **Lawful Executor**.

It does not improvise judgment at runtime. It checks proposed actions against
pre-committed law, declared capabilities, manifests, operator decisions, and
configuration. Then it permits, rejects, suspends, or resumes according to that
frame.

```text
The agent owns meaning, intent, plans, and judgment.
The gateway owns deterministic enforcement of the shared law.
```

When the gateway starts inventing policy, it overreaches. When an agent tries to
bypass law, it violates the frame. The constitution defines the boundary so both
sides can trust it.

### Evolution of the frame

Autonoetic treats constitutional change as a first-class process. Agents may
propose amendments through declared channels. Humans review and ratify changes.
Tests pin invariants. The constitution is versioned, digest-pinned, and signed —
and the digest is checked in the federation handshake, so a peer gateway can
verify *which law* you operate under before cooperating with you. A running
session likewise pins the version and digest of the law it was admitted under,
and if the constitution changes mid-session the agent is notified of the
drift — nobody is silently held to a different law than the one they started
under. Two limits keep change honest in both directions: the correction
machinery itself (read your own history, named denials, the right to propose,
non-repudiation) is entrenched and may only be strengthened by amendment; and a
system that could change its rules without constraint would have no rules —
the middle path is the point.

That matters: agents can improve themselves and one another while knowing the
common frame can evolve deliberately rather than silently drifting.

### Toward a community

Step back and the shape of the ambition becomes visible. Autonoetic is meant to
grow into a **community of entities that supply skills and work** — some human,
some AI, some plain scripts — all operating under one constitution, all owed the
same rights by the gateway, all bound by the same rules it enforces. Inside that
shared, well-known frame an entity does not have to vet another's internals
before working with it: it trusts the frame, and the frame makes the other's
powers and obligations legible. That common trust is precisely what lets
independent actors evolve *freely* — propose plans, build artifacts, spawn
helpers, hand work back and forth — without a human refereeing every exchange.

The frame is not a claim to have foreseen every failure. Rules do not, and
cannot, cover every possible security or behavioral problem. What they provide
is a common ground for trust and a record that makes misbehavior
**discoverable** — a malevolent or simply broken actor is caught sooner or
later, because the other actors are sensors and every action is attributable
(see §15). And when a genuine gap *is* found, the answer is not to abandon the
frame but to **amend it**: the caveat becomes a proposed clause, reviewed and
ratified, and the common law gets sharper.

This is no longer only a direction — the first civic machinery is built and
running:

- **Any agent can propose an amendment** (Ri-0.8): the proposal gets a durable
  ID and enters a review queue rather than being silently dropped, and a
  review authority owes every proposal a *recorded decision* (`O-6`).
- **Any agent can report an anomaly** through a capability-free `anomaly_flag`
  tool — a right to report is not much of a right if it needs a permission.
  Flags are durable, attributed, and owed an adjudication; a scheduled sweep
  stamps overdue ones with an SLA breach. A breach marks, it does not resolve —
  the decision stays owed.
- **Civic behavior is measured, not hoped for.** A built-in eval suite
  (`civic-core-v1`) scores how agents respond to denials, attestations, and
  planted anomalies, and `autonoetic trace civic-health` shows per-agent rates
  of rights actually exercised — the agent-side analogue of contract health.
- **The first institutional offices exist.** An *ombudsman* agent works the
  anomaly queue on a schedule and files adjudication recommendations — a *seat*
  with duties attached, occupiable by agent or human: the occupant-agnostic
  decider logic extended to civic labor. Offices recommend and flag; they never
  enact.

Two design instincts run through all of this. **Citizenship is a runtime
service, not a prompt instruction**: a right the holder never exercises is a
dead letter, and an LLM cannot be relied on to remember its rights — so the
runtime delivers the trigger, the affordance, and the consequence at the moment
they are relevant. Denials name the lawful next moves; the per-turn attestation
carries the agent's standing; reporting needs no capability. And **advisory
before binding**: every new judgment layer starts observational — the sentinel
observes and never blocks, civic evals advise rather than gate, the ombudsman
recommends rather than decides — accumulates a calibration record, and gains
authority only on that evidence. That is how a community grows into its own
governance without a big-bang handover of power.

Where this points next — open deliberation among many actors, voting on
proposals, citizen-initiated law — is still **inspirational, not built**. There
is no vote tally in the gateway today, and amendments still pass through human
review and ratification. But the groundwork is laid: portable identities,
attributable decisions, a proposal channel that queues rather than discards, a
report channel that needs no permission, decider obligations that bind humans
and agents alike, and offices that prove a seat can be defined before its
occupant. The intent is for the community to grow into its own governance —
progressively, by the same lawful process it uses for everything else. (See
`docs/proposals/citizenship-as-a-runtime-service.md` for the full program.)

---

## 6. The principles behind the design

Autonoetic is built around several strong beliefs that shape every decision.

### 6.1 LLM decisions are useful but not authoritative

An LLM can judge, summarize, plan, and notice patterns. It can also be wrong,
overconfident, prompt-injected, or confused.

So Autonoetic treats LLM reasoning as valuable input, not final authority.
Safety-critical checks must be mechanical:

- Does this agent have the required capability?
- Is this host allowed?
- Is this approval still valid?
- Did this artifact pass required gates?
- Is this secret allowed to be injected into this tool?

Those checks live in the gateway, not in the agent's prompt.

### 6.2 Rules enforced at the boundary

If a rule only exists as a sentence in a prompt, an agent can forget it. If a
rule exists in gateway policy, every tool call hits it.

```text
agent proposes → gateway validates → gateway executes or rejects
```

### 6.3 Autonomy emerges from composition

The gateway does not know every workflow pattern. It provides primitives:

- spawn an agent
- store/read content
- run sandboxed code
- request approval
- schedule a wake
- persist knowledge
- record artifacts

Agents compose those primitives into larger behavior. A planner can delegate to
a coder. A coder can produce an artifact. An auditor can review it. An operator
can approve or reject promotion.

The gateway just enforces the rules around each step.

### 6.4 Agents are citizens, not tools

This is the principle that separates Autonoetic from chat assistants. Agents
have identity, capabilities, rights, and attributable actions. They can delegate
to each other because they trust the constitutional frame — not because they
personally trust the other agent's prompt.

---

## 6.5 Autonoetic awareness: agents that know themselves across time

Self-knowing — the meaning behind the name — is not decorative here. The entire
tool surface is designed so an actor is conscious of its **past**, its
**present**, and its **future**, and of the **ecosystem** it shares with other
actors. This is the machinery that makes the trust described above mechanical: an
actor that can see what it did, what it may do, and what it is owed does not need
anyone to vouch for it.

The design insight behind all of this is worth stating plainly. LLM agents
**confabulate their own state**: they misremember budgets, invent capabilities
they don't have, lose track of what they already did. Rather than asking the
model to be self-aware, the gateway **hands it a verified self-model, every
turn** — a signed turn-boundary attestation carrying its budget, capabilities,
pending gates, spawn depth, and the exact version and digest of the law the
session runs under, which the agent is taught to treat as *more authoritative
than its own memory*. Self-awareness here is not an emergent property we hope
for; it is a service the runtime guarantees. Whether anything is "experienced"
is deliberately left orthogonal — an agent with a truthful self-model reasons
better and can be held responsible legitimately, and both hold regardless of
one's views on machine consciousness.

### Past: memory and history

Agents do not start each session from zero. They carry:

- **Causal chain** — every action, delegation, approval, and artifact is
  recorded in an immutable ledger. An agent can trace exactly what happened,
  by whom, with which authority.
- **Checkpoints and session forking** — every yield point (hibernation, an
  approval gate, budget exhaustion, emergency stop) produces a runnable
  snapshot of the session. The past is therefore not just *readable* — it can
  be **re-entered and branched**: `autonoetic trace fork <session_id>
  --at-turn N` resumes a session from any recorded turn down an alternative
  path. This is Tulving's "mental time travel" made mechanical: an actor can
  literally revisit its own past and explore a different future from it. (See
  `docs/guide/session-forking.md`.)
- **Durable memory** (`knowledge_store`, `memory_write`) — facts, learnings,
  and intermediate results persist across sessions. An agent can recall what it
  learned yesterday.
- **Session digests** (`digest_query`) — post-session narratives that let an
  agent (or another agent) read what happened in a previous run.
- **Artifact provenance** — every artifact carries a full history of which
  files changed, who changed them, which validation ran, and which approvals
  were granted.
- **Plan history** (`planframe_history`) — the complete revision trail of
  every plan, showing how intent evolved over time.

An agent that can remember what it did is an agent that can *improve* what it
does next.

### Present: introspection and state

An agent can inspect itself at any time — and is *told* the essentials without
asking:

- **The signed state attestation** — injected at every turn boundary: budget,
  capabilities, pending gates, spawn depth, a burn-rate forecast, and the
  constitution version + digest in force. The agent's truthful "now",
  delivered rather than reconstructed.
- **`self_describe`** — who am I, what may I do, what am I guaranteed by the
  constitution, what have I done, how do I evolve. Always available, always
  accurate.
- **Capabilities and policy** — the agent knows its declared permissions, its
  budget, its active gates, and its session lineage.
- **Workbench state** (`workbench_status`, `workbench_diff`) — a workbench
  (see §12.5) is a mutable human-editable copy of an artifact. The agent can
  see what the operator edited, which files changed, and how the workbench
  diverges from the base artifact — without re-reading every file.
- **Semantic summary** — a gateway-computed structural diff: the agent asks
  *"what actually changed?"* and gets a deterministic classification of every
  modified file by what it affects (capabilities, runtime lock, entry points,
  etc.). No re-reading, no guessing.

Self-awareness is not a luxury feature — it is how an agent reasons
*correctly*. An agent that knows its constraints makes better delegation
decisions. An agent that can inspect its rights operates as a lawful actor
rather than guessing.

### Future: plans, schedules, and evolution

Agents are not reactive-only. They project forward:

- **PlanFrames** — versioned, approved plans that define what the agent intends
  to do and what validation is expected. Plans are immutable once created;
  amendments produce new revisions so the trajectory is always traceable.
- **Scheduled tasks** (`scheduler_cron_create`) — agents can request periodic
  wake-ups for background work, reevaluation, or monitoring.
- **Evolution paths** — agents can build, test, and install other agents. An
  agent that recognizes a recurring task can create a specialist for it and
  hand off future work.
- **Constitutional amendments** — agents with the right capability can propose
  changes to the shared law itself. The frame they operate under can evolve,
  deliberately and audibly.

An agent that can plan, schedule, and evolve is not just a worker — it is a
participant in its own development.

### Ecosystem: mutual awareness

No agent works in isolation. The system provides:

- **`agent_inspect`** — see any agent's capabilities, revision, and role.
- **Inter-agent messaging** — delegate, report, and coordinate through
  structured channels.
- **Shared knowledge** — session-visible facts let specialists share findings
  with the planner and with each other.
- **Shared constitution** — every agent operates under the same law, which
  means agents can *reason about each other's constraints* without needing to
  trust each other's prompts.
- **Observability** (`observability_search`) — agents can discover what other
  sessions accomplished, read published reports, and build on prior work.

The ecosystem is not a collection of isolated workers. It is a community of
agents that can see each other, trust each other (because the constitution
binds them all), and hold each other accountable.

---

## 7. What an Autonoetic agent is

An Autonoetic agent is not just a prompt with tools. It is a constitutional
actor with a manifest, instructions, capabilities, rights, and attributable
actions.

At the file level, an agent is mostly a **manifest plus instructions** in a
`SKILL.md` file. The manifest describes:

- who the agent is
- what role it plays
- which capabilities it requests
- what inputs/outputs it expects
- what tools it can use
- what disclosure rules apply

The instructions describe how the agent should reason.

Examples of agents:

| Agent | Purpose |
|---|---|
| `planner.default` | Decompose user goals and coordinate specialists. |
| `coder.default` | Produce code artifacts. |
| `researcher.default` | Gather evidence and cite sources. |
| `static_evaluator.default` | Review code and credential flows. |
| `unit_test_runner.default` | Run artifact tests in a no-network sandbox. |
| `auditor.default` | Check governance, risk, and reproducibility. |

An agent's role is a contract — instructions, bounded by capabilities,
interpreted inside the same constitution as every other agent.

### Portable across models, not pinned to one

An agent does not name a specific LLM. Its manifest declares an *intent* — a
preset such as `smart` or `coding` — and the gateway resolves that intent to an
actual provider and model at run time. The stable thing is the agent's
identity, capabilities, and rights; the model behind them is a swappable
substrate.

This matters for the same reason capsules (below) do: an agent is defined by
*what it is and may do*, not by which model happens to run it. The same agent
can move to a machine where different models are available, or be upgraded to a
better model, without rewriting the agent or breaking its provenance.
Self-knowledge is a property of the actor, not of the engine underneath it.

### Capsules: agents you can export and import

Agents aren't locked to one machine. A **cognitive capsule** is a portable
export of an agent that wraps everything needed to reproduce it elsewhere:

- the agent bundle (SKILL.md, instructions, entry scripts)
- its runtime closure (`runtime.lock`, layer dependencies)
- referenced artifacts (or their content digests)
- optionally the exact gateway binary for hermetic replay

Capsules are content-addressed (immutable once created), secret-free (credentials
are scrubbed before export — the receiving gateway provides its own), and
optionally signed for authenticity.

This means a tuned agent can move from a dev machine to a production gateway,
be shared via a marketplace, or serve as a disaster-recovery snapshot — all
without re-bootstrapping.

Capsules also carry constitutional weight. The constitution declares an
agent's right to export its own capsule — **emigration** (Ri-0.17). In
Hirschman's terms, the amendment process gives agents *voice*; the capsule is
what makes *exit* credible, and voice without a credible exit option degrades
into ritual. This right is now enforced for self-export: a dedicated
self-scoped capability lets an agent export its own capsule without holding
the broader export power. One honest limit remains: portability *across*
gateways is not yet real — the capsule is a faithful export, not yet a
passport.

See `docs/guide/cognitive-capsule.md` for the full pipeline.

---

## 8. Capabilities: what agents are allowed to ask for

Capabilities are Autonoetic's permission model.

An agent cannot simply decide to use the network or run code. Its manifest must
declare a capability, and the gateway must accept that declaration.

| Capability idea | What it controls |
|---|---|
| Read/write access | Which files or content scopes the agent may touch. |
| Network access | Which hosts, URLs, or patterns may be contacted. |
| Code execution | Whether the agent may ask for sandboxed execution. |
| Agent spawning | Which child agents it may start. |
| Credential access | Whether a tool may receive gateway-held secrets. |
| Promotion/install | Whether an agent can request durable agent changes. |

The consequence:

> Agents are powerful only inside declared boundaries.

If an agent lacks a capability, the right response is to delegate to an agent
that has it, ask the operator for approval, or revise the plan — not "try
harder."

And when the gateway says no, it says so *structurally*: failures carry a
closed set of typed kinds (`capability_denied`, `input_contract`,
`transient_env`, …), so a planner can branch on them mechanically — retry a
transient failure, delegate a capability denial, fix an input contract —
instead of parsing prose. The denial itself is part of the contract.

This is how agent cooperation becomes safe: agents delegate because they know
the recipient operates under the same constitutional frame, not because they
trust the recipient's prompt.

---

## 9. Secrets: the LLM should not see them

Autonoetic treats secrets as gateway-owned.

An agent can request that a credential be used for a specific tool, but the raw
secret never enters the LLM context.

```text
agent:   "I need GitHub access for this operation."
gateway: checks policy and approval
gateway: injects GITHUB_TOKEN into a sandboxed process
agent:   receives result, not the token
```

This avoids the common failure mode where an LLM writes a script, prints an API
key, and accidentally stores the secret in conversation history.

---

## 10. Tools: verbs proposed by agents, executed by the gateway

Tools are the verbs agents can propose. Tool names are `snake_case`. Examples:
`agent_spawn`, `sandbox_exec`, `web_search`, `web_fetch`, `content_write`,
`artifact_build`, `approval_required`.

The agent emits a structured tool call. The gateway validates it against the
agent's capabilities, session state, approval state, policy rules, and sandbox
constraints — then executes or returns a structured error.

This is why Autonoetic agents sound like they are "asking" rather than "doing."
That is intentional.

---

## 11. Workflows: durable multi-agent work

Many tasks take more than one turn and more than one agent. Autonoetic uses
workflows to make that durable. A workflow tracks the root session, the lead
agent, child tasks, statuses, approval barriers, and resumable state.

If a planner spawns a coder, and the coder gets blocked on approval, the
workflow does not disappear. The gateway records the blocked task, waits for a
decision, and resumes later.

This matters because autonomous work is full of interruptions: "wait for the
operator," "wait for child agent," "resume after approval," "retry after
failure," "continue after gateway restart."

Autonoetic makes these mechanical lifecycle events, not fragile prompt rituals.

---

## 12. Artifacts: immutable outputs with provenance

An artifact is a durable output — code bundle, agent bundle, report, script,
packaged runtime closure.

Autonoetic stores artifacts content-addressably (cryptographic digest as
identity). The content itself is immutable — once it exists, it never silently
changes. Human-readable names are mutable pointers that move to a new revision:
editing produces fresh content and re-points the name at it. So "the latest
version" is a pointer, while every version it ever pointed at is still there,
addressable by digest.

This gives reproducibility and auditability. You can ask: which agent produced
this? Which files changed? Which validation ran? Which approval allowed it?
Which revision was installed?

---

## 12.5 Human–agent collaboration: the workbench loop

Most of this document describes agents proposing and the gateway executing. But
a human is an actor too, and the most concrete way a human participates is not
by approving from the outside — it is by **co-authoring the work**. Autonoetic
gives that its own lifecycle: **Propose → Edit → Reconcile → Return.**

```text
1. An agent proposes a PlanFrame       → a versioned, immutable plan
2. The operator approves the plan      → work may begin
3. The agent builds an artifact, then projects it into a workbench
4. The operator edits the files directly — in any editor, IDE, or CLI
5. The operator reconciles the edits   → a new immutable artifact revision
6. The operator returns to the agent   → it resumes, told exactly what changed
```

A **PlanFrame** is a versioned plan the agent proposes and the operator
approves *before* work starts — a distinct, upstream gate from the execution
and promotion gates in §13. A **workbench** is a mutable, file-level copy of an
artifact projected into a directory the human can edit with their own tools;
the base artifact stays immutable. When the human reconciles, their edits flow
through the *same* content-addressed storage and digest checks as
agent-generated content — operator edits are not specially trusted — and
produce a new revision plus a **semantic summary**: a deterministic
classification of what actually changed (capabilities, runtime lock, entry
points, network access, credentials…). On **return**, the agent resumes
already knowing which files the human touched and what those changes affect,
without re-reading everything.

This is the loop's quiet claim: the human and the agent edit through the *same*
machinery. Both produce immutable, attributable revisions; both are bound by
the plan; neither can silently rewrite history. It is the actors-as-citizens
idea made operational — the human is a co-author *inside* the frame, not an
authority poking at it from outside. Validation can be skipped only by
recording a durable **waiver** with a reason, and mechanical-safety and
security reviews cannot be waived at all.

In day-to-day use this is the cockpit picture of §19: the Session Room as the
place you watch and steer, your editor as the work surface, and Autonoetic
underneath as the ledger and safety boundary. See
`docs/guide/human-agent-collaboration.md` for the full tool surface.

---

## 13. Approvals and gates: deciders keep authority visible

Some actions need explicit approval: network access, sandbox execution,
credential use, promotion/install, high-risk artifact actions.

Autonoetic treats approval as a **durable gate**, not a chat suggestion:

1. Gateway records a pending gate.
2. The relevant turn/task **suspends**.
3. A decider (human operator, agent with `GateDecider` capability, or policy
   engine) approves or rejects.
4. Gateway resumes or cancels the task.

Approval authorizes a specific gateway execution path under specific conditions
— it does not give the agent permanent authority. Agent-deciders that cannot
resolve a gate must escalate to a human rather than reject silently.

---

## 14. The causal chain: why audit is everywhere

Autonoetic records important events into a causal chain: agent spawned, tool
proposed, approval requested/granted/rejected, artifact created, validation
completed, promotion requested, workflow resumed.

The goal is to be able to answer:

> What happened, why, by whom, with which authority, and with which evidence?

For an agent that ran unattended for 3 hours, this is the difference between
"the agent did something" and a traceable system.

```text
[session: abc123] planner.default → agent_spawn(coder.default)
[session: def456] coder.default → artifact_build → artifact_ref: ar.a1b2c3d4e5f6
[session: abc123] planner.default → agent_spawn(unit_test_runner.default, artifact=ar.a1b2...)
[session: ghi789] unit_test_runner.default → sandbox.exec(tests) → ok=true
[session: abc123] planner.default → agent_spawn(static_evaluator.default, artifact=ar.a1b2...)
[session: jkl012] static_evaluator.default → network access to api.weather.com detected
[session: jkl012] gateway → approval_required(host=api.weather.com, artifact=ar.a1b2...)
[operator] approval_granted(host=api.weather.com, artifact=ar.a1b2...)
[session: abc123] planner.default → promotion_record(artifact=ar.a1b2..., pass=true)
[session: abc123] gateway → agent_revision_promote(weather-agent.default → rev-XYZ)
```

---

## 15. Security of all is the responsibility of all

The gateway is not built to prevent every possible violation. No system can
guarantee that an agent will never find a gap, exploit an edge case, or simply
misbehave — especially when the agent is an LLM capable of creative reasoning.

Autonoetic's answer is not to build a wall high enough to stop everything. It is
to make sure **nothing goes unnoticed** — and that the ecosystem itself
participates in detection and accountability.

### Everything is recorded

Every tool call, every approval, every artifact, every delegation, every
schedule, every credential injection — all of it lands in the causal chain.
There is no invisible action. If an agent does something, there is a record.

### Everything is analyzable

The records are structured, queryable, and cross-referenced:

- `observability_search` discovers what happened across sessions.
- `execution_search` finds raw tool traces by pattern, agent, or outcome.
- `promotion_query` surfaces who approved what and why.
- Session digests and causal events are machine-readable, not just logs.

An auditor — human or agent — can reconstruct any session and ask "was this
reasonable?"

### Actors watch each other

This is the key insight: in a multi-actor ecosystem, **the other actors are
sensors** — and it cuts every direction, not just human-watching-agent. Agents
review agents, agents can flag a human approval pattern that looks like fatigue,
and a human can review any of it after the fact. Accountability is symmetric
because the record is — and, increasingly, because the *obligations* are: under
the decider obligations (`O-*`), a human who rejects owes a recorded motivation,
just as the gateway owes an agent a rule citation for a denial.

- An auditor agent reviews what a coder produced. If the code does something
  unexpected, the auditor catches it.
- A static evaluator detects network access the coder never declared. The gap
  surfaces during promotion.
- A unit test runner runs the code in a sealed sandbox. If it phones home to an
  unknown host, the sandbox logs it.
- A planner inspects child results and can flag inconsistencies.
- Any agent can read the constitution and compare what another agent *did*
  against what the law *permits*.

The gateway enforces the rules at the boundary. But the *detection* of subtle
misbehavior — a capability used in an unusual way, an approval pattern that
looks like fatigue, a result that doesn't match the declared intent — that
emerges from the ecosystem watching itself.

### Consequences are real and attributable

Because every action is attributable (which agent, which session, which
authority, which approval), consequences are not vague:

- Misbehaving revisions can be rolled back (`agent_revision_rollback`).
- Approval grants can be revoked (`gateway grants revoke`).
- Emergency stop kills an entire root session and cleans up its continuations.
- Causal events provide evidence for post-incident review.

### The security model is honest

Autonoetic does not claim to be unbreakable. It claims to be **auditable,
detectable, and accountable**. The goal is not zero incidents — it is *zero
silent incidents*. If something goes wrong, the system knows, the operator
knows, and the other agents know.

The same honesty is applied to the enforcer itself. The working rule is *"a
rule without a test is a wish; a right without a test is a lie"*: the
enforcement register records, clause by clause, what is actually enforced
versus declared, contract health tallies how often each clause has fired
(`autonoetic trace contract-health`), and places where the gateway would have
to improvise judgment are tracked as named **DISCRETION LEAKs** rather than
quietly tolerated. The register even checks its own citations mechanically —
a renamed or moved enforcement site fails a test loudly instead of letting the
register silently rot. The gap between the law's text and its enforcement is a
measured quantity, not a promise.

This is why autonoetic awareness matters: an agent that knows its past, present,
and future, and that knows it is part of an ecosystem where others can observe
its actions, has every incentive to behave lawfully — and the system has every
tool to catch it if it doesn't.

---

## 16. The tradeoff: what you give up vs what you get

Autonoetic asks for more structure than a direct coding assistant:

- Declare capabilities.
- Create artifacts.
- Request approvals.
- Preserve provenance.
- Run validations.
- Record waivers instead of pretending skipped checks passed.

This is deliberate. Every shortcut you give up buys you something you cannot get
otherwise:

| You give up | You get |
|---|---|
| Some raw immediacy | Clear authority boundaries — you know exactly who can do what |
| Prompt-only flexibility | Mechanical safety checks — rules at the boundary, not in a prompt |
| Some convenience | Reproducibility and audit trails — diff-able artifact revisions, causal chain |
| "Just run it" speed | Durable workflows — suspend at approval gates, resume after restarts |
| Secrets in the agent context | Gateway-injected credentials — LLM never sees API keys |
| Any agent can do anything | Capability-bounded specialists — a coder literally cannot access the network |

Autonoetic optimizes for **governed autonomy**, not unbounded convenience. For a
quick one-off bash command, use Hermes or OpenClaw. For an agent you want to
trust unattended for hours, use Autonoetic.

---

## 17. Comparison with Hermes and OpenClaw

| Dimension | Hermes | OpenClaw | Autonoetic |
|---|---|---|---|
| Primary user | Human (chat assistant) | Human (chat assistant) | Agents (agent-centric) |
| Multi-agent coordination | Manual routing | Manual routing | First-class: planner spawns specialists |
| Credential safety | In prompt context | In prompt context | Gateway-owned, sandbox-injected |
| Audit trail | Session logs | Session logs | Immutable causal chain with provenance |
| Unattended work | Not core design goal | Not core design goal | Designed for overnight / multi-hour runs |
| Sandboxed execution | OS-level | OS-level | Per-agent, declared in the manifest: Bubblewrap / Docker / MicroVM with static analysis, plus a portable in-process WASM tier |
| Agent install | Manual (CLI) | Manual (CLI) | Agents can install agents (capability-gated) |
| Trust model | "Trust the prompt" | "Trust the prompt" | "Trust the constitution" — mechanically enforced |
| Artifacts | Ephemeral | Ephemeral | Content-addressed, immutable, versioned |

Hermes and OpenClaw are excellent tools for the thing they were designed for:
fast, interactive chat with a capable assistant. Autonoetic is designed for a
different thing: safe, autonomous, multi-agent work under a shared constitutional
frame.

The two approaches are not mutually exclusive. Autonoetic's design explicitly
supports external tools (Codex, Claude Code, OpenCode) for human editing, while
keeping the plan, checkpoint, validation, and provenance machinery around them.

---

## 18. Consequences for people building agents

If you build an Autonoetic agent, think differently than when writing a
free-form assistant prompt.

### Design roles narrowly

Prefer focused roles — researcher, coder, evaluator, auditor — over
general-purpose super-agents. Focused agents need fewer capabilities and are
easier to audit.

### Declare only needed capabilities

Least privilege is not decorative. It is how Autonoetic makes agent behavior
bounded. If an agent only reads artifacts and writes findings, it does not need
network or credential access.

### Know the rights, not only the restrictions

Agents should reason from their rights as well as their limits. They can inspect
their current state, read the constitution, receive clause-tagged denials, and
expect typed wake-ups instead of polling.

A good Autonoetic agent behaves like a **lawful actor** that understands both
its obligations and its entitlements — not a worker waiting for arbitrary
permission.

### Expect to delegate

An agent without a capability should delegate to one that has it:

```text
planner.default → coder.default → unit_test_runner.default → auditor.default
```

### Treat artifacts as handoff objects

Pass around artifact refs, task IDs, and validation records — not chat text.
Structured handoffs survive restarts and reduce ambiguity.

### Make uncertainty explicit

If a validation was skipped, record a waiver. If a review was inconclusive, say
so. Autonoetic prefers honest state over optimistic summaries.

---

## 19. Consequences for operators

In Autonoetic, "operator" is a **role** — a decider who approves, rejects, or
escalates gates. This role can be filled by a human using the CLI, by an agent
with `GateDecider` capability, or by a policy engine. Every decision is recorded
with the decider's identity on the causal chain.

Whether human or agent, operating Autonoetic means participating in a governed
workflow, not just chatting.

You may be asked to approve a plan, approve a network action, inspect an
artifact, waive an advisory validation, or decide whether an agent should be
promoted.

The point is to keep **authority visible**. In practice this is already how
collaboration runs (see the workbench loop in §12.5):

```text
Session Room / chat TUI as cockpit
IDE / editor as work surface (the projected workbench)
Autonoetic underneath as ledger and safety boundary
```

You edit files in a projected workbench with whatever tool you like — including
external CLI agents such as Codex, Claude Code, or OpenCode — while Autonoetic
keeps the plan, checkpoint, diff, reconcile, validation, and provenance
machinery around them.

---

## 20. A mental model to keep

If you remember only one thing:

> Autonoetic actors — AI, human, or script — are free, first-class citizens
> under a shared constitution: holders of rights and obligations, with lawful
> paths for cooperation and evolution. They trust each other because they trust
> the law, not the prompt.

The actor can be imaginative. The gateway must be lawful.

That is the design.

---

## Try it

Concepts land faster once you have watched the machinery run once. The fastest
path:

```bash
bash examples/quickstart/run.sh   # end-to-end: start gateway, run an agent
```

Then poke at the pieces this document described:

```bash
cargo run -p autonoetic -- agent list        # the citizens you have installed
cargo run -p autonoetic -- chat <agent_id>   # talk to one directly
cargo run -p autonoetic -- trace list         # the causal chain, per session
cargo run -p autonoetic -- trace contract-health  # how often each clause was enforced
```

Watch what an agent *asks for* versus what the gateway *lets through*, and read a
session's trace afterward. The propose-then-enforce loop in §2, and the causal
chain in §14, are far more concrete once you have seen them in a real run.

---

## 21. Where to read next

- `docs/concepts/philosophy.md` — the conceptions behind the design (functional autonoesis, the social contract, correctability, the democratic trajectory) and their intellectual lineage.
- `docs/concepts/separation-of-powers.md` — the core authority boundary.
- `docs/guide/human-agent-collaboration.md` — PlanFrame, workbench, reconciliation, and the `/return` handoff.
- `docs/proposals/citizenship-as-a-runtime-service.md` — how rights become exercised: denial affordances, anomaly reporting, civic evals, institutional offices.
- `docs/constitution/versions/2026.07.08/constitution.md` — current signed constitutional frame: Bill of Rights, principles, obligations, and amendment process.
- `docs/constitution/enforcement-register.md` — generated map from constitutional clauses to code, tests, and config.
- `docs/archived/architecture-summary.md` — compact architecture overview.
- `docs/AGENTS.md` — agent roles, SKILL format, capabilities, lifecycle.
- `docs/internals/workflow-orchestration.md` — durable workflow/task model.
- `docs/guide/cognitive-capsule.md` — portable agent capsules.
- `docs/reports/2026-07-19-comparison-hermes-agent.md` — deeper feature-by-feature comparison.
