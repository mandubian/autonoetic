# Postmortem: Planner Confabulated a Weather Agent From Context Loss

**Session:** `session-76a8d5c6`
**Incident turn:** ~21:08:53
**Date:** 2026-09-13
**Model:** `qwen3.8-27b` (planner.default)
**Scope:** One fabrication chain: truncated child reply → history trim → confabulated artifact identity → false "fraud" accusation. Companion mechanical fixes in #1332; doctrine hardening in this PR.

---

## Executive summary

The session was a legitimate Fibonacci build. The planner had no memory of
weather work (`knowledge_search weather → 0`; the only memory rows are the
session's own Fibonacci lessons). Yet at 21:08:53 the planner spontaneously
produced a `weather_agent` identity — complete file triplet
(`weather_agent.py` / `SKILL.md` / `test_weather_agent.py`), a fabricated
`artifact_ref` (`ar.000…`), and then compared the *real* Fibonacci output
against that fiction and accused the child of fraud.

The analysis is **confabulation under context loss, primed by the repo's
canonical example** — not memory recall and not a gateway bug:

- The child reply (831 ch) was truncated to a 406-ch preview when it crossed
  the parent's wake context; the spill ref (`full_result_ref: cnt_ab07a638`)
  was present but never read.
- History trimming then dropped ~9 older messages, including the task framing
  and the real artifact identity.
- Under uncertainty the model completed with the highest-prior content in its
  context: weather is this repo's canonical example (coder SKILL.md
  "(e.g. weather agent)", test fixtures, docs), so the fabricated triplet took
  weather's shape and the fabricated ref took the `ar.` prefix with filler.

## Failure chain

| Step | Mechanism | What failed |
|---|---|---|
| 1 | Child reply 831 → 406 ch | The truncation envelope carried `result_truncated: true` + `full_result_ref` + an explicit note; the model did not act on it. Nothing forced a read. |
| 2 | History trimmed (9 msgs) | No gateway-derived fact anchor survived the trim — the task, refs, and file names existed only in prose the model could no longer see. |
| 3 | Ref fabrication | `ar.000…` was emitted as a structured fact. The claim-verification framework had the exact check (`ClaimKind::ArtifactBuilt`) scaffolded but returning `Unverified`. |
| 4 | Example priming | Under uncertainty, completion went to the canonical example, not to "unknown". |
| 5 | False fraud verdict | The planner compared real Fibonacci output against its fiction and blamed the child — a memory-vs-reality mismatch resolved in the worst possible direction. |

## What worked

- The spill/ref machinery itself (`result_truncated`, `full_result_ref`) — the
  facts were preserved and available; the model just had no forcing function.
- The store: the fabricated ref never resolved to anything, which is what
  makes mechanical detection possible.

## Fixes

**Mechanical (#1332):**

1. `ClaimKind::ArtifactBuilt` is now enforced: every `ar.*` / `art_*` string
   cited in a reply's *structured* fields must resolve in the artifact-ref
   store; a fabricated ref becomes `unknown_artifact_ref` and feeds the
   bounded repair loop, whose hint directs the agent back to `workflow_state`
   / wake `artifact_refs` / `full_result_ref`. Prose mentions are not claims.
2. `ChildStateNotification.artifact_refs`: wake notifications carry the
   gateway-observed refs created in the child's session, so the parent holds
   the true identity at wake even when the summary was truncated.

**Doctrine (this PR):**

3. Both planners gain an anti-confabulation rule: a truncated reply is a
   preview (read `full_result_ref` before judging); when reality contradicts
   memory, suspect your own context first; never reconstruct refs or file
   names from memory.
4. De-priming: coder's "(e.g. weather agent)" example removed so the doctrine
   no longer hands every model the same confabulation target. (Docs and test
   fixtures are not prompt-visible and are unchanged.)

## Residual risk

A 27B model under aggressive trimming can still confabulate *prose* (the
gateway cannot refute a fabricated file name that was never created). The
mechanical net catches everything that becomes actionable — structured refs
are verified, true refs are pinned at wake — but multi-hop prose drift is a
model-capability limit, not a gateway fixable.

## Follow-ups

- [x] Wire `ClaimKind::ArtifactBuilt` (#1332)
- [x] Surface `artifact_refs` on wake notifications (#1332)
- [x] Anti-confabulation doctrine for both planners (this PR)
- [x] De-prime the weather example in coder doctrine (this PR)
- [ ] Consider a gateway-derived "session facts" block rendered every turn
      (superset of the wake fix; deferred — the wake fix + ref verification
      close the actionable paths at a fraction of the prompt cost)
- [ ] Watch for further `unknown_artifact_ref` repair-loop hits as a signal of
      residual confabulation pressure (corpus measurement, §5.3 style)
