# Promotion Federation — Design Plan

**Status:** Draft
**Refs:** Issue #185 (sealed-network), Issue #186 (evaluator spawn loop), Constitution §14 (Lawful-Executor invariant), P-3.10 (promotion-gate network denial), I-10 (gateway determinism), P-2.17 (self-approval ban)

---

## 1. Motivation

### 1.1 What's broken

The current promotion gate has a single evaluator role (`evaluator.default`) that must perform **dynamic execution** of the artifact to pass the gate. This creates three problems:

**Problem A — Evaluator can't run without fixtures.** Artifacts containing HTTP calls trigger static analysis, creating an operator approval request. The evaluator blocks, the turn suspends, and the operator must approve network access. But the operator *shouldn't* approve live network access to a critical production service just for evaluation (e.g., Moltbook posting test messages to the real feed). The sealed-network fixture mechanism exists but requires hand-authoring fixture files for every endpoint — a bottleneck that scales linearly with artifact count.

**Problem B — Dynamic execution is overkill for simple artifacts.** An HTTP wrapper service that calls one endpoint and returns a response doesn't need full dynamic evaluation. Code review (does it call the right URL? does it handle credentials correctly?) suffices for most cases. The current gate force-applies `Full` mode to any artifact with `CodeExecution` or `NetworkAccess`, regardless of how simple the artifact actually is.

**Problem C — A single evaluator is a single point of failure.** If the evaluator LLM has a bad day, the promotion gate fails for reasons unrelated to artifact quality. A diverse panel of evaluators with different methodologies produces a more reliable verdict.

### 1.2 What we want instead

A **federation of evaluation roles**, each with a different methodology, producing independent verdicts. The operator reviews all verdicts and decides whether to promote. This is a jury model, not a single-judge model.

The sealed evaluator (dynamic execution) becomes an **operator-invokable diagnostic tool**, not a mandatory gate. It runs only when the operator wants deeper evidence.

---

## 2. Design

### 2.1 Federation of evaluation roles

| Role | Agent ID | Method | Execution? | Network? | When applicable |
|------|----------|--------|------------|----------|-----------------|
| **Auditor** | `auditor.default` (exists) | Security, capabilities, declaration consistency | No | No | All agents |
| **Static evaluator** | `static_evaluator.default` (new) | Code correctness, behavioral contract, credential flow, URL pattern analysis | No | No | Artifact-backed agents |
| **Unit test runner** | `unit_test_runner.default` (new) | Runs artifact's built-in test suite in no-network sandbox | Yes | No | Artifact-backed agents with tests |
| **Sealed evaluator** | `sealed_evaluator.default` (rename from `evaluator.default`) | Runs artifact in sealed-network sandbox with fixtures | Yes | Sealed proxy only | Operator-invokable |
| **Operator** | Human (or habilitated agent) | Reviews all reports, makes final call | N/A | N/A | Always (for now) |

Each evaluator role calls `promotion_record(role, pass, findings)` independently. The promotion store accumulates all verdicts for a given artifact. The gate checks the **collection**, not a single evaluator.

### 2.2 Applicability matrix

| Artifact type | Auditor | Static eval | Unit tests | Sealed eval |
|---|---|---|---|---|
| Pure-skill (SKILL.md only, no code) | Yes | Yes (reviews skill consistency) | N/A | N/A |
| Artifact-backed, no HTTP | Yes | Yes | Yes (if tests exist) | N/A |
| Artifact-backed, has HTTP calls | Yes | Yes (credential flow + URL analysis) | Yes (if tests exist) | Operator-gated |

The planner knows the artifact type from its shape and spawns the applicable roles. The gateway doesn't know — it just processes `promotion_record` calls as they arrive.

### 2.3 The operator as conductor

```
┌──────────────────────────────────────────────────────────┐
│                    Planner / Orchestrator                 │
│                                                          │
│  For artifact-backed agents:                              │
│    Spawn auditor ───────────────────► promotion_record   │
│    Spawn static_evaluator ──────────► promotion_record   │
│    Spawn unit_test_runner ──────────► promotion_record   │
│                                                          │
│  For pure-skill agents:                                   │
│    Spawn auditor ───────────────────► promotion_record   │
│    Spawn static_evaluator ──────────► promotion_record   │
│                                                          │
│  Bundle all reports ──► escalate to operator              │
│                                                          │
│  Operator reviews:                                        │
│    ├── Promote  ("all clear")                             │
│    ├── Sealed-eval ("I want deeper evidence, run sealed") │
│    ├── Fix      ("findings need addressing, route to      │
│    │              coder")                                 │
│    └── Reject   ("this artifact cannot be promoted")      │
└──────────────────────────────────────────────────────────┘
```

For now, **all promotions escalate to the operator**. Automatic promotion (when all mandatory roles pass without operator review) is deferred — the operator workflow needs to be exercised and proven before we automate it.

### 2.4 Channel-agnostic operator review

The gateway emits a **structured `EscalationMessage`**, never channel-specific formatting. Each channel adapter (TUI, WhatsApp, Discord, Slack) converts it to its native format.

```rust
struct EscalationMessage {
    id: Uuid,
    escalation_type: EscalationType,
    artifact_ref: String,
    artifact_digest: String,
    reports: Vec<PromotionReport>,
    code_reference: Option<ArtifactRef>,
    suggested_actions: Vec<SuggestedAction>,
    urgency: Urgency,
}

enum EscalationType {
    PromotionReview,         // new artifact needs operator decision
    SealedEvalInquiry,       // remote endpoints detected, want sealed eval?
    PostPromotionAnomaly,    // background sentinel found something
    RecordingComplete,       // recorded fixture set is ready
}
```

The operator's decision comes back through the existing approval/decision API (`record_decision`), same mechanism regardless of channel.

### 2.5 Recording mode for fixture bootstrapping

Instead of hand-authoring fixtures, the operator can run the **real agent** with network recording enabled. The proxy captures real traffic, redacts secrets, and stores it as a content-addressed fixture set.

```
Operator: autonoetic agent run moltbook --record-network --duration 5m
         ↓
Gateway starts sandbox with sandbox_network: recording
         ↓
Agent makes real HTTP calls (post_to_feed, get_posts, etc.)
         ↓
Proxy intercepts, redacts secrets, records → fixture set (SHA-256 addressed)
         ↓
Fixture set stored: ar.recording-<agent_id>-<timestamp>-<digest>
         ↓
--- later ---
Operator: "run sealed eval of moltbook v2 against fixture set ar.recording-..."
         ↓
Sealed evaluator replays recorded traffic → deterministic verdict
```

Key properties:
- Recording requires explicit operator authorization (no silent recording)
- Redaction is mandatory — credentials, Authorization headers, cookies stripped
- Fixtures are content-addressed and immutable
- Recording is time-bounded (operator sets a duration or request count)

This is scope 5.3 from the original sealed-network plan, repurposed as a production observation tool rather than a fixture-authoring framework.

### 2.6 Post-promotion background review (future)

After an agent is promoted and live, a background sentinel periodically:
- Watches live traces for anomalies
- Detects behavioral drift (new URLs, unexpected error patterns)
- Re-evaluates against newer recordings
- Escalates suspicious findings to the operator using the same `EscalationMessage` schema

Not in Phase 1, but designed into the escalation schema from the start.

---

## 3. Promotion Gate Redesign

### 3.1 Current gate

The current gate is four mutually-exclusive branches derived mechanically from capabilities:

| Branch | Condition | Gate mode | Requirements |
|---|---|---|---|
| 1 | CodeExecution / AgentSpawn + artifact | Full | evaluator PASS + auditor PASS, distinct identities |
| 2 | NetworkAccess + artifact (no CodeExecution/AgentSpawn) | Full | Same as Branch 1 |
| 3 | Artifact + non-empty capabilities (no high-risk) | AuditOnly | auditor PASS, distinct from proposer |
| 4 | No artifact or zero capabilities | Direct promote | None |

### 3.2 New gate

The gate mode is now driven by a declaration that the operator (or eventually automated policy) sets, rather than derived solely from capabilities.

```rust
enum PromotionGateMode {
    /// All declared mandatory evaluator roles must pass, auditor must pass,
    /// distinct identities. Operator has not yet reviewed.
    FullJury,
    /// Operator has reviewed all reports and explicitly promoted.
    OperatorApproved,
    /// Rejected by operator or gate failure.
    Rejected,
}
```

The gate outcome is one of:
- **All mandatory roles pass + operator approves → promote**
- **Any mandatory role fails → escalate to operator with reason**
- **Operator overrides (e.g., small findings but operator accepts) → promote**

The gateway stays dumb: it reads declared mandatory roles, checks that all passed, checks operator approval, decides promote/reject/escalate.

### 3.3 Promotion record accumulation

The `PromotionRole` enum is extended:

```rust
enum PromotionRole {
    Auditor,
    StaticEvaluator,
    UnitTestRunner,
    SealedEvaluator,
}
```

Each role records independently. The `PromotionRecord` structure accumulates verdicts per role:

```rust
struct PromotionRecord {
    artifact_id: String,
    content_digest: Option<String>,
    verdicts: HashMap<PromotionRole, RoleVerdict>,
}

struct RoleVerdict {
    agent_id: String,
    pass: bool,
    findings: Vec<Finding>,
    timestamp: String,
    summary: Option<String>,
    metadata: HashMap<String, serde_json::Value>,  // role-specific extras
}
```

Static evaluator adds `remote_endpoints_detected: [String]` to metadata. Unit test runner adds `tests_total`, `tests_passed`, `tests_failed`. Sealed evaluator adds `fixture_set_ref`.

The `enforce_promotion_gate` function checks all mandatory roles for `pass == true`, validates distinct-identity requirements, and returns any failing role's evidence to the operator.

---

## 4. Data Model Changes

### 4.1 `PromotionRole` enum (`autonoetic-types/src/promotion.rs`)

```rust
pub enum PromotionRole {
    Auditor,
    StaticEvaluator,
    UnitTestRunner,
    SealedEvaluator,
}
```

### 4.2 `PromotionRecord` struct (`autonoetic-types/src/promotion.rs`)

Replace flat role-specific fields (`evaluator_pass`, `auditor_pass`, etc.) with a verdict map. Keep backward-compatible deserialization for existing records.

### 4.3 `EscalationMessage` struct (new, `autonoetic-types/src/escalation.rs`)

Channel-agnostic escalation payload for operator review.

### 4.4 `RecordingSession` + `FixtureSet` (new types for Phase 2)

Track recording sessions and their resulting fixture sets.

---

## 5. New Agent Bundles

### 5.1 `static_evaluator.default`

```
agents/specialists/static_evaluator.default/
├── SKILL.md
└── runtime.lock
```

**Capabilities:** ReadAccess (self.*, skills/*), WriteAccess (self.*, skills/*), SandboxFunctions (content., knowledge., artifact., promotion.)
**Network:** None
**Role:** Reads artifact source code. Analyzes correctness, behavioral contract, credential flow, URL patterns. Records `StaticEvaluator` verdict with findings and `remote_endpoints_detected` metadata.
**Tool calls:** `artifact_inspect`, `content_read`, `promotion_record`

### 5.2 `unit_test_runner.default`

```
agents/specialists/unit_test_runner.default/
├── SKILL.md
└── runtime.lock
```

**Capabilities:** ReadAccess (self.*, skills/*), WriteAccess (self.*, skills/*), CodeExecution, SandboxFunctions (content., knowledge., artifact., promotion., sandbox.)
**Sandbox network:** Off (`--unshare-net`)
**Role:** Discovers and runs artifact's test suite in a no-network sandbox. Records `UnitTestRunner` verdict with `pass=true` and test stats if tests pass, `pass=false` if any test fails, or skips (no record) if no tests are found.
**Key rule:** If no tests exist, returns early without recording a verdict — this is not a failure, just inapplicable.

### 5.3 `sealed_evaluator.default` (renamed from `evaluator.default`)

```
agents/specialists/sealed_evaluator.default/
├── SKILL.md
├── runtime.lock
```

Same as current evaluator but:
- Explicitly a diagnostic tool, not a mandatory gate
- Spawned only when operator requests sealed evaluation
- Accepts `fixture_set_ref` in metadata for replay
- Records `SealedEvaluator` verdict

---

## 6. Tool Changes

### 6.1 `promotion_record` (existing tool, updated)

- Accepts new `PromotionRole` variants: `StaticEvaluator`, `UnitTestRunner`, `SealedEvaluator`
- `is_promotion_agent()` check extended to include `static_evaluator.default`, `unit_test_runner.default`, `sealed_evaluator.default`
- Severity gating (error/critical with pass=true, warnings without evidence) applies to all roles
- Role-specific metadata fields validated (e.g., unit test runner must include test stats)

### 6.2 `promotion_query` (existing tool, updated)

- Returns all role verdicts for an artifact, not just evaluator/auditor

### 6.3 `artifact_exec` preapproved bypass (bugfix)

- Wire `remote_access.approval_mode: preapproved` check into `artifact_exec.rs` (currently missing)
- Required for sealed evaluator to run without operator approving network routes

---

## 7. Constitution Implications

### 7.1 Principles unchanged

| Principle | Effect |
|---|---|
| §14 (Lawful-Executor invariant) | Gateway doesn't know about evaluation roles. It reads verdicts and applies declared policy mechanically. |
| P-3.10 (network denial) | Sealed evaluator still runs without live network. Static evaluator/test runner have no network. P-3.10 vacuously satisfied for non-execution roles. |
| I-10 (determinism) | Gateway decision is `threshold_check(verdicts, policy)` — pure function of recorded state. Verdicts come from LLMs (non-deterministic), but the gateway surface is deterministic. |
| P-2.17 (self-approval) | Each evaluator role must have a distinct identity from the revision proposer and from each other. Extended to cover the federation. |

### 7.2 P-3.10 clarification

Current: "Promotion-gate execution is denied network access."
Proposed: "Promotion-gate execution, when it occurs, is denied live network access."

The clarification acknowledges that execution is optional — `StaticEvaluator` and `UnitTestRunner` either don't execute or execute without network. The principle holds for `SealedEvaluator`.

### 7.3 New rule: operator as promotion arbiter

> **R-2.xx** — The operator is the final arbiter of the promotion gate. All promotion verdicts are escalated to the operator for review. The operator may promote, reject, or request additional evaluation (e.g., sealed execution). The gateway mechanically enforces the operator's decision.

---

## 8. Implementation Phases

### Phase 1 — Federation + operator conductor

| Task | Scope |
|---|---|
| Extend `PromotionRole` enum: add `StaticEvaluator`, `UnitTestRunner`, `SealedEvaluator` | `autonoetic-types/src/promotion.rs` |
| Replace `PromotionRecord` flat role fields with verdict map (backward-compat deserialization) | `autonoetic-types/src/promotion.rs` |
| Update `promotion_store.record_promotion()` for verdict map | `autonoetic-gateway/src/runtime/promotion_store.rs` |
| Update `promotion_record` tool: new roles, extended access check, role-specific metadata validation | `autonoetic-gateway/src/runtime/tools/promotion.rs` |
| Update `promotion_query` tool: return all verdicts | `autonoetic-gateway/src/runtime/tools/promotion.rs` |
| Create `EscalationMessage` type + `EscalationType` enum | `autonoetic-types/src/escalation.rs` |
| Redesign promotion gate: check all mandatory roles pass; escalate to operator | `autonoetic-gateway/src/runtime/tools/agent_revision.rs` |
| Create `static_evaluator.default` agent bundle | `agents/specialists/static_evaluator.default/` |
| Create `unit_test_runner.default` agent bundle | `agents/specialists/unit_test_runner.default/` |
| Rename `evaluator.default` → `sealed_evaluator.default` (or keep both, deprecate old) | `agents/specialists/` |
| Update planner SKILL: spawn federation roles per artifact type, escalate to operator | `agents/lead/planner.default/SKILL.md` |
| Fix `artifact_exec` preapproved bypass | `autonoetic-gateway/src/runtime/tools/artifact_exec.rs` |
| Tests: full federation cycle, operator escalation, role-specific validation | `autonoetic-gateway/tests/` |

### Phase 2 — Production recording

| Task | Scope |
|---|---|
| `--record-network` flag on agent run CLI | `autonoetic/src/cli/` |
| Recording proxy captures real traffic to fixture files | `autonoetic-gateway/src/runtime/sealed_network_proxy.rs` |
| Redaction layer strips credentials before storage | `autonoetic-gateway/src/runtime/sealed_network.rs` |
| `RecordingSession` + `FixtureSet` types | `autonoetic-types/` |
| Fixture set storage as content-addressed artifacts | `autonoetic-gateway/src/scheduler/gateway_store/` |
| CLI: list/inspect/delete fixture sets | `autonoetic/src/cli/` |

### Phase 3 — Sealed evaluator from recordings

| Task | Scope |
|---|---|
| Sealed evaluator accepts `fixture_set_ref` in spawn metadata | `autonoetic-gateway/src/runtime/tools/agent_spawn.rs` |
| Gateway mounts fixture set into sealed sandbox | `autonoetic-gateway/src/runtime/sealed_network.rs` |
| Operator CLI: `autonoetic eval sealed --artifact-ref X --fixture-set Y` | `autonoetic/src/cli/` |

### Phase 4 — Post-promotion background review (future)

| Task | Scope |
|---|---|
| Sentinel watches live agent traces | `autonoetic-gateway/src/sentinel/` |
| Behavioral drift detection | `autonoetic-gateway/src/sentinel/checks/` |
| Periodic re-evaluation against new recordings | `autonoetic-gateway/src/scheduler/` |
| Escalate anomalies via `EscalationMessage` | `autonoetic-gateway/src/scheduler/escalation.rs` |

---

## 9. Open Questions

1. **`unit_test_runner` — separate agent or merged into `static_evaluator`?** Separate is recommended (cleaner role isolation, independent reports) but costs one extra spawn + LLM session.

2. **`operator_certified` tier — Phase 1 or Phase 4?** Deferred. Let the operator-in-the-loop workflow prove itself before adding an "I trust this, skip all gates" escape hatch.

3. **Backward compatibility for existing `PromotionRecord` schema?** The flat fields (`evaluator_pass`, `auditor_pass`, etc.) are stored on disk. Deserialization must handle both old and new formats. Migration is read-time, not a data migration script.

4. **What if an evaluator role fails for reasons unrelated to the artifact?** (e.g., LLM outage, sandbox crash). The promotion gate should distinguish `role_failure` (gate can't run) from `artifact_failure` (artifact is bad). Suggested approach: failed tool calls with infrastructure error codes produce no record, and the gate reports "incomplete" rather than "fail." The operator decides whether to retry or reject.

5. **Should `sealed_evaluator.default` replace `evaluator.default` entirely?** The old `evaluator.default` tried to run artifacts against live network and blocked on approval. That doesn't work and the sealed-network wiring gap confirms it. Rename to `sealed_evaluator.default` to make its purpose explicit. The old `evaluator.default` SKILL can be archived.

---

## 10. References

- `docs/design/sealed-network-evaluation-plan.md` — Original sealed-network design (scopes 5.1-5.11)
- `docs/ARCHITECTURE.md` — Security model, separation of powers
- `docs/approval-system.md` — Operator approval lifecycle
- `autonoetic-gateway/src/runtime/tools/agent_revision.rs` — Current promotion gate (lines 1920-2139)
- `autonoetic-gateway/src/runtime/promotion_store.rs` — Current promotion record storage
- `autonoetic-types/src/promotion.rs` — Current `PromotionRole` and `PromotionRecord`
- `agents/specialists/evaluator.default/SKILL.md` — Current evaluator (to become sealed_evaluator)

---

## 11. Validation & Code Audit

Codebase audit performed 2026-05-13. Findings below affect the plan's scope.

### 11.1 Existing eval-run mechanism already exists

There is **already a second promotion gate path** that the plan did not account for. `agent_revision_promote` accepts an optional `required_eval_run_id` parameter. When provided, the gate checks that the eval run exists, has status `Passed`, and targets the correct revision (`agent_revision.rs:2130-2150`).

This mechanism is used by the **protected-agent gate** (`agent_revision.rs:2152-2181`): critical agents (e.g., `agent-factory.default`) cannot be promoted without eval evidence. It uses a separate type system: `EvalSuiteRecord`, `EvalRunRecord`, `EvalCaseResultRecord` in `autonoetic-types/src/evaluation.rs`.

**Plan impact:** The federation plan's `OperatorApproved` gate mode should compose with the existing eval-run mechanism, not replace it. The `required_eval_run_id` path is already the right shape for a federated evaluation result. Consider whether the federation's verdict map should be stored as eval runs rather than as promotion records.

### 11.2 `Evaluation` capability conflicts with `sandbox_network: sealed`

In `sandbox_exec`, agents with the `Evaluation` capability get `force_network_off = true` (`sandbox.rs:2207-2216`), which blocks ALL network at the kernel level. If the same agent also declares `sandbox_network: sealed`, the sealed proxy starts but is unreachable because `force_network_off` takes precedence in `append_bwrap_isolation_flags` (`sandbox.rs:1077-1084`).

This is a latent bug: the current evaluator has both `Evaluation` capability and `sandbox_network: sealed` — the proxy spawns uselessly.

**Plan impact:** Phase 1.C (rename evaluator) should also resolve this conflict. Options:
- Remove `Evaluation` capability from sealed_evaluator (sealed-network proxy IS the network control)
- Or make `sandbox_network: sealed` override `force_network_off` when both are present

### 11.3 `artifact_exec` lacks `Evaluation` capability guard

`sandbox_exec` forces network off for `Evaluation` capability agents. `artifact_exec` does NOT check for `Evaluation` capability at all. This is an asymmetry — an evaluator using `artifact_exec` can access the network freely.

**Plan impact:** The plan's Phase 1.D (preapproved bypass) should also decide: should `artifact_exec` enforce `Evaluation` capability's network-off? Or is the evaluator's primary tool exempt? This needs an explicit decision.

### 11.4 Sealed-network code: keep, don't remove

All sealed-network code (`sealed_network.rs`, `sealed_network_proxy.rs`, `SandboxNetworkPolicy`) is **functional and well-tested**. Nothing should be removed.

Minor fixes needed:
- **Stale doc comment** on `SandboxNetworkPolicy` in `agent.rs:175-180` says "Sealed and Recording are dormant until RFC scopes 5.2/5.3 ship." Scope 5.2 has shipped. Update the comment.
- **Recording stub** in `decide_egress()` is a documented stub, not dead code. It becomes Phase 2 of this plan.

### 11.5 `workflow_state` reuse guards are hard-coded

`workflow.rs:857-863` builds `reuse_guards` by checking `latest_artifact_by_role` for keys `"evaluator"` and `"auditor"`. The role is extracted by splitting the agent_id on `.` (`"evaluator.default"` → `"evaluator"`).

Under federation, new agents (`static_evaluator.default`, `unit_test_runner.default`) would add new keys. The reuse guard logic needs updating to track all federation roles, not just evaluator/auditor.

**Plan impact:** Phase 1.E (planner SKILL update) must also update `workflow_state` to track federation roles. The `has_evaluator_result` guard generalizes to `has_static_evaluator_result`, `has_unit_test_runner_result`, etc.

### 11.6 Test inventory — 24 tests need updating

**High impact (exercise gate logic + hard-code identities):**

| Test file | Tests affected |
|---|---|
| `promotion_gate_hardening_integration.rs` | 8 tests — all exercise four-branch gate, hard-code `evaluator.default`/`auditor.default`, use `PromotionRole::Evaluator/Auditor` |
| `constitution_promotion_gate_audit_only.rs` | 5 tests — AuditOnly branch, hard-code `auditor.default` |
| `promotion_record_e2e.rs` | 1 test — full pass flow, uses `PromotionRole` variants |
| `promotion_record_evaluator_fail.rs` | 3 tests — evaluator/auditor failure paths |
| `constitution_promotion_distinct_identity.rs` | 2 tests — P-2.17 distinct-identity |
| `promotion_required_record_integration.rs` | 2 tests — `require_promotion_record` spawn option |

**Low impact (hard-code evaluator.default in helpers only):**

| Test file | Tests affected |
|---|---|
| `promotion_record_findings_validation.rs` | 2 tests — findings validation, evaluator in helper only |
| `phase2_promotion_stability_integration.rs` | 2 tests — alias pinning, gate exercised indirectly |

**Unaffected:**

| Test file | Reason |
|---|---|
| `constitution_promotion_capability_delta.rs` | Tests P-2.16 capability delta, no evaluator/auditor |
| `protected_agents_promotion_gate_integration.rs` | Tests eval-run gate, not promotion roles |
| `constitution_promotion_no_network.rs` | Tests sandbox isolation, not promotion roles |
| `promotion_record_reject.rs` | Tests tool removal |

### 11.7 `PromotionRecord` on-disk format is flat, not extensible

Current `PromotionRecord` stores flat fields: `evaluator_pass`, `evaluator_id`, `evaluator_findings`, `evaluator_timestamp`, `auditor_pass`, `auditor_id`, `auditor_findings`, `auditor_timestamp`. The file is `promotion_registry.json` (JSON, one HashMap per artifact).

The plan proposes replacing this with a `HashMap<PromotionRole, RoleVerdict>` verdict map. This is a **breaking change to the on-disk format**. Backward-compatible deserialization is needed because existing sessions may have records on disk from the old format.

**Plan impact:** The plan §4.2 already notes "Keep backward-compatible deserialization for existing records." This is correct but underspecified. The migration strategy:
1. New `PromotionRecord` uses `verdicts: HashMap<String, RoleVerdict>` (JSON key is role name string)
2. Custom `Deserialize` reads both old flat format and new verdict map
3. On first write after upgrade, the old format is normalized to the new format
4. No separate migration script needed

### 11.8 `is_promotion_agent()` is a hard-coded allowlist

`promotion.rs:19-24` hard-codes `"evaluator.default" | "auditor.default"`. Under federation, this becomes `"static_evaluator.default" | "unit_test_runner.default" | "sealed_evaluator.default" | "auditor.default"`.

**Plan impact:** Consider making this dynamic — agents that declare a specific capability (e.g., `PromotionRecording { roles: [...] }`) can call `promotion_record`. This is more extensible than maintaining a hard-coded allowlist. But this can be deferred to a later iteration; for Phase 1, extending the allowlist is sufficient.

### 11.9 Revised plan summary

| Original plan item | Audit finding | Action |
|---|---|---|
| §3.3 Verdict map | On-disk format migration needed | Add migration spec to Phase 1.A |
| §5.3 Rename evaluator | `Evaluation` + `sealed` conflict | Resolve conflict in Phase 1.C |
| §6.3 Preapproved bypass | `artifact_exec` also lacks `Evaluation` guard | Expand Phase 1.D scope |
| New: §11.1 | Existing eval-run mechanism | Compose with, don't replace |
| New: §11.5 | `workflow_state` hard-coded roles | Add to Phase 1.E scope |
| New: §11.6 | 24 tests need updating | Add test update task to Phase 1.A |
| Sealed-network code | Functional, keep as-is | No removal needed |
