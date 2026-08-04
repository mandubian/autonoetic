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

## Credential Tools

| Tool | Purpose |
|------|---------|
| `credential_check` | Check if credentials exist for a service |
| `credential_setup` | Set up credentials with automated or human-assisted entry |
| `credential_request` | Use stored credentials in HTTP requests without seeing secrets |
| `credential_refresh` | Refresh an OAuth / token credential |

## Flow

```
Agent calls credential_check("github")
  → Returns CredentialRecord (no secret value)

Agent calls credential_setup({ service: "github", steps: [...] })
  → May suspend for human approval (UserPrompt step)
  → Returns credential_id

Agent calls credential_refresh({ credential_id: "cred_abc" })
  → Refreshes OAuth / token credentials on 401 or explicit request

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

## Env-Var Contract for Spawned Script Agents

When a script agent declares `credential_services` and is spawned, the gateway injects each resolved credential as an env var:

- **Name**: the credential's `inject_as` when it holds a valid env-var identifier, otherwise the service-derived `<SERVICE>_SECRET` (e.g. service `photos` → `PHOTOS_SECRET`). HTTP injection styles (`bearer`, `header:X-…`) belong to `credential_request` and fall back to the derived name.
- **Multi-field credentials**: a `user_prompt` flow that collected several secret fields stores a combined flat JSON object, and the record points at it. At spawn the combined object is injected under the credential's env-var name, **and** each field is also injected under `<SERVICE>_<FIELD>` (uppercase-sanitized) — e.g. fields `account_name` + `app_token` for service `photos` yield `PHOTOS_ACCOUNT_NAME` and `PHOTOS_APP_TOKEN` alongside the combined value. A script can read either shape.
- **Verify before spawning**: `credential_check` and `credential_setup` return an `injection` block per credential — `env_var`, `value_shape` (`scalar` / `json_object`), and for multi-field credentials the `fields` and derived `field_env_vars`. Check it against what the script reads; do not guess env-var names.
- **Wrong env-var name on an existing credential**: retry `credential_setup` with the same `service` (and `label`, if one was used) plus the correct `inject_as`. The dedup path reuses the credential and applies the new name (`inject_as_updated: true`) — the secret is untouched and never re-collected.

## CLI Credential Commands

Operators can also manage credentials directly:

```bash
autonoetic agent credential put --service github --value ghp_xxx
autonoetic agent credential list
autonoetic agent credential rm cred_abc123
```

## Important Rules

1. **Never put secrets in tool arguments** — always use `credential_env` or `credential_request`
2. **Never log secrets** — the gateway redacts secrets in logs automatically
3. **Credential IDs are not secrets** — they are mechanical identifiers that can be logged and shared
4. **Vault is encrypted at rest** — AES-256-GCM with a gateway-managed key
5. **OAuth 401 auto-retry** — the gateway can refresh and retry requests when a credential has a refresh token
