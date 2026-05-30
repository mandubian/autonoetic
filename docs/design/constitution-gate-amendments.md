# Constitutional Amendments for Unified Gate Abstraction

> Related: [#167](https://github.com/mandubian/autonoetic/issues/167) — HumanGate unification
> Status: Active rationale doc. `P-2.18`, `P-2.19`, and `P-8.19` are ratified and enforced in the current constitution; `P-2.20` and `P-2.21` remain ratified-but-pending implementation.
> Constitution version: `2026.05.30`

## Motivation

The HumanGate unification (#167) introduced `GateService` as a single abstraction for approvals, user interactions, and escalations. The constitution (§2 in particular) was written before this unification and treats approval gates and `user_ask` interactions as separate mechanisms. Additionally, the design supports future non-human deciders (autonomous reviewer agents, policy engines), which the constitution does not yet address.

This document proposes amendments to align the constitution with the unified gate and prepare for agent-as-decider scenarios.

---

## Category 1: Rule Updates (existing rules, amended wording)

### P-2.1 — Broaden scope to unified gate

**Current:**
> Remote network access across all networked tools (`sandbox_exec`, `credential.*`, `web.*`) is statically detected and blocks pending approval rather than hard-denying.

**Proposed:**
> Remote network access across all networked tools (`sandbox_exec`, `credential.*`, `web.*`) is statically detected and blocks pending approval via the unified `GateService` (`GateKind::Approval`) rather than hard-denying. All tool-level approval gates use the centralized gate pipeline for creation, dedup, grant checks, and suspension.

**Rationale:** Reflects that approval gates are no longer reimplemented per-tool but go through `GateService.check()`.

**Implementation reference:** `runtime/human_gate.rs::GateService::check_approval`

---

### P-2.3 — Reflect centralized dedup

**Current:**
> Identical operations within a session deduplicate.

**Proposed:**
> Identical operations within a session deduplicate. The `GateService` centralizes dedup via `find_pending_for_targets`, matching by session, action kind, and host overlap. Tools no longer implement their own dedup logic.

**Implementation reference:** `runtime/human_gate.rs::find_pending_for_targets`

---

### P-2.10 — Unified resume covers both approvals and user interactions

**Current:**
> Approval-gated turns suspend to a continuation; real tool result replays on approve.

**Proposed:**
> Gate-suspended turns (approval, user interaction, escalation) checkpoint via `YieldReason` and resume through `resume_from_checkpoint`. For approval gates, `approval_ref` is auto-injected into tool call arguments on resume. For user interaction gates, the answer is injected as a synthetic tool result. The resume path is unified regardless of `GateKind`.

**Implementation reference:** `execution.rs::resume_from_checkpoint`

---

### P-2.12 — Decider-agnostic resolution

**Current:**
> Operators approve/reject via durable CLI; decisions persist and dispatch signals.

**Proposed:**
> Deciders (human operators, autonomous reviewer agents, or policy engines) approve/reject gates via the approval resolution API. Decisions persist with `decided_by` recording the decider identity (e.g. `"operator"`, `"agent:security-reviewer"`, `"policy-engine:network-rules"`). Decisions dispatch signals for session resume. Human operators use durable CLI; agent deciders call the same `approve_request` / `reject_request` API.

**Rationale:** Prepares for agent-as-decider without changing the current human-operator path.

---

### P-2.13 — user_ask through unified gate

**Current:**
> `user_ask` checkpoints the session as `YieldReason::UserInputRequired`.

**Proposed:**
> `user_ask` creates a gate via `GateService` with `GateKind::UserInput` and checkpoints the session as `YieldReason::UserInputRequired`. The gate row is created in the same store as approval gates, with enrichment thread support.

**Implementation reference:** `runtime/human_gate.rs::check_user_input`

---

### P-2.14 — pending gates (not just approvals)

**Current:**
> `user_ask` is refused if the workflow has active children or pending approvals.

**Proposed:**
> `user_ask` is refused if the workflow has active children or pending gates (approvals, escalations, or other `user_ask` interactions).

**Rationale:** Generalizes from "pending approvals" to "pending gates" since escalations and other gate kinds can also block `user_ask`.

---

### Ri-0.1 — Broaden to pending gates

**Current:**
> Every agent may inspect its own currently-active capabilities, budget state, pending approvals, spawn depth, and session lineage at any turn boundary.

**Proposed:**
> Every agent may inspect its own currently-active capabilities, budget state, pending gates (approvals, user interactions, escalations), spawn depth, and session lineage at any turn boundary.

---

### P-6.23 — Attestation includes gate state

**Current:**
> ...the gateway injects a signed machine-readable state block (remaining budget, active capabilities, pending approvals, spawn depth, session ids, turn counter)...

**Proposed:**
> ...the gateway injects a signed machine-readable state block (remaining budget, active capabilities, pending gates — including approvals, user interactions, and escalations — spawn depth, session ids, turn counter)...

---

### P-10.7 — Generalize self-approval ban

**Current:**
> Remote agents cannot self-approve network access.

**Proposed:**
> No agent may resolve its own gate requests, whether directly or via a delegated agent it spawned. Remote agents cannot self-approve network access. Self-approval is determined by spawn-tree ancestry: an agent and its descendants form a single trust boundary for gate resolution purposes.

**Rationale:** The current rule only covers remote agents and network access. With agent-as-decider, the self-approval ban must extend to any agent resolving gates for itself or its children.

---

## Category 2: New Rules

### P-2.18 — Unified gate mechanism

> All execution suspension points awaiting external input (approvals, user interactions, escalations) use the unified `GateService`. Gate creation, dedup, session grant checks, and enrichment follow the same persistence and audit rules regardless of `GateKind`. Tools create gates via `GateService.check()` and must not bypass it with direct store operations.

**Implementation reference:** `runtime/human_gate.rs`

---

### P-2.19 — Gate enrichment auditability

> Gate enrichment messages (`gate_messages`) are append-only and recorded on the causal chain. Enrichment content is subject to the same redaction rules as tool results (P-4.13). Every enrichment message records sender identity and timestamp. Enrichment threads are visible to the affected agent via `Ri-0.1`.

**Implementation reference:** `scheduler/gateway_store/gate_messages.rs`, `runtime/human_gate.rs::add_gate_message`

---

### P-2.20 — Agent-as-decider capability

> Agents acting as gate deciders require the `GateDecider` capability. The capability scope declares which gate kinds the agent may resolve (`approval`, `escalation`, or both). An agent without `GateDecider` cannot call `approve_request` or `reject_request`. Decider agents are subject to the same dwell time, confirmation phrase, and hardening rules as human operators (P-2.24).

---

### P-2.21 — Agent-decider escalation to human

> When an agent-decider cannot determine whether to approve or reject a gate (insufficient context, policy ambiguity, or high-risk action beyond its scope), it must escalate to a human operator rather than reject. Escalation creates a new `GateKind::Escalation` gate referencing the original gate ID. The original gate remains pending until the human operator resolves both.

---

### P-8.19 — Gate decision attribution

> Every gate resolution (approve, reject, cancel, timeout) records `decided_by` with the full decider identity on the causal chain. For human operators: `"operator"` or `"operator:<username>"`. For agent deciders: `"agent:<agent_id>"`. For policy engines: `"policy:<engine_id>"`. The `decided_by` field is immutable after recording.

---

## Category 3: I-6 compliance in GateService (`R+++3` placeholder)

I-6 requires every gateway decision to record enforced rule IDs (the code
still uses the `R+++3` placeholder string for the default baseline).
`human_gate.rs` currently makes decisions without recording which rules
drove them. The following annotations should be added:

| Gate decision | Enforced rules |
|---------------|---------------|
| Session grant clears gate | `P-2.4` |
| Dedup returns `AlreadyPending` | `P-2.3` |
| `approval_ref` validates and clears | `P-2.6` (fingerprint) or `P-2.4` (host grant) |
| New approval row created | `P-2.1`, `P-2.2` |
| Flood cap rejects creation | `P-7.17` |
| Pre-validated bypass (cache) | `P-2.6` |
| `UserInput` gate created | `P-2.13` |
| `user_ask` refused due to pending gates | `P-2.14` |

These should be emitted as `enforced_rules` on the corresponding causal events.

---

## Summary of Changes

| Type | Count | Items |
|------|-------|-------|
| Amended rules | 7 | P-2.1, P-2.3, P-2.10, P-2.12, P-2.13, P-2.14, P-10.7 |
| Amended rights | 1 | Ri-0.1 |
| Amended attestation | 1 | P-6.23 |
| New rules | 4 | P-2.18, P-2.19, P-2.20, P-2.21 |
| New audit rule | 1 | P-8.19 |
| I-6 annotations (`R+++3` placeholder) | 8 | In `human_gate.rs` gate decisions |
