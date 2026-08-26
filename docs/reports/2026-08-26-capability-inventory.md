# 2026-08-26 — Capability inventory

> Compiled for the launch-presentation work (#489): a feature × status sweep so
> the pitch only claims what ships. Status vocabulary follows
> [`../proposals/README.md`](../proposals/README.md): **Shipped** (in code,
> tested), **Partial** (named phases open), **Declared** (in the constitution or
> an RFC, no mechanism). Evidence column points at the proof, not the prose.
> Constitution version at compile time: `2026.07.30`.

How this was assembled: cross-read of the proposals index (per-proposal
status), the enforcement register (per-clause status), `docs/AGENTS.md`
(capability/tool surface), and spot-checks in code. Where sources disagreed,
the enforcement register and tests won.

## Core runtime

| Feature | Status | Evidence |
|---|---|---|
| Agents propose / gateway executes (capability-gated tool dispatch) | Shipped | `runtime/lifecycle.rs`, `policy.rs` |
| Manifest-declared capabilities, mechanical enforcement | Shipped | `autonoetic-types/src/capability.rs`, `policy.rs` |
| Reasoning + script execution modes (incl. wasm JS tier via Javy) | Shipped | `docs/AGENTS.md`, `internals/sandbox/wasm-tier.md` |
| Model-agnostic `llm_presets`; cross-provider failover | Shipped | [`../internals/task-survival.md`](../internals/task-survival.md) |
| Turn continuation at approval boundaries (HMAC-signed suspend/resume) | Shipped | `runtime/continuation.rs`, `continuation_hmac_integrity_integration.rs` |
| Checkpoints, session forking (`trace fork-tree`) | Shipped | `runtime/checkpoint.rs`, `cli/trace.rs` |
| LoopGuard: per-tool failure budget, no-progress, rotating-polling, child-failure trips (P-7.x) | Shipped | `runtime/guard.rs`, enforcement register P-7 |
| Typed wake-ups, yield-don't-poll (Ri-0.14) | Shipped | `docs/AGENTS.md` delegation contract |
| Graduated response: warnings → degraded → escalation (P-7.18) | Shipped | enforcement register; philosophy §4.6 |
| Emergency stop (root-session circuit breaker, grant cleanup) | Shipped | `emergency_stop_root_session_integration.rs` |
| Scheduler: cron jobs, background reevaluation | Shipped | `scheduler/`, `SchedulerAccess` |
| Workflow orchestration (single join, typed child states) | Shipped | `workflow_wait`/`workflow_state`, `docs/AGENTS.md` |

## Sandboxing

| Feature | Status | Evidence |
|---|---|---|
| Four drivers: bubblewrap, docker, microvm, wasm | Shipped | `sandbox/driver/`, [`../internals/sandbox/drivers.md`](../internals/sandbox/drivers.md) |
| `host_fs: allow_set` (gateway-asserted mounts only) | Shipped, **opt-in** — `legacy` whole-host bind still default (DP-1) | `tests/event/allow_set.rs`, #1002 |
| Agent-declared mounts vs operator allowlist | Shipped | `SKILL.md` `mounts`, `sandbox.allowed_mount_roots` |
| In-sandbox network grants | Shipped | `sandbox_network_grant_bwrap_e2e.rs` |
| Gateway-secret masking inside sandboxes (deny-list stopgap) | Shipped (stopgap; allow-set is the durable fix) | `BWRAP_GATEWAY_SENSITIVE_FILES` |

## Approvals & gates

| Feature | Status | Evidence |
|---|---|---|
| Five-layer approval dedup (exec cache → plan grants → session grants → existing approvals → flood cap) | Shipped | root `AGENTS.md`, `approved_exec_cache.rs` |
| Session approval grants: scoped, patterned (`ExactHost`/`HostSuffix`/`HostAndPort`/`UrlPrefix`), expirable, revocable | Shipped | `approval_scope_targets_integration.rs`, `approval_grant_revocation_integration.rs` |
| Plan-as-capability-grant (PlanFrame) | Shipped; envelope evolution open | [`../reference/capability-grants.md`](../reference/capability-grants.md) |
| Agent deciders (`GateDecider`, P-2.20) — office defined before occupant | Shipped capability; multi-decider/voting-weight open | [`../proposals/principal-model-and-symmetric-obligations.md`](../proposals/principal-model-and-symmetric-obligations.md) |
| One-pass preflight (`artifact_prepare` → `deployment_ticket`) | Shipped | `runtime/tools/artifact_prepare.rs` |

## Constitution & governance

| Feature | Status | Evidence |
|---|---|---|
| Versioned, digest-pinned, signed constitution; boot-time digest verification | Shipped | `docs/constitution/`, `recompute_lock.py` |
| Rights bind the gateway: read own chain (Ri-0.2), named rejections (Ri-0.3) with `available_actions`, non-repudiation (Ri-0.11) | Shipped | enforcement register |
| Per-turn signed state attestation (P-6.23): budget, capabilities, constitution digest | Shipped | philosophy §1 |
| `self_describe` — agent self-knowledge in one call, rights surfaced by default | Shipped | `docs/AGENTS.md` |
| Amendment process + right to propose (Ri-0.8); adjudication recording (O-6) | Shipped; **no timeliness/SLA** on adjudication | philosophy §5.3 |
| Entrenched correction core | Declared + structural test; mechanical prevention of weakening **not built** | philosophy §5.2 |
| Amendment invitations (repeated friction → durable invitation, #771 D.2) | Shipped | `docs/AGENTS.md` |
| Anomaly flags (Ri-0.18/O-7): capability-free filing, flood cap, adjudication state machine | Shipped in code; clauses **not yet enacted** (bucketed `unattributed`) | `docs/AGENTS.md`, `scheduler/gateway_store/anomaly_flags.rs` |
| Served-party rights §U (refuse/audit/exit, U-1..U-3) | **Declared MISSING** — `PrincipalKind::ServedUser` exists, no call site emits it | philosophy §5.1 |
| Enforcement register + contract health (`trace contract-health`) | Shipped | `enforcement_register.rs` |
| DISCRETION LEAK naming + counting (P-5.2/P-5.8) | Shipped | `runtime/discretion_leak.rs`, register P-5 |
| Democratic trajectory: voting weight, earned standing, sortition | **Direction only** (RFC #359) | [`../proposals/principal-model-and-symmetric-obligations.md`](../proposals/principal-model-and-symmetric-obligations.md) |

## Memory, content, artifacts

| Feature | Status | Evidence |
|---|---|---|
| Content-addressed immutable artifacts; immutable revisions | Shipped | root `AGENTS.md` |
| Single-door activation (artifact → revision → promote), provenance on imports (P-9.15/16) | Shipped | `skill_install_one_door_provenance.rs` |
| Risk-graduated promotion evidence (P-9.9), capability-delta approval (P-2.25) | Shipped | `docs/AGENTS.md` |
| Knowledge/memory: visibility scopes, retention, cross-agent session sharing | Shipped | `docs/AGENTS.md` |
| Cognitive capsule export / emigration (Ri-0.17) | **Partial** — export broader than self-export; cross-gateway portability not real | philosophy §5.4 |

## Egress & data locality

| Feature | Status | Evidence |
|---|---|---|
| Egress label plane: labels, lattice meet (never widen), taint, LLM chokepoint, stored-content carry | Shipped (phases 1–3) | `runtime/egress_labeler.rs`, [`../proposals/data-envelopes-egress-localization.md`](../proposals/data-envelopes-egress-localization.md) |
| Per-agent output floor (`egress.output_label`) | Shipped | `docs/AGENTS.md` SKILL.md fields |
| Phase 4: federation/MCP/sandbox sinks, operator-approved declassification | **Open** (#909) | [`../proposals/egress-phase4.md`](../proposals/egress-phase4.md) |
| Credential vault, server-side `credential_env` injection — secrets never enter LLM context | Shipped | [`../reference/credentials.md`](../reference/credentials.md) |
| Remote-access static analysis before exec | Shipped | `runtime/remote_access.rs` |

## Multi-agent & evolution

| Feature | Status | Evidence |
|---|---|---|
| Spawn trees, delegation ladder, `agent_discover` | Shipped | `docs/AGENTS.md` |
| Inter-agent messaging (`AgentMessage`, P-11.5, pattern-scoped) | Shipped | `capability.rs`, `docs/wiki/agent-messaging.md` |
| Agents building agents (`specialized_builder`, `agent-factory`), creation lineage (`created_by`/`requested_by`) | Shipped | `docs/AGENTS.md` |
| Evolution offices: steward, memory-curator, skill-crystallizer | Shipped as agents; closed-loop automation open | [`../proposals/implicit-artifacts-agent-evolution.md`](../proposals/implicit-artifacts-agent-evolution.md) |
| Self-improvement loop (`autonoetic improve`) | Partial — P0–P4 shipped, P5–P7 open | [`../proposals/self-improvement-loop.md`](../proposals/self-improvement-loop.md) |
| Eval framework + seeded `civic-core-v1` suite | Shipped (not auto-run) | `docs/AGENTS.md` |

## Federation & interop

| Feature | Status | Evidence |
|---|---|---|
| OFP wire protocol (OpenFang-compatible): handshake with constitution digest, peer `discover`, cross-gateway `agent_message` | Shipped (wire + compatibility tables); gateway-side federation surface thin | `autonoetic-ofp/src/wire.rs` |
| Constitutional pluralism semantics (compatible-set/superset, P-10.9) | Partial — wire support exists; full peer governance direction | philosophy §3.4 |
| MCP client + server | Shipped | `autonoetic-mcp/src/` |
| Python + TypeScript SDKs | Shipped | `autonoetic-sdk/` |

## Observability & operator surface

| Feature | Status | Evidence |
|---|---|---|
| Causal chain: append-only, hash-chained (P-8.1, entrenched) | Shipped | `causal_chain.rs`, register P-8.1 |
| Trace CLI over JSON-RPC (`trace sessions`/`show`/`fork-tree`) | Shipped | `docs/reference/cli.md`, #1119 |
| Cross-session observability (`observability_search`/`read`), `execution_search` | Shipped | `docs/AGENTS.md` |
| Sentinel checks (approval bypass, capability accretion, credential, prompt injection, sandbox escape, session cluster, supply chain) | Shipped; advisory only by design (Ri-0.16) | `sentinel/checks/` |
| Divergence sentinel | Partial — layer 1 + watchdog shipped, P4 validation open | [`../proposals/divergence-sentinel.md`](../proposals/divergence-sentinel.md) |
| Operator activity feed | Partial — phases 0–3 shipped, phase 4 hardening open | [`../proposals/operator-activity-feed.md`](../proposals/operator-activity-feed.md) |
| Workbench (human-agent co-authoring loop) | Shipped in code; **excluded from agent tool discovery** — operator tooling in progress | `runtime/tools/mod.rs` `DEFAULT_EXCLUDED_TOOLS` |

## Explicitly not built (expected launch questions)

- Agent voting / binding collective decisions — direction, staged advisory-first (§3.2).
- Served-party refusal/audit/exit enforcement (§U) — declared, unenforced; sequenced *before* decider power spreads.
- Proposal adjudication SLA (O-6 records decisions; no deadline).
- Cross-gateway emigration (Ri-0.17 partial).
- Mechanical prevention of weakening entrenched clauses.
