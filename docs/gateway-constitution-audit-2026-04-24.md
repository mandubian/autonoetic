# Gateway Constitutional Audit — 2026-04-24

> Frozen snapshot of the first comprehensive audit of gateway rule
> enforcement against the architectural constitution.
>
> This document is **historical** — it records findings at a point in
> time. The living rule list is `docs/constitution/versions/2026.05.05/constitution.md`; the
> active backlog is `docs/gateway-constitution-roadmap.md`.

## Purpose

The gateway's job is to **enforce rules** — not to invent them. Agents are
low-privilege reasoners free to propose any action; the gateway is the
high-privilege executor that validates each proposal against a finite set
of declared laws. Together those laws form a **constitution** that should
be reviewable by humans or agents and improvable over time.

This audit:

1. Extracts the rules the docs say the gateway enforces (168 rules, 11
   categories).
2. Maps each rule to concrete enforcement code (~120 enforcement points).
3. Classifies each rule as `ENFORCED`, `PARTIAL`, `MISSING`, or
   `DESIGN DEBT`.
4. Flags places where the gateway is **not dumb** — where it invents
   policy rather than enforcing declared rules.
5. Proposes additions (`R+` rules) that close real gaps.

## Overall posture

Roughly **65% of the constitution is fully enforced**, **20% partial**,
**15% missing or design debt**. Weaknesses cluster in three areas:

- **Gateway logic creep.** The response-repair loop, the LLM-coercion
  schema fallback, and the remote-access static analyzer make the
  gateway a decision-maker rather than a rule-enforcer. Most acute in
  `execution.rs::validate_and_maybe_repair` and the schema enforcement
  hook's LLM fallback.

- **Declared but not invariant.** Several constitutional rules are
  written in design docs with no runtime invariant and no test pinning
  them — spawn-chain depth caps, approval flood control, runtime-lock
  drift detection, fsync-before-state-transition ordering.

- **Ingress/egress asymmetry.** Incoming messages get schema validation
  and policy gates; outgoing tool results flowing back into parent
  conversations do not. The perimeter is hardened in one direction only.

## Category-by-category findings

Each category lists only the rules with a non-obvious status or a gap.
The full rule table lives in `docs/constitution/versions/2026.05.05/constitution.md`.

### 1. Capability & Rights

Strong. Every native tool call is gated at
`runtime/tool_call_processor.rs:294` via `policy.can_invoke_tool`.
Capability-specific checks live in `policy.rs:406-695`.

- **Gap**: shorthand capabilities (bare strings) are only rejected for
  the three high-risk caps (`NetworkAccess`, `CodeExecution`,
  `AgentSpawn`). Low-risk caps still accept shorthand, leaving a soft
  path for underdeclaration.

### 2. Approval Gates

Strong core: persistence (`approvals` table), session grants
(`session_approval_grants`), fingerprint cache
(`approved_exec_cache.rs`), continuation replay
(`runtime/continuation.rs:178`). Promotion severity gating enforced in
`runtime/tools/promotion.rs`.

- **Missing — approval flood cap.** A rogue agent can manufacture
  endless distinct approval requests. Nothing rate-limits pending
  approvals per root session.
- **Missing — grant TTL.** Session grants live for the entire session
  lifetime, with no max-age. A one-hour-old approval and a one-day-old
  approval carry the same weight.
- **Uncertain — atomicity.** Approve-decision and grant-write don't
  demonstrably commit in the same SQLite transaction. Partial-state
  after crash is possible.

### 3. Sandbox Isolation

Strong isolation via bubblewrap `--unshare-all` (`sandbox.rs:914`) with
`--share-net` opt-in derived from `NetworkAccess` capability
(`sandbox.rs:42`). Layer mounts are read-only. Dangerous commands
blocked by `policy.rs::analyze_command:46`.

- **Gap — resource limits.** Bubblewrap has no built-in cgroup quotas.
  Memory, PID, and disk limits rely on OS defaults unless Docker or
  microvm drivers are used. A fork-bomb in bubblewrap is not
  constitutionally bounded.

### 4. Credential & Secret Protection

Vault is AES-256-GCM with random 96-bit nonces (`vault.rs:112`) and
requires a master key at startup (`vault.rs:70`). Redaction of
logs/traces covers bearer tokens, PEM, JWTs, common key names
(`log_redaction.rs:128`). Response validation blocks secret-like text
in outputs via `prohibited_text_patterns`.

- **Uncertain — redaction ordering.** I could not verify that redaction
  runs *before* causal-chain append on every path. Redaction-after-write
  leaves a raw payload briefly on disk.
- **Partial — zeroization.** `SecretString` wrapping is used but not
  audited end-to-end across every sandbox-exec code path.

### 5. I/O Schema Validation

Deterministic schema coercion via the enforcement hook
(`DeterministicCoercionEnforcer`), response-contract validation against
authoritative runtime state (`runtime/response_validation.rs:68`),
bounded repair loop in `execution.rs:1965`.

- **Missing — egress validation.** Schema enforcement applies only to
  ingress (messages going into child sessions). Tool *results* flowing
  back to parents are not validated against any `io.produces` schema.
  The perimeter is one-way.
- **Partial — uniform error envelope.** Most native tools conform to
  `{error_type, message, repair_hint}`, but there is no shared helper or
  test pinning the shape.

### 6. Session / Workflow / Budget

Per-session caps are solid:
`runtime/session_budget.rs` for turn/tool/time/cost,
`runtime/prompt_budget.rs` for tokens. Checkpoints cover all yield
points (`runtime/checkpoint.rs`).

- **Missing — root-session tree budget.** A parent can spawn 10 siblings
  each under its own budget; there is no tree-wide aggregation. A
  misbehaving parent pattern can legally outspend any individual limit
  by fan-out.
- **Missing — continuation chain depth.** No cap on nested approval
  continuations.

### 7. Abuse / Hard-Stop

Emergency stop is gated by capability (`policy.rs:535`), kills child
processes, cancels approvals, deletes grants
(`execution.rs::emergency_stop_root_session`). Loop guard trips on any
of three independent conditions (`runtime/guard.rs:66`).

- **Missing — spawn-chain depth.** `AgentSpawn.max_children` bounds
  fan-out. Nothing bounds `A → B → C → D → …` depth.
- **Missing — orphan reaper.** If a parent session dies or stalls,
  nothing guarantees its children are cancelled or hoisted.
- **Gap — loop-guard thresholds hardcoded.** Constants in `guard.rs`
  apply to all agents uniformly, not declared per-manifest within a
  system ceiling.

### 8. Audit & Traceability

Hash-chained JSONL in `.gateway/history/causal_chain.jsonl`
(`causal_chain.rs:65`), mirrored to SQLite (`causal_events` table),
execution traces separately (`execution_traces` table). Runtime lock
includes source and binary SHA (`runtime_lock.rs`, `build.rs`).

- **Missing — fsync invariant.** No explicit assertion that a JSONL
  append is flushed before a state transition that depends on it
  (permitting continuation resume before the approve event is durable).
- **Missing — runtime-lock drift check.** The lock is computed, but I
  found no gate refusing to start a session whose lock diverges from the
  current gateway binary's SHA.

### 9. Agent Install & Provenance

Three-stage activation (`artifact_build → revision.create →
revision.promote`) is enforced. Revisions are immutable and
content-addressed. SKILL.md is parsed and its capabilities extracted.
External-import detection in
`runtime/install_contract.rs::detect_external_python_imports`.

- **Missing — bundle signature verification.** Content-addressing pins a
  revision once created but does not verify the bundle originated from a
  trusted issuer. Trust-domain claims currently rely on out-of-band
  checks.

### 10. Federation / Remote

HTTP bearer auth (`server/http.rs`), shared-secret JSON-RPC auth
(`server/jsonrpc.rs`). OFP is design-debt.

- **Uncertain — constant-time comparison** for the shared secret; worth
  confirming in `server/jsonrpc.rs`.
- **Partial — cross-gateway bypass prevention.** Policy enforces
  locally; no test pins the invariant against federated-agent smuggling.

### 11. Inter-Agent Messaging

Peer messaging via `agent_message` is ACL-gated
(`policy.rs:554 can_message_agent`). Auto-injection at target session
turn is enforced. `root_session_id` inheritance is correct.

No significant gaps beyond the cross-category fan-out concerns above.

## Where the gateway is NOT dumb

Twelve places where the gateway makes policy *decisions* rather than
enforcing declared rules. The top ones by structural significance:

1. **Response repair loop** (`execution.rs:1965`). The gateway retries
   and re-prompts the agent after validation failure. Smart recovery
   logic living in the gateway. A dumb gateway would reject the turn
   and let the agent decide whether to retry.

2. **Schema LLM-coercion fallback**. If `DeterministicCoercionEnforcer`
   fails, the fallback calls an LLM to reshape input. The gateway has
   become an agent. This is the single biggest violation of "dumb
   gateway."

3. **Remote-access static analyzer** (`runtime/tools/sandbox.rs:935+`,
   `runtime/remote_access.rs`). The gateway hard-codes `urllib`,
   `requests`, `socket`, `subprocess` as proxies for network intent.
   Policy invented in code, not derived from manifest.

4. **Package-manager command redirection** (`sandbox.rs:86`). Same
   concern: pip/npm detection rules baked into gateway code.

5. **Content-handle-as-path heuristic** (`sandbox.rs:107,124`). Brittle
   false-positive risk. Prefer a strict positive check — paths must
   resolve within the sandbox bind-mount layout.

6. **Loop-guard thresholds hardcoded** (`runtime/guard.rs`). Fine as
   system ceilings, but no path for an agent manifest to declare stricter
   limits within them.

7. **Tool-tier filtering based on workflow state**
   (`runtime/tools/mod.rs:79`, `runtime/lifecycle.rs`). The decision
   about which tier a tool belongs to is baked into Rust, not a
   reviewable registry.

8. **Model routing / pricing catalog**
   (`runtime/openrouter_catalog.rs`, `runtime/llm_preset_resolver.rs`).
   Catalog-fetch failure *silently disables* R-6.5 (session cost
   budget). Silent failure of an invariant is the wrong default.

Items 9–12 are minor and documented inline in the enforcement map.

## Proposed additions — `R+` rules

High-priority, starred:

- **R+1★** Structured capability scopes mandatory universally (kill
  bare-string shorthand beyond the three high-risk caps).
- **R+2★** Egress schema validation against `io.produces` on child→parent
  tool results, symmetric to ingress `io.accepts`.
- **R+3★** Spawn-chain depth cap — child `max_children` and `max_depth`
  never exceed parent's; global ceiling applies.
- **R+4★** Root-session tree budget — tokens/time/cost aggregated across
  all descendants.
- **R+5★** Approval flood cap per root session; further requests reject
  with `approval_flood`.
- **R+6★** Causal-chain fsync ordering invariant — state transitions
  gated on event durability.
- **R+7★** Runtime-lock drift check at session start.

Secondary:

- **R+8** Vault master-key presence probe at gateway startup, not on
  first access.
- **R+9** Redaction-before-write ordering invariant.
- **R+10** sandbox→gateway SDK-bridge rate and payload-size limits.
- **R+11** Bundle signature verification at `agent_revision_create`.
- **R+12** Orphan-child reaper on parent termination.
- **R+13** Approval grant TTL.
- **R+14** Deny-by-default on unknown tool names in `can_invoke_tool`.
- **R+15** Constant-time comparison for JSON-RPC shared-secret auth.
- **R+16** Promotion-gate execution denied network access.
- **R+17** Retention pruning emits `retention.pruned` causal event.
- **R+18** Canonical `docs/constitution/versions/2026.05.05/constitution.md` maintained alongside
  code.

## Methodology

- Doc extraction: 24 architecture and spec docs in `docs/`, skimmed
  `docs/design/` as secondary source.
- Code map: `policy.rs`, `sandbox.rs`, `vault.rs`, `log_redaction.rs`,
  `router.rs`, `execution.rs` (3563 lines), all of `runtime/tools/`,
  `runtime/guard.rs`, `runtime/continuation.rs`,
  `runtime/response_validation.rs`, `runtime/prompt_budget.rs`,
  `runtime/session_budget.rs`, `runtime/approved_exec_cache.rs`,
  `runtime/remote_access.rs`, `runtime/checkpoint.rs`,
  `runtime/lifecycle.rs`, `runtime/install_contract.rs`,
  `runtime/tool_call_processor.rs`, `runtime/parser.rs`, `server/*`,
  `causal_chain.rs`, `runtime_lock.rs`, `runtime/memory/*`.
- Classification: rules were marked `ENFORCED` when a concrete code path
  mechanically rejects violation, `PARTIAL` when enforcement is present
  but gapped, `MISSING` when no mechanized check exists,
  `DESIGN DEBT` when the rule is aspirational and unimplemented.

## Next steps

See `docs/gateway-constitution-roadmap.md` for the prioritized plan to
close the gaps identified here.
