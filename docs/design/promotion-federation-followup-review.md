# Follow-up Review — promotion-federation Phase 2 → 4

**Status:** Second-pass review covering commits since the original review (`96fd2de`, `docs/design/promotion-federation-plan-review.md`). Written 2026-05-13.

**Reviewed against:** plan as of 5812399; codebase at `be2b665` (latest on `main` at time of review).

**Commits covered (in order):**
- `0f3e632` — `EscalationMessage` type, federation.escalate tool, admin routes (#200)
- `a583920` — address #200 review issues (gate, dedup, audit trail, cleanup)
- `22c8bb8` — FullJury branch in `enforce_promotion_gate` (#201)
- `f5f9f0d` — federation FullJury gate end-to-end integration tests (#202)
- `69b2185` — federation fixes: sealed_evaluator role string, remove deprecated evaluator.default, Evaluation cap guard in artifact_exec (#201)
- `08f3349` — Phase 2 recording-mode foundation (#187)
- `a9817dd` — CLI `--record-network` flag and `recording` subcommand
- `bcab346` — thread recording dir through proxy setup + causal events
- `f2df96f` — CLI `autonoetic eval sealed --artifact-ref X --fixture-set Y` (#198)
- `19319e6` — `artifact_exec` `fixture_set_ref` argument for recorded fixture replay
- `be2b665` — Phase 4 post-promotion background review Tier 1 (#199)

---

## TL;DR

Most of the recommended next-session work from the previous review (`96fd2de`) shipped cleanly. The core promotion-unification mechanic — FullJury gate, EscalationMessage, federation.escalate tool — is land and mechanically enforced. P-2.17 under federation correctly implements the middle-ground I recommended. Phase 2 recording-mode redaction policy is actually *better* than my §4.3 concern asked for. Phase 4 Tier 1 reuses the same EscalationMessage channel cleanly.

Ten forgotten or imperfect points remain. The highest-impact one is **operator-throughput safeguards (#5 below)** — this was flagged as the single biggest risk in the previous review's closing section and is still unaddressed. Everything else is incremental polish, with one item (#3, post-promotion `artifact_id` empty) leaning toward a minor design bug.

---

## What landed well

### Gate enforcement (previous review §3.2 was the concern)

- **FullJury mechanically enforces operator approval.** `autonoetic-gateway/src/runtime/tools/agent_revision.rs:1941-2167`. The gate checks `promo_store.has_federation_roles(artifact_id)` (`promotion_store.rs:266-276`) and **requires** an approved `EscalationMessage` via `gateway_store.find_escalation(artifact_id, revision_id, EscalationStatus::Approved)` (`agent_revision.rs:2155-2167`). Not advisory; not bypassable from a compromised planner that does `revision.promote` directly.

- **P-2.17 distinct-identity is middle-ground as recommended.** `agent_revision.rs:2135-2152`. Each federation role's `agent_id` must differ from `rev.created_by_id` AND from every other federation role's `agent_id`. The error message cites P-2.17 directly.

- **`required_eval_run_id` legacy path preserved.** `agent_revision.rs:2179-2183`. Both gates can be active in parallel; no removal needed. This matches §11.1 of the original audit and my §3.2 recommendation.

### EscalationMessage (previous review §3.3 was the concern)

- **EscalationMessage type exists with the shape the plan proposed.** `autonoetic-types/src/escalation.rs:49-114` carries `escalation_id`, `artifact_id`, `revision_id`, `role_verdicts: Vec<RoleVerdictSummary>`, `planner_synthesis`, `status: EscalationStatus`, `decided_by`, `decision_reason`. The audit-trail fields (decided_by, decision_reason) were added in `a583920`.

- **`federation.escalate` tool gated to `AgentSpawn` capability.** `runtime/tools/federation.rs:35-40` — only orchestrators can create escalations. Closes a hole that existed in the initial `0f3e632`.

- **Dedup guard on pending escalations.** `escalations.rs:31-46` rejects a second escalation with the same `(artifact_id, revision_id)` while a previous one is `Pending`. Idempotent by design.

- **Admin route for operator resolve.** `router.rs::admin.escalation_resolve` with `escalation_id`, `decided_by`, `status`, `reason`. Audit trail recorded on resolution; idempotent re-resolution check at `escalations.rs:154-160` prevents double-decide.

### Recording-mode redaction (previous review §4.3 was the concern)

This is **better than I expected.** `sealed_network.rs:255-354` covers:

| Category | Fields |
|---|---|
| Request headers | `authorization`, `cookie`, `x-api-key`, `proxy-authorization` |
| Response headers | `set-cookie`, `www-authenticate`, `proxy-authenticate` |
| Query params | `token`, `api_key`, `apikey`, `secret`, `key`, `password`, `auth`, `signature`, `access_token`, `refresh_token` |
| Body (regex) | `bearer\s+[^\s,;}\]]+` → `bearer [REDACTED]` |

The `redact_fixture` function (`sealed_network.rs:288-344`) returns the list of redacted field names so the fixture file itself carries a "redaction manifest" alongside the cached response. Forensically useful.

### Phase 4 Tier 1 reuses the federation channel

`post_promotion_review.rs:151-159` constructs an `EscalationMessage` using the same type as federation escalations. The operator sees both in the same `admin.escalation_list`. Anomaly thresholds (`post_promotion_review.rs:69-127`) are concrete (tool-failure-rate > 1.5× → warning, > 3.0× → critical; auth_denials doubled; suspensions doubled; sentinel findings > 0 → warning, > 2 → critical). Comparison-based, not absolute, which is the right shape for drift detection.

---

## Forgotten or imperfect points

### 1. P-2.22 not formally cited in code (previous review §3.7)

**Severity:** Documentation gap / forensic ambiguity.
**File:line:** `agent_revision.rs:2051` emits `enforced_rules: ["P-2.8", "P-2.17"]` for the FullJury event.

The previous review §3.7 specifically asked for a concrete rule number for "operator as final arbiter" instead of the placeholder `R-2.xx`. The mechanic shipped; the rule citation didn't. Anyone reading the causal chain to find federation promotions sees `P-2.8` (high-risk promotion requires eval+audit) and may not realise FullJury is a separate, stronger requirement.

**Fix:** allocate `P-2.22` (next free in P-2.x) with explicit text covering "operator approval is required when any federation-role verdict is present"; add to `enforced_rules` at the FullJury emission site; submit constitutional amendment via `constitution_propose_amendment` per Ri-0.8 amendment channel.

### 2. `EscalationType` enum not shipped (plan §4.3)

**Severity:** Future-proofing gap.
**File:line:** `autonoetic-types/src/escalation.rs:49-114` — no `EscalationType` variant tagging.

Plan §4.3 proposed `enum EscalationType { PromotionReview, SealedEvalInquiry, PostPromotionAnomaly, RecordingComplete }`. The implementation distinguishes escalations by the originating `agent_id` (planner for federation, `security_sentinel` for post-promotion). This works today but:

- A future Slack/Discord adapter cannot route categories to different channels without parsing agent IDs (which are not stable identifiers across deployments).
- The schema cannot grow to add `RecordingComplete` (operator notification "your recording session is finished") without disambiguation logic.
- Filtering / dashboards have to know agent-ID conventions.

**Fix:** add `escalation_type: EscalationType` to `EscalationMessage`. Backward-compat: default to `PromotionReview` for existing rows. Post-promotion review explicitly sets `PostPromotionAnomaly`.

### 3. Post-promotion-review escalations have empty `artifact_id`

**Severity:** Minor design bug.
**File:line:** `post_promotion_review.rs:153` — `artifact_id` is left `String::new()`.

Tier 1 watches the running agent, not an artifact, so there isn't always a meaningful artifact reference at escalation time. But the `EscalationMessage` schema's foreign-key semantics expect an artifact context, and the dedup guard at `escalations.rs:31-46` keys on `(artifact_id, revision_id, status)`. An empty `artifact_id` means:

- Operators viewing the escalation in `admin.escalation_list` cannot click through to the artifact.
- Two post-promotion escalations for the same agent end up both keying on `("", revision_id)` — accidentally deduplicating to one entry, OR (if revision_id differs) creating two unrelated rows that look paired in the UI.
- Any future per-artifact filter ("show me all escalations for ar.X") silently misses post-promotion rows.

**Fix:** either resolve to the latest installed revision's artifact_id (`revision.artifact_id` is available on the `AgentRevisionRecord`), or change the schema to make `artifact_id` `Option<String>` and the dedup key honour the absence explicitly.

### 4. Background trigger for post-promotion review is hidden

**Severity:** Documentation gap.
**File:line:** `post_promotion_review.rs:44-191` defines `run_post_promotion_review()` but the scheduler integration is not visible in the audit commits.

The function exists. Whether it fires on a cron, after every promotion, on operator command, or never — unclear from the audit window. This matters because:

- Operators don't know what cadence to expect drift escalations on.
- A test cannot easily exercise the integration end-to-end without knowing the trigger.
- If the scheduler hook is missing entirely, Tier 1 is dead code today.

**Fix:** either point to the scheduler call site in a doc comment on `run_post_promotion_review`, or — if the hook isn't wired yet — add a TODO that says so and an issue tracking it.

### 5. **Operator-throughput safeguards: NONE shipped** ⚠ biggest risk

**Severity:** Operational risk.
**File:line:** `escalations.rs:96-110` (`list_pending_escalations`) returns all rows; no rate limit, no per-operator queue cap, no batching.

The previous review's closing section flagged operator throughput as **the single biggest risk** to whether the federation model works in practice. The shipped dedup guard (`escalations.rs:31-46`) only catches *exact* (artifact_id, revision_id, status=pending) duplicates. It does not protect against:

- 100 distinct federation runs for distinct artifacts arriving at once — operator sees 100 escalations.
- A misbehaving planner that spawns federation per minute for the same revision (different `escalation_id`s because previous ones resolved).
- Batch agent installs across an organisation hitting one operator.

This is not a regression of an existing feature; it's a category of feature that hasn't been built yet. But it is the prerequisite for the federation model to be *usable* at any scale.

**Fix options (any one of):**
- Operator-side dedup: "you already reviewed this artifact-content-digest in the last N minutes, auto-approve / mute"
- Rate limit on `federation.escalate` per (root_session, agent_id) pair
- Batching API: `admin.escalation_list_batch` returns escalations grouped by content_digest with bulk-approve semantics

### 6. Ask-agent NOT integrated with federation review (previous review §4.2)

**Severity:** Operator UX gap.

Clarification-child-session machinery (#172) exists. The federation escalation flow does not wire it. An operator reading an escalation sees the synthesised verdict and the role findings but cannot ask "static_evaluator, why did you flag this URL pattern?" without manually spawning a new session.

**Fix:** the admin route for viewing an escalation exposes a `ask_role(escalation_id, role, question)` action that spawns a clarification child session of the recorded role with the question. The role's answer threads onto the escalation via `gate_messages` (the enrichment thread already exists from P-2.19).

### 7. Negative-path test for "planner skips federation entirely" is missing

**Severity:** Coverage gap.
**File:line:** `constitution_federation_e2e.rs:974-1055` covers operator rejection; no test for direct-promote bypass.

The FullJury gate at `agent_revision.rs:2155-2167` rejects when federation verdicts are present but no approved escalation exists. The test that pins this behaviour against regression is missing. A future refactor (e.g., moving the federation-detection logic) could silently regress this property and the e2e suite would pass.

**Fix:** add a test:
1. Create artifact, record federation verdicts (static_evaluator.pass=true, unit_test_runner.pass=true, auditor.pass=true).
2. Do NOT create an escalation.
3. Call `revision.promote` directly.
4. Assert: promote fails with "artifact has federation role verdicts but no approved operator escalation."

### 8. Recording-mode causal events not explicitly emitted

**Severity:** Audit gap.

The redaction list is captured in the fixture file metadata, which is forensically useful. But the causal chain does not record:

- "Recording session started at T by operator O for agent A"
- "Fixture captured for {host, method, path}"
- "Secret redacted from {field}"

This means a later security review cannot reconstruct *when* recording was enabled or *what was captured* from the causal chain alone — they have to read the fixture files on disk, which may have been rotated or pruned.

**Fix:** emit at minimum a `recording.session_started` event at session start with operator identity, and a `recording.fixture_captured` event per fixture with the host+method+path target (not the body content; that lives in the fixture file). If sensitive fields are redacted, the redaction count goes in the event payload.

### 9. Flat `PromotionRecord` at 5 roles × 4 fields = 20 fields

**Severity:** Tech-debt accrual.
**File:line:** `autonoetic-types/src/promotion.rs:50-121`.

Previous review §3.1 noted this was acceptable as a stopgap with an explicit deferral trigger ("when role count > 5"). The shipped state is *at* the trigger (5 roles: evaluator, auditor, static_evaluator, unit_test_runner, sealed_evaluator). Adding a sixth role (a `live_tester` or `federation_smoke` role, for example) would push past it cleanly.

**Fix (when 6th role arrives, not before):**
- Refactor `PromotionRecord` to `HashMap<PromotionRole, RoleVerdict>` per plan §3.3.
- Custom `Deserialize` reads both old flat format and new map.
- On first write after upgrade, normalise to the new format.
- No data migration script; read-time migration only.

For now, a `// TODO: refactor to verdict-map when role 6 is added` comment in `promotion.rs` is sufficient to anchor the decision.

### 10. Code excerpts on escalations missing (previous review §4.1)

**Severity:** Operator UX gap.
**File:line:** `#186` Phase 1 work in `f3f2e18` (`code_excerpts.rs`, `set_approval_code_excerpts`) attaches code to approval rows, not to escalation rows.

This was the "biggest sibling integration" recommendation in the previous review. Operators reviewing a federation escalation see a wall of verdicts but no source code to ground them in. The code-extraction machinery (32 KiB per file, 128 KiB total, file-by-file) is already shipped and tested for the approval surface — it just needs to be wired to escalations too.

**Fix:** when `federation.escalate` creates an escalation, attach the same `code_excerpts` from the artifact. The TUI / CLI surfaces that render escalations show them under a `Code:` section (mirroring the `c` toggle that approval cards already have).

---

## Priority ranking for follow-up work

Ordered by impact / risk:

1. **Operator-throughput safeguards** (#5). Biggest operational risk. Without this, the federation model fails at scale regardless of how clean the mechanics are.

2. **P-2.22 formally cited** (#1). Small change, removes forensic ambiguity, lets the constitutional amendment process catch up.

3. **Negative-path tests** (#7). Pins the mechanical guarantee against regression. A few hours of test-writing.

4. **Code excerpts on escalations** (#10). Composes #186 Phase 1 with federation review. Small wiring, large operator UX win.

5. **Recording-mode causal events** (#8). Closes the audit gap. Small surface, important for security review story.

6. **Background-trigger documentation / wiring** (#4). At minimum write down where Tier 1 fires; at maximum, add the scheduler hook if missing.

7. **Post-promotion `artifact_id` resolution** (#3). Either populate it from the latest revision, or change the schema to make it optional and honour absence in dedup.

8. **`EscalationType` enum** (#2). Adds before adding a second channel adapter (Slack/Discord/etc).

9. **Ask-agent integration** (#6). Operator UX win, larger surface. Composes #172 machinery into the federation flow.

10. **Verdict-map refactor** (#9). Defer until role 6 actually arrives. Add a TODO comment now to anchor the decision.

---

## Closing thoughts

The promotion-federation pivot is in. The mechanic that the original plan promised — federation roles record verdicts, planner aggregates, operator decides, gate honours the operator's decision — works end to end. The previous review's biggest call-out (gate logic untouched + EscalationMessage missing) has been fully addressed.

Two things to keep an eye on as the model lands operationally:

**Operator throughput is the real test.** No technical mechanism solves this — it's a UX-and-policy question. The current shape (every promotion → operator escalation) will not scale to even a small organisation. The `operator_certified` tier deferred in plan §9.2 will eventually need to ship as some form of "I trust this artifact-content-digest, auto-approve."

**The post-promotion review is the most interesting piece operationally.** Tier 1 watching for drift after install — comparing failure rates, auth denials, suspensions — is genuinely new ground. Whether the thresholds (`1.5×`, `3×`, `2×`) are calibrated correctly will only be known after a few months of real operator feedback. The same channel reuse with `EscalationMessage` means the cost of getting it wrong is low (operators ignore noisy escalations; the surface adapts).

The work is good. The mechanic is sound. Ten polish items remain.
