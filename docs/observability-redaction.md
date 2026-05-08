# Observability Redaction

How the gateway controls what shows up in observability surfaces — execution traces, causal events, approval summaries — based on **who is reading**.

This doc covers `ViewerClass` (the per-actor mechanism introduced in PR #143). It is distinct from, and composes with, the older `DisclosureClass` mechanism (which controls what an LLM may quote back to a user). See [Relationship to `DisclosureClass`](#relationship-to-disclosureclass) below.

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
| `Agent` | An autonoetic agent reading observability/approval data via gateway tools. | **Most redacted.** Metadata only. Body text, headers, payloads, command strings, evidence references all blanked or masked. |
| `Operator` | A human operator using the CLI / chat TUI. | **Targeted redaction.** Secrets within JSON payloads are masked (key-name catalogue + in-place value masking); structural fields preserved. Commands, hosts, request shapes visible for triage. |
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
| `arguments` (JSON string) | identity | secret-key values redacted, in-place value masking | `None` |
| `result` (JSON string) | identity | same as `arguments` | `None` |

Source: `autonoetic-types/src/causal_chain.rs::ExecutionTraceRecord::redact_for_viewer`. The `command` field was previously visible to `Agent` until commit 7f8525d (issue #4 follow-up) blanked it.

### `CausalEventRecord`

| Field | `Admin` | `Operator` | `Agent` |
|---|---|---|---|
| `event_id`, `agent_id`, `session_id`, `turn_id`, `event_seq`, `timestamp` | identity | identity | identity |
| `category`, `action`, `status`, `target`, `enforced_rules` | identity | identity | identity |
| `payload` (JSON string) | identity | secret-key values redacted, in-place value masking | `None` |
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
| `SandboxExec` | `command` | identity | identity | `"***REDACTED***"` (issue #158 fix) |
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

> Variants in the fall-through set carry no fields with raw secret material today (agent IDs, summaries, request IDs). If a future variant adds a body field, the `agent_viewer_falls_through_for_agent_install_today` regression pin will fail and prompt the author to add an explicit redaction arm rather than silently leaking.

---

## Where the class is selected

The viewer class is chosen at the call site. There are two production call sites today:

| Call site | Class | Why |
|---|---|---|
| `runtime/tools/execution.rs::ExecutionSearchTool` | `Agent` | The caller is always an agent invoking the `execution.search` tool. |
| `runtime/tools/approval.rs::approval_summary` | `Agent` | The caller is the agent that requested or is querying the approval. |

The CLI and HTTP API paths do not invoke the viewer redaction layer at all today — they go through their own rendering paths (CLI via `redact_for_display` which is `Operator`-equivalent; HTTP API via the unredacted store readers gated by HMAC auth).

> **Convention:** any code path that emits trace, event, or approval data to a non-Admin consumer should pass `ViewerClass::Agent` if the consumer is an autonoetic agent, `ViewerClass::Operator` if it is a human, `ViewerClass::Admin` only when the consumer is part of the gateway core (e.g. log persistence, where R+9 already redacted at write time). When in doubt, pass `Agent` — too restrictive is recoverable; too permissive is not.

---

## The default-Operator footgun

`ViewerClass::default()` returns `Operator`. This is a deliberate compromise — most call sites in non-test code are operator-facing and `Operator` is the right default for those — but it means a future call site that *forgets* to pass a class explicitly will silently inherit `Operator` rather than `Agent`. For agent-facing tool paths this would expose more information than intended.

**Discipline:**

- Code paths that serve agents (gateway tools, agent-side API readers) must pass `ViewerClass::Agent` explicitly. The two existing tool call sites do this.
- Code review for any new tool path or API endpoint should specifically check for the class selection. Grepping for `ViewerClass::` is the simplest verification.
- The `ViewerClass::default()` impl on `disclosure.rs` is intentionally an `Operator` rather than a non-`Default` type, to keep ergonomics for CLI rendering. Tightening this (e.g. removing `Default` and forcing every call site to choose explicitly) is a backlog candidate but not currently planned.

---

## Canonical redaction module

The primitives behind these redactions live in **`autonoetic-types/src/redaction.rs`** (centralised in #161). Three call layers use them:

1. `causal_chain::redact_for_viewer` — for `ExecutionTraceRecord` and `CausalEventRecord`.
2. `background::ScheduledAction::redact_for_viewer` — for approval subjects.
3. `gateway::log_redaction` — re-exports the canonical helpers; `RedactedPayload` (the R+9 wrapper) lives there.

Public functions in `autonoetic-types::redaction`:

| Function | Purpose |
|---|---|
| `is_sensitive_key(k)` | True when a JSON object key matches the credential-shaped substring catalogue (`secret`, `token`, `password`, `api_key`, `apikey`, `authorization`, `access_key`, `access_token`, `refresh_token`, `client_secret`). |
| `looks_like_secret_value(t)` | True when free-form text smells secret-bearing (Bearer prefix, `sk-` prefix, PEM marker, env-var-name regex, long hex, JWT shape). Used by free-text classification, not by JSON-value redaction. |
| `looks_like_secret_collection_prompt(t)` | True when text appears to *solicit* a secret from a human ("paste your API key…"). |
| `redact_embedded_secrets(t)` | In-place masking via the regex catalogue (env-var assignments, URL query params, Bearer headers, raw `sk-` prefix). Preserves surrounding prose. |
| `redact_json_value(v)` | Recursive JSON redaction: object keys via `is_sensitive_key` → wholesale value redact; string values via `redact_embedded_secrets`; **narrow PEM fallback** for values that can't be masked locally. |
| `redact_text_for_logs(t)` | Top-level entry: JSON parse → `redact_json_value`; otherwise `redact_embedded_secrets`. |

### Why the JSON-value fallback is narrow

The fallback for values that can't be masked in place is restricted to PEM blocks (`-----BEGIN`). It is intentionally **not** the broader `looks_like_secret_value` predicate, because content digests, revision IDs, and hook delivery IDs (e.g. `hook-<sha256>`) routinely match JWT/long-hex shapes — falling back on those would silently mangle ordinary identifiers. The narrow PEM-only path is pinned by the `redact_json_value_handles_each_secret_shape_appropriately` test.

---

## R+9 and `RedactedPayload`

`ViewerClass` is a *read-time* defence. The complementary *write-time* invariant is **R+9 (redaction-before-write)**, enforced by `gateway::log_redaction::RedactedPayload`. The newtype wraps `serde_json::Value`; constructors run the canonical `redact_json_value` before the payload reaches the causal chain. Direct `Value` cannot be passed to `CausalLogger::log` — only `RedactedPayload` can.

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

- **Cross-agent secret leakage via observability tools.** A specialist agent invoking `execution.search` or `approval_summary` to learn from another session sees only metadata, not stdout/stderr/headers/bodies. Even if the source agent had `NetworkAccess` and made a credential-bearing request, the credential never reaches the curious agent.
- **Operator-class triage with masked secrets.** Operators inspecting payloads see structural fields (host, method, path, JSON keys) but credential values are masked in place — enough to debug without exposing the secret.
- **Inadvertent exposure through reports.** The HTML and JSON session reports go through `redact_text_for_logs` before being written to the content store (commit 7f8525d, closes part of issue #4).

### What this does NOT protect against

- **A compromised gateway.** ViewerClass is part of the trusted reader stack; an attacker who controls the gateway can read raw rows.
- **A malicious admin.** The `Admin` viewer class is identity. By design — admins trace incidents and need full data. Defence here is upstream (`R+9`, audit trail of admin reads, organisational controls).
- **Secrets that bypass the redaction pipeline at write time.** If `R+9` fails — e.g. a payload bypasses `RedactedPayload` and reaches the causal chain unwrapped — the row contains the raw secret and the read-time redaction may not catch every credential shape. The sentinel's credential-leak check (`autonoetic-gateway/src/sentinel/checks/credential.rs`) is the backstop, scanning persisted payloads for known credential patterns.

---

## Known gaps and follow-ups

| Gap | Issue | Status |
|---|---|---|
| `X-API-Key` (hyphenated) does not match `is_sensitive_key` (catalogue uses `api_key` underscore form). `X-Auth-Token` and similar with `token` substring do match. | #156 (filed; partially addressed by canonical move) | Open — adding `api-key` to the catalogue is straightforward but requires intent confirmation. Pinned by `is_sensitive_key_misses_hyphenated_api_key`. |
| Frozen-baseline contract claim in `docs/security-sentinel.md` overstates what dual-sweep catches. | #153 | Open — separate version-pinned baseline module is the architectural fix. |
| Promotion gate scopes critical findings system-wide rather than per-agent. | #155 | Open. |

When closing one of these, please update the relevant table or note in this file in the same PR.
