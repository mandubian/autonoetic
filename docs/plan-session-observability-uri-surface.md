# Plan: Session Observability URI Surface

**Date:** 2026-04-12  
**Status:** Draft v2  
**Related:** `docs/agent-learning.md`, `docs/fts-session-search.md`, `docs/content-store.md`, `docs/ARCHITECTURE.md`

---

## Problem

Agents need a safe way to learn from prior sessions, recover from errors, and inspect prior outcomes without turning all logs into a globally readable corpus.

Today the observability surface is fragmented across multiple storage layers that are not mechanically joined:

| Layer | What it stores | Join status |
|---|---|---|
| **Causal chain** (`causal_events` + JSONL) | Every execution event: session start/end, tool invoke/completed, approvals, LLM calls, hibernation, errors | `event_id` populated, hash-chained |
| **Execution traces** (`execution_traces`) | Tool results: stdout/stderr/exit_code/duration/arguments/result/approval | `event_id` = **`None`** — the join is broken |
| **Session transcripts** (`session_transcripts`) | Full `Vec<Message>` history as content blobs with FTS excerpt | Handle stored in causal event payload |
| **Content store** (SHA-256 blobs) | All large data: transcripts, tool payloads, artifacts, reports | Referenced by handles |
| **Session report files** | Derived markdown/json/html summaries in session directory | Not in causal chain |

Current gaps:

1. **`execution_traces.event_id` is always `None`** — the most important join in the system doesn't work. Execution traces and causal events are parallel, unconnected tables.
2. **Errors exist in the causal chain** but the report layer doesn't use causal `event_id` as its error identity.
3. **`execution.search` is a second search tool** that overlaps heavily with what `observability.search` should do.
4. **Report publishing is hardcoded** in the session close path — there is no hook mechanism for pluggable post-session processing.
5. **`payload_ref` in the live report** points at session-local filenames under `report-data/`, which are not durable identifiers.
6. **No redaction model** exists for cross-session observability access.

The goal: make the causal chain the authoritative spine, fix the execution-trace join, and build a URI-addressable observability surface on top with proper access control.

---

## Scope

### In Scope for v1

- Fix `execution_traces.event_id` join
- Gateway event hook system (replaces hardcoded publish logic)
- Published final session report via hooks
- Report node URIs with causal `event_id` as the universal node key
- Two observability tools: `observability.search` and `observability.read`
- Same-root redacted drill-down into causal events and execution traces
- Privileged cross-root redacted drill-down

### Explicitly Deferred from v1

- Transcript access as part of the default cross-session learning surface
- Evidence file access as part of the default cross-session learning surface
- Direct filesystem path exposure to agents
- Redaction profile design (acknowledged as open question, see dedicated section)
- Replacing the causal chain as the execution audit source of truth

### Backward Compatibility

Backward compatibility is **not** a goal for this plan.

This plan explicitly allows the following breaking changes:

- **Remove `execution.search`** entirely — replaced by `observability.search`
- Make `.gateway/sessions/...` report files projections instead of the canonical source of truth
- Replace filename-based `payload_ref` references with content-backed identifiers
- Change the shape of `session_report.json` to use `event_id` as node keys
- Remove hardcoded report publishing from the session close path (replaced by hooks)

---

## Terminology

| Term | Meaning | Storage |
|---|---|---|
| **Causal event** | Immutable execution event — the authoritative record of "what happened" | `causal_events` (SQLite) + `causal_chain.jsonl` (hash-chained) |
| **Execution trace** | Detail view of a tool invocation — stdout/stderr/exit_code/duration | `execution_traces` (SQLite), joined to causal event via `event_id` |
| **Transcript** | Raw message history (`Vec<Message>`) | Content store blob + `session_transcripts` (FTS excerpt) |
| **Session report** | Derived summary for operators and downstream agents | Content store blob + `published_session_reports` catalog |
| **Published report** | A report intentionally exposed outside the originating root session | Catalog table, not a content visibility class |
| **Hook** | A reactive binding from a gateway event to an action | Gateway config + hook executor |

Rules:

- Causal events answer "what happened?" (the spine)
- Execution traces answer "what exactly did the tool return?" (detail view of a causal event)
- Transcripts answer "what was said?" (conversation history)
- Session reports answer "what should another agent start from?" (derived entry point)

---

## Design Principles

1. **Causal chain is the spine.** Every observable thing produces a causal event. Execution traces are detail views of those events. Reports are derived projections. The `event_id` is the universal join key.
2. **Published report first.** Cross-root agents start from a redacted published report, not a raw transcript or trace.
3. **Drill-down is explicit.** A report node links to deeper resources, but the deeper read is separately authorized.
4. **No new synthetic IDs.** Use `event_id` for timeline events and errors, `request_id` for approvals, `session_id` for agents, `artifact_id` for artifacts. Introduce no new ID namespaces.
5. **No filesystem paths in agent-facing APIs.** Filesystem layout is implementation detail, not resource identity.
6. **Keep policy orthogonal to addressing.** A URI identifies a resource. Access to that resource is a separate ACL decision.
7. **One authoritative interface.** Two tools: `observability.search` and `observability.read`. Replace overlapping legacy tools.
8. **Hooks, not hardcoding.** Report publishing, notification, and any post-event processing are driven by configurable hooks, not embedded in session lifecycle code.

---

## Chosen Design Decisions

1. **Causal `event_id` as universal node key:** No `report_event_id` or `report_error_id`. Timeline nodes and error nodes both use the causal `event_id`. This eliminates an entire ID namespace and makes the report-to-causal join trivial.
2. **Publication model:** `published_session_reports` catalog table + FTS index. No new content visibility class.
3. **Tool surface:** Two tools — `observability.search` (discover) and `observability.read` (inspect). `observability.resolve` is merged into `observability.read` with `view: metadata | full`.
4. **Hook system:** Generic event hook mechanism replaces hardcoded report publishing. Report generation is just another hook consumer.
5. **Legacy cleanup:** Remove `execution.search`. Keep `session.search`/`session.peek` as transcript-only tools.

---

## Gateway Event Hook System

### Why Hooks

The current system hardcodes two reactive behaviors:
- Approval resolution → signal delivery (in `scheduler/signal.rs`)
- Session close → report finalization (in `execution.rs`)

This is inflexible. The right model is a generic hook system where any gateway event can trigger any action. Report publishing becomes a hook consumer, not embedded logic.

### Hook Configuration

```yaml
hooks:
  - on: "session.closed"
    action: "publish_report"
    async: true

  - on: "session.closed"
    action: "agent.spawn"
    agent_id: "report-generator.default"
    message: "Generate cross-session digest for root {{root_session_id}}"
    async: true

  - on: "approval.resolved"
    action: "deliver_signal"
    async: true
```

### Hook Event Types

| Event | Trigger | Available Context |
|---|---|---|
| `session.closed` | Session reaches terminal state | `root_session_id`, `session_id`, `agent_id`, `close_reason`, `turn_count` |
| `session.suspended` | Session suspends at approval/user-input boundary | `root_session_id`, `session_id`, `agent_id`, `reason`, `request_id` |
| `approval.resolved` | Approval request is decided | `request_id`, `agent_id`, `session_id`, `decision`, `decided_by` |
| `approval.requested` | New approval request created | `request_id`, `agent_id`, `session_id`, `kind` |
| `workflow.join.satisfied` | All tasks in a join group complete | `workflow_id`, `root_session_id`, `task_ids` |
| `artifact.created` | New artifact persisted | `artifact_id`, `session_id`, `root_session_id`, `kind` |
| `agent.promoted` | Agent revision promoted to active | `agent_id`, `revision_id`, `promotion_id` |
| `emergency_stop` | Emergency stop triggered | `root_session_id`, `trigger_kind`, `mode` |

### Hook Action Types

| Action | Description |
|---|---|
| `publish_report` | Write session report to content store + publish to catalog |
| `agent.spawn` | Spawn an agent with the event context as input |
| `deliver_signal` | Send a notification signal to a session |
| `http.callback` | POST the event payload to an external URL |

### Hook Execution Model

- Hooks execute **after** the triggering event is committed to the causal chain and SQLite
- **Async hooks** return immediately; the action runs in a background tokio task
- **Sync hooks** block the triggering operation until the action completes (use sparingly)
- Failed hooks are retried up to 3 times with exponential backoff
- Hook state is tracked in a `hook_deliveries` table (idempotency key = event_id + hook_id)

### Implementation Location

New file: `autonoetic-gateway/src/scheduler/hooks.rs`

### Impact on Existing Code

- `scheduler/signal.rs` `send_workflow_join_satisfied` → becomes a hook registered for `workflow.join.satisfied`
- `scheduler/signal.rs` approval signal delivery → becomes a hook registered for `approval.resolved`
- `execution.rs` report finalization on session close → becomes a `publish_report` hook registered for `session.closed`

---

## Canonical URI Grammar

### Scheme

All observability resources use a single canonical scheme:

```text
autonoetic://observability/...
```

- Scheme is always lowercase: `autonoetic`
- Authority is fixed: `observability`
- Paths are lowercase except stored identifiers, which preserve their original value
- Canonical resource identity never depends on query parameters

### Identifier Encoding Rules

1. `root_session_id` is used as stored
2. `session_id` is encoded as a **single percent-encoded path segment**
3. Content handles are rendered without the `sha256:` prefix in path segments
4. **No new synthetic IDs.** All node keys reuse existing durable IDs:
   - Timeline events and errors: **`event_id`** (from the causal chain)
   - Approvals: **`request_id`**
   - Agents: **`session_id`** (encoded)
   - Artifacts: **`artifact_id`**

### Grammar

```text
observability-uri = "autonoetic://observability" root-resource

root-resource     = "/roots/" root-session-id [ root-tail ]

root-tail         = ""
                  / "/report"
                  / "/report/overview"
                  / "/report/agents"
                  / "/report/agents/" encoded-session-id
                  / "/report/timeline"
                  / "/report/timeline/" event-id
                  / "/report/approvals"
                  / "/report/approvals/" request-id
                  / "/report/errors"
                  / "/report/errors/" event-id
                  / "/narrative"
                  / "/sessions"
                  / "/sessions/" encoded-session-id [ session-tail ]
                  / "/artifacts/" artifact-id
                  / "/content/sha256/" sha256-hex

session-tail      = ""
                  / "/causal"
                  / "/causal/" event-id
                  / "/causal/by-seq/" event-seq
                  / "/traces"
                  / "/traces/" trace-id
                  / "/digest/live"
                  / "/transcript"            ; reserved
                  / "/evidence/" evidence-id ; reserved
```

### Canonicality Rules

- `autonoetic://observability/roots/<root>/report` is the canonical cross-root entry point
- `report/timeline/<event_id>` and `report/errors/<event_id>` both use the causal chain's `event_id` — no new ID namespace
- `sessions/<encoded_session_id>/causal/by-seq/<event_seq>` is convenience addressing; `event_id` is canonical
- `content/sha256/<hex>` is only returned from gateway-generated links

---

## Exact Path Layout

### Published Root Resources

| Resource | URI Template | Key | Notes |
|---|---|---|---|
| Root report | `.../roots/{root}/report` | `root_session_id` | Canonical cross-root entry point |
| Report overview | `.../roots/{root}/report/overview` | — | Summary-only projection |
| Report agents | `.../roots/{root}/report/agents` | — | Lists agent-session nodes |
| Report agent node | `.../roots/{root}/report/agents/{encoded_session}` | `session_id` | Per-agent detail |
| Report timeline | `.../roots/{root}/report/timeline` | — | Lists event nodes |
| Report timeline node | `.../roots/{root}/report/timeline/{event_id}` | `event_id` | Backlinks to causal/trace |
| Report approvals | `.../roots/{root}/report/approvals` | — | Published approval summary |
| Report approval node | `.../roots/{root}/report/approvals/{request_id}` | `request_id` | Reuses existing ID |
| Report errors | `.../roots/{root}/report/errors` | — | Published error summary |
| Report error node | `.../roots/{root}/report/errors/{event_id}` | `event_id` | Same ID as the causal event |
| Root narrative | `.../roots/{root}/narrative` | — | Post-session narrative |

### Same-Root / Privileged Drill-Down

| Resource | URI Template | Key |
|---|---|---|
| Session node | `.../roots/{root}/sessions/{encoded_session}` | `session_id` |
| Causal collection | `.../roots/{root}/sessions/{encoded_session}/causal` | — |
| Causal event | `.../roots/{root}/sessions/{encoded_session}/causal/{event_id}` | `event_id` |
| Causal by sequence | `.../roots/{root}/sessions/{encoded_session}/causal/by-seq/{event_seq}` | `event_seq` |
| Trace collection | `.../roots/{root}/sessions/{encoded_session}/traces` | — |
| Execution trace | `.../roots/{root}/sessions/{encoded_session}/traces/{trace_id}` | `trace_id` |
| Live digest | `.../roots/{root}/sessions/{encoded_session}/digest/live` | — |
| Artifact | `.../roots/{root}/artifacts/{artifact_id}` | `artifact_id` |
| Content blob | `.../roots/{root}/content/sha256/{hex}` | content digest |

### Reserved Future Resources

| Resource | URI Template | v1 Status |
|---|---|---|
| Transcript | `.../sessions/{encoded_session}/transcript` | Reserved |
| Evidence | `.../sessions/{encoded_session}/evidence/{evidence_id}` | Reserved |

### Example URIs

```text
autonoetic://observability/roots/demo-root/report
autonoetic://observability/roots/demo-root/report/agents/demo-root%2Fcoder.default-7b2f
autonoetic://observability/roots/demo-root/report/timeline/1f524b9a-0a2f-46b3-9af1-c95d8b18a78e
autonoetic://observability/roots/demo-root/report/errors/1f524b9a-0a2f-46b3-9af1-c95d8b18a78e
autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f/causal/1f524b9a-0a2f-46b3-9af1-c95d8b18a78e
autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f/traces/9b6c58aa-d77b-4d1d-a9d2-66f7fbac6e46
autonoetic://observability/roots/demo-root/narrative
autonoetic://observability/roots/demo-root/artifacts/art_a1b2c3d4
autonoetic://observability/roots/demo-root/content/sha256/5f3a4f0c9d2f0b1e...
```

Note: `report/timeline/{event_id}` and `report/errors/{event_id}` may point to the same causal event. The URI path communicates the *view* (timeline entry vs error detail), not a different identity.

---

## Report Link Semantics

Every report node exposes a `links` object with URI backlinks.

### Linking Rules

| Report Node | Required Backlinks |
|---|---|
| Root report | `overview`, `agents`, `timeline`, `approvals`, `errors`, `narrative` |
| Agent node | `session`, `causal`, `traces`, optional `artifact`/`narrative` |
| Timeline node | `session`, optional `causal`, `trace`, `approval`, `artifact`, `content` |
| Approval node | `session`, optional `causal`, `trace` |
| Error node | `session`, optional `causal`, `trace`, `content` |

### Report Schema (evolved)

```json
{
  "event_id": "1f524b9a-0a2f-46b3-9af1-c95d8b18a78e",
  "category": "tool_invoke",
  "action": "completed",
  "status": "ERROR",
  "summary": "sandbox.exec failed: exit=1",
  "links": {
    "self": "autonoetic://observability/roots/demo-root/report/timeline/1f524b9a-0a2f-46b3-9af1-c95d8b18a78e",
    "session": "autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f",
    "causal": "autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f/causal/1f524b9a-0a2f-46b3-9af1-c95d8b18a78e",
    "trace": "autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f/traces/9b6c58aa-d77b-4d1d-a9d2-66f7fbac6e46"
  }
}
```

No `report_event_id` or `report_error_id`. The `event_id` from the causal chain is the node key in both `report/timeline/` and `report/errors/`.

---

## Access Model

### Default Policy

1. **Same root session**, agents may read redacted report, narrative, causal, and trace resources
2. **Cross-root**, ordinary agents may read only published report resources
3. **Cross-root**, privileged introspection agents may read redacted narrative, causal, and trace resources
4. **Transcript** and **evidence** are not in the v1 surface
5. The gateway applies disclosure filtering before returning observability content

### Actor Classes

| Actor Class | Meaning |
|---|---|
| **same-session** | Same `session_id` that produced the resource |
| **same-root peer** | Different session under the same `root_session_id` |
| **cross-root ordinary** | Different root, normal agent |
| **cross-root introspector** | Different root, agent with cross-root observability scopes |

### Access Matrix

| Resource | same-session | same-root peer | cross-root ordinary | cross-root introspector | Returned Form |
|---|---|---|---|---|---|
| Published report / overview / agents / timeline / approvals / errors | yes | yes | yes, if published | yes | redacted summary JSON |
| Root narrative | yes | yes | no | yes | redacted text |
| Causal event | yes | yes | no | yes | redacted JSON |
| Execution trace | yes | yes | no | yes | redacted JSON |
| Live digest | yes | yes | no | no in v1 | redacted markdown |
| Transcript | reserved | reserved | no | reserved | n/a in v1 |
| Evidence | reserved | reserved | no | reserved | n/a in v1 |
| Content blob | inherit parent | inherit parent | no discovery | inherit parent | redacted blob |

### Publication Model

- Store report body in the content store
- Track publication in `published_session_reports` catalog table (not a content visibility class)
- FTS index over report summaries for discovery
- Report publishing is driven by hooks, not hardcoded in session close

---

## Capability Names

### ReadAccess Scope Family

```text
observability/report/published/*
observability/report/root/*
observability/narrative/root/*
observability/narrative/cross-root/*
observability/causal/root/*
observability/causal/cross-root/*
observability/trace/root/*
observability/trace/cross-root/*
observability/transcript/root/*        ; reserved
observability/transcript/cross-root/*  ; reserved
observability/evidence/root/*          ; reserved
observability/evidence/cross-root/*    ; reserved
```

These work with the existing prefix-based scope checks in `policy.rs`.

### Capability Profiles

| Profile | Scopes |
|---|---|
| **published-report-reader** | `observability/report/published/*` |
| **same-root-observer** | `observability/report/root/*`, `observability/narrative/root/*`, `observability/causal/root/*`, `observability/trace/root/*` |
| **cross-root-introspector** | `observability/report/published/*`, `observability/narrative/cross-root/*`, `observability/causal/cross-root/*`, `observability/trace/cross-root/*` |

---

## Tool Contracts

### `observability.search`

Discovers observability resources. **Replaces `execution.search`.**

```json
{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "Search text matched against published report summaries, causal event payloads, and indexed metadata."
    },
    "resource_types": {
      "type": "array",
      "items": {
        "type": "string",
        "enum": [
          "report",
          "causal_event",
          "execution_trace",
          "approval"
        ]
      },
      "description": "Optional filter. Default: [\"report\"]."
    },
    "scope": {
      "type": "string",
      "enum": ["published", "same_root", "cross_root_privileged"],
      "default": "published"
    },
    "root_session_id": { "type": "string" },
    "session_id": { "type": "string" },
    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 },
    "cursor": { "type": "string" }
  },
  "required": ["query"],
  "additionalProperties": false
}
```

Response:

```json
{
  "ok": true,
  "query": "weather approval failure",
  "scope_applied": "published",
  "results": [
    {
      "uri": "autonoetic://observability/roots/demo-root/report",
      "resource_type": "report",
      "root_session_id": "demo-root",
      "published": true,
      "title": "Session report: demo-root",
      "summary": "2 agents, 1 approval, 1 failed sandbox run",
      "links": {
        "self": "autonoetic://observability/roots/demo-root/report",
        "overview": "autonoetic://observability/roots/demo-root/report/overview"
      }
    }
  ],
  "next_cursor": null
}
```

### `observability.read`

Fetches a resource by URI. **Merges the former `resolve` and `read` tools.** The `view` parameter controls depth:

- `metadata` — URI, type, links, children (no body). This is what `resolve` used to do.
- `summary` — compact body with key fields.
- `full` — maximum redacted detail allowed by ACL.

```json
{
  "type": "object",
  "properties": {
    "uri": {
      "type": "string",
      "description": "Canonical or non-canonical observability URI."
    },
    "view": {
      "type": "string",
      "enum": ["metadata", "summary", "full"],
      "default": "summary"
    },
    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50 },
    "cursor": { "type": "string" },
    "include_links": { "type": "boolean", "default": true }
  },
  "required": ["uri"],
  "additionalProperties": false
}
```

**`view=metadata` response** (collection):

```json
{
  "ok": true,
  "canonical_uri": "autonoetic://observability/roots/demo-root/report/timeline",
  "resource_type": "report_timeline_collection",
  "access_class": "published",
  "links": {
    "self": "autonoetic://observability/roots/demo-root/report/timeline",
    "parent": "autonoetic://observability/roots/demo-root/report"
  },
  "children": [
    {
      "uri": "autonoetic://observability/roots/demo-root/report/timeline/1f524b9a-0a2f-46b3-9af1-c95d8b18a78e",
      "resource_type": "report_timeline_event",
      "title": "sandbox.exec failed"
    }
  ]
}
```

**`view=full` response** (leaf — execution trace):

```json
{
  "ok": true,
  "canonical_uri": "autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f/traces/9b6c58aa-d77b-4d1d-a9d2-66f7fbac6e46",
  "resource_type": "execution_trace",
  "access_class": "same_root",
  "body": {
    "trace_id": "9b6c58aa-d77b-4d1d-a9d2-66f7fbac6e46",
    "event_id": "1f524b9a-0a2f-46b3-9af1-c95d8b18a78e",
    "tool_name": "sandbox.exec",
    "success": false,
    "exit_code": 1,
    "error_summary": "sandbox.exec failed: exit=1",
    "stdout": "",
    "stderr": "Traceback ..."
  },
  "links": {
    "self": "autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f/traces/9b6c58aa-d77b-4d1d-a9d2-66f7fbac6e46",
    "causal": "autonoetic://observability/roots/demo-root/sessions/demo-root%2Fcoder.default-7b2f/causal/1f524b9a-0a2f-46b3-9af1-c95d8b18a78e"
  }
}
```

`view=full` never bypasses redaction or access boundaries.

---

## Open Question: Redaction

The current `DisclosureState` does per-reply filtering on agent output. It does not address the cross-session observability use case.

**Open questions that need resolution before Phase 4:**

1. **What gets redacted for same-root peers?** If agent A reads agent B's causal event from the same root, does it see the raw `sandbox.exec` arguments (which may contain API keys in environment variables)? The current system never exposes this.

2. **What gets redacted for cross-root introspectors?** Even privileged agents should not see raw secrets. But they may need to see tool arguments to understand failures.

3. **Is there a single redaction profile or per-actor-class profiles?** The simplest model: one profile, applied uniformly. The most flexible: different stripping rules per actor class.

4. **Who defines redaction rules?** The gateway? The originating agent's manifest? A site-wide config?

**Minimum viable approach for Phase 3:** apply the existing `DisclosureState` filtering to all observability output. This may over-redact in some cases, but it's safe. Refine in Phase 4.

---

## Implementation Plan

### Phase 0: Hook System (prerequisite)

**New file:** `autonoetic-gateway/src/scheduler/hooks.rs`

- [ ] Define `HookEvent`, `HookAction`, `HookConfig` types
- [ ] Add `hooks` section to `GatewayConfig`
- [ ] Implement hook executor: match event → dispatch action (async/sync)
- [ ] Add `hook_deliveries` table for idempotency tracking
- [ ] Migrate existing signal delivery to hook-based dispatch
- [ ] Migrate report finalization to `publish_report` hook on `session.closed`

### Phase 1: Fix the Join

**This is the single highest-value fix.**

#### Task 1.1: Populate `execution_traces.event_id`

**Files:** `session_tracer.rs`, `tool_call_processor.rs`

- [ ] Thread the causal `event_id` through the tool execution pipeline
- [ ] Stop writing `ExecutionTraceRecord.event_id = None`
- [ ] Every execution trace row is now mechanically joinable to its causal event

#### Task 1.2: Add `links` and `event_id` to report nodes

**File:** `session_report.rs`

- [ ] Use `event_id` from the causal chain as the node key for timeline events and errors
- [ ] Add `links` payloads with URI backlinks to causal/trace/session
- [ ] Drop any use of `report_event_id` or `report_error_id` — these do not exist
- [ ] Evolve `payload_ref` from session-local filenames to content-backed handles

### Phase 2: Published Report Catalog

#### Task 2.1: Catalog table + FTS index

**Files:** `gateway_store/migrate.rs`, `gateway_store/observability.rs`

- [ ] Add `published_session_reports` table:
  - `root_session_id TEXT PRIMARY KEY`
  - `report_handle TEXT NOT NULL`
  - `overview_handle TEXT`, `html_handle TEXT`, `narrative_handle TEXT`
  - `title TEXT NOT NULL`, `status TEXT NOT NULL`
  - `started_at TEXT`, `ended_at TEXT`
  - `agent_count INTEGER`, `error_count INTEGER`, `approval_count INTEGER`
  - `search_text TEXT NOT NULL` (FTS-indexed)
  - `generated_at TEXT NOT NULL`
  - `report_version INTEGER NOT NULL`
- [ ] Add FTS index for discovery
- [ ] Lookup/search helpers

#### Task 2.2: Content-store-backed report storage

**Files:** `session_report.rs`, `content_store.rs`

- [ ] Write final report to content store (not global manifest)
- [ ] Register in catalog on publish (via hook)
- [ ] Session-directory files become optional projections

### Phase 3: Observability Tools

#### Task 3.1: `observability.search` + `observability.read`

**New file:** `runtime/tools/observability.rs`

- [ ] `observability.search` — discovers published reports and same-root resources
- [ ] `observability.read` — fetches resource by URI, `view` parameter controls depth
- [ ] Register in the native tool registry

#### Task 3.2: ACL enforcement

**Files:** `observability.rs`, `policy.rs`

- [ ] Gate tools by `observability/...` scope prefixes
- [ ] Combine static scope checks with dynamic root-session checks
- [ ] Keep transcript/evidence disabled

#### Task 3.3: Remove `execution.search`

**Files:** `runtime/tools/execution.rs`, `runtime/tools/mod.rs`

- [ ] Remove `execution.search` tool entirely
- [ ] Update agent SKILL.md docs to reference `observability.search`

### Phase 4: Redaction (deferred, design TBD)

- [ ] Define redaction rules per actor class
- [ ] Apply existing `DisclosureState` as minimum viable redaction in Phase 3
- [ ] Replace `report-data/` filenames with content URIs

### Phase 5: Tests and Documentation

- [ ] Same-root agent can read causal/trace URIs
- [ ] Cross-root ordinary agent can read only published report URIs
- [ ] Cross-root introspector can read redacted causal/trace/narrative URIs
- [ ] Report timeline node backlinks resolve correctly
- [ ] Execution trace rows now populate `event_id`
- [ ] Hook-driven report publishing works end-to-end
- [ ] Update `docs/agent-learning.md`, `docs/fts-session-search.md`, `docs/ARCHITECTURE.md`

---

## Current-Code Notes

1. `session_tracer.rs` generates stable causal `event_id` values. The missing work is carrying them into trace persistence and report links.
2. `tool_call_processor.rs` writes `ExecutionTraceRecord.event_id = None` — the main join bug. Fix this first.
3. Errors are already in the causal chain (`status=ERROR`). No new `report_error_id` is needed.
4. `session.search`/`session.peek` prove that root-scoped ACL enforcement exists and can be reused.
5. `policy.rs` supports prefix-based `ReadAccess.scopes` — the new `observability/...` family fits the current engine.
6. `scheduler/signal.rs` already does event-reactive dispatch (approval resolved, workflow join). Generalize this into the hook system.
7. `execution.search` should be removed, not narrowed. `observability.search` replaces it.

---

## Verification Criteria

- `execution_traces.event_id` is populated for every new trace row
- Every published report node has a canonical URI keyed by existing durable IDs (no synthetic IDs)
- Every report node with drill-down context exposes deterministic backlinks
- A report timeline/error node can open the full redacted trace without heuristics
- Cross-root ordinary agents cannot fetch raw causal or trace data
- Same-root agents can inspect redacted failure data for self-repair
- Report publishing is driven by hooks, not hardcoded in session close
- Hook system supports at least: `session.closed`, `approval.resolved`, `workflow.join.satisfied`

---

## Summary

1. **Fix the `event_id` join first** — this is the single highest-value change
2. **No new synthetic IDs** — `event_id` is the universal node key
3. **Hook system drives report publishing** — not hardcoded session logic
4. **Two tools: `observability.search` and `observability.read`** — replace `execution.search`, merge `resolve` into `read`
5. **Causal chain is the spine** — execution traces are detail views of causal events
6. **Redaction design is deferred** — use existing `DisclosureState` as minimum viable in v1
7. **Same-root drill-down by default, cross-root only for privileged introspectors**
