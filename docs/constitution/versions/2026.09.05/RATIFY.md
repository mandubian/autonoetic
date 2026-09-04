# RATIFY.md — Constitution Version 2026.09.05

## Summary

**A one-character repair.** `P-5.2`'s statement contains a literal `|` inside
a code span, which splits its table row and makes the *digest* record the
wrong enforcement citation. One clause statement is edited, typographically.
Baseline: **2026.09.04**.

Nothing else changes. No clause is added or removed, no `Relation` moves, no
normative content is touched.

## The defect

`P-5.2` reads, in part:

> Coercion is deterministic only. The LLM-coercion fallback was removed:
> `SchemaEnforcementMode` is `Disabled | Deterministic` …

`extract_enforcement_table` builds the digest's rule-enforcement table by
splitting each row on `|`, filtering empty cells, and reading `cells[3]`. The
pipe inside that code span produces a seventh cell, so `cells[3]` lands on the
**Source** column and the digest records:

    P-5.2 -> "schema-enforcement-hook.md"

instead of the actual citation
(`autonoetic_types::schema_enforcement::{SchemaEnforcer, default_enforcer}` …).

The repair replaces `` `Disabled | Deterministic` `` with `` `Disabled` or
`Deterministic` ``. Same meaning, same enumeration, no pipe. `P-5.2` now has
six cells like every other clause row, and `cells[3]` is its enforcement.

## Why this needed an amendment rather than a fix

Three routes were available and two do not work. Recorded because the reasoning
is the interesting part, and because the next person to meet a table-breaking
character in signed text will face the same three:

1. **Escape it (`\|`).** Does nothing. The parser plain-splits and ignores
   markdown escaping, so the row still breaks. It would change signed bytes
   while fixing nothing.
2. **Teach the parser to honour `\|`.** Correct in isolation, but **six
   already-signed versions** (2026.05.05 through 2026.05.29) contain escaped
   pipes. Their digests would stop reproducing, and "the digest is the identity
   of the law" is exactly what breaks. A parser change is retroactive in a way
   a text change is not.
3. **Remove the character.** What this amendment does.

## The deliberate exception

`2026.09.04` stated that no clause statement is edited. **This amendment edits
one**, and says so plainly rather than smuggling it under a typographical
heading.

The edit is meaning-preserving — an enumeration of two enum variants, rendered
identically — but the rule that clause statements are not edited exists so that
"typographical" cannot become a door. This is the exception, it is one clause,
and the alternative was leaving a wrong enforcement citation frozen in the
digest of the law now in force.

## What the repair also enables

`constitution/relation_column.rs::every_clause_row_is_well_formed` asserts that
every clause row in the newest constitution version has exactly six cells.
Against `2026.09.04` it fails — `P-5.2` has seven; against `2026.09.05` it
passes for all 221.

Two malformations of this class have now been found by hand: `P-9.13` was
missing a cell (repaired in the 2026.09.04 amendment, where the digest recorded
its *Status* as the citation) and `P-5.2` has one too many. Both were invisible
because the parser filters empty cells and never checks arity. The guard closes
the class.

Earlier versions are not checked: their bytes are frozen, and several carry
known malformations that can only be repaired by amendment.

## Signing and activation

Draft: `constitution.md` + `RATIFY.md`, no lock, `CURRENT` and
`ACTIVE_CONSTITUTION_VERSION` still `2026.09.04`.

1. **Draft** — this PR.
2. **Sign** — `python3 docs/constitution/recompute_lock.py --version 2026.09.05
   --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"`, then update
   `docs/constitution/CURRENT`.
3. **Activate** — flip `ACTIVE_CONSTITUTION_VERSION` in
   `autonoetic-types/src/config.rs` and `docs/reference/config.md`, then
   re-bless the three derived artifacts:

```bash
cargo nextest run -p autonoetic-gateway --lib -E 'test(constitution_lock_matches_canonical_digest_and_counts)'
cargo nextest run -p autonoetic-gateway --test constitution
BLESS_GLOSSARY=1  cargo nextest run -p autonoetic-gateway --lib -E 'test(bless_constitution_glossary)'
BLESS_REGISTER=1  cargo nextest run -p autonoetic-gateway --lib -E 'test(bless_register_doc)'
BLESS_LAW_TABLE=1 cargo nextest run -p autonoetic-gateway --lib -E 'test(bless_law_table)'
```

**The rule-enforcement count in the lock will change**, because `P-5.2`'s entry
changes from `schema-enforcement-hook.md` to its real citation. That is the
repair landing, not a discrepancy.
