# Sealed-Network Sandbox — Design Plan

**Status:** Draft RFC. No gateway-side code change yet; this document captures
the design and acceptance bars so an implementation PR can target them.

**Refs:**
- Issue #184 — Problem 3 (the umbrella).
- Constitution `R+16` — promotion-gate execution denied network. This document
  generalises the property so it can apply outside the promotion gate too,
  driven by a manifest field rather than by a hard-coded gate decision.
- Constitution `R++9` — gateway-decision determinism. The fixture-driven
  egress is one of the surfaces this rule protects.
- Constitution §14 (dumbness invariant) — the gateway should not learn about
  agent roles; it should enforce what manifests declare. This RFC is shaped
  by that constraint.
- Session that motivated this:
  `~/.autonoetic/agents/.gateway/sessions/latest/` (2026-05-12 10:07–10:21).

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

This violates the principle behind `R+16` and `R++9`: gateway decision
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

2. **It violates §14 dumbness.** Teaching the gateway "what an evaluator is"
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
   at runtime — revisions go through R++2 (capability-delta gating). The
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

## 4. Constitutional alignment

This RFC does **not** require a new constitutional rule. R+16 already
covers the promotion-gate case; the manifest-declared sealed-network is a
generalisation that any role can opt into. The gateway change is one
manifest-driven egress hook — a primitive, not a policy.

If after operating with this for a release we find we want to *require*
sealed-network for evaluator-like roles, that becomes a constitutional rule
amendment proposal (R+19 perhaps, via `constitution_propose_amendment`).
Until then, it's a declaration each role makes for itself.

Existing rules this builds on:
- **R+16** — promotion-gate execution denied network. This RFC's mechanism
  could subsume R+16's current ad-hoc enforcement: rather than promotion-gate
  having its own network-denial path, the promotion-stage manifest declares
  `network: sealed`. Cleaner. Refactor optional.
- **R++9** — gateway determinism. Fixture-driven egress is one of the
  surfaces this protects.
- **§14 dumbness** — the gateway gains one generic primitive, not
  role-specific knowledge.
- **Ri-0.6** — no silent capability reduction. The manifest declaration is
  set at manifest-creation time and signed; not narrowed mid-flight.

## 5. Acceptance criteria

### 5.1 Manifest field

- Add `metadata.autonoetic.sandbox.network: SandboxNetworkPolicy` to the
  manifest schema (`autonoetic-types::agent::SandboxPolicy` or similar).
  Variants: `Normal` (default), `Sealed`, `Recording`.
- Parse from SKILL.md; validate that `Recording` requires an operator-level
  config flag at the gateway (refuse-boot if both are missing the
  permission).
- Constitution test pinning that unknown / mis-spelled values are rejected
  at manifest load.

### 5.2 Egress hook (the bulk of the work)

- Sandbox driver hooks (bubblewrap / docker / microvm) recognise the
  manifest field and route HTTP/DNS through the fixture responder when
  `network` is sealed or recording.
- Fixture loader: resolves `fixtures/<host[-port]>/<method>-<path>.json`
  relative to the artifact root.
- Causal events: `artifact.fixtured_response` for hits,
  `artifact.unfixtured_target` for sealed misses,
  `artifact.fixture_recorded` for recording-mode captures.
- Constitution test: a session whose manifest declares `network: sealed`
  cannot reach a non-fixtured target via `sandbox_exec` or `artifact_exec`,
  regardless of declared `NetworkAccess`.

### 5.3 Recording mode safety

- `Recording` is operator-gated: refuse-boot the session if the manifest
  says `Recording` and the gateway config does not include the matching
  permission flag.
- Recording mode emits a prominent causal event banner and is auditable.
- Constitution test pinning the refuse-boot path.

### 5.4 Evaluator SKILL update

- Evaluator's SKILL.md sets `metadata.autonoetic.sandbox.network: sealed`.
- Evaluator's output schema adds the `unable_to_evaluate` outcome.
- Prompt content teaches the agent about fixture-driven runs and the third
  outcome.

### 5.5 Orchestrator awareness

- Planner/promotion-gate consumer treats `unable_to_evaluate` as a
  non-promoting block, not a broken-artifact verdict.
- Documentation update in `docs/planner-principles.md` and
  `docs/promotion-gate.md` (or wherever the promotion-gate semantics live).

### 5.6 Optional: refactor R+16 enforcement to use the new field

- The current promotion-gate-execution path that denies network via R+16
  could be expressed as "promotion-gate sandbox manifest declares
  `network: sealed`." Drop the special-case code; the generic egress hook
  takes over.
- Drop only after 5.2 ships and is stable.

## 6. Migration and rollout

The scopes are sequenced so each PR is meaningful alone and does not enable
partial enforcement:

1. **5.1**: just the manifest field + parsing + rejection of bad values.
   No behaviour change.
2. **5.2**: egress hook. With 5.1 + 5.2 the gateway *can* enforce sealed
   mode for any manifest that declares it, but no real agent declares it
   yet.
3. **5.3**: recording mode safety. Cannot ship before 5.2.
4. **5.4**: evaluator's SKILL.md flips to `network: sealed`. From this PR
   onward, the evaluator runs sealed.
5. **5.5**: orchestrator awareness of `unable_to_evaluate`. The verdict
   plumbing lights up.
6. **5.6** (optional): refactor R+16's ad-hoc enforcement to use the
   generic hook.

The order matters: ship the *capability* (5.1–5.3) before any role *uses*
it (5.4), so a partial deploy doesn't leak. Operators can roll back at
5.4 without breaking the gateway primitive.

## 7. Open questions

1. **Where do fixtures live for **agent bundles** vs. **library artifacts**?**
   Natural answer: "next to the code, same artifact root." Agent bundles
   already have constraints on their structure (SKILL.md, runtime.lock);
   adding a `fixtures/` directory needs to play well with content-addressed
   artifact identity and signing.

2. **Fixture freshness.** A fixture captured in January may be stale by
   July. Possible answers: fixtures carry a `captured_at` and the evaluator
   surfaces age in the verdict; or a policy field `max_fixture_age: 30d` on
   the consuming manifest.

3. **Partial fixtures.** If the artifact makes 3 HTTP calls and only 2 have
   fixtures, what's the outcome? Probably `unable_to_evaluate` with the
   missing-fixture set named — but this needs spec.

4. **Side effects on disk / filesystem / clock.** Sealed network is one
   axis; sealed filesystem (e.g. ramdisk-only writes), sealed clock (frozen
   `Utc::now()`), sealed randomness (deterministic RNG seed) are others.
   Not in scope here but the same manifest-driven shape would apply, with
   each a separate field under `sandbox`. The hook surface in the sandbox
   driver becomes the right place to add them.

5. **Fixture authoring UX.** Hand-writing JSON fixtures is tedious.
   Recording mode (5.3) covers seed creation. UX for subsequent updates
   (drift detection, regeneration, diff review) is a follow-up.

6. **Should `R+16` be refactored or left as-is?** R+16's current
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
