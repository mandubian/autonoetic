# Active Design Docs

Plans and RFCs with **open work** — partial implementation, pending validation,
or constitutional migration still in flight. Completed or superseded plans live
under [`../archived/`](../archived/).

| Doc | Status | Live reference (when shipped) |
|-----|--------|-------------------------------|
| [`human-gate-unification-plan.md`](human-gate-unification-plan.md) | Partial — GateService done; tool migrations + P-2.20/P-2.21 pending | `runtime/human_gate.rs`, [`constitution-gate-amendments.md`](constitution-gate-amendments.md) |
| [`constitution-gate-amendments.md`](constitution-gate-amendments.md) | Partial — P-2.18/19 enforced; agent-as-decider pending | Constitution §2 |
| [`constitution-restructure.md`](constitution-restructure.md) | Partial — P-x.y restructure in progress | [`constitution/enforcement-register.md`](../constitution/enforcement-register.md) |
| [`operator-approval-inspection-plan.md`](operator-approval-inspection-plan.md) | Partial — Phase 1 (code excerpts) shipped; Phase 2 pending | Approval CLI / `code_excerpts.rs` |
| [`human-agent-artifact-collaboration-plan.md`](human-agent-artifact-collaboration-plan.md) | Draft RFC — PlanFrame + workbench projection for human/agent co-construction | — |
| [`operator-activity-feed-plan.md`](operator-activity-feed-plan.md) | Draft RFC — gateway-owned operator activity stream for TUI + messaging bridges | — |
| [`post-promotion-review-design.md`](post-promotion-review-design.md) | Partial — Tier 1 observational review shipped; Tier 2 fixture drift pending | `post_promotion_review.rs` |
| [`divergence-sentinel-design.md`](divergence-sentinel-design.md) | Partial — Layer 1 + manual watchdog shipped; P4 validation pending | [`security-sentinel.md`](../security-sentinel.md) |
| [`divergence-sentinel-validation.md`](divergence-sentinel-validation.md) | Pending operator sign-off | `sentinel_experiment` CLI |
| [`self-improvement-loop-design.md`](self-improvement-loop-design.md) | Partial — P0–P4 shipped; P5–P7 pending | `autonoetic improve`, [`context-compression.md`](../context-compression.md) |
| [`self-improvement-loop-validation.md`](self-improvement-loop-validation.md) | Pending 3-cycle validation | — |
| [`constitutional-evolution-reflections.md`](constitutional-evolution-reflections.md) | Discussion draft (not a proposal) | — |
| [`principal-model-and-symmetric-obligations.md`](principal-model-and-symmetric-obligations.md) | Draft RFC — unified Principal (AI/human/script) + decider obligations (§O) as near-term; authorship/attestation vs ratification; per-instance authority (cardinality + domains, judicial door reserved); democratic frame as explicit horizon | — |
