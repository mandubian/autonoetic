# Federation Carry-Forward

_Status: design (draft). Targeted at the re-federation churn observed in
`session-964ea6d7` and addressed partially by #1066 (build-time capability
validation + planner manifest preflight). This doc is the spec for the deeper,
opt-in follow-up._

## Problem

A federation verdict (`unit_test_runner` / `static_evaluator` / `auditor`) is
recorded against the **whole-artifact** content digest
(`artifact_canonical_digest`). Any rebuild — even a one-line `SKILL.md` prose
fix that leaves the code and the declared contract byte-identical — produces a
new digest and voids **every** prior verdict. The planner must then re-spawn
all three gates.

In `session-964ea6d7` (gmail-reader promotion) this happened three times in a
row: each round surfaced one `SKILL.md` defect, the coder fixed that one
field, rebuilt, and the planner re-ran the full 3-gate fan-out — including the
42-test `unit_test_runner` and the `auditor` LLM review, neither of which
reviewed anything that had changed.

#1066 collapses the two **avoidable** rounds (build now rejects malformed
capabilities; the planner preflights the manifest before spawning gates). The
residual cost is the **legitimate** rebuild case: a coder fixes a real
`static_evaluator` finding in `SKILL.md`, the code is unchanged, yet
`unit_test_runner` + `auditor` re-run anyway because their prior verdicts are
digest-voided.

## Goal

Let a gate's verdict survive a rebuild **when the bytes that gate actually
reviewed did not change** — without weakening the tamper-evidence of the
verdict, and without delegating a safety-critical decision to LLM judgment.

## Non-goals

- **Not** caching gate verdicts across *different* artifacts (no "we audited
  something similar last week"). Carry-forward only relates two artifacts in
  the same promotion lineage where one is a deliberate rebuild of the other.
- **Not** replacing the operator escalation. Carry-forward verdicts are still
  surfaced in `federation_escalate` for operator review; the operator sees
  which bytes each verdict covers and which are carried.
- **Not** making the agent the authority on what's safe. The agent proposes;
  the gateway disposes.

## Design principles

1. **The decision to skip a gate is a reasoning act; the floor is mechanical.**
   The planner reads a structured change-classification, reasons about whether
   the change is big enough to warrant re-running a given gate, and proposes a
   carry-forward with a justification. The gateway **verifies** the proposal
   against per-input digests and **rejects** any proposal that exceeds the
   configured strictness floor. The agent can always be *more* conservative
   than the floor (re-run a gate it was allowed to skip); it can never be more
   permissive.

2. **Safety-critical invariants stay mechanically enforced.** Per
   `AGENTS.md`'s Separation of Powers: the agent does not get to assert "only
   prose changed" — the gateway computes the per-input digests itself and
   checks the claim. A tampered or hallucinated claim fails closed (gate must
   re-run).

3. **The strictness floor is operator-configured, not agent-configured.** An
   agent cannot dial its own strictness; that would always trend to the most
   permissive. The dial lives in `config.yaml` and is enforced opaquely — the
   planner simply receives a structured rejection if its proposal is below the
   floor.

4. **Provenance is preserved end-to-end.** A carried verdict is never silently
   aliased. It is recorded with `carried_from: {prior_artifact_id,
   prior_record_id, verified_at, justification}`, so the audit trail from a
   live promotion back to the bytes each gate originally reviewed is
   unbroken.

## Where the decision lives: gateway verifies, planner reasons

The answer to "gateway side or planner side?" is **both, split by concern**:

| Concern | Owner | Why |
|---|---|---|
| Compute per-input digests (code / contract / prose) | **Gateway** | Must be tamper-evident and non-gameable; cannot trust agent-supplied hashes |
| Classify frontmatter fields as semantic vs prose | **Gateway** (fixed const table) | The boundary between "contract changed" and "prose changed" is the security boundary; can't be agent-defined |
| Surface a structured change-diff to the planner | **Gateway** (`artifact_inspect` / new `artifact_diff`) | Reasoning needs evidence; evidence must come from the privileged side |
| Decide which gates to re-run vs carry, per rebuild | **Planner** (reasoning) | Context-sensitive; the planner saw *why* the coder rebuilt |
| Construct the carry-forward claim with justification | **Planner** | The justification is the reasoning trace |
| Verify the claim against digests + strictness floor | **Gateway** | Mechanical, fail-closed |
| Record provenance + expose to operator | **Gateway** | Tamper-evidence + audit |

This is the same pattern as the approval system (agent requests, gateway
dedups + enforces caps) and plan-capability-grants (operator approves envelope,
gateway materializes as a grant). The agent is the strategist; the gateway is
the executor that refuses unsafe moves.

## The three-digest model

Every `agent_bundle` artifact gets three digests computed at build time, in
addition to the existing whole-artifact `artifact_canonical_digest`:

| Digest | Covers | Which gates care |
|---|---|---|
| `code_digest` | Per-file SHA-256 of every file classified as **code**: declared `entrypoints`, `*.py`/`*.js`/`*.rs`/… source, test files, `requirements.txt`/`package.json`/`Cargo.toml` (deps change test execution) | `unit_test_runner`, `auditor` |
| `contract_digest` | Canonical-JSON SHA-256 of the **semantic** frontmatter fields (see table below) | `unit_test_runner`, `auditor`, `static_evaluator` |
| `prose_digest` | Per-file SHA-256 of everything else: SKILL.md prose body, `description`, README, examples, comments | `static_evaluator` only |

A gate's verdict is reusable across a rebuild **iff the digests of all inputs
that gate reviews are byte-identical**:

- `unit_test_runner` reviews `code_digest` + `contract_digest`
- `auditor` reviews `code_digest` + `contract_digest`
- `static_evaluator` reviews `code_digest` + `contract_digest` + `prose_digest`

This is tighter than "the whole artifact didn't change" and looser than
"nothing changed". It is exactly "the bytes this gate actually looked at
didn't change".

### Frontmatter field classification

The contract/prose boundary is a **fixed const table** in the gateway, not an
LLM judgment. Anything not listed defaults to **prose** (fail-safe: an unknown
field does not get to void code-gate verdicts, but also does not silently
bypass contract checks).

| Frontmatter field | Class | Rationale |
|---|---|---|
| `capabilities` | contract | defines what the agent may do — every gate reviews against this |
| `remote_access` | contract | auditor + static_evaluator verify code stays within declared hosts |
| `entrypoints` | contract | unit_test_runner + static_evaluator assume the entry shape |
| `script_input_mode` | contract | the round-1 defect in `session-964ea6d7`; determines how code receives input |
| `io_accepts`, `io_returns` | contract | unit tests assert against this schema |
| `credential_services` | contract | auditor verifies code reads secrets the declared way |
| `dependencies` / `layers` | contract | unit_test_runner imports through layers |
| `execution_mode` | contract | changes sandbox semantics all gates assume |
| `sandbox_network`, `egress` | contract | auditor + static_evaluator network posture |
| `background`, `disclosure` | contract | affects what the gates must consider |
| `name`, `description`, `metadata.autonoetic.agent.{name,description}` | prose | presentation only |
| SKILL.md body prose, examples, comments | prose | guidance text |
| README, CHANGELOG, doc files | prose | non-executable, non-contract |

A field not in either list defaults to **prose** and is logged at INFO so a
missing classification is visible during the rollout window.

## The carry-forward lifecycle

```
 ┌─────────────────────┐         ┌──────────────────────────────┐
 │ build artifact v1   │────────▶│ promotion.record by 3 gates  │
 │ (code/contract/prose│         │ each record stores the 3     │
 │  digests computed)  │         │ digests of v1 at record time │
 └─────────────────────┘         └──────────────────────────────┘
                                           │
                                           │  static_evaluator flags a
                                           │  SKILL.md prose defect
                                           ▼
 ┌─────────────────────┐         ┌──────────────────────────────┐
 │ coder fixes prose,  │────────▶│ build computes v2 digests     │
 │ rebuilds → v2       │         │ gateway emits structured diff │
 └─────────────────────┘         └──────────────────────────────┘
                                           │
                                           ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ PLANNER REASONS (reads the diff)                             │
 │  "code_digest(v1)==code_digest(v2): true                     │
 │   contract_digest(v1)==contract_digest(v2): true             │
 │   prose_digest differs (that's the fix)                      │
 │   → unit_test_runner and auditor reviewed identical inputs   │
 │   → I will carry them forward and re-spawn static_evaluator  │
 │     only, with justification: 'prose-only fix to the         │
 │     script_input_mode doc block'."                           │
 └─────────────────────────────────────────────────────────────┘
                                           │
                                           ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ federation_escalate({                                        │
 │   artifact_ref: v2,                                          │
 │   role_verdicts: [                                           │
 │     {role: unit_test_runner, carried_from: <v1 record id>,  │
 │      justification: "code+contract unchanged"},              │
 │     {role: auditor, carried_from: <v1 record id>, ...},      │
 │     {role: static_evaluator, passed: true, ...}  // fresh    │
 │   ]})                                                        │
 └─────────────────────────────────────────────────────────────┘
                                           │
                                           ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ GATEWAY VERIFIES (per claim)                                 │
 │  • prior record exists + is terminal-pass                    │
 │  • code_digest(v1)==code_digest(v2)        ✓                 │
 │  • contract_digest(v1)==contract_digest(v2) ✓                │
 │  • strictness floor allows carry when only prose differs  ✓  │
 │  → accept; record carried_from provenance                    │
 │  (any failure → reject the claim; that gate must re-run)     │
 └─────────────────────────────────────────────────────────────┘
```

### Failure modes (all fail-closed)

- **Planner omits a required gate entirely** → `federation_escalate` already
  rejects (existing behavior; unchanged).
- **Planner claims carry but a reviewed digest differs** → gateway rejects
  the claim with `"carry_forward_rejected: role={role}, reason={code_digest_mismatch | contract_digest_mismatch}, strictness={level}"`. Planner must re-run that gate.
- **Prior record is not terminal-pass** (e.g. `warning`, `unable_to_evaluate`)
  → not eligible for carry; must re-run.
- **Strictness floor disallows** (e.g. `off`, or `conservative` and contract
  changed) → rejected even if digests would match.
- **Planner hallucinates a prior record id** → not found → rejected.

In every rejection the planner gets a structured reason and re-runs only the
rejected gate, not the whole federation.

## Strictness dial

Operator-configured in `config.yaml`, enforced opaquely by the gateway. The
planner does not read this value; it simply receives rejections when it
proposes below the floor.

```yaml
federation:
  # How conservative the gateway is when accepting a gate-verdict
  # carry-forward across a rebuild. The planner always proposes; the gateway
  # verifies against this floor.
  #
  # off          — every rebuild re-runs every gate (today's behavior; default)
  # conservative — carry forward only when prose_digest is the only difference
  # per_role     — carry forward whenever the gate's reviewed input digests match
  carry_forward_strictness: off
```

| Level | Allows carry when… | Does NOT allow carry when… | Use when |
|---|---|---|---|
| `off` | (never — full re-federation always) | everything | default; regulated/paranoid environments; until trust is earned |
| `conservative` | only `prose_digest` differs (code + contract byte-identical) | code OR contract changed at all | the `session-964ea6d7` class — SKILL.md prose fixes after a gate flags them |
| `per_role` | each gate's reviewed-input digests match its prior record | a gate's inputs changed | high-volume dev loops where contract-stable code edits are common |

`off` is the initial default so an upgrade produces **zero** behavior change.
Operators opt in per environment.

The planner is always free to be more conservative — e.g. under `per_role`,
seeing that the prose change was in a security-sensitive section, the planner
may choose to re-run `auditor` anyway. The floor is a maximum permissiveness,
not a mandate.

## Tamper-resistance analysis

Today's invariant: *a `promotion_record` proves a gate reviewed a specific,
whole artifact, end to end.* Carry-forward narrows the binding to *a gate
reviewed a specific **subset** of the artifact, end to end*. The subset is
defined by a fixed, gateway-owned classification — the agent cannot rename a
file to move it between classes, because classification is by content + the
declared `entrypoints`, not by agent-supplied tags.

Threats considered:

1. **Coder hides a code change in a "prose" file.** Mitigation: file
   classification is by extension + entrypoint declaration, not by directory
   or agent label. A `.py` file is code regardless of where it lives.
   `dependencies`/`layers` are contract, not prose, so smuggling code through
   a dep manifest still trips `contract_digest`.
2. **Coder changes a contract field but labels it prose.** Mitigation: the
   field→class table is a gateway const; the agent has no API to override it.
   Unknown fields default to prose **and log**, so anything unclassified is
   visible during rollout.
3. **Planner claims carry for a digest that doesn't match.** Mitigation:
   gateway recomputes digests itself from the stored artifact bytes; the
   planner only supplies the prior record id. Mismatch → rejected.
4. **Replay of a stale carried verdict across an unrelated lineage.**
   Mitigation: carry-forward is only honored between artifacts in the **same
   promotion lineage** (the rebuild must descend from the prior artifact via
   the coder's `content_write` → `artifact_build` chain; tracked via the
   existing `source_artifact_ref` lineage the packager already maintains).
5. **Operator misses that a verdict was carried.** Mitigation: the operator
   UI (`promotion_query`, escalation surface) shows `carried_from` + the
   input-coverage digests for every carried role, distinct from fresh
   verdicts. No verdict looks identical to a fresh one when it isn't.

Net: the verdict-to-bytes binding is preserved for the bytes that gate
actually reviewed. The thing that weakens is the implicit assumption "and
nothing else in the artifact is relevant to this gate" — which was always an
over-approximation (today's whole-artifact digest also covers files the gate
ignored).

## Data model and migration

Schema version bump `79 → 80`. Add columns to the gate-verdict records table:

```sql
ALTER TABLE <gate_verdicts> ADD COLUMN code_digest        TEXT;
ALTER TABLE <gate_verdicts> ADD COLUMN contract_digest    TEXT;
ALTER TABLE <gate_verdicts> ADD COLUMN prose_digest       TEXT;
ALTER TABLE <gate_verdicts> ADD COLUMN carried_from_id    TEXT;       -- FK to prior verdict
ALTER TABLE <gate_verdicts> ADD COLUMN carry_verified_at  TEXT;
ALTER TABLE <gate_verdicts> ADD COLUMN carry_justification TEXT;
CREATE INDEX IF NOT EXISTS idx_gate_verdicts_input_digests
    ON <gate_verdicts>(artifact_id, code_digest, contract_digest, prose_digest);
```

(`SCHEMA_VERSION_LATEST` → 80; new `apply_federation_carry_forward_v80()` in
`migrate.rs`, gated by the standard `if current >= 80 { return Ok(()) }`.)

Existing records get `NULL` digests → treated as **unverifiable** → must
re-run. Carry-forward only helps from the first post-migration rebuild onward.
This is intentional: we do not backfill digests for records we cannot prove
were computed under the new classification.

## API / tool surface

| Surface | Change |
|---|---|
| `artifact_build` | Also computes + persists `code_digest`, `contract_digest`, `prose_digest` (gateway-owned, like `content_digest` today) |
| `artifact_inspect` | Returns the three digests + a `lineage: {source_artifact_ref, prior_verdict_coverage}` block when applicable |
| `artifact_diff` (new) | `artifact_diff(from: <ar.*>, to: <ar.*>)` → `{code_changed, contract_changed, prose_changed, changed_fields: [...], changed_files: [...], verdicts_eligible_for_carry: [{role, prior_record_id}]}`. The planner's reasoning input. |
| `promotion.record` | Records the three digests at write time (gateway-computed; agent cannot supply them, mirroring `content_digest` today) |
| `federation_escalate` | Accepts `carried_from: <prior_record_id>` + `justification` per role-verdict; verifies; records provenance; rejects below-floor claims with a structured error |
| `promotion_query` | Returns `input_coverage: {code_digest, contract_digest, prose_digest}` and `carried_from` per verdict |
| Config (`config-template.yaml`, `autonoetic-types/src/config.rs`, `docs/config-reference.md`) | New `federation.carry_forward_strictness` enum, default `off` |

`artifact_diff` is the reasoning enabler. Without it the planner would have to
guess what changed by diffing raw `resolve` output, which is exactly the kind
of unreliability this design avoids.

## Planner prompt changes

`agents/lead/planner.default/SKILL.md` Evaluation Federation section gains:

1. After a coder rebuild, before re-spawning gates: call
   `artifact_diff(from=<prior ar.*>, to=<current ar.*>)`.
2. Read the structured diff. Reason out loud: which gates reviewed inputs that
   did not change? Which did?
3. For each gate whose reviewed inputs are byte-identical **and** whose prior
   verdict was a terminal pass, you MAY propose a carry-forward in
   `federation_escalate` with a one-sentence `justification`. The gateway
   verifies; if it rejects, re-run only that gate.
4. You may always choose to re-run a gate you were allowed to carry — do so
   when the change was in a security-adjacent area (capability declarations,
   remote_access, secret handling) even if the diff says the gate's inputs
   were stable.
5. Carry-forward never exempts you from the operator escalation or from
   surfacing `carried_from` provenance in your reply.

The existing Step 0 manifest preflight (from #1066) and the failure-routing
rule ("Static evaluator fails → re-preflight") stay; they compose. Step 0
catches the *avoidable* cases; carry-forward absorbs the *legitimate* rebuild
cases that slip through.

## Operator visibility

- `promotion_query` output: each role verdict shows
  `input_coverage: {code_digest, contract_digest, prose_digest}` and either
  `fresh: true` or `carried_from: {record_id, artifact_ref, verified_at,
  justification}`. A carried verdict is visually distinct.
- The escalation surface (`/approvals`, CLI `gateway approvals show <id>`)
  renders carried verdicts with a "↻ carried from `<prior ar.*>`" marker and
  the justification, so the operator cannot mistake a carried verdict for a
  fresh one.
- A causal event `federation.carry_forward` is emitted on each accepted carry,
  keyed to `(prior_record_id, new_record_id, role, strictness)` — same shape
  as `grant_revocation` for audit queries.

## Rollout

Four independently-shippable stages. Each is a standalone PR, each leaves
strictness at `off` (zero behavior change) until the stage that flips it.

| Stage | Content | Behavior change at `off`? |
|---|---|---|
| **1. Compute + store** | Gateway computes the 3 digests at `artifact_build`; `promotion.record` stores them; migration. Strictness still `off`. | None (just extra columns, all `NULL` for old rows) |
| **2. Diff surface** | `artifact_diff` tool + `artifact_inspect` enhancement. Planner prompt learns to *read* the diff but still re-runs everything. | None (planner gets new info, no carry yet) |
| **3. Verify + claim** | `federation_escalate` accepts `carried_from`, verifies against digests, records provenance. Strictness floor enforced. Default still `off`. | None unless operator opts in |
| **4. UI + audit** | `promotion_query` coverage fields, escalation-surface markers, `federation.carry_forward` causal event. | Surface-only |

Operators flip `carry_forward_strictness: conservative` (or `per_role`) in
their own `config.yaml` when they've watched Stage 2 logs and are satisfied
the field classification is sound for their agent population.

## Risks and open questions

1. **Layered artifacts (packager).** A layered artifact's `code_digest` must
   fold in layer file digests, not just the coder's base files. Need to decide
   whether each layer gets its own digest triplet or the triplet covers the
   composed bundle. _Lean: triplet over the composed bundle (matches what the
   gates actually import), with per-layer digests stored for diagnostic
   surface only._
2. **The field-classification table is authority.** Any misclassification is a
   security hole (too strict → wasted re-runs; too loose → silent bypass).
   Mitigation: default-to-prose + INFO log for unclassified fields + a
   sentinel test asserting every frontmatter field used by the bundled agents
   is classified.
3. **Constitution / enforcement register.** The constitution may cite the
   whole-artifact-digest invariant. Need to check `enforcement_register.rs`
   and `docs/constitution/enforcement-register.md` and update both copies if a
   cited test changes name. The register's `every_parseable_citation_resolves`
   test will fail loudly if we miss one.
4. **First-rebuild-only value.** Carry-forward helps from the 2nd rebuild of a
   lineage onward. The first federation always runs fresh. That's the whole
   point (collapse the 2nd/3rd rounds), but worth setting expectations.
5. **"Big enough to re-run" is genuinely fuzzy for code edits.** Under
   `per_role`, a one-line code change in a non-entrypoint still trips
   `code_digest` → both code gates re-run. That's correct (the gates can't know
   the line was irrelevant), but means `per_role` mostly pays off for
   contract-stable, prose/manifest-only rebuilds — the same population
   `conservative` covers. _Open: is `per_role` worth the complexity over
   `conservative`, or do we ship `off` + `conservative` only and defer
   `per_role` until there's a demonstrated need?_

## Alternatives considered

- **Pure mechanical per-role digest, no reasoning (original Option A).**
  Rejected per the project's steer: the decision should be a reasoning act.
  The planner-adds-conservatism direction (re-run a skippable gate when the
  change is security-adjacent) is lost under a pure mechanical rule.
- **Per-file manifest stored per verdict (Option B).** More flexible, more
  storage, same tamper story. Rejected: the three-digest triplet captures the
  same guarantee with a fixed, auditable classification, and avoids
  per-verdict manifest churn.
- **Explicit operator carry-forward approval per rebuild (Option C).**
  Strongest posture, but couples every rebuild to an operator round-trip —
  defeating the entire purpose (the planner is already paused on the gate
  round; adding an approval makes it slower, not faster). Rejected as the
  default; could be added later as a fourth strictness level
  (`operator_approved`) for regulated environments if ever needed.

## Related

- #1066 — build-time capability validation + planner manifest preflight
  (Options 1 + 2; ships the avoidable-round collapse)
- `docs/approval-system.md` — the five-layer approval dedup model this design
  mirrors (planner proposes, gateway dedups + caps)
- `docs/plan-capability-grants.md` — same "operator approves envelope, gateway
  materializes + enforces" pattern
- `agents/lead/planner.default/SKILL.md` — Evaluation Federation section
  (Step 0 preflight composes with this design)
- `session-964ea6d7` — the three-round re-federation case this and #1066
  target
