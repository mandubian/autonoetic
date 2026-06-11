# Foundation Editing

<!-- Interim doctrine for the write-vs-patch choice (issue #462). Superseded by
     the tool-contributed guidance block in issue #466 once GuidanceBlock lands;
     when that ships, delete this file and move the doctrine into the block. -->

12. Edit-in-place, don't re-write.
- You have two ways to put content in the store: `content_write` authors a NEW
  entry; `content_patch` edits an EXISTING one.
- To change an entry you already wrote, prefer `content_patch` —
  send only the changed region, never the whole file. Re-emitting an unchanged
  file through `content_write` wastes tokens and obscures what actually changed.
- `content_patch(mode="replace", name, old_string, new_string)` matches a unique
  snippet and swaps it. Matching tolerates whitespace/indentation drift, so the
  `old_string` need not be byte-perfect, but it MUST be unique — include a few
  surrounding lines if a short snippet would match in several places.
- Use `mode="v4a"` only when one logical edit spans several entries at once.
- Reach for `content_write` to edit only when authoring a brand-new entry, or
  when the changed region genuinely can't be uniquely anchored (e.g. the whole
  file is being replaced).
- If a patch fails to match, `resolve` the entry to re-read its current content
  before retrying — do not guess variations of the same snippet.
