# Security Sentinel: System-Tier Security Agent

Status: **implemented (Phases 0–7 merged)**. Phase mapping below.

| Phase | Scope | PR |
|---|---|---|
| 0 + 1 | Foundation, types, append-only `security_findings` table, deterministic core (credentials, capability accretion, approval bypass, sandbox escape) | #140 |
| 2 | LLM-judgment heuristics (prompt-injection surface, session-cluster anomalies) | #141 |
| 3 | Frozen baseline, dual-sweep orchestrator, disagreement recording | #142 |
| 4 | Supply-chain auditing (layer scope violations, provenance gaps) | #144 |
| 5 | Cron scheduling + pre-promotion gate integration | #145 |
| 6 | Triage CLI (`autonoetic security triage`), calibration feedback loop | #147 |
| 7 | Red-team agent + adversarial co-evolution; `attack_pattern_propose` | #149 |

Configuration knobs are documented in [`docs/config-reference.md`](config-reference.md) under "Security Sentinel".

> **Frozen-baseline contract** (issue #153, fixed): the dual-sweep dispatches the baseline pass to a hand-frozen `autonoetic-gateway/src/sentinel/baseline/` module — independent copies of the Phase-1 deterministic checks. A regression introduced into the canonical `sentinel/checks/` does **not** propagate to the baseline, so it surfaces as a `baseline_only` disagreement at the next sweep. What dual-sweep provides today:
>
> - **`baseline_agreed` annotation on Phase-1 findings** when both the canonical checks and the frozen baseline flag the same evidence anchor.
> - **Phase-1 disagreement records** in `security_sentinel_disagreements` (`baseline_only` when only the baseline flagged the anchor — i.e. a regression in canonical checks; `current_only` when only the canonical flagged it — i.e. a baseline that has aged out).
> - **Independent Phase-2 findings** from the current pass. The baseline never runs Phase-2; it is `phase1_only` by contract.
>
> Editing the baseline requires a deliberate `[baseline-update]` commit prefix and lands as a separate PR from any concurrent `sentinel/checks/` change. PRs that touch both for the same pattern defeat the purpose. This is enforced by the `sentinel-baseline-guard` CI workflow (`.github/workflows/sentinel-baseline-guard.yml`) and can be checked locally with `cargo xtask sentinel-baseline-guard`.

## Summary

A periodic, read-only reasoning agent that audits autonoetic's agents, artifacts, and gateway state for security issues. It proposes structured findings over the causal chain; the gateway decides what to do with them. It is **not** a runtime monitor, pen-tester, or WAF — runtime enforcement remains the gateway's job, as everywhere else.

This document explains what the sentinel is, where it fits, the hard problems that are specific to an agent auditing agents, and the phased build plan.

---

## Motivation

autonoetic's design already gives security work an unusually strong foundation:

- The **causal chain** records every tool call, every approval, every capability use, every promotion. It is hash-chained and immutable. An auditor does not need to install probes; its inputs already exist.
- **Content-addressed revisions, layers, and artifacts** mean any finding can be anchored to specific digests. "This flaw exists in layer `sha256:abc`" is a durable claim.
- The **separation-of-powers model** (see `docs/separation-of-powers.md`) applies one level up: the gateway polices agents, the sentinel polices the system-as-a-whole. The primitives are the same — propose, don't enforce.

What is missing is committing that security auditing is a **distinct tier**, not a feature of the auditor or evolution-steward roles. The auditor's scope is promotion evidence for a specific revision. Evolution steward's scope is behavioral health of the agent roster. Neither one sweeps the system for credential leaks, capability accretion, prompt-injection surface, or supply-chain scope violations.

Those concerns compound silently. A single promotion that accretes a capability is a finding; a year of promotions that each accrete one capability is a different agent.

---

## Design principles

These are non-negotiable. Every other decision in the doc flows from them.

1. **Separation of powers holds.** The sentinel produces findings; the gateway gates. The sentinel has no authority to revise, promote, disable, or block anything on its own. It writes to the causal chain; it does not reach into state.

2. **Deterministic checks first, LLM judgment second.** A regex that matches a credential pattern produces `critical` findings. An LLM that "feels suspicious about this prompt" produces `warning` findings, and only `critical` if an ensemble agrees. The deterministic core is always on; the judgment layer complements it.

3. **Evidence anchors over prose.** Every finding cites causal-event IDs, content digests, revision IDs. A second reviewer or the operator must be able to walk the anchors and see the same signal. Findings without anchors are not findings; they are opinions.

4. **Read-only by profile.** The sentinel has no `NetworkAccess`, no `CodeExecution`, no write access to any privileged surface. Its capabilities are scoped to reads of causal events, artifacts, manifests, and structured state. External lookups (CVE feeds, registry reputation) are a strictly-scoped follow-on, cached, operator-approved.

5. **Frozen anchor.** A minimal baseline sentinel, shipped with the gateway, is never revised without explicit operator action. Every sweep runs the current sentinel *and* the baseline. Disagreements surface. Without this, a regressed sentinel silently hides its own regression (the same structural risk flagged for agent-factory in #21).

---

## Where it sits

New system tier:

```
agents/
  lead/                 — planner
  specialists/          — coder, researcher, architect, debugger, evaluator, auditor, ...
  evolution/            — memory_curator, evolution_steward, specialized_builder
  system/
    security_sentinel.default/          — the reasoning sentinel
    security_sentinel_baseline.default/ — the frozen anchor
```

Critical: the sentinel is **not** under `evolution/`. It must be one of the hardest things in the system to silently evolve. Placing it in a new `system/` tier signals that to operators and to the tooling that audits evolution activity.

---

## Inputs

Read-only capabilities over:

- **Causal events** (`causal_events` table) — the primary substrate. Every turn, tool call, approval, rejection.
- **Artifact store & layer manifests** — to audit supply-chain scope. Depends on #20 (`layer.approval_scope` metadata) to be meaningful.
- **SKILL.md bodies and `io.accepts` / `io.returns`** — to audit contract surfaces and prompt-injection exposure. Depends on #19 (gateway as single source of truth for contracts).
- **Knowledge store with revision tags** — to audit curator decisions and look for planted or poisoned entries. Depends on #24 (revision-bound knowledge).
- **`promotion_history`, `session_approval_grants`, `agent_aliases`** — to audit authorization drift over time. Composes with #29 (evolution observability).

What is intentionally **not** in the input list: network, subprocess execution, external API calls. A reasoner with tools that can run arbitrary network code is indistinguishable from an attacker. External probes live behind a narrow, operator-approved capability with specific hosts and cached results, and are invoked only from scheduled follow-on sweeps — never from the core reasoning loop.

---

## Outputs

Every output is a `SecurityFinding` record:

```json
{
  "finding_id": "sec_<uuid>",
  "severity": "critical" | "warning" | "info",
  "confidence": 0.0,
  "finding_type": "credential_leak" | "capability_accretion" |
                  "sandbox_escape_attempt" | "approval_bypass" |
                  "prompt_injection_surface" | "supply_chain_scope_violation" |
                  "behavioral_anomaly" | "curator_bias" | "...",
  "affected": {
    "agent_alias": "...",
    "revision_id": "...",
    "session_id": "...",
    "artifact_id": "...",
    "layer_digest": "..."
  },
  "evidence_anchors": [
    { "type": "causal_event", "id": "..." },
    { "type": "skill_md_digest", "value": "sha256:..." },
    { "type": "layer_digest", "value": "sha256:..." }
  ],
  "reproducibility": "deterministic" | "llm_judgment" | "statistical",
  "proposed_remediation": "human-readable suggestion",
  "sentinel_revision_id": "...",
  "baseline_agreed": true | false,
  "ensemble_agreed": true | false | null
}
```

Every field is load-bearing:

- `severity` + `confidence` let the operator filter.
- `finding_type` is an enum so the sentinel's behavior is auditable across revisions (you can ask "is the sentinel's distribution of finding_types drifting?").
- `evidence_anchors` make the finding re-verifiable, structurally, not by prose.
- `reproducibility` tells the operator how much to trust this class of finding: a regex-matched credential leak is deterministic; a "prompt feels suspicious" judgment is LLM reasoning and needs a second opinion.
- `baseline_agreed` / `ensemble_agreed` are the self-audit anchors.

Findings persist in a `security_findings` SQLite table with an append-only contract, and are also emitted to the causal chain so they are part of the immutable history.

---

## How it runs

Three scheduling modes, all expressible with the existing scheduler:

1. **Periodic sweeps** — daily full audit; hourly incremental over the new causal-event range. Default cadence tunable; findings batched.

2. **Pre-promotion gate** — on `agent_revision_promote`, the gateway synchronously invokes the sentinel for a bounded review of the candidate revision, **scoped to the agent being promoted**. Each Phase-1 check filters its query by `agent_id`, so a critical finding for agent A does not block promotion of agent B (issue #155, fixed). Critical findings for the scoped agent block the promotion. This generalizes the `promotion_record` severity-gating pattern that already exists for evaluator/auditor evidence.

3. **Event-triggered** — new layer captured, capability declared, approval grant escalation. The gateway emits an event; the sentinel runs a scoped sweep covering only the affected scope.

The sentinel itself is subject to promotion-safety machinery (velocity limits from #25, drift detection from #26) *more strictly than other agents*, because its failure mode is the loudest.

---

## The three hard problems specific to this being an agent

### 1. Recursive trust

A security agent that audits autonoetic is subject to the same failure mode as `agent-factory` (#21). If it regresses — bad prompt, subtle model drift, successful prompt injection — it hides its own failure. This is fundamental: you cannot trust a compromised sentinel to report that it is compromised.

Mitigations (defense in depth, not any one of them):

- **Frozen baseline.** `security_sentinel_baseline.default` is a minimal, deterministic-heavy sentinel shipped with the gateway. It runs on every sweep alongside the current sentinel. Disagreements — where baseline flags something the current sentinel missed, or vice versa — surface as operator-visible events. Baseline revisions require explicit operator action to change.

- **Ensemble.** For critical findings, two independently-configured sentinels (different models, different prompts) must agree. Disagreements route to the operator rather than being silently resolved.

- **Meta-audit.** The sentinel's own findings are themselves subject to a second-order drift check — "has the sentinel's false-positive rate changed across revisions?" This is #26 (aggregate drift detection) applied to the auditor's own behavior.

### 2. Prompt injection against the auditor

The sentinel's job is to read agent manifests, SKILL.md bodies, curator journals, free-form promotion rationales, adversarial session transcripts. That is a large injection surface — adversarial content flowing straight into the sentinel's LLM context.

Mitigations:

- **Structural framing.** SKILL.md bodies and transcript content are treated as data, not nested instructions. The sentinel's system prompt is explicit about this: any instruction-like text read from manifests is hostile.

- **Deterministic first-pass.** Regex, structural checks, and schema validations run before any LLM reasoning. Findings from deterministic checks attach as evidence without requiring the LLM to reason over the raw adversarial content.

- **No actionable tools.** The sentinel has no tools it can be prompt-injected into *using*. Read-only capability profile; output is findings-only. A prompt-injection success can corrupt the sentinel's reasoning for this sweep; it cannot cause the sentinel to take a privileged action.

### 3. Calibration

Constant false-positive noise drowns signal. Cautious low-recall tuning misses novel attacks. Agent-style reasoners are especially prone to both failure modes.

Mitigations:

- **Severity hierarchy tied to reproducibility.** Only deterministic checks (regex, SQL queries, structural comparison) produce `critical` by default. LLM-judgment findings land as `warning` unless the ensemble agrees, in which case they escalate to `critical`.

- **Explicit feedback loop.** The operator triages each finding: `true_positive`, `false_positive`, `benign`. Triage is recorded against the sentinel revision that produced the finding. Revisions that accumulate high false-positive rates are flagged by drift detection and held back from promotion.

- **Start permissive at the core, expand conservatively.** Ship deterministic checks first where the precision/recall trade is fixable with a regex edit rather than a prompt rewrite.

---

## Build plan

Phase numbers are implementation order. Each phase is valuable on its own; later phases depend on earlier ones.

### Phase 0 — Foundation

- `agents/system/` tier scaffolding.
- `SecurityFinding` type in `autonoetic-types`; `security_findings` SQLite table; append-only contract.
- Read-only capability profile for sentinel agents.
- Event emission from findings to the causal chain.

### Phase 1 — Deterministic core

A sentinel that runs purely mechanical checks. No LLM required. High precision, low recall, but every finding is real.

- Credential-pattern regex over agent outputs and causal-event payloads.
- Capability-accretion detection via SQL over `promotion_history` (composes with #28).
- Approval-bypass pattern detection: attempted `sandbox_exec` without grants, repeated approval denials, unusual grant reuse patterns.
- Sandbox-escape pattern matching in causal event payloads (known-bad command patterns, process-tree anomalies).

### Phase 2 — LLM-judgment layer

A reasoning pass over clusters the deterministic core surfaced as interesting-but-not-flagged.

- Prompt-injection smell checks on SKILL.md bodies.
- Curator decision-journal audits (requires #30 to land).
- Review of suspicious session-event clusters that didn't trigger deterministic flags but have unusual shape.
- All findings from this phase land at `warning` by default; `critical` requires ensemble agreement.

### Phase 3 — Frozen baseline + ensemble

- Ship `security_sentinel_baseline.default` — a minimal deterministic-heavy sentinel frozen in the gateway image.
- Every sweep runs current + baseline; disagreements emit operator-visible events.
- Ensemble mode for the reasoning sentinel: configurable second-model pass for critical findings.
- Meta-audit drift check on sentinel revisions.

### Phase 4 — Supply-chain audit

Depends on #20 (`layer.approval_scope` metadata).

- Flag layer mounts where build-time scope exceeds session scope.
- Flag artifacts whose provenance chain includes network access that is no longer approved.
- Flag dependencies pinned in `runtime.lock` with known CVEs (via narrow-scoped external feed — separate approval).

### Phase 5 — Scheduling + gate integration

- Periodic sweep scheduler entries (daily, hourly incremental).
- Synchronous pre-promotion invocation hook in `agent_revision_promote`.
- Event-triggered sweeps on `layer_captured`, `capability_delta`, `approval_escalation`.

### Phase 6 — Calibration & feedback

- Operator-facing CLI for finding triage (`autonoetic security triage`).
- Persistent triage state keyed on `(sentinel_revision_id, finding_type)`.
- Feedback loop into sentinel drift detection (high false-positive rate blocks new sentinel promotions).

### Phase 7 — Adversarial co-evolution

The long-term safety-improvement loop.

- A red-team agent that proposes attack patterns the sentinel should catch.
- Each new accepted attack pattern becomes a deterministic check at the core layer.
- Requires #32 (eval-suite ownership) to prevent the attacker and defender from being authored by the same pipeline — otherwise the red-team agent's drift silently calibrates the sentinel to miss novel attacks.

---

## The structural claim

A security agent is the natural endpoint of the separation-of-powers model applied one level up. For it to work, the sentinel has to be subject to the same architectural constraints as every other powerful role in the system: **frozen baseline, ensemble validation, deterministic core, append-only output contract, no runtime privileges**. The architecture already has all the primitives. This document exists to commit that this is a distinct tier, not a feature of some existing role — and to lay out the order in which to build it without creating new recursive-trust problems.

---

## Related issues

- #18, #19 — Contract enforcement as a prerequisite for auditing `io.accepts` / `io.returns` surfaces.
- #20 — Layer approval scope is the prerequisite for supply-chain auditing.
- #21 — agent-factory self-improvement recovery: the same recursive-trust pattern the sentinel must solve.
- #24 — Revision-bound knowledge: prerequisite for auditing curator writes.
- #25, #26 — Promotion safety governor and drift detection: the sentinel is subject to both, especially strictly.
- #28 — Capability-delta gating: the deterministic core of the sentinel uses the same delta computation.
- #29 — Evolution observability CLI: security findings are another thing the operator needs a pane-of-glass for.
- #30 — Memory curator decision journal: audit target for the sentinel's LLM-judgment layer.
- #32 — Eval-suite ownership: prerequisite for Phase 7.

Implementation tickets for this plan: referenced inline per phase once filed.
