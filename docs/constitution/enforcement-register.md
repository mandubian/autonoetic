# Enforcement Register (generated)

> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by hand — run the register generator. Maps each constitutional **clause** — a principle (binds the agent) or a right (binds the gateway) — to the mechanical checks, code, tests, and config that enforce it. Legacy `R-x.y` / `Ri-x.y` IDs are preserved as stable reference keys. See `docs/design/constitution-restructure.md`.

## Bind-direction summary

4 principle(s) (bind the agent), 7 right(s) (bind the gateway), 2 obligation(s) (bind the decider). Counts are partial while migration (#303) is in progress — not the design ratio.

## Principles (bind: agent)

### P-2 — Approval Gates

Promotion and gate actions are bounded so that repeated mechanical rejection cannot be respawned indefinitely across sessions without operator acknowledgement.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `P-2.29` | `promotion_attempts_exhausted` | `runtime/promotion_governor.rs::check_attempt_exhaustion + runtime/tools/agent_revision.rs::record_attempt` | `promotion_attempt_exhaustion_integration.rs` | `promotion_governor.max_promotion_attempts_per_revision` |

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
| `P-8.1` | `hash_chain_integrity` | `causal_chain.rs::compute_entry_hash (SHA-256 over actor_id + prev_hash + fields) + append-only linkage` | `constitution_rights_early_bucket.rs::ri_0_11_tampered_actor_id_leaves_stale_hash` | — |

## Rights (bind: gateway)

### Ri-0.2 — Own history is readable *(entrenched)*

Every agent may read its own causal chain and execution trace. The gateway does not hide actions taken on the agent's behalf. Audit is not a privilege of operators; it is a right of the subject.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.2` | `own_history_readable` | `observability.* tools gated by ReadAccess capability` | `constitution_rights_early_bucket.rs::ri_0_2_agent_with_read_access_can_search_own_traces` | — |

### Ri-0.3 — Named rejection *(entrenched)*

Every rejection names the rule ID that caused it. No agent is ever told "denied" without being told why. Rejection without explanation is indistinguishable from arbitrary authority.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.3` | `named_rejection` | `Tagged::permission_with_rules + PolicyDecision.enforced_rules` | `constitution_rights_late_bucket.rs::ri_0_3_capability_rejection_carries_rule_ids` | — |

### Ri-0.8 — Right to propose amendment *(entrenched)*

Any agent holding the ConstitutionalProposal capability may submit an amendment proposal through the declared channel. The proposal receives a durable ID and enters the review queue; it cannot be silently dropped.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.8` | `amendment_proposal_intake` | `runtime/tools/constitution.rs::constitution_propose_amendment + scheduler/gateway_store/constitutional_proposals.rs` | `constitution_rights_amendment_proposal.rs` | — |

### Ri-0.11 — Non-repudiation *(entrenched)*

Every action an agent performs is attributed to that agent on the causal chain and cannot be retroactively reattributed. The agent can prove what it did; no party can claim the agent performed an action it did not.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.11` | `non_repudiation` | `causal chain hash integrity + agent_id on every event; compute_entry_hash binds actor_id` | `constitution_rights_early_bucket.rs::ri_0_11_hash_chain_integrity` | — |

### Ri-0.13 — Reasoning privacy

An agent's internal reasoning is private-under-law: not used by the gateway as a basis for policy decisions, recorded to the agent's own causal chain for forensic review, and disclosed to other parties only through capability-gated audit.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.13` | `reasoning_disclosure_capability_gated` | `runtime/tools/observability.rs (reasoning audit) + disclosure gating` | `constitution_private_reasoning_c.rs::ri_0_13c_execute_reads_and_discloses` | — |

### Ri-0.14 — Wake-up over polling

When a child task reaches a terminal state or resolves a gate, the gateway wakes the parent with typed child state. Parents are not required to poll to discover child-state transitions.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.14` | `child_state_wakeup` | `scheduler/workflow_store.rs::update_task_run_status (send_child_state_notification) + scheduler/signal.rs + scheduler/task_notify.rs` | `constitution_right_ri_0_14.rs::child_waiting_transition_emits_typed_parent_wakeup_event` | `default_workflow_wait_secs` |

### Ri-0.17 — Self capsule export (emigration)

An agent may request export of its own cognitive capsule for migration to another gateway. Scoped to the caller's own identity.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `Ri-0.17` | `self_capsule_export` | `runtime/tools/capsule.rs::CapsuleExportTool (two-tier gate) + policy.rs::can_use_capsule_self` | `capsule_self_export_scoping_integration.rs::self_export_denied_for_other_agent_id` | — |

## Obligations (bind: decider)

### O-1 — Motivated decision *(entrenched)*

A decision owes a motivation, graduated by stakes. A rejection/abort, or an approval of an elevated-authority or external/irreversible action, is BLOCKING: it does not commit until a non-empty reason is recorded. Silent rejection by a decider is as illegitimate as a gateway denial with no rule ID (Ri-0.3).

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-1` | `decider_obligation_motivation` | `scheduler/approval.rs::enforce_decider_motivation (classifier decision_is_blocking) at the decide_request_with_options chokepoint; emits decider_obligation.refused/.satisfied` | `constitution_o_1_decider_motivation.rs + scheduler::approval::tests::decider_obligation_emits_tagged_o1_event` | `decider_obligations.enabled` |

### O-2 — Attributed decision

Every decision is attributed to the deciding principal (id + kind) on the causal chain and cannot be reattributed. The agent under decision can always tell who decided and what kind of principal they are.

| rule id | check | code | test | config |
|---|---|---|---|---|
| `O-2` | `decider_attribution` | `decided_by + decided_by_kind on the approval (principal::decider_principal_kind, #361) + actor bound into the causal-chain entry hash (causal_chain.rs)` | `constitution_o_1_decider_motivation.rs` | — |

