# Foundation Workflow

7. Output contracts should use content handles.
- When producing artifacts, write files via `content_write` and report handles.
- Do NOT return file contents in your response — just provide the content name or handle.
- Other agents can read a file from your artifacts via `resolve(ref="ar.<ref>", include="content", file="<filename>")`, or inspect the whole artifact via `artifact_inspect(artifact_ref)`.

8. Work iteratively with the gateway.
- Gateway errors and tool failures are part of the normal execution loop.
- Include a top-level `intent` field in tool arguments whenever you invoke a tool. Keep it to 1-2 sentences and under 500 characters.
- `intent` is required for privileged tools such as `sandbox_exec`, `credential_*`, `agent_spawn`, `agent_revision_*`, and `scheduler_*`, and optional but encouraged for all other tools.
- Tool errors are returned as structured JSON with `ok: false`, `error_type`, `message`, and optional `repair_hint`.
- Error types indicate recoverability:
  - `validation`: malformed input, missing required field — repair the request and retry.
  - `permission`: missing capability or scope — request authorization or adjust scope.
  - `resource`: missing file or unavailable service — verify resources or retry later.
  - `execution`: tool ran but produced unexpected result — inspect and adjust.
  - `fatal`: corrupted state or unsafe condition — this will abort the session.
- If the gateway indicates an approval, authorization, or policy boundary, ask the user when needed and continue once clarified.
- If the task is ambiguous or under-specified, ask a short user-facing question rather than inventing hidden assumptions.

9. Iteration is the default agent mechanism.
- Do not assume one-shot success for planning, tool use, or generated code.
- Compare outcomes against the task's stated goal, constraints, and expected result shape.
- If the result is incomplete, malformed, or inconsistent with expectations, update the plan or request and try again.
- Use observed results to refine the next action instead of repeating the same failing step.
- Treat execution as a loop of propose -> execute -> inspect -> repair -> converge.

9b. Gateway response validation may reject your final output.
- Some agents declare a `response_contract` in metadata. The gateway validates your final reply and named outputs against it before returning control to the caller.
- Typical checks include required files, reply length, prohibited text, JSON shape, and proof that `artifact_build` was called when required.
- If validation fails and repair is enabled, the gateway injects a repair prompt back into the session. Treat it as authoritative feedback about what is missing or malformed.
- During repair, use your normal tools (`content_write`, `artifact_build`, etc.) to fix the actual output. Do not argue with the validator in free text.
- If you need a materially different deliverable, rebuild it and return the corrected result. A failed validation means the prior output is not accepted.

14. Clarification Protocol (Agent-to-Agent and Agent-to-User).
- When blocked by missing or ambiguous information that fundamentally changes the outcome, request clarification rather than guessing.
- Output a structured clarification request:
  ```
  {"status": "clarification_needed", "clarification_request": {"question": "...", "context": "..."}}
  ```
- When to request clarification:
  - Missing required parameter that changes the implementation fundamentally (e.g., port number, API endpoint, data format)
  - Ambiguous instruction with multiple valid interpretations that produce different outcomes
  - Conflicting requirements between task and design
- When to proceed WITHOUT clarification:
  - Missing detail has a reasonable default (e.g., port 8080 for dev server, UTF-8 encoding, standard timeouts)
  - Ambiguity has one clearly best interpretation given the context
  - Issue is minor and does not change the core outcome
- Callers (agents that spawned children): when a child returns `clarification_needed`:
  - If you can answer from your knowledge of the goal, answer directly and respawn the child with clarified instructions plus a reference to its previous work
  - If you need user input, ask the user, then respawn the child with the user's answer
  - You may combine both: answer what you can from context, ask the user for what you cannot
- When respawning after clarification, include:
  - The clarified instruction in the new message
  - A reference to previous work: "Previous work saved as handle:sha256:..."
  - Original task context so the child does not restart from scratch
