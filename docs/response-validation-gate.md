# Response Validation Gate

Autonoetic can validate an agent's final response after execution and before returning it to the caller. The goal is to enforce durable output constraints at the gateway boundary, not to rely on the agent's free-text claims.

When a response contract is declared, the gateway validates the produced `SpawnResult`. If validation fails and repair is enabled, the gateway gives the same agent a bounded chance to repair the actual outputs and try again.

## What The Gateway Validates

The response contract is declared as `metadata.autonoetic.response_contract` and currently supports these fields:

```json
{
  "required_artifacts": ["report.json", "summary.md"],
  "max_artifacts": 4,
  "max_total_size_mb": 10,
  "max_reply_length_chars": 4000,
  "output_schema": {
    "type": "object",
    "required": ["status", "summary"],
    "properties": {
      "status": {"type": "string"},
      "summary": {"type": "string"}
    },
    "additionalProperties": true
  },
  "prohibited_text_patterns": ["BEGIN RSA PRIVATE KEY", "/home/"],
  "min_artifact_builds": 1,
  "validation_max_loops": 2,
  "validation_max_duration_ms": 2000
}
```

Validation uses authoritative runtime state, not natural-language assertions:

- `required_artifacts`: checks that the final returned file set contains each declared name.
- `max_artifacts`: limits the number of returned files.
- `max_total_size_mb`: sums authoritative byte sizes for returned content handles from the content store.
- `max_reply_length_chars`: validates the final reply string length.
- `output_schema`: validates the final reply text when it is JSON.
- `prohibited_text_patterns`: rejects replies that match forbidden regex patterns.
- `min_artifact_builds`: checks durable execution-trace evidence for successful `artifact.build` calls in the current session branch.

## Repair Semantics

If response validation fails, the gateway returns a repair prompt to the same agent. That prompt contains:

- the list of violations
- the attempt counter
- a reminder that the agent must repair real outputs, not merely explain the problem

The repair loop is bounded by two contract fields:

- `validation_max_loops`: maximum retry count, clamped to `1..8`
- `validation_max_duration_ms`: maximum wall-clock repair window, clamped to `0..30000`

If the loop budget is exhausted, the gateway returns a final validation error to the caller.

## What Agents Must Do During Repair

Repair is not a debate with the gateway. The agent must use normal tools to change the produced outputs so the next validation pass succeeds.

Typical repair actions:

- write or rewrite missing files with `content.write`
- rebuild the promoted output with `artifact.build`
- shorten or restructure the final reply to satisfy length or schema constraints
- remove forbidden text or local-path leakage from the reply

Non-repairs that will still fail:

- saying an artifact exists without returning it
- claiming a build happened without a successful `artifact.build` trace
- arguing that a response is acceptable without changing the violating output

## Specialist Guidance

### `coder.default`

`coder.default` already has the correct core split between ordinary coding work and promotable artifact-building work. For response validation and repair, its operational rules should be:

1. Treat `artifact.build` as the authoritative completion event for promotable outputs.
2. When a planner asks for a durable artifact, do not end the turn after writing files; build the artifact and return the resulting `artifact_id`.
3. If the gateway sends a repair prompt, fix the real output set first. That usually means writing the missing file, rebuilding the artifact, or trimming the final reply.
4. Do not treat a passed `sandbox.exec` as sufficient evidence when the contract requires durable output artifacts.
5. When evaluator or auditor feedback arrives, rebuild and return a new artifact instead of claiming the prior artifact was implicitly updated.

Concretely, the coder SKILL should state that response-contract repair has the same priority as tool error repair: the agent must modify files, artifacts, or reply text until the gateway contract passes.

### `evaluator.default`

`evaluator.default` already produces a structured evaluation report and records promotion evidence. For response validation and repair, its operational rules should be:

1. Ensure the final reply remains valid JSON when the evaluation report is expected to be machine-readable.
2. Treat `promotion.record` as promotion evidence, but not as a substitute for response-contract outputs; if the contract requires files or bounded reply text, those constraints still apply.
3. If the gateway issues a repair prompt, repair the evaluation output itself. That can mean rewriting the JSON report, reducing reply size, or returning the required named report artifact.
4. Keep findings traceable to the reviewed `artifact_id` in both the report content and promotion record.
5. If execution is blocked on approval, stop as instructed; do not force a partial report into a shape that looks complete just to satisfy validation.

Concretely, the evaluator SKILL should say that repair prompts are authoritative gateway feedback about the evaluation deliverable, not a request to reinterpret the findings.

## Contract Examples

### Example: promotable coder output

```yaml
metadata:
  autonoetic:
    response_contract:
      required_artifacts:
        - main.py
      min_artifact_builds: 1
      max_reply_length_chars: 1200
      validation_max_loops: 2
      validation_max_duration_ms: 2000
```

Effect:

- the coder must return `main.py` in the final output set
- the session must contain at least one successful `artifact.build`
- the final textual reply must stay concise

### Example: evaluator JSON report

```yaml
metadata:
  autonoetic:
    response_contract:
      max_reply_length_chars: 8000
      output_schema:
        type: object
        required: [status, evaluator_pass, summary]
        properties:
          status:
            type: string
          evaluator_pass:
            type: boolean
          summary:
            type: string
      prohibited_text_patterns:
        - BEGIN RSA PRIVATE KEY
      validation_max_loops: 2
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

- `min_artifact_builds` is based on successful `artifact.build` execution traces. It measures durable evidence, not text.
- The current artifact-build evidence counts successful build calls, including reuse cases where the tool reports `reused: true`.
- `max_total_size_mb` uses content-store byte sizes for returned files rather than estimated reply text size.
- Validation runs after execution completes and before the result is returned to the caller.

## Recommended Usage

- Use response validation for agents that must return durable, reviewable outputs.
- Keep contracts narrow and operational; validate only what the gateway can verify authoritatively.
- Prefer artifact/file requirements for coder-like agents and schema/length requirements for evaluator-like agents.
- Enable repair mode when the task is realistically repairable in-session.