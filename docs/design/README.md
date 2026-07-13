# Active Design Docs

Plans and RFCs with **open work** — partial implementation, pending validation,
or constitutional migration still in flight. Completed or superseded plans live
under [`../archived/`](../archived/).

| Doc | Status | Live reference (when shipped) |
|-----|--------|-------------------------------|
| [`constitution-gate-amendments.md`](constitution-gate-amendments.md) | Mostly shipped — gate unification (P-2.18), gate enrichment (P-2.19), agent-as-decider `GateDecider` capability (P-2.20), and escalation (P-2.21) are all `ENFORCED`. What remains as draft RFC work is the broader multi-decider / voting-weight / ratification vision ([`principal-model-and-symmetric-obligations.md`](principal-model-and-symmetric-obligations.md)) | Constitution §2 |
| [`constitution-restructure.md`](constitution-restructure.md) | Partial — P-x.y restructure in progress | [`constitution/enforcement-register.md`](../constitution/enforcement-register.md) |
| [`operator-approval-inspection-plan.md`](operator-approval-inspection-plan.md) | Partial — Phase 1 (code excerpts) shipped; Phase 2 pending | Approval CLI / `code_excerpts.rs` |
| [`human-agent-artifact-collaboration-plan.md`](human-agent-artifact-collaboration-plan.md) | Draft RFC — PlanFrame + workbench projection for human/agent co-construction | — |
| [`operator-activity-feed-plan.md`](operator-activity-feed-plan.md) | Partial — Phases 0–3 shipped; Phase 4 hardening pending | `operator_activity` table, `operator.activity.list`, chat TUI |
| [`post-promotion-review-design.md`](post-promotion-review-design.md) | Partial — Tier 1 observational review shipped; Tier 2 fixture drift pending | `post_promotion_review.rs` |
| [`divergence-sentinel-design.md`](divergence-sentinel-design.md) | Partial — Layer 1 + manual watchdog shipped; P4 validation pending | [`security-sentinel.md`](../security-sentinel.md) |
| [`divergence-sentinel-validation.md`](divergence-sentinel-validation.md) | Pending operator sign-off | `sentinel_experiment` CLI |
| [`self-improvement-loop-design.md`](self-improvement-loop-design.md) | Partial — P0–P4 shipped; P5–P7 pending | `autonoetic improve`, [`context-compression.md`](../context-compression.md) |
| [`self-improvement-loop-validation.md`](self-improvement-loop-validation.md) | Pending 3-cycle validation | — |
| [`constitutional-evolution-reflections.md`](constitutional-evolution-reflections.md) | Discussion draft (not a proposal) | — |
| [`principal-model-and-symmetric-obligations.md`](principal-model-and-symmetric-obligations.md) | Draft RFC — unified Principal (AI/human/script) + decider obligations (§O) as near-term; authorship/attestation vs ratification; per-instance authority (cardinality + domains, judicial door reserved); democratic frame as explicit horizon | — |
| [`operator-legibility.md`](operator-legibility.md) | Draft RFC — tiered timeline + plan inherit/diff + approve-the-envelope + t=0 workbench | — |
| [`citizenship-as-a-runtime-service.md`](citizenship-as-a-runtime-service.md) | **Partial** — Parts A.1 (denial `available_actions`), B.1 (task-matched injected recall), C.1 (`anomaly_flag` tool + adjudication RPC), and C.2 (`anomalies` schema field) shipped (PR #782); the Ri-0.18/O-7 amendment they anticipate is drafted but unsigned (`docs/constitution/amendments/2026-07-12-anomaly-reporting-DRAFT.md`). Remaining draft RFC work: civic attestation line, O-6 SLA + mechanical amendment invitations + DISCRETION LEAK register, civic evals / civic health / promotion gating, institutional offices. Companion to [`principal-model-and-symmetric-obligations.md`](principal-model-and-symmetric-obligations.md); tracking #774 (workstreams #768–#773) | `docs/AGENTS.md`, `docs/response-validation-gate.md`, `docs/agent-prompt-guidance.md`, `docs/agent-learning.md` |
| [`task-robustness.md`](task-robustness.md) | Draft RFC — task survival for non-deterministic agents: typed failure taxonomy with per-kind retry policies, enforced `expected_outputs` as a loop-safe bundle (check → typed failure → dignified exit → parent loop guard), plan-time capability preflight, burn-rate forecasting in the attestation; references existing compression/failover designs in [`../comparison-hermes-agent.md`](../comparison-hermes-agent.md). Companion to [`citizenship-as-a-runtime-service.md`](citizenship-as-a-runtime-service.md); tracking #781 (workstreams #775–#780) | — |
