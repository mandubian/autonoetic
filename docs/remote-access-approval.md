# Remote Access Approval

> **Note:** This document covers the detection and analysis side of remote
> access approval. The approval lifecycle itself (dedup, grants, enrichment,
> resolution) is now handled by the unified `GateService`
> (`runtime/human_gate.rs`). See
> [`design/human-gate-unification-plan.md`](./design/human-gate-unification-plan.md).

This document describes the static analysis system for detecting remote/network access in sandboxed code execution.

## Overview

When `sandbox_exec` runs, the gateway **statically analyzes** the invoked command/code before execution to detect patterns that imply network access. If detected, execution is blocked pending operator approval. The **artifact_exec** path uses the same remote-access analysis and attaches the **same operator-facing hint suffix** (`→ hosts:` or `→ signals:`) when it requires approval.

Outbound network paths share a common declaration resolver (`runtime/network_policy.rs`) that evaluates `metadata.autonoetic.remote_access` target policy (`targets` only) for `sandbox_exec`, `web_search`/`web_fetch`/`web_call`, and credential HTTP calls.

This is a **deterministic** security check that does not rely on LLM self-declaration.

## Why Static Analysis?

The LLM cannot be trusted to self-declare that code needs remote access:

```
User: "Fetch weather data"
  → LLM generates: import requests; requests.get("https://...")
  → LLM claims: "sandbox_exec with no network access"
  → ❌ LLM is wrong/misleading
```

Static analysis inspects the **actual code** to detect remote access patterns deterministically.

## Detection Categories

### 1. Network Library Imports

| Pattern | Example | Reason |
|---------|---------|--------|
| `import requests` | HTTP client | Makes HTTP requests |
| `from urllib import urlopen` | URL handling | Opens URLs |
| `import socket` | Low-level networking | TCP/UDP connections |
| `import httpx` | Async HTTP client | Makes HTTP requests |
| `import aiohttp` | Async HTTP client | Makes HTTP requests |
| `import ftplib` | FTP client | File transfer |
| `import smtplib` | SMTP client | Email sending |
| `import paramiko` | SSH client | Remote shell |
| `import boto3` | AWS SDK | Cloud access |
| `import google.cloud` | GCP SDK | Cloud access |

### 2. Network Function Calls

| Pattern | Example | Reason |
|---------|---------|--------|
| `.connect()` | `sock.connect(addr)` | Socket connection |
| `.send()` | `sock.send(data)` | Network transmission |
| `.recv()` | `sock.recv(1024)` | Network reception |
| `urlopen()` | `urlopen(url)` | URL connection |
| `requests.get()` | `requests.get(url)` | HTTP GET |
| `requests.post()` | `requests.post(url)` | HTTP POST |
| `httpx.get()` | `httpx.get(url)` | HTTP GET |

### 3. URL Literals

| Pattern | Example | Reason |
|---------|---------|--------|
| `https://` | `"https://api.example.com"` | External resource |
| `http://` | `"http://localhost:8080"` | HTTP endpoint |
| `ftp://` | `"ftp://server.com"` | FTP server |

**Excluded**: `example.com`, `localhost` (development patterns)

### 4. IP Address Literals

| Pattern | Example | Reason |
|---------|---------|--------|
| Literal IPv4 | `203.0.113.42` | External-looking host |

**Excluded**: `127.x.x.x`, `0.0.0.0` (loopback/all interfaces)

## Operator-facing approval hints

Detected patterns are surfaced in **`stderr`**, **`ApprovalRequest.reason`**, and the embedded **`approval`** payload. Concrete URL/IP literals are distilled to hostnames/IP strings via **`normalize_targets`** (`approved_exec_cache.rs`). When hosts are known, approvals append **` → hosts: host1, host2`**.

When there is remote-access risk but **no** extractable literal host (`function_call`, `import`, `network_command`, etc.), approvals append **` → signals: category:snippet; ...`** built by **`approval_remote_operator_suffix`** in `remote_access.rs` (snippet length is capped so lines stay usable in operator UIs and session summaries).

## Approval Flow

```
┌─────────────────────────────────────────────────────────────┐
│ sandbox_exec called                                         │
│                                                             │
│ 1. Policy check (CodeExecution capability)                  │
│    ↓ allowed                                                │
│ 2. Static analysis (remote_access.rs)                       │
│    ├─ No remote patterns → Execute immediately             │
│    └─ Remote patterns found → proceed to approval checks     │
│ 3. Declaration target check (`remote_access.targets`)        │
│    ├─ Target covered → continue                              │
│    └─ Target undeclared → fail-shut deny                     │
│ 4. Approval resolution checks (in order):                    │
│    a. Exec cache hit (identical code fingerprint) → EXECUTE │
│    b. Root-session grant covers targets → EXECUTE           │
│    c. Existing approved/pending approval → REUSE            │
│    d. None of the above → BLOCK + require approval           │
└─────────────────────────────────────────────────────────────┘
```

### Session Approval Grants

When an operator approves `sandbox_exec` for specific hosts, those hosts are recorded as **session approval grants** — pattern-based targets stored in SQLite. Subsequent `sandbox_exec` calls whose detected hosts are covered by a grant are auto-approved.

**Grant targets** support four kinds (set at approval time via `--target`):

| Target kind | Syntax | Example match |
|-------------|--------|---------------|
| `ExactHost` | `host:api.github.com` | `api.github.com` |
| `HostSuffix` | `suffix:*.github.com` | `api.github.com`, `v2.api.github.com` |
| `HostAndPort` | `hostport:api.github.com:443` | `api.github.com:443` |
| `UrlPrefix` | `url:https://api.github.com/public/` | `https://api.github.com/public/users` |

**Grant scope:** `RootSession` (default — all agents in the workflow benefit) or `Session` (only the requesting child session). Set via `--scope root` or `--scope session`.

**Grant expiry:** `--ttl 10m` or `--until 2025-12-31T23:59:59Z`. Without either, the grant lasts until session end or emergency stop.

**Grant revocation:** `autonoetic gateway grants revoke --root-session <id> --host api.example.com --reason "..."` — revokes without emergency stop, emits a causal event.

Grants are cleaned up on session end, emergency stop, or expiry. See [Approval System](approval-system.md) for full details including similarity scoring and analytics.

### Promotion Severity Gating

The `promotion_record` tool enforces mechanical validation:

- `pass=true` with `error` or `critical` findings → **rejected by the gateway**
- `pass=true` with `warning` findings → **rejected** unless every warning includes non-empty `evidence` (e.g., sandbox output proving the issue was investigated)

This prevents evaluators from passing code that has never been functionally validated.

### When Remote Access Detected (`sandbox_exec`)

The tool returns a structured response instead of executing. **`stderr`** and **`approval.reason`** repeat the analyzer summary plus the hint suffix (**`hosts`** or **`signals`**):

```json
{
  "ok": false,
  "exit_code": null,
  "stdout": "",
  "stderr": "Remote access detected: Detected 2 remote access pattern(s) in categories: import, url_literal. Operator approval required to execute code with network access. → hosts: api.open-meteo.com",
  "approval_required": true,
  "remote_access_detected": true,
  "detected_patterns": [
    {
      "category": "import",
      "pattern": "import requests",
      "line_number": 1,
      "reason": "HTTP client library"
    },
    {
      "category": "url_literal",
      "pattern": "https://api.open-meteo.com/v1/forecast",
      "line_number": 5,
      "reason": "URL literal indicates external resource access"
    }
  ],
  "request_id": "apr-a1b2c3d4",
  "suspended": true,
  "message": "Execution suspended pending operator approval (apr-a1b2c3d4). The approved command is persisted and will be used automatically on resume.",
  "approval": {
    "kind": "sandbox_exec",
    "reason": "Remote access detected: Detected 2 remote access pattern(s) in categories: import, url_literal → hosts: api.open-meteo.com",
    "summary": "Sandbox exec: python3 weather_client.py",
    "requested_by_agent_id": "coder.default",
    "session_id": "demo-session-1/coder.default-abc123",
    "retry_field": "approval_ref",
    "subject": {
      "command": "python3 weather_client.py",
      "remote_access_detected": true,
      "detected_patterns": [],
      "normalized_targets": ["api.open-meteo.com"],
      "hosts": ["api.open-meteo.com"]
    }
  }
}
```

When there are **imports or call shapes** suggesting network access but **no literal URL/IP** landed in **`normalize_targets`**, the suffix uses **`signals`** instead (illustrative shape):

```
 ... Operator approval required to execute code with network access. → signals: import:from urllib.request import urlopen; function_call:urlopen(
```

Subject JSON still carries full **`detected_patterns`** for review.

### `artifact_exec`

If an artifact triggers remote-access approval, persisted **`ApprovalRequest.reason`** and returned **`stderr`** use the same suffix: the line starts with **`Artifact exec: {artifact_id} → {remote_analysis.summary}`** and then **` → hosts:`** or **` → signals:`** depending on whether literal targets were extracted.

Example shape:

```json
{
  "stderr": "Remote access detected in artifact weather-artifact. Operator approval required. → hosts: api.example.com"
}
```

## How to Approve Remote Access

When an agent encounters remote access approval:

1. **Agent reports the approval requirement** to the user
2. **User reviews the detected patterns** and the **`hosts` / `signals`** line to understand what network access may occur
3. **User decides** whether to approve or deny
4. **If approved**, user grants scoped host targets and the gateway persists approval/grant scope for reuse

## Pattern Details

### RemoteAccessAnalyzer

Located in: `autonoetic-gateway/src/runtime/remote_access.rs`

```rust
let analysis = RemoteAccessAnalyzer::analyze_code(code);

if analysis.requires_approval {
    // Return approval request
    return ApprovalRequired {
        detected_patterns: analysis.detected_patterns,
        summary: analysis.summary,
    };
}
// Proceed with execution
```

Operator hint suffixes are assembled with **`approval_remote_operator_suffix(concrete_hosts, &detected_patterns)`** so **`sandbox_exec`** and **`artifact_exec`** stay consistent.

### DetectedPattern Structure

```rust
struct DetectedPattern {
    category: String,              // import, function_call, url_literal, ip_address, …
    pattern: String,              // matched text (may be shortened in signals output)
    line_number: Option<usize>,    // approximate line where found (1-indexed)
    reason: String,               // human explanation used for diagnostics
}
```

## Testing

Run remote access analyzer tests:

```bash
cargo test --lib remote_access
```

Test coverage includes:
- No remote access (pure computation)
- HTTP import detection
- urllib import detection
- Socket call detection
- URL literal detection
- IP address detection
- Local IP exclusion
- Combined patterns (import + usage)
- **`approval_remote_operator_suffix`** (hosts preferred; signals when no literals)

## Integration with Agent Capabilities

Agents that legitimately need network access should declare it:

```yaml
capabilities:
  - type: "NetworkAccess"
    hosts: ["api.open-meteo.com", "nominatim.openstreetmap.org"]
  - type: "CodeExecution"
    patterns: ["python3 "]
```

With `NetworkAccess` declared, policy can allow outbound access consistent with manifests and approvals.
