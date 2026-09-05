# The Amendment Materializer

How an approved Ri-0.8 proposal becomes a candidate constitution version,
mechanically. The command surface is documented in
[`../reference/cli.md`](../reference/cli.md#autonoetic-gateway-constitution-materialize);
this note is for maintainers: it records the **invariants** the implementation
must preserve, why they exist, and which test pins each one. If you change the
materializer or anything it depends on, this is the list to re-read first.

Implementation: `autonoetic-gateway/src/constitution_materializer.rs`.

## The premise: a proposal is already a patch

`constitution_propose_amendment` never accepted free text. Intake persists a
structured triple — `kind` ∈ {`add`,`modify`,`remove`} × {`rule`,`right`},
`target_id` (the clause address), `proposed_text` (the payload) — and the O-6
adjudication flow forces a recorded operator decision on it. The materializer
therefore creates **no new authority**: it is the first *consumer* of a format
that already existed and was already adjudicated. It is a deterministic
function of the adjudicated row — no LLM, no heuristic, no judgment (Lawful
Executor, §14).

## Invariants

### 1. The clause ID is the only addressing scheme

Rows are located by exact match on the first table cell. The same key space
already has three consumers — the digest's enforcement-table extractor
(`cells[0]` in `constitution_digest.rs`), the enforcement register, and the
relation-column tests. The materializer is the fourth. Never introduce a
second way to address a clause (section names, prose search, fuzzy matching):
two addressing schemes *will* disagree eventually.

### 2. Row arity is sacred

Positional cell access (`cells[3]` = the enforcement citation) only works if
every clause row has exactly six non-empty cells. This was learned twice, by
hand: `P-9.13` was missing a cell (the digest recorded its Status as the
citation) and `P-5.2` carried a literal `|` inside a code span (seven cells;
the digest recorded its Source). Both corrupted the digest silently — no check
looked at arity until `relation_column.rs::every_clause_row_is_well_formed`
was added by 2026.09.05, the amendment that repaired `P-5.2` by removing the
character (a parser fix would have been retroactive: six signed versions
contain escaped pipes).

Consequences the materializer enforces at drafting time:

- a proposed statement containing `|` is refused, with the P-5.2 story in the
  error — the defect class is closed at the source, not downstream;
- `modify` refuses a base row whose non-empty cell count ≠ 6 — it will not
  edit a row the digest cannot parse unambiguously;
- `add` emits exactly six cells, so the arity guard stays green.

Pinned by `constitution_materializer::tests::pipe_in_statement_is_refused_the_p5_2_lesson`
and `::malformed_base_row_is_refused_for_modify`.

### 3. Modify moves one cell; every other byte stands still

The row is split on `|`, only the statement segment is replaced, and the line
is rejoined. Source, Enforcement, Status and Relation survive byte for byte.
The digest covers the whole text, so any amendment bumps it — that is expected.
What modify must never do is smuggle an enforcement-fact change inside a
statement change; byte-preservation makes that unrepresentable, and the
provenance diff shows exactly one cell moving.

### 4. Add derives its insertion point and refuses to classify

The section family is parsed from the target ID itself (a target in the
`P-8.*` family yields the prefix `P-8.`), and the row is inserted after the
*last* row with that prefix. No section headers consulted; if no sibling
exists, the materializer refuses — a new *section* is a structural act no
mechanical inserter should perform.

The inserted row carries explicit `DRAFT` / `TBD` placeholder cells. The
distinction is deliberate: `TBD — not yet implemented` is a **fact** the
gateway may state; Relation (`binds · owed_to · requires`) is normative
classification the gateway must **not author**. Because the row is still
six cells, the digest's enforcement table includes the clause citing
"TBD — not yet implemented" — which is true. The table never lies; it waits.
Completing the classification is the operator's substantive act, surfaced in
the scaffold RATIFY.md (see below).

### 5. Family consistency: `*_rule` → `P-*`, `*_right` → `Ri-*`

Intake does not enforce the family match, and adjudication might miss it —
but the materializer addresses rows mechanically, so a mismatch would
silently edit the wrong family's table. Refused in all three apply paths.
Pinned by `::kind_target_family_mismatch_is_refused`.

### 6. The unsigned lock is a prediction, cross-checked at signing

The candidate lock is the same `ConstitutionLock` struct with
`signature: None`. Its digest comes from `compute_constitution_digest()` —
the same extraction + canonical-payload + SHA-256 path boot uses, byte-identical
to `docs/constitution/recompute_lock.py` (Rust `BTreeMap`-ordered serde JSON ≡
Python `sort_keys=True`; compact separators; `ensure_ascii=False` parity).
Stable fields (`format_version`, `constitution_id`, `canonicalization`) are
inherited from the base lock exactly as the script's template seeding
inherits them.

Do not "improve" either side of this parity in isolation: the unsigned lock's
digest is a **prediction** that the Python script must reproduce over the same
bytes. The two implementations cross-check each other at signing time; the
boot-time verify and
`constitution_lock_matches_canonical_digest_and_counts` are what fail if they
drift.

Precondition guarding the whole construction:
`compute(base_text) == base_lock.constitution_digest`. The materializer
refuses to draft on a base that does not reproduce its own pin.

### 7. The candidate is inert

The candidate directory touches nothing: not `docs/constitution/CURRENT`, not
`ACTIVE_CONSTITUTION_VERSION`, not any signed byte. Existing candidate
directories are never clobbered; base bytes are never mutated (the apply
function takes `&str` and returns new text; failures leave the input
untouched). The unsigned lock refuses boot under the default
`constitution.require_signature=true` — the materializer has **no path to
law**. Even a malicious draft needs the operator's Ed25519 key, and the diff
that key would bless is in `provenance.json` for review first.

### 8. Voice is split from fact

`provenance.json` holds the machine facts: proposer, filing justification,
adjudication (`decided_by`/`decided_at`/`decision_reason`), before/after
rows, both digests. The scaffold `RATIFY.md` fills in the same facts plus the
ceremony, and raises explicit `RATIFY-PLACEHOLDER` markers for everything
that is *argument* — the ratification rationale, and, when an edit touches a
clause in `enforcement_register::entrenched_clauses()`, the dated
entrenched-clause justification the amendment process mandates. The gateway
quotes the agent's filing justification as a fact; it never writes the
operator's judgment. Pinned by `::scaffold_omits_conditional_sections_when_not_applicable`.

### 9. Store stamping is atomic and idempotent

`materialized_in_version` (migration v88) is deliberately separate from
`published_in_release` — drafting and release-labelling are different acts;
conflating them would make `gateway constitution release` a silent no-op for
materialized proposals. `mark_proposals_materialized` uses
`UPDATE … RETURNING` with a `IS NULL` guard, so stamping is once-only under
concurrent operators. The migration's `ALTER` is guarded on table existence
because migrations ≤ v87 are skipped by synthetic partial databases (see the
upgrade-path test in `tests/constitution/decider_appointment.rs`).

## The refusal inventory

Every refusal is loud, specific, and leaves the world untouched: unknown
kind, kind/target family mismatch, `|` or newline in a statement, unknown
target, duplicate target for `add`, no section siblings for `add`, malformed
base row, base digest mismatch, existing candidate directory, path-unsafe
version string, candidate == base, empty proposal batch. If you add a new
refusal, give it a test; if you remove one, update this list and explain why
in the PR.
