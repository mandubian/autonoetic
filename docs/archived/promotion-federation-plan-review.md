> **ARCHIVED** — Historical design or implementation record. Not current source-of-truth. See [`docs/README.md`](../README.md) for live references.
>

**Status:** Independent review of `docs/archived/promotion-federation-plan.md` after Phase 1.A–E shipped (commits 3a06f00 → 82d0670). Written 2026-05-13. Not authoritative — a second pair of eyes on the plan and its execution to date.

**Reviewed against:** plan as of 5812399; codebase as of 82d0670.

---

## TL;DR

The plan is the right direction. The pivot from "mandatory sealed-network sandbox" to "federation of evaluator roles + operator as conductor" is the correct architectural call — sealed-network is the right *tool* for ~20% of cases, the wrong *gate* for the other 80%. Phase 1.A–E ships the foundation cleanly. The core innovation (jury gate logic + operator-escalation message) is not yet in code; that's the next focused session's work. The shipped sealed-network track is preserved and becomes the foundation of the operator-invokable sealed evaluator.

Main concerns: (1) the on-disk format diverged from §3.3 (flat fields instead of verdict map); (2) `enforce_promotion_gate` is unchanged so federation verdicts are advisory-not-mechanical; (3) no `EscalationMessage` type yet, so federation review has no structured channel; (4) §11.6 test migration strategy is under-specified; (5) §7.3's new R-2.xx rule needs a number and explicit text.

---

## 1. What the plan gets right

**The pivot is correct.** Sealed-network sandbox was getting expensive: fixture loader + HTTP proxy + bubblewrap netns + nftables transparent redirect + HTTPS MITM via gateway CA. That stack delivers a real enforcement property but only for artifacts that genuinely need dynamic execution to evaluate. For most agents (HTTP wrappers, single-endpoint clients, pure-skill bundles), static review + competent reviewer + operator who reads the code suffices. The federation plan keeps the sealed track as an operator-invokable diagnostic and centres evaluation on cheaper, more flexible roles. Less infrastructure for the common case; more honest about what each tool is for.

**Operator-as-conductor is constitutional, not a cop-out.** "All promotions escalate to the operator" admits what the gateway cannot do (judge artifact intent autonomously) while keeping what it can do mechanical (verdict aggregation, distinct-identity enforcement, policy application). §14 Lawful-Executor preserved; the operator is the human-authoritative arbiter; agents inform the decision but do not make it.

**The §11 audit is excellent.** It found real things the original sketch missed: the existing `required_eval_run_id` mechanism (§11.1), the `Evaluation` capability vs `sandbox_network: sealed` conflict (§11.2), the `artifact_exec` asymmetry (§11.3), the workflow_state reuse-guard hard-coding (§11.5), the on-disk format change cost (§11.7), the test migration cost (§11.6). This is the kind of pre-flight that distinguishes a plan-as-document from a plan-as-marketing.

**Channel-agnostic escalation is the right abstraction.** Decoupling `EscalationMessage` from any specific channel means the same verdict bundle reaches TUI / WhatsApp / Discord / Slack via thin adapters. Future-proofs operator workflow without coupling core logic to UI.

**Recording-as-production-observation flips the chicken-and-egg.** Instead of "developer hand-authors fixtures during artifact authoring" (the awkward part of the original sealed-network design), "operator runs the real agent under recording mode, captures redacted real traffic, produces a fixture set." Operationally feasible because operators are the ones who already have credentials and access to real endpoints.

**Sealed-network code is preserved, not discarded.** §11.4 explicitly says "no removal." The shipped Track A work (`sealed_network.rs`, `sealed_network_proxy.rs`, `SandboxNetworkPolicy`) becomes the foundation of the operator-invokable sealed evaluator. No effort lost in the pivot.

---

## 2. Plan vs shipped — audit table

| §11 finding | Plan proposed | Actually shipped | Verdict |
|---|---|---|---|
| §11.1 compose with eval-run | Compose with existing | Composed (no removal); EvalSuiteRecord/EvalRunRecord intact | ✅ |
| §11.2 Evaluation + sealed conflict | Resolve in 1.C | `sealed_evaluator.default` drops `Evaluation` capability | ✅ |
| §11.3 `artifact_exec` preapproved | Wire in 1.D | Done (f232bd1) | ✅ |
| §11.4 sealed code | Keep | Kept; no removal | ✅ |
| §11.5 `workflow_state` hard-coded | Generalise in 1.E | Federation roles tracked (82d0670) | ✅ |
| §11.6 test update (24 tests) | "Update them" | **Not done.** Tests still reference old roles only | ⚠️ |
| §11.7 `PromotionRecord` on-disk | HashMap verdict map | **Flat fields extended.** Diverged from plan | ⚠️ |
| §11.8 `is_promotion_agent` allowlist | Extend | Extended to 5 agents (was 2) | ✅ |
| §3.2 Gate logic (FullJury / OperatorApproved) | New gate modes | **Not done.** Still 4-branch (Full / AuditOnly) | ❌ |
| §4.3 `EscalationMessage` type | New type + emission | **Not done.** Type doesn't exist | ❌ |

**Foundation: shipped.** Phase 1.A–E (types, agent bundles, planner SKILL, workflow_state reuse guards, `artifact_exec` preapproved fix) is in. The new roles can be spawned, they can record verdicts via `promotion_record`, the planner knows about them and aggregates results.

**Core innovation: not yet shipped.** The jury gate and operator escalation — the actual policy change — is deferred. Today the new agents record verdicts the gate ignores. The promotion gate at `enforce_promotion_gate` still uses the 4-branch dispatch from scope 5.11 and only checks `evaluator_pass + auditor_pass`. So a federation-aware planner can spawn `static_evaluator.default` + `unit_test_runner.default` + `auditor.default` and accumulate three verdicts — but the gateway promotes (or rejects) on the legacy binary.

This is workable today because the **planner does the federation aggregation in its own prompt logic** rather than relying on the gateway. The operator effectively reviews the planner's synthesis. But it's a soft enforcement: a compromised planner could skip the federation entirely and the gateway wouldn't know.

---

## 3. Substantive concerns

### 3.1 Flat-field `PromotionRecord` divergence is a trade-off worth noting

The plan §3.3 proposed `HashMap<PromotionRole, RoleVerdict>`. The implementation extended the existing flat fields instead. This is a real trade-off, not a bug:

- **Flat fields**: simpler, no custom Deserialize, but adding role 5 means another `role5_pass / role5_id / role5_findings / role5_timestamp` tuple in the struct.
- **HashMap verdict map**: extensible but requires custom Deserialize, read-time migration, and updates to anything that pattern-matches the struct.

The flat-field approach is fine *as a stopgap*. Adding 2–3 more roles before the unwieldy threshold is acceptable. But this deserves an explicit note in the plan: **deferred refactor with stated trigger** — e.g., "verdict map is required when role count > 5 OR when any role needs more than four metadata fields beyond pass/id/findings/timestamp." Without that, the flat shape will accrete by default until someone notices.

### 3.2 Gate logic untouched is a real coverage gap

The plan §3.2 introduces `FullJury / OperatorApproved / Rejected` modes. None of those exist in code. The gate at `autonoetic-gateway/src/runtime/tools/agent_revision.rs:1934-2129` still mechanically promotes on `evaluator_pass + auditor_pass`. Consequence:

- A planner that does the right thing (spawn federation, accumulate, escalate) works correctly **by convention**.
- A planner that does the wrong thing (skip federation, fake an evaluator pass via a compromised agent) escapes through the unchanged gate.
- The §11.4 acknowledgement that I-10 determinism still holds is technically true ("gateway decision is `threshold_check(verdicts, policy)`") but only if the policy is enforced — which currently it isn't.

To honour the plan's R-2.xx ("operator as final arbiter"), the gate must mechanically *require* operator approval for federation-based promotions, the way it currently mechanically requires `evaluator_pass`. The shape that fits cleanly: extend `enforce_promotion_gate` with a new branch that triggers when ANY federation-role verdict is present, requires operator approval (via the existing `RevisionPromote` approval action shape), and rejects if not approved.

### 3.3 `EscalationMessage` is the missing glue

Without it, the federation aggregation runs through `gate_messages` (the enrichment thread from #172) or ad-hoc `user_ask` payloads. That works for one or two roles but doesn't scale: there's no structured way for an operator's TUI to render "3 federation reports, here are the diffs, click to approve the consolidated verdict." The plan §4.3 has the right shape; it just hasn't been built.

This is the highest-priority next step. Without it:
- Operator review is unstructured (per-channel improvisation).
- Phase 4 (post-promotion background review) has no escalation channel to reuse.
- The plan's #186 sibling (code visibility in approval cards) has no way to render federation verdicts.

### 3.4 §11.6 test migration strategy is the most under-specified piece

"24 tests need updating" without describing *how* leaves the implementation team to invent the strategy. Concrete options:

- **Option A**: Update each existing test to use a federation role. Preserves coverage; high churn; risks losing coverage of the legacy gate path during transition.
- **Option B** *(recommended)*: Mark old tests as legacy; add new federation-path tests alongside. Lower churn; both old and new gate behaviour exercised; eventual cleanup when the old gate path is removed in a future phase.
- **Option C**: Replace old tests entirely. Risky; loses coverage of the existing gate behaviour during transition.

The plan should pick. Option B preserves coverage during the transition window and lets the gate-redesign land without invalidating existing tests.

### 3.5 The plan reintroduces complexity in a different layer

Sealed-network was complex at the *infrastructure* layer (kernel netns, proxy, MITM). Federation is complex at the *orchestration* layer (multiple roles spawning, verdict accumulation, operator review workflow). The new complexity is more "vertical" (SKILL prompts and orchestration logic) and less "horizontal" (gateway primitives).

Whether this is a net simplification depends on whether the operator-review workflow scales. §9.2 (`operator_certified` tier deferred) acknowledges this honestly: the plan defers the "auto-promote based on operator standing approval" path until the manual workflow proves itself.

This is the right call — but the plan should be explicit that **operator throughput is a critical metric** for the federation model. If operators are flooded with 30 federation-review messages per agent install, the model has failed regardless of how clean the gateway-side mechanics are.

### 3.6 Distinct-identity (P-2.17) under federation needs explicit spec

The plan §7.1 says P-2.17 is "extended to cover the federation" but doesn't pin the rule. Options:

- **All federation roles pairwise distinct** — strongest; expensive (need 4+ distinct evaluator agents).
- **Each role's identity differs from the revision proposer** (current rule, applied per-role) — weakest.
- **Each role identity differs from every other role identity AND the proposer** — middle ground.

**Recommendation: middle ground.** It's strong enough to prevent any single compromised agent from passing the gate (no role can stand in for two), without requiring an absurd number of distinct evaluator agents in the ecosystem. The plan should be explicit about which rule and update P-2.17's gateway enforcement accordingly.

### 3.7 §7.3's new constitutional rule needs a number

"R-2.xx" is a placeholder. The plan should propose a concrete number (likely P-2.22 since P-2.21 was the last `R-2.x` allocated) and the exact text. This matters because:

- The rule will be cited in causal events (`enforced_rules` field).
- Anyone reading the gate code needs to look up the rule by ID.
- The amendment process via `constitution_propose_amendment` needs the canonical text.

---

## 4. What I think is missing

### 4.1 Relationship to operator approval inspection (#186)

That work makes code visible in approval cards (Phase 1) and adds escalation-on-complexity for high-risk artifacts (Phase 2). This federation plan spawns federation reports that the operator must review. **The two should reference each other**: the operator reviewing federation reports IS the use case for #186's code visibility.

Concrete recommendation: Phase 1 of #186 (code in approval cards) and the federation `EscalationMessage` work should land in the same window. Otherwise the operator gets a wall of verdicts with no source code to ground them in.

### 4.2 Relationship to ask-agent (#172)

When the operator is reviewing federation reports and wants to ask "why did the static evaluator flag this URL pattern?", that's the clarification child session use case. The federation plan should call this out explicitly: **each federation agent that recorded a verdict is ask-agent-able during operator review**. The clarification child session machinery already exists; the integration is a SKILL-side instruction to the planner ("here's how to surface the ask-agent option").

### 4.3 Phases 2–4 are sketched but light on detail

Phase 2 (production recording), Phase 3 (sealed eval from recordings), Phase 4 (background sentinel review) are the meat of "how does sealed evaluation actually work in this model." They warrant their own document or substantial expansion before they're scheduled. Specifically:

- **Phase 2 redaction policy**: the plan says "credentials, Authorization headers, cookies stripped" — but the redaction rules need their own spec. Are query-string secrets redacted? Bearer tokens in body? What about session cookies in `Set-Cookie` response headers (response-side leak)?
- **Phase 3 fixture-set mounting**: how does the sealed proxy locate the fixture set when the operator asks for sealed eval? Is it referenced by ID at agent_spawn time?
- **Phase 4 anomaly detection criteria**: "behavioral drift" needs a definition. What counts as an anomaly worth escalating?

### 4.4 Failure-mode taxonomy (acknowledged §9.4, not specified)

`role_failure` vs `artifact_failure` is the right axis but the plan defers to "the operator decides." The promotion gate needs concrete handling:

- **Infrastructure failures** (LLM outage, sandbox crash): record `status: "incomplete"`, retry path. Not a verdict against the artifact.
- **Artifact failures** (test fails, audit critical finding): record `status: "fail"`, coder route.
- **Inapplicable** (unit_test_runner runs against artifact with no tests): record `status: "skipped"`, not counted as fail.

The distinction should be encoded in the verdict shape, not left to operator judgement. This affects the `RoleVerdict` schema and the verdict-aggregation logic.

---

## 5. Recommended next-session work

In priority order:

1. **`EscalationMessage` type + emission** (`autonoetic-types/src/escalation.rs` + planner-side construction + JSON-RPC route for operator-side consumption). This is the missing glue that turns federation from "advisory verdicts" into "structured operator review."

2. **Gate logic redesign**: extend `enforce_promotion_gate` with a new `FullJury` branch that requires operator approval (carried on the revision via the existing approval machinery — `RevisionPromote` already has the right shape) when federation roles are present. The gate becomes: legacy path for old artifacts; FullJury path when any federation role recorded a verdict.

3. **Federation-path end-to-end test**: at least one test that exercises planner spawns roles → roles record verdicts → escalation → operator decision → gate enforcement. Without this, regressions in any of the above pieces will pass CI silently.

4. **Test migration strategy decision** (§11.6 work): pick Option B (parallel coverage during transition); add federation tests alongside; deprecate old-path tests when gate-redesign lands.

5. **Distinct-identity rule for federation (P-2.17 extension)**: spell out the middle-ground option in the plan, then enforce in `enforce_promotion_gate`.

The flat-field `PromotionRecord` is acceptable as a stopgap. **Defer the verdict-map refactor** until the role count justifies it; note the deferral and the trigger explicitly in the plan.

The `operator_certified` auto-promotion tier (§9.2) stays deferred. The plan's instinct — let the manual operator workflow prove itself before adding escape hatches — is correct.

---

## 6. Critical files for the next session

| File | Why |
|---|---|
| `autonoetic-types/src/escalation.rs` (new) | `EscalationMessage` type |
| `autonoetic-types/src/promotion.rs` | `PromotionRole` + `PromotionRecord` (already extended; verdict-map deferred) |
| `autonoetic-gateway/src/runtime/tools/agent_revision.rs:1934-2129` | `enforce_promotion_gate` — the four-branch dispatch needs a fifth (FullJury) |
| `autonoetic-gateway/src/runtime/tools/workflow.rs:857-891` | Federation reuse guards (already extended) |
| `agents/lead/planner.default/SKILL.md` | Planner needs to construct `EscalationMessage` payloads |
| `agents/specialists/static_evaluator.default/SKILL.md` | Already created; verify metadata schema for `remote_endpoints_detected` |
| `agents/specialists/unit_test_runner.default/SKILL.md` | Already created; verify "no tests = skip, not fail" path |
| `agents/specialists/sealed_evaluator.default/SKILL.md` | Already created; verify dropped `Evaluation` capability |
| `autonoetic-gateway/tests/constitution_federation_e2e.rs` (new) | Federation-path end-to-end test |

---

## 7. Verification

**Positive path**: spawn agent-factory for a simple HTTP wrapper agent → planner spawns static_evaluator + unit_test_runner + auditor → each records a verdict → planner escalates to operator with `EscalationMessage` carrying all three verdicts → operator approves → gate honours the operator's approval → install proceeds.

**Negative path**: same flow but one federation role records `pass: false` → planner escalates with the failing verdict surfaced → operator routes to coder → no promotion until coder fixes.

**Bypass-attempt path** (defence-in-depth): malicious planner skips federation and submits promotion directly → gate rejects with "no operator approval for federation-required artifact" → install blocked.

**Tests**: `cargo test -p autonoetic-gateway --test constitution_federation_e2e` (new file). Plus the existing test suites for promotion gate hardening, distinct identity, capability delta — all should remain green during the transition window.

---

## 8. Closing thoughts

The plan is the right direction. The execution has built the foundation cleanly. The core innovation (gate redesign + operator escalation) is the next piece and warrants its own focused session.

The biggest single improvement: **land `EscalationMessage` first**, because everything else (gate redesign, operator review, future post-promotion review) hangs off it.

The biggest single risk: **operator throughput**. If the federation model floods operators with review messages per agent install, the model fails regardless of how clean the gateway-side mechanics are. The `operator_certified` deferral acknowledges this honestly; the metric needs to be watched as the model lands.

The biggest single win compared to the sealed-network track: **less infrastructure, more honest division of labour**. Static review is what it is. Dynamic eval is opt-in when the operator wants deeper evidence. The sealed-network sandbox stays available as a tool, not a mandate.
