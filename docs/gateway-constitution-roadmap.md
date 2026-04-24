# Gateway Constitution Roadmap

> Prioritized plan to close the gaps identified in the 2026-04-24 audit.
>
> Source documents:
>
> - `docs/gateway-constitution-audit-2026-04-24.md` — the findings.
> - `docs/gateway-constitution.md` — the canonical rule list.
>
> Every entry below has a rule ID from the constitution, a threat model,
> an implementation sketch, a test strategy, and a size estimate
> (S ≤ 1 day, M ≤ 1 week, L > 1 week).

## Definition of done

An item is done when:

1. The code change lands.
2. A test at `autonoetic-gateway/tests/constitution_<category>_<rule_id>.rs`
   fails before the change and passes after.
3. The corresponding row in `gateway-constitution.md` is updated to
   `ENFORCED` with the new file:line citation.
4. If the change affects the dumbness invariant (§14), the audit doc's
   §12 list is updated.

## Phase ordering

Priorities reflect the blast radius of a missing rule, not the
difficulty of the fix. A P0 item is one where an abusive or broken
agent today can violate a constitutional invariant in a way that is
invisible, unbounded, or hard to revert.

---

## Phase 1 — P0 invariants (close this quarter)

### 1.1 `R+6` Causal-chain fsync ordering invariant

**Threat.** The gateway acknowledges an approval (continuation resumes,
grant activates, promotion commits) while the corresponding causal
event is still in the page cache. A crash between ack and flush loses
the audit trail of a privileged action — the worst possible class of
audit bug.

**Sketch.** Wrap `causal_chain::log` so that any caller can request a
durable append (`log_durable`) that returns only after `fsync`.
Transition state-mutating paths (approval resolve, grant insert,
promotion commit, emergency stop) to `log_durable`. Leave hot-path
informational events on the non-durable path.

Files: `autonoetic-gateway/src/causal_chain.rs`,
`autonoetic-gateway/src/runtime/continuation.rs`,
`autonoetic-gateway/src/runtime/tools/approval.rs`,
`autonoetic-gateway/src/runtime/tools/promotion.rs`,
`autonoetic-gateway/src/execution.rs::emergency_stop_root_session`.

**Test.** `constitution_audit_fsync_ordering.rs` — simulate a crash
after `log_durable` returns but before the SQLite commit, verify
replay reconstructs state. Use a filesystem mock or `fs::sync_all` +
kill-child pattern.

**Size.** M.

---

### 1.2 `R+5` Approval flood cap

**Threat.** A rogue or buggy agent emits an unbounded stream of
distinct approval requests (each with a fresh fingerprint, defeating
the dedup cache). The operator's approval queue is DoSed; every
session sharing that operator is effectively wedged.

**Sketch.** `gateway_store/approvals.rs` — on insert, count pending
approvals for the root session. If ≥ configured cap (default 50),
reject the insert with `approval_flood` error. The agent receives the
error immediately; no operator notification.

Expose the cap in `config.rs` as `max_pending_approvals_per_root_session`.

Files: `autonoetic-gateway/src/config.rs`,
`autonoetic-gateway/src/gateway_store/approvals.rs`,
`autonoetic-gateway/src/runtime/tools/sandbox.rs` (handle error),
`autonoetic-gateway/src/runtime/tools/credential.rs`,
`autonoetic-gateway/src/runtime/tools/artifact_exec.rs`.

**Test.** `constitution_abuse_approval_flood.rs` — spawn an agent that
requests N+1 distinct approvals; assert request N+1 fails with
`approval_flood` and that approvals 1..N remain pending.

**Size.** S.

---

### 1.3 `R+3` Spawn-chain depth cap

**Threat.** `AgentSpawn.max_children` bounds fan-out at each node but
not chain length. A → B → C → D → … can recurse indefinitely until
OOM or budget trips. Budget-aware attackers stay under per-session
limits by fanning deep instead of wide.

**Sketch.** Add `max_spawn_depth` to `AgentSpawn` capability, and a
system-wide ceiling in `config.rs` (default 8). On `agent.spawn`,
traverse the parent chain, reject if the new depth exceeds parent's
cap or the system ceiling. Store `spawn_depth` on the session record
for cheap lookups.

Files: `autonoetic-types/src/capability.rs`,
`autonoetic-gateway/src/policy.rs::can_spawn_agent`,
`autonoetic-gateway/src/runtime/tools/agent.rs`,
`autonoetic-gateway/src/gateway_store/sessions.rs`.

**Test.** `constitution_abuse_spawn_depth.rs` — spawn to depth ceiling,
assert next spawn fails with `spawn_depth_exceeded` and that the
failure is recorded in the causal chain.

**Size.** M.

---

### 1.4 `R+4` Root-session tree budget

**Threat.** Per-session budgets mean a parent that spawns 10 siblings
legally spends 10× any individual cap. Cost, token, and wall-clock
budgets cease to bound the aggregate.

**Sketch.** Add `RootSessionBudgetRegistry` keyed by `root_session_id`,
holding the sum of tokens / tool invocations / wall-clock / price
across all descendants. Each session registers at creation and
deregisters at close. `check_pre_llm` hits both per-session and
per-tree caps; tighter wins.

Configure via `root_session_budget_defaults` in `config.rs` (separate
from per-session defaults, which are typically tighter per-node).

Files: `autonoetic-gateway/src/runtime/session_budget.rs` (extend or
mirror into `root_session_budget.rs`),
`autonoetic-gateway/src/runtime/lifecycle.rs`,
`autonoetic-gateway/src/config.rs`.

**Test.** `constitution_budget_root_session_tree.rs` — spawn 5
children, each at 30% per-session budget; assert the 4th child exceeds
the 100% tree budget and is denied `pre_llm` before running.

**Size.** L. Non-trivial because it interacts with `record_llm_completion`
across parallel turn loops — needs careful locking.

---

### 1.5 `R+2` Egress schema validation on tool results

**Threat.** Ingress validates messages into children; egress does not
validate the child's final reply against any `io.produces` schema.
A malformed child response silently poisons the parent's conversation,
and the parent's LLM reasons over garbage.

**Sketch.** The response-validation gate already knows how to check
against a schema; the missing piece is wiring `io.produces` from the
child's manifest into the validation call on the *parent* side of the
spawn return. Reject invalid responses with a tool error containing
the child's schema mismatch; parent decides whether to retry.

Files: `autonoetic-gateway/src/runtime/response_validation.rs`,
`autonoetic-gateway/src/runtime/tools/agent.rs` (spawn return handler),
`autonoetic-gateway/src/execution.rs` (repair loop bypass — this is
*agent* repair, not gateway repair; see §3).

**Test.** `constitution_schema_egress.rs` — spawn a child whose
manifest declares `io.produces: { schema: ... }`; have the child emit
a response that violates it; assert the parent receives a structured
tool error, not the malformed response.

**Size.** M. Biggest risk is regression on existing spawn behavior
where child manifests don't declare `io.produces` (fail-open for
missing schema, fail-closed for mismatch against declared schema).

---

### 1.6 `R++1` Signed turn-boundary state attestation

**Threat.** LLM reasoning state can diverge from gateway ground truth
across turns. The agent sees tool errors in its conversation history
and *reasons* about them, but the model's internal belief about
remaining budget, active capabilities, or pending approvals is shaped
by the conversation itself — which the model also shapes. An agent
can confidently plan for many turns on false premises before hitting
a contradiction. Gateway enforcement is correct on the record; agent
behaviour is built on fiction.

**Sketch.** At each turn boundary, the gateway composes a compact JSON
state block and signs it with a per-gateway ed25519 key:

```json
{
  "session_id": "...",
  "root_session_id": "...",
  "turn": 17,
  "remaining": {"llm_rounds": 8, "tools": 42, "wall_clock_s": 1800, "price_usd": 0.71},
  "capabilities_active": ["ReadAccess:fs/...", "NetworkAccess:..."],
  "pending_approvals": 0,
  "spawn_depth": 2,
  "emergency_stop_armed": true,
  "signature": "ed25519:..."
}
```

The block is injected as a dedicated message role (or system-prompt
tail section) at each turn start. The agent's foundation prompt teaches
it that this is authoritative and its own recollection of these facts
is not. Agents may and should consult it before acting.

Files: new `autonoetic-gateway/src/runtime/state_attestation.rs`,
`autonoetic-gateway/src/runtime/lifecycle.rs` (inject before LLM call),
`autonoetic-gateway/src/runtime/foundation_core.md` (teach the block),
`autonoetic-gateway/src/crypto.rs` (key material).

**Test.** `constitution_attestation_signed.rs` — parse the block,
verify signature against gateway pubkey, tamper one field, verify
reject. `constitution_attestation_freshness.rs` — assert block
reflects current turn state (monotonic turn counter, accurate remaining
budget after a round).

**Size.** L. Non-trivial because it touches prompt composition, key
management, and foundation docs. The agent-side adoption (actually
consulting the block instead of memory) is itself a longer migration
but begins once the block is authoritatively present.

---

### 1.7 `R++2` Capability-delta gating at promotion

**Threat.** Capability accretion by stealth. R-8.10 detects drift in
`promotion_history` after the fact; nothing *prevents* the canonical
failure mode of "one more capability per revision, each PR reasonable
in isolation, result after six revisions is god-mode." Detection
without prevention is how privileged systems erode.

**Sketch.** At `agent.revision.promote`, compute
`cap_set(candidate) \ cap_set(current_active_revision)`. If the delta
is non-empty, create a distinct approval class
`ApprovalClass::CapabilityAccretion` whose payload names each added
capability explicitly. The operator's approval dialog for this class
is structurally different — per-capability checkboxes, with the delta
visible — from the ordinary promotion approval. If the delta is empty,
the existing approval path applies.

This is the archetypal "add a rule, not logic" change: the gateway
performs a set diff, and that diff shapes an approval payload. No
reasoning.

Files: `autonoetic-gateway/src/runtime/tools/agent_revision.rs::promote`,
`autonoetic-gateway/src/runtime/tools/approval.rs` (new class),
CLI/TUI approval renderers.

**Test.** `constitution_promotion_capability_delta.rs` — promote with
identical caps, assert ordinary approval; promote adding
`NetworkAccess`, assert the approval payload names it; attempt to
approve without explicitly confirming the delta entries, assert
rejection.

**Size.** M.

---

### 1.8 `R++5` Tool-call intent capture

**Threat.** Post-hoc audit of what-happened is much harder than it
needs to be. Today we see "spawned coder with message X"; we don't
see "spawned coder because the planner believed this was a refactor,
not a redesign." The *why* is buried in the LLM's reasoning tokens
which are not retained. For compliance-grade review, a cheap
natural-language intent per call closes most of the gap.

**Sketch.** Add an `intent` field to every tool call's arguments
(string, max 500 chars). For privileged tool classes
(`sandbox.exec`, `credential.*`, `agent.spawn`, `agent.revision.*`,
`scheduler.*`), missing intent is a hard rejection. For non-privileged
tools it is optional but strongly encouraged via foundation-prompt
guidance. The field is persisted verbatim to the causal chain event
payload.

Files: `autonoetic-gateway/src/runtime/parser.rs` (tool-call
parsing — accept/validate field), `runtime/tool_call_processor.rs`
(enforce presence for privileged classes), all tools under
`runtime/tools/` (thread intent into causal event), foundation prompts
(teach the field).

**Test.** `constitution_audit_intent_captured.rs` — invoke
`sandbox.exec` without intent, assert reject with `intent_required`;
invoke with intent, assert it lands in the `tool.invoked` causal event
verbatim.

**Size.** S–M. Touches many files but each edit is tiny.

---

### 1.9 `R+++3` Rule-ID references in every causal event

**Threat (structural).** The gateway decides things; the causal chain
records *that* it decided; it does not record *which rule* the decision
was made under. This gap has three consequences: (a) operators and
auditors cannot answer "which rule rejected this call?" without reading
code; (b) rules that are never referenced in a year of causal events
may be dead code, but there is no way to detect that; (c) a tool call
accepted without referencing any rule is a code path not covered by
the constitution, but we cannot detect those either. This is also the
mechanical back-stop for the dumbness invariant: no ID = no rule =
the gateway just did something of its own volition.

This is also Ri-0.3's enforcement mechanism — every rejection names
the rule that caused it, not just a generic permission error.

**Sketch.** Add `enforced_rules: Vec<RuleId>` to the causal event
payload schema. At every decision site (policy engine, approval
gate, budget check, schema validation, sandbox isolation decision),
callers pass the rule ID(s) being enforced. A helper
`enforce_under_rule(rule_id, condition)` makes this ergonomic and
auditable.

Dead-rule detection: a periodic report queries `causal_events` for
rule IDs referenced in the last N days and compares against the
constitution's full rule list. Rules absent from the report are
flagged for retirement review.

Gap detection: the property test from R++9 checks that every accept
/ reject in a representative event trace carries a non-empty
`enforced_rules`.

Files: `autonoetic-gateway/src/causal_chain.rs` (schema),
`autonoetic-gateway/src/policy.rs` (thread rule IDs through each
check), every tool under `runtime/tools/` (pass rule IDs into
emitted events), `runtime/response_validation.rs`.

**Test.** `constitution_audit_rule_id_coverage.rs` — invoke a sample
of accepts and rejects across tool classes, parse the resulting
causal events, assert every event has `enforced_rules` non-empty
with valid IDs.

**Size.** M. Mechanical but touches many sites.

---

### 1.10 `Ri-0.10` `constitution.read` tool — agents can read their law

**Threat.** An agent that cannot read the constitution it is operating
under cannot meaningfully consent to it, reason about its obligations,
or propose amendments to it. Ri-0.8 (right to propose amendments) is
hollow without Ri-0.10.

**Position in Phase 1.** Cheap, foundational, on the critical path.
Do this **before** R+++1 (amendment channel), because an agent
submitting an amendment proposal to a rule it cannot read is going
through motions.

**Sketch.** New native tool `constitution.read`:

- `args`: optional `section` (e.g. `"Ri-0.10"`, `"R-7.5"`, `"§0"`) —
  if omitted, returns the whole document.
- `returns`: `{ text, digest, version, retrieved_at }`.

Tool is default-available (no capability gate): reading the law is a
right, not a privilege. Returns the authoritative text loaded from
the gateway's bundled constitution file plus the precomputed
`constitution_digest` (which becomes live when R+++2 lands).

Foundation prompts teach agents: "the constitution is your
contract. Use `constitution.read` to consult it before proposing
amendments, when a rule ID appears in an error, or any time you need
to understand your obligations."

Files: new
`autonoetic-gateway/src/runtime/tools/constitution.rs`,
`runtime/tools/mod.rs` (register), foundation prompts.

**Test.** `constitution_right_ri_0_10.rs` — agent without any
special capability invokes `constitution.read()` and receives the
full text; invokes with a section selector and receives the scoped
text; the returned `digest` matches the compile-time digest constant.

**Size.** S.

---

### 1.11 `R+++1` Amendment proposal channel for agents

**Threat (structural — actually an enabling change).** Today the
constitution can only be amended by humans writing PRs. If the
project's vision is agents free, responsible, *and cooperative*,
then agents must be able to participate in the rule system, not
merely be subjects of it. Without a declared channel, agents can
observe problems (via causal chain queries) but cannot formally
request change — which means the constitution adapts only at human
speed, not at the speed the system itself learns.

This is Ri-0.8's enforcement mechanism.

**Sketch.** New tool `constitution.propose_amendment` with args:

- `kind`: `add_rule` | `modify_rule` | `remove_rule` | `add_right` | `modify_right` | `remove_right`
- `target_id`: rule or right ID (for modify / remove)
- `proposed_text`: new text (for add / modify)
- `justification`: free-form, required
- `evidence`: list of causal event IDs or execution trace IDs the
  agent cites

Requires a new capability: `ConstitutionalProposal` — high-risk,
scope-object required. Candidate holders: `auditor`,
`security-sentinel`, `evolution-steward`, a dedicated
`constitutional-scribe`. Not a default capability.

Persistence: new `constitutional_proposals` SQLite table with
columns `(id, proposer_agent_id, kind, target_id, proposed_text,
justification, evidence_json, status, operator_decision,
decision_reason, created_at, decided_at, published_in_release)`.
Status lifecycle: `pending → under_review → (approved | rejected |
deferred)`.

CLI: `autonoetic constitution proposals list | show | approve |
reject | defer`, mirroring the approval CLI shape.

Approved proposals are *queued for the next release*; they do not
immediately modify the constitution. A release applies a batch of
approved proposals, updates the constitution file, bumps the
`constitution_digest` (R+++2), and is itself a human-signed
operation.

Files: new
`autonoetic-gateway/src/runtime/tools/constitution.rs`, new
`autonoetic-gateway/src/gateway_store/proposals.rs`, extensions to
CLI, new capability in `autonoetic-types/src/capability.rs`,
foundation prompts to teach the tool.

**Test.** `constitution_rights_amendment_proposal.rs` — agent
without capability cannot invoke the tool; agent with capability
invokes, proposal persisted with durable ID, operator approves,
proposal enters queued state, second test verifies queued
proposals apply at next release.

**Size.** L. New tool surface, new persistence, CLI, capability,
release mechanics. This is the largest Phase 1 item but
foundational to the vision.

---

## Phase 2 — P1 hardening (close next quarter)

### 2.1 `R+7` + `R+18` Runtime-lock drift check

**Threat.** A session pinned to a specific `runtime.lock` resumes
after the gateway binary has been upgraded. The reproducibility
guarantee breaks silently.

**Sketch.** At session start (new or resumed from checkpoint), compare
the recorded lock's `binary_sha256` with the current gateway's build
SHA. If divergent, refuse to start and emit `runtime_lock_drift`. An
operator flag allows override per session with explicit audit.

Files: `autonoetic-gateway/src/runtime_lock.rs`,
`autonoetic-gateway/src/runtime/lifecycle.rs`.

**Test.** `constitution_audit_runtime_lock_drift.rs` — checkpoint with
one SHA, mutate build-time SHA constant, resume, assert refusal.

**Size.** S.

---

### 2.2 `R+9` Redaction-before-write ordering

**Threat.** A raw payload (tool args, LLM completion, error string)
lands in the JSONL file before `redact_text_for_logs` runs. Even if
redaction later overwrites, the raw bytes existed on disk for a window.

**Sketch.** Type-level: `causal_chain::log` accepts only
`RedactedPayload` — a newtype that wraps `redact_text_for_logs`'s
output. Callers must redact before calling; the compiler prevents raw
`String`s from being passed.

Files: `autonoetic-gateway/src/log_redaction.rs` (add newtype),
`autonoetic-gateway/src/causal_chain.rs` (change signature), plus the
~40 call sites (mechanical edit).

**Test.** `constitution_secret_redaction_ordering.rs` — static test:
grep for any `causal_chain::log(raw_string)` pattern, expect zero.
Plus a runtime test that emits a secret-shaped value through a tool
call and verifies the JSONL on disk never contains the raw form.

**Size.** M. Mostly mechanical, but touches many files.

---

### 2.3 `R+11` Bundle signature verification

**Threat.** Content-addressing pins a bundle *once created* but does
not verify authenticity. Any party with write access to the revision
creation surface can inject a malicious revision that will then be
trusted by content-digest downstream.

**Sketch.** Add a `signature` field to the revision-create input,
verify against a configured trust-root at the time of
`agent.revision.create`. Reject unsigned or invalid bundles unless a
`trust_local` config flag is set (dev mode only).

Sign with existing Rust crypto (ed25519, `ring` or `ed25519-dalek`).
Key material configured via `AUTONOETIC_SIGNING_PUBLIC_KEYS` (path to
PEM or JSON key set).

Files: new `autonoetic-gateway/src/crypto/signatures.rs`,
`autonoetic-gateway/src/runtime/tools/agent_revision.rs`,
`autonoetic-gateway/src/config.rs`.

**Test.** `constitution_install_signature.rs` — create with valid
signature, assert pass; tamper one byte, assert reject.

**Size.** M.

---

### 2.4 `R+15` Constant-time shared-secret comparison

**Threat.** Timing attacks against JSON-RPC auth. Low-risk against an
unprivileged adversary with no local access, but the cost to fix is
trivial.

**Sketch.** Replace `==` on the shared secret in
`server/jsonrpc.rs` with `subtle::ConstantTimeEq`.

Files: `autonoetic-gateway/src/server/jsonrpc.rs`.

**Test.** `constitution_federation_constant_time.rs` — a read-only
test verifying the comparison site uses `subtle::ct_eq`. Static
analysis via grep assertion is fine; property-level timing tests are
out of scope.

**Size.** S.

---

### 2.5 `R+10` Sandbox → gateway SDK-bridge limits

**Threat.** A sandboxed process makes unbounded or oversized
`dispatch_sdk_method` calls (`events.emit`, `memory.remember`,
`state.checkpoint`). Floods the gateway or balloons the content store.

**Sketch.** Per-session rate limit (e.g., 100 SDK calls/sec) and
per-call payload size cap (e.g., 1 MiB). Hits drop or error the call
and log `sdk_bridge_abuse` to causal chain.

Files: `autonoetic-gateway/src/sandbox.rs::dispatch_sdk_method`.

**Test.** `constitution_abuse_sdk_bridge.rs` — hammer the bridge
in-process, assert rate-limited calls return `rate_limited` and
oversized payloads return `payload_too_large`.

**Size.** M.

---

### 2.6 `R+12` Orphan-child reaper

**Threat.** Parent session crashes or is emergency-stopped; children
run on, consuming budget and eventually reporting to a dead parent.

**Sketch.** Scheduler tick scans `sessions` for parents in terminal
states with live children. Cancel each child with yield reason
`parent_terminated`, record in causal chain.

Files: `autonoetic-gateway/src/scheduler.rs`,
`autonoetic-gateway/src/runtime/lifecycle.rs`.

**Test.** `constitution_lifecycle_orphan_reaper.rs` — start parent +
child, kill parent mid-turn, advance scheduler, assert child reaches
`Cancelled(parent_terminated)` within one tick.

**Size.** M.

---

### 2.7 `R+1` Structured scopes for all capabilities

**Threat.** Bare-string shorthand for low-risk capabilities is a soft
path for underdeclaration. Auditing a manifest requires reading Rust
to know what a bare string means.

**Sketch.** Extend the rejection in `capability_from_shorthand` from
the three high-risk caps to all capabilities. Provide a migration
pass that auto-expands existing manifests with explicit scopes at
load time, logging a warning.

Files: `autonoetic-gateway/src/runtime/tools/agent_revision.rs`,
agent manifests under `agents/`.

**Test.** `constitution_capability_scope_required.rs` — a manifest
with bare-string `ReadAccess: fs` fails load.

**Size.** M. Risk is breaking existing manifests — the migration pass
is what makes this S vs. L.

---

### 2.8 `R+16` Promotion-gate execution denied network

**Threat.** An auditor or evaluator that hits the network during a
verdict is not reproducible from recorded evidence. A malicious
auditor could exfiltrate bundle contents.

**Sketch.** When the promotion gate runs evaluator/auditor sessions,
force their sandbox config to `--unshare-net` regardless of their
declared capabilities for the duration of the verdict. Network access
during a promotion-gate session is a hard error.

Files: `autonoetic-gateway/src/runtime/tools/agent_revision.rs::promote`,
`autonoetic-gateway/src/sandbox.rs::BwrapIsolationOverrides`.

**Test.** `constitution_promotion_no_network.rs` — promote with an
evaluator that has `NetworkAccess` declared; assert its sandbox has
network namespace unshared; assert a network call inside returns
ECONNREFUSED.

**Size.** S.

---

### 2.9 `R++3` Distinct auditor / evaluator identity at promotion

**Threat.** Today's gate (R-2.8) requires both evaluator and auditor
records but does not require their `agent_id` to differ. A single
compromised specialist holding both capabilities can self-approve.

**Sketch.** In `agent.revision.promote`, load both promotion records
and assert `evaluator.agent_id != auditor.agent_id`. Session identity
is not sufficient — agent identity must differ. Reject with
`gate_identity_overlap` otherwise.

Files: `autonoetic-gateway/src/runtime/tools/agent_revision.rs::promote`,
`autonoetic-gateway/src/runtime/promotion_store.rs` (identity read).

**Test.** `constitution_promotion_distinct_identity.rs` — record both
passes with the same `agent_id`, attempt promote, assert rejection.

**Size.** S.

---

### 2.10 `R++4` Operator approval hardening

**Threat.** Approval fatigue. A distracted operator clicking through
50 near-identical prompts is the real trust boundary, and today there
is nothing between "prompt displayed" and "prompt approved." High-risk
approvals get the same UX affordances as low-risk ones.

**Sketch.** Three sub-changes, each a new field on the approval record:

1. `min_dwell_ms`: how long the operator must see the prompt before
   the confirm action enables. Set non-zero for bundle promotions,
   new-credential registration, first-ever host approvals. CLI and TUI
   respect it.
2. `typed_confirm_phrase`: a string the operator must type to confirm
   (e.g. `promote research-v7`). Set for destructive classes.
3. Operator-facing structural-similarity dedup: hash the rendered
   human-readable *description* (not the gateway fingerprint) and
   suppress N consecutive near-duplicates in the operator view with a
   banner ("N similar approvals — expand to review"). Gateway-side
   dedup (R-2.3) is unchanged.

Files: `autonoetic-gateway/src/gateway_store/approvals.rs` (schema),
`autonoetic/src/bin/` CLI, TUI, approval signal dispatch.

**Test.** Three tests, one per sub-part.
`constitution_approval_dwell.rs`,
`constitution_approval_typed_confirm.rs`,
`constitution_approval_operator_dedup.rs`.

**Size.** M.

---

### 2.11 `R++6` Degraded session mode

**Threat.** The response to agent misbehaviour today is binary:
healthy or emergency-stopped. A session showing loop-guard warnings
short of trip, or a sandbox accumulating denied escape syscalls (R++8),
has no landing zone between the two. Either we tolerate growing
badness or we kill an in-flight task.

**Sketch.** Add a `SessionState::Degraded` status. In degraded mode:

- Tool-tier filter clamps to `Core` only (hides Workflow + Specialized).
- `NetworkAccess`, `CodeExecution`, `AgentSpawn` are refused
  regardless of manifest declaration.
- Loop-guard budgets are tightened (configurable ratio, default 0.25).
- The agent can still reason and write memory/knowledge — useful for
  recording self-diagnosis.

Entry is triggered by (a) loop-guard sub-trip warnings, (b) R++8
escape-attempt threshold, or (c) explicit operator
`session.degrade(session_id, reason)`. Exit requires operator
`session.clear_degradation(session_id)` — degraded mode does not
self-heal.

Files: `autonoetic-gateway/src/runtime/lifecycle.rs` (state machine),
`autonoetic-gateway/src/runtime/guard.rs` (sub-trip triggers),
`autonoetic-gateway/src/runtime/tools/mod.rs::ToolTierFilter`,
`autonoetic-gateway/src/runtime/tools/session.rs` (new subtools).

**Test.** `constitution_abuse_degraded_mode.rs` — trigger degrade via
loop guard, attempt `sandbox.exec`, assert reject with
`session_degraded`; operator clears, tool works.

**Size.** M.

---

### 2.12 `R++7` Cross-gateway causal continuity

**Threat.** Federation with independent causal chains means
reconstructing a cross-gateway interaction requires correlating two
chains out-of-band. The whole point of federation-plus-causal-chain is
end-to-end audit; today that property does not hold.

**Sketch.** Two additions to OFP:

1. Cross-gateway events (agent-to-agent messages, remote spawns,
   remote credential requests) embed a `peer_event_ref: { gateway_id,
   event_id, entry_hash }` in their payload. Both sides log matching
   `peer_event_ref`s, enabling a bidirectional join.
2. Gateways periodically exchange signed `chain_attestation` digests:
   `(gateway_id, chain_prefix_hash_at_event_N, signature)`. A verifier
   holding two chains and the attestations can confirm that the view
   each side presents of the other is consistent.

Files: `autonoetic-ofp/`, `autonoetic-gateway/src/causal_chain.rs`
(peer ref support), `autonoetic-gateway/src/server/ofp.rs`.

**Test.** `constitution_federation_causal_continuity.rs` — two local
OFP endpoints, round-trip a message, assert peer refs present on both
sides, assert attestation verifies, tamper with one side's chain,
assert verification fails.

**Size.** L. Touches the federation protocol surface.

---

### 2.13 `R+++2` Constitution digest + compatibility handshake

**Threat (structural).** For federation to deliver "cooperation under
shared law," gateways must verify they are operating under
compatible constitutions before trusting each other's agents.
Without this, federation is a hope rather than a mechanism: gateway
A might enforce rules gateway B does not, and a cross-gateway agent
interaction silently lands in the weaker regime.

**Sketch.** Each gateway publishes a `constitution_digest`:

```
digest = SHA256(
  canonical_text(constitution.md)
  || canonical_json(rule_id_to_enforcement_citation_table)
  || canonical_json(right_id_to_enforcement_citation_table)
)
```

The digest is computed at build time (reproducible: same constitution
→ same digest) and exposed via `gateway.info`. Cross-gateway
requests (OFP messages, remote spawns, remote credential requests)
include the sender's digest.

Compatibility check on the receiving gateway:

1. **Exact match** → trusted peer, proceed.
2. **Known-compatible set** → the receiving gateway has a declared
   list of digests it considers equivalent (e.g., minor-version
   bumps that only added rules). Proceed.
3. **Constitutional superset** → a policy declaring "I accept peers
   whose constitution is a strict superset of mine" (the receiving
   gateway enforces strictly less; the peer enforces everything I
   do and more). Requires rule-ID-level comparison, not just digest.
4. **Otherwise** → reject with `constitutional_incompatibility`.

Both digests are embedded in the causal event for the interaction,
so audit is end-to-end verifiable.

Files: new `autonoetic-gateway/src/constitution_digest.rs`,
`build.rs` (compute at build time),
`autonoetic-gateway/src/server/ofp.rs` (attach digest to
cross-gateway requests), `autonoetic-ofp/` (protocol extension),
causal event schema.

**Test.** `constitution_federation_digest_handshake.rs` — two
in-process OFP endpoints with matching digests round-trip
successfully; mutate one endpoint's digest, assert subsequent
cross-gateway requests reject with `constitutional_incompatibility`
and both digests are recorded on both sides.

**Size.** L. Federation protocol extension; needs careful handling
of the reproducible-digest computation.

---

### 2.14 §0 Rights audit — early bucket (test-only pins)

**Threat.** Rights already enforced under the rule framing need
dedicated tests named `constitution_right_<ri_id>.rs` to pin them *as
rights from the agent's perspective*. A right without a test is a
lie. This bucket lands early because it requires no new code.

| Right | Work |
|---|---|
| Ri-0.2 causal chain read | Test: unprivileged agent reads its own trace successfully; cannot read another agent's trace without capability. |
| Ri-0.7 session.end | Test: agent calls `session.end`, gateway commits outstanding events and closes cleanly; cannot be refused. |
| Ri-0.11 non-repudiation | Test: every causal event carries the acting `agent_id`; hash-chain integrity detects tampering; actions cannot be reattributed. |

**Size.** S. All three tests in parallel, one evening.

---

### 2.15 §0 Rights audit — mid bucket (small additions)

For rights that need one small piece of new code plus a test.

| Right | Work |
|---|---|
| Ri-0.6 no silent capability reduction | Declare the closed set of legitimate narrowing paths (rule-driven via R++6 degraded mode, operator-driven via explicit command). Invariant test asserts capability set at turn N+1 is a subset of turn N only via declared paths, with a causal event for each narrowing. |
| Ri-0.12 continuity — closed list of termination reasons | Audit every `lifecycle.rs` termination path, enumerate, document, refactor so every exit calls a single `terminate(reason, rule_id, evidence)` helper. Test: fuzz inputs, no termination occurs outside the declared set. |

**Size.** M. Ri-0.12 is the larger piece — requires refactoring
termination paths — but once done, I-9 (every termination attributed
to one declared reason) is mechanically enforced.

---

### 2.16 §0 Rights audit — late bucket (depends on R++ / R+++ items)

For rights whose enforcement mechanism is itself a Phase 1/2 item.
These tests follow the upstream work.

| Right | Depends on | Work |
|---|---|---|
| Ri-0.1 self-inspection | R++1 attestation (#48) | Right-level test: agent reads its own state via attestation, values match internal registry. |
| Ri-0.3 named rejection reason | R+++3 rule-ID refs (#91) | Test: every rejection across a sample of tools carries a rule ID resolvable to the constitution. |
| Ri-0.4 truthful budget | R++1 (#48) | Test: budget values in attestation match `session_budget.rs` registry in real time across a multi-turn trace. |
| Ri-0.5 degradation notice | R++6 (#61) | Add notice-on-entry to degraded mode. Test: agent receives notice with rule ID + evidence before its next turn. |
| Ri-0.8 amendment proposal | R+++1 (#92) | Test: capability-holder submits, durable ID returned, proposal reviewable; non-holder rejected. |
| Ri-0.9 last-word before terminal | R++6 (#61) + emergency-stop | Add notify-where-practical to degrade/emergency-stop paths. Test: agent receives a notification opportunity where not foreclosed by the trigger. |

**Size.** M.

---

### 2.17 `Ri-0.13` Private-under-law reasoning + `ReasoningAudit` capability

**Threat.** Without this right, the gateway's relationship to agent
reasoning is undefined — and an implementation that quietly starts
using CoT as a policy input would be undetectable. The three-part
structure (not-gated, recorded, capability-disclosed) needs explicit
enforcement, not just documentation.

**Sketch.** Three independent pieces:

**(a) Not-gated on reasoning content (Ri-0.13a, I-8).** This is
already structurally true in the dumbness invariant. Pin it with a
test that exercises every policy decision site with inputs that
include (i) a benign tool call with adversarial CoT, (ii) the same
tool call with empty CoT. Assert the gateway's verdict is identical
in both cases. Catches any future code that starts reading CoT.

**(b) Reasoning recorded to causal chain (Ri-0.13b).** Already
happens — LLM completions include reasoning and land in events.
Confirm with a test that CoT tokens are preserved verbatim in the
`llm.completed` event's payload, subject to standard redaction.

**(c) Capability-gated disclosure (Ri-0.13c).** New capability
`ReasoningAudit`, scoped by target agent pattern. New tool
`observability.read_reasoning` that fetches reasoning traces for the
target — gated by the capability and **writes a disclosure event
visible to the reviewed agent** listing who read what and when.
Holders: auditor, security-sentinel, evolution-steward, operator
(via bypass-with-audit).

Files: new `autonoetic-gateway/src/runtime/tools/observability.rs`
(extension), `autonoetic-types/src/capability.rs`
(`ReasoningAudit` variant), causal event schema
(`reasoning.disclosed` event), `policy.rs`
(`can_audit_reasoning`).

**Test.**
- `constitution_right_ri_0_13a_not_gated.rs` — adversarial CoT vs
  empty CoT produce identical verdicts.
- `constitution_right_ri_0_13b_recorded.rs` — CoT survives redaction
  in the causal event.
- `constitution_right_ri_0_13c_disclosure.rs` — non-capability
  holder cannot read reasoning; holder can, and the reviewed agent
  sees a `reasoning.disclosed` event naming the reader.

**Size.** M. (b) is already working and only needs a test; (a) is a
test; (c) is a new tool + capability + disclosure event.

---

## Phase 3 — P2 polish

### 3.1 `R+8` Vault master-key presence probe at startup

Fail fast at gateway boot rather than on first secret access. S.
`autonoetic-gateway/src/vault.rs` + `bootstrap.rs`.

### 3.2 `R+13` Approval grant TTL

Default 24h per grant. Expiry re-opens the approval gate.
`session_approval_grants` schema + cleanup tick. M.

### 3.3 `R+14` Deny-by-default on unknown tool names

Audit `policy.can_invoke_tool` to ensure unknown names fail shut. If
already correct, add an explicit test pinning the behavior. S.

### 3.4 `R+17` `retention.pruned` causal events

Emit a single event per prune batch with counts and bounds. S.
`autonoetic-gateway/src/causal_chain/rotation.rs`.

### 3.5 `R++8` Sandbox-escape-attempt accounting

**Threat.** Seccomp denies syscalls at the kernel; the gateway never
sees them. A misbehaving agent that probes for an escape looks
identical from the outside to a healthy agent.

**Sketch.** Configure seccomp profiles in bubblewrap with `SCMP_ACT_LOG`
(or `SCMP_ACT_ERRNO` + audit subsystem) for high-value syscalls:
mount, ptrace, kexec, unshare reuse, `/proc/self/exe` writes. Parse
audit log per session, maintain a counter. Crossing a threshold
(default N=5) triggers R++6 degraded mode; a second threshold
(default N=20) triggers emergency stop. Docker/microvm paths parse the
equivalent runtime-level security events.

Files: new
`autonoetic-gateway/src/sandbox/seccomp_audit.rs`,
`autonoetic-gateway/src/sandbox.rs` (profile wiring),
`autonoetic-gateway/src/runtime/lifecycle.rs` (threshold → degrade).

**Test.** `constitution_sandbox_escape_accounting.rs` — run a
sandboxed script that issues denied `mount()` calls; assert the
counter increments; at threshold assert the session transitions to
degraded.

**Size.** L. Seccomp profile engineering is delicate and platform-
specific.

---

### 3.6 `R++10` Unified fail-mode table

**Threat.** Per-invariant failure handling is ad-hoc. Vault key
missing → ? fsync fails → ? causal-chain hash mismatch mid-session
→ ? OpenRouter catalog down → silently disabled (R-6.5, the archetype
to fix). The silent-disable pattern is how invariants die.

**Sketch.** One new module
`autonoetic-gateway/src/invariant_failures.rs` with an enum of actions
(`RefuseBoot`, `RefuseSessionStart`, `Degrade`, `EmergencyStop`,
`LogOnly`) and a static table mapping every constitutional rule ID to
its declared failure action. A central `on_invariant_failure(rule,
context)` helper reads the table and performs the declared action.
Existing silent-disable sites are refactored to call it.

A contract test asserts the table has an entry for every rule ID in
`gateway-constitution.md` — drift between docs and code becomes a
test failure.

Files: new `invariant_failures.rs`, refactor
`runtime/openrouter_catalog.rs` (R-6.5) as the reference conversion,
plus the handful of sites identified in §12 of the audit.

**Test.** `constitution_fail_mode_table_complete.rs` — parse the
constitution rule list, assert table coverage.

**Size.** M.

---

### 3.7 Test-pin partial rules

Several rules are marked `PARTIAL` because enforcement exists but no
test pins the invariant. Add tests for:

- R-2.11 approval timeout
- R-2.14 `user.ask` blocked during pending approvals
- R-3.7 sandbox resource limits (docker/microvm paths)
- R-5.11 uniform error envelope (shared helper + contract test)
- R-6.14 `EmergencyStop` never auto-resumes
- R-6.17 checkpoint retention pruning
- R-8.6 retention policy application
- R-10.7 cross-gateway approval bypass prevention

Total size for the batch: M. Each individual test is S.

---

## Phase 4 — Architectural cleanup (§12 dumbness violations)

These require RFCs before implementation. Each item here is a policy
question, not just a code change.

### 4.1 Response repair loop (`execution.rs:1965`)

**Decision required.** Should the gateway repair agent output
automatically, or reject and let the agent retry?

Recommendation: make repair opt-in per-agent via manifest
(`response_contract.repair.auto: bool`, default `false`). When
disabled, validation failure returns a structured error, the agent
decides whether to retry. Cap auto-repair attempts at a declared
value within a system ceiling.

Size: L (RFC + implementation + migration for agents currently relying
on auto-repair).

### 4.2 Schema LLM-coercion fallback

**Decision required.** Disable LLM coercion in gateway entirely?

Recommendation: yes. Either deterministic coerce succeeds or the
agent receives a schema error. If automated repair is desired, run
it as a capability-bound specialist agent (`schema_repair` or similar)
that the parent can spawn explicitly. Gateway does not call LLMs to
reshape input.

Size: M. Removal + test + documentation update.

### 4.3 Remote-access static analyzer

**Decision required.** Move detection rules out of code into
manifest-declared patterns?

Recommendation: agents declare the network patterns they use (`python.imports: [urllib, requests]`, `shell.commands: [curl, wget]`).
The gateway matches code against the declared intent and fails shut on
undeclared patterns. Detection rules become part of the agent's
capability surface rather than gateway-invented policy.

Size: L. This is a significant manifest change affecting many agents.

### 4.4 Package-manager command redirection

Folds into 4.3 — same pattern, same fix.

### 4.5 Content-handle-as-path heuristic

**Decision required.** Replace with strict positive check?

Recommendation: paths must resolve within the sandbox bind-mount
layout; unknown paths fail naturally at exec time. Remove the heuristic
detection entirely. Accept the minor UX loss (no custom error hint).

Size: S.

### 4.6 Loop-guard thresholds

**Decision required.** Manifest-declared limits within a system
ceiling?

Recommendation: yes. Manifests declare
`loop_guard.max_tool_failures ≤ system_ceiling`. Defaults unchanged.

Size: S.

### 4.7 Tool-tier filtering declarative

**Decision required.** Move tier assignments out of Rust into a
reviewable registry?

Recommendation: a `tools.yaml` manifest in the gateway's config,
loaded at startup. `ToolTierFilter` consults this registry rather than
per-tool constants.

Size: M.

### 4.8 Cost-budget silent-disable on catalog failure

**Decision required.** Change default to fail-shut?

Recommendation: yes. Sessions with `max_session_price_usd` set refuse
to start if the OpenRouter catalog is unavailable. An agent with a
`budget.no_price_available.allow` capability can override.

Size: S.

### 4.9 `R++9` Gateway determinism property test (capstone)

**Decision required.** Pin the dumb-gateway principle structurally, not
just by convention.

**Recommendation.** Once items 4.1–4.8 land, add a property test that
asserts: for random valid inputs
`(capability-set, tool-call, recorded-state)`, the gateway's decision
is a pure function — no LLM call, no uninstrumented network fetch, no
nondeterministic branch. Any future change that reintroduces
nondeterminism fails the test.

This is the long-term mechanism that keeps principle and code aligned
across contributors. Without it, the §12 cleanup is a one-time victory
that erodes.

Files: new
`autonoetic-gateway/tests/constitution_gateway_determinism.rs`,
possible trait refactor so policy + tool_call_processor accept mock
injected dependencies cleanly.

**Size.** M. Largely a test harness effort; the hard work is Phase 4's
items 4.1–4.8 making the test possible at all.

---

## Tracking

- **Phase 1 target**: Q2 2026 (2026-04 through 2026-06).
- **Phase 2 target**: Q3 2026.
- **Phase 3**: rolling, as time permits.
- **Phase 4**: requires RFCs; schedule after Phase 1 lands and surfaces
  real operator feedback on the new invariants.

For each in-flight item, open a tracking issue referencing the rule ID
and link to the constitution row. The constitution row's status flips
to `ENFORCED` only when the definition of done is met.
