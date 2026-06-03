# Principal Model, Symmetric Obligations, and Authored Law

> **Status:** Draft RFC (2026-06-02) — design exploration, not yet a proposal.
> Parts A–B are concrete near-term work; Parts C–D are direction-setting and
> deliberately phased. Nothing here amends the constitution; constitution text
> and lock signing remain the authoritative entity's to ratify
> (see [`../constitution-signing.md`](../constitution-signing.md) and the
> "actors-as-citizens" framing in
> [`../autonoetic-concepts-for-beginners.md`](../autonoetic-concepts-for-beginners.md)).

---

## Motivation

The runtime is moving from "agents are low-privilege reasoners the gateway
contains" to "**actors are first-class citizens under a shared law**, where an
actor may be an AI agent, a human operator, or a script." The beginner doc now
states that paradigm. The constitution does not yet *implement* it.

The gap is visible in the current bind-direction (constitution §0 preamble):

> Every clause binds exactly one party. A **right** (`Ri-*`) binds the
> **gateway**. A **rule** (`P-*`) binds the **agent**. The bind-direction is
> uniform by section.

There is **no section that binds a human operator or decider**. An agent owes a
reason for every rejection (Ri-0.3 — actually the gateway owes it on the agent's
behalf), is attributable for every action (Ri-0.11), and may terminate only for
declared reasons (Ri-0.12). A human decider, by contrast, can reject a gate
silently, with no recorded reason, under no rate constraint. That asymmetry is
not a citizenship model — it is "agents are subjects, humans are unconstrained
authority." If we mean the paradigm, the law has to bind whoever acts.

This RFC proposes a near-term step, an enabling layer, and a horizon — in that
priority order:

- **Part A — The Principal model.** *(near-term)* One abstraction for any actor,
  carrying identity, capabilities, rights, and obligations, with an orthogonal
  `kind`.
- **Part B — Symmetric obligations.** *(near-term)* Clauses that bind *deciders*
  (human or agent), mirroring the obligations agents already carry. **Giving
  humans and AI the same rights and duties is the concrete first step** — the
  democratic frame is explicitly *not*.
- **Part C — Authored law.** *(enabling)* Distinguishing *authorship*,
  *contributor attestation* (sign-off), and *authoritative ratification* — so AI
  and human contributors can author and stand behind the constitution while the
  ratifying authority remains who the instance configures it to be.
- **Part D — Authority: role, cardinality, and domain.** *(enabling — schema
  only, leave doors open)* Authority is a configurable role; an instance may have
  one or several authorities; ratification is only *one* domain of authority,
  with judicial/sanction authority a door to leave open.
- **Part E — Toward a democratic frame.** *(horizon — do NOT build now)*
  Standing, time-bounded extended rights, voting, external-law interop. Gated on
  AI maturity and instance experience; described only so we don't foreclose it.

### Invariants this RFC must not break

0. **Pluralism — Autonoetic is not (yet) a single ecosystem.** There will be
   *many instances*, each choosing more or less restriction. One instance may run
   under a single authoritative entity; another, involving more actors, may
   require several authorities. Governance is therefore **per-instance
   configuration**, not a global polity. No design here may hardcode
   "one ecosystem" or "exactly one authority" assumptions — single-authority must
   be one *configuration*, not a baked-in law. (This mirrors federation: OFP
   already negotiates differing constitution digests between sovereign peers.)
1. **Sovereignty over the frame stays with the instance's configured
   authority.** Equal civic standing in *interaction*; constituent power (ratify
   amendments, ultimate escalation) stays with whoever the instance designates —
   one human today, possibly several authorities elsewhere. The principal model
   must not flatten that authority into "just another agent."
2. **Lawful Executor (§14).** The gateway enforces pre-committed law
   deterministically; it does not gain discretion. New obligations must be
   mechanically checkable, not "the gateway judges reasonableness."
3. **Rights are the floor (§0).** Symmetric obligations add duties to deciders;
   they never narrow an agent's existing rights.
4. **Leave doors open.** Prepare the future without building it. Where a future
   need is foreseeable (multiple authorities, judicial sanction, voting), the
   near-term deliverable is a *schema/role shape* that admits it later — defaulting
   to today's simple case — never the mechanism itself, and never a door welded
   shut.

---

## Part A — The Principal model

### A.1 Today

Identity is fragmented by kind:

- **Agents** have `agent_id`, a manifest, declared capabilities, rights (§0),
  and non-repudiable attribution (Ri-0.11).
- **Operators** are a *role* a decider fills (the beginner doc and §2/§19 already
  say "operator is a role, not necessarily a human"), but they have no manifest,
  no declared obligations, and only a `decider identity` stamped on gate events.
- **Policy engines / scripts** appear as deciders too, with even less structure.

The vocabulary ("low-privilege reasoner" vs "operator") quietly encodes
subordination of the AI rather than equal citizenship.

### A.2 Proposal — a unified `Principal`

Introduce a single principal abstraction. Conceptually:

```text
Principal
  id            stable, portable identity (survives sessions; see capsules)
  kind          ai | human | script        # orthogonal attribute, not a privilege tier
  capabilities  what it may request/do      # already exists for agents; generalize
  rights        what it is owed             # §0 today binds gateway "for every agent" → "for every principal"
  obligations   what it must do/not do      # NEW for deciders (Part B)
  standing      accrued track record        # Part D; null/■ in Phase 1
```

Key properties:

- **`kind` is orthogonal to authority.** A human is not more privileged *because*
  human; humans are sovereign over the *frame* (amendments/escalation), but in
  day-to-day interaction a `human` principal and an `ai` principal are bound by
  the same obligation clauses and owed the same interaction rights.
- **Attribution is uniform.** Every causal event already carries `actor_id`
  (Ri-0.11). Generalize "agent_id" → "principal_id" so a human's gate decision
  and a script's policy verdict are first-class, non-repudiable causal actors —
  not metadata bolted onto a gate row.
- **`self_describe` becomes principal-describe.** A human operator should be able
  to ask the same question an agent asks: *what may I do, what am I owed, what am
  I obligated to do here, what have I done?* Symmetric tooling for symmetric
  citizenship.

### A.3 Migration sketch (pre-release — break freely)

- Rename `agent_id` → `principal_id` on the causal chain and gate records;
  `kind` defaults to `ai` for existing agents, `human` for CLI operators,
  `script` for policy-engine deciders. No alias shim (per project convention).
- §0 preamble wording "for every agent" → "for every principal" where the right
  is genuinely kind-agnostic (most are: read your own chain, get reasons, exit).
  Some rights stay agent-specific (e.g. Ri-0.14 child wake-up) — those are about
  the *reasoner* role, not the principal, and should be tagged role-scoped.

---

## Part B — Symmetric obligations

### B.1 The bind-direction problem

Today bind-direction is *uniform by section* — clean, but it has exactly two
parties (gateway, agent). Deciders are a third. Two options:

- **(B-i) New numbered section that binds deciders.** e.g. a `§O. Decider
  Obligations` block with IDs like `O-*`, bind = decider (human or agent-decider
  or policy engine). Preserves "uniform by section."
- **(B-ii) Per-clause `bind:` tag.** Drop uniform-by-section, tag every clause
  with its bound party. More flexible, but loses the elegant "you know who's
  bound by where you are" property the constitution currently advertises.

**Recommendation: B-i.** It keeps the section-as-bind-direction invariant the
§0 preamble relies on, and it makes "deciders have duties" legible as its own
chapter rather than scattered tags. Open question below on numbering.

### B.2 Candidate decider obligations (mirrors of existing agent duties)

| Proposed | Mirrors | Obligation on a decider (human or agent) |
|---|---|---|
| `O-1` Reasoned decision | Ri-0.3 (agents get reasons) | A rejection (and ideally an approval) records a reason. Silent denial by a human is as illegitimate as a gateway "denied" with no rule ID. |
| `O-2` Attribution & non-repudiation | Ri-0.11 | Every decision is attributed to the decider principal on the causal chain and cannot be reattributed. (Largely exists for gates; make it a *duty*, not an implementation detail.) |
| `O-3` Anti-fatigue / rate discipline | abuse/circuit-breaker §7 | Decisions are subject to rate + pattern checks; a burst of rubber-stamp approvals is flagged (the beginner doc already cites "an approval pattern that looks like fatigue" as a sensor signal — make it a named obligation, not just an observation). |
| `O-4` Scope honesty | approval-scope rules (§2) | An approval authorizes a specific execution path under stated conditions; a decider may not record an approval broader than what was reviewed. |
| `O-5` Duty to escalate, not silently reject | Ri-0.9 / human-sovereignty | An agent-decider that cannot resolve a gate must escalate to a human rather than reject. (Already a stated principle; promote to a decider obligation so it binds mechanically.) |

All five are **mechanically checkable** — reason-present, attribution-present,
rate-within-bound, scope ⊆ reviewed, escalation-on-unresolved — so they respect
the Lawful Executor invariant. None requires the gateway to judge whether a
decision was *wise*, only whether the decider met the procedural duty.

### B.3 What this buys

The "actors watch each other, both directions" claim in the beginner doc (§15)
becomes real: an agent's right to a reason (Ri-0.3) now has a counterpart duty
on whoever decides (O-1), and reciprocal accountability stops being rhetorical.

### B.4 Graduated motivation policy (operationalizing O-1)

O-1 says a rejection "(and ideally an approval)" records a reason. "Ideally" is
not mechanically checkable — so this section makes O-1 concrete as a **graduated**
duty: *when* a reason is owed, never *whether it is good*. Motivated by the
Session Room UX requirement (`docs/rfc/session-room-channel-agnostic-timeline.md`
§3.5) that accountability must not become tedium: the obligation must bind hardest
exactly where a "why" matters, and fall away where it would be busywork.

Three obligation tiers on a gate decision:

| Tier | Duty | Mechanism |
|---|---|---|
| **BLOCKING** | A non-empty motivation is required *at decision time* | The decision does not commit until a reason (free-text, or a labeled pre-digested choice carrying one) is attached. |
| **DEFERRED** | A motivation is *owed* within a bounded window | Commits immediately; auto-discharged if the decider typed free-text; unmet by deadline ⇒ a recorded **obligation-gap** event (surfaced, not punitive — tracked like a discretion leak). |
| **OPTIONAL** | None | Informational acks; answering an agent's clarification. |

**Tier classifier — a pure function of facts the gateway already holds** (no
judgment, Lawful Executor):

```
tier(decision, gate) =
  BLOCKING  if decision.polarity ∈ {Reject, Deny, Abort}          // ← exact mirror of Ri-0.3
         or decision.is_override                                   // bypasses default/safe choice — generalizes governor `force_reason`
         or gate.required_authority != Operator                    // ApprovalLevel::Admin | Agent(id)
         or action_class(gate.action) == ExternalOrIrreversible
  DEFERRED  else if polarity == Approve and required_authority == Operator
                 and action_class == Local
  OPTIONAL  else
```

`action_class` is a small pure map over existing `ScheduledAction` variants:
- **ExternalOrIrreversible:** `WebFetch`, `WebCall`, `CredentialRequest`,
  `CredentialPrompt`, `AgentInstall`, non-workspace / destructive `WriteFile`.
- **Local:** `SandboxExec` without network, workspace-scoped `WriteFile`.

**Why these triggers.** Each reuses an existing accountability precedent rather
than inventing one: rejection ⇒ the exact symmetric mirror of Ri-0.3; override ⇒
the governor's `force_reason` already requires a justification on bypass
(`agent_revision.rs`); elevated authority ⇒ `ApprovalLevel::Admin`; external /
irreversible ⇒ where the audit trail most needs intent. The common path —
approving a local, reversible, operator-level action — is DEFERRED/OPTIONAL, i.e.
one tap, no reason.

**Operator-as-AI corollary.** When an AI principal holds a deciding seat, reason
cost is ≈0, so policy may collapse all tiers to BLOCKING for that decider (an AI
decider always motivates now). This is a per-instance authority choice (Part D),
not a constitutional constant.

**Proposed §O addition.** O-1 is refined to name the graduated duty; the tiers
and `action_class` membership are **config, not constitution** (the
don't-pin-tunables rule — the constitution binds the *mechanism*; thresholds and
the action map live in code + `config-reference.md`). The constitution change is
**prepared unsigned**; the configured ratifying authority recomputes and signs
the lock (Part C / `constitution-signing.md`). This RFC does not alter the lock.

| Refined | Mirrors | Mechanically-checkable duty |
|---|---|---|
| `O-1a` Reason-on-reject | Ri-0.3 | A `Reject`/`Deny`/`Abort` decision carries a non-empty motivation at decision time. |
| `O-1b` Reason-on-override | governor `force_reason` | A decision that bypasses a default/safe choice or a safety gate carries a motivation at decision time. |
| `O-1c` Reason-on-stakes | approval-scope §2 | A decision on an elevated-authority or external/irreversible action carries a motivation at decision time. |
| `O-1d` Deferred reason + debt | — | Approvals of local, reversible, operator-level actions may defer the motivation to a bounded window; unmet motivation is recorded as an obligation gap, not blocked. |

These slot under the existing O-1 (Reasoned decision) and inherit O-2's
attribution (`decider_kind`, #361) and O-3's rate discipline unchanged.

---

## Part C — Authored law: authorship, attestation, ratification

The maintainer's observation: *the constitution and code are not written by a
single agent; I sign as the entity that initiated the project and maintains
global consistency — but the actual author(s), AI or human, should be able to
sign in some way too.*

This is three distinct things, currently collapsed into one:

| Concept | Question it answers | Who | Mechanism (proposed) | Authoritative? |
|---|---|---|---|---|
| **Authorship** | Who wrote/contributed this clause? | any principal (AI/human) | `author` / `co-authored-by` attribution on the amendment record + commit trailer | No |
| **Contributor attestation (sign-off)** | Who authored this text and stands behind it? | the contributing principal | a *detached, non-authoritative* signature — for an AI contributor, its **revision key** (see [`../revision-signing.md`](../revision-signing.md)); DCO-style `Signed-off-by` for humans | No — attestation, not enactment |
| **Authoritative ratification** | What text is *enacted as enforced law*? | the instance's **configured ratifying authority** (one entity today; possibly a threshold of several elsewhere — Part D) | the existing **Ed25519 lock signature** over the constitution digest (`constitution-signing.md`); generalizes to a signer set / threshold | **Yes — the only signature(s) that make it law** |

### C.1 Why keep them separate

- **Ratification ≠ authorship.** This mirrors real lawmaking: a bill has authors
  and co-sponsors who sign on, but it becomes law only when the authoritative
  body enacts it. Collapsing the two would either (a) let any contributor enact
  law (no sovereignty) or (b) erase contributor recognition (no citizenship).
- **AI contributors become recognized authors.** An AI that authored a clause
  can attest to it with its revision key. That attestation is verifiable and
  recorded, *without* granting it the power to enact. Recognition without
  sovereignty — exactly the Part-A property.
- **The lock stays untouched.** Part C adds *attestation* metadata alongside the
  amendment proposal; it does **not** change the signed lock payload defined in
  `constitution-signing.md`. The authoritative signature remains whatever the
  instance configures — today a single fail-shut signer (`trusted_signers` for
  `autonoetic:constitution:v1`), generalizable to a signer set under Part D.

### C.2 Concrete shape (Phase 1)

- Each amendment proposal (`constitution_propose_amendment`, Ri-0.8) gains:
  `authors[]` (principals), optional `attestations[]` (principal_id + detached
  signature over the proposal digest).
- The constitution version directory gains a non-authoritative
  `authorship.json` (authors + attestations) sitting *beside* the authoritative
  `gateway-constitution.lock.json`. Startup verification ignores `authorship.json`
  (it is recognition metadata, not enforcement input) — keeping the lock's
  verification surface unchanged.
- A clause-level `Authored-by:` / `Attested-by:` convention in the amendment
  history, so the causal record of *who shaped the law* is durable.

> **Boundary, restated:** I (the AI) may **author** and **attest**; I do **not**
> sign the lock. The instance's configured authority ratifies — one entity today.
> This RFC keeps that line bright.

---

## Part D — Authority: role, cardinality, and domain (leaving doors open)

The maintainer's framing: *for now ratification just concerns the constitution
and one instance may have a single authoritative entity — but others, involving
more actors, will need several authorities. And tomorrow, when the security
sentinel finds an agent misbehaving, someone must decide whether it is degraded
or banished. Prepare the future; leave doors open; don't weld anything shut.*

The deliverable here is **schema, not mechanism**: make authority a configurable
role so richer arrangements are *possible later*, while every instance defaults
to today's simple case.

### D.1 Authority is a role with a cardinality

Generalize the single lock-signer into an **authority role** an instance
configures:

```text
authority:
  domain:     constituent | judicial | operational   # what kind of decision
  members:    [principal_id, ...]                     # who holds it
  threshold:  k of n                                  # how many must concur
```

- **Today's instance = `{domain: constituent, members: [maintainer], threshold: 1}`.**
  Nothing changes operationally; we just stop hardcoding "one signer" as a law.
- **A multi-actor instance** can require `k of n` — a threshold of authorities
  must co-sign to ratify. The existing `trusted_signers` config and the Ed25519
  lock are the natural substrate (verify *k* valid signatures instead of one).
- Single-authority is the `n=1, k=1` special case, not a separate code path.

### D.2 Authority has *domains* — ratification is only one

"Authoritative" today means one thing: ratify a constitution amendment. The
sentinel question shows it is really several distinct powers that an instance may
unify (small instance) or separate (larger one):

| Domain | Decides | Today | Door to leave open |
|---|---|---|---|
| **Constituent** | What the law *is* (amendments) | the lock signer | threshold of signers (D.1) |
| **Judicial / enforcement** | Sanctions on a *principal* — degrade, suspend, **banish** | *(none — degraded mode P-7.18 is mechanical, not adjudicated)* | a configured authority adjudicates sentinel findings |
| **Operational** | Time-bounded grants, routine approvals | operators / gate deciders | unchanged; standing-gated later (Part E) |

Separating these matters: the entity that *writes the law* need not be the entity
that *judges a violation of it* — and conflating them is exactly the
concentration of power the constitution otherwise guards against.

### D.3 The sanction flow (the sentinel question, sketched — not built now)

When the security sentinel
([`../security-sentinel.md`](../security-sentinel.md),
[`divergence-sentinel-design.md`](divergence-sentinel-design.md)) detects an agent
doing bad things, *who decides it is degraded or banished?* The Lawful-Executor
shape gives a clean separation that we should leave room for:

```text
sentinel  →  detects + gathers evidence, files a finding   (sensor / investigator — does NOT decide)
chain     →  finding + evidence recorded, attributable      (Ri-0.11)
authority →  judicial authority adjudicates: degrade | suspend | banish | clear   (decision, with reason)
gateway   →  enforces the decision mechanically             (Lawful Executor — does NOT judge)
subject   →  told the reason + has an appeal/escalation path (mirror of Ri-0.3 / Ri-0.9)
```

- The **sentinel proposes, it does not sentence.** Detection ≠ judgment — same
  reason an agent's LLM reasoning is input, not authority (§6.1).
- **Degrade** already exists mechanically (P-7.18). **Banish** is new: revoke a
  principal's standing/right to run *within this instance* — necessarily bounded,
  recorded, reasoned, and appealable, never silent. (Note: banishment is
  *instance-scoped* by Invariant 0 — a principal banished on one instance is not
  globally outlawed; another instance with different rules may admit it.)
- **Built now? No.** The only near-term move is reserving the `judicial` domain in
  the authority schema so the future adjudicator has a home — the mechanism waits.

### D.4 What is concrete now vs reserved

- **Now (door-opening):** the `authority` config schema (domain + members +
  threshold), defaulting to `{constituent, [maintainer], 1}`. Lock verification
  generalized to *k of n* but exercised at `k=n=1`.
- **Reserved (not built):** multi-authority instances, the `judicial` domain, the
  banishment mechanism, the appeal path. Named so they are not foreclosed.

---

## Part E — Toward a democratic frame (horizon — do NOT build now)

> **This is the horizon, and it must stay there for now.** The maintainer is
> explicit: we cannot and *must not* build this yet — **AI agents need to mature**
> first, and instances need operating experience. This section exists only so the
> near-term work (Parts A–D) does not accidentally foreclose it. Nothing here is
> scheduled.

The maintainer's longer vision: Autonoetic could become a **democratic
framework** where persistent entities — human or AI — can vote or be granted
**extended rights for a bounded duration**, collaborating with recognized laws
and public organizations to ensure *common freedom without too much
bureaucracy*. This part is direction, not near-term work; each step is gated on
the prior one being mechanically sound.

### E.1 Standing from track record

Persistent identity already exists (portable identity, cognitive capsules). Add
**standing**: a derived, auditable measure of a principal's track record from the
causal chain (clean terminations, upheld obligations, ratified contributions,
absence of violations). Standing is *computed from* the ledger, never asserted —
so it inherits the chain's non-repudiation and cannot be gamed by claim.

### E.2 Time-bounded extended rights

Generalize the existing session-grant pattern (`session_approval_grants`,
auto-expiring host approvals) up to the governance layer: a principal can be
**granted extended rights for a duration** (e.g. propose-and-fast-track during a
migration window; broader read scope for an audit), automatically expiring and
fully recorded. Bounded delegation, not permanent privilege — the same shape
that already works for network approvals, lifted to standing/rights.

### E.3 Voting on amendments

The amendment process today: propose (PR or Ri-0.8), test that fails-before /
passes-after, second-human or auditor sign-off, extra operator sign-off for
rights changes. A democratic layer would add **weighted voting** among
qualifying principals (gated by kind/role/standing) as an *input* to ratification
— while the **authoritative ratification (Parts C/D) remains the sovereignty
backstop** (one authority, or a configured threshold). Voting informs; the
configured authority enacts. (This is the most speculative step and the one most
in tension with Invariants 0–1 — flagged in Open Questions.)

### E.4 Interop with recognized external law

"Collaboration with recognized laws and public orgs" suggests the constitution
should be able to *reference and defer to* external legal/governance frames
(e.g. an org's policy, a jurisdiction's requirement) as named, versioned,
verifiable inputs — without importing their bureaucracy wholesale. Federation
(OFP) already negotiates constitution digests between peers; extend that to
"this node additionally honors external-frame X@version" as a declared,
auditable commitment. The design principle the maintainer named —
**common freedom without too much bureaucracy** — becomes a concrete test for
every clause here: *does this add freedom-preserving structure, or just
process?* If the latter, it doesn't belong (same bar as
[`constitutional-evolution-reflections.md`](constitutional-evolution-reflections.md)).

---

## Open questions

1. **Section numbering for decider obligations (B-i).** A new `§O` / `O-*`
   block, or fold into an existing section? Does adding a third bound party
   require updating the §0 preamble's "exactly one party / uniform by section"
   wording?
2. **Is O-1 (reasoned decision) a *right of the affected agent* or a *duty of the
   decider*?** It can be framed either way; the affected agent already has Ri-0.3
   for gateway rejections. Cleanest may be: the right already exists; B adds the
   *human/agent-decider* duty that makes Ri-0.3 hold for human-origin rejections.
3. **AI attestation key.** Reuse the revision key (`revision-signing.md`) as the
   contributor-attestation key, or mint a distinct "authorship" key per
   principal? Reuse is simpler and already trusted; a distinct key separates
   "this revision is authentic" from "this principal authored this clause."
4. **Voting vs sovereignty (E.3).** How much can voting bind ratification before
   it erodes Invariants 0–1? Proposed stance: voting is *advisory input recorded
   on the chain*; the configured authority (one signer, or a threshold) stays
   sovereign. Horizon-only — not now.
5. **Does `standing` risk becoming a reputation system that distorts behavior**
   (the Ri-0.13 "performative reasoning" failure mode, one level up)? Keep it
   derived-and-auditable, never self-asserted, and surface how it's computed.
6. **Authority threshold representation (D.1).** Does `k of n` lock-signing fit
   the existing `trusted_signers` / Ed25519 lock cleanly, or does multi-sig need a
   distinct lock schema version? (Schema only now; mechanism deferred.)
7. **Judicial domain & banishment (D.3).** Is banishment a constitutional clause
   (a new sanction with its own due-process rights), a config-level authority
   power, or both? And what is the appeal path — a right mirroring Ri-0.3/Ri-0.9?
   Reserve the domain now; design the mechanism only when AI maturity warrants it.

---

## Phasing

| Phase | Scope | Touches constitution? |
|---|---|---|
| **P1** *(near-term)* | Additive `Principal`/`PrincipalKind` types + typed decider kind on gates + consolidate scattered kind discriminators. **No `agent_id`→`principal_id` rename** (see Appendix). | No (code/types) |
| **P2** *(near-term)* | Decider obligations §O (O-1..O-5), mechanically enforced + tests — **the "same rights/duties for humans and AI" step** | **Yes** — prepared *unsigned*; configured authority ratifies |
| **P3** *(enabling)* | Authorship + attestation metadata (`authorship.json`, proposal `authors[]`/`attestations[]`); lock payload unchanged | No (additive metadata) |
| **P4** *(enabling — schema only)* | `authority` config (domain + members + threshold), default `{constituent,[maintainer],1}`; lock verify generalized to *k of n* at `k=n=1`. Reserve `judicial` domain — no mechanism. | Maybe (authority clause) |
| **P5** *(horizon — DO NOT build now)* | Standing; time-bounded extended rights; advisory voting; judicial/banishment mechanism; external-frame interop. **Gated on AI maturity + instance experience.** | Yes — most speculative; revisit Invariants 0–1 |

Per project convention, any constitution-text change is prepared in a new
**unsigned** version directory with failing→passing enforcement tests; the
instance's configured authority ratifies and signs the lock. The AI authors and
may attest; it does not sign the lock. **P5 is deliberately unscheduled** — it is
documented only so P1–P4 leave its doors open (Invariant 4).

---

## Appendix — P1 implementation plan (revised after code audit, 2026-06-02)

A code audit reshaped P1 substantially. **The original "introduce `Principal` and
rename `agent_id` → `principal_id` everywhere" framing is rejected as superfluous,
high-breakage churn.** Findings:

- The causal chain **already** binds a generic `actor_id` into the entry hash
  (`autonoetic-gateway/src/causal_chain.rs:304,317`) — distinct from `agent_id`.
  The "principal identity on the ledger" the rename was meant to create *already
  exists* at the only layer where identity integrity matters.
- Kind discrimination **already** exists, scattered: `ApprovalLevel { Operator,
  Admin, Agent(String) }` (`autonoetic-types/src/background.rs:673`) and
  `updated_by_type` (`agent_revision.rs:200`).
- `agent_id` (~3,900 sites) is genuinely *the agent* in the vast majority of
  uses, and it is part of the public wire contract (JSON-RPC params, Python/TS
  SDK signatures, HTTP API, SQLite columns). Renaming it is a breaking change
  that buys no semantic clarity the chain's `actor_id` doesn't already provide.

**Conclusion:** keep `agent_id`. The valuable, additive P1 is small and
non-breaking, and its real payoff is unblocking Part B (a decider's *kind* must be
recorded for symmetric obligations to bind). P1 breaks into:

| Issue | Scope | Breaking? |
|---|---|---|
| **P1.a** | Add `PrincipalKind { Ai, Human, Script }` + thin `Principal { id, kind }` in `autonoetic-types`. Pure addition. | No |
| **P1.b** | Record decider **kind** on gate decisions — `decided_by: Option<String>` carries no kind today (`background.rs:613`). Derive/store `PrincipalKind` alongside it (mapping from `ApprovalLevel`). This is the gap Part B (O-1/O-2 decider obligations) needs. | Additive field |
| **P1.c** | Consolidate scattered discriminators (`updated_by_type` strings, `ApprovalLevel`) onto `PrincipalKind`; **document** that causal-chain `actor_id` *is* the principal id (no rename). Cleanup/clarity. | No |

Explicitly **out of P1**: any `agent_id` rename; `self_describe`-for-operators
parity (needs an operator introspection surface that does not exist yet — defer to
its own design); the full capability/rights generalization (only matters once
non-agent principals actually carry capabilities — not now).

This keeps P1 honest with Invariant 4: it ships the *type shape* that lets later
phases (typed deciders → §O obligations → multi-authority) land, without building
mechanism or breaking the wire.

---

## Relationship to existing docs

- [`../constitution-signing.md`](../constitution-signing.md) — authoritative lock
  signing (unchanged by this RFC).
- [`../revision-signing.md`](../revision-signing.md) — revision keys, candidate
  basis for AI contributor attestation (Part C / OQ-3).
- [`constitutional-evolution-reflections.md`](constitutional-evolution-reflections.md)
  — the "would the system break without it?" bar this RFC's clauses must clear.
- [`constitution-gate-amendments.md`](constitution-gate-amendments.md) —
  agent-as-decider work that O-5 (duty to escalate) builds on.
- [`../security-sentinel.md`](../security-sentinel.md) /
  [`divergence-sentinel-design.md`](divergence-sentinel-design.md) — the detector
  whose findings the (future) judicial authority would adjudicate (D.3): sentinel
  proposes, authority sentences, gateway enforces.
- [`../autonoetic-concepts-for-beginners.md`](../autonoetic-concepts-for-beginners.md)
  — the actors-as-citizens framing this RFC operationalizes.
