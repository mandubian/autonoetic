# Credential Management

> How agents securely interact with external services requiring authentication, without leaking secrets into the LLM context.

## Overview

Autonoetic provides a credential management system that lets agents:

1. **Check** if credentials exist for a service
2. **Request** setup of new credentials (with automated registration or human-assisted entry)
3. **Use** stored credentials in HTTP requests without exposing secrets to the LLM

Secrets are stored in an encrypted vault (AES-256-GCM) and injected at the gateway level — agents never see raw secret values.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Agent (sandboxed)                                          │
│                                                             │
│  credential.check("github")  ──→  CredentialRecord (no secret)
│  credential.request({         ──→  Gateway injects secret   │
│      host: "api.github.com",          into HTTP request     │
│      path: "/user",                   (agent never sees it) │
│      auth: { type: "bearer" }         ──→  Redacted response│
│  })                                                         │
│  credential.setup({             ──→  May suspend for        │
│      service: "github",                human approval        │
│      steps: [...]                      (UserPrompt step)    │
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
result = sdk.tools.invoke("credential.check", {
    "service": "github"
})
# → { "found": true, "credential_id": "cred_abc", "expires_at": ... }
# → { "found": false }
```

Returns a `CredentialRecord` (metadata only — no secret value).

### 2. Setup — Register a new credential

```python
result = sdk.tools.invoke("credential.setup", {
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

After approval, the agent resumes automatically. The gateway passes the `approval_ref` to `credential.setup`, which detects the completed `UserPrompt` step and proceeds to store the secret in the vault.

### 5. Request — Use the credential in HTTP calls

```python
result = sdk.tools.invoke("credential.request", {
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

## Vault Encryption

Secrets are stored in an encrypted vault using AES-256-GCM with a random 96-bit nonce per entry.

**Master key sources (in order of precedence):**
1. `AUTONOETIC_VAULT_KEY` env var (hex-encoded 32-byte key)
2. `AUTONOETIC_VAULT_KEY_PATH` env var (path to file containing hex key)

Without a master key, the vault cannot be persisted or loaded.

## Security Model

| Threat | Mitigation |
|--------|-----------|
| Secret in LLM context | Secrets never exposed to agent; injected at gateway level |
| Secret in shell history | Interactive TUI uses masked password prompt; `--secret` flag flagged as future risk |
| Secret exfiltration via `extract_public` | Paths overlapping `extract_secrets` paths are silently dropped |
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
| `autonoetic-gateway/src/runtime/store.rs` | SecretStoreRuntime (JSON extraction, redaction) |
| `autonoetic-gateway/src/runtime/tools/credential.rs` | credential.check, credential.request, credential.setup |
| `autonoetic-gateway/src/scheduler/approval.rs` | Approval handler for CredentialPrompt |
| `autonoetic/src/cli/gateway.rs` | Interactive TUI with masked password prompt |
| `autonoetic/src/cli/common.rs` | CLI approval command with `--secret` flags |
| `autonoetic-gateway/tests/credential_integration.rs` | Integration tests |
