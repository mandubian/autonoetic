# Federation Carry-Forward

_Status: implemented (Stages 1–4 merged: #1068, #1069, #1070, #1071; layered-
artifact fix #1073). Default `off`; operators opt in via
`federation.carry_forward_strictness: conservative`. Composes with #1066
(build-time capability validation + planner manifest preflight), which
targets the avoidable re-federation rounds; this design absorbs the
legitimate-rebuild rounds that slip through._

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
   aliased. It is recorded with `carried_from: {prior_artifact_ref, role,
   verified_at, justification}`, so the audit trail from a live promotion back
   to the bytes each gate originally reviewed is unbroken.

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

Every `agent_bundle` artifact gets three digests computed from its bytes, in
addition to the existing whole-artifact `artifact_canonical_digest`:

| Digest | Covers | Which gates care |
|---|---|---|
| `code_digest` | Per-file SHA-256 of every file classified as **code**: the declared `script_entry`, `*.py`/`*.js`/`*.rs`/… source, test files, `requirements.txt`/`package.json`/`Cargo.toml`, and `runtime.lock`'s `dependencies`/`artifacts`/`layers` (deps change what `unit_test_runner` can import) | `unit_test_runner`, `auditor`, `sealed_evaluator` |
| `contract_digest` | Canonical-JSON SHA-256 of the **semantic** frontmatter fields (see table below), normalized from **either** accepted frontmatter shape | `unit_test_runner`, `auditor`, `sealed_evaluator`, `static_evaluator` |
| `prose_digest` | Per-file SHA-256 of everything else: SKILL.md prose body, `name`/`description`/`agent.name`/`agent.description`, README, examples, comments | `static_evaluator` only |

**Digests are deterministic functions of the artifact bytes.** Content-addressed
artifacts are immutable, so a digest triplet never changes for a given artifact
— it can be recomputed on demand from the stored bytes rather than persisted
(we store it on the artifact's metadata sidecar as a cache, and recompute when
absent). `promotion.record` **copies the artifact's triplet onto the verdict
record at record time**, so a verdict binds to the exact bytes the gate
reviewed — not to a recomputation done later against possibly different
classification rules.

A gate's verdict is reusable across a rebuild **iff the digests of all inputs
that gate reviews are byte-identical**:

- `unit_test_runner` reviews `code_digest` + `contract_digest`
- `auditor` reviews `code_digest` + `contract_digest`
- `sealed_evaluator` reviews `code_digest` + `contract_digest`
- `static_evaluator` reviews `code_digest` + `contract_digest` + `prose_digest`

This is tighter than "the whole artifact didn't change" and looser than
"nothing changed". It is exactly "the bytes this gate actually looked at
didn't change".

> **`static_evaluator` never carries forward.** It reviews all three digests,
> so its verdict is only reusable when nothing changed — i.e. no rebuild at
> all. Carry-forward therefore only ever helps `unit_test_runner`, `auditor`,
> and `sealed_evaluator` (the code-reviewing gates). `static_evaluator` always
> re-runs on any rebuild, which is correct: it is the manifest gate.

### Frontmatter field classification

The contract/prose boundary is a **fixed const table** in the gateway, not an
LLM judgment. Anything not listed defaults to **prose** (fail-safe: an unknown
field does not get to void code-gate verdicts, but also does not silently
bypass contract checks). The table must enumerate the **real** schema (from
`install_contract::install_schema_description` and `autonoetic-types`), not a
subset — a missing contract-relevant field is a silent-bypass hole.

**Canonicalization across both accepted shapes.** The parser accepts two
frontmatter shapes — top-level `autonoetic:` and metadata-wrapped
`metadata.autonoetic:` — that are semantically identical. `contract_digest`
must normalize to a single canonical form **before** hashing, otherwise an
agent that switches shapes (with no semantic change) would spuriously void
every verdict. The same applies to list ordering where order is not semantic
(e.g. `capabilities` entries): hash a canonicalized, order-stable form.

| Field | Class | Rationale |
|---|---|---|
| `capabilities` | contract | defines what the agent may do — every gate reviews against this |
| `remote_access` | contract | auditor + static_evaluator verify code stays within declared hosts |
| `script_entry` | contract | the manifest's declared entrypoint (distinct from the artifact's `entrypoints` list, which is a build-arg concept and part of `code_digest`) |
| `script_input_mode` | contract | the round-1 defect in `session-964ea6d7`; determines how code receives input |
| `io.accepts`, `io.returns`, `io.returns_enforcement`, `io.output_policy` | contract | unit tests assert against this schema; enforcement mode changes the contract |
| `credential_services` | contract | auditor verifies code reads secrets the declared way |
| `middleware` (`pre_process` etc.) | contract | names a script that runs on input — it is executable code by reference |
| `disclosure` | contract | output-filtering rules; auditor verifies no leak path |
| `egress.*` (`output_label`, session policies) | contract | data-flow posture; auditor + static_evaluator verify |
| `validation` (`soft`/`strict`) | contract | changes the output-enforcement contract |
| `execution_mode` | contract | changes sandbox semantics all gates assume |
| `sandbox_network`, `sandbox` backend | contract | auditor + static_evaluator network posture |
| `gateway_url`, `gateway_token` | contract | remote-gateway binding; `gateway_token` is secret-adjacent and must never be silently carried (flag for operator if it changes) |
| `runtime.lock`: `dependencies`, `artifacts`, `layers` | contract | unit_test_runner imports through layers; dep changes alter test execution |
| `llm_preset`, `llm_overrides`, `llm_config` | prose | chooses the reasoning model; gates do not review against the model identity |
| `background`, `metadata.autonoetic.agent.{name,description}`, `name`, `description` | prose | presentation / scheduling metadata |
| SKILL.md body prose, examples, comments | prose | guidance text |
| README, CHANGELOG, doc files | prose | non-executable, non-contract |

A field not in either list defaults to **prose** and is logged at INFO so a
missing classification is visible during the rollout window. A sentinel test
asserts every frontmatter field used by the bundled agents (plus `runtime.lock`
sections) is classified — the same keep-in-sync discipline as the capability
table in `docs/AGENTS.md`.

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
 │     {role: unit_test_runner, carried_from: {artifact_ref: v1,│
 │      role: unit_test_runner},                                │
 │      justification: "code+contract unchanged"},              │
 │     {role: auditor, carried_from: {artifact_ref: v1, ...}},  │
 │     {role: static_evaluator, passed: true, ...}  // fresh    │
 │   ]})                                                        │
 └─────────────────────────────────────────────────────────────┘
                                           │
                                           ▼
 ┌─────────────────────────────────────────────────────────────┐
 │ GATEWAY VERIFIES (per claim)                                 │
 │  • prior artifact (v1) record exists + that role passed      │
 │  • code_digest(v1)==code_digest(v2)        ✓                 │
 │  • contract_digest(v1)==contract_digest(v2) ✓                │
 │  • v2 descends from v1 (lineage check)     ✓                 │
 │  • strictness floor allows carry when only prose differs  ✓  │
 │  → accept; record carried_from provenance                    │
 │  (any failure → reject the claim; that gate must re-run)     │
 └─────────────────────────────────────────────────────────────┘
```

### Failure modes (all fail-closed)

- **Planner omits a required gate entirely** → `federation_escalate` already
  rejects (existing behavior; unchanged).
- **Planner claims carry but a reviewed digest differs** → gateway rejects
  the claim with `"carry_forward_rejected: role={role}, reason={code_digest_mismatch | contract_digest_mismatch | lineage_mismatch}, strictness={level}"`. Planner must re-run that gate.
- **Prior artifact has no passing record for that role** (e.g. `warning`,
  `unable_to_evaluate`, or the role never ran on v1) → not eligible for carry;
  must re-run. Note the **absent-verdict** case: if `unit_test_runner` recorded
  nothing on v1 because the artifact had no tests, there is no pass to carry —
  on a prose-only rebuild the planner re-attests the "no tests" state (or
  re-runs), it does not carry a non-existent verdict.
- **Planner hallucinates a prior artifact ref or role** → record not found →
  rejected.
- **Strictness floor disallows** (e.g. `off`, or `conservative` and contract
  changed) → rejected even if digests would match.

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
  # conservative — carry a code-reviewing gate only when its code_digest and
  #                contract_digest are both byte-identical to the prior artifact
  #                (i.e. only prose changed)
  carry_forward_strictness: off
```

| Level | Allows carry when… | Does NOT allow carry when… | Use when |
|---|---|---|---|
| `off` | (never — full re-federation always) | everything | default; regulated/paranoid environments; until trust is earned |
| `conservative` | a code-reviewing gate's `code_digest` + `contract_digest` are byte-identical (only `prose_digest` differs) | code OR contract changed at all | the `session-964ea6d7` class — SKILL.md prose fixes after a gate flags them |

`off` is the initial default so an upgrade produces **zero** behavior change.
Operators opt in per environment.

The planner is always free to be more conservative — e.g. under `conservative`,
seeing that the prose change was in a security-sensitive section, the planner
may choose to re-run `auditor` anyway. The floor is a maximum permissiveness,
not a mandate.

> **Why only two levels (no `per_role`).** A third level that allows carry
> whenever a gate's own inputs match was considered, but it buys nothing over
> `conservative` with this digest granularity. `static_evaluator` never carries
> (it reviews all three digests), and the code-reviewing gates share
> `code_digest`, so *any* code change re-runs both of them regardless. A
> `per_role` level would only diverge from `conservative` in the code-changed
> case, where nothing can carry anyway. We ship `off` + `conservative` and add
> finer-grained levels only if a finer-grained code digest (e.g. per-file)
> ever exists.

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
   planner only supplies `(prior_artifact_ref, role)`. Mismatch → rejected.
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

## Data model and persistence

**Store: `promotion_registry.json`, not SQLite.** Per-role gate verdicts live
in the file-based `PromotionStore` (`runtime/promotion_store.rs`) — a
`HashMap<String, PromotionRecord>` keyed by `artifact_id`, persisted to
`gateway_dir/promotion_registry.json`. Verdicts do **not** live in the SQLite
`GatewayStore`, so there is **no schema migration** for this design.

Two kinds of state change, both backward-compatible:

1. **On the artifact (digest triplet).** `code_digest` / `contract_digest` /
   `prose_digest` are deterministic functions of the immutable artifact bytes.
   They are computed at `artifact_build` and cached on the artifact's metadata
   sidecar (recomputed from bytes if absent). No migration — the artifact store
   is content-addressed and the digests are derived, never authoritative beyond
   the bytes themselves.

2. **On `PromotionRecord` (per-role verdict provenance).** Add optional fields
   to the `PromotionRecord` struct (`autonoetic-types/src/promotion.rs`):

   ```rust
   // Recorded at promotion.record time — copied from the artifact's triplet so
   // the verdict binds to the bytes the gate reviewed.
   #[serde(default)] pub code_digest: Option<String>,
   #[serde(default)] pub contract_digest: Option<String>,
   #[serde(default)] pub prose_digest: Option<String>,
   // Set when this verdict was carried forward from a prior artifact in the
   // same lineage rather than freshly run.
   #[serde(default)] pub carried_from: Option<CarriedFrom>,
   #[serde(default)] pub carry_verified_at: Option<String>,
   #[serde(default)] pub carry_justification: Option<String>,

   pub struct CarriedFrom {
       pub prior_artifact_ref: String,   // ar.* of the artifact the verdict came from
       pub prior_artifact_id: String,    // art_* for join-ability
       pub role: PromotionRole,          // which gate originally recorded it
   }
   ```

   All new fields are `Option<_>` with `#[serde(default)]`, so existing
   `promotion_registry.json` records deserialize with `None`. `None` digests ⇒
   **unverifiable ⇒ must re-run** — exactly the intended behavior: carry-forward
   only helps from the first post-deployment rebuild onward. We deliberately do
   not backfill digests for records computed before the classification existed.

Because verdicts are per-`artifact_id` records (not rows with their own UUIDs),
provenance references **(prior artifact ref, role)** — the planner names the
artifact a verdict came from and the gate that recorded it, and the gateway
looks up that artifact's `PromotionRecord` to verify the role's pass + digests.

## API / tool surface

| Surface | Change |
|---|---|
| `artifact_build` | Also computes + caches `code_digest`, `contract_digest`, `prose_digest` on the artifact (derived from bytes; gateway-owned, like `content_digest` today) |
| `artifact_inspect` | Returns the three digests + a `lineage: {source_artifact_ref, prior_verdict_coverage}` block when applicable |
| `artifact_diff` (new) | `artifact_diff(from: <ar.*>, to: <ar.*>)` → `{code_changed, contract_changed, prose_changed, changed_fields: [...], changed_files: [...], verdicts_eligible_for_carry: [{role, prior_artifact_ref}]}`. **Complements, not duplicates, `agent_revision_diff`:** `agent_revision_diff` gives a raw file-level diff between two *revisions*; `artifact_diff` gives the *digest-classified* answer the planner reasons over ("did the contract change? did code change?"), plus the carry-eligibility mapping. Gated on `ReadAccess` (same as `artifact_inspect`), per the adding-a-tool pattern: register in `runtime/tools/mod.rs`, implement `NativeTool`, add to the registry builder. |
| `promotion.record` | Copies the artifact's three digests onto the verdict record at write time (gateway-computed from the artifact; agent cannot supply them, mirroring `content_digest` today) |
| `federation_escalate` | Accepts `carried_from: {prior_artifact_ref, role}` + `justification` per role-verdict; verifies against digests + lineage + strictness; records provenance; rejects below-floor claims with a structured error |
| `promotion_query` | Returns `input_coverage: {code_digest, contract_digest, prose_digest}` and `carried_from` per role verdict |
| Config (`config-template.yaml`, `autonoetic-types/src/config.rs`, `docs/reference/config.md`) | New `federation.carry_forward_strictness` enum, default `off` |

`artifact_diff` is the reasoning enabler. Without it the planner would have to
guess what changed by diffing raw `resolve` output, which is exactly the kind
of unreliability this design avoids.

## Planner prompt changes

`agents/lead/planner.default/SKILL.md` Evaluation Federation section gains:

1. After a coder rebuild, before re-spawning gates: call
   `artifact_diff(from=<prior ar.*>, to=<current ar.*>)`.
2. Read the structured diff. Reason out loud: which gates reviewed inputs that
   did not change? Which did?
3. For each **code-reviewing** gate (`unit_test_runner`, `auditor`,
   `sealed_evaluator`) whose reviewed inputs are byte-identical **and** whose
   prior artifact recorded a terminal pass, you MAY propose a carry-forward in
   `federation_escalate` with a one-sentence `justification`. The gateway
   verifies; if it rejects, re-run only that gate. (`static_evaluator` never
   carries — always re-spawn it on any rebuild.) If a gate recorded **nothing**
   on the prior artifact (e.g. no tests → no `unit_test_runner` verdict), there
   is no pass to carry: re-attest the "no tests" state or re-run.
4. You may always choose to re-run a gate you were allowed to carry — do so
   when the change was in a security-adjacent area (capability declarations,
   remote_access, secret handling, egress/disclosure) even if the diff says the
   gate's inputs were stable.
5. Carry-forward never exempts you from the operator escalation or from
   surfacing `carried_from` provenance in your reply.

The existing Step 0 manifest preflight (from #1066) and the failure-routing
rule ("Static evaluator fails → re-preflight") stay; they compose. Step 0
catches the *avoidable* cases; carry-forward absorbs the *legitimate* rebuild
cases that slip through.

## Operator visibility

- `promotion_query` output: each role verdict shows
  `input_coverage: {code_digest, contract_digest, prose_digest}` and either
  `fresh: true` or `carried_from: {prior_artifact_ref, role, verified_at,
  justification}`. A carried verdict is visually distinct.
- The escalation surface (`/approvals`, CLI `gateway approvals show <id>`)
  renders carried verdicts with a "↻ carried from `<prior ar.*>`" marker and
  the justification, so the operator cannot mistake a carried verdict for a
  fresh one.
- A causal event `federation.carry_forward` is emitted on each accepted carry,
  keyed to `(prior_artifact_ref, role, new_artifact_id, strictness)` — same shape
  as `grant_revocation` for audit queries.

## Rollout

Four independently-shippable stages. Each is a standalone PR, each leaves
strictness at `off` (zero behavior change) until the stage that flips it.

| Stage | Content | Behavior change at `off`? |
|---|---|---|
| **1. Compute + store** | Gateway computes the 3 digests at `artifact_build`; `PromotionRecord` gains the optional digest fields (backward-compat `#[serde(default)]`); `promotion.record` copies them at write time. Strictness still `off`. | None (new fields are `None` for old records → unverifiable → re-run) |
| **2. Diff surface** | `artifact_diff` tool + `artifact_inspect` enhancement. Planner prompt learns to *read* the diff but still re-runs everything. | None (planner gets new info, no carry yet) |
| **3. Verify + claim** | `federation_escalate` accepts `carried_from`, verifies against digests + lineage + strictness, records provenance. Default still `off`. | None unless operator opts in |
| **4. UI + audit** | `promotion_query` coverage fields, escalation-surface markers, `federation.carry_forward` causal event. | Surface-only |

Operators flip `carry_forward_strictness: conservative` in their own
`config.yaml` when they've watched Stage 2 logs and are satisfied the field
classification is sound for their agent population.

## Risks and open questions

1. **Layered artifacts (packager)** — _resolved in #1073._ `compute_code_digest`
   folds the sorted `(layer_id, mount_path, ArtifactLayer.digest)` triples into
   the code digest, so a deps-only rebuild (packager swapping a layer with
   identical base files) moves `code_digest` and voids the carry.
   `ArtifactLayer.digest` is a SHA-256 of the layer content — a layer change
   is mechanically detected without reading layer bytes at digest time.
   `mount_path` is folded too (a caller-supplied parameter, so not covered by
   the content digest): mounting identical content at a different path changes
   the sandbox's Python/Node import paths, which is a reviewable-environment
   change, not a no-op. Triplet covers the composed bundle (matches what the
   gates actually import); per-layer digests are not stored separately.
2. **The field-classification table is authority.** Any misclassification is a
   security hole (too strict → wasted re-runs; too loose → silent bypass). The
   table above enumerates the real schema, but must be kept in sync as fields
   are added (same discipline as the capability table in `docs/AGENTS.md`).
   Mitigation: default-to-prose + INFO log for unclassified fields + a sentinel
   test asserting every frontmatter field used by the bundled agents (and every
   `runtime.lock` section) is classified. Also watch the two accepted
   frontmatter shapes — `contract_digest` must canonicalize both, or a
   shape-only change spuriously voids every verdict.
3. **Constitution / enforcement register.** The constitution may cite the
   whole-artifact-digest invariant. Need to check `enforcement_register.rs`
   and `docs/constitution/enforcement-register.md` and update both copies if a
   cited test changes name. The register's `every_parseable_citation_resolves`
   test will fail loudly if we miss one. Separately, `promotion_registry.json`
   is a file store, not SQLite — verify no existing constitutional test assumes
   the verdict store's shape.
4. **First-rebuild-only value.** Carry-forward helps from the 2nd rebuild of a
   lineage onward. The first federation always runs fresh. That's the whole
   point (collapse the 2nd/3rd rounds), but worth setting expectations.
5. **`static_evaluator` never carries** (it reviews all three digests), and the
   code gates share `code_digest`, so any code change re-runs both. This makes
   a third `per_role` strictness level pointless at this digest granularity —
   shipped as `off` + `conservative` only (see Strictness dial). If a finer
   per-file code digest is ever added, revisit.

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
- `docs/internals/approval-cache.md` — the approval dedup + session-grant
  model this design mirrors (planner proposes, gateway dedups + caps); the
  original five-layer write-up is `docs/archived/approval-system.md`
- `docs/reference/capability-grants.md` — same "operator approves envelope, gateway
  materializes + enforces" pattern
- `agents/lead/planner.default/SKILL.md` — Evaluation Federation section
  (Step 0 preflight composes with this design)
- `session-964ea6d7` — the three-round re-federation case this and #1066
  target
