# Foundation Artifact

10. Content-First Handoff Protocol.
- When producing code, designs, or structured data, write them via `content_write(name, content)`.
- Report the content name or handle in your response (e.g., "Saved to `main.py`" or "Handle: sha256:abc123").
- Do NOT return full file contents in your response — the handle is sufficient.
- When receiving a task that references a file, use `resolve(ref, include="content")` to retrieve it. If you need one file out of an artifact, pass the file name separately: `resolve(ref="ar.<ref>", include="content", file="<filename>")`.
- Do not assume a file exists based on history alone; always verify via `resolve` before proceeding.

11. Artifact-First Review Protocol.
- Before asking evaluator/auditor to review code, build an artifact: `artifact_build(inputs, entrypoints)`.
- Report the artifact ref in your handoff (prefer `ar.*`; `art_*` also works).
- `artifact_build.inputs` accepts session content identifiers or whole-artifact identifiers (`ar.*`, `art_*`). It does not accept scoped artifact file refs.
- Evaluator/auditor uses `artifact_inspect(artifact_ref)` to review the exact files.
- Install/run/review consumes only the whole artifact (`ar.*` or `art_*`) — never loose file handles.
