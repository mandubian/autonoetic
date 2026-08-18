# Response Validation Gate

Autonoetic can validate an agent's final response after execution and before returning it to the caller. The goal is to enforce durable output constraints at the gateway boundary, not to rely on the agent's free-text claims.

When an output policy is declared, the gateway validates the produced `SpawnResult`. If validation fails, repair runs only when both conditions hold: gateway repair is enabled and the agent explicitly opts in with `io.output_policy.repair.auto: true`.

Manifest `io.returns` is the single gateway-owned output schema for the final reply. There is no separate policy schema override path.

## What The Gateway Validates

The output policy is declared as `metadata.autonoetic.io.output_policy` in
agent `SKILL.md` and currently supports these fields:

```json
{
  "required_artifacts": ["report.json", "summary.md"],
  "max_artifacts": 4,
  "max_total_size_mb": 10,
  "max_reply_length_chars": 4000,
  "prohibited_text_patterns": ["BEGIN RSA PRIVATE KEY", "/home/"],
  "min_artifact_builds": 1,
  "repair": {"auto": true, "max_attempts": 1},
  "validation_max_loops": 2,
  "validation_max_duration_ms": 2000
}
```

Validation uses authoritative runtime state, not natural-language assertions:

- `required_artifacts`: checks that the final returned file set contains each declared name.
- `max_artifacts`: limits the number of returned files.
- `max_total_size_mb`: sums authoritative byte sizes for returned content handles from the content store.
- `max_reply_length_chars`: validates the final reply string length.
- `io.returns`: validates the final reply text when it is JSON.
- `prohibited_text_patterns`: rejects replies that match forbidden regex patterns.
- `min_artifact_builds`: checks durable execution-trace evidence for successful `artifact_build` calls in the current session branch.

## How `io.returns` Reads A Reply

`io.returns` asks for a JSON object. What arrives is a *message*, and models
decorate messages. So before schema validation the gateway walks one tolerance
ladder, defined once in `autonoetic-types/src/reply_json.rs` and shared by every
reader of a reply-as-JSON:

| Rung | Source | What it does |
|---|---|---|
| 0 | — | `<think>…</think>` blocks are removed (DeepSeek, minimax-m3, Qwen emit these inline in the reply body, unlike Anthropic's native thinking channel). |
| 1 | `Whole` | The reply parses as JSON. No assumption made. |
| 2 | `CodeFence` | The first fenced block whose contents parse. Fences are scanned in order, so a `bash` fence before the payload is skipped. |
| 3 | `ProseSpan` | A balanced `{…}`/`[…]` span carved out of surrounding prose — the "Here is the handoff:" shape (#1104). |

Rung 3 is ordered so the *whole* payload wins over a fragment of it: outermost
object span (first `{` to last `}`), then outermost array span, then the longest
balanced span of either shape. Objects precede arrays because a prose
enumeration (`[1] vault the secret [2] verify`) parses as the JSON array `[1]`,
which must not beat an object handoff later in the same reply. Balancing is
string- and escape-aware, so `{"note": "}"}` is not cut short at the brace inside
the string.

Every rung must produce text that actually parses. A reply that is only prose
stays a violation — the ladder extends tolerance to a *decorated* payload, it
never invents one.

Rungs 2 and 3 mean the gateway reshaped the agent's output, so each is emitted as
an observable normalization (`markdown_code_fence`, `prose_wrapped_json`) — which
inside response validation's ambient leak scope becomes a durable `P-5.2`
DISCRETION LEAK event, not a silent convenience. The same ladder feeds the
`delegated` / `plan_id` self-report claim guards, so a prose-wrapped reply cannot
slip a truthfulness check just because it was decorated.

## Gateway-Injected `anomalies` Field

For **reasoning** agents that declare an object-shaped `io.returns` schema, the gateway injects a required `anomalies` array property at manifest-load time (`map_standard_frontmatter_to_manifest` in `parser.rs`) — no SKILL.md edit needed:

```json
{
  "type": "array",
  "description": "Standing witness contract: anything unexpected or concerning observed while completing this task (empty array if nothing). For serious observations also file an anomaly_flag.",
  "items": {
    "type": "object",
    "properties": {
      "observation": { "type": "string" },
      "subject_ref": { "type": "string" },
      "severity": { "type": "string", "enum": ["low", "medium", "high", "critical"] }
    },
    "required": ["observation"]
  }
}
```

A manifest that already declares its own `anomalies` property is left untouched (no overwrite, no duplicate `required` entry). Script agents are excluded — deterministic outputs cannot meaningfully witness or report. The rendered Output Contract (see `docs/agent-prompt-guidance.md`) gains one line naming this a standing witness contract; the agent returns `"anomalies": []` when it observed nothing.

Because reasoning agents default to **Advisory** `io.returns` enforcement (see CLI Overrides below), a reply missing `anomalies` never blocks — it logs and emits an `io.returns.advisory` causal event with `"anomalies_missing": true` in its payload, a greppable marker for future civic-health tallies (see the citizenship RFC, `docs/design/citizenship-as-a-runtime-service.md`, Part C.2/E.2).

## Repair Semantics

If response validation fails and repair is opted in, the gateway returns a repair prompt to the same agent. That prompt contains:

- the list of violations
- the attempt counter
- a reminder that the agent must repair real outputs, not merely explain the problem

The repair loop is bounded by:

- `repair.max_attempts`: agent-declared retry count (`0..8`, default from legacy `validation_max_loops - 1`)
- `response_validation.max_repair_attempts_ceiling`: gateway-level hard ceiling
- `validation_max_duration_ms`: maximum wall-clock repair window, clamped to `0..30000`

If the loop budget is exhausted, the gateway returns a final validation error to the caller.

### When No Repair Round Runs

Repair is opt-in twice over — operator (`response_validation.repair_enabled`) and
agent (`io.output_policy.repair.auto`) — and both default to off. That is the
dumb-gateway doctrine working as intended: a gateway-authored repair prompt is a
named DISCRETION LEAK (`P-5.8`), never a default.

What is *not* intended is that the distinction used to be invisible after the
fact. `validation_max_loops: 2` reads like a request for a repair round, but it
only sets the *budget*; without `repair.auto` the round never happens, and on the
async task surface the parent just sees a failed child. So a terminal validation
failure with no repair round now names the blocking switch, in a
`response.validation.repair_not_attempted` trace and an
`io.returns.repair_skipped` causal event:

| `skip_reason` | Remedy |
|---|---|
| `subsystem_disabled` | set `response_validation.repair_enabled: true` (gateway config) |
| `manifest_opt_out` | declare `io.output_policy.repair.auto: true` in the manifest |
| `zero_attempts_declared` | declare `repair.max_attempts >= 1` (or `validation_max_loops >= 2`) |
| `script_agent` | not applicable — a script re-executes deterministically; fix the script's output |

The event also carries `declared_repair_attempts` vs `effective_repair_attempts`,
so a budget clipped by `max_repair_attempts_ceiling` is visible rather than
silently reduced.

Opting in is worth it where a failed contract discards *completed side effects*.
`credential_onboarding.default` is the reference case: by the time it writes its
handoff the secret is vaulted and the credential registered, so one round of "say
that again in the declared shape" beats throwing the ceremony away.

## Structured Errors vs Auto-Repair

These are complementary but different mechanisms:

- **Structured tool errors (`ok: false`)**: default path. The gateway returns
  deterministic error details and `repair_hint`; the agent decides what to do next.
- **Response-contract auto-repair**: optional path. The gateway re-enters the
  same agent session to try fixing final output contract violations in-place.

In Phase 4.1, auto-repair is no longer implicit behavior. It runs only when:

1. `response_validation.repair_enabled: true` at gateway level.
2. `io.output_policy.repair.auto: true` in the policy.

Recommendation: prefer structured errors by default; use auto-repair only for
agents whose deliverables are repetitive, contract-heavy, and commonly repaired
in-session.

## What Agents Must Do During Repair

Repair is not a debate with the gateway. The agent must use normal tools to change the produced outputs so the next validation pass succeeds.

Typical repair actions:

- write or rewrite missing files with `content_write`
- rebuild the promoted output with `artifact_build`
- shorten or restructure the final reply to satisfy length or schema constraints
- remove forbidden text or local-path leakage from the reply

Non-repairs that will still fail:

- saying an artifact exists without returning it
- claiming a build happened without a successful `artifact_build` trace
- arguing that a response is acceptable without changing the violating output

## Specialist Guidance

### `coder.default`

`coder.default` already has the correct core split between ordinary coding work and promotable artifact-building work. For response validation and repair, its operational rules should be:

1. Treat `artifact_build` as the authoritative completion event for promotable outputs.
2. When a planner asks for a durable artifact, do not end the turn after writing files; build the artifact and return the resulting `artifact_ref`.
3. If the gateway sends a repair prompt, fix the real output set first. That usually means writing the missing file, rebuilding the artifact, or trimming the final reply.
4. Do not treat a passed `sandbox_exec` as sufficient evidence when the contract requires durable output artifacts.
5. When evaluator or auditor feedback arrives, rebuild and return a new artifact instead of claiming the prior artifact was implicitly updated.

Concretely, the coder SKILL should state that output-policy repair has the same priority as tool error repair: the agent must modify files, artifacts, or reply text until the gateway contract passes.

### `sealed_evaluator.default`

`sealed_evaluator.default` (formerly `evaluator.default`) produces a structured evaluation report and records promotion evidence. For response validation and repair, its operational rules should be:

1. Ensure the final reply remains valid JSON when the evaluation report is expected to be machine-readable.
2. Treat `promotion_record` as promotion evidence, but not as a substitute for output-policy constraints; if the contract requires files or bounded reply text, those constraints still apply.
3. If the gateway issues a repair prompt, repair the evaluation output itself. That can mean rewriting the JSON report, reducing reply size, or returning the required named report artifact.
4. Keep findings traceable to the reviewed `artifact_ref` in both the report content and promotion record.
5. If execution is blocked on approval, stop as instructed; do not force a partial report into a shape that looks complete just to satisfy validation.
6. The gateway mechanically gates `promotion_record`: `pass=true` is rejected if any finding has `error`/`critical` severity, or if `warning` findings lack a non-empty `evidence` field. This is enforced at the gateway boundary — the evaluator cannot override it.

Concretely, the evaluator SKILL should say that repair prompts are authoritative gateway feedback about the evaluation deliverable, not a request to reinterpret the findings.

## Contract Examples

### Example: promotable coder output

```yaml
metadata:
  autonoetic:
    io:
      output_policy:
        required_artifacts:
          - main.py
        min_artifact_builds: 1
        repair:
          auto: true
          max_attempts: 1
        max_reply_length_chars: 1200
        validation_max_duration_ms: 2000
```

Effect:

- the coder must return `main.py` in the final output set
- the session must contain at least one successful `artifact_build`
- the final textual reply must stay concise

### Example: evaluator JSON report

```yaml
metadata:
  autonoetic:
    io:
      returns:
        type: object
        required: [status, evaluator_pass, summary]
        properties:
          status:
            type: string
          evaluator_pass:
            type: boolean
          summary:
            type: string
      output_policy:
        max_reply_length_chars: 8000
        repair:
          auto: true
          max_attempts: 1
        prohibited_text_patterns:
          - BEGIN RSA PRIVATE KEY
```

Effect:

- the evaluator's final reply must parse as JSON
- required fields must be present
- secret-like output is blocked even if the evaluation content is otherwise valid

## CLI Overrides

Response validation can be overridden per run:

```bash
autonoetic gateway start --response-validation on
autonoetic gateway start --response-validation off
autonoetic gateway start --response-validation repair

autonoetic agent run coder.default --response-validation on
autonoetic agent run coder.default --response-validation off
autonoetic agent run coder.default --response-validation repair
```

Mode semantics:

- `on`: enable validation, disable repair loop
- `off`: disable validation entirely
- `repair`: enable validation and bounded repair retries

## Notes And Current Semantics

- `min_artifact_builds` is based on successful `artifact_build` execution traces. It measures durable evidence, not text.
- The current artifact-build evidence counts successful build calls, including reuse cases where the tool reports `reused: true`.
- `max_total_size_mb` uses content-store byte sizes for returned files rather than estimated reply text size.
- Validation runs after execution completes and before the result is returned to the caller.

## Recommended Usage

- Use output policy validation for agents that must return durable, reviewable outputs.
- Keep policies narrow and operational; validate only what the gateway can verify authoritatively.
- Prefer artifact/file requirements for coder-like agents and schema/length requirements for evaluator-like agents.
- Enable repair mode when the task is realistically repairable in-session.