# RFC: Launch Readiness — Prioritized Issue Triage

Date: 2026-08-24
Status: Draft
Scope: All open GitHub issues reviewed and triaged into launch-blocking, launch-quality, and post-launch tiers. Deprecated/superseded issues closed in the same pass (see §2).

---

## 0. Summary

The tracker held ~90 open issues accumulated across eight months of parallel workstreams
(egress localization, remote-access hardening, task robustness, constitution/civics,
modularity). A large fraction is forward-looking roadmap that must not block launch.
This RFC fixes the launch set: **14 issues**, of which **7 are security-correctness
blockers**. Everything else moves behind the launch milestone explicitly so it stops
competing for attention.

A blocking observation from the same review: the working tree was found failing to
compile (`cargo build`, 20 errors in `autonoetic-gateway`) while committed `main`
builds clean. Root cause and proposed guard are in §5 — this class of failure is
exactly what the pre-launch CI posture is supposed to prevent.

## 1. Triage method

Each open issue was classified by:

1. **Is any claim still true of current code?** Several bugs were verified against
   source before judging (e.g. #650's inert digest guard turned out fixed-by-refactor;
   #808's dropped `artifact_id` turned out still live).
2. **Is the issue superseded by a later umbrella or shipped mechanism?** Old sealed-network
   and constitution-restructure slices were folded into successors (#1025, enforcement register).
3. **Would we ship without it?** Security-correctness gaps in sandbox/egress boundaries:
   no. Reliability gaps that produce wrong operator-visible behavior under stress: yes,
   but they degrade trust on day one — kept as launch-quality. Feature roadmaps: post-launch.

## 2. Closed as deprecated / superseded / done (this pass)

| Issues | Disposition |
|---|---|
| #297, #298 | Constitution restructure epic + principle/register split — shipped (`enforcement_register.rs`, versioned constitutions). |
| #300, #302 | Epic satellites never built as specced; stale specs closed under epic wind-down. |
| #333 | Validation waivers — fully implemented via closed #547–#549. |
| #608 | Divergence-robustness umbrella — all phases (#609–#613) plus #621 closed. |
| #438 | WASM tracking — P1–P4 closed; remainder lives in #546 / #443. |
| #190, #191, #192 | Sealed-network slices superseded by `sealed_network_proxy.rs` and #1025. |
| #650 | content_digest drift guard — obsolete after gateway-owned binding refactor (`bind_content_digest_if_unset`). |

Left open deliberately: #343–#350 (external CLI delegation) — commented for a
product-direction decision (workbench is currently in `DEFAULT_EXCLUDED_TOOLS`).

## 3. Tier 0 — Launch blockers (security correctness)

Ordered by urgency.

1. **#1145** — bwrap gateway-secret mask emits zero flags in production
   (`agent_dir.parent()` vs revision dir). Gateway secrets are readable from inside
   sandboxes today. This is the single worst open defect: it silently voids the
   deny-list stopgap documented in AGENTS.md.
2. **#1002** — whole-host `/` ro-bind; replace with explicit mount allow-set.
   The deny-list masking (#1145's mechanism) is acknowledged stopgap for exactly this.
   Fixing both together removes an entire class of "new secret file forgotten in the list".
3. **#988** — no write-side path taint: copying labeled content to a new path launders
   its egress label. Defeats the egress model at its core invariant.
4. **#987** — OFP send and capsule export ignore artifact labels. Federation egress
   bypasses the labeling the rest of the system enforces.
5. **#808** — `create_from_intent` drops/mismatches `artifact_id`; integration test on
   main asserts a field the response builder never writes. Verified live at
   `agent_revision.rs` response construction. Either the test or the builder is wrong;
   both cannot ship.
6. **#649** — vestigial `AgentRevisionStatus::Rejected`: unconstructed, unhandled;
   one careless match arm away from reintroducing the create→promote loop already
   killed once. Remove the variant.
7. **#1078** — constitution/YieldReason drift: Ri-0.12 says 11 causes and ManualStop
   terminal; code has 12 and ManualStop is a resumable pause. Constitution-text/code
   divergence fails the repo's own mechanical-enforcement doctrine; fix code or text,
   then re-run lock recompute if the constitution changes.

Conditional blocker: **#897 + #815** (federated messaging semantics, outbound OFP stub).
If launch messaging claims federation capability, these are Tier 0; if federation stays
explicitly experimental, demote to Tier 2 *and* say so in docs/config defaults.

## 4. Tier 1 — Launch quality

Ship-blockers only in the sense that launching with them visibly degrades reliability:

- **#651** — non-atomic revision create (get-then-insert race).
- **#855** — planner delegation lacks exit criterion/turn budget → child divergence.
- **#842** — `soft_budget_tokens` unset everywhere → context governor never fires on
  128K models; budget exhaustion arrives as truncation instead of managed compression.
- **#775, #776** — typed child-failure contract + loop-safe `expected_outputs`.
- **#779** — provider failover actually executed at the driver boundary.
- **#916, #884** — stack-budget guards: startup path and the 4200-line router dispatch
  arm have each been patched reactively; neither has a failing-first test.
- **#1134, #956, #1034** — test-infra credibility: seed-race flake, last standalone
  binaries, `#[cfg(test)]` contract tests to domain binaries. CI signal is a
  precondition for everything above staying fixed.
- **#378** — finish GateService migration; three production `create_approval` callers
  remain outside the single door (`session.rs`, `user_profile.rs`, `scheduler/runner.rs`).

## 5. Tier 2 — Post-launch backlog (explicitly non-blocking)

Grouped so owners can be assigned per theme rather than per issue:

- **Modularity**: #1116, #1118, #1119, #1127, #1039, #2.
- **Egress completion** (§9 introspection, authoring aid, room legibility,
  declassification): #993, #994, #967, #978, #979, #971, #948, #903 umbrella with
  remaining phases #905, #907, #910.
- **Task robustness remainder**: #778, #780, #792, #764.
- **Memory & autonoesis**: #811, #812, #813, #819.
- **Evolution/governance**: #817, #880, #891, #816, #844, #822, #810, #244, #577.
- **Perf**: #595 tracker with #588–#594.
- **Singleton sessions**: #683, #686.
- **Capsule**: #284, #285, #286.
- **Prompt burden**: #1087.
- **Long-tail wishlist (April/May vintage)**: #3, #5, #9, #10, #16, #17, #22, #25–#27,
  #29, #31, #131, #203, #232, #233, #379, #384-era items, #394, #399, #431, #443, #546,
  #604, #605, #630, #641, #686-adjacent, #894, #1018, #368, #325, #316, #286-adjacent.

Recommendation: apply a `post-launch` label to every Tier 2 issue and filter the board
by milestone from here on.

## 6. Build hygiene incident (2026-08-24)

During this triage, `cargo build` failed with 20 errors in `autonoetic-gateway` on the
checked-out working tree while committed `HEAD` builds clean in a fresh worktree. The
breakage came from ~51 uncommitted modifications (an in-flight refactor touching
`llm/`, `lifecycle.rs`, `execution.rs`, `checkpoint.rs`) left half-migrated: trait
members referenced but removed (`request_timeout`/`ttfb_timeout` vs `LlmDriver`),
fields renamed at one call site only (`session_phase`, `llm_ttfb_timeout_secs`).

Committed main is healthy; the process allowed it not to be. Proposed guards:

1. **Never commit on a red tree** — `cargo check --workspace --tests` locally before
   any commit (already cheap post-#920/#921).
2. **CI compile gate on PRs** — #921 exists; extend sharding so lib-test compilation
   failures are caught within minutes, not nightly (#1142 showed how long red can hide).
3. **Worktree discipline for long refactors** — multi-file refactors like the one found
   here should live on a branch/worktree, not directly atop a checkout of `main`.

## 7. Execution order proposal

```
Week 1   #1145 + #1002 (sandbox secrets/host-fs), #808, #649, #1078 (+ lock recompute)
Week 2   #988, #987 (egress write-side + federation labels), #1134/#956/#1034 (CI signal)
Week 3   #855, #842, #775, #776, #779, #651, #916, #884, #378
Then     federation go/no-go decision → #897/#815 in or out; freeze; launch candidate
```

Tier 0 items are all small-to-medium mechanically verifiable fixes; none requires the
RFC-amendment machinery except possibly #1078 depending which side wins.
