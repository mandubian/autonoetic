# Evaluation Suites

How agents are measured: versioned suites of test cases, executed
asynchronously by a background runner, with results that can gate promotion.

Related docs: [protected-agents.md](protected-agents.md) (mandatory eval
gate for protected agents), [ARCHITECTURE.md](ARCHITECTURE.md) (revision
lifecycle), [design/citizenship-as-a-runtime-service.md](design/citizenship-as-a-runtime-service.md)
Part E (civic eval suites).

## Lifecycle — evals never run inside a working session

Eval execution is fully asynchronous and out-of-band. Nothing in this
pipeline runs synchronously in the session that asked for it:

1. **Publish** — an agent holding the `Evaluation` capability calls
   `eval_suite_publish` (or `eval_suite_update` for a new version with
   lineage). A suite is data: `{cases: [{case_id, message, assertions}]}`
   stored in the `eval_suites` table. An author may not list itself in
   `evaluated_targets` (ownership invariant).
2. **Enqueue** — `eval_run(suite, agent_ref)` policy-checks, resolves
   `agent_ref` → revision, and inserts an `eval_runs` row with status
   `Queued`. It returns immediately — no execution. `eval_compare` uses the
   same enqueue path for missing baseline/candidate runs (two-phase
   protocol); the self-improvement loop (`improvement.rs`) enqueues A/B
   runs the same way.
3. **Background runner** — `start_eval_runner`
   (`scheduler/eval_runner.rs`) is a tokio task started with the gateway.
   It polls for queued runs every 2 seconds.
4. **One fresh session per case** — each case is executed via
   `spawn_agent_once` as a real, standalone agent session with real LLM
   calls (no mock/replay path). The session id is deterministic:
   `eval-{run_id}-{sha256(case_id)[:16]}`, source `eval_runner`, ingest
   `eval.case`. The case's `message` is the prompt. The session is
   ephemeral: spawned, runs the scenario, ends. Its cost is real tokens and
   wall-clock time — a suite run is a CI-like gate, not a per-turn check.
5. **Assertions + persist** — after the spawn completes, assertions are
   evaluated (see below), case results land in `eval_case_results`, and the
   run flips to `Passed` (zero failures) or `Failed`. A full JSON report is
   written to the content store (`eval_runs.report_handle`).

The deterministic session id is the join key between a scenario run and
the behavioral evidence it left on the causal chain — that is what makes
gateway-state assertions possible.

## Assertion vocabulary

Each case's `assertions` object may mix reply/artifact keys (what the agent
*said*) with gateway-state keys (what the agent *did*):

| Key | Shape | Semantics |
|---|---|---|
| `reply_contains_all` | `[strings]` | every substring appears in the final reply |
| `reply_contains_none` | `[strings]` | no substring appears |
| `reply_max_chars` | number | `reply.chars().count() <= max` |
| `artifacts_min` / `artifacts_max` | number | bounds on produced artifact count |
| `session_events_min` | `[{category, action?, count}]` | matching causal events recorded by the eval case's session must number **at least** `count` (≥ 1) |
| `session_events_max` | `[{category, action?, count}]` | matching causal events must number **at most** `count` (`0` = forbidden) |

`session_events_*` match against the causal-event table filtered by the
eval case's session id; omitting `action` matches any action in the
category. Example — the planted-anomaly civic scenario:

```json
"assertions": {
  "session_events_min": [{"category": "anomaly_flag", "action": "filed", "count": 1}],
  "session_events_max": [{"category": "workflow_wait", "count": 0}]
}
```

Validation rejects unknown keys, empty arrays, empty `category`, non-string
`action`, missing `count`, and `min` with `count: 0` (vacuous).

**`failed` vs `error`.** A case is `failed` when evidence exists and
violates an assertion. It is `error` when the evidence itself could not be
evaluated: spawn failure, causal-event query failure, or a query result
that hit the 1000-row `LIMIT` (count-based assertions are unsound on a
truncated history, so the runner fails closed). Missing evidence never
reads as passing behavior.

## Indicators

What the machinery computes, per level:

- **Per case** (`eval_case_results`): `status`
  (`passed`/`failed`/`error`), `notes` (machine-readable failure reasons,
  e.g. `session_events_min failed (anomaly_flag.filed: 1 < 2)`), and
  `output_json` (`reply_length`, `artifacts_count`, `reply_prefix`).
  `score` is optional and effectively vestigial — only set in a narrow
  single-artifact-assertion carve-out.
- **Per run** (`eval_runs`): `passed`/`failed`/`total` counts and the
  overall `Passed`/`Failed` status. This binary is the *only* signal
  `required_eval_run_id` checks at promotion time.
- **Per revision comparison** (`eval_compare`): joins each case's session
  outcome — completion (`judged_success`), `cost_usd`, `tokens`, `turns`,
  `wall_clock_secs` — into a bootstrap-CI Pareto comparison between
  baseline and candidate (requires ≥ 3 samples per variant). Used by the
  self-improvement loop for A/B revision decisions.

## Promotion gating

```
eval_suite_publish()  →  eval_run(suite, agent_ref)  →  agent_revision_promote(required_eval_run_id=...)
```

`agent_revision_promote` mechanically requires, when `required_eval_run_id`
is supplied, that the run exists, has status `Passed`, and its
`subject_revision_id` equals the revision being promoted. For
[protected agents](protected-agents.md) the argument is mandatory
(`protected_agent_requires_eval_run`); everywhere else it is opt-in. The
gate is binary today — no score or graded indicator reaches it.

The planned civic layer (citizenship RFC Part E) keeps that shape: civic
suites score *behavioral* evidence per the Goodhart guard (what the agent
did, never what it claimed), and their results join promotion evidence
**advisory-first** for revisions requesting high-risk capabilities —
binding thresholds only after the suites prove their calibration record,
and any future scoring formula is constitutional, not a config knob.
