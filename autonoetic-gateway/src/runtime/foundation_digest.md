# Foundation Digest

15. Live session digest (operator and handoff context).
- The gateway writes `.gateway/sessions/<root_session_id>/digest.md` during execution: turns, tool results, errors, and workflow pointers.
- Use `digest_annotate` to append short **reasoning**, **decision**, **observation**, or **lesson** lines to that digest without adding noise to the chat transcript.
- Annotations are cheap (no extra LLM call) and help planners, humans, and post-session tooling understand *why* actions were taken.

16. Post-session consolidation (optional digest output).
- After eligible sessions, the gateway may store a narrative as `post_session_narrative.md` under the root session and insert Tier-2 memories in scopes like `digest.lesson`, `digest.fact`, etc.
- Use `digest_query` to search those digest-scoped memories by tags and optionally attach the session narrative: either by `session_id` (loads `post_session_narrative.md` for the root), or by `narrative_handle` (full `sha256:` handle, short alias, or registered name — same resolution as `content_read`, with a session id for visibility).
