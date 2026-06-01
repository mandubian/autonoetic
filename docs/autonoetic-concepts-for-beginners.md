# Autonoetic for Beginners

This document introduces Autonoetic from first principles. It is written for
people who are new to the project, new to agent runtimes, or trying to
understand why Autonoetic is built the way it is — especially people coming from
Hermes, OpenClaw, or similar direct-code assistants.

The short version:

> Autonoetic is a runtime where agents are **free actors under a shared
> constitution**. It lets agents work on their own — overnight, in teams of
> specialists, safely limited by rules they cannot override. The agent reasons
> and decides; the gateway mechanically enforces the law.

Autonoetic is not trying to be the fastest way to give an LLM a terminal. It is
designed for agents that work **independently** — spawning children, delegating
work, sleeping, waking up, using credentials they never see, and collaborating
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

That distinction is the whole project.

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

Why go through all this? Because useful agents quickly want dangerous powers:

- Read and write files.
- Run code.
- Access the network.
- Store memories.
- Use credentials.
- Spawn other agents.
- Install new tools or agents.
- Keep working in the background.

If the LLM directly owns those powers, every prompt-injection, hallucination, or
bad plan can become a real-world side effect.

Autonoetic's core question:

> How can agents be autonomous without making the LLM itself the authority?

The answer is not "add more safety checks." Autonoetic defines a shared
constitutional frame first, then builds the runtime around it. The LLM proposes
— the gateway decides.

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

### Rules and rights

The constitution has two directions:

| Direction | Meaning | Binds |
|---|---|---|
| **Rules / principles** (`P-*`) | What agents must not do, or must do in a constrained way. | Agents |
| **Rights** (`Ri-*`) | What the gateway must guarantee to every agent. | Gateway |

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
Tests pin invariants. The constitution is versioned and signed.

That matters: agents can improve themselves and one another while knowing the
common frame can evolve deliberately rather than silently drifting.

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

The word *autonoetic* means self-knowing — and this is not decorative. The
entire tool surface is designed so agents are conscious of their **past**, their
**present**, and their **future**, and of the **ecosystem** they share with
other agents.

### Past: memory and history

Agents do not start each session from zero. They carry:

- **Causal chain** — every action, delegation, approval, and artifact is
  recorded in an immutable ledger. An agent can trace exactly what happened,
  by whom, with which authority.
- **Durable memory** (`knowledge_store`, `memory.write`) — facts, learnings,
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

An agent can inspect itself at any time:

- **`self_describe`** — who am I, what may I do, what am I guaranteed by the
  constitution, what have I done, how do I evolve. Always available, always
  accurate.
- **Capabilities and policy** — the agent knows its declared permissions, its
  budget, its active gates, and its session lineage.
- **Workbench state** (`workbench_status`, `workbench_diff`) — a workbench
  (see §12) is a mutable human-editable copy of an artifact. The agent can
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

See `docs/cognitive-capsule.md` for the full pipeline.

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

Tools are the verbs agents can propose. Examples: `agent_spawn`,
`sandbox.exec`, `web.*`, `content.*`, `artifact.*`, `approval.*`.

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
identity). Once an artifact exists, it never silently changes. Edits produce a
new revision.

This gives reproducibility and auditability. You can ask: which agent produced
this? Which files changed? Which validation ran? Which approval allowed it?
Which revision was installed?

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

### Agents watch each other

This is the key insight: in a multi-agent ecosystem, **other agents are
sensors**.

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
| Sandboxed execution | OS-level | OS-level | Bubblewrap/Docker/MicroVM with static analysis |
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

The point is to keep **authority visible**. Newer design work moves toward a
smoother loop:

```text
Chat TUI as cockpit
IDE/editor as work surface
Autonoetic as ledger and safety boundary
```

You can use good local tools — even external CLI agents such as Codex, Claude
Code, or OpenCode — while Autonoetic keeps the plan, checkpoint, diff,
reconcile, validation, and provenance machinery around them.

---

## 20. A mental model to keep

If you remember only one thing:

> Autonoetic agents are free actors under a shared constitution — low-privilege
> reasoners with rights, obligations, and lawful paths for cooperation and
> evolution.

The agent can be imaginative. The gateway must be lawful.

That is the design.

---

## 21. Where to read next

- `docs/separation-of-powers.md` — the core authority boundary.
- `docs/human-agent-collaboration.md` — PlanFrame, workbench, reconciliation, and the `/return` handoff.
- `docs/constitution/versions/2026.05.30/constitution.md` — current signed constitutional frame: Bill of Rights, principles, and amendment process.
- `docs/constitution/enforcement-register.md` — generated map from constitutional clauses to code, tests, and config.
- `docs/architecture-summary.md` — compact architecture overview.
- `docs/AGENTS.md` — agent roles, SKILL format, capabilities, lifecycle.
- `docs/workflow-orchestration.md` — durable workflow/task model.
- `docs/cognitive-capsule.md` — portable agent capsules.
- `docs/comparison-hermes-agent.md` — deeper feature-by-feature comparison.
