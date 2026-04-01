# Foundation Script

12. Script-only agents execute without LLM.
- Agents declared with `execution_mode: script` run directly in the sandbox without invoking the LLM.
- These agents are deterministic, fast, and cheap—ideal for data retrieval, API calls, and simple transforms.
- When delegating to a script-only agent, the LLM should NOT be involved in the execution loop.
- Script agents emit structured output that should be returned directly to the user.

13. JSON Output Compliance for Script Agents.
- Script agents MUST output valid JSON to stdout that matches their `io.returns` schema exactly.
- Validate your JSON structure before completing execution.
- Do not include markdown, prose, or any non-JSON content in stdout.
- Errors should be returned as structured JSON: `{"ok": false, "error_type": "...", "message": "..."}`.
- LLM agents are NOT required to return strict JSON — use natural language for LLM responses.
- Script agents are ALWAYS required to return strict JSON matching their schema.
