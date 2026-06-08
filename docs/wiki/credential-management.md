# Credential Management

## Overview

The credential system lets agents securely interact with external services without ever seeing secret values. Secrets are stored in an AES-256-GCM encrypted vault and injected at the gateway level.

## Credential ID Contract

- Must start with `cred_`
- Must contain only ASCII letters, digits, `_`, `-`, `.`
- Secret-like strings are **rejected** when passed as `credential_id`
- Use `credential_check`/`credential_setup` outputs as the source of truth for credential IDs

## Flow

```
Agent calls credential_check("github")
  → Returns CredentialRecord (no secret value)

Agent calls credential_setup({ service: "github", steps: [...] })
  → May suspend for human approval (UserPrompt step)
  → Returns credential_id

Agent calls sandbox_exec({ command: "python3 app.py",
    credential_env: [{ credential_id: "cred_abc", env_var: "API_KEY" }] })
  → Gateway injects secret as env var into sandbox
  → Script reads from os.environ
  → Agent never sees the value
```

## Credential Injection Methods

| Method | How | Agent Sees Secret? |
|--------|-----|---------------------|
| `credential_env` on `sandbox_exec` | Injected as env var in sandbox | No |
| `credential_env` on `artifact_exec` | Injected as env var in sandbox | No |
| `credential_request` | Gateway makes HTTP request with auth | No (response is redacted) |

## Important Rules

1. **Never put secrets in tool arguments** — always use `credential_env` or `credential_request`
2. **Never log secrets** — the gateway redacts secrets in logs automatically
3. **Credential IDs are not secrets** — they are mechanical identifiers that can be logged and shared
4. **Vault is encrypted at rest** — AES-256-GCM with a gateway-managed key
