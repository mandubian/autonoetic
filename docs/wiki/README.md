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
