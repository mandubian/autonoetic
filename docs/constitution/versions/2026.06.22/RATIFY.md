# RATIFY.md — Constitution Version 2026.06.22

## Summary

Constitutional amendments implementing the consolidated enforcement issue (#576):
mechanical install/promotion contracts replacing paperwork gates. Three principles,
four constitutional changes.

**Prerequisite:** PR #583 (trace-based promotion evidence) must merge for P-2.9/P-2.26
enforcement anchors to be live. PRs #581 (host contract) and #582 (smoke test) are merged.

## Amendments

### P-1.5 — NetworkAccess host scoping (amended)

The gateway now owns the detected-host contract. Hosts extracted from artifact source
at install time are persisted as `revision.detected_network_hosts` (gateway-owned, not
LLM-declared). Declared `NetworkAccess.hosts` must cover all detected hosts. Wildcard
`hosts: ["*"]` is rejected unless the agent declares `open_web: true`. At runtime,
`enforce_remote_target_policy` additionally verifies the outbound target is covered by
`NetworkAccess.hosts`, not just `remote_access.targets`.

**Diff:**
```
< | P-1.5 | `NetworkAccess` is scoped by host allowlist. | ARCHITECTURE.md | `policy.rs:468 can_connect_net` | ENFORCED |
---
> | P-1.5 | `NetworkAccess` is scoped by host allowlist. The gateway owns the detected-host contract: hosts extracted from artifact source at install time are persisted as `revision.detected_network_hosts` (gateway-owned, not LLM-declared). Declared `NetworkAccess.hosts` must cover all detected hosts at install time; wildcard `hosts: ["*"]` is rejected unless the agent declares `open_web: true`. At runtime, `enforce_remote_target_policy` additionally verifies the outbound target is covered by the declared `NetworkAccess.hosts`, not just `remote_access.targets`. | ARCHITECTURE.md; this amendment | `runtime/network_host_contract.rs::validate_network_host_contract` (install), `runtime/network_policy.rs::enforce_remote_target_policy` (runtime), `scheduler/approval.rs::emit_host_contract_drift_events` (drift signal) | ENFORCED |
```

**Implementation:** PR #581 — `runtime/network_host_contract.rs`, migration v56.

### P-2.9 — promotion_record evidence contract (amended)

`promotion_record` evidence is now trace-based. Execution roles must attach
`execution_trace_id`; `pass` is derived from `exit_code=0`. Auditor keeps explicit
`pass`; only critical findings veto. The severity/evidence opinion matrix is removed.
The promote gate re-verifies stored traces at promotion time.

**Diff:**
```
< | P-2.9 | `promotion_record` with `pass=true` rejects on error/critical findings, and on warning findings lacking evidence. | approval-system.md | `runtime/tools/promotion.rs` | ENFORCED |
---
> | P-2.9 | `promotion_record` evidence is trace-based. Execution roles (`unit_test_runner`, `static_evaluator`, `sealed_evaluator`, legacy `evaluator`) must attach `execution_trace_id` from a completed run; `pass` is derived from `exit_code=0`, not set by the LLM. The auditor role sets `pass` explicitly; only `severity=critical` findings veto an otherwise-passing audit — non-critical findings are advisory annotation. The promote gate (`agent_revision_promote`) re-verifies stored execution traces at promotion time: a role with `pass=true` but missing, not-found, or failed trace is rejected. The severity/evidence opinion matrix is removed. | approval-system.md; this amendment | `runtime/tools/promotion.rs` (trace derivation), `runtime/promotion_evidence.rs::verify_stored_execution_traces` (promote-time re-check) | ENFORCED |
```

**Implementation:** PR #583 — `runtime/promotion_evidence.rs`, `runtime/tools/promotion.rs`.

### P-2.26 — All executed gate roles must pass (amended)

Same intent, but "pass" is now trace-derived (P-2.9). The enforcement anchor shifts from
the promotion DB verdict to the execution trace. The promote gate re-verifies via
`verify_stored_execution_traces`.

**Diff:**
```
< | P-2.26 | ... The orchestrator (agent-factory) cannot skip a failed gate by omitting it from the install dispatch ... | this amendment | `runtime/tools/agent_revision.rs::enforce_promotion_gate` (checks `unit_test_runner_pass` when `unit_test_runner_id` is present); `runtime/promotion_store.rs::is_fully_promoted` | ENFORCED |
---
> | P-2.26 | ... "Pass" for execution roles is trace-derived (P-2.9): the promotion gate re-verifies the stored execution trace at promote time, not the LLM-recorded verdict. ... | this amendment | `runtime/tools/agent_revision.rs::enforce_promotion_gate`, `runtime/promotion_evidence.rs::verify_stored_execution_traces`; `runtime/promotion_store.rs::is_fully_promoted` | ENFORCED |
```

**Implementation:** PR #583.

### P-2.28 — Smoke-test gate for new agents (new)

New agents declaring `NetworkAccess` or `CodeExecution` require a successful execution
trace before promotion to `Ready`. Operator involvement is auto-determined from capability
risk. Complements P-3.10 (sealed evaluator = no network; smoke test = with network).
The `agent_install_smoke_test` config knob is deprecated.

**New row:**
```
> | P-2.28 | **Smoke-test gate for new agents.** New agents declaring `NetworkAccess` or `CodeExecution` require a successful execution trace before promotion to `Ready`. Operator involvement is auto-determined from capability risk ... The `agent_install_smoke_test` config knob is deprecated — the gate is unconditional for capability-bearing agents. | this amendment | `runtime/smoke_test_gate.rs::smoke_test_involvement`, `runtime/tools/agent_revision.rs::AgentRevisionPromoteTool::execute` | ENFORCED |
```

**Implementation:** PR #582 — `runtime/smoke_test_gate.rs`.

## Related

- Parent issue: #576
- Implementation: #581 (P2 host contract), #582 (P1 smoke test), #583 (P3 trace evidence)
- Postmortem: `docs/postmortems/session-b6d27af2-weather-agent.md`
