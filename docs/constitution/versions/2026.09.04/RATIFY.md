# RATIFY.md — Constitution Version 2026.09.04

## Summary

**The relational amendment.** Every clause gains a `Relation` — which power it
binds, who may invoke it, what it requires of any implementation — and two
statements the text made about bind-direction are corrected because
implementing the model proved them false. Baseline: **2026.09.02**.

No clause is added or removed. No clause *statement* changes. What changes is
that the relationships each clause has always had become declared data rather
than something a reader infers from an ID prefix.

## Why this amendment exists

The prior text said, in §0:

> The bind-direction is uniform by section, so no per-row tag is needed —
> everything under §0 binds the gateway, everything under §1–§11 binds the
> agent, and everything under §O binds the decider.

Classifying all 221 clauses (#1284) found that **181 of the 182 `P-*` bind the
enforcer, not the agent.** Only `P-2.9` binds the reasoner — "they must attach
`execution_trace_id` from a completed run".

That is not drift. The sentence was asserted, and a test pinned it
(`principles_bind_agent_rights_bind_gateway`), so the implementation agreed
with the document while both were wrong. A re-implementation reading it would
have made the agent responsible for causal-chain integrity (`P-8.1`) and for
egress confinement (`P-15.*`) — the latter a duty `I-14` forbids an agent from
discharging at all. §0's bar is that this document be the specification a
compatible gateway is rebuilt from; a false bind-direction defeats that
directly.

The second correction is §12, which said its clauses bind "the community — the
gateway and the agents acting within it, collectively". That contradicted §0's
own rule that a clause binds exactly one party, and left a re-implementer
unable to tell which component must implement a served-party refusal. `U-*`
now binds the **enforcer**, owed to the **served party** — an entitlement
asserted against an aggregate is a claim; an invariant on the enforcer is a
guarantee.

## What the `Relation` column says

`binds · owed to · requires`, on every table row and every §13 bullet.

- **binds** — one of three powers: `reasoner`, `enforcer`, `decider`. Powers
  are functions, so a clause binds a *seat* and never its occupant: an agent
  holding `GateDecider` (`P-2.20`) is bound by an `O-*` duty exactly as a human
  operator is.
- **owed to** — a principal kind, a seat, or `none`. `none` is a positive
  claim: an **integrity property** with no invocable beneficiary. Nobody can
  demand their own sandbox confinement.
- **requires** — `preventive`, `detective`, or both. `detective` is not a
  weaker `preventive`. Where a clause forbids a behaviour no static check can
  exclude (`I-4`), recording each occurrence is the correct requirement, and
  raising it would assert a proof nobody holds.

## What the classification measured

Numbers the document could not previously state about itself:

| | |
|---|---|
| binds `enforcer` | 215 |
| binds `decider` | 5 |
| binds `reasoner` | **1** (`P-2.9`) |
| owed to `none` (integrity properties) | 167 |
| owed to `autonoetic_agent` | 43 |
| **agent rights by relation** | **38**, of which only 17 carry an `Ri-` prefix |
| `requires` | 185 preventive / 15 detective / 21 both |

Two of these change how the document should be read.

**The rights/rules ratio was understating rights by better than a factor of
two.** §0 keeps the ratio deliberately, as an honesty metric — "a constitution
heavy on rules and light on rights is one to watch". Computed from prefixes it
read 18 against 182. Computed from the relation it is 38 agent rights, because
21 enforcer-duties-owed-to-the-agent are filed under rule IDs (`P-1.10`,
`P-2.10`, `P-5.3`, `P-6.23`, `P-7.18`, …). A metric that flatters the rules is
the one kind of dishonesty this section exists to prevent, so the ratio is now
computed from the `Relation` column.

**Integrity properties are the largest category in the document** — 167 of
221. That category had no home in the prefix scheme at all, which is why
gateway duties owed to nobody wore a prefix meaning "binds the agent".

## Also added: Semantic Foundations

A short section after the Preamble pinning the vocabulary against established
concepts — rights bind the state (vertical application), `O-*` is
administrative law for the deciding seat, `U-*` are directive principles
declared before enforcement, `I-*` are structural principles distinguished by
*modality* rather than by party — and stating the document's most radical
commitment plainly: **there is no judiciary, by design.** Interpretation is
abolished in favour of determinism (`I-10`); where the text is insufficient
the remedies are refusal and amendment, and improvisation is recorded as a
DISCRETION LEAK rather than absorbed as precedent.

That absence is what makes the document portable. Jurisprudence does not
transfer between implementations; decidable rules do.

## On the "fails before, passes after" requirement

`constitution/relation_column.rs::the_signed_relation_column_agrees_with_the_register`
reads the `Relation` cell of every clause in the **active** constitution and
requires it to equal what `constitution_relations` declares. Against
`2026.09.02` it fails — there is no column to read. Against `2026.09.04` it
passes for all 221.

That test is the point of the amendment, not a formality. Columns of prose in
a signed document drift from the code that implements them; this one cannot,
because the same test that reads the document reads the register. It is RFC
#1283 §6.4, which could not be written until a document existed to agree with.

## Deliberately unchanged

- **No clause statement is edited.** Only bind-direction *descriptions*, which
  describe the clauses rather than being clauses: §0's, §12's, and the Vision
  bullets. The Vision carried the same false claim in different words — "Rules
  bind the agent. Rights bind the gateway" — and a first pass that corrected
  §0 and §12 left it standing, which is worth recording as the failure mode:
  a claim stated in three places is not corrected by fixing the two you were
  looking at. It also carried a cross-reference to the amendment process
  "after §14", stale since §15 was added; an amendment is the right vehicle
  for fixing a pointer in signed text.

- **`Ri-0.15`'s statement still reads "this binds the gateway to give the
  context", and that is correct, not an oversight.** Under the Semantic
  Foundations section added here, "the gateway" is this constitution's name
  for whatever provides the **enforcer** power. A clause may name the occupant
  where that reads better; what may not be inferred from a prefix is *which
  power* is bound, and `Ri-0.15`'s `Relation` states it. Grepping the document
  for "binds the gateway" will find this line — it is licensed by the
  vocabulary section rather than missed by the sweep.
- **No clause ID changes.** IDs are names, not claims — the `R+`/`R++`/`R+++`
  wreckage is what renaming for meaning produces.
- **`verified_by` does not enter the signed text.** The modality a given
  implementation achieves is conformance data and belongs in that
  implementation's register (RFC §2.7). Signing a modality taxonomy would put
  an amendment ceremony in front of every advance in verification technique.
- **The 21 `Both` clauses are recorded, not split.** A clause requiring both
  prevention and detection usually carries two obligations under one id, and
  splitting one is a statement change needing its own amendment. Marking is
  the honest interim.

## Signing and activation

This directory is **draft**: `constitution.md` + `RATIFY.md`, no
`gateway-constitution.lock.json`. The established three-step ceremony:

1. **Draft** — this PR. No lock, `CURRENT` untouched,
   `ACTIVE_CONSTITUTION_VERSION` still `2026.09.02`.
2. **Sign** — `python3 docs/constitution/recompute_lock.py --version 2026.09.04
   --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"`, then update
   `docs/constitution/CURRENT`. Requires the signing key.
3. **Activate** — flip `ACTIVE_CONSTITUTION_VERSION` in
   `autonoetic-types/src/config.rs` and `docs/reference/config.md`, then:

```bash
cargo nextest run -p autonoetic-gateway --lib -E 'test(constitution_lock_matches_canonical_digest_and_counts)'
cargo nextest run -p autonoetic-gateway --test constitution
BLESS_GLOSSARY=1 cargo nextest run -p autonoetic-gateway --lib -E 'test(bless_constitution_glossary)'
BLESS_REGISTER=1 cargo nextest run -p autonoetic-gateway --lib -E 'test(bless_register_doc)'
BLESS_LAW_TABLE=1 cargo nextest run -p autonoetic-gateway --lib -E 'test(bless_law_table)'
```

The re-blesses matter: an activation invalidates every constitution-derived
artifact at once, and missing the glossary re-bless is what turned `main` red
after the `2026.08.30` activation (#1252). The law table is new to this list —
it renders from the active constitution's clause set, so it moves too.
