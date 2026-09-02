# RFC: Bind-direction as data — making the constitution re-implementable

Status: **Open** (proposal — no code, no amendment)
Date: 2026-09-02
Scope: the family structure of constitutional clause IDs, and the `binds` model in
`enforcement_register.rs`. Supersedes nothing; blocks nothing.

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

### 1.2 `owed_to` does not exist — and `Binds` was *not* the problem

**Corrected after review.** An earlier revision of this RFC claimed
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
- **`Binds` is already correct.** Its three values map exactly onto the three
  powers of §2 — *"Reasoner, enforcer, and decider are seats, not identities."*
  The served party needs no `Binds` value because **the served party never
  complies with anything**; they have no seat and never participate in a
  session. They can only be *owed*.

So the missing thing is not a `Binds` value. It is that **`owed_to` does not
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
   `P-3.8` (destructive commands blocked). Genuinely agent-bound.
2. **The mechanism of an existing right** — `P-6.23` is the enforcement cited by
   `Ri-0.1`. Six such pairs exist (`P-2.20`, `P-2.21`, `P-5.14`, `P-6.21`,
   `P-6.23`, `P-7.18`). Not misfiled rights — *the same obligation written
   twice*, once as an entitlement and once as a mechanism.
3. **Gateway duties owed to no party** — `P-3.1`, `P-4.14`, `P-4.15`, `P-8.1`,
   `P-8.17`. ~15 clear cases. These have no home in the current scheme, which is
   why they wear a prefix that means "binds the agent".

Separately, `I-*` groups clauses by **modality** (universal-over-paths) rather
than by bound party: `I-8` binds the gateway, `I-12` binds a future decision
mechanism, `I-3` binds the gateway. Two independent axes share one prefix, so
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

This section has been through two wrong versions, and both errors are worth
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

The resolution is that the two fields genuinely range over different domains,
for a structural reason — and the code already models both.

### 2.1 The two domains

**`binds` ranges over seats** — the three powers of §2. A seat is a *function
inside a session*, so only a seat can be obliged to act:

| `binds` | Seat | `SessionRole` |
|---|---|---|
| `agent` | reasoner | `Planner`, `Specialist`, `Sentinel`, `Curator`, `Auditor` |
| `gateway` | enforcer | `Runtime` |
| `decider` | deciding seat | `Operator` |

**`owed_to` ranges over principals** — *identities*, some of which are outside
the session entirely:

| `owed_to` | Is | Why not a seat |
|---|---|---|
| `agent` | `PrincipalKind::AutonoeticAgent` | also holds seats, so appears in both — legitimately |
| `served_party` | `PrincipalKind::ServedUser` | **has no seat at all**; never participates in a session |
| `none` | no invocable beneficiary | see §2.3 |

That asymmetry is the point: the served party can be *owed* but never *bound*,
because being bound requires occupying a seat and they never do. Conversely the
gateway holds a seat (`Runtime`) but has no human/agent principal identity.
Neither domain collapses into the other.

`community` is **not** in either. It is "gateway + agents", and clauses that
appear to bind it bind the *gateway*, because the gateway is what implements the
mechanism. That is the same conclusion `docs/concepts/philosophy.md` §3.3 already
reaches for data locality: *"the mechanism landed on the other side of the
bind-direction… an entitlement in §12 would be a claim, whereas an invariant on
the enforcer is a guarantee."*

`operator` is **not** in either, because it is the occupant name for the
`decider` seat (§1.2), not a separate party.

### 2.2 The three fields

| Field | Domain | Meaning |
|---|---|---|
| `binds` | exactly one party | who must comply. Non-compliance is *their* violation |
| `owed_to` | one party, **or `none`** | who has standing to invoke it |
| `verified_by` | see §2.4 | how compliance is established |

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

### 2.4 Verification modality is a third, independent axis

`owed_to` does **not** subsume the rule/invariant distinction, and it is worth
being explicit that these are orthogonal:

- `I-8` is universal-over-paths **and** `owed_to: agent` (it is "the mechanical
  form of Ri-0.13(a)").
- `P-1.1` is a specific chokepoint **and** `binds: agent`.

So modality is about *how you establish compliance*, not about parties — which is
exactly why `I-*` as a family confuses. The conversions actually in use (see
PR #1281's `RATIFY.md`):

| `verified_by` | Means | Example |
|---|---|---|
| `construction` | the bad state is unrepresentable (type, signature, closed enum) | `I-8`, `I-9` |
| `chokepoint` | N paths reduced to 1, plus a bypass guard | `I-1` |
| `registry` | "every X has Y" as a set comparison | `I-11` |
| `sampling` | property-based over generated inputs | `I-10` |
| `detection` | recorded and counted in production, not proven absent | `I-6`, `I-4` |
| `test` | an ordinary example-based test at a named site | most `P-*` |

`construction` is strictly strongest: it covers call sites that do not exist yet.

### 2.5 Rights and obligations are duals, not duplicates

Under this model a **right is a view, not a family**: an obligation with
`binds: gateway, owed_to: agent` *is* an agent right. So the six right/mechanism
pairs collapse — `Ri-0.1` and `P-6.23` are one obligation described twice, and
the duplication exists only because the prefix had to carry the relationship.

This also settles the question that prompted the RFC ("if rights are gateway
obligations, shouldn't gateway rules be rights?"): **only those owed to the
agent.** `P-3.1` is `binds: gateway, owed_to: none` and is therefore not a right,
which is the distinction the current scheme cannot draw.

### 2.6 Why not just reclassify into the existing families?

Because it would corrupt a deliberate honesty metric. §0 tracks:

> The **rights/obligations ratio** (18 rights against 182 rules) is itself a
> design signal: a constitution heavy on rules and light on rights is one to
> watch.

Moving ~15 gateway-integrity duties into §0 takes the ratio from 18:182 to
33:167 **without changing one guarantee owed to any agent** — gaming the metric
that exists to catch exactly that. Under the proposed model the ratio becomes
derivable and meaningful: count clauses with `owed_to: agent`.

## 3. Migration

| Today | Becomes |
|---|---|
| `Ri-*` (18) | `binds: gateway, owed_to: agent` |
| `O-*` (4) | `binds: decider, owed_to: agent` |
| `U-*` (3) | `binds: gateway, owed_to: served_party` — resolves defect 1.3 |
| `P-*` agent constraints | `binds: agent, owed_to: none` |
| `P-*` right-mechanisms (6) | merged into the right they serve (§2.5) |
| `P-*` gateway duties (~15) | `binds: gateway, owed_to: none` |
| `I-*` (14) | `binds` per clause; `verified_by` set; the `I-` prefix retires as a *family* |
| `P-15.*` | `binds: gateway, owed_to: served_party` |

**Prefixes become stable identifiers with no semantics.** `P-8.1` stays `P-8.1`;
its meaning moves into the fields. That is deliberate: renaming clause IDs is
what produced the `R+`/`R++`/`R+++` wreckage (#1277), where 29 IDs had to be
recovered from breadcrumbs months later. An ID should be a name, not a claim.

The 117 ambiguous `P-*` need a clause-by-clause read. That is the bulk of the
work and it cannot be automated: "who must comply" is a semantic judgement.

## 4. Code changes

- **`Binds` is unchanged** — its three values are already the three powers
  (§1.2). Add a new `OwedTo { Agent, ServedParty, None }` and a `VerifiedBy`
  enum per §2.4. The earlier revision of this RFC proposed extending `Binds`
  with `Operator`/`ServedParty`; that was a category error.
- **`binds()` stops deriving from the prefix** and reads the clause's declared
  field. The test `principles_bind_agent_rights_bind_gateway` **inverts**: it
  currently asserts the falsehood, and should assert that every clause declares
  its own `binds` rather than inheriting one.
- The constitution tables gain `Binds` / `Owed to` / `Verified by` columns, so
  the document and the register are generated from one source rather than
  asserted in two places that can disagree.

## 5. Tests that keep it honest

1. **Completeness** — every clause declares `binds`, `owed_to`, `verified_by`.
   Fails today for all of them; would have caught §15 when it was appended.
2. **One party per clause** — `binds` is a single party, enforcing §0's own rule
   and making `community` unrepresentable.
3. **No prefix inference** — assert `binds()` reads the field, e.g. by checking
   that at least one `P-*` is `Binds::Gateway` (impossible under today's
   derivation, so it fails before and passes after).
4. **Register/document agreement** — the same clause cannot declare different
   fields in the two places.
5. **Non-duplication** — no two clauses share `(binds, owed_to, statement)`;
   this is the check that would have caught `R+9` duplicating `R-4.14` on the day
   it was written (#1277).

## 6. Open questions

- ~~**`operator` vs `decider`**~~ — **resolved.** `SessionRole::Operator` *is*
  the deciding seat (its doc: "The deciding seat (gates, redirects). Symmetric
  obligations attach here"), and it is occupant-agnostic. `operator` is
  therefore not a separate party; it is the seat `decider` names. Removed from
  the model.
- **Does `agent` need splitting** by role (reasoner / decider / auditor)? `P-2.20`
  lets an agent occupy the decider seat, which the model handles via `decider`
  being a seat — but `binds: agent` then means "any agent", including one wearing
  the decider hat.
- **Entrenchment** is currently a separate list (`entrenched_clauses()`). Should
  it be a fourth field, or stay a list? A field is more uniform; a list is harder
  to weaken by accident.
- **Is `verified_by` law or commentary?** It describes implementation strategy,
  which arguably belongs in the register rather than the signed text. Counter: a
  re-implementer needs it, and §Preamble already makes testability
  constitutional.

## 7. What this RFC does not propose

- No clause statement changes. Only the relationships become explicit.
- No renaming of clause IDs (§3).
- No behavioural change; nothing in the runtime is affected.

It is a **prerequisite** for the §12/§15 questions rather than a competitor:
PR #1281 (invariant enforcement citations) is orthogonal and correct under either
scheme, and #1277's legacy-ID cleanup is unaffected.
