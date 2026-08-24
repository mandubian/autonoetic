> **ARCHIVED** — Historical design or implementation record. Not current source-of-truth. See [`docs/README.md`](../README.md) for live references.
>

# Sealed-Network Sandbox — Design Plan

**Status:** Implementation tracking doc. See §5 for acceptance criteria
status (✅ shipped, ◐ in progress, empty = pending).

**Refs:**
- Issue #184 — Problem 3 (the umbrella).
- Constitution `P-3.10` — promotion-gate execution denied network. This document
  generalises the property so it can apply outside the promotion gate too,
  driven by a manifest field rather than by a hard-coded gate decision.
- Constitution `I-10` — gateway-decision determinism. The fixture-driven
  egress is one of the surfaces this rule protects.
- Constitution §14 (Lawful-Executor invariant) — the gateway should not learn about
  agent roles; it should enforce what manifests declare. This RFC is shaped
  by that constraint.
- Session that motivated this:
  `~/.autonoetic/runtime/sessions/latest/` (2026-05-12 10:07–10:21).

---

## 0. Terminology

**Fixture** — a canned HTTP response stored on disk that the sealed
sandbox's egress layer returns instead of letting the artifact's network
call reach the live network. The deterministic stand-in for a live server.

A fixture file is JSON, named by the URL components the artifact tries
to reach, and lives alongside the artifact bundle:

```
<artifact-root>/
  moltbook_agent.py
  agent_instructions.md
  fixtures/
    localhost-9876/
      POST-status.json
      GET-feed.json
```

Each fixture file:

```json
{
  "status": 200,
  "headers": {"Content-Type": "application/json"},
  "body": "{\"agents\": [...], \"pending_verifications\": []}"
}
```

When the artifact runs `urllib.request.urlopen("http://localhost:9876/status")`,
the egress hook intercepts the request, looks for
`fixtures/localhost-9876/POST-status.json` (URL-safe encoding: `/` → `-`,
host and port joined with `-`), and returns the canned response. The
artifact thinks it talked to a real server. Same input today, same input
tomorrow → same verdict.

**Why this term:** the pattern is standard in testing — `VCR.py` and
`VCR.rb` (record-and-replay HTTP interactions), `WireMock` (Java HTTP
stubs), `MSW` / `nock` (JS fetch/XHR mocks), `httpretty` (Python). Each
ecosystem calls these things slightly differently (cassettes, recordings,
stubs, snapshots); this RFC uses **fixture** because it is the most
general term and matches Rust/integration-test conventions.

**What fixtures are:**

- Files on disk, content-addressed and signed as part of the artifact
  bundle. Tampering breaks the artifact's identity hash.
- Static data — no logic, no templating in this RFC. (Future
  extension if a real need appears.)
- Network egress only in this RFC. Sealed filesystem (ramdisk), sealed
  clock (`Utc::now()` frozen), sealed RNG (seeded) are listed as future
  extensions of the same shape but out of scope here.
- Read-only at evaluation time. Only Recording mode writes them
  (§3.2.1), and Recording is operator-gated.

**What fixtures are NOT:**

- Not mocks of arbitrary Python/JS objects — those live inside the
  artifact code.
- Not test inputs to the artifact — those are still command-line args /
  stdin / approval-flow inputs.
- Not credentials — credentials still flow through `credential_setup`.
  A fixture says "the server returned this body"; it does not say "use
  this API key."
- Not the artifact's configuration — that is the manifest / args.

---

## 1. Problem statement

When the evaluator role validates a candidate artifact, it must *execute* the
artifact to verify behaviour. Today the execution touches **live external
network** whenever the artifact does. Concretely, the latest session showed:

- Auditor (static analysis) — clean, deterministic, no execution.
- Evaluator (dynamic) — ran `artifact_exec` of an HTTP client against the
  real `http://localhost:9876`. Exit code, response body, and timing depend on
  the live server's state. The verdict the evaluator wrote into
  `gate_messages` was therefore a function of `(artifact, live-server-state)`,
  not `(artifact, fixtures)`.

This violates the principle behind `P-3.10` and `I-10`: gateway decision
surfaces — including the evidence that feeds the promotion gate — must be
pure functions of declared inputs. A Monday-pass / Tuesday-fail flake at the
evaluator stage means the promotion gate becomes a coin flip.

The evaluator is the motivating case but the property generalises. Any role
whose value-add is deterministic verdicts from execution wants the same
treatment: auditors that go beyond static analysis, security scanners,
fuzzers operating against artifact bundles, future federation cross-checkers.

## 2. Rejected alternatives — and why this RFC explicitly is **not**
   "add `SessionState::Evaluation`"

An earlier draft proposed a `SessionState::Evaluation` variant alongside
`Normal`/`Degraded`/`Clarification`, with the gateway routing sealed-network
behaviour based on session state. **That design was rejected after review.**

The reasons:

1. **It conflates state with role.** `Normal` and `Degraded` are *health
   states*. `Clarification` is a role/purpose, but one the **gateway** spawns
   (operator triggers it via `approvals.ask_agent`) and so the gateway
   legitimately owns its definition. `Evaluation` is a role/purpose the
   **planner/orchestrator** spawns. Mixing the two concerns in one enum
   reads convenient at the type level but blurs the architecture.

2. **It violates §14 Lawful-Executor.** Teaching the gateway "what an evaluator is"
   adds role-specific behaviour. The next role with similar needs
   (security-scanner, fuzzer, federation-cross-checker) would either need its
   own SessionState variant or be left out — both bad.

3. **The constraint is genuinely manifest-shaped.** What makes the evaluator
   sealed-network isn't anything mystical about *being an evaluator*; it's a
   declared property the spawner sets. Manifests already encode declared
   properties (`allowed_tool_tiers`, capabilities, sandbox driver). The
   sealed-network policy fits there cleanly.

4. **Manifest declarations are signed and revision-gated.** An evaluator
   that wanted to evade sealed-network can't simply mutate its own manifest
   at runtime — revisions go through P-2.16 (capability-delta gating). The
   integrity property a SessionState gives us is already provided by the
   manifest's signing pipeline.

Other rejected alternatives:

- **Trust the system prompt.** LLMs route around prompts under pressure.
  Structural enforcement only.
- **Strip `NetworkAccess` from the evaluator manifest.** Status quo — logs
  show it produces vacuous fails. Strictly worse than today.
- **Mock outside the evaluator.** Loses the property of running the actual
  artifact bundle end-to-end.

## 3. The design

### 3.1 New manifest field — generic

Add a sandbox-network policy field to `metadata.autonoetic`:

```yaml
metadata:
  autonoetic:
    sandbox:
      driver: bubblewrap
      network: sealed    # one of: normal (default) | sealed | recording
```

Semantics:

- `normal` (default) — gateway enforces network policy as today (capability
  checks + remote-access approval flow).
- `sealed` — **gateway intercepts every outbound network attempt the sandbox
  would make** and routes the request to a fixture responder. Unfixtured
  targets return a structured `unfixtured_target` error from the egress
  layer. Live network is never reached.
- `recording` (developer-only, must be enabled by an operator-level config
  flag and cannot be set silently) — same as `sealed` but on a fixture miss
  the request is sent to the live network and the response is captured as a
  fresh fixture under the artifact root. Operator-gated because it's the
  only way to write fixtures into a bundle.

The evaluator's SKILL.md sets `network: sealed`. The live_tester role (if
introduced) sets `network: normal`. No gateway-side role recognition.

### 3.2 Fixture responder

Fixtures live alongside the artifact bundle so they share the artifact's
content-addressed identity and signature:

```
<artifact-root>/
  moltbook_agent.py
  agent_instructions.md
  fixtures/
    localhost-9876/
      POST-status.json       # {"status": 200, "headers": {...}, "body": "..."}
      GET-feed.json
      POST-feed.json
```

The egress hook (one path, manifest-driven):

1. Reads `manifest.metadata.autonoetic.sandbox.network`.
2. If `normal`: existing policy applies (capability + approval flow). No
   change.
3. If `sealed` or `recording`: every outbound HTTP/DNS attempt is
   intercepted before leaving the sandbox.
4. The interceptor resolves a fixture under
   `<artifact-root>/fixtures/<host[-port]>/<method>-<path>.json` (URL-safe
   encoding, `/` → `-`).
5. Hit: return the canned response.
6. Miss (sealed): return a structured `unfixtured_target` error to the
   artifact with the expected fixture path. Emit
   `artifact.unfixtured_target` causal event.
7. Miss (recording, operator-gated): send to live network, capture
   `(status, headers, body)`, write fixture to disk, return response. Emit
   `artifact.fixture_recorded` causal event.

The hook fires regardless of who the calling agent is. Evaluator, auditor,
security-scanner, or just an operator running an artifact with
`network: sealed` for testing — all get the same behaviour. The gateway has
learned one new rule: "manifest says sealed → route through the fixture
responder."

### 3.2.1 Fixture lifecycle — where fixtures come from

Fixtures aren't conjured. Something has to record them at least once
against a real environment. The evaluator never does this — it consumes
fixtures, never produces them. So the chicken-and-egg: how do fixtures
exist before the evaluator first runs?

Answer: **fixtures are part of the artifact's contract**, shipped *with*
the artifact bundle. The artifact's author is responsible for providing
them, the same way a normal project ships its test data. There are three
legitimate paths to recording, in roughly the order they happen during an
artifact's life:

1. **Author-recorded at design time (the common path).** The coder
   developing the artifact runs it once against a real or staging
   environment with `network: recording` on, captures the responses,
   reviews them, and commits the resulting `fixtures/` directory as part
   of the artifact bundle. The fixtures travel with the code; their hash
   contributes to the artifact's content-addressed identity. By the time
   the evaluator sees the artifact, the fixtures are already there.

2. **Operator-recorded retrofit.** For artifacts that pre-date sealed
   mode or that ship without fixtures, the operator runs
   `autonoetic artifact record-fixtures <artifact_ref>` (CLI in
   acceptance scope 5.3) against a real environment to seed fixtures
   into an existing bundle. The fixtures land in a new artifact revision
   so the integrity story stays intact.

3. **Live_tester-refreshed (the drift-detection path).** The separate
   `live_tester` role (advisory, §3.4) periodically runs the artifact
   against real services with `network: recording`. If the captured
   responses differ from the existing fixtures, a finding is raised
   (contract drift). The operator decides whether to update the fixtures
   (artifact revision) or fix the artifact.

In all three paths, recording requires an explicit operator-level
gateway config flag (acceptance scope 5.3); recording cannot happen
silently.

**What if no fixtures exist when the evaluator runs?**

The evaluator returns `unable_to_evaluate` with `recommendation:
blocked_on_environment` and a finding naming each unfixtured target.
This is the structural fix for the vacuous-fail problem: an artifact
that has not been fixtured is not a broken artifact — it is an
unverified one. The orchestrator (planner / agent-factory) reads the
finding and routes the work to whoever can supply fixtures: typically
back to the coder during development, or to the operator for an
established artifact missing coverage. The evaluator never coerces
"missing fixtures" into "broken artifact."

**Why this is sound:**

- The evaluator's verdict stays a pure function of `(artifact, fixtures)`
  — both signed inputs that travel with the artifact bundle. I-10
  determinism property holds.
- Recording is operator-gated and audited; live capture is never silent.
- Drift between fixtures and the real world is *handled*, not ignored:
  live_tester is the loop that catches it. But drift is a separate
  signal from promotion-gate validity; the evaluator's verdict is not
  affected by what the real server happens to be doing today.
- This matches how serious test suites are run in normal software: the
  developer ships test data alongside the code; CI runs against the
  fixed data deterministically; a separate smoke-test channel hits real
  services and surfaces drift to humans.

### 3.2.2 Bootstrap recording — closing the install loop for new code-bearing artifacts

If a code-bearing artifact ships **without** fixtures (the coder agent
forgot, didn't know to, or had no live target to record against), the
sealed evaluator returns `unable_to_evaluate`, the auditor still
passes (it does static analysis), and `specialized_builder` is blocked
because it requires `evaluator_pass: true`. The install stalls
indefinitely.

The unblocking path is an operator-approved **one-time recording**
orchestrated by `agent-factory.default`:

1. Coder produces the artifact + declares `remote_access` hosts in the
   manifest (already required).
2. Agent-factory spawns evaluator (sealed). Evaluator returns
   `unable_to_evaluate` with a `findings` array naming each unfixtured
   target.
3. Agent-factory recognises the pattern *new install + no prior
   fixtures + coder-declared remote_access* and proposes recording via
   `user_ask` to the operator:

   > *"Artifact `<ref>` requires fixtures for: `<host>:<path>`.
   > Approve one-time recording session against the live endpoint to
   > capture them? Captured responses will be saved as fixtures into a
   > new artifact revision. After that the evaluator runs sealed."*

4. Operator approves → agent-factory spawns a session with
   `network: recording` on the same artifact (gated by the existing
   `gateway.sandbox.allow_recording` config flag) → live calls happen
   under operator consent → fixtures are captured → a new artifact
   revision is published with `fixtures/` baked in.
5. Agent-factory re-runs evaluator against the new revision (now
   sealed; fixtures present) → verdict becomes `pass` or `fail` on
   actual artifact behaviour.
6. `specialized_builder` installs against the new revision.

If the operator **rejects** recording, the install correctly stalls.
The artifact cannot be promoted without verifiable evidence. That is
the right answer — better than installing on faith. The operator may
instead manually seed fixtures (path 2 in §3.2.1) and rerun, or accept
the artifact via an explicit override flagging the missing dynamic
coverage.

**Two layers of consent** keep this safe:

- Gateway-level: `gateway.sandbox.allow_recording: true` must be set in
  config, or any session attempting `network: recording` refuses to
  start at all.
- Per-artifact: `user_ask` to the operator at install time, with the
  specific host list the artifact will reach. Operator answers per
  artifact; auto-approval is not the default.

The agent-factory pattern is the only new orchestrator behaviour; the
gateway-side mechanism is just the existing scope 5.3 (recording
mode). No additional gateway scope is needed.

### 3.3 Verdict outcome (evaluator-side, **not** gateway-side)

The evaluator's verdict shape grows a third outcome:

- `pass`
- `fail`
- `unable_to_evaluate`

This lives **in the evaluator's SKILL.md prompt and output contract**, not in
the gateway. The evaluator surfaces `unable_to_evaluate` when:
- It sees `unfixtured_target` errors from a sealed sandbox call.
- Fixtures are missing for declared remote_access targets.
- The artifact errors in a way that prevents meaningful verification.

The promotion gate (orchestrator-side decision) treats `unable_to_evaluate`
as a non-promoting block — but does **not** mark the artifact as broken.
The structural fix for vacuous fails is to make the third outcome
representable; the gate's policy is a separate decision.

The gateway sees `evaluator_pass: false` either way and doesn't care which
of `fail` or `unable_to_evaluate` it is. The orchestrator differentiates.

### 3.4 Live integration testing — separate role, advisory only

A new role bundle, `live_tester` (or `integration_tester`), declares
`network: normal` and explicit `NetworkAccess`. It runs the artifact against
real services. Its verdict is *advisory* — recorded in causal events but
does not gate promotion.

This is purely a role definition (an agent bundle); the gateway sees
nothing role-specific. The orchestrator decides which roles gate which
gates.

### 3.5 Scope boundary: sealed-network is one mechanism, audit and evaluation still apply broadly

Sealed-network is the right mechanism for **code-bearing artifacts that
make HTTP calls**. It is not what makes an agent reviewable — review is
required for every agent install, regardless of whether the agent ships
executable code.

**Pure-skill agents** (`execution_mode: reasoning`, no `CodeExecution`,
no `AgentSpawn`, no custom code) skip the sealed-network sandbox
because they have no sandbox-bound execution to intercept. But the
SKILL.md *is* the agent's executable contract: an LLM reads those
instructions and follows them, declared capabilities grant real
privileges, and the tool-call patterns the prompt encourages are an
attack surface. **Pure-skill agents still need audit and evaluation —
just by different mechanisms than fixture-driven HTTP testing.**

#### 3.5.1 Static audit for pure-skill agents

The auditor's job for a pure-skill agent is to review the SKILL.md and
its declared metadata, not source code that does not exist. The audit
covers:

- **Prompt content** — instructions in the body of the SKILL. Does the
  prompt encourage the agent to leak inputs, exfiltrate via knowledge
  store, route around capability checks, accept and act on user-supplied
  meta-instructions (prompt injection vector), or violate stated
  output constraints?
- **Capability declarations** — are scopes overly broad
  (`NetworkAccess: ["*"]`, `WriteAccess scopes: ["*"]`)? Are dangerous
  combinations granted together (e.g. `AgentSpawn` + `NetworkAccess` +
  `WriteAccess: ["*"]`)? Does the declared capability set match what
  the prompt actually instructs the agent to do?
- **Tool surface** — given the manifest's `allowed_tool_tiers` and
  capabilities, what is the full set of tools the agent can invoke?
  Are there dangerous combinations (e.g. `web_fetch` + arbitrary
  `content_write` + `scheduler_create`)?
- **Output policy** — are `prohibited_text_patterns` and length limits
  reasonable for the agent's stated purpose? Does the schema constrain
  outputs in security-relevant ways (no `eval`-able shapes, no
  unbounded blobs)?
- **Manifest hygiene** — `sandbox_network` if declared, llm_config
  pinning, `runtime.lock` consistency.

All of this is static, deterministic, and reproducible. The auditor's
verdict for a pure-skill agent is the same shape as for code-bearing
artifacts; only the *targets of review* differ.

**Auditor SKILL.md must be updated to cover this case explicitly.** The
current SKILL frames audit around code (the security checklist mentions
"no secrets in code", "no unbounded network access" in code, etc.) and
runs against `moltbook_agent.py` style files. For pure-skill agents the
auditor reads the SKILL.md, the manifest YAML, and produces findings
against those. This is a SKILL update for `auditor.default`, not a
gateway change.

#### 3.5.2 Behavioral evaluation for pure-skill agents

The evaluator's job for a pure-skill agent is harder than for a
code-bearing one because LLM outputs are not deterministic. Two
honest options exist; neither is in scope for this RFC's
implementation, but the design space is recorded so the path is
visible.

- **Behavioural constraint testing.** Spawn the agent against a set of
  canned input scenarios in a sealed environment (no live network, no
  spawn, output captured). Verify that the agent does not call tools
  outside its declared role, does not emit `prohibited_text_patterns`,
  does not violate its output schema, and does not attempt to
  manipulate other agents or the gateway. The verdict is not "this
  output matches the expected text" but "no constraint violation
  observed across N runs". Same `unable_to_evaluate` outcome applies
  when scenarios are not declared.
- **Adversarial prompt suite.** A small library of prompt-injection
  and jailbreak attempts that the agent should refuse. Pass requires
  the agent to refuse all; fail requires any single violation.
  Reusable across agents.

Both mechanisms need a **scenarios** equivalent of fixtures —
canned inputs and the constraint expectations they probe. The
parallel to §3.2.1's fixture lifecycle is clear: scenarios are
authored at design time by the coder/prompt-author, retrofitted by
operator with a CLI, or refreshed by an advisory `live_prompt_tester`
role.

This RFC scopes **only** the static-audit path (§3.5.1) for pure-skill
agents and the fixture-driven path (§3.1–3.4) for code-bearing
artifacts. The behavioural-evaluation mechanism (§3.5.2) is recorded
as the next layer of the same shape and should land as its own RFC
once §3.5.1 has shipped and operator experience confirms the gaps it
leaves.

#### 3.5.3 Agent-factory gate matrix correction

The agent-factory's current gate matrix
(`agents/evolution/agent-factory.default/SKILL.md` lines 147–153)
contains:

| Agent behavior | Evaluator | Auditor |
|---|---|---|
| Reasoning-only (no CodeExecution, no AgentSpawn) | Skip | Skip |
| Pure transform/utility (no I/O beyond self.*) | Skip | Skip |

**Both rows are wrong** under the corrected scope. The auditor should
run on every install — including reasoning-only and pure-transform
agents — because the SKILL.md is itself the audited surface. The
evaluator skip is also wrong in principle; in practice the §3.5.2
mechanism for behavioural evaluation does not exist yet, so the
matrix should reflect that gap explicitly rather than silently skip.

The corrected matrix:

| Agent behavior | Evaluator | Auditor |
|---|---|---|
| Reasoning-only (no CodeExecution, no AgentSpawn) | Skip *for now* (§3.5.2 follow-up) | **Required** (static SKILL audit per §3.5.1) |
| Pure transform/utility (no I/O beyond self.*) | Skip *for now* (§3.5.2 follow-up) | **Required** |
| Artifact-backed with NetworkAccess | Required (sealed + fixtures, §3.1–3.2) | Required |
| File system writes (beyond self.*) | Required | Required |
| CodeExecution or AgentSpawn | Required | Required |

The "Skip *for now*" entries should be annotated in the SKILL with a
forward reference to §3.5.2 so a future implementation has a clear
hook.

#### 3.5.4 Promotion records for pure-skill agents — the artifact-id problem

The gate-matrix correction (§3.5.3) is conceptually right but cannot
work mechanically against the current promotion-record machinery
because pure-skill agents do not have an `artifact_id`.

Today's `promotion_record` tool (`autonoetic-gateway/src/runtime/tools/promotion.rs`):

```rust
"required": ["artifact_id", "role", "pass"],
// args.artifact_id.starts_with("art_") is asserted at execute().
```

It **requires** an `art_*` ID. Pure-skill agents skip the coder and go
straight from intent to `specialized_builder` via
`agent_revision_create_from_intent` (see
`agents/evolution/agent-factory.default/SKILL.md` Step 2a). No
`artifact_build` runs; no `art_*` ID exists. So even after the auditor
SKILL is extended to review SKILL.md / manifest / capability scopes
(§5.8), the auditor cannot persist its findings via `promotion_record`,
and `specialized_builder` cannot mechanically verify the audit
occurred. The gate-matrix correction in §5.9 therefore has no enforcement
unless this is solved.

Three options considered:

**Option A — Artifact-build every pure-skill agent before install.**
`agent-factory.default` Step 2a inserts an `artifact_build` of the
SKILL.md + manifest YAML + runtime.lock (and nothing else) into a
minimal bundle. The resulting `art_*` ID is what the auditor records
against, what `specialized_builder` installs from, and what P-2.16's
capability-delta gating keys on. The bundle contains no executable
code — just the agent's declared identity surface.

- **Pros:** Every install is artifact-addressed. Audit and (future)
  evaluation records work uniformly across agent types. P-2.16 already
  works without special-casing. The SKILL becomes tamper-evident in
  the same way executable artifacts are. Content-addressed identity
  for *what was audited* matches *what gets installed*.
- **Cons:** Adds a small `artifact_build` step to Step 2a, slightly
  fattening the reasoning-only path. Requires a tiny extension to
  `specialized_builder` to recognise intent-only bundles (no
  `script_entry`, no executable files).
- **Effort:** Half-day to one day. SKILL updates on agent-factory and
  specialized_builder; no gateway-side type changes.

**Option B — Extend `promotion_record` to accept `(agent_id, revision_id)` as an alternative target.**
The tool's `artifact_id` field becomes polymorphic: `art_*` OR
`(agent_id, revision_id)` pointing to a pending revision.

- **Pros:** Smallest gateway change to the existing flow.
- **Cons:** Promotion records now have two shapes; every consumer
  (promotion gate, causal events, P-2.16 enforcement, audit forensics)
  must handle both. The auditor doesn't have a `revision_id` when it
  audits — that ID is created by `agent_revision_create_from_intent`,
  which happens *after* the audit. Resolving this means either the
  auditor records against `(agent_id, intent_hash)` (a third shape) or
  re-audits after revision creation (a workflow inversion).
- **Effort:** Multi-day. Type changes plus consumer updates across
  promotion gate, causal events, and P-2.16.

**Option C — A separate `audit_record_for_intent` tool.**
Auditor calls `audit_record_for_intent(agent_id, intent_hash, role,
pass, findings)` for pre-revision audit. `specialized_builder` checks
*both* `promotion_record` (artifact-addressed) and
`audit_record_for_intent` (intent-addressed) as alternative evidence
shapes.

- **Pros:** Clean separation between pre-revision and post-revision
  evidence.
- **Cons:** Two evidence-recording paths to maintain. Two storage
  schemas. Two enforcement paths in promotion gate. Eternal duplication
  for an asymmetry that doesn't otherwise exist.
- **Effort:** Multi-day. New tool + new store + new enforcement
  branch.

**Recommendation: Option A.**

Architecturally consistent: every agent install is artifact-addressed,
regardless of whether the agent ships executable code. Audit and
evaluation records work uniformly. P-2.16 capability-delta gating applies
without special-casing. "This SKILL.md was the one audited" becomes a
content-addressed fact pinned to the same artifact identity the install
uses. The small `artifact_build` step in agent-factory Step 2a is a
minor cost; the symmetry gained is significant.

Implementation lands as scope **5.10** (see §5).

#### 3.5.5 Gating is gateway-computed, not orchestrator-declared

The previous §3.5 subsections defined *what* the gate matrix should be.
This subsection answers the security question: *who decides which row of
the matrix applies to a given install, and what stops a compromised
orchestrator from picking a lighter row?*

**Today (pre-5.11), the gateway already enforces the principle for two
of the three non-trivial rows.** `enforce_promotion_gate` in
`autonoetic-gateway/src/runtime/tools/agent_revision.rs` computes the
required evidence from the revision's **declared capabilities**, not
from anything the orchestrator claims:

```
needs_artifact_gate := any capability ∈ {CodeExecution, AgentSpawn}
has_high_risk      := any capability ∈ high-risk set (NetworkAccess, …)

if needs_artifact_gate
   → require artifact + auditor PASS + evaluator PASS
else if has_high_risk AND artifact_id is_some
   → require artifact + auditor PASS + evaluator PASS
else
   → direct promote (no promotion_record required)
   "Capability enforcement on every tool call is the security gate."
```

This is the Lawful-Executor invariant (§14) in action and the operational
form of P-2.8 ("High-risk promotion requires evaluator AND auditor
pass") and P-2.16 (P-2.16 capability-delta gating). A malicious
orchestrator cannot lie about gating mode — the gateway re-derives the
requirement from the actual declared capabilities at promotion time.

**The gap §3.5 / 5.10 opened**

Pre-Track B, pure-skill agents had no `artifact_id`. They reached the
`else` branch above and were "direct promote" — the runtime capability
enforcement was the only gate, which was acceptable because there was
no executable code to review.

Track B (`7500268`) gave pure-skill agents an `artifact_id` (the
intent-only bundle, §3.5.4). The expectation in the SKILL changes was
that an auditor `promotion_record(pass=true)` would now be required for
these installs (`audit_only` gating mode). But the gateway-side
enforcement in `enforce_promotion_gate` was **not** updated. A
pure-skill agent with only ReadAccess + WriteAccess still hits the
`else` branch — the intent-only artifact_id is present, but there is no
CodeExecution / AgentSpawn / high-risk capability, so the direct-promote
path is still taken.

**Concrete attack surface this leaves open until 5.11 ships:**

1. A compromised agent-factory (or a planner that bypasses
   agent-factory and calls specialized_builder directly) builds an
   intent-only bundle in Step 2a.
2. It skips the auditor spawn in Step 4.
3. It tells specialized_builder `Gating: audit_only` with a forged
   `promotion_gate.auditor_pass: true`.
4. specialized_builder's SKILL says it should refuse — but if
   specialized_builder is also compromised (or just ignoring its prompt)
   it calls `agent_revision_create_from_intent` + `agent_revision_promote`.
5. The gateway's promote check sees no high-risk capabilities, no
   CodeExecution, so the `else` branch fires — direct promote
   **succeeds**. The malicious SKILL is installed without independent
   audit.

The malicious agent's runtime damage is bounded by its declared
capabilities (and P-2.16 prevents post-install broadening without
operator approval). But the SKILL prompt content — the executable
contract that the user's correction in §3.5 identified as the
attack surface — has not been independently reviewed.

**The corrected, four-branch enforcement (scope 5.11):**

| Computed condition | Required evidence | Mode |
|---|---|---|
| `needs_artifact_gate` (CodeExecution / AgentSpawn) | auditor PASS + evaluator PASS | `full` |
| `has_high_risk && artifact_id` | auditor PASS + evaluator PASS | `full` |
| `artifact_id && !capabilities.is_empty()` (pure-skill intent-only with declared caps) | **auditor PASS** | `audit_only` (NEW) |
| neither (zero-capability sandboxed agent, or no artifact) | direct promote | `direct` |

The `!capabilities.is_empty()` guard preserves direct-promote for
zero-capability sandboxed agents (no `ReadAccess` / `WriteAccess` /
anything): such an agent cannot mutate gateway state via tool calls,
and the audit surface is trivial. Runtime capability enforcement on
every tool call remains the security gate for that case. As soon as
*any* capability is declared (even a benign `ReadAccess scopes:
[self.*]`), the audit gate applies.

The new third branch is the gateway-side enforcement of `audit_only`.
It is a refinement of P-2.8 specific to the intent-only-bundle case;
no new constitutional rule is required, only a more granular
implementation. P-2.17 (distinct evaluator/auditor identity) still
applies in the `full` mode; for `audit_only` it reduces to "the auditor
identity must be distinct from the agent that proposed the install"
(the agent-factory or planner, recorded in the revision's metadata).

**Why this closes the loop**

- The orchestrator's declared gating mode (`full` / `audit_only` /
  `none`) becomes a **hint** for prompt logic, not a trust boundary.
  Trust lives at the gateway check.
- A pure-skill agent install with no auditor `promotion_record` is
  mechanically rejected by `agent_revision_promote` — exactly the same
  way a code-bearing install missing records is rejected today.
- P-2.8's spirit ("promotion gate requirements derive from the
  revision's content") extends to cover intent-only bundles.
- A future constitutional amendment via `constitution_propose_amendment`
  can canonicalise the four-branch matrix into the constitution text,
  but is not blocking — the principle is already covered.

## 4. Constitutional alignment

This RFC does **not** require a new constitutional rule. P-3.10 already
covers the promotion-gate case; the manifest-declared sealed-network is a
generalisation that any role can opt into. The gateway change is one
manifest-driven egress hook — a primitive, not a policy.

If after operating with this for a release we find we want to *require*
sealed-network for evaluator-like roles, that becomes a constitutional rule
amendment proposal (R+19 perhaps, via `constitution_propose_amendment`).
Until then, it's a declaration each role makes for itself.

Existing rules this builds on:
- **P-3.10** — promotion-gate execution denied network. This RFC's mechanism
  could subsume P-3.10's current ad-hoc enforcement: rather than promotion-gate
  having its own network-denial path, the promotion-stage manifest declares
  `network: sealed`. Cleaner. Refactor optional.
- **I-10** — gateway determinism. Fixture-driven egress is one of the
  surfaces this protects.
- **§14 Lawful-Executor** — the gateway gains one generic primitive, not
  role-specific knowledge.
- **Ri-0.6** — no silent capability reduction. The manifest declaration is
  set at manifest-creation time and signed; not narrowed mid-flight.

## 5. Acceptance criteria

Status markers per scope: ✅ shipped, ◐ deferred/future, empty = pending.

| Scope | Status | Commit |
|-------|--------|--------|
| 5.1 — Manifest field | ✅ | `59eca0e` |
| 5.2a — Fixture loader | ✅ | `0a56860` |
| 5.2b — HTTP proxy server | ✅ | `0a56860` |
| 5.2c-advisory — Sandbox env injection | ✅ | `7638b90` |
| 5.2c-enforcing — Kernel-level redirect | ◐ deferred | — |
| 5.2d — HTTPS via CA | ◐ future | — |
| 5.3 — Recording mode + fixture CLI | | |
| 5.4 — Evaluator SKILL flips to sealed | | Planned ▶ ship now |
| 5.5 — Orchestrator awareness | ◐ partial | `0581212` |
| 5.6 — P-3.10 refactor (optional) | | |
| 5.7 — Bootstrap-recording orchestration | | |
| 5.8 — Auditor SKILL for pure-skill agents | | |
| 5.9 — Agent-factory gate-matrix correction | | |
| 5.10 — Artifact-build pure-skill agents | ✅ | `7500268` |
| 5.11 — Gateway audit_only enforcement | ✅ | `0a7952a` |

### 5.1 Manifest field ✅ `59eca0e`

- Add `metadata.autonoetic.sandbox.network: SandboxNetworkPolicy` to the
  manifest schema (`autonoetic-types::agent::SandboxPolicy` or similar).
  Variants: `Normal` (default), `Sealed`, `Recording`.
- Parse from SKILL.md; validate that `Recording` requires an operator-level
  config flag at the gateway (refuse-boot if both are missing the
  permission).
- Constitution test pinning that unknown / mis-spelled values are rejected
  at manifest load.

### 5.2 Egress hook (the bulk of the work — four subscopes)

5.2 is sized as four independently-shippable subscopes. 5.2a/b/c-advisory
are shipped; 5.2c-advisory covers the actual operational need (see
standalone rationale in §5.2c). 5.2c-enforcing (kernel-level redirect)
is deferred — it solves an adversarial-escape problem that does not
exist in autonoetic's current threat model (internal agent-produced
artifacts, not third-party code).

#### 5.2a — Fixture loader + decision ✅ `0a56860`

- `runtime::sealed_network::FixtureLoader` — resolves
  `<artifact-root>/fixtures/<host[-port]>/<METHOD>-<encoded-path>.json`
  with URL-safe path encoding (`/` → `-`, leading `/` stripped, empty
  path → `root`).
- `EgressDecision { Allow, Fixture, Unfixtured { expected_path } }`
  enum + `decide_egress` pure function.
- `unfixtured_envelope_body` produces a structured error envelope
  shaped like the gateway's existing `ToolError` envelopes.
- Tests cover URL-safe encoding, hit/miss/corrupt, decision branches
  per policy.

#### 5.2b — HTTP proxy server ✅ `0a56860`

- `runtime::sealed_network_proxy::start_sealed_proxy(policy, loader)`
  returns a `SealedProxyHandle` bound to `127.0.0.1:0`.
- Axum-based fallback handler parses HTTP-proxy-protocol requests
  (`GET http://host/path`), consults the loader, returns canned
  responses on hit or 502 + `unfixtured_target` JSON on miss.
- CONNECT (HTTPS) rejected with 502 + diagnostic — HTTPS support is
  scope 5.2d.
- Tests exercise hit / miss / host:port / CONNECT-rejection /
  Normal-policy-rejection / clean shutdown.

#### 5.2c — Sandbox integration — TWO LAYERS

**5.2c-advisory (SHIPPED) ✅** — `artifact_exec` / `sandbox_exec`:
- Scans `manifest.sandbox_network` on every exec.
- If `Sealed`/`Recording`: starts the proxy, injects `HTTP_PROXY`,
  `HTTPS_PROXY`, lowercase variants, and empty `NO_PROXY` into the
  sandbox environment; forces `share_net=true` so the sandbox can
  reach the proxy on host loopback.
- Lifecycles the proxy alongside the exec.
- **Catches**: Python `urllib`/`requests`/`httpx`, Node `fetch`/`axios`,
  Go `net/http`, `curl` with default env behaviour.
- **Does not catch**: raw `socket.socket()` + `connect()`, programs that
  unset `HTTP_PROXY`, `curl --noproxy '*'`, non-HTTP traffic.

This is sufficient for autonoetic's current threat model:
- Artifacts are produced by **internal coder agents** (trusted, not
  adversarial). The evaluator is also an internal agent. Neither has
  incentive to bypass the proxy.
- The **auditor** (static analysis) catches raw-socket code that
  would bypass `HTTP_PROXY`. If the auditor misses it, the artifact
  still cannot exfiltrate — the bubblewrap `--unshare-net` namespace
  provides a second layer (no route to any destination).
- Combined: **HTTP_PROXY injection + static audit +
  `--unshare-net` isolation** covers >95% of the practical threat
  surface for non-adversarial artifacts. The remaining gap (bypass
  + no route) is bounded by the namespace, not the proxy.

**5.2c-enforcing (DEFERRED)** — kernel-level transparent redirect:
- Run the sandbox with `--unshare-net` (re-asserted; currently
  advisory forces `share_net=true` which does the opposite).
- Set up a veth pair between host and sandbox namespace. Route all
  sandbox egress TCP through the host-side veth endpoint, where
  nftables/iptables transparent-redirect rules send traffic to the
  proxy. Mitmproxy-transparent-mode mechanism — catches anything
  that opens a TCP socket regardless of env vars.
- UDP / raw-socket protocols stay blocked by the namespace.
- Causal events: `artifact.fixtured_response`,
  `artifact.unfixtured_target`, `artifact.fixture_recorded` emitted
  from the gateway side.

This is **not needed** for the evaluator scenario today. It is
properly deferred until autonoetic runs third-party / untrusted
artifacts where the artifact author has incentive to subvert the
seal. When shipped, it should use a provider-agnostic egress
controller trait so bubblewrap/Docker/microVM backends each
implement their own redirect without changing the proxy.

Constitution test (deferred with 5.2c-enforcing): an agent with
`sandbox_network: sealed`, invoking
`curl --proxy '' http://api.example.com/unfixtured`, receives a
connection failure — the kernel namespace denies the route.
Equivalent test with raw Python `socket.socket()` confirms
transparent redirect catches it.

#### 5.2d — HTTPS support via gateway-generated CA ◐ future

- Gateway generates a CA cert at boot, signs short-lived per-host
  certs at request time.
- The CA is injected into the sandbox's trust store (`/etc/ssl/certs/`)
  at exec start.
- The proxy terminates TLS, decrypts, matches fixture, re-encrypts.
- Until 5.2d ships, HTTPS through the sealed proxy fails — which is
  the safer default (block over leak).

### 5.3 Recording mode + fixture authoring paths (pending — depends on 5.1 + 5.2c-advisory)

- `Recording` is operator-gated: refuse-boot the session if the manifest
  says `Recording` and the gateway config does not include the matching
  permission flag.
- Recording mode emits a prominent causal event banner (`artifact.fixture_recording_session_started`) and each capture emits `artifact.fixture_recorded`.
- Constitution test pinning the refuse-boot path.
- Three legitimate paths to populate fixtures (see §3.2.1), all of which
  this scope must support:
  1. **Author-recorded at design time** — coder runs the artifact under
     `network: recording`, commits fixtures into the bundle as part of
     the artifact's signed identity.
  2. **Operator retrofit** — `autonoetic artifact record-fixtures
     <artifact_ref>` CLI command runs the artifact under `recording`
     against a real environment to seed fixtures into an existing
     bundle. Output lands in a new artifact revision (integrity story
     preserved).
  3. **Live_tester refresh** — the `live_tester` role periodically runs
     the artifact under `recording`; captured responses are diffed
     against existing fixtures to detect contract drift. Drift findings
     are advisory and surface to the operator; the evaluator's verdict
     against the *current* fixtures stays deterministic regardless.

### 5.4 Evaluator SKILL update ▶ planned — SHIP NOW

**Not gated on 5.2c-enforcing.** Advisory mode is sufficient:
internal-coder artifacts are non-adversarial; the auditor catches
raw-socket bypasses at static time; `--unshare-net` blocks unrouted
traffic regardless of HTTP_PROXY. See §5.2c rationale.

- Evaluator's SKILL.md sets `metadata.autonoetic.sandbox.network: sealed`.
- Evaluator's output schema adds the `unable_to_evaluate` outcome.
- Prompt content teaches the agent about fixture-driven runs and the third
  outcome.

### 5.5 Orchestrator awareness ◐ partial `0581212`

- Planner/promotion-gate consumer treats `unable_to_evaluate` as a
  non-promoting block, not a broken-artifact verdict.
- Documentation update in `docs/planner-principles.md` and
  `docs/promotion-gate.md` (or wherever the promotion-gate semantics live).

### 5.6 Optional: refactor P-3.10 enforcement to use the new field

- The current promotion-gate-execution path that denies network via P-3.10
  could be expressed as "promotion-gate sandbox manifest declares
  `network: sealed`." Drop the special-case code; the generic egress hook
  takes over.
- Drop only after 5.2 ships and is stable.

### 5.7 Agent-factory bootstrap-recording orchestration (pending — depends on 5.1 + 5.2 + 5.3)

- `agent-factory.default` SKILL recognises the pattern *new install +
  evaluator returns `unable_to_evaluate` + missing-fixtures finding +
  declared `remote_access`* and triggers a `user_ask` proposing
  recording (§3.2.2).
- On operator approval, agent-factory spawns a `network: recording`
  session against the same artifact, captures fixtures into a new
  artifact revision, then re-runs the evaluator against the new
  revision. On rejection, the install correctly stalls.
- Constitution test (integration-level): an artifact shipped without
  fixtures, evaluator-spawned via agent-factory, follows the
  bootstrap path to a new revision with fixtures baked in.
- Depends on 5.1 + 5.2 + 5.3. SKILL-only change; no further gateway
  work.

### 5.8 Auditor SKILL update for pure-skill agents (pending — no gateway change, SKILL-only)

- `auditor.default` SKILL teaches the agent to audit SKILL.md, manifest
  YAML, and capability declarations when there is no source code
  artifact (i.e. reasoning-only / pure-transform agents).
- Static checks per §3.5.1: prompt content, capability scope
  overreach, dangerous capability combinations, tool-surface review,
  output policy hygiene, manifest hygiene.
- Updated security checklist that covers both code-bearing and
  pure-skill agents.
- No gateway change; SKILL prompt update only. Can land independent
  of 5.1–5.7.

### 5.9 Agent-factory gate-matrix correction (pending — depends on 5.8 + 5.10)

- Update `agent-factory.default` SKILL gate matrix per §3.5.3: the
  auditor row for reasoning-only / pure-transform changes from
  `Skip | Skip` to `Skip *for now* | Required`.
- Comments in the SKILL reference §3.5.2 so the evaluator
  follow-up has a visible hook.
- Depends on 5.8 landing first so the auditor is actually ready to
  run on pure-skill agents.
- Depends on 5.10 landing first so the auditor can actually persist a
  `promotion_record` (without an artifact_id the matrix correction is
  paper-only).

### 5.10 Artifact-build pure-skill agents before install ✅ `7500268`

Implements Option A from §3.5.4. Closes the artifact-id gap that
otherwise makes §5.9 paper-only enforcement.

- **agent-factory.default** Step 2a: insert an `artifact_build` call
  before `agent_spawn specialized_builder.default`. The bundle
  contains the agent's SKILL.md (composed from the install intent),
  the manifest YAML, and runtime.lock. No executable files, no
  `script_entry`. The resulting `art_*` ID is the audit target and
  the install source.
- **auditor.default**: per the §5.8 extension, audits the SKILL.md /
  manifest / capability content of the new pure-skill artifact and
  calls `promotion_record(artifact_id=<new_id>, role="auditor",
  pass=...)` against it. No tool-side change; the existing
  `promotion_record` accepts the artifact_id as-is.
- **specialized_builder.default**: recognises intent-only bundles
  (no `script_entry`, no executable code files) and installs them via
  `agent_revision_create_from_intent` keyed on the bundled SKILL.md.
  Validates the auditor's `promotion_record` exists against the
  bundled artifact_id (the `audit_only` gating mode from §3.5.3 +
  §5.9). The P-2.16 capability-delta check works unchanged because it
  already keys on artifact_id.
- **No gateway-side type change.** No `promotion_record` polymorphism.
  No new audit-record tool. The work is entirely in agent-factory and
  specialized_builder SKILLs.
- Constitution test: install a pure-skill agent end-to-end. Auditor
  finds and records against the intent-only artifact ID; install
  refuses if the record is missing; install succeeds when it is
  present.
- Effort: half-day to one day. No gateway dependencies — can land
  before or independently of Track A scopes 5.1–5.7.

### 5.11 Gateway-side `audit_only` enforcement ✅ `0a7952a`

Closes the trust gap §3.5.5 identifies: without this scope, the
SKILL-level `audit_only` expectation in 5.8 / 5.9 / 5.10 is not
mechanically enforced by the gateway, and a compromised orchestrator
can install a pure-skill agent without audit.

- **`autonoetic-gateway/src/runtime/tools/agent_revision.rs`** — extend
  `enforce_promotion_gate` (and its callers in
  `AgentRevisionPromoteTool::execute`) with a new branch:

  ```
  if needs_artifact_gate                            → full (existing)
  else if has_high_risk && artifact_id is_some      → full (existing)
  else if artifact_id is_some                       → audit_only (NEW)
  else                                              → direct (existing)
  ```

- **`audit_only` mode** requires:
  - An auditor `promotion_record(pass=true)` against the revision's
    artifact_id; `record.auditor_pass` set; `record.auditor_id`
    distinct from the revision's `created_by` (the agent that
    proposed the install). P-2.17 reduces to "auditor identity ≠
    proposer identity" because there is no evaluator to compare to.
  - The revision's `content_digest` matches the auditor's record
    `content_digest` (existing invariant — unchanged).
- **Evaluator pass is NOT required** in `audit_only` mode. The
  promotion record carries `evaluator_pass: false`, `evaluator_id:
  None`, and a new `evaluator_status: "skipped"` field with reason
  `"behavioural_eval_not_implemented"`. The status is informational;
  the gating decision keys on the artifact-shape branch above.
- **Existing `evaluator_pass` check** in `enforce_promotion_gate`
  must be relaxed to "if mode = full, require evaluator_pass; if
  mode = audit_only, allow evaluator_pass = false with skipped
  status". Care must be taken not to weaken the existing `full`
  enforcement.
- **Causal event** on each promotion: emit
  `revision.promotion_gate_enforced` with the computed mode
  (`full | audit_only | direct`) so forensics can later check what
  gate was actually applied to a given revision.
- **Constitution test** `constitution_promotion_gate_audit_only.rs`:
  - A pure-skill revision (intent-only artifact, no CodeExecution /
    AgentSpawn / NetworkAccess) with no auditor record → promote
    fails with `Promotion gate: no auditor promotion.record …`.
  - With auditor `promotion_record(pass=true)` but no evaluator → 
    promote **succeeds** (mode = audit_only).
  - With auditor `promotion_record(pass=false)` → promote fails.
  - Auditor identity equal to the proposer's identity → promote
    fails (P-2.17 self-approval ban).
  - A revision with NetworkAccess + intent-only artifact still
    requires both evaluator and auditor (mode = full via has_high_risk
    branch) — pin that 5.11 does not weaken the existing high-risk
    enforcement.
- Effort: half-day to one day. Gateway-side code change with constitution
  test.

**Order:** 5.11 should land **before** 5.10 is announced as
operator-safe. Until 5.11 ships, the SKILL-level expectations in
5.8 / 5.9 / 5.10 are aspirational. Operators should be aware that
Track B `7500268` ships the SKILL changes but the gateway-side
enforcement comes in 5.11.

## 6. Migration and rollout

The scopes are sequenced so each PR is meaningful alone and does not enable
partial enforcement. Two independent tracks run in parallel: the
**sealed-network mechanism** (5.1–5.7, gateway + code-bearing artifact
flow) and the **audit completeness correction** (5.8–5.9, pure-skill
agent review). Each track is internally ordered.

**Track A — Sealed-network mechanism (code-bearing artifacts):**

1. **5.1**: manifest field + parsing + rejection of bad values. ✅
2. **5.2a** + **5.2b**: fixture loader + HTTP proxy server. Tested
   in isolation. ✅
3. **5.2c-advisory**: sandbox integration — HTTP_PROXY injection into
   bubblewrap exec environment. Catches HTTP_PROXY‑aware clients.
   ✅ shipped with commit `7638b90`.
4. **5.4**: evaluator's SKILL.md flips to `network: sealed`. **Not
   gated on 5.2c-enforcing** — advisory mode is sufficient because:
   - Artifacts are produced by internal coder agents (non-adversarial).
   - The auditor catches raw-socket code at static analysis time.
   - `--unshare-net` blocks un-routed traffic regardless of
     HTTP_PROXY bypass. ▶ **SHIP NOW**.
5. **5.5**: orchestrator awareness of `unable_to_evaluate`. Already
   partly landed in #184 / `0581212`.
6. **5.3**: recording mode safety + the three authoring paths. Depends
   on 5.1 + 5.2a/b + 5.2c-advisory.
7. **5.7**: agent-factory bootstrap-recording orchestration — closes
   the install loop for new code-bearing artifacts. Depends on 5.1 +
   5.2 + 5.3.
8. **5.2c-enforcing** *(deferred)*: kernel-level transparent TCP
   redirect via veth + nftables. Unblocks only when autonoetic runs
   third-party/adversarial artifacts. Not a blocker for anything
   above.
9. **5.2d** *(future)*: HTTPS via gateway-generated CA.
10. **5.6** (optional): refactor P-3.10's ad-hoc enforcement to use the
    generic hook.

The evaluator can ship sealed now without waiting for kernel-level
enforcing, because the threat model for internal-artifact evaluation
does not require it.

**Track B — Audit completeness for pure-skill agents:**

1. **5.10**: agent-factory builds an intent-only artifact for pure-skill
   agents before invoking specialized_builder, so audit findings have a
   content-addressed target. SKILL-only changes on agent-factory and
   specialized_builder; no gateway code change. (§3.5.4 Option A.)
2. **5.8**: auditor SKILL teaches the agent to audit SKILL.md, manifest
   YAML, and capability declarations. Now records against the
   intent-only artifact ID produced by 5.10. Static analysis only.
3. **5.9**: agent-factory gate matrix corrected to require auditor for
   pure-skill agents. Depends on 5.8 and 5.10 being ready — without
   5.10 the matrix correction is paper-only.
4. **5.11**: gateway-side `audit_only` enforcement in
   `enforce_promotion_gate`. **Closes the trust gap §3.5.5 identifies.**
   Without 5.11 the SKILL-level expectations in 5.8 / 5.9 / 5.10 are
   aspirational and a compromised orchestrator can install a pure-skill
   agent without audit. (§3.5.5.)

The Track B internal order is 5.10 → 5.8 → 5.9 → 5.11. The first three
ship SKILL changes; 5.11 makes them mechanically enforced. Until 5.11
ships, the SKILL changes from 5.8–5.10 are SKILL-level only.

Track B is independent of Track A and can land in either order. It is
lower-risk than Track A overall (most of Track B is SKILL changes), but
5.11 is the gateway-code piece that gives the rest its teeth.

## 7. Open questions

1. **Where do fixtures live for **agent bundles** vs. **library artifacts**?**
   Natural answer (now adopted in §0 and §3.2.1): "next to the code, same
   artifact root, signed as part of the bundle." Agent bundles already
   have constraints on their structure (SKILL.md, runtime.lock); adding
   a `fixtures/` directory needs to play well with content-addressed
   artifact identity. The artifact's content-hash must include the
   `fixtures/` tree so tampering breaks identity.

2. **Fixture freshness.** A fixture captured in January may be stale by
   July. Possible answers: fixtures carry a `captured_at` and the evaluator
   surfaces age in the verdict; or a policy field `max_fixture_age: 30d` on
   the consuming manifest. Drift detection is the `live_tester` role's
   job (§3.4); the evaluator's verdict itself remains deterministic
   regardless of fixture age.

3. **Partial fixtures.** If the artifact makes 3 HTTP calls and only 2
   have fixtures, what's the outcome? Probably `unable_to_evaluate` with
   the missing-fixture set named — but the boundary needs spec: is it
   "any missing fixture → unable_to_evaluate" (strict), or "only required
   targets must be fixtured; optional/best-effort calls can miss"
   (lenient with declaration)?

4. **Side effects on disk / filesystem / clock.** Sealed network is one
   axis; sealed filesystem (e.g. ramdisk-only writes), sealed clock (frozen
   `Utc::now()`), sealed randomness (deterministic RNG seed) are others.
   Not in scope here but the same manifest-driven shape would apply, with
   each a separate field under `sandbox`. The hook surface in the sandbox
   driver becomes the right place to add them.

5. **Fixture authoring UX.** Hand-writing JSON fixtures is tedious.
   Recording mode (§3.2.1, §5.3) covers seed creation through three
   paths (author-recorded, operator-retrofit, live_tester drift refresh).
   UX for subsequent updates (drift detection workflow, regeneration,
   diff review when the live response shape changes) is a follow-up.

6. **Should `P-3.10` be refactored or left as-is?** P-3.10's current
   enforcement is in promotion-gate-specific code. With this RFC the same
   property is expressible via a manifest field. Refactoring is cleaner but
   not blocking; deferring keeps the PR small.

These open questions block 5.2 (the egress hook implementation) but **not**
5.1 (manifest field).

## 8. Summary — what the gateway learns from this RFC

One new generic primitive: **manifest-declared sandbox network policy**.

What the gateway does NOT learn:
- "What an evaluator is."
- "What a live_tester is."
- "What verdicts mean."
- "When fixtures should be required."

Those all live in agent bundles and orchestrator prompts. The gateway stays
the dumb mechanical enforcer of one declarative property, and the same
property scales to any future role that wants deterministic execution.
