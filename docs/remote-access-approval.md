# Remote Access Approval

This document describes the static analysis system for detecting remote/network access in sandboxed code execution.

## Overview

When `sandbox_exec` is called, the code is **statically analyzed** before execution to detect patterns that require network access. If detected, execution is blocked and requires operator approval.

This is a **deterministic** security check that does not rely on the LLM's self-declaration.

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
| Public IP | `"192.168.1.100"` | External host |

**Excluded**: `127.x.x.x`, `0.0.0.0` (local/loopback)

## Approval Flow

```
┌─────────────────────────────────────────────────────────────┐
│ sandbox_exec called                                         │
│                                                             │
│ 1. Policy check (CodeExecution capability)                  │
│    ↓ allowed                                                │
│ 2. Static analysis (remote_access.rs)                       │
│    ├─ No remote patterns → Execute immediately              │
│    └─ Remote patterns found → proceed to approval checks    │
│ 3. Approval resolution checks (in order):                   │
│    a. Exec cache hit (identical code fingerprint) → EXECUTE │
│    b. Session grant covers targets (scope-aware) → EXECUTE  │
│    c. Existing approved/pending approval → REUSE            │
│    d. None of the above → BLOCK + require approval          │
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

### When Remote Access Detected

The tool returns a structured response instead of executing:

```json
{
  "ok": false,
  "exit_code": null,
  "stdout": "",
  "stderr": "Remote access detected: Detected 2 remote access pattern(s) in categories: import, url_literal. Operator approval required to execute code with network access.",
  "approval_required": true,
  "remote_access_detected": true,
  "detected_patterns": [
    {
      "category": "import",
      "pattern": "import requests",
      "line_number": 1,
      "reason": "Makes HTTP requests"
    },
    {
      "category": "url_literal",
      "pattern": "https://api.open-meteo.com/v1/forecast",
      "line_number": 5,
      "reason": "External resource"
    }
  ],
  "request_id": "apr-a1b2c3d4",
  "suspended": true,
  "message": "Execution suspended pending operator approval (apr-a1b2c3d4). The approved command is persisted and will be used automatically on resume.",
  "approval": {
    "kind": "sandbox_exec",
    "reason": "Remote access detected: 2 patterns → hosts: api.open-meteo.com",
    "summary": "Sandbox exec: python3 weather_client.py",
    "requested_by_agent_id": "coder.default",
    "session_id": "demo-session-1/coder.default-abc123",
    "retry_field": "approval_ref",
    "subject": {
      "command": "python3 weather_client.py",
      "remote_access_detected": true,
      "detected_patterns": [...],
      "normalized_targets": ["host:api.open-meteo.com"],
      "hosts": ["api.open-meteo.com"]
    }
  }
}
```

The `reason` field now includes extracted hostnames when URL literals are detected in the code, giving operators immediate visibility into what domains will be accessed.```json
{
  "ok": false,
  "exit_code": null,
  "stdout": "",
  "stderr": "Remote access detected: Detected 2 remote access pattern(s) in categories: import, url_literal. Operator approval required to execute code with network access.",
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
  ]
}
```

## How to Approve Remote Access

When an agent encounters remote access approval:

1. **Agent reports the approval requirement** to the user
2. **User reviews the detected patterns** to understand what network access is needed
3. **User decides** whether to approve or deny
4. **If approved**, user can:
   - Grant `NetworkAccess` capability to the agent
   - Or provide an alternative implementation that doesn't require network access

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

### DetectedPattern Structure

```rust
struct DetectedPattern {
    category: String,      // "import", "function_call", "url_literal", "ip_address"
    pattern: String,       // The matched text
    line_number: Option<usize>,  // Line where found (1-indexed)
    reason: String,        // Why this indicates remote access
}
```

## Testing

Run remote access analyzer tests:

```bash
cargo test --lib remote_access
```

Test coverage:
- No remote access (pure computation)
- HTTP import detection
- urllib import detection
- Socket call detection
- URL literal detection
- IP address detection
- Local IP exclusion
- Combined patterns (import + usage)

## Integration with Agent Capabilities

Agents that legitimately need network access should declare it:

```yaml
capabilities:
  - type: "NetworkAccess"
    hosts: ["api.open-meteo.com", "nominatim.openstreetmap.org"]
  - type: "CodeExecution"
    patterns: ["python3 "]
```

With `NetworkAccess` declared, the static analysis check can be bypassed for approved hosts.
