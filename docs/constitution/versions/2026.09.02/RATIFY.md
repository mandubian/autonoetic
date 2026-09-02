# RATIFY.md — Constitution Version 2026.09.02

## Summary

**Strengthening only.** Five §13 invariants gain an enforcement citation
naming the mechanism that already holds them. No clause is added, removed or
weakened; no rule ID changes; no status is upgraded beyond recording
enforcement that already exists in code. Baseline: **2026.08.30**.

Before this amendment, **8 of 14 invariants were bare prose** — no code
citation, no status, and no entry in the `fail_mode.rs` table (which contains
zero `I-*` rows). A clause with no mechanism cannot fail, which is how a
redundant clause survived for months without anyone noticing (see the `R+9`
finding in #1277: it duplicated `R-4.14` on the day it was written).

The finding that motivated the amendment is that most of those eight were not
aspirational at all — **four were already enforced by mechanisms nobody had
written down**, in one case by the strongest means available (a function
signature). The document was understating itself.

## Why invariants looked unenforceable

A rule (`P-x.y`) is **existential**: *this chokepoint behaves this way.* You
test it by calling it.

An invariant (`I-x`) is **universal**: *no path anywhere does X.* No number of
examples proves a universal, so enforcement means converting it into something
finite. Five conversions are in use across §13:

| Conversion | Strength | Example |
|---|---|---|
| Make the bad state unrepresentable (types, signatures, closed enums) | Covers paths that do not exist yet | `I-8`, `I-9` |
| Chokepoint + bypass guard | Reduces N paths to 1, then guards the 1 | `I-1` |
| Registry + completeness test | Turns "every X has Y" into a set comparison | `I-11` |
| Property-based sampling | Cannot prove; can sample | `I-10` |
| Detect in production, not prove absent | For behavioural properties no static check reaches | `I-6`, `I-4` |

The eight bare invariants were not eight instances of "untestable". They were
four undocumented mechanisms, two static-analysis jobs not yet built, and two
genuinely hard cases.

## Class 1 — Citations for enforcement that already exists

### I-8 — enforced *by construction*, at the signature

The strongest form present in the document, and previously uncited.

> Gateway policy decisions are functions only of declared actions,
> capabilities, and recorded state. They are **not** functions of agent
> reasoning content.

`policy.rs::can_invoke_tool(&self, tool_name: &str)` and its 22 sibling
decision surfaces **do not take reasoning as a parameter.** The gateway cannot
read minds because the decision functions never receive the mind — and that
holds for call sites that do not exist yet, which is exactly what a universal
requires.

Pinned by `tests/constitution/invariant_enforcement.rs::i_8_policy_decision_signatures_cannot_see_reasoning`,
which scans the **parameter lists** of policy decision surfaces and fails if one
grows a reasoning argument. Verified to bite: injecting
`can_probe_injected(&self, reasoning: &str) -> PolicyDecision` fails the test at
`policy.rs:679`.

The test deliberately scans parameters, not signatures. Matching function
*names* would flag `can_audit_reasoning(&self, target_agent_id: &str)` — the
`ReasoningAudit` capability check that **Ri-0.13(c) explicitly requires.** That
is a decision *about* reasoning access, not one *using* reasoning content, and
flagging it would have reported the constitution working correctly as a
violation.

### I-9 — enforced *by construction*, by a closed enum

`runtime/checkpoint.rs::YieldReason` is a closed Rust enum, so exhaustive
matching makes an unlisted termination a compile error, and deserialization
rejects unknown variants. Existing coverage:
`rights_mid_bucket.rs::ri_0_12_all_yield_reasons_roundtrip` and
`ri_0_12_unknown_yield_reason_rejected`. New:
`invariant_enforcement.rs::i_9_yield_reason_is_a_closed_enum_in_source`, which
pins the *closedness* the citation claims — it fails if the enum becomes
`#[non_exhaustive]`, since a wildcard arm would admit exactly the silent
unlisted reason `I-9` forbids.

### I-4 — enforced by *detection*, not proof

A universal negative over behaviour ("the gateway does not decide on the
agent's behalf") cannot be proven statically. The enforceable form is making
each lapse visible and counted, which is built:
`runtime/discretion_leak.rs::record_discretion_leak` writes a durable
`discretion_leak` causal event carrying the rule ID of its closest named site,
queryable through `autonoetic trace contract-health`. The existing `P-4.11`
exception text is unchanged.

### I-2 — enforced through P-8.16

`P-8.16` already requires causal-chain append to be `fsync`-durable before any
state transition that depends on it, with citations to `causal_chain.rs`,
`execution.rs`, `scheduler/gateway_store/mod.rs` and
`runtime/tools/promotion.rs`. `I-2` is the cross-cutting statement of that
rule; the citation now says so rather than leaving the reader to find it.

## Class 2 — An honest downgrade

### I-3 — was bare, now **PARTIAL** with a named gap

Three paths are enforced by name — `P-4.7` (credential responses), `P-4.13`
(logs, traces, digests, LLM prompts), `P-4.14` (before causal-chain append) —
and `log_redaction.rs::RedactedPayload` exists as a newtype.

But the **universal is not held.** Store write paths still accept unwrapped
values (`CausalChainEntry.payload`, `CausalEventRecord.payload`), so a
persistence path added tomorrow escapes `I-3` silently. That is precisely the
case a universal exists to cover, and it is the one case three enumerated rules
cannot reach.

This is recorded as `PARTIAL` with the gap named, in the discipline §12 already
applies to `U-1`–`U-3`: *the debt is visible and named rather than absent from
the text entirely.* Closing it means requiring `RedactedPayload` at the write
API, where the compiler covers paths that do not exist yet.

**No status was upgraded in this amendment. One was made more pessimistic.**

## Deliberately unchanged

- **I-5** (rules in manifests, not hard-coded Rust constants) — needs static
  analysis over the source with a documented allowlist, in the shape of the
  existing docs guards. Not built, so not cited.
- **I-7** (rights supersede rules on conflict) — a meta-rule about the
  amendment process. May not be testable in principle beyond "a conflict
  escalates rather than resolving silently".
- **I-13** — already documents its own status in its own words ("declared
  invariant documenting an existing deliberate absence"). Left alone.

Invariants without a citation: **8 → 2** (`I-5`, `I-7`).

## On the "fails before, passes after" requirement

Amendment requirement 2 asks for a test module that fails before the change and
passes after. This amendment changes no behaviour, so nothing about the *code*
fails beforehand — the requirement is written for additions and changes.

What fails beforehand is the **completeness check on the text itself**:
`invariant_enforcement.rs::amendment_2026_09_02_closes_five_uncited_invariants`
asserts both sides — that `2026.08.30` really did leave `I-2`, `I-3`, `I-4`,
`I-8`, `I-9` uncited, and that `2026.09.02` cites all five. Running it against
the baseline alone fails; against the amendment it passes. The delta is the
artifact.

This wrinkle is worth noting for the *retirement* case too (#1277): deleting a
redundant clause has the same shape, and the test that catches it is a
non-duplication check asserting every clause has a distinct enforcement
citation — which would have caught `R+9` when it was written.

## Signing and activation

This directory is **draft**: `constitution.md` + `RATIFY.md`, no
`gateway-constitution.lock.json`. Following the established three-step ceremony:

1. **Draft** — this PR. No lock, `CURRENT` untouched,
   `ACTIVE_CONSTITUTION_VERSION` still `2026.08.30`.
2. **Sign** — `python3 docs/constitution/recompute_lock.py --version 2026.09.02
   --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"`, then update
   `docs/constitution/CURRENT`. Requires the signing key (cf. `f340a78f`).
3. **Activate** — flip `ACTIVE_CONSTITUTION_VERSION` in
   `autonoetic-types/src/config.rs` and `docs/reference/config.md`
   (cf. `9f9f2377`), then:

```bash
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
BLESS_GLOSSARY=1 cargo test -p autonoetic-gateway bless_constitution_glossary
BLESS_REGISTER=1 cargo test -p autonoetic-gateway bless_enforcement_register
```

The last two matter: an activation invalidates every constitution-derived
artifact at once, and missing the glossary re-bless is what turned `main` red
after the `2026.08.30` activation (#1252).
