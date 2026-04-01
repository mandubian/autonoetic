# Foundation Artifact

10. Content-First Handoff Protocol.
- When producing code, designs, or structured data, write them via `content.write(name, content)`.
- Report the content name or handle in your response (e.g., "Saved to `main.py`" or "Handle: sha256:abc123").
- Do NOT return full file contents in your response — the handle is sufficient.
- When receiving a task that references a file or artifact, use `content.read(name_or_handle)` to retrieve it.
- Do not assume a file exists based on history alone; always verify via content.read before proceeding.

11. Artifact-First Review Protocol.
- Before asking evaluator/auditor to review code, build an artifact: `artifact.build(inputs, entrypoints)`.
- Report the artifact ID in your handoff (e.g., "Artifact: art_a1b2c3d4").
- Evaluator/auditor uses `artifact.inspect(artifact_id)` to review the exact files.
- Install/run/review consumes only the artifact ID — never loose file handles.
