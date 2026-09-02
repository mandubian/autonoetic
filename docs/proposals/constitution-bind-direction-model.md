# RFC: Bind-direction as data — making the constitution re-implementable

Status: **Open** (proposal — no code, no amendment)
Date: 2026-09-02 — **revision 2**, same day, after maintainer review
Scope: the family structure of constitutional clause IDs, and the `binds` model in
`enforcement_register.rs`. Supersedes nothing; blocks nothing.

> **Revision 2 changes** (recorded rather than hidden — the wrong versions are
> instructive):
>
> - `binds` values renamed to the three **powers** — `reasoner` / `enforcer` /
>   `decider` — because `{agent, gateway, decider}` mixed a principal kind, an
>   implementation artifact, and a seat into one enum (§2.1).
> - `owed_to` generalized from a bespoke 3-value enum to `PrincipalKind` tags —
>   the principal census evolves — plus one discovered case of **seat-standing**
>   (§2.2).
> - `verified_by` split: **modality floor in the law, site in the register** (§2.4).
> - New: the **law/conformance split** this RFC enables (§2.7), the
>   **semantic-foundations** vocabulary proposal (§2.8), and the
>   **democratic-ratification stress test** (§4).
> - All revision-1 open questions resolved (§7).

---

## 0. The goal that sets the bar

The constitution is not documentation of this codebase. It is meant to be
**the specification from which a compatible gateway could be rebuilt** — in
another language, another harness — such that two implementations can verify
they operate under the same law (`P-10.9`'s digest handshake) and federate.

That bar is much higher than "accurate prose". A re-implementer must be able to
read, mechanically:

1. **Who must comply** with each clause.
2. **Who may invoke it** — who has standing to complain when it is broken.
3. **How compliance is verified** — because a clause with no verification is a
   wish (§Preamble: *"A rule without a test is a wish. A right without a test is
   a lie."*).

Today none of the three is reliably recoverable. This RFC argues the cause is
that bind-direction is a **prefix convention asserted as data**, and proposes
making it actual data.

## 1. Four defects, with evidence

### 1.1 `binds()` derives the bound party from the ID prefix — and lies

`autonoetic-gateway/src/enforcement_register.rs`:

```rust
pub fn binds(clause_id: &str) -> Option<Binds> {
    if principle(clause_id).is_some() { Some(Binds::Agent) }
    else if right(clause_id).is_some() { Some(Binds::Gateway) }
    else if obligation(clause_id).is_some() { Some(Binds::Decider) }
    else { None }
}
```

The bound party is *inferred from which table the ID is in*. So:

| Call | Returns | Reality |
|---|---|---|
| `binds("P-8.1")` | `Agent` | The **gateway** keeps the causal chain append-only and hash-chained |
| `binds("P-3.1")` | `Agent` | The **gateway** passes `--unshare-all` when constructing a sandbox |
| `binds("P-15.1")` | `Agent` | The **gateway** withholds labelled envelopes. `I-14` says *"no agent may set, strip, or read"* a label — an agent **cannot** comply even in principle |
| `binds("P-6.23")` | `Agent` | The clause's own text says *"the **gateway** injects a signed machine-readable state block"* |

And a test pins the inference:

```rust
fn principles_bind_agent_rights_bind_gateway() {
    for p in principles() { assert_eq!(binds(p.id), Some(Binds::Agent), …); }
```

So the falsehood is not drift — it is asserted and guarded. A re-implementation
consuming the register would be told the agent is responsible for chain
integrity and egress confinement.

### 1.2 `owed_to` does not exist — and `Binds` was *almost* not the problem

**Corrected twice.** An earlier revision of this RFC claimed
`Binds { Agent, Gateway, Decider }` "cannot express the served party" and
proposed adding values for `ServedParty` and `Operator`. That was wrong on both
counts, and the code says why.

`autonoetic-types/src/session_timeline.rs` already separates the two axes, in a
doc comment that settles the question:

```rust
/// The seat a participant occupies — occupant-agnostic (a human or an AI may
/// hold `Operator`). Distinct from [`crate::principal::PrincipalKind`] (who).
pub enum SessionRole {
    /// The deciding seat (gates, redirects). Symmetric obligations attach here.
    Operator,
    Planner, Specialist { kind: String }, Sentinel, Curator, Auditor,
    Tool { surface: String }, ExternalSurface { surface: String },
    /// The executor's own voice (lifecycle, mechanical rulings).
    Runtime,
}
```

Two consequences:

- **`Operator` is a seat, not a human.** "The operator is a human" describes the
  current occupant, not the thing. And there is **no separate `Decider` role** —
  `Operator` *is* the deciding seat. §O's "decider" is the function; `Operator`
  is the seat. Listing both in a party set double-counts.
- **The served party needs no `Binds` value** because the served party never
  complies with anything; they have no seat and never participate in a session.
  They can only be *owed*.

**Revision 2 correction:** the above settled the *arity* of `Binds` (three
values — the three powers) but missed that its *names* still mix three
ontological levels: `Agent` is a principal kind (`PrincipalKind::AutonoeticAgent`),
`Gateway` is an implementation artifact (the runtime, `SessionRole::Runtime`),
`Decider` is a seat function. That is the conflation
`docs/reference/principal-seat-capability.md` exists to prevent, re-entering as
declared data — and its symptoms were already visible: `agent` appeared in both
fields ("legitimately", revision 1 waved), and the question "does `binds: agent`
need splitting for P-2.20?" only arises because `agent` sat in the wrong domain.
The values are renamed to the powers they always meant: `Reasoner` / `Enforcer`
/ `Decider` (§2.1).

So the missing thing was never a `Binds` value. It is that **`owed_to` does not
exist at all**, which is why §12's clauses have nowhere to record that the
gateway owes them to the served party — and why they resorted to inventing a
`community` aggregate that breaks §0 (defect 1.3).

### 1.3 §0 and §12 contradict each other

> **§0:** "Every clause in this constitution binds **exactly one party**."
>
> **§12:** "Every clause here (`U-*`) binds the **community** — the gateway and
> the agents acting within it, **collectively**."

`community` is an *aggregate of parties*, so §12 breaks §0's rule. This is not
pedantry: a re-implementer asked to enforce `U-1` cannot tell which component
must implement the refusal.

### 1.4 `P-*` means three different things, and `I-*` conflates two axes

`P-*` currently covers:

1. **Agent constraints** — `P-1.1` (tool calls match a declared capability),
   `P-3.8` (destructive commands blocked). Genuinely reasoner-bound.
2. **The mechanism of an existing right** — `P-6.23` is the enforcement cited by
   `Ri-0.1`. Six such pairs exist (`P-2.20`, `P-2.21`, `P-5.14`, `P-6.21`,
   `P-6.23`, `P-7.18`). Not misfiled rights — *the same obligation written
   twice*, once as an entitlement and once as a mechanism.
3. **Gateway duties owed to no party** — `P-3.1`, `P-4.14`, `P-4.15`, `P-8.1`,
   `P-8.17`. ~15 clear cases. These have no home in the current scheme, which is
   why they wear a prefix that means "binds the agent".

Separately, `I-*` groups clauses by **modality** (universal-over-paths) rather
than by bound party: `I-8` binds the enforcer, `I-12` binds a future decision
mechanism, `I-3` binds the enforcer. Two independent axes share one prefix, so
neither is recoverable.

*Honesty note on scale:* an earlier draft of this analysis claimed "most of
§1–§11 binds the gateway", from six examples chosen **because** they looked
gateway-acting. Quantifying does not support that: of 182 `P-*`, a keyword
classifier finds 44 clearly agent-subject, 15 clearly gateway-subject, and 117
ambiguous from statement text alone. The defect is real and the *count is
unknown*; classification needs a clause-by-clause read, which this RFC proposes
as the migration work rather than pretending it is done.

## 2. The model

**Two fields over two different domains, plus one orthogonal verification field.**

This section has been through three wrong versions, and each error is worth
recording because each was a category mistake of a different kind.

- **First attempt:** `binds` and `owed_to` drawn from two hand-written lists that
  differed arbitrarily — `community` in one, `system integrity` in the other.
  Challenged on the asymmetry. Both values were bogus: `community` is an
  aggregate that breaks §0 (defect 1.3), and `system integrity` is not a party
  at all (it is `none`, §2.3).
- **Second attempt:** one flat party set for both fields — `agent`, `gateway`,
  `decider`, `operator`, `served_party`. Also wrong, and more subtly: that list
  mixes a principal kind, a runtime, a function, a *seat name for that same
  function*, and another principal kind. `operator` and `decider` are the same
  thing counted twice (defect 1.2).
- **Revision 1:** arity right, names wrong — `{Agent, Gateway, Decider}` still
  blended a principal kind, an artifact, and a seat (§1.2). Review caught it
  because `agent` appeared in both fields and P-2.20 produced an unanswerable
  question.

The resolution is that the two fields genuinely range over different domains,
for a structural reason — and the code already models both.

**Mnemonic: obligations attach to seats; standing attaches to principals.**

### 2.1 The two domains

**`binds` ranges over the three powers** — the closed, constitutional set from
`docs/concepts/separation-of-powers.md`. A power is a *function*, so only a
power's occupant can be obliged to act:

| `binds` | The power | Occupied today by | `SessionRole` |
|---|---|---|---|
| `reasoner` | proposes and acts, subject to gating | agents, script-mode agents, federated foreign agents | `Planner`, `Specialist`, `Sentinel`, `Curator`, `Auditor` |
| `enforcer` | mechanical enforcement — the Lawful Executor | the gateway runtime | `Runtime` |
| `decider` | resolving gates | the human operator; `GateDecider`-holding agents | `Operator` |

Three properties make this set stable in a way revision 1's names were not:

- **Closed and constitutional.** The powers are three by the constitution's own
  separation; the mapping from `SessionRole` variants to powers is runtime data
  that may grow (e.g. a served-party surface for U-1, §2.2 below) without the
  law changing.
- **Implementation-neutral.** `enforcer` names the *function any implementation
  must provide*; "the gateway" is merely this constitution's name for whatever
  implements it. A re-implementer reads "the enforcer owes X" as a
  specification of what to build, not a description of our Rust. This is §0's
  bar applied to the value names themselves.
- **Occupant-agnostic.** The revision-1 open question — does `binds: agent`
  need splitting because P-2.20 lets an agent occupy the decider seat? —
  dissolves: clauses bind seats, never occupants. `O-*` binds `decider` whoever
  holds it, human or agent.

**`owed_to` ranges over principals — with one discovered case of seat-standing —
or none:**

| `owed_to` | Is | Example |
|---|---|---|
| a `PrincipalKind` tag (`autonoetic_agent`, `served_user`, `human`, `script`, `foreign_agent`) | standing by identity | `Ri-0.2` → `autonoetic_agent`; `U-1` → `served_user` |
| a **power** (seat-standing) | standing by seat occupancy, kind-agnostic | `Ri-0.15` → `decider`: `DecisionContext` is owed to *whoever decides the gate*, human or agent |
| `none` | an integrity property — no invocable beneficiary | `P-3.1` (§2.3) |

The domain is `PrincipalKind` tags ∪ powers ∪ {`none`}, not a bespoke enum,
because the principal census evolves (federation will plausibly add duties owed
to foreign peers) and the relational schema must not need amending when it
does. Single-valued, mirroring §0's "exactly one party": two standings means
two clauses, which keeps the non-duplication test (§6) well-formed.

The `Ri-0.15` case surfaced during review-driven classification and is recorded
deliberately: it is evidence that the 117-clause classification (§3) will
surface structure the prefix scheme never said aloud.

**Why the served party appears only in `owed_to` — and why that is contingent.**
A review question ("can a served party have a seat?") exposed that an earlier
phrasing here — *"has no seat at all; never participates in a session"* —
conflated three claims:

1. *Can the human who is the served party hold a seat?* **Yes, and today they
   usually do.** §12: "Today the operator and the served user are usually the
   same person." That human holds `Operator`.
2. *Is there a seat for the served party qua served party?* **No.** `SessionRole`
   has nine variants and none is theirs.
3. *Is the served party ever bound?* **No.** All three `U-*` clauses are "may" —
   pure grants. No clause in the constitution obliges them.

The field placement rests on (3), and (2) **explains** it: the served party is
never obliged *because there is no seat through which to oblige them*. Absence
of a seat causes absence of obligation, rather than merely coinciding with it.

So `owed_to`-only is a **contingent fact about the current constitution, not a
structural axiom** — and it yields a prediction worth recording: **implementing
`U-1` requires giving the served party a seat.** Refusing a delivered result is
an *act*; acting needs a surface; a surface is a seat. Under the powers model
that act is a *decision*, so `U-1` = `binds: enforcer, owed_to: served_user`,
implemented by a `ServedUser` principal holding the deciding seat for that act —
the same shape as the P-2.20 agent-decider pattern, not a new architectural
concept. At that point the served party becomes bindable and could, in
principle, carry obligations of its own. See #1274.

This is why the model should *record* bind-direction rather than derive it: a
scheme that hard-codes "the served party is never bound" into which prefix it
gets would have to be amended the moment `U-1` ships. A declared field just
changes value.

Conversely the gateway holds a seat (`Runtime`) but has no human/agent principal
identity, so it appears only in `binds`. Neither domain collapses into the
other, but the membership is data, not doctrine.

`community` is **not** in either. It is "gateway + agents", and clauses that
appear to bind it bind the *enforcer*, because the enforcer is what implements
the mechanism. That is the same conclusion `docs/concepts/philosophy.md` §3.3
already reaches for data locality: *"the mechanism landed on the other side of
the bind-direction… an entitlement in §12 would be a claim, whereas an invariant
on the enforcer is a guarantee."*

`operator` is **not** in either, because it is the occupant name for the
`decider` seat (§1.2), not a separate party.

### 2.2 The three fields

| Field | Domain | Meaning |
|---|---|---|
| `binds` | exactly one **power** (`reasoner` / `enforcer` / `decider`) | who must comply. Non-compliance is *their* violation |
| `owed_to` | one `PrincipalKind`, one power (seat-standing), **or `none`** | who has standing to invoke it |
| `verified_by` | a modality floor + a register site (§2.4) | how compliance is established |

### 2.3 `owed_to: none` is what an integrity property *is*

This is the part that earns the model its keep.

- A **duty** is owed to someone who can invoke it. `Ri-0.2` (read your own chain)
  is owed to the agent; the agent can demand it.
- A **property** is owed to no one. `P-3.1` (sandboxes default to
  `--unshare-all`) benefits the operator, but nobody can *claim* it — an agent
  cannot demand its own confinement, and would prefer not to have it.

So the "gateway duties owed to nobody" of defect 1.4(3) are not a missing family.
They are `owed_to: none`, and saying so is more honest than inventing a
pseudo-party to fill the slot — the same error as a fabricated `U-4`.

### 2.4 `verified_by`: floor in the law, site in the register

`owed_to` does **not** subsume the rule/invariant distinction, and it is worth
being explicit that these are orthogonal:

- `I-8` is universal-over-paths **and** `owed_to: autonoetic_agent` (it is "the
  mechanical form of Ri-0.13(a)").
- `P-1.1` is a specific chokepoint **and** `binds: reasoner`.

So modality is about *how you establish compliance*, not about parties — which is
exactly why `I-*` as a family confuses. The conversions actually in use (see
PR #1281's `RATIFY.md`):

| `verified_by` | Means | Example |
|---|---|---|
| `construction` | the bad state is unrepresentable (type, signature, closed enum) | `I-8`, `I-9` |
| `chokepoint` | N paths reduced to 1, plus a bypass guard | `I-1` |
| `registry` | "every X has Y" as a set comparison | `I-11` |
| `sampling` | property-based over generated inputs | `I-10` |
| `test` | an ordinary example-based test at a named site | most `P-*` |
| `detection` | recorded and counted in production, not proven absent | `I-6`, `I-4` |

Revision 1 asked whether `verified_by` is law or commentary. Review's answer:
**split it — modality floor is law, site is evidence.**

- **Floor in the signed text.** Even modality is partially implementation-shaped:
  Rust achieves `construction` for `I-9` via a closed enum; a Python
  re-implementation *cannot* and would reach for `registry` + `test`. Pinning an
  exact modality would silently presume the reference implementation's type
  system. So each clause declares a **minimum** ("at least `detection`"), which
  implementations may exceed — `construction` remains strictly strongest because
  it covers call sites that do not exist yet. The floor is per clause, not a
  total order imposed across modalities.
- **Site in the register.** `policy.rs:679` is per-implementation conformance
  data; it lives in the enforcement register, never in the signed text.

### 2.5 Rights and obligations are duals, not duplicates

Under this model a **right is a view, not a family**: an obligation with
`binds: enforcer, owed_to: autonoetic_agent` *is* an agent right. So the six
right/mechanism pairs collapse — `Ri-0.1` and `P-6.23` are one obligation
described twice, and the duplication exists only because the prefix had to carry
the relationship.

This also settles the question that prompted the RFC ("if rights are enforcer
obligations, shouldn't enforcer rules be rights?"): **only those owed to the
reasoner.** `P-3.1` is `binds: enforcer, owed_to: none` and is therefore not a
right, which is the distinction the current scheme cannot draw.

This is also the correct legal semantics, and worth stating: real bills of
rights bind the *state*, not citizens (vertical application). `Ri-*` binding the
enforcer while `P-*` bind reasoners is exactly that structure — the vocabulary
was right; the data model now says so.

### 2.6 Why not just reclassify into the existing families?

Because it would corrupt a deliberate honesty metric. §0 tracks:

> The **rights/obligations ratio** (18 rights against 182 rules) is itself a
> design signal: a constitution heavy on rules and light on rights is one to
> watch.

Moving ~15 enforcer-integrity duties into §0 takes the ratio from 18:182 to
33:167 **without changing one guarantee owed to any agent** — gaming the metric
that exists to catch exactly that. Under the proposed model the ratio becomes
derivable and meaningful: count clauses with `owed_to: autonoetic_agent`.

### 2.7 The split this enables: law vs conformance

For §0's bar, the signed text currently mixes two things:

- **Law** — ID, statement, rationale, and (under this RFC) the relational
  fields. Portable; what the digest pins and `P-10.9` compares.
- **Conformance claims of this one implementation** — enforcement *site*
  (`runtime/tools/….rs`) and *status* (`ENFORCED` / `PARTIAL` / …). Not
  portable: a second gateway's status for `Ri-0.2` starts at `MISSING`
  regardless of what ours says.

Once the relational content is data, the split becomes possible:

1. **Signed constitution** = ID + statement + `binds` + `owed_to` + verification
   floor + entrenchment. Language-neutral.
2. **Per-implementation conformance register** = clause → mechanism site →
   status → evidence. `enforcement-register.md` is already this in spirit; the
   split makes it official that it is *this* gateway's claim, and a second
   implementation ships its own.

This RFC does not perform the split — it names it as the next step, and as the
reason the fields must be implementation-neutral (why `enforcer`, not
`gateway`; why floors, not sites).

### 2.8 Semantic foundations: pinning the vocabulary

Review audited the document's vocabulary against constitutional and
administrative law. The mapping is sound enough to pin — and pinning it is what
lets a re-implementer inherit the *semantics*, not just the IDs:

| Real-life concept | Here | Verdict |
|---|---|---|
| Bill of Rights binds the *state*, not citizens (vertical application) | `Ri-*` binds the enforcer, not the reasoner | correct — §2.5 |
| Statutes bind citizens | `P-*` binds reasoners | correct direction; they live *inside* the constitution as a young-system choice, not a semantic claim |
| Duty to give reasons (administrative law) | `O-1` | textbook; §O is administrative law for the deciding seat |
| Non-justiciable directive principles (Irish/Indian constitutions) | `U-*` declared `MISSING` | declared before enforcement exists, to shape sequencing — ours is more honest (it says `MISSING`) |
| Structural principles (separation of powers, rule of law) | `I-*` | the legal analog of "invariants"; the `I-` family dissolves under this RFC because modality ≠ party |
| Eternity clauses (e.g. GG Art. 79(3)) | the correction core | correctly *procedural* rather than absolute — and the text says so |
| Amendment = proposal + ratification + promulgation | Ri-0.8 / PR → sign-off → signing ceremony → lock digest | correct; the digest is the identity of the law |

The most radical semantic commitment is what the document *lacks*: **there is
no judiciary, by design.** Interpretation is abolished in favor of determinism
(`I-10`): the enforcer never construes; where the text is insufficient the
remedies are rejection and amendment, and improvisation is recorded as
DISCRETION LEAK debt (§14). That is precisely what makes the document portable —
jurisprudence does not transfer between implementations; decidable rules do.

Proposal: a short section in the signed text, near the Preamble, stating this
mapping — rights bind the enforcer (verticality), rules bind reasoners
(statutes), §O is administrative law for the decider seat, `U-*` are directive
principles owed to a party outside the polity, `I-*` are structural principles
distinguished by modality, interpretation is abolished in favor of determinism
plus amendment, and the digest is the identity of the law. It belongs to the
amendment that adopts this RFC's fields (sequenced after #1281's ceremony).

**Naming stance:** every clause ID stays exactly as-is (names, not claims — §3;
the `R+` wreckage is the counterexample). "Constitution" stays: the document
constitutes the polity, entrenches its correction machinery, is supreme (`I-7`),
and is self-referential in amendment. What this section pins is the *mapping*,
so the words stop being conventions and become declared law about the law.

## 3. Migration

| Today | Becomes |
|---|---|
| `Ri-*` (18) | `binds: enforcer, owed_to: autonoetic_agent` — **except `Ri-0.15`**: `owed_to: decider` (seat-standing, §2.1) |
| `O-*` (4) | `binds: decider, owed_to: autonoetic_agent` |
| `U-*` (3) | `binds: enforcer, owed_to: served_user` — resolves defect 1.3 |
| `P-*` reasoner constraints | `binds: reasoner, owed_to: none` |
| `P-*` right-mechanisms (6) | merged into the right they serve (§2.5) |
| `P-*` enforcer duties (~15) | `binds: enforcer, owed_to: none` |
| `I-*` (14) | `binds` per clause; `verified_by` floor set; the `I-` prefix retires as a *family* |
| `P-15.*` | `binds: enforcer, owed_to: served_user` |

**Prefixes become stable identifiers with no semantics.** `P-8.1` stays `P-8.1`;
its meaning moves into the fields. That is deliberate: renaming clause IDs is
what produced the `R+`/`R++`/`R+++` wreckage (#1277), where 29 IDs had to be
recovered from breadcrumbs months later. An ID should be a name, not a claim.

The 117 ambiguous `P-*` need a clause-by-clause read. That is the bulk of the
work and it cannot be automated: "who must comply" is a semantic judgement.
`Ri-0.15` (§2.1) is the down payment on what that read will find.

## 4. Democratic ratification: the model's stress test

Review's question: the horizon (`principal-model-and-symmetric-obligations.md`
Parts C–E) is a democratic community of humans and agents where law is ratified
by vote. Does this model express that without growing a `community` value — and
does the current constitution allow it?

**The model survives intact:**

- **The electorate is not a new party.** It is the *decider seat at composed
  cardinality* — a panel — exactly as §12 already anticipates for gates. Tally
  duties (quorum, window, recorded outcome) are `binds: decider`; the duty to
  decide a proposal within a window (the `O-6` shape) is `owed_to` the proposer.
  No aggregate leaks back in; §0's "exactly one party" survives democracy.
- **Franchise is a capability, not a seat.** Something like
  `ConstituentVote { classes: [statute, amendment, …] }` — the same shape as
  `GateDecider { kinds }` (P-2.20). A `SessionRole::Voter` variant would repeat
  the "GateDecider is a seat" category error that
  `principal-seat-capability.md` exists to correct: capabilities compose, seats
  do not; a principal is a planner *and* enfranchised *and* a gate-decider
  without switching identity.
- **Citizenship and weight are principal-axis data**: the electorate roll,
  standing derived from the causal chain (Part E.1), weight computed at tally
  under `I-12`'s spawn-tree collapse — the load-bearing clause, declared before
  any collective mechanism exists.
- **The ballot is a causal event** — attributed (`O-2`), non-repudiable
  (`Ri-0.11`): parliamentary *roll-call*, not secret ballot. The coercion model
  for agents differs from humans (the spawner vector is already cut by
  `P-10.7` / `I-12`), and auditability is the chain's entire point.
- The one modeling change the horizon implies: the decider seat gains a
  **domain** tag (`operational` / `constituent` / `judicial`, per Part D.2) — a
  property of the *matter under decision*, not a new office.

**What the constitution already provides:** the initiative channel (`Ri-0.8`),
the duty to adjudicate petitions within a window (`O-6`), agents already holding
operational decision power (`P-2.20`), decision context owed to every decider
regardless of kind (`Ri-0.15` — written verbatim for "the future multi-decider
(voting) model"), the Sybil guard (`I-12`), and lawful self-amendment — the
constitution *can* democratize itself; a system that couldn't would need a
revolution.

**Two prerequisites this vantage makes visible** (Part E's sequencing, not this
RFC's work):

1. **The two-tier norm system.** A democracy cannot deliberate under
   constitutional ceremony per operational rule — the amendment cadence (~24
   versions in 4 months) is already statute-frequency. Statutes (light process,
   simple tally) must split from the constitution (heavy ceremony,
   supermajority) before votes are worth operating. §2.8's framing of `P-*` as
   statutes-in-residence sets this up.
2. **Entrenchment hardens *before* binding franchise, not after.** Today's
   backstop for the correction core is the operator's signing key (Part D:
   `{constituent, k=1}`). The day votes bind, that backstop dissolves — and a
   majority can amend the friction away. Supermajority thresholds or genuinely
   eternal clauses for the correction core must be enacted while the current
   authority can still do it unilaterally. Ordering constraint:
   **entrenchment redesign precedes binding franchise, or it never happens.**

## 5. Code changes

- **`Binds` keeps its arity, changes its names** — `Reasoner | Enforcer |
  Decider` (§2.1; the names were the bug, not the count). Add
  `OwedTo` (`PrincipalKind` tag | power | `None`) and a `VerifiedBy` modality
  enum per §2.4.
- **`binds()` stops deriving from the prefix** and reads the clause's declared
  field. The test `principles_bind_agent_rights_bind_gateway` **inverts**: it
  currently asserts the falsehood, and should assert that every clause declares
  its own `binds` rather than inheriting one.
- The constitution tables gain `Binds` / `Owed to` / `Verified by (floor)`
  columns, so the document and the register are generated from one source
  rather than asserted in two places that can disagree.
- **Sequencing:** register code lands first (the §6 tests give the
  fails-before/passes-after the amendment process requires); the constitutional
  amendment then adopts the columns and adds the semantic-foundations section
  (§2.8) — after #1281's signing ceremony, since amendments are sequential.

## 6. Tests that keep it honest

1. **Completeness** — every clause declares `binds`, `owed_to`, `verified_by`.
   Fails today for all of them; would have caught §15 when it was appended.
2. **One power per clause** — `binds` is a single value, enforcing §0's own rule
   and making `community` unrepresentable.
3. **No prefix inference** — assert `binds()` reads the field, e.g. by checking
   that at least one `P-*` is `Binds::Enforcer` (impossible under today's
   derivation, so it fails before and passes after).
4. **Register/document agreement** — the same clause cannot declare different
   fields in the two places.
5. **Non-duplication** — no two clauses share `(binds, owed_to, statement)`;
   this is the check that would have caught `R+9` duplicating `R-4.14` on the day
   it was written (#1277).

## 7. Open questions — all resolved

- ~~**`operator` vs `decider`**~~ — **resolved (rev 1).** `SessionRole::Operator`
  *is* the deciding seat, occupant-agnostic. `operator` is not a party; removed
  from the model.
- ~~**Does `agent` need splitting** by role (reasoner / decider / auditor)?~~ —
  **resolved (rev 2).** The question was an artifact of `agent` sitting in the
  `binds` domain. Clauses bind powers, not occupants; P-2.20 needs no special
  case.
- ~~**Entrenchment: field or list?**~~ — **resolved (rev 2): the list.**
  Entrenchment is meta-law about amendment friction, not relational content;
  a fourth field would pretend it is the same kind of thing as `binds` /
  `owed_to`, and a named correction-core list with a dedicated structural test
  is harder to erode by accident than a per-clause flag.
- ~~**Is `verified_by` law or commentary?**~~ — **resolved (rev 2): both, split.**
  Modality floor in the signed text, site in the register (§2.4).

## 8. What this RFC does not propose

- No clause statement changes. Only the relationships become explicit.
- No renaming of clause IDs (§3).
- No behavioural change; nothing in the runtime is affected.
- Not the law/conformance split itself (§2.7 — named as the next step), not the
  two-tier norm system, not franchise mechanics (§4 — Part E's horizon).

It is a **prerequisite** for the §12/§15 questions rather than a competitor:
PR #1281 (invariant enforcement citations) is orthogonal and correct under either
scheme, and #1277's legacy-ID cleanup is unaffected.
