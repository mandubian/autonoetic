# Content Visibility

How content written by one session becomes readable by another. This is the
model behind `content.write`'s `visibility` argument, the `named_outputs` a
parent sees on a child's implicit artifact, and why a child's result can be read
by its caller at all.

## The one rule

**Reachability is decided by where a write propagated, never by searching other
sessions.** A reader's lookup is a fixed outward walk over its own manifest, then
the root's, then the global one. Nothing scans a peer's namespace.

That is what makes `Private` a guarantee rather than a hint: private content is
unreachable because it was never published anywhere else, not because a reader
politely declines to look.

## The three levels

`ContentVisibility` (`autonoetic-gateway/src/runtime/content_store.rs`):

| level | published to | who can read it |
|---|---|---|
| `Private` | the writing session's manifest only | the writing session |
| `Session` (**default**) | the writing session **and the root session** | every session under that root |
| `Global` | writing session, root, and the global manifest | any session |

`content.write` with no `visibility` argument means `Session` — the collaborative
default. `Session` is why a planner can read what its child produced, and why the
promotion gates can read what the coder built.

## Writes

`register_name_with_visibility(session, name, handle, visibility)` always
registers the `name` **and** an 8-hex short alias (`cnt_<alias>`) in the writing
session's manifest, then propagates both:

- `Session` → also into the **root** session's manifest
- `Global` → also into the root's and the global manifest

Propagation depends on the writing session's manifest knowing its root, which is
set by `set_root_session(child, root)` when a child is spawned
(`runtime/tools/agent.rs`, logged as `Set up hierarchical content namespace for
child agent`).

**If that link is missing, `Session` silently behaves like `Private`.** Nothing
propagates, the write still reports success, and the parent's later read simply
fails to find the name. The link has only two production call sites — the
`agent.spawn` path and one JSON-RPC path — and a failure to set it is logged as a
warning, not an error. When cross-session reads mysteriously come up empty, check
for that warning first.

## Reads

Two resolvers, both walking outward from the caller:

- **by ref** — `resolve_alias_with_root`: caller → root → global
- **by name** — `resolve_name_with_root`: caller → root → global

`is_handle_visible(session, handle)` answers the same question for a handle
rather than a name, over the same three manifests. `create_implicit_artifact`
uses it to decide which of a child's named outputs to list for the parent, which
is why a child's `Private` content never appears in `named_outputs`.

Prefer refs. Names are a **shared namespace with no owner**: the root manifest is
keyed by name, so two sessions under one root that write `notes.md` overwrite each
other's entry there, and a later read by name gets whichever wrote last. A `cnt_`
ref is derived from the content hash and cannot collide.

## Parent, or root?

The propagation target is the **root**, never the immediate parent. For a flat
tree (`root/child`) those are the same session and the distinction is invisible.
For nested delegation (`root/A/B`), `B`'s `Session` write lands in `root`'s
manifest, and `A` finds it by walking `A → root` — because both share a root, not
because `A` is `B`'s parent.

A consequence worth being deliberate about: there is **no way to express "share
with my caller only"**. A child handing a result back to its delegator publishes
to the entire root tree, so every peer under that root can read it. That is
correct if a root session is one trust domain; if it is not, the model has no
level that says so.

## Sibling reads

Peers under one root read each other's `Session` content through the root
manifest — that is the intended collaboration path (an architect writes a design
document, the coder reads it by name).

There used to be an additional fallback that scanned every sibling session
directory and read that session's manifest directly. It could not resolve
anything: it passed a directory basename (`architect-1`) where a full session id
(`root/architect-1`) was required, so every lookup missed. It was removed rather
than repaired — repairing it would have granted peers access to `Private` and
undeclared content (including each session's `session_history`), which is the
opposite of the rule at the top of this document. Declared sharing already
resolves at the root.

## Testing against this model

Set up the root link explicitly, or `Session` visibility does nothing and tests
pass for the wrong reason:

```rust
store.set_root_session("root/child-a", "root")?;
store.set_root_session("root/child-b", "root")?;
```

The invariants are pinned in `content_store.rs` tests:
`sibling_reads_declared_session_content_via_root`,
`sibling_cannot_read_private_content_by_name`,
`sibling_cannot_read_private_content_by_ref`, and
`sibling_cannot_read_undeclared_content_by_name`.
