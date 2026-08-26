# Autonoetic documentation

Three entry points, then eight directories sorted by **what you came here for**.

| Start here | For |
|---|---|
| [`start/concepts.md`](start/concepts.md) | New to Autonoetic — what a gateway, capability, and constitution are, and why agents don't hold privileges |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | The system: components, data flow, security model, execution modes |
| [`AGENTS.md`](AGENTS.md) | The agent model: roles, routing, `SKILL.md`, capabilities, lifecycle |

## Where things live

| Directory | Holds | You are |
|---|---|---|
| [`start/`](start) | Tutorials — a first success, in order | new here |
| [`guide/`](guide) | How-to, one operator or author task per doc. [`guide/runbooks/`](guide/runbooks) for procedures with a pass/fail outcome | trying to do something |
| [`reference/`](reference) | Contracts you code against: CLI, config keys, HTTP API, SQLite schema, `SKILL.md` fields, tool errors, capabilities | integrating |
| [`concepts/`](concepts) | Why it is built this way — philosophy, separation of powers, planner principles | trying to understand |
| [`internals/`](internals) | How the runtime does it, by subsystem: [`prompt/`](internals/prompt) [`sandbox/`](internals/sandbox) [`storage/`](internals/storage) [`session/`](internals/session) [`egress/`](internals/egress) | changing the runtime |
| [`constitution/`](constitution) | The governance corpus: signed versions, the [enforcement register](constitution/enforcement-register.md), signing, key management, roadmap | reading the law |
| [`wiki/`](wiki) | Short digests served to **agents** at runtime via `wiki_list` / `wiki_get` — not the human reference | editing what agents read |
| [`proposals/`](proposals) | In-flight design and RFC work, one status table in [`proposals/README.md`](proposals/README.md) — a test asserts every proposal is listed | proposing a change |
| [`reports/`](reports) | Dated and immutable: audits, validations, [postmortems](reports/postmortems), comparative studies | looking for evidence |
| [`archived/`](archived) | Superseded. Historical record, never source of truth | doing archaeology |
| [`diagrams/`](diagrams) | Rendered visual maps (published via GitHub Pages) | looking at pictures |

**This file is a map, not a catalogue.** Per-file lists go stale — the
directory listing is the catalogue. Each directory's contents are named for
what they describe.

## Which directory does my doc go in?

Apply the first test that answers:

- **Would changing this break someone outside the repo?** Config keys, CLI
  flags, HTTP routes, SQLite columns, `SKILL.md` fields, error envelopes →
  `reference/`. How the labeler resolves a sink, how the governor compresses →
  `internals/`.
- **Does it have a goal and an order?** Then it is a `guide/`, not a
  `reference/` (which has coverage and no order).
- **Would it survive a rewrite of the code?** Then `concepts/`, not
  `internals/`.
- **Is the work still in flight?** `proposals/`. The moment it ships, the
  *description of behaviour* moves to `internals/` or `reference/` and the
  proposal is archived with a pointer — a shipped proposal is never the live
  description. If no live doc describes it, the proposal is **promoted** rather
  than archived, otherwise archiving deletes the documentation.
- **Written once, dated, never updated?** `reports/`. If you want to update it,
  you wanted a `guide/runbooks/` doc.

## Conventions

- `kebab-case.md`, except `README.md`, `ARCHITECTURE.md`, `AGENTS.md`.
- No kind prefixes or suffixes in filenames (`plan-`, `spec-`, `-rfc`,
  `-plan`, `-design`) — the directory carries the kind.
- No dates in filenames outside `reports/`, where the date leads:
  `YYYY-MM-DD-slug.md`.
- **A cited path is a promise that it resolves.** `docs_link_guard`
  (`autonoetic-gateway/src/docs_link_guard.rs`) fails the build on a dangling
  `docs/…` citation or broken relative link, in Markdown, agent bundles, and
  production Rust alike. Intentional exceptions go in `.link-guard-allow` with
  a reason.

Two directories have paths the runtime depends on and must not be moved:
`constitution/versions/**` and `constitution/CURRENT` (loaded at startup,
digest-signed), and `wiki/**` with its `index.toml` (read at bootstrap).

The reorganization this layout came from, including what is still to merge, is
[`design/docs-reorganization-plan.md`](proposals/docs-reorganization.md).
