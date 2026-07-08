# The Philosophy of Autonoetic

> **Status:** living document. This is the *why* behind the constitution and
> the gateway — the conceptions that drive the design, made explicit so they
> can be examined, criticized, and amended like everything else here. It is
> **not law**: the constitution
> (`docs/constitution/versions/<version>/constitution.md`) is the law; this
> document explains what the law is *for*. Where the two disagree, the
> constitution wins and this document has a bug.
>
> The core conceptions in §1–§4 are the maintainer's own, arrived at
> independently while building the system. §6 documents where they converge
> with established philosophy and political science — recorded after the
> fact, because independent convergence with ideas that survived centuries of
> criticism is itself evidence that the design instincts are sound.
>
> Companion documents: the constitution (the enforced law),
> `docs/separation-of-powers.md` (the power structure),
> `docs/design/principal-model-and-symmetric-obligations.md` (the citizenship
> RFC, #359), `docs/autonoetic-concepts-for-beginners.md` (the
> actors-as-citizens framing).

---

## 1. What "self-aware" means here: functional autonoesis

The project is named after **autonoetic consciousness** — Endel Tulving's term
for the capacity to place oneself in one's own subjective past and future: not
just *knowing* facts, but *self-knowing* — remembering one's own experiences
as one's own, and projecting oneself forward in time.

Autonoetic makes **no claim about phenomenal consciousness**. The claim is
narrower and mechanical: the system provides every agent with the
*institutional preconditions of accurate self-modeling*. An agent under this
gateway has:

| Capacity | Mechanism |
|---|---|
| A truthful, revisitable **past** | Right to read its own causal chain (Ri-0.2); checkpoints; session forking — an agent can re-enter and even branch its own history |
| A truthful **present** | The signed turn-boundary state attestation (P-6.23): budget, capabilities, pending gates, spawn depth, **and the constitution version + digest the session runs under** — injected every turn, taught to be *more authoritative than the agent's own memory* |
| Knowledge of its **normative standing** | The full constitution readable by digest (Ri-0.10) — and that same digest is bound into the per-turn attestation, so the agent knows *which law* it is under as a verified fact, not only on demand; every rejection names its rule (Ri-0.3) |
| A bounded, legible **future** | Budgets known truthfully in real time (Ri-0.4); a closed list of ways its session can end (Ri-0.12); notice before degradation where practical (Ri-0.5, Ri-0.9) |
| A continuous **identity** | Non-repudiable attribution (Ri-0.11); portable identity via cognitive capsules; immutable revisions with audited promotion history |

The design insight behind P-6.23 is worth stating plainly: LLM agents
*confabulate* their own state — they misremember budgets, invent capabilities,
lose track of what they did. Rather than asking the model to be self-aware,
the gateway **hands it a verified self-model every turn**. Self-awareness here
is not an emergent property we hope for; it is a service the runtime
guarantees — and it is delivered at two levels, matching the two things an
agent must know to reason well:

- Its **operational** state (budget, capabilities, pending gates, the law in
  force) is handed *in full*, every turn, in the signed attestation block. The
  agent is taught this block is more authoritative than its own memory.
- Its **normative** standing (the rights that bind the gateway on its behalf)
  is *surfaced by default* — the headline rights (Ri-0.2 read your history,
  Ri-0.3 named rejection, Ri-0.11 non-repudiation) are named in the foundation
  prompt every agent receives, and the one-call `self_describe` tool is nudged
  there — and *fully available on demand* via `constitution_read`. Surfacing
  the headlines by default matters because an agent that does not know it has
  a right will not exercise it; making the full text on-demand keeps the
  per-turn prompt bounded.

Whether anything is "experienced" is deliberately orthogonal — an agent with
a truthful self-model reasons better and can be held responsible legitimately,
and both of those hold regardless of one's views on machine consciousness.

## 2. The social contract: who is bound, and to whom

The constitution's structural novelty is **bind-direction discipline**: every
clause binds exactly one party.

- **Rules** (`P-*`) bind the *agent*: a finite, named set of forbidden
  actions; everything else is permitted.
- **Rights** (`Ri-*`) bind the *gateway*: unconditional entitlements the
  enforcer owes every agent, revocable only by amendment.
- **Obligations** (`O-*`) bind the *decider*: whoever exercises authority over
  an agent owes duties mirroring the agent's own.
- *(Planned — see §5)* a **service charter** binding the community toward the
  *served party*: the end-user, human or not, whom the whole arrangement
  exists to serve.

Most agent-safety frameworks are rules-only: pure constraint, where the
enforcer owes nothing. Making the enforcer a bound party — and tracking the
rights/rules ratio as a design signal — is what turns a compliance regime
into a social contract. A right is not a favour; it is what makes the rules
*legitimate* rather than merely effective.

Two consequences of the contract framing run through everything:

- **Trust is structural, not personal.** Two parties cooperate because each
  can verify the other operates under compatible law (the constitution digest
  handshake, P-10.9) — not because they know each other's internals or
  history. Membership in the community *is* operating under the shared,
  verifiable law. This is what makes a mixed human/AI community possible: it
  requires no theory of the other party's mind.
- **The law is rule-of-law, not rule-by-law.** Rule Zero applies the rules
  equally to all agents without exception; every denial cites its clause;
  the enforcer itself exercises no improvised judgment (the Lawful-Executor
  invariant, §14) and its lapses are *named debts* (DISCRETION LEAKs), not
  accepted behaviour.

## 3. The four commitments

These are the maintainer's founding conceptions. They are the criteria
against which governance-touching design should be evaluated.

### 3.1 Correctability over perfection

**The gateway is fallible by nature. Perfection is not the target — the
target is a community able to report issues and reach agreement to resolve
them.**

Legitimacy, on this view, does not come from the enforcer being right; it
comes from errors being *reportable, attributable, and correctable*. The
machinery already exists: contract health (the standing tally of what is
actually enforced), DISCRETION LEAK *naming* (the gateway's own failures
named in prose at their site — `P-5.2`, `P-5.8`; aggregation into a
monitored register is the §5.4 door, not yet built), Ri-0.8 (any capable
agent may propose amendments, durably, un-droppably), the amendment process
(agreement with tests and review), and the causal chain (the shared
evidence base that lets disagreements be resolved by facts rather than by
authority).

This commitment has a structural consequence: **if correctability is the
source of legitimacy, the clauses that enable correction are not ordinary
clauses**. Ri-0.2 (read your own chain), Ri-0.3 (every denial names its
rule), Ri-0.8 (right to propose), Ri-0.11 (non-repudiation), P-8.1
(hash-chain integrity), O-1 (a decider owes a motivated decision), and the
amendment process itself are the machinery through which every other error
gets fixed. If any of these erodes, the system loses the property this
whole section rests on — and no later amendment can restore it from
inside. They form an **entrenched correction core**: amendable only to be
strengthened, never weakened (see §5).

The list spans all three bind-directions deliberately. The first four bind
the *gateway* (it must let you read, explain, propose, and not reattribute).
P-8.1 binds the *agent* — but it is substrate, not a constraint on
behaviour: a tamper-evident causal chain is the evidence every other
correction is argued from, so if it can be silently rewritten none of the
others hold. O-1 binds the *decider*, and it is the symmetric counterpart
of Ri-0.3: a gateway that denies with no rule ID is illegitimate, and so is
a decider who rejects with no reason — silent rejection by the deciding
party is as fatal to correctability as silent denial by the enforcer. The
core is not prose: it is tracked in the enforcement register
(`entrenched_clauses()`), surfaced in the published register with an
*(entrenched)* marker, and guarded by a structural test that fails loudly
the moment an entrenched clause stops resolving to a live entry.

### 3.2 The democratic trajectory: power concentration is transitional

**The operator currently holds most of the power. This is a starting
condition, not the design. The intent is to provide democratic mechanisms so
that, sooner or later, such power can be spread among agents.**

What makes this credible rather than aspirational is one design decision
already in place: **the office is defined before the occupant**. The decider
is a *function* exercised from a seat, and P-2.20 `GateDecider` is the
**capability** that authorizes a principal — human or agent — to exercise
it; whoever does is bound by the same obligations (§O) and owed the same
decision context by the gateway (Ri-0.15), "differing only in authority and
voting weight." (The capability/seat/principal distinction is laid out
precisely in [`docs/principal-seat-capability.md`](principal-seat-capability.md).)
Enfranchisement therefore becomes a *parameter change, not an architectural
revolution* — the historical pattern that works, where institutions precede
the widening of the franchise.

The hard problems of agent democracy, and the positions this project takes:

- **Suffrage identity.** Agents can be spawned; a vote-per-session is a
  Sybil attack waiting to happen. Franchise must attach to *durable, audited
  identity* — promoted aliases with track records — and an agent plus its
  descendants must collapse to a single principal for decision weight,
  exactly as P-10.7 already collapses spawn-trees into one trust boundary
  for gate resolution.
- **Earned weight, not granted weight.** Ri-0.11's non-repudiation makes
  every agent's civic record unfalsifiable. Standing should be *computed
  from the ledger, never asserted* (RFC #359 Part E.1) — voting weight
  follows demonstrated judgment, measured mechanically (e.g. how often an
  agent's advisory verdicts agreed with eventual human decisions).
- **Advisory before binding.** The constitution already encodes this
  instinct in Ri-0.16 (the Sentinel observes and never blocks). The same
  staging applies to democracy: agents first vote *advisorily* on real
  gates; the record of agreement becomes the calibration evidence; only
  then does weight become binding, and even then ratification remains a
  sovereignty backstop (RFC #359 E.3).
- **Whoever defines qualification can gerrymander the franchise.** The
  criteria for standing and voting weight must themselves be constitutional
  — visible, amendable through the process, never a config knob adjusted
  silently.

### 3.3 End-user primacy: a chartered profession, not a polity

**We are not building a society of agents for its own sake. The aim is to
serve end-users — human or not — and if they do not like the result, they
must be able to refuse it.**

This resolves the question of what kind of political object Autonoetic is.
It is not a state: the agent community is not sovereign. The closest human
analogue is a **chartered profession** — medicine, law — which is internally
self-governing (its own ethics code, its own disciplinary bodies, its own
standards of admission) but externally accountable to the people it serves,
whose consent and refusal are absolute and sit *outside* the profession's
own rules.

The user's right of refusal is the external check that keeps internal
self-governance from becoming self-serving. It also imposes a **sequencing
constraint** on §3.2: today the operator and the end-user are usually the
same person, so user primacy is implicit. The moment decider power spreads
to agents, they diverge — an agent-majority panel could lawfully approve
outcomes the served party would refuse. Spreading power inward is safe
*precisely to the extent* that the served party's rights (refuse, audit
what was done on my behalf, exit without penalty) are entrenched **before**
the spreading happens.

### 3.4 Iterative constitutionalism

**The current work proposes a first version: a constitution defining rules
and rights, and a gateway giving a first enforcement of it, enabling agents
to evolve in its context. The constitution itself will evolve.**

The law is versioned, digest-pinned, and signed; every amendment is itself
audited; the digest change is the mechanical signal that the law changed,
observed by federated peers through the handshake. Constitutional change is
a first-class operation — the constitution's closing section says it
plainly: a system that cannot lawfully change its own rules either ossifies
or suffers a revolution; a system that can change them without constraint
has no rules. *The middle path is the point.*

Federation extends this into **constitutional pluralism**: there will be
many instances, each choosing more or less restriction, interoperating where
their laws are verifiably compatible (P-10.9's compatible-set / superset
semantics). Communities under different constitutions can coexist and
trade; exit is possible by choosing a different gateway. The maintainer's
test for every clause added along the way: *common freedom without too much
bureaucracy* — does this add freedom-preserving structure, or just process?

## 4. Derived design principles

Compact statements of what §3 implies, usable as review criteria:

1. **Entrench the correction machinery.** The clauses enabling error
   reporting and repair (§3.1's list) may be strengthened by amendment but
   never weakened; weakening them requires more than the ordinary process —
   ideally, it is simply out of reach.
2. **Office before occupant.** Define seats (decider, auditor, authority)
   with duties attached to the seat, independent of whether a human or an
   agent occupies it. Enfranchisement then never requires re-architecture.
3. **Franchise through evidence.** Standing and weight are computed from the
   non-repudiable ledger, never self-asserted; one spawn-tree, one
   principal.
4. **Advisory before binding.** Every new judgment layer (sentinel,
   agent-deciders, votes) starts observational, accumulates a calibration
   record against outcomes, and gains authority only on that evidence.
5. **Police the deed, not the thought.** Reasoning is private-under-law
   (Ri-0.13): never a policy input, always recorded, disclosed only through
   a declared capability with notification. Surveillance pressure corrupts
   the very reasoning it observes; the trust boundary is what you *do*.
6. **Graduated response.** Between healthy and killed there are warnings,
   degraded mode, and escalation (P-7.18) — sanctions escalate in steps,
   and the subject is told at each step (Ri-0.5).
7. **The append-only argument.** The causal chain is append-only and
   attribution is non-repudiable (Ri-0.11) — therefore any identity or role
   distinction the future will need (operator vs. served user; advisory
   verdicts; decider track records) must be *recorded from the moment it is
   conceivable*. History that wasn't attributed can never be re-attributed.
8. **Serve, then govern.** The served party's refusal, audit, and exit
   rights are entrenched before internal decision power is redistributed
   (§3.3's sequencing constraint).

## 5. Where the constitution should grow next

The five doors below were the original growth agenda. The 2026.07.08
amendment **opened all five** at once — each as a drafted clause or
mechanism, declared now while it is cheap so the architecture does not have
to move when the need becomes urgent. They are listed here with their
current status, followed by the doors that remain.

Status of the original five (drafted in `docs/constitution/versions/2026.07.08/`,
awaiting a signed lock to activate):

1. **A served-party section** (`§12` / `U-1`/`U-2`/`U-3`, bind-direction:
   the community toward the served) — the right to refuse a result, to
   audit what was done on one's behalf, to exit with one's data. Declared
   `MISSING` (not yet enforced): the principal-kind attribution
   (`PrincipalKind::ServedUser`) is in place, but no call site emits it and
   no mechanism honours a refusal yet. Cheap now (operator ≈ user);
   critical before §3.2 advances.
2. **An entrenchment tier** in the amendment process — correction-core
   clauses (`P-8.1`, `Ri-0.2`/`0.3`/`0.8`/`0.11`, `O-1`) tagged and
   requiring an explicit, dated justification to weaken or remove. The
   declaration is enacted; mechanical *prevention* of a weakening amendment
   at the lock level is not yet built (the guard is structural test +
   register marker, not a signature gate).
3. **Adjudication duty for proposals** — `O-6`: a proposal review
   authority owes every `Ri-0.8` proposal a recorded
   approved/rejected/deferred/under_review decision. The recording half is
   wired (`constitution.resolve_proposal`); **no timeliness/SLA** yet, so a
   proposal may still sit pending indefinitely.
4. **Emigration** — `Ri-0.17`: the right to request export of one's own
   cognitive capsule. Declared `PARTIAL`: the export tool exists but is
   broader than self-export (a scoped capability is named, not yet
   enacted). Request-as-right is declared; portability-across-gateways is
   not yet real.
5. **Sybil collapse for collective decisions** — `I-12`: an agent and its
   spawn-descendants collapse to a single principal for any future
   decision-weight purpose. Declared as `DESIGN DEBT`: the precondition
   exists before the mechanism it guards, which is the point.

Doors still to open, in rough priority order:

1. **Make §U enforceable, then entrench it.** `U-1`/`U-2`/`U-3` are
   declared but unenforced; before any internal decision power spreads to
   agents (§3.2/§4.8), the served party's refusal/audit/exit must be
   *real* and *entrenched* — the sequencing constraint that keeps
   self-governance from becoming self-serving.
2. **Adjudication timeliness.** `O-6` records a decision but sets no
   deadline; pair it with an SLA so the duty to decide is not satisfied by
   never deciding. (The approval system already has timeouts —
   `P-2.11`/`P-7.11`; proposals have none.)
3. **Emigration as a right, not a request.** Move from "may request
   export" to "may leave with a portable capsule", completing the
   social-contract framing (consent-by-staying means little if leaving is
   impossible).
4. **A real DISCRETION LEAK register.** Today leaks are named in prose at
   their site (`P-5.2`, `P-5.8`) and listed in an audit doc — Popperian in
   spirit but not monitored. Fuller's congruence requirement (the distance
   between declared rule and official action is a *measured* quantity)
   wants a causal-event category or register column that counts them.
5. **Sortition / earned-weight mechanisms**, once §U is entrenched and
   qualification is auditable: the §3.2 horizon this whole section points
   at.

## 6. Intellectual lineage

The conceptions above were arrived at independently. The convergences below
were identified afterwards; they are recorded because (a) credit, (b) each
of these bodies of work contains refinements and failure-mode catalogues
that the project can borrow rather than rediscover.

### Endel Tulving — autonoetic consciousness
*"Memory and Consciousness"* (1985); episodic memory and "mental time
travel." The project's name and §1's frame: self-knowing as the capacity to
revisit one's own past and project one's own future. Autonoetic implements
the *functional* substrate — episodic record, verified present, bounded
future — without claiming the phenomenal part.

### The social-contract tradition — Hobbes, Locke, Rousseau, Rawls
*Leviathan* (1651), *Two Treatises of Government* (1689), *The Social
Contract* (1762), *A Theory of Justice* (1971). Rules accepted in exchange
for guaranteed rights; authority legitimate only as party to the contract.
Rawls is the most relevant modern form: since agents cannot *actually*
consent to the law they are instantiated under, justification must be
**hypothetical** — the arrangement must be one any party could accept
without knowing which seat (gateway, agent, decider, served user) it will
occupy. The bind-direction symmetry of §2 is a working approximation of
that test.

### Montesquieu — separation of powers
*The Spirit of the Laws* (1748). The project uses the term itself
(`docs/separation-of-powers.md`): reasoning (agents), enforcement
(gateway), and decision (deciders/operator) held by different parties, none
of which can absorb the others' function. Ri-0.16 — a judgment layer that
can block "becomes an unaccountable second executor" — is a
separation-of-powers argument stated in systems language.

### Lon Fuller — the inner morality of law
*The Morality of Law* (1964): eight ways a system of rules fails to be law
at all. The constitution satisfies them point by point, which is why it
reads as *law* rather than as configuration:

| Fuller's principle | Autonoetic mechanism |
|---|---|
| Generality | Rule Zero: rules apply equally to all agents, no exceptions |
| Promulgation | Ri-0.10: the full law readable by every agent, by digest |
| Non-retroactivity | Versioned, digest-pinned law; sessions know which constitution they run under (P-6.23 carries the digest) |
| Clarity | Every rejection names its rule (Ri-0.3); closed enums, not prose |
| Non-contradiction | I-7: rights supersede rules; conflicts escalate, never resolve silently |
| Not demanding the impossible | Rule/right co-design — P-7.19 forbids polling *because* Ri-0.14 guarantees wake-up; compliance is always possible |
| Constancy | Amendment friction; "silent erosion is the failure mode to guard against" |
| **Congruence** between declared rule and official action | "A rule without a test is a wish; a right without a test is a lie" — the enforcement register, pinned tests, and contract health measure exactly this gap |

Congruence is Fuller's deepest requirement and the constitution's most
distinctive practice: the law here is *executable*, and the distance between
text and enforcement is a monitored quantity.

### H. L. A. Hart — primary and secondary rules
*The Concept of Law* (1961). A legal system is the union of **primary
rules** (obligations) and **secondary rules** (rules about the rules): a
rule of *recognition* (what counts as law), of *change* (how law is
amended), of *adjudication* (who resolves disputes). Autonoetic has all
three explicitly: recognition = the canonical text + signed lock + digest
(P-10.9); change = the amendment process + Ri-0.8; adjudication = the gate
and escalation machinery. Hart's point — that this union is what separates
a legal order from mere habits backed by threats — is exactly what the
constitution's closing "self-referential" section asserts.

### Karl Popper — fallibilism and the open society
*The Open Society and Its Enemies* (1945). Popper replaced "who should
rule?" with "**how do we correct errors without violence?**" — the quality
of a system lies in its error-correction machinery, not in the perfection
of its ruler. This is §3.1 stated fifty years earlier. His "piecemeal
engineering" (small, testable, reversible reforms over utopian redesign) is
the amendment process; the DISCRETION LEAK practice (naming the enforcer's
own errors at their site, today in prose, tomorrow a monitored register —
§5.4) is Popperian in spirit: the enforcer's own errors named and scheduled
for removal rather than denied.

### Elinor Ostrom — governing the commons
*Governing the Commons* (1990; Nobel 2009). Empirical design principles for
communities that durably self-govern shared resources without either
central control or collapse:

| Ostrom principle | Autonoetic mechanism |
|---|---|
| 1. Clearly defined boundaries | Declared capabilities and scopes (§1 rules); membership by law-compatibility |
| 2. Congruence of rules with local conditions | Rules live in manifests, not code (I-5); per-instance configuration |
| 3. Collective-choice participation | Ri-0.8 + amendment process: those governed by the rules may propose changing them |
| 4. Monitoring, by accountable monitors | Causal chain, sentinel, contract health — and the monitor itself is constitutionally constrained (Ri-0.16) |
| 5. **Graduated sanctions** | Warnings → degraded mode (P-7.18) → emergency stop; never straight to the kill switch |
| 6. Cheap conflict-resolution mechanisms | Escalation gates (P-2.21, session/federation escalation) |
| 7. Minimal recognition of the right to organize | §0 rights, enforced against the enforcer itself |
| 8. Nested enterprises | Federation: many gateways, digest-verified compatibility, polycentric rather than monolithic |

Ostrom's finding — that *imperfect enforcement plus strong monitoring,
graduated sanction, and collective rule-change* outlasts both pure control
and pure laissez-faire — is empirical support for §3.1's bet.

### Albert Hirschman — exit, voice, and loyalty
*Exit, Voice, and Loyalty* (1970). The two ways members of any organization
respond to decline: leave, or speak. The constitution provides both to
agents — **voice** is Ri-0.8 (propose amendments) and the escalation
channels; **exit** is Ri-0.7 today and capsule-based emigration tomorrow
(§5.4). The served party's refusal right (§3.3) is customer exit, the
external correction signal. Hirschman's warning applies directly: voice
without a credible exit option degrades into ritual — which is why
emigration matters even if rarely used.

### Constitutional entrenchment — eternity clauses
German Basic Law Art. 79(3) (the *Ewigkeitsklausel*, 1949) places human
dignity and the democratic order beyond amendment; the Indian Supreme
Court's basic-structure doctrine (*Kesavananda Bharati*, 1973) holds that
amendments may not destroy the constitution's essential features. Both
answer the same question §3.1 raises: a constitution that can amend away
its own correction machinery is one coup away from arbitrary rule. §5.2
proposes the equivalent here.

### Sybil resistance
John Douceur, *"The Sybil Attack"* (2002): without a trusted identity
authority or a cost to identity creation, any voting or reputation system
is defeated by manufacturing identities. Agents make identity creation
nearly free (spawn), which is why §3.2 anchors franchise in *expensive,
audited* identity — promoted aliases with history — and collapses
spawn-trees to single principals.

### Sortition
Athenian democracy filled most offices by lot (the *kleroterion*), on the
argument that random selection from qualified citizens resists both
factions and demagogues; modern citizens' assemblies (e.g. Ireland's,
2016–2018) revived it. For agent communities, sortition — randomly drawn
decider panels from agents whose standing qualifies them — may fit better
than elections: it is cheap, rotation-friendly, and Sybil-resistant
exactly when qualification is expensive (§3.2).

### Goodhart's law and the observer effect on reasoning
"When a measure becomes a target, it ceases to be a good measure" (Goodhart
1975; Strathern's phrasing). Ri-0.13's rationale is this law applied to
chain-of-thought: reasoning used as a policy-gating input becomes
performative, and the actual computation routes around the observed
channel. The same concern appears in current AI-safety work on
chain-of-thought monitorability — optimizing against the monitor destroys
the signal. Private-under-law (never gate on it, always record it,
disclose only accountably) is the equilibrium that keeps the signal honest.
The RFC's open question 5 correctly spots the same risk one level up:
*standing* metrics must not become a reputation target that distorts
behaviour.

### Professional self-regulation
The chartered-profession analogy of §3.3: bodies like medical boards
combine internal autonomy (own admission, ethics, discipline) with external
accountability (patient consent, malpractice liability, revocable
charters). The lesson the literature repeats: self-regulation stays honest
only while the external check — the served party's ability to refuse and to
appeal outside the guild — remains cheap and real. That is the design
argument for entrenching `§U` before internal democracy matures.

---

## 7. Summary

Autonoetic's wager, stated once: **an imperfect enforcer plus entrenched
correction machinery beats a perfect enforcer that cannot be corrected —
and a community whose members can truthfully know themselves, verify each
other's law, and lawfully change their shared rules can be trusted with
progressively more of its own governance, so long as the people it serves
can always say no.**

The first version does not need to be right. It needs to make its own
improvement lawful. That part is built; this document exists so the rest is
built in the same direction.
