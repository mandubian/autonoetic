# Observability Redaction

How the gateway controls what shows up in observability surfaces — execution traces, causal events, approval summaries — based on **who is reading**.

This doc covers `ViewerClass` (the per-actor mechanism introduced in PR #143). It is distinct from, and composes with, the older `DisclosureClass` mechanism (which controls what an LLM may quote back to a user). See [Relationship to `DisclosureClass`](#relationship-to-disclosureclass) below.

> **Status note.** Several behaviours described here are tied to in-flight PRs. Where a section's claim depends on a specific PR landing, that dependency is called out inline (`(after #N: …)`). Today's behaviour is described first; the post-PR target is described second.
>
> Open PRs that affect this doc:
> - **#160** — `ScheduledAction::SandboxExec.command` is currently preserved verbatim for `ViewerClass::Agent`; #160 redacts it to `"***REDACTED***"`.
> - **#161** — Redaction primitives are currently triplicated across `causal_chain.rs`, `background.rs`, and `gateway/log_redaction.rs`; #161 centralises them in `autonoetic-types/src/redaction.rs` and replaces the wholesale-substring fallback with precise in-place masking.

---

## Why two mechanisms

The gateway has two redaction surfaces because they answer two different questions:

| Question | Mechanism | Scope |
|---|---|---|
| *Who is reading this trace right now?* | `ViewerClass` | Observability and approval reads (execution.search, approval_summary, …). |
| *What may the LLM repeat back to the user in its reply?* | `DisclosureClass` | Per-content classification consumed by the assistant-reply filter. |

A piece of output can be `Public` to the assistant (DisclosureClass) and still be redacted from a low-trust agent reading the trace later (ViewerClass) — and vice versa.

---

## Trust model

Three classes, ordered by decreasing redaction:

| Class | Who | What they see |
|---|---|---|
| `Agent` | An autonoetic agent reading observability/approval data via gateway tools. | **Most redacted.** Body text, headers, payloads, evidence references blanked. *Note: `SandboxExec.command` for approval subjects is currently preserved verbatim — see issue #158, fixed by PR #160.* |
| `Operator` | A human operator using the CLI / chat TUI. | **Targeted redaction.** Secret-named keys in JSON payloads have values replaced with `"***REDACTED***"`. For non-JSON strings the current `redact_json_string` falls back to wholesale redaction when the string contains substrings like `token`, `secret`, or `authorization` (this over-redacts benign strings such as `tokenizer config`, fixed by PR #161 — after which non-JSON strings get precise in-place masking via `redact_embedded_secrets`). Commands, hosts, request shapes are visible for triage. |
| `Admin` | An admin with full access (currently equivalent to "no redaction applied at this layer"). | Identity. The original record is returned unchanged. Secret material is still subject to the R+9 redaction-before-write invariant — see [R+9 and `RedactedPayload`](#r9-and-redactedpayload). |

The default is `Operator` (`ViewerClass::default()`). See [the default-Operator footgun](#the-default-operator-footgun) for what to watch out for.

---

## What each viewer class sees, by record type

### `ExecutionTraceRecord`

| Field | `Admin` | `Operator` | `Agent` |
|---|---|---|---|
| `trace_id`, `event_id`, `agent_id`, `session_id`, `turn_id`, `timestamp` | identity | identity | identity |
| `tool_name`, `exit_code`, `duration_ms`, `success` | identity | identity | identity |
| `error_type`, `error_summary` | identity | identity | identity |
| `approval_required`, `approval_request_id` | identity | identity | identity |
| `command` | identity | identity | `"***REDACTED***"` |
| `stdout`, `stderr` | identity | identity | `None` |
| `arguments` (JSON string) | identity | secret-key values replaced with `"***REDACTED***"`; non-JSON fallback wholesale-redacts on substring match (after #161: in-place masking) | `None` |
| `result` (JSON string) | identity | same as `arguments` | `None` |

Source: `autonoetic-types/src/causal_chain.rs::ExecutionTraceRecord::redact_for_viewer`. The `command` field was previously visible to `Agent` until commit 7f8525d (issue #4 follow-up) blanked it.

### `CausalEventRecord`

| Field | `Admin` | `Operator` | `Agent` |
|---|---|---|---|
| `event_id`, `agent_id`, `session_id`, `turn_id`, `event_seq`, `timestamp` | identity | identity | identity |
| `category`, `action`, `status`, `target`, `enforced_rules` | identity | identity | identity |
| `payload` (JSON string) | identity | secret-key values replaced; non-JSON fallback wholesale-redacts on substring match (after #161: in-place masking) | `None` |
| `payload_ref` | identity | `None` | `None` |
| `evidence_ref` | identity | identity | `None` |
| `reason` | identity | identity | `None` |

`payload_ref` is cleared for non-Admin viewers because resolving it to a content-store handle would expose the underlying artifact body. Pinned by the `event_operator_viewer_redacts_payload_keys_and_clears_payload_ref` test.

Source: `autonoetic-types/src/causal_chain.rs::CausalEventRecord::redact_for_viewer`.

### `ScheduledAction` (approval subjects)

The redaction shape varies per-variant. The table below uses the most-redacting class (`Agent`); `Operator` falls between `Admin` (identity for most variants) and `Agent`.

| Variant | Field | `Admin` | `Operator` | `Agent` |
|---|---|---|---|---|
| `WriteFile` | `path` | identity | identity | identity |
| | `content` | identity | identity | `"***REDACTED***"` |
| | `evidence_ref` | identity | identity | `None` |
| `SandboxExec` | `command` | identity | identity | **identity today** (issue #158); `"***REDACTED***"` after PR #160 lands |
| | `dependencies` | identity | identity | identity |
| | `detected_hosts` | identity | identity | identity |
| | `requires_approval` | identity | identity | identity |
| | `evidence_ref` | identity | identity | `None` |
| `CredentialRequest` | `credential_id`, `url`, `method` | identity | identity | identity |
| | `headers` | identity | sensitive keys/values redacted | empty map |
| | `body` (JSON) | identity | secret-key values redacted | `None` |
| | `payload` (JSON) | identity | same | `None` |
| | `inject_secret_as` | identity | identity | `None` |
| `AgentInstall`, `CredentialPrompt`, `SessionContinue`, `ProfileShare`, `SessionEscalate`, `LayerMount`, `RevisionPromote` | (all fields) | identity | identity | identity (fall-through; pinned by `agent_viewer_falls_through_for_agent_install_today`) |

Source: `autonoetic-types/src/background.rs::ScheduledAction::redact_for_viewer`.

> **Issue #158 (open, fixed in PR #160).** `SandboxExec.command` is currently preserved verbatim for the `Agent` class — a command embedding a Bearer token in `curl -H 'Authorization: …'` leaks to any agent-class consumer of the approval (e.g. an approver agent reading `approval_summary`). Pinned by `agent_viewer_sandbox_exec_command_currently_preserves_secrets`. PR #160 changes the behaviour to `"***REDACTED***"` and flips that pin.

> Variants in the fall-through set carry no fields with raw secret material today (agent IDs, summaries, request IDs). If a future variant adds a body field, the `agent_viewer_falls_through_for_agent_install_today` regression pin will fail and prompt the author to add an explicit redaction arm rather than silently leaking.

---

## Where the class is selected

The viewer class is chosen at the call site. Two production call sites today pass an explicit class:

| Call site | Class | Why |
|---|---|---|
| `runtime/tools/execution.rs::ExecutionSearchTool` | `Agent` | The caller is always an agent invoking the `execution.search` tool. |
| `runtime/tools/approval.rs::approval_summary` | `Agent` | The caller is the agent that requested or is querying the approval. |

Other paths today do **not** invoke `ViewerClass`-aware redaction:

- **CLI rendering of approvals** formats `ApprovalRequest.action` directly (truncating `SandboxExec.command`, etc.) without going through `redact_for_display` or `redact_for_viewer(Operator)`. The CLI is operator-only and the data is already operator-class, but applying the redaction layer would be more defensible — tracked as a follow-up.
- **HTTP API readers** go through unredacted store readers gated by HMAC auth (the assumption is that HMAC-authenticated callers are equivalent to `Admin`).

> **Convention:** any code path that emits trace, event, or approval data to a non-Admin consumer should pass `ViewerClass::Agent` if the consumer is an autonoetic agent, `ViewerClass::Operator` if it is a human, `ViewerClass::Admin` only when the consumer is part of the gateway core (e.g. log persistence, where R+9 already redacted at write time). When in doubt, pass `Agent` — too restrictive is recoverable; too permissive is not.

---

## The default-Operator footgun

`ViewerClass::default()` returns `Operator`. This is a deliberate compromise — most call sites in non-test code are operator-facing and `Operator` is the right default for those — but it means a future call site that *forgets* to pass a class explicitly will silently inherit `Operator` rather than `Agent`. For agent-facing tool paths this would expose more information than intended.

**Discipline:**

- Code paths that serve agents (gateway tools, agent-side API readers) must pass `ViewerClass::Agent` explicitly. The two existing tool call sites do this.
- Code review for any new tool path or API endpoint should specifically check for the class selection. Grepping for `ViewerClass::` is the simplest verification.
- The `ViewerClass::default()` impl on `disclosure.rs` is intentionally an `Operator` rather than a non-`Default` type, to keep ergonomics for CLI rendering. Tightening this (e.g. removing `Default` and forcing every call site to choose explicitly) is a backlog candidate but not currently planned.

---

## Where the redaction primitives live

### Today

The primitives are triplicated across the workspace:

- `autonoetic-types/src/causal_chain.rs` — local copies of `redact_json_string`, `redact_json_value`, `is_sensitive_key`. The non-JSON fallback in `redact_json_string` wholesale-redacts on substring match (the bug fixed by PR #161).
- `autonoetic-types/src/background.rs` — separate local copies of `is_sensitive_key`, `looks_like_secret_value`, plus per-`ScheduledAction`-variant redaction.
- `autonoetic-gateway/src/log_redaction.rs` — the most comprehensive copy: regex catalogue, JWT/long-hex detection, JSON-aware redaction. Owns `RedactedPayload` (the R+9 wrapper).

The three sets have drifted slightly: the gateway version has the regex catalogue, the types versions don't.

### After PR #161 lands

The canonical primitives move to **`autonoetic-types/src/redaction.rs`**. The three call layers above all delegate to it:

1. `causal_chain::redact_for_viewer` — for `ExecutionTraceRecord` and `CausalEventRecord`.
2. `background::ScheduledAction::redact_for_viewer` — for approval subjects.
3. `gateway::log_redaction` — re-exports the canonical helpers; `RedactedPayload` (the R+9 wrapper) stays local.

Public functions in the canonical module (post-#161):

| Function | Purpose |
|---|---|
| `is_sensitive_key(k)` | True when a JSON object key matches the credential-shaped substring catalogue (`secret`, `token`, `password`, `api_key`, `apikey`, `authorization`, `access_key`, `access_token`, `refresh_token`, `client_secret`). |
| `looks_like_secret_value(t)` | True when free-form text smells secret-bearing (Bearer prefix, `sk-` prefix, PEM marker, env-var-name regex, long hex, JWT shape). Used by free-text classification, not by JSON-value redaction. |
| `looks_like_secret_collection_prompt(t)` | True when text appears to *solicit* a secret from a human ("paste your API key…"). |
| `redact_embedded_secrets(t)` | In-place masking via the regex catalogue (env-var assignments, URL query params, Bearer headers, raw `sk-` prefix). Preserves surrounding prose. |
| `redact_json_value(v)` | Recursive JSON redaction: object keys via `is_sensitive_key` → wholesale value redact; string values via `redact_embedded_secrets`; **narrow PEM fallback** for values that can't be masked locally. |
| `redact_text_for_logs(t)` | Top-level entry: JSON parse → `redact_json_value`; otherwise `redact_embedded_secrets`. |

#### Why the JSON-value fallback is narrow (post-#161)

The fallback for values that can't be masked in place is restricted to PEM blocks (`-----BEGIN`). It is intentionally **not** the broader `looks_like_secret_value` predicate, because content digests, revision IDs, and hook delivery IDs (e.g. `hook-<sha256>`) routinely match JWT/long-hex shapes — falling back on those would silently mangle ordinary identifiers. The narrow PEM-only path is pinned by the `redact_json_value_handles_each_secret_shape_appropriately` test.

---

## R+9 and `RedactedPayload`

`ViewerClass` is a *read-time* defence. The complementary *write-time* invariant is **R+9 (redaction-before-write)**, enforced by `gateway::log_redaction::RedactedPayload`. The newtype wraps `serde_json::Value`; constructors run `redact_json_value` before the payload reaches the causal chain. Direct `Value` cannot be passed to `CausalLogger::log` — only `RedactedPayload` can.

The two layers compose:

- R+9 ensures secrets never enter the causal chain in the first place. An Admin reading a trace cannot leak what was never written.
- `ViewerClass` is the second line: even when a record is innocuous at write time, the read path strips fields that lower-trust consumers don't need.

---

## Relationship to `DisclosureClass`

| Aspect | `ViewerClass` | `DisclosureClass` |
|---|---|---|
| Defined in | `autonoetic-types/src/disclosure.rs` | same file |
| Granularity | Per consumer (Agent/Operator/Admin) | Per piece of content (Public/Restricted with legacy aliases) |
| Applied | At observability/approval read time | At assistant-reply filter time, before the LLM response reaches the user |
| Configurable per-agent? | No (chosen at call site) | Yes (via `DisclosurePolicy` rules in `SKILL.md`) |
| Documented in | this file | `docs/AGENTS.md` § "Disclosure policy" |

A piece of data can be both:

- **`DisclosureClass::Restricted` + `ViewerClass::Operator`**: the LLM may not repeat it back to the user, but a human operator reading the trace can see it.
- **`DisclosureClass::Public` + `ViewerClass::Agent`**: a child agent reading the parent's published report sees only the metadata; the LLM is allowed to quote the public parts.

They cooperate; neither subsumes the other.

---

## Threat model

### What this protects against

- **Cross-agent secret leakage via observability tools.** A specialist agent invoking `execution.search` to learn from another session sees only metadata, not stdout/stderr/headers/bodies. Even if the source agent had `NetworkAccess` and made a credential-bearing request, the credential-in-body never reaches the curious agent. (One known gap: `approval_summary` for `ScheduledAction::SandboxExec` currently exposes the command verbatim — issue #158, fixed by PR #160.)
- **Operator-class triage with redacted secrets.** Operators inspecting payloads see structural fields (host, method, path, JSON keys) but secret-named keys have their values replaced with `"***REDACTED***"`. Non-JSON strings today get the coarse substring-fallback behaviour; PR #161 replaces this with precise in-place masking.
- **Inadvertent exposure through reports.** The HTML and JSON session reports go through `redact_text_for_logs` before being written to the content store (commit 7f8525d, closes part of issue #4).

### What this does NOT protect against

- **A compromised gateway.** ViewerClass is part of the trusted reader stack; an attacker who controls the gateway can read raw rows.
- **A malicious admin.** The `Admin` viewer class is identity. By design — admins trace incidents and need full data. Defence here is upstream (`R+9`, audit trail of admin reads, organisational controls).
- **Secrets that bypass the redaction pipeline at write time.** If `R+9` fails — e.g. a payload bypasses `RedactedPayload` and reaches the causal chain unwrapped — the row contains the raw secret and the read-time redaction may not catch every credential shape. The sentinel's credential-leak check (`autonoetic-gateway/src/sentinel/checks/credential.rs`) is the backstop, scanning persisted payloads for known credential patterns.

---

## Known gaps and follow-ups

| Gap | Issue / PR | Status |
|---|---|---|
| `SandboxExec.command` leaks verbatim to `ViewerClass::Agent`. | #158 (issue), #160 (PR) | PR open. Pinned by `agent_viewer_sandbox_exec_command_currently_preserves_secrets` — flips when PR lands. |
| Redaction primitives triplicated; non-JSON fallback wholesale-redacts on substring match (`tokenizer config` becomes `"***REDACTED***"`). | #156 (issue), #161 (PR) | PR open. Bonus regex fix: bare `PASSWORD=…` now matches; `KEY=value` regex's leading `[A-Z][A-Z0-9_]*` was tightened to `[A-Z0-9_]*`. |
| `X-API-Key` (hyphenated) does not match `is_sensitive_key`. `X-Auth-Token` and similar with `token` substring do match. | #156 (filed) | Open — adding `api-key` to the catalogue is straightforward but requires intent confirmation. Pinned by `is_sensitive_key_misses_hyphenated_api_key`. |
| CLI rendering of approvals does not call `redact_for_display` or `redact_for_viewer(Operator)`; it formats `ApprovalRequest.action` directly. | (not yet filed) | Open — applying `Operator` redaction would be more defensible. The CLI is operator-only so secret leakage is not a current concern, but the layer should be applied for consistency. |
| Frozen-baseline contract claim in `docs/security-sentinel.md` overstates what dual-sweep catches. | #153 | Open — separate version-pinned baseline module is the architectural fix. |
| Promotion gate scopes critical findings system-wide rather than per-agent. | #155 (issue), #163 (PR) | PR open — adds `scope_agent_id` and threads it through the Phase-1 checks. |

When closing one of these, please update the relevant table or note in this file in the same PR.
