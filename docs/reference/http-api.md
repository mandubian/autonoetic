# Remote Agents & HTTP Content API

Autonoetic supports remote agents that can read/write content on a different machine than the gateway.

## Architecture

```
┌─────────────────┐         HTTP/REST          ┌─────────────────┐
│ Remote Agent    │ ◄──────────────────────────►│ Gateway         │
│ (different host)│    Bearer token auth        │ (central server)│
│                 │                             │                 │
│ autonoetic_sdk  │                             │ Content Store   │
│ (http mode)     │                             │ Session History │
└─────────────────┘                             └─────────────────┘
```

## Listener binding & ports

- **JSON-RPC (localhost IPC)** continues to use `GatewayConfig.port` (default `4000`) and only accepts traffic from `127.0.0.1`.
- **HTTP ingress** listens on **`0.0.0.0:http_port`** (default **`4100`**). Set `http_port: 0` to disable the HTTP server entirely while keeping JSON-RPC available.

Both transports share `AUTONOETIC_SHARED_SECRET` for authentication.

## Channel metadata convention (`event.ingest`)

Remote bridges (Discord bots, WhatsApp relays, web frontends, etc.) should populate `metadata.channel.kind` to identify their transport while keeping the JSON envelope identical to TCP JSON-RPC:

```json
{
  "channel": {
    "kind": "http",
    "channel_id": "bridge:customer-portal",
    "sender_id": "user-123",
    "session_id": "optional-session-hint"
  }
}
```

Suggested values for `kind`: `"http"`, `"discord"`, `"whatsapp"`, `"slack"`, `"terminal"` (built-in TUI uses `"terminal"`).

The chat TUI additionally forwards `root_session_id` inside `metadata` for `/policy` requests so governance agents can scope proposals.

## HTTP Event Ingress & Streaming

### `POST /api/event/ingest`

Accepts a JSON body matching the JSON-RPC **`event.ingest`** `params` object (same fields as the TCP API). Returns a JSON-RPC envelope (`result` or `error`) identical to line-delimited TCP responses.

```http
POST /api/event/ingest
Authorization: Bearer <AUTONOETIC_SHARED_SECRET>
Content-Type: application/json

{
  "event_type": "chat",
  "message": "hello from bridge",
  "session_id": "sess-http-123",
  "target_agent_id": "planner.default",
  "async_mode": true,
  "served_party": "user:alice",
  "metadata": {
    "channel": {
      "kind": "http",
      "channel_id": "bridge:demo",
      "sender_id": "external-client",
      "session_id": "sess-http-123"
    }
  }
}
```

**`served_party`** (optional) — on whose behalf the run executes. Use
`user:<id>` when the end-user is distinct from the operator (a hosted or
multi-tenant deployment), or `operator` to say so explicitly. Omitted, the
operator is recorded as the served party and the row is marked a *default*
rather than a declaration.

Bound **once per root session**: the first ingest answers "who is this for?"
and later ones cannot rewrite it — a run whose served party could change
mid-flight would be worthless as evidence. A conflicting later value is logged
and ignored, never applied.

This is **attributive only**. §12 (`U-1`–`U-3`) is `MISSING` in the active
constitution — nothing yet honours a refusal, packages an account, or exits
with data — so the binding enforces no clause and its causal event names none.
It exists because attribution cannot be backfilled: see
[`../concepts/philosophy.md`](../concepts/philosophy.md) §4.7.

### `GET /api/session/stream/{session_id}` (SSE)

Streams periodic JSON-RPC **`session.status`** poll results as Server-Sent Events. Designed for clients that already submitted an `event.ingest` with `"async_mode": true`.

- **Auth**: `Authorization: Bearer …` **or** `?token=<AUTONOETIC_SHARED_SECRET>` (required for browser `EventSource`, which cannot set headers).
- **Tuning**: optional `interval_ms` query parameter (100–10 000 ms, default `500`).
- **Termination**: the stream stops once the embedded JSON shows a terminal async status (`completed`, approvals/input suspension, failure, or JSON-RPC error).

```http
GET /api/session/stream/sess-http-123?token=<AUTONOETIC_SHARED_SECRET>&interval_ms=750
Accept: text/event-stream
```

Each SSE `data` payload is the serialized `JsonRpcResponse` from `session.status`.

### `GET /api/operator/activity/stream/{root_session_id}` (SSE)

Streams **operator activity** rows for a root session — the channel-neutral feed used by the chat TUI and future messaging bridges (Discord, WhatsApp, etc.). Polls `operator.activity.list` on an interval until the client disconnects.

- **Auth**: Bearer or `?token=` (same as session stream).
- **Query**: `interval_ms` (100–10 000, default `500`), optional `after` activity cursor for reconnect.
- **Events**: `operator.activity` (JSON batch `{ "activities": [...] }`), `operator.activity.heartbeat` when empty, `operator.activity.error` on RPC failure.

```http
GET /api/operator/activity/stream/session-46d65624?token=<AUTONOETIC_SHARED_SECRET>&interval_ms=1000
Accept: text/event-stream
```

JSON-RPC equivalent: `operator.activity.list` with `{ "root_session_id", "after_activity_id", "limit", "min_severity" }`.

## HTTP Content API Endpoints

All endpoints require Bearer token authentication.

### Write Content

```http
POST /api/content/write
Authorization: Bearer <shared_secret>
Content-Type: application/json

{
  "session_id": "my-session",
  "name": "main.py",
  "content": "print('hello')",
  "encoding": "utf8"  // or "base64" for binary
}
```

Response:
```json
{
  "handle": "sha256:abc123...",
  "name": "main.py",
  "size_bytes": 14
}
```

### Read Content

**GET** (path-based):
```http
GET /api/content/read/{session_id}/{name_or_handle}
Authorization: Bearer <shared_secret>
```

**POST** (body-based):
```http
POST /api/content/read
Authorization: Bearer <shared_secret>
Content-Type: application/json

{
  "session_id": "my-session",
  "name_or_handle": "main.py"
}
```

Response:
```json
{
  "content": "cHJpbnQoJ2hlbGxvJyk=",  // base64 encoded
  "encoding": "base64",
  "size_bytes": 14,
  "handle": "sha256:abc123..."
}
```

### Persist Content

```http
POST /api/content/persist
Authorization: Bearer <shared_secret>
Content-Type: application/json

{
  "session_id": "my-session",
  "handle": "sha256:abc123..."
}
```

Response:
```json
{
  "handle": "sha256:abc123...",
  "persisted": true
}
```

### List Content Names

```http
GET /api/content/names?session_id=my-session
Authorization: Bearer <shared_secret>
```

Response:
```json
{
  "names": [
    {"name": "main.py", "handle": "sha256:abc123..."},
    {"name": "utils.py", "handle": "sha256:def456..."}
  ]
}
```

## Python SDK Usage

### Environment Variables

| Variable | Description |
|----------|-------------|
| `AUTONOETIC_HTTP_URL` | Gateway HTTP URL (e.g., `http://gateway:8080`) |
| `AUTONOETIC_SHARED_SECRET` | Bearer token for authentication |
| `CCOS_SOCKET_PATH` | Unix socket path (local mode only) |

### Remote Agent (Environment)

Set these before initializing SDK:

```python
import os
os.environ["AUTONOETIC_HTTP_URL"] = "http://gateway-host:8080"
os.environ["AUTONOETIC_SHARED_SECRET"] = "my-secret"

from autonoetic_sdk import Client
sdk = Client()

# Content operations go via HTTP
sdk.files.write("main.py", "print('hello')")  # Returns handle
content = sdk.files.read("main.py")            # Returns content dict
sdk.files.persist("sha256:abc123...")          # Mark as permanent
```

### Remote Agent (Explicit)

```python
from autonoetic_sdk import init_remote

sdk = init_remote(
    gateway_url="http://gateway-host:8080",
    token="my-secret"
)

sdk.files.write("data.json", '{"key": "value"}')
```

### Local Agent (Unchanged)

For agents running on the same machine as the gateway:

```python
from autonoetic_sdk import Client

# Automatically uses Unix socket
sdk = Client()
sdk.files.write("main.py", "print('hello')")  # Goes via Unix socket
```

## Security

### Authentication
- All HTTP endpoints require `Authorization: Bearer <token>` header
- Token must match the gateway's `AUTONOETIC_SHARED_SECRET`
- 401 returned for missing auth, 403 for invalid token

### Input Validation
- Session IDs: alphanumeric, `-`, `_`, `.` only (max 128 chars)
- Content names: alphanumeric, `-`, `_`, `.`, `/` only (max 512 chars)
- No `..` allowed (path traversal prevention)
- Max content size: 10MB per request

### Recommendations for Production
1. **TLS/HTTPS**: Run behind nginx or use a load balancer with TLS
2. **Firewall**: Restrict HTTP API port to known agent hosts
3. **Secret rotation**: Periodically rotate the shared secret
4. **Network isolation**: Run gateway in a private network/VPC

## Configuration

### Gateway Side

Set `http_port` in `gateway.yaml` (defaults to `4100`). The HTTP server shares `AUTONOETIC_SHARED_SECRET` with JSON-RPC/OFP for authentication.

```bash
export AUTONOETIC_SHARED_SECRET="my-secret"
autonoetic gateway start --config /path/to/gateway.yaml
```

### Agent Side

For remote agents, configure either:

**Option A: Environment (recommended)**
```bash
export AUTONOETIC_HTTP_URL="http://gateway-host:8080"
export AUTONOETIC_SHARED_SECRET="my-secret"
python agent.py
```

**Option B: SDK parameters**
```python
from autonoetic_sdk import init_remote
sdk = init_remote("http://gateway-host:8080", token="my-secret")
```

## Limitations

HTTP exposes **content APIs**, **`event.ingest`**, and **async session polling via SSE**. Everything else (rich MCP transports, vault RPCs, etc.) remains oriented around local sockets unless explicitly added.

| Operation | Local JSON-RPC | Remote HTTP |
|-----------|----------------|-------------|
| `files.write/read/persist` | Yes | Yes |
| `event.ingest` (+ async follow-up) | Yes | Yes (`POST` + SSE stream) |
| Vault / secrets tooling | Yes | No |
| Ad-hoc MCP transports | Yes | Not via HTTP |

Treat HTTP as an ingress surface for orchestrators you trust — layer TLS/network ACLs accordingly.

## Agent Manifest Configuration

Remote agents can declare their gateway connection in their `SKILL.md` manifest. This is useful for agents that always connect to a specific gateway.

### Manifest Fields

Add these optional fields to your agent's `SKILL.md` frontmatter:

```yaml
---
name: "my.remote.agent"
description: "A remote agent that connects to a central gateway"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.remote.agent"
      name: "My Remote Agent"
      description: "Connects to central gateway via HTTP"
    # Remote gateway configuration
    gateway_url: "http://gateway-host:8080"
    gateway_token: "my-secret-token"  # Optional, can use env var instead
    llm_config:
      provider: "openai"
      model: "gpt-4o"
---
# My Remote Agent

This agent connects to the central gateway via HTTP.
```

### Environment Variable Priority

The SDK checks for gateway configuration in this order:
1. `http_url` parameter passed to `init()` or `init_remote()`
2. `AUTONOETIC_HTTP_URL` environment variable
3. `gateway_url` in the agent manifest (injected as env var by gateway)

The same applies for the token:
1. `token` parameter
2. `AUTONOETIC_SHARED_SECRET` environment variable
3. `gateway_token` in the manifest

### Gateway-Side Injection

When the gateway spawns an agent with `gateway_url` set in the manifest, it will automatically inject:
- `AUTONOETIC_HTTP_URL` - Set to the manifest's `gateway_url`
- `AUTONOETIC_SHARED_SECRET` - Set to the manifest's `gateway_token` (if provided)

This means the SDK in the sandbox will automatically use HTTP mode without any code changes.

### Use Cases

**Distributed Coding Teams**: Multiple coder agents on different machines can write to a central gateway's content store.

**CI/CD Integration**: Build agents can read/write artifacts to the gateway without sharing filesystem access.

**Multi-Gateway Federation**: An agent can be configured to connect to a specific gateway for specialized content.

## Example: Remote Coder Agent

```python
#!/usr/bin/env python3
"""Remote coder agent that writes files to a central gateway."""

from autonoetic_sdk import init_remote
import json
import sys

# Initialize for remote gateway
sdk = init_remote(
    gateway_url="http://gateway-host:8080",
    token="my-secret"
)

def create_weather_agent():
    """Create a simple weather agent and write files."""
    
    # Write main script
    main_py = '''
def get_weather(location: str) -> dict:
    """Get weather for a location."""
    return {"location": location, "temp": 22, "condition": "sunny"}
'''
    main_handle = sdk.files.write("weather/main.py", main_py)
    print(f"main.py: {main_handle}")
    
    # Write SKILL.md
    skill_md = '''---
name: "weather"
description: "Weather data retrieval"
script_entry: "main.py"
---

# Weather Agent
Retrieves weather data for locations.
'''
    skill_handle = sdk.files.write("weather/SKILL.md", skill_md)
    print(f"SKILL.md: {skill_handle}")
    
    # Persist for installation
    sdk.files.persist(main_handle["handle"])
    sdk.files.persist(skill_handle["handle"])
    
    return [main_handle["handle"], skill_handle["handle"]]

if __name__ == "__main__":
    handles = create_weather_agent()
    print(f"Created agent with {len(handles)} files")
```
