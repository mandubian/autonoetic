# Tool Reference Guide

Agents interact with the gateway through tools. This page lists the major tool categories and their purposes.

## Content Tools (Working Memory)

| Tool | Description |
|------|-------------|
| `content_write` | Write content with visibility (private/session/global). Default: session |

## Artifact Tools (Trust Boundary)

| Tool | Description |
|------|-------------|
| `artifact_build` | Build immutable artifact from session content |
| `artifact_inspect` | Inspect artifact files, entrypoints, digest |
| `artifact_exec` | Execute an artifact entrypoint in sandbox with artifact-aware analysis |
| `artifact_prepare` | One-pass preflight: resolve credentials + approval before execution |
| `resolve` | Resolve any artifact/content handle (metadata/files/content) |

## Knowledge Tools (Durable Memory)

| Tool | Description |
|------|-------------|
| `knowledge_store` | Upsert a fact with scope, tags, confidence, retention, visibility |
| `knowledge_recall` | Retrieve fact by ID if visible |
| `knowledge_search` | FTS5 search within scope, optional tag AND-filtering |
| `digest_query` | Read post-session narrative / digest content |

## Agent Tools

| Tool | Description |
|------|-------------|
| `agent_spawn` | Spawn child agent session |
| `agent_discover` | Find reusable agents matching an intent |
| `agent_inspect` | Inspect any installed agent's metadata/capabilities |
| `self_describe` | Describe your own identity, capabilities, rights, history |

## Execution Tools

| Tool | Description |
|------|-------------|
| `sandbox_exec` | Execute a command in an isolated sandbox |
| `execution_search` | Search raw tool execution traces within sessions |

## Observability Tools (Cross-Session)

| Tool | Description |
|------|-------------|
| `observability_search` | Discover published session reports by text search |
| `observability_read` | Read an observability resource by URI |

## Workflow Tools

| Tool | Description |
|------|-------------|
| `workflow_wait` | Wait for child agent(s) to complete (blocking join) |
| `workflow_state` | Read mechanical state of a task (once per wake, never in a loop) |

## Revision and Promotion Tools

| Tool | Description |
|------|-------------|
| `agent_revision_create` | Create immutable revision from artifact |
| `agent_revision_create_from_intent` | Create revision from semantic intent (preferred) |
| `agent_revision_promote` | Move alias to a revision (activates it) |
| `agent_revision_rollback` | Roll alias back to previous revision |
| `agent_revision_list` | List revisions for an agent |
| `agent_revision_inspect` | Inspect revision metadata and status |
| `agent_revision_diff` | File-level diff between two revisions |
| `promotion_record` | Record evaluator/auditor evidence for promotion |
| `promotion_query` | Query promotion records |

## Credential Tools

| Tool | Description |
|------|-------------|
| `credential_check` | Check if credentials exist for a service |
| `credential_setup` | Set up credentials with automated or human-assisted entry |
| `credential_request` | Use stored credentials in HTTP requests without seeing secrets |

## Wiki Tools

| Tool | Description |
|------|-------------|
| `wiki.list` | List all available wiki pages (id + title + tags) |
| `wiki.get` | Get full content of a wiki page by id |

## Web Tools

| Tool | Description |
|------|-------------|
| `web_fetch` | Fetch a URL (requires NetworkAccess capability) |
| `web_search` | Search the web (requires NetworkAccess capability) |
| `web_call` | Make structured HTTP calls with credential injection |

## Scheduled Task Tools

| Tool | Description |
|------|-------------|
| `scheduler_cron_create` | Create a scheduled cron job |
| `scheduler_cron_list` | List scheduled jobs |
| `scheduler_cron_pause` | Pause a scheduled job |
| `scheduler_cron_resume` | Resume a paused job |
| `scheduler_cron_cancel` | Cancel a scheduled job |

## Skill Install Tool

| Tool | Description |
|------|-------------|
| `skill_install` | Fetch a remote SKILL.md and install it as a new local agent |
