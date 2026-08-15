# Autonoetic Platform Wiki

This directory contains the curated knowledge corpus for the Autonoetic gateway wiki system.
Agents can browse these docs at runtime via `wiki_list` and `wiki_get` tools.

## Adding a New Page

1. Create a `.md` file in this directory with the page content
2. Add an entry to `index.toml` with a unique `id` (matching the filename without `.md`), a `title`, and optional `tags`
3. The page will be available to agents after the next gateway bootstrap

## File Naming

- Use lowercase, hyphenated IDs: `sdk-python`, `approval-system`, `promotion-lifecycle`
- Each `.md` file is a single wiki page
- Frontmatter is not required — the entire file content is served as-is

## Citing Gateway Config Keys (machine-checked)

When a page mentions a gateway config key or an env var, cite it in the
backticked prefixed form — `` `config:llm_request_timeout_secs` `` or
`` `env:AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS` `` (for map keys use a
placeholder segment: `` `config:llm_presets.<name>.model` ``). Two unit
tests in `autonoetic-gateway/src/runtime/tools/wiki.rs` validate every
citation:

- `config:` paths are resolved against the serde field schema **parsed live
  from `autonoetic-types/src/config.rs`** — rename or remove a field without
  updating the page and the build fails;
- `env:` names must occur literally in `autonoetic-gateway/src/` or
  `autonoetic/src/`.

This is the same contract as the enforcement register's
`every_parseable_citation_resolves`: agents advise the operator from these
pages, so a hallucinated key in a wiki page is a lie every agent will
repeat. When documenting config, cite — never paraphrase a key outside the
prefixed form.
