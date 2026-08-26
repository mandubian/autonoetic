# `content_patch` — editing content-store entries in place

`content_write` authors a new entry (or replaces one wholesale). `content_patch`
edits an existing entry by sending **only the changed region**. Both return the
same shape: registered `name`, short `ref` (`cnt_<8 hex>`), and `sandbox_path`
(`/tmp/<name>`) for `sandbox.exec`.

Gated on `WriteAccess` — an agent without it does not see the tool.

The tool's own description and the prompt guidance block
(`editing.content_patch`) tell an agent *when* to reach for it. This page is the
contract: what a match means, when it refuses, and what the two modes accept.

## Two modes

| Mode | Scope | Shape |
|---|---|---|
| `replace` (default) | one entry | fuzzy find-and-replace of `old_string` → `new_string` |
| `v4a` | several entries at once | a unified-diff-like patch addressed by content name |

## `replace`: the match must be unique

A patch applies only when `old_string` identifies **exactly one** region. Two
matches is a refusal, not a coin flip — `replace_all` is how you say you meant
all of them.

Matching is deliberately tolerant, because a model-authored `old_string`
routinely differs from the stored bytes in ways that carry no meaning: trailing
whitespace, re-indentation, collapsed interior spacing. Rather than force the
agent to re-emit the whole file over a lost space, the engine widens tolerance in
ordered strategies and stops at the **first that yields a unique match**:

1. `Exact` — literal substring.
2. `LineTrimmed` — lines equal after trimming each end.
3. `WhitespaceNormalized` — lines equal after collapsing internal runs.

Ordering is the safety property: the loosest strategy is only consulted when
stricter ones found nothing, so tolerance never silently overrides a precise
match.

The two line-based strategies also **re-indent the replacement** to the matched
region's base indentation. An edit authored at the wrong indent level still lands
correctly — which matters because indentation is exactly what a model reproduces
least reliably.

Engine: `runtime/fuzzy_match.rs`.

## `v4a`: multi-entry patches

A custom unified-diff-like format, with entries addressed by content **name**
(names are path-like):

```text
*** Begin Patch
*** Update File: src/main.rs
@@ optional context @@
```

Hunks apply through the same fuzzy engine, so trivial whitespace or indentation
drift in *context* lines does not break a patch — the common failure mode of
strict diff formats when the diff was authored by a model rather than by `diff`.

Parser: `runtime/v4a.rs`.

## When it refuses

A refusal is preferable to a wrong edit, so the tool declines rather than guesses
when:

- `old_string` matches nothing at any tolerance;
- it matches more than once and `replace_all` was not set;
- the named entry does not exist (`content_patch` edits; it never creates).

Refusals arrive in the standard envelope
([`tool-errors.md`](tool-errors.md)) with a repair hint. The right response to
"could not be uniquely anchored" is a larger `old_string` with more surrounding
context — not a retry of the same patch, and not falling back to rewriting the
whole entry unless the region genuinely cannot be anchored.

## Related

- [`../internals/storage/content-store.md`](../internals/storage/content-store.md)
  — content addressing, names, refs, and visibility
- [`tool-errors.md`](tool-errors.md) — the failure envelope
- [`../archived/content-patch-tool.md`](../archived/content-patch-tool.md) —
  the design record, including the open questions
