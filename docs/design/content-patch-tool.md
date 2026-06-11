# Design: `content_patch` — token-efficient targeted edits on content-store entries

## Problem

Today an agent that wants to change one line of a file it already wrote must
re-emit the **entire file** through `content_write`. For a large source file
this burns output tokens proportional to file size on every iteration of the
write → test → fix loop — the loop the coder specialist runs constantly.

There is currently **no targeted-edit tool** in Autonoetic. The only edit paths
are:

- `content_write` — full-file rewrite → new SHA-256 handle, re-points the name.
- `sandbox_exec` — `sed`/`awk`/scripts inside the sandbox (brittle, off-ledger
  for the content store, and the agent still has to author the script).

This is the gap Hermes closes with its `patch` tool (`mode='replace'` + V4A),
which its study shows is the *strongly preferred* path for editing existing
code, and which Claude-family models drive most reliably in `replace` mode.

## Goal

Add a core-tier `content_patch` tool that edits an existing **content-store
entry** in place (semantically): read current bytes → apply edit(s) → write a
new immutable blob → re-point the name. The agent sends only the changed region,
not the whole file.

Two input formats, shipped together:

1. **`replace`** — `old_string` / `new_string` with fuzzy matching. Default,
   single-entry. The format Claude models handle most reliably.
2. **`v4a`** — the Hermes-style multi-file diff (`*** Update File: <name>` with
   `@@`/space/`-`/`+` hunks). Multi-entry; entries are addressed by content
   **name** (names are path-like, slashes allowed).

## Why the content-store surface (not sandbox / workbench)

Chosen surface: **content-store entries**. It is the exact locus of the token
waste (re-`content_write` to change one line), it is immutable-store-native
(produces a new handle, stays on the causal ledger), and it is the loop the
coder specialist actually runs. Sandbox-file and workbench surfaces are
out of scope here — sandbox edits overlap `sandbox_exec`; workbench already has
`workbench_diff`/`reconcile` for operator-facing edits.

## Content-store mechanics this relies on (verified)

All in `autonoetic-gateway/src/runtime/content_store.rs` unless noted.

| Need | Mechanism | Location |
|---|---|---|
| Resolve name/ref/handle → handle | `resolve_name_or_handle_to_handle` | `content_store.rs:507` |
| Read current bytes | `read` / `read_by_name_or_handle` | `content_store.rs:156`, `:562` |
| Write new blob → handle | `write` (dedup, readonly) | `content_store.rs:126` |
| Re-point name (mutable, last-writer-wins) | `register_name_with_visibility` | `content_store.rs:269` |
| Preserve visibility | `manifest.visibility: HashMap<Handle, Visibility>` | `content_store.rs:55` |
| Short ref `cnt_<8hex>` | `handle_to_short_alias` | `content_store.rs:103` |
| Agent reads content back (to author `old_string`) | `resolve` tool (`include="content"`) | `runtime/tools/resolve.rs` |

Names are **mutable pointers**; re-registering the same name with a new handle
overwrites silently (`manifest.names.insert`, `content_store.rs:237`). This is
exactly the in-place-edit semantics we want.

## Tool spec

### Name, tier, capability

- Name: `content_patch`.
- Tier: **core** — inherited automatically from the `content_` prefix
  (`config/tools.yaml:5`). No override needed.
- Capability gate: `Capability::WriteAccess { .. }` — mirror `content_write`
  (`runtime/tools/content.rs:24`).

### Schema

```jsonc
{
  "mode": "replace" | "v4a",          // default "replace"

  // --- mode = "replace" ---
  "name": "src/main.rs",              // registered content name (re-pointing
                                      // needs a name, so refs/handles aren't accepted)
  "old_string": "...",                // snippet to find (fuzzy)
  "new_string": "...",                // replacement
  "replace_all": false,               // default false → require unique match

  // --- mode = "v4a" ---
  "patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@ ...\n End Patch",

  "include_canonical_digest": false   // same opt-in as content_write
}
```

### Result (mirrors `content_write`, `content.rs:111`)

`replace` mode and single-file `v4a` return the `content_write` shape plus a
display diff:

```jsonc
{
  "ok": true,
  "name": "src/main.rs",
  "alias": "ab12cd34",
  "ref": "cnt_ab12cd34",
  "sandbox_path": "/tmp/src/main.rs",
  "bytes_written": 1234,
  "visibility": "session",
  "strategy": "line-trimmed",            // which fuzzy strategy matched: exact | line-trimmed | whitespace-normalized
  "diff": "@@ -10,3 +10,3 @@\n-old\n+new",
  "canonical_digest": "sha256:..."       // only if requested
}
```

Multi-file `v4a` returns `{ "ok": true, "files": [ <per-file result>, ... ] }`.

## Fuzzy matching engine

New module `autonoetic-gateway/src/runtime/fuzzy_match.rs`. Port a **subset** of
Hermes's 9 strategies — the ones that matter for Claude-authored edits, tried in
order, first match wins. **As built, 3 strategies ship** (indentation handling
is folded into the replacement step rather than being a separate strategy):

1. **exact** — literal substring.
2. **line-trimmed** — lines equal after trimming each end.
3. **whitespace-normalized** — lines equal after collapsing internal runs.

The two line-based strategies **re-indent** the replacement to the matched
region's base indentation, so an edit authored at the wrong indent still lands
correctly. Exact never re-indents (it matched verbatim). Each strategy reports
its name so the result can surface which one fired.

Deferred (add only if real edits miss): escape-normalized, unicode-normalized,
block-anchor, context-aware.

**Uniqueness rule:** with `replace_all=false`, more than one match is an error
(don't guess which). `replace_all=true` replaces every **exact**-strategy match
only (fuzzy + all is too dangerous); for line-based matches `replace_all` does
not apply and a non-unique match is still an error.

The display diff is a compact `-old / +new` snippet (no diff crate is vendored,
so it is not a minimal LCS diff). Diff is **display only** — the new content is
the substituted string, never reconstructed from the diff.

## V4A parser

New module `autonoetic-gateway/src/runtime/v4a.rs`.

- Parse `*** Begin/End Patch`, `*** Update File: <name>`, `@@` context hints,
  ` `/`-`/`+` hunk lines into operations keyed by content **name**.
- **Two-phase apply** (Hermes pattern): validate every hunk against current
  bytes via the fuzzy engine first — **no store writes if any hunk fails** —
  then apply all. Prevents half-applied multi-file edits.

**Operation scope for v1:**

- `Update File` — supported (fuzzy-apply hunks to the named entry).
- `Add File` — supported (equivalent to a `content_write` of a new name).
- `Delete File` / `Move File` — **deferred.** The store has no
  unregister/rename for names today (`manifest.names` is insert-only in the
  public API). Adding `Delete`/`Move` means new `ContentStore` methods
  (`unregister_name`, `rename`) + visibility cleanup; out of scope for v1.
  Parser recognizes them and returns a clear "not yet supported" error rather
  than silently dropping.

## Prompt guidance (the part that actually drives adoption)

Hermes's key finding: the tool alone changes nothing — a **system-prompt block**
tells each model family to *prefer* targeted edits over full rewrites, and the
model then emits the right call. We must do the same or agents will keep
calling `content_write`.

Action: locate where Autonoetic assembles the agent/specialist system prompt and
tool guidance (candidate: coder specialist bundle under `agents/specialists/`,
and/or runtime prompt assembly). Inject, for editing existing content:

> To edit existing content you already wrote, prefer `content_patch`
> (`mode="replace"`): match a unique snippet and swap it — do **not** re-send the
> whole file through `content_write`. Use `mode="v4a"` only when one edit spans
> several entries. Reach for `content_write` only to author a new entry or when
> the changed region can't be uniquely anchored.

(Exact injection site to be confirmed during implementation — flagged as the
first impl step because it gates the whole feature's value.)

## Failure / anti-loop behavior

**As built**, every match failure carries the escalation hint immediately (no
per-`(session, name)` counter):

> Stop retrying variations of the same snippet. Either (1) `resolve` the entry
> fresh to re-read current content, (2) use a longer, more unique `old_string`
> with surrounding context lines, or (3) `content_write` the whole entry if the
> region can't be uniquely anchored.

The original design tracked consecutive failures and only escalated after 3.
That counter was dropped for v1 — surfacing the hint on the first failure is
simpler and strictly more helpful. A counter could be re-added later in
`NativeToolRunContext` if telemetry shows agents ignoring the first hint.

## Files touched

| File | Change |
|---|---|
| `autonoetic-gateway/src/runtime/fuzzy_match.rs` | new — 4-strategy matcher |
| `autonoetic-gateway/src/runtime/v4a.rs` | new — V4A parse + two-phase apply |
| `autonoetic-gateway/src/runtime/tools/content_patch.rs` | new — the tool |
| `autonoetic-gateway/src/runtime/tools/mod.rs` | register tool |
| agent prompt assembly (TBD site) | inject edit-format guidance |
| `config/tools.yaml` | none (inherits `content_` → core) |

## Tests

- `fuzzy_match`: unit tests per strategy + uniqueness/`replace_all` rules.
- `v4a`: parse round-trip; two-phase abort leaves store untouched on a bad hunk.
- Tool integration (`autonoetic-gateway/tests/`): write entry → patch (replace)
  → `resolve` returns patched bytes, name re-points to new handle, **old handle
  still readable** (immutability preserved), visibility preserved.
- v4a multi-entry: two `Update File` hunks in one patch both land atomically.
- anti-loop: 3 failures surface the escalation hint.

## Open questions

1. **Prompt injection site** — exact file that builds specialist tool guidance.
   First thing to pin down; the token win is theoretical without it.
2. **`Delete`/`Move` in v4a** — confirm we're fine deferring (needs new store
   methods). Recommend deferring.
3. **`diff` crate** — reuse an existing dependency or add `similar`? Check the
   workspace lockfile before adding.
