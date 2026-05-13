# Phase 2 — Production Recording Mode

**Status:** Draft
**Refs:** Issue #187, `docs/design/promotion-federation-plan.md` §2.5, `docs/design/sealed-network-evaluation-plan.md` §5.3, `docs/design/promotion-federation-plan-review.md` §4.3

---

## 1. Motivation

### 1.1 What exists today

The proxy (`sealed_network_proxy.rs`) intercepts outbound HTTP from sandboxed agents. The `FixtureLoader` (`sealed_network.rs`) resolves requests against fixture files on disk. Three policies exist:

| Policy | Behaviour |
|--------|-----------|
| `Normal` | Proxy not started; normal network access via capability/approval flow |
| `Sealed` | Proxy intercepts; fixture hit → canned response; fixture miss → structured 502 error |
| `Recording` | Proxy intercepts; fixture hit → canned response; fixture miss → **stub** (treated identically to `Sealed`) |

The `Recording` stub is a documented placeholder. The `decide_egress()` function's caller is meant to distinguish `Sealed` from `Recording` and capture live traffic on miss — but this is unimplemented.

### 1.2 What's broken

Hand-authoring fixture files for every HTTP endpoint an artifact calls is a bottleneck that scales linearly with artifact complexity. The evaluator blocks on operator approval for live network access, the operator doesn't want to approve production traffic just for evaluation, and the artifact sits in limbo.

Recording mode flips this: instead of "developer writes fixtures during artifact authoring", the **operator runs the real agent against real endpoints**, the proxy redacts secrets and captures the traffic, and the resulting fixture set feeds the sealed evaluator for later deterministic replay.

### 1.3 Scope

Phase 2 is the production recording tool. Phase 3 (sealed evaluator replay from recorded fixtures) and Phase 4 (post-promotion background review) are separate.

---

## 2. Design

### 2.1 Recording mode flow

```
Operator: autonoetic agent run moltbook --record-network --duration 5m
         ↓
Gateway starts sandbox with sandbox_network: recording
  - Verifies config: gateway.sandbox.allow_recording == true
  - Creates a RecordingSession record (agent_id, timestamp, duration/config)
  - Emits causal event: artifact.fixture_recording_session_started
         ↓
Agent makes real HTTP calls (post_to_feed, get_posts, etc.)
         ↓
Proxy intercepts each request:
  1. Check FixtureLoader for existing fixture  (for the artifact under recording)
  2. MISS → send request live (real HTTP call)
  3. Capture response (status, headers, body)
  4. REDACT secrets from request + response
  5. Write fixture file to staging directory
  6. Emit causal event: artifact.fixture_recorded
         ↓
Agent session ends (timeout, duration reached, or agent stops)
         ↓
Gateway finalises RecordingSession:
  - Computes SHA-256 digest of the fixture set (sorted manifest of files + digests)
  - Stores fixture set as content-addressed artifact: ar.recording-<agent_id>-<timestamp>-<digest>
  - Emits causal event: artifact.fixture_recording_completed
  - Prints fixture set ref and summary to operator
```

### 2.2 What changes in `decide_egress()`

Currently (`sealed_network.rs:147-182`):

```rust
pub fn decide_egress(...) -> Result<EgressDecision> {
    match policy {
        SandboxNetworkPolicy::Normal => Ok(EgressDecision::Allow),
        SandboxNetworkPolicy::Sealed | SandboxNetworkPolicy::Recording => {
            // fixture lookup...
        }
    }
}
```

Phase 2 changes: the `Recording` branch in `decide_egress` still returns `Fixture` on hit and `Unfixtured` on miss — the distinction is in the **caller** (the proxy request handler in `sealed_network_proxy.rs`). On `Unfixtured`:
- `Sealed` policy → return 502 (existing behaviour)
- `Recording` policy → send live, redact, capture, serve the live response to the sandbox

### 2.3 Fixture file format

Fixtures follow the existing path convention:

```
<recording_staging_dir>/<host>[-<port>]/<METHOD>-<encoded-path>.json
```

Each fixture file contains the full captured round-trip:

```json
{
  "request": {
    "method": "GET",
    "url": "https://api.example.com/posts?limit=10",
    "headers": {
      "content-type": "application/json"
    },
    "body": null
  },
  "response": {
    "status": 200,
    "headers": {
      "content-type": "application/json",
      "x-request-id": "abc123"
    },
    "body": "{\"posts\": [...]}"
  },
  "recorded_at": "2026-05-13T12:00:00Z",
  "redacted": ["authorization", "cookie", "x-api-key"]
}
```

### 2.4 Types

#### `RecordingSession`

```rust
/// Tracks a single recording run.
pub struct RecordingSession {
    pub session_id: String,           // rs_xxxxxxxx
    pub agent_id: String,
    pub artifact_id: String,          // the artifact being recorded
    pub root_session_id: String,      // the gateway session that triggered recording
    pub started_at: String,           // RFC 3339
    pub stopped_at: Option<String>,
    pub duration_secs: Option<u64>,   // operator-configured max duration
    pub max_requests: Option<u64>,    // operator-configured max request count
    pub request_count: u64,
    pub status: RecordingStatus,
    pub fixture_set_id: Option<String>, // set when finalised
    pub created_by: String,            // operator identity
}
```

#### `RecordingStatus`

```rust
pub enum RecordingStatus {
    Active,
    Completed,
    Cancelled,     // operator or error aborted mid-recording
    Failed,        // infrastructure error during capture
}
```

#### `FixtureSet`

```rust
/// A content-addressed set of recorded HTTP fixtures.
pub struct FixtureSet {
    pub fixture_set_id: String,        // ar.recording-<agent_id>-<ts>-<digest>
    pub agent_id: String,
    pub revision_id: String,           // the revision this fixture set was recorded against
    pub recording_session_id: String,
    pub created_at: String,
    pub fixture_file_count: u64,
    pub total_bytes: u64,
    pub digest: String,                // SHA-256 of the sorted fixture manifest
    pub host_summary: Vec<String>,     // unique hosts captured, e.g. ["api.example.com", "auth.example.com"]
    pub host_count: u64,
    pub redaction_summary: Vec<String>, // redacted field names, e.g. ["authorization", "cookie"]
    pub status: FixtureSetStatus,
}
```

#### `FixtureSetStatus`

```rust
pub enum FixtureSetStatus {
    Ready,
    Expired,   // optional TTL
    Invalid,   // integrity check failed
}
```

### 2.5 Storage model

Fixture files are written to a staging directory during recording:

```
<gateway_dir>/recordings/<session_id>/fixtures/
```

On finalisation, the gateway:
1. Walks the fixture tree, computes SHA-256 for each file
2. Builds a sorted manifest `{path → digest}` and computes the overall digest
3. Stores the fixture set as a content-addressed artifact in the existing `ArtifactStore`
4. The `ArtifactStore` already stores immutable content-addressed bundles — fixture sets fit the same model
5. Stores a `RecordingSession` and `FixtureSet` record in SQLite (migration v34)

SQLite schema for recording sessions:

```sql
CREATE TABLE recording_sessions (
    session_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    root_session_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    stopped_at TEXT,
    duration_secs INTEGER,
    max_requests INTEGER,
    request_count INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'active',
    fixture_set_id TEXT,
    created_by TEXT NOT NULL
);

CREATE TABLE fixture_sets (
    fixture_set_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    recording_session_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    fixture_file_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,
    digest TEXT NOT NULL,
    host_summary TEXT NOT NULL,       -- JSON array of strings
    host_count INTEGER NOT NULL,
    redaction_summary TEXT NOT NULL,   -- JSON array of strings
    status TEXT NOT NULL DEFAULT 'ready'
);
```

### 2.6 Redaction policy

Mandatory redactions applied to **both request and response** before the fixture is written to disk:

| Category | What's redacted | Scope |
|----------|----------------|-------|
| **Authorization** | `Authorization` header value (any scheme) | Request headers |
| **Cookies** | `Cookie` request header, `Set-Cookie` response header | Request + response headers |
| **Bearer tokens** | `Bearer <token>` pattern in `Authorization` header | Request headers |
| **API keys** | `X-Api-Key`, `X-API-Key`, `api_key` headers | Request headers |
| **Query secrets** | Query params named `token`, `api_key`, `secret`, `key`, `password`, `auth`, `signature` | Request URL query string |
| **Session cookies** | Any `Set-Cookie` header — full value redacted | Response headers |
| **Request body** | Body content is NOT redacted by default (the operator explicitly opts in to recording, and the artifact controls what it sends) | N/A |

Redaction replaces the value with `"[REDACTED]"`. The `redacted` field in the fixture JSON records which fields were redacted for auditability.

**Deferred:** Configurable redaction rules (allow-listed paths, custom header patterns). The hard-coded set above is sufficient for Phase 2; configurable rules are Phase 3+ refinement.

### 2.7 Causal events

| Event | When | Payload |
|-------|------|---------|
| `artifact.fixture_recording_session_started` | Recording session begins | `session_id`, `agent_id`, `artifact_id`, `root_session_id`, `duration_secs` |
| `artifact.fixture_recorded` | Each fixture capture | `host`, `method`, `path`, `status_code`, `response_size`, `redacted_fields` |
| `artifact.fixture_recording_stopped` | Recording stops (timeout, max requests, or agent end) | `session_id`, `request_count`, `duration_actual_secs` |
| `artifact.fixture_recording_completed` | Fixture set stored | `fixture_set_id`, `digest`, `fixture_count`, `hosts` |
| `artifact.fixture_recording_cancelled` | Operator or error abort | `session_id`, `reason` |

### 2.8 CLI interface

#### `autonoetic agent run --record-network`

```
autonoetic agent run moltbook.default \
  --record-network \
  --duration 300            # seconds (optional; default 600 / 10 min)
  --max-requests 100        # max fixture captures (optional; default 1000)
  --output ar.recording-moltbook-20260513  # explicit fixture set ID (optional)
```

The existing `RunArgs` struct gains:
```rust
pub struct RunArgs {
    // ... existing fields ...
    pub record_network: bool,
    pub recording_duration_secs: Option<u64>,
    pub recording_max_requests: Option<u64>,
}
```

#### `autonoetic recording list`

```
autonoetic recording list [--agent <agent_id>] [--limit 20]
```

Lists recording sessions with summary: session_id, agent_id, status, request_count, started_at.

#### `autonoetic recording inspect <session_id>`

```
autonoetic recording inspect rs_abc123
```

Detailed view: session metadata + fixture set info + per-host breakdown.

#### `autonoetic recording delete <session_id>`

```
autonoetic recording delete rs_abc123
```

Deletes the recording session record and its fixture set artifact. Requires confirmation.

#### `autonoetic recording cancel <session_id>`

```
autonoetic recording cancel rs_abc123
```

Forces a running recording session to stop (abort signal to the proxy, finalises what it has, stores as `Cancelled` status).

---

## 3. Security & enforcement

### 3.1 Operator gate

Recording requires two levels of authorisation:

1. **Config-level:** `gateway.sandbox.allow_recording` must be `true` (already implemented — refuse-boot guard in `lifecycle.rs`)
2. **CLI-level:** The `--record-network` flag is a deliberate opt-in — no silent recording

### 3.2 No silent recording

The causal event `artifact.fixture_recording_session_started` is non-optional. Every recording session is traced in the causal chain. The operator's identity (`created_by`) is captured. Missed events would be a detected anomaly.

### 3.3 Time-bounded recording

Recording stops automatically when:
- The agent turn ends (normal completion)
- The configured `--duration` elapses
- The configured `--max-requests` is reached

The proxy checks a shared `AtomicU64` request counter on each capture. The session runner checks duration against `started_at + duration_secs` on each turn boundary.

### 3.4 HTTPS limitation

Recording-mode proxy currently intercepts HTTP only. HTTPS requests (CONNECT tunnelling) return a 502 error with a structured message explaining that HTTPS recording is not yet supported (scope 5.2d). This is the same behaviour as `Sealed` mode. The operator should use artifacts that speak HTTP to the recording proxy, or deploy the artifact against an internal HTTP endpoint for recording.

---

## 4. Acceptance criteria

- [ ] `--record-network` flag on `autonoetic agent run` with `--duration` and `--max-requests`
- [ ] Recording proxy captures real traffic to fixture files on fixture miss (modify proxy handler's treatment of `Unfixtured` in `Recording` mode)
- [ ] Redaction layer strips credentials before storage (Authorization, cookies, Set-Cookie, query secrets)
- [ ] `RecordingSession` + `FixtureSet` types in `autonoetic-types/src/recording.rs`
- [ ] SQLite migration v34 for `recording_sessions` and `fixture_sets` tables
- [ ] Fixture set stored as content-addressed artifact in `ArtifactStore`
- [ ] CLI: `autonoetic recording list|inspect|delete|cancel`
- [ ] Recording is time-bounded (duration or request count)
- [ ] Recording requires explicit operator authorisation (no silent recording; config gate + CLI opt-in)
- [ ] Causal events for session start, fixture capture, stop, completion, cancellation
- [ ] Integration tests:
  - Positive: start recording session, capture a fixture, verify fixture file written with redactions
  - Negative: recording refused when `allow_recording` is `false`
  - Timeout: recording stops when duration elapses
  - Redaction: verify Authorization header, Cookie header, query parameter secrets are redacted
  - CLI: list/inspect/delete recording fixtures
  - HTTPS: verify CONNECT returns structured error (same as Sealed)

---

## 5. Dependencies & boundaries

### 5.1 Dependencies

- `SandboxNetworkPolicy::Recording` variant (exists)
- Proxy infrastructure (exists: `sealed_network_proxy.rs`, `start_sealed_proxy`, `setup_sealed_proxy_for_exec`)
- `FixtureLoader` (exists)
- `decide_egress()` Recording stub (exists, documented as pending)
- `SandboxConfig.allow_recording` (exists)
- Refuse-boot guard in `lifecycle.rs` (exists)
- `ArtifactStore` for content-addressed artifact storage (exists)

### 5.2 Out of scope (Phase 3)

- Sealed evaluator accepting `fixture_set_ref` in spawn metadata
- Gateway mounting fixture sets into sealed sandbox
- Operator CLI: `autonoetic eval sealed --artifact-ref X --fixture-set Y`
- Configurable redaction rules

### 5.3 Out of scope (Phase 4)

- Post-promotion background review
- Behavioral drift detection
- Periodic re-evaluation against new recordings

---

## 6. Decisions & open questions

### 6.1 Resolved

1. **Fixture set → revision association.** Fixture sets are associated to the agent's **revision** (not just the agent). When the operator records, the fixture set is bound to the current promoted revision. If the operator records again after a revision bump, the old fixture set stays accessible (immutable) and the new recording produces a fresh one. This avoids ambiguity about "which revision do these fixtures match?"

   The `FixtureSet` type gains:
   ```rust
   pub revision_id: String,
   ```

2. **Maximum recording cap.** Default 50 MB total fixture bytes and 1000 requests. Configurable via `--max-bytes` and `--max-requests` CLI flags. If either cap is hit mid-request, recording stops, the last fixture is saved, and the session finalises normally (Completed, not Cancelled).

3. **TTL for fixture sets.** Not in Phase 2 (the operator deletes explicitly). The schema includes `FixtureSetStatus::Expired` for a future TTL-aware cleanup task (Phase 4+).

4. **Partial recordings on cancel.** Stored as-is with `Cancelled` status. Even partial data can seed a sealed evaluator run. The operator chooses whether to use or delete.

### 6.2 Open: HTTPS recording

Recording an agent whose endpoints require HTTPS is blocked: the proxy rejects CONNECT tunnelling with a 502 error (same as `Sealed` mode). Forcing HTTP is not an option — the service requires TLS.

Several approaches exist but none is clearly best:

| Approach | Pros | Cons |
|----------|------|------|
| **5.2d HTTPS proxy** — gateway generates a CA, signs per-host certs, terminates TLS at the proxy | Full HTTPS support, no artifact changes | Complex, security-sensitive (CA management), deferred from sealed-network plan |
| **Record via external proxy** — operator runs a separate recording proxy (mitmproxy, charles) and feeds fixtures in manually | Zero gateway changes | Manual, breaks the seamless `autonoetic agent run --record-network` workflow |
| **Agent speaks HTTP internally** — artifact runs HTTP to the proxy, proxy forwards as HTTPS upstream (HTTP/1.1 CONNECT-less forwarding) | Proxy doesn't need TLS termination | Non-standard, may break HTTP libraries that expect proper CONNECT |

**Proposed:** Phase 2 ships without HTTPS recording. The HTTPS 502 error message is updated to point to issue #203 ("HTTPS recording support") and suggest the operator either (a) deploy the artifact against an HTTP-speaking internal endpoint for recording, or (b) use an external mitmproxy and import fixtures manually.
