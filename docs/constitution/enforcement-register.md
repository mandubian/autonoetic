# Enforcement Register (generated)

> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by hand — run the register generator. Maps each constitutional **clause** — a principle (binds the agent) or a right (binds the gateway) — to the mechanical checks, code, tests, and config that enforce it. Legacy `R-x.y` / `Ri-x.y` IDs are preserved as stable reference keys. See `docs/design/constitution-restructure.md`.

## Bind-direction summary

5 principle(s) (bind the agent), 9 right(s) (bind the gateway), 4 obligation(s) (bind the decider). Counts are partial while migration (#303) is in progress — not the design ratio.

## Principles (bind: agent)

### P-2 — Approval Gates

Promotion and gate actions are bounded so that repeated mechanical rejection cannot be respawned indefinitely across sessions without operator acknowledgement.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `P-2.29` | `promotion_attempts_exhausted` | `runtime/promotion_governor.rs::check_attempt_exhaustion + runtime/tools/agent_revision.rs::record_attempt` | `promotion/attempt_exhaustion.rs` | `promotion_governor.max_promotion_attempts_per_revision` |

### P-5 — Deterministic coercion and response validation

The gateway normalizes model I/O only through deterministic, pre-committed tolerances; every such intervention is observable and counted as a named discretion leak (§14). No gateway judgment about the agent's output is silent or hidden.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `P-5.2` | `input_normalization_leak` | `runtime/discretion_leak.rs::record_discretion_leak (tokio::task_local scope) + runtime/tool_call_processor.rs::note_llm_normalization + runtime/response_validation.rs::strip_markdown_code_fences` | `runtime::discretion_leak::tests` | — |
| `P-5.8` | `gateway_authored_repair_leak` | `runtime/response_validation.rs::validate_and_maybe_repair (gateway-authored repair prompt) + runtime/discretion_leak.rs::record_discretion_leak` | `runtime::discretion_leak::tests` | `response_validation.repair_enabled, response_validation.max_validation_loops, max_validation_duration_ms` |

### P-7 — Bounded progress

A session is halted when it stops making progress, on a closed, configurable set of mechanically-detected non-progress conditions, each emitting a typed, attributable reason. No condition relies on agent self-report.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `P-7.5` | `tool_failure_budget` | `guard.rs::register_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_tool_failure_budget` | `loop_guard.max_tool_failures` |
| `P-7.7` | `no_meaningful_progress` | `guard.rs::check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_max_loops` | `loop_guard.max_loops_without_progress` |
| `P-7.19` | `rotating_polling_pattern` | `guard.rs::register_progress_inner (window + trip) + check_loop` | `runtime::guard::tests::rotating_polling_pattern_with_five_tools_trips` | `loop_guard.rotation_window_size, loop_guard.rotation_distinct_floor` |
| `P-7.20` | `child_failure_budget` | `guard.rs::register_child_failure + check_loop` | `runtime::guard::tests::test_loop_guard_trips_on_child_failures` | `loop_guard.max_child_failures` |

### P-8.1 — Hash-chain integrity *(entrenched)*

The causal chain is append-only JSONL with hash-chain integrity — each entry's `entry_hash` binds its fields and its `prev_hash` links it to the prior entry. Tampering with any recorded field (actor, action, outcome) leaves a stale hash detectable by recomputation.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `P-8.1` | `hash_chain_integrity` | `causal_chain.rs::compute_entry_hash (SHA-256 over actor_id + prev_hash + fields) + append-only linkage` | `constitution/rights_early_bucket.rs::ri_0_11_tampered_actor_id_leaves_stale_hash` | — |

### P-9 — Agent Install & Provenance

Three-stage activation — artifact_build, revision.create, revision.promote — gated so that every surface that activates an agent passes the same promotion gates (single door), and every externally-installed agent carries durable import provenance.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `P-9.15` | `single_door_activation` | `runtime/tools/skill.rs::SkillInstallTool + bootstrap.rs::bootstrap_single_agent_candidate_only + bootstrap.rs::bootstrap_agents + runtime/tools/agent_revision.rs::AgentRevisionPromoteTool + runtime/tools/agent_revision.rs::check_capability_delta` | `skill_install_one_door_provenance.rs::one_door_generous_install_stays_candidate_and_unpromoted` | — |
| `P-9.16` | `import_provenance_recorded` | `runtime/tools/skill.rs::SkillInstallTool + bootstrap.rs::bootstrap_single_agent_candidate_only` | `skill_install_one_door_provenance.rs::provenance_recorded_on_revision_and_causal_event` | — |

## Rights (bind: gateway)

### Ri-0.2 — Own history is readable *(entrenched)*

Every agent may read its own causal chain and execution trace. The gateway does not hide actions taken on the agent's behalf. Audit is not a privilege of operators; it is a right of the subject.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.2` | `own_history_readable` | `observability.* tools gated by ReadAccess capability` | `constitution/rights_early_bucket.rs::ri_0_2_agent_with_read_access_can_search_own_traces` | — |

### Ri-0.3 — Named rejection *(entrenched)*

Every rejection names the rule ID that caused it. No agent is ever told "denied" without being told why. Rejection without explanation is indistinguishable from arbitrary authority.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.3` | `named_rejection` | `Tagged::permission_with_rules + PolicyDecision.enforced_rules` | `constitution/rights_late_bucket.rs::ri_0_3_capability_rejection_carries_rule_ids` | — |

### Ri-0.8 — Right to propose amendment *(entrenched)*

Any agent holding the ConstitutionalProposal capability may submit an amendment proposal through the declared channel. The proposal receives a durable ID and enters the review queue; it cannot be silently dropped.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.8` | `amendment_proposal_intake` | `runtime/tools/constitution.rs::constitution_propose_amendment + scheduler/gateway_store/constitutional_proposals.rs` | `constitution/rights_amendment_proposal.rs` | — |

### Ri-0.11 — Non-repudiation *(entrenched)*

Every action an agent performs is attributed to that agent on the causal chain and cannot be retroactively reattributed. The agent can prove what it did; no party can claim the agent performed an action it did not.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.11` | `non_repudiation` | `causal chain hash integrity + agent_id on every event; compute_entry_hash binds actor_id` | `constitution/rights_early_bucket.rs::ri_0_11_hash_chain_integrity` | — |

### Ri-0.12 — Closed list of termination reasons

A session terminates only for a reason in the declared, closed list (agent exit, budget exhaustion, operator emergency stop, parent-orphan reap, unrecoverable fatal error naming a rule ID, scheduled timeout). Turn-budget exhaustion — the `max_session_turns_hard` ceiling that continuation approvals cannot lift — terminates as budget exhaustion; any termination outside the list is a rights violation and a gateway bug.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.12` | `session_turn_hard_cap` | `runtime/lifecycle.rs::execute_with_history + emit_session_turn_hard_cap_event + runtime/tool_dispatch.rs::effective_max_session_turns_hard` | `runtime::lifecycle::tests::test_max_session_turns_hard_cap_terminates_without_approval` | `max_session_turns_hard, max_session_turns, loop_guard.max_session_turns_hard` |

### Ri-0.13 — Reasoning privacy

An agent's internal reasoning is private-under-law: not used by the gateway as a basis for policy decisions, recorded to the agent's own causal chain for forensic review, and disclosed to other parties only through capability-gated audit.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.13` | `reasoning_disclosure_capability_gated` | `runtime/tools/observability.rs (reasoning audit) + disclosure gating` | `constitution/private_reasoning_c.rs::ri_0_13c_execute_reads_and_discloses` | — |

### Ri-0.14 — Wake-up over polling

When a child task reaches a terminal state or resolves a gate, the gateway wakes the parent with typed child state. Parents are not required to poll to discover child-state transitions.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.14` | `child_state_wakeup` | `scheduler/workflow_store.rs::update_task_run_status (send_child_state_notification) + scheduler/signal.rs + scheduler/task_notify.rs` | `constitution/right_ri_0_14.rs::child_waiting_transition_emits_typed_parent_wakeup_event` | `default_workflow_wait_secs` |

### Ri-0.17 — Self capsule export (emigration)

An agent may request export of its own cognitive capsule for migration to another gateway. Scoped to the caller's own identity.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.17` | `self_capsule_export` | `runtime/tools/capsule.rs::CapsuleExportTool (two-tier gate) + policy.rs::can_use_capsule_self` | `capsule_self_export_scoping_integration.rs::self_export_denied_for_other_agent_id` | — |

### Ri-0.18 — Right to report

Any agent may file an anomaly report without holding any capability; every flag is durably recorded, non-repudiably attributed, cannot be silently dropped, and filing is never itself grounds for sanction.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.18` | `anomaly_flag_capability_free_intake` | `runtime/tools/anomaly_flag.rs::AnomalyFlagTool + scheduler/gateway_store/anomaly_flags.rs::insert_anomaly_flag + scheduler/gateway_store/anomaly_flags.rs::emit_anomaly_flag_flood_alert` | `anomaly_flag_integration.rs::tool_available_with_zero_capabilities + anomaly_flag_integration.rs::filing_emits_causal_event_tagged_ri_0_18 + anomaly_flags.rs::flood_cap_rejects_at_limit_and_keeps_existing + anomaly_flag_integration.rs::flood_cap_rejects_filing_loudly` | `max_pending_anomaly_flags_per_reporter` |

## Obligations (bind: decider)

### O-1 — Motivated decision *(entrenched)*

A decision owes a motivation, graduated by stakes. A rejection/abort, or an approval of an elevated-authority or external/irreversible action, is BLOCKING: it does not commit until a non-empty reason is recorded. Silent rejection by a decider is as illegitimate as a gateway denial with no rule ID (Ri-0.3).

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-1` | `decider_obligation_motivation` | `scheduler/approval.rs::enforce_decider_motivation (classifier decision_is_blocking) at the decide_request_with_options chokepoint; emits decider_obligation.refused/.satisfied` | `constitution/o_1_decider_motivation.rs + scheduler::approval::tests::decider_obligation_emits_tagged_o1_event` | `decider_obligations.enabled` |

### O-2 — Attributed decision

Every decision is attributed to the deciding principal (id + kind) on the causal chain and cannot be reattributed. The agent under decision can always tell who decided and what kind of principal they are.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-2` | `decider_attribution` | `decided_by + decided_by_kind on the approval (principal::decider_principal_kind, #361) + actor bound into the causal-chain entry hash (causal_chain.rs)` | `constitution/o_1_decider_motivation.rs` | — |

### O-6 — Duty to adjudicate proposals, on time

A proposal review authority owes every Ri-0.8 proposal a recorded, motivated decision within a bounded adjudication window; a proposal left un-adjudicated past the window is a recorded breach attributed to the adjudicating seat (the decision is still owed). Window duration is config.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-6` | `proposal_adjudication_recorded_within_sla` | `scheduler/gateway_store/constitutional_proposals.rs::decide_constitutional_proposal + scheduler/gateway_store/constitutional_proposals.rs::flag_proposal_sla_breaches + scheduler.rs::check_adjudication_sla_breaches` | `router.rs::test_dispatch_constitution_resolve_proposal + scheduler.rs::breaches_are_recorded_without_changing_status` | `decider_obligations.enabled, decider_obligations.adjudication_sla_secs` |

### O-7 — Duty to adjudicate reports, on time

An anomaly review authority owes every Ri-0.18 flag a recorded, motivated decision (confirmed/dismissed/deferred, with under_review as the non-terminal holding state) within a bounded adjudication window; a flag left un-adjudicated past the window is a recorded breach attributed to the adjudicating seat (the decision is still owed). Window duration is config.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-7` | `anomaly_adjudication_recorded_within_sla` | `runtime/tools/anomaly_adjudicate.rs::AnomalyAdjudicateTool + scheduler/gateway_store/anomaly_flags.rs::decide_anomaly_flag + scheduler/gateway_store/anomaly_flags.rs::flag_anomaly_flag_sla_breaches + scheduler.rs::check_adjudication_sla_breaches` | `router.rs::test_dispatch_anomaly_resolve_terminal_decision_without_reason_rejected + anomaly_adjudicate_tool_integration.rs::terminal_decision_requires_reason + scheduler.rs::breaches_are_recorded_without_changing_status` | `decider_obligations.enabled, decider_obligations.adjudication_sla_secs` |

