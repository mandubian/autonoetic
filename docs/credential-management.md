# Credential Management

> How agents securely interact with external services requiring authentication, without leaking secrets into the LLM context.

## Overview

Autonoetic provides a credential management system that lets agents:

1. **Check** if credentials exist for a service
2. **Request** setup of new credentials (with automated registration or human-assisted entry)
3. **Use** stored credentials in HTTP requests without exposing secrets to the LLM
4. **Inject** credentials as environment variables into sandbox executions without exposing secrets to the LLM

Secrets are stored in an encrypted vault (AES-256-GCM) and injected at the gateway level — agents never see raw secret values.

## Credential ID Contract

Credential references are mechanical IDs, not secret material.

- Must start with `cred_`
- Must contain only ASCII letters, digits, `_`, `-`, `.`
- Must not be empty and must be short (gateway-enforced limit)
- Secret-like strings are rejected when passed as `credential_id`

Use `credential_check`/`credential_setup` outputs as the source of truth for credential IDs.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Agent (sandboxed)                                          │
│                                                             │
│  credential_check("github")  ──→  CredentialRecord (no secret)
│  credential_request({         ──→  Gateway injects secret   │
│      host: "api.github.com",          into HTTP request     │
│      path: "/user",                   (agent never sees it) │
│      auth: { type: "bearer" }         ──→  Redacted response│
│  })                                                         │
│  credential_setup({             ──→  May suspend for        │
│      service: "github",                human approval        │
│      steps: [...]                      (UserPrompt step)    │
│  })                                                         │
│  sandbox_exec({                 ──→  Gateway injects secret │
│      command: "python3 app.py",        as env var into       │
│      credential_env: [{                sandbox (agent never  │
│        credential_id: "cred_abc",      sees the value)       │
│        env_var: "API_KEY"             ──→  Script reads from │
│      }]                                os.environ            │
│  })                                                         │
└─────────────────────────────────────────────────────────────┘
         │
         ▼
┌─────────────────────────────────────────────────────────────┐
│  Gateway                                                     │
│                                                             │
│  ┌───────────────┐    ┌───────────────┐    ┌─────────────┐ │
│  │ CredentialStore│    │ ApprovalQueue │    │    Vault    │ │
│  │  (SQLite)     │    │  (suspension) │    │ (encrypted) │ │
│  └───────────────┘    └───────────────┘    └─────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

## Credential Lifecycle

### 1. Check — Does a credential exist?

```python
result = sdk.tools.invoke("credential_check", {
    "service": "github"
})
# → { "found": true, "credential_id": "cred_abc", "expires_at": ... }
# → { "found": false }
```

Returns a `CredentialRecord` (metadata only — no secret value).

### 2. Setup — Register a new credential

```python
result = sdk.tools.invoke("credential_setup", {
    "service": "github",
    "registration_url": "https://github.com/settings/tokens/new",
    "network_policy": {
        "allowed_hosts": ["api.github.com"],
        "empty_host_denied": true
    },
    "steps": [
        {
            "type": "user_prompt",
            "prompt": "Enter your GitHub Personal Access Token",
            "secret_fields": ["github_token"],
            "inject_as": "Authorization",
            "expires_at": "2027-01-01T00:00:00Z"
        }
    ]
})
# → { "ok": false, "suspended": true, "approval_required": true,
#     "approval_request_id": "apv_xyz" }
```

The `user_prompt` step suspends the session and creates an `ApprovalRequest` with `ScheduledAction::CredentialPrompt`. A human operator must approve via the gateway CLI/TUI.

### 3. Approval — Human operator enters the secret

**Interactive TUI (recommended):**
```bash
autonoetic gateway approve apv_xyz
# → Masked password prompt (no shell history)
# → Secret stored in vault, CredentialRecord created
```

**Non-interactive CLI:**
```bash
autonoetic gateway approve apv_xyz --secret github_token=ghp_xxxx
```

The approval handler validates that all `secret_fields` from the `CredentialPrompt` are provided (rejects `None` and empty). On success, a `CredentialRecord` is created with full metadata (`inject_as`, `allowed_hosts`, `expires_at`).

### 4. Resume — Agent continues with approval_ref

After approval, the agent resumes automatically. The gateway passes the `approval_ref` to `credential_setup`, which detects the completed `UserPrompt` step and proceeds to store the secret in the vault.

### 5. Request — Use the credential in HTTP calls

```python
result = sdk.tools.invoke("credential_request", {
    "service": "github",
    "host": "api.github.com",
    "path": "/user",
    "method": "GET",
    "auth": {
        "type": "bearer",
        "header_name": "Authorization"
    }
})
# → { "status": 200, "body": "...", "redacted": true }
```

The gateway:
1. Looks up the `CredentialRecord` by service name
2. Fetches the secret from the vault
3. Injects it into the HTTP request (header, query param, or body)
4. Redacts the response to prevent secret leakage back to the agent

### 6. Inject — Use the credential in sandbox executions

For scripts that need API keys or tokens at runtime, the `credential_env` parameter on `sandbox_exec` and `artifact_exec` injects secrets as environment variables into the sandbox. The gateway resolves the secret from the vault server-side — it never appears in the LLM context.

```python
# Agent calls sandbox_exec with credential_env:
result = sdk.tools.invoke("sandbox_exec", {
    "command": "python3 /tmp/weather_fetcher.py London today",
    "credential_env": [
        {"credential_id": "cred_abc123", "env_var": "OPENWEATHER_API_KEY"}
    ]
})
```

The script reads the secret from the environment:

```python
import os
api_key = os.environ.get("OPENWEATHER_API_KEY")
```

**How agents learn about this:**
- The **coder** writes scripts that read secrets from env vars (instructed in `foundation_sdk.md`)
- The **planner** delegates with the `credential_id` in the message
- The **executor** uses `credential_env` when calling `sandbox_exec` or `artifact_exec`
- No agent ever needs `CredentialAccess` to use `credential_env` — only the `credential_id`

**Delegation flow:**
```
1. Operator: autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY
2. Planner: delegates to coder: "Write a weather script using os.environ['OPENWEATHER_API_KEY']"
3. Planner: delegates to executor: "Run the script, inject cred_abc123 as OPENWEATHER_API_KEY"
4. Executor: artifact_prepare({ artifact_id, entrypoint, required_credentials })
5. Gateway: resolves credentials, checks approval, returns deployment_ticket
6. Executor: artifact_exec({ deployment_ticket, artifact_id, entrypoint, args })
7. Gateway: injects secret as env var, executes with network access
8. Script: reads os.environ["OPENWEATHER_API_KEY"], makes API call
```

The `artifact_prepare` tool eliminates the multi-suspend problem where execution first needs approval, then credential resolution, then re-approval. It does everything in a single pass: static analysis for remote access, credential verification, and a single approval request covering all domains + credential injection.

## Token Refresh (OAuth / Expiring Credentials)

When credentials have expiring access tokens (e.g. OAuth2), the gateway can automatically refresh them without human intervention.

### How it works

A `CredentialRecord` can carry **refresh metadata** — 7 optional fields that tell the gateway how to obtain a new access token when the current one expires:

| Field | Purpose | Default |
|-------|---------|---------|
| `refresh_token_secret_name` | Vault key holding the refresh token | — |
| `refresh_url` | OAuth token endpoint URL | — |
| `refresh_method` | HTTP method for refresh request | `POST` |
| `refresh_headers` | Static headers (e.g. `Content-Type`) | — |
| `refresh_extract_access_token` | JSON path to extract new access token from response | `access_token` |
| `refresh_extract_refresh_token` | JSON path to extract rotated refresh token | — |
| `refresh_extract_expires_in` | JSON path to extract TTL in seconds | — |

If `refresh_url` is `None`, the credential is considered non-refreshable.

### credential_refresh (explicit)

Agents can call `credential_refresh` to force a token refresh:

```python
result = sdk.tools.invoke("credential_refresh", {
    "credential_id": "cred_oauth_github"
})
# → { "ok": true, "credential_id": "...", "refreshed": true, "new_expires_at": "..." }
```

The gateway:
1. Reads the refresh token from the vault
2. POSTs `{ "grant_type": "refresh_token", "refresh_token": "..." }` to `refresh_url`
3. Extracts the new access token from the JSON response via `refresh_extract_access_token`
4. If `refresh_extract_refresh_token` is set, extracts and stores the rotated refresh token
5. If `refresh_extract_expires_in` is set, updates `expires_at` on the credential record
6. Persists the updated vault and credential record

### 401 Auto-Retry (implicit)

When `credential_request` receives a **401 Unauthorized** response, the gateway automatically attempts a refresh before retrying the original request — provided the credential has `refresh_url` set and no prior refresh was attempted in the same call:

```
1. credential_request → HTTP request with current access token
2. Server returns 401
3. Gateway calls try_auto_refresh() internally
4. On success: retries the original request with the new access token
5. On failure: returns the 401 response to the agent
```

The auto-retry only fires once per `credential_request` call (guarded by a `refreshed` flag) to avoid infinite loops.

### Configuring refresh during credential_setup

Refresh metadata is typically set during `credential_setup` or via the CLI. For OAuth flows, the setup steps would capture both the access token and refresh token, storing the refresh metadata alongside the credential record.

### Security

- The refresh token is stored in the same encrypted vault as the access token
- Refresh tokens are never exposed to the agent context — only the gateway reads and uses them
- Refresh token rotation (extracting a new refresh token from the response) is supported but optional

## Vault Encryption

Secrets are stored in an encrypted vault using AES-256-GCM with a random 96-bit nonce per entry.

**Master key sources (in order of precedence):**
1. `AUTONOETIC_VAULT_KEY` env var (hex-encoded 32-byte key)
2. `AUTONOETIC_VAULT_KEY_PATH` env var (path to file containing hex key)

Without a master key, the vault cannot be persisted or loaded.

## CLI Credential Management

The `autonoetic agent credential` command provides direct vault operations without going through an agent session.

### `autonoetic agent credential put`

Store a secret in the encrypted vault and register a credential record.

```bash
# Interactive prompt (masked input)
autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY

# From environment variable
export OPENWEATHER_API_KEY="your-api-key"
autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY --from-env OPENWEATHER_API_KEY

# Direct value (less secure — visible in shell history)
autonoetic agent credential put --service openweathermap --secret-name OPENWEATHER_API_KEY --value "your-api-key"

# With all options
autonoetic agent credential put \
  --service openweathermap \
  --secret-name OPENWEATHER_API_KEY \
  --from-env OPENWEATHER_API_KEY \
  --credential-id cred_myweather \
  --inject-as env:OPENWEATHER_API_KEY \
  --allowed-hosts api.openweathermap.org \
  --expires-at 2027-01-01T00:00:00Z
```

| Option | Required | Description |
|--------|----------|-------------|
| `--service` | Yes | Service name (e.g., `openweathermap`, `github`) |
| `--secret-name` | Yes | Vault key for the secret (e.g., `OPENWEATHER_API_KEY`) |
| `--from-env` | No | Read secret from this environment variable |
| `--value` | No | Provide secret directly (use `--from-env` for better security) |
| `--credential-id` | No | Credential ID (auto-generated UUID if omitted) |
| `--inject-as` | No | Injection method (e.g., `env:API_KEY`, `bearer`, `header:X-Custom`) |
| `--allowed-hosts` | No | Hosts this credential may be used with |
| `--expires-at` | No | ISO 8601 expiry timestamp |

If neither `--from-env` nor `--value` is provided, prompts for the secret with masked input.

### `autonoetic agent credential list`

List registered credentials (metadata only — never shows secret values).

```bash
# List all
autonoetic agent credential list

# Filter by service
autonoetic agent credential list --service openweathermap

# JSON output
autonoetic agent credential list --json
```

### `autonoetic agent credential rm`

Remove a credential and its secret from the vault.

```bash
autonoetic agent credential rm cred_abc123
```

## Security Model

| Threat | Mitigation |
|--------|-----------|
| Secret in LLM context | Secrets never exposed to agent; injected at gateway level (HTTP headers or sandbox env vars) |
| Secret in shell history | Interactive TUI uses masked password prompt; `--from-env` reads from env var |
| Secret exfiltration via `extract_public` | Paths overlapping `extract_secrets` paths are silently dropped |
| Secret exfiltration via `credential_env` | Env vars are blocked in sandbox (`env`, `printenv` forbidden by policy) |
| Empty host network access | Explicitly denied in network policy check |
| Plaintext vault on disk | AES-256-GCM encryption at rest |
| Missing secrets bypass | `approve_request()` always requires and validates all `secret_fields` for `CredentialPrompt` |

## Capability Gating

The `CredentialAccess` capability type controls which agents can use credential tools:

```yaml
capabilities:
  - type: "CredentialAccess"
    allowed: ["github", "openai"]  # service name patterns
```

## Files

| File | Role |
|------|------|
| `autonoetic-gateway/src/vault.rs` | Vault (SecretString anti-leak, AES-256-GCM) |
| `autonoetic-gateway/src/runtime/tools/credential.rs` | credential_check, credential_request, credential_setup, credential_refresh |
| `autonoetic-gateway/src/runtime/tools/sandbox.rs` | sandbox_exec with `credential_env` injection |
| `autonoetic-gateway/src/runtime/tools/artifact_exec.rs` | artifact_exec with `credential_env` injection |
| `autonoetic-gateway/src/scheduler/gateway_store/credentials.rs` | Credential CRUD in SQLite |
| `autonoetic-gateway/src/scheduler/approval.rs` | Approval handler for CredentialPrompt |
| `autonoetic/src/cli/gateway.rs` | Interactive TUI with masked password prompt |
| `autonoetic/src/cli/agent.rs` | `agent credential put/list/rm` CLI handlers |
| `autonoetic-gateway/src/runtime/foundation_sdk.md` | Agent instructions for env var secret pattern |
