> **ARCHIVED** — Historical design or implementation record. Not current source-of-truth. See [`docs/README.md`](../README.md) for live references.
>

> Status: **Implemented (2026-05-23)**.
> No backward compatibility preserved — all changes are breaking.

## 1. Problem Statement

The credential system assumed **one credential per service**. Two agents that need
separate identities on the same external service (e.g., two distinct registrations
on a "moltbook" messaging platform) could not coexist because:

1. **`credential_setup` dedup**: if any credential already existed for a service,
   the tool returned the existing one and skipped external registration.
2. **`resolve_credential_env`**: picked the **first** (oldest) credential for the
   service, so script agents always got the same secret.
3. **`runtime.lock`** only declared `credentials: [{ service: "moltbook" }]` — no
   way to specify *which* credential to use.
4. **`agent_spawn`** had no mechanism to bind credentials to child agents.

## 2. Changes Made

### 2.1 Credential Labels (`CredentialRecord.label`)

Added optional `label: Option<String>` to `CredentialRecord`:

```rust
pub struct CredentialRecord {
    // ... existing fields ...
    pub label: Option<String>,
}
```

- SQLite migration v40: `ALTER TABLE credentials ADD COLUMN label TEXT DEFAULT NULL`
- Multiple credentials for the same service are distinguished by label
- `credential_setup` dedup is now scoped to `(service, label)` — calling with
  `label: "agent-b"` only returns an existing credential if one with that exact
  label exists; calling without a label only matches unlabeled credentials

### 2.2 Runtime Lock Credential ID (`LockedCredentialMount.credential_id`)

Added optional `credential_id` to `LockedCredentialMount`:

```rust
pub struct LockedCredentialMount {
    pub service: String,
    pub credential_id: Option<String>,  // pin specific credential
}
```

Example runtime.lock:
```yaml
credentials:
  - service: "moltbook"
    credential_id: "cred_moltbook_a1b2c3"
```

### 2.3 Credential Resolution (`resolve_credential_env_with_bindings`)

New function `resolve_credential_env_with_bindings` accepts spawn-time bindings:

1. Merge `spawn_bindings` with `lock.credentials` (spawn bindings override for
   matching services)
2. For each entry: if `credential_id` is set → resolve by ID directly
3. Fallback: resolve by service name → filter by `inject_as` → first match

### 2.4 Spawn-Time Credential Binding (`agent_spawn.credential_bindings`)

`agent_spawn` accepts `credential_bindings`:

```json
{
  "tool": "agent_spawn",
  "agent_id": "moltbook.default",
  "message": { "task": "send message" },
  "credential_bindings": [
    { "service": "moltbook", "credential_id": "cred_moltbook_a1b2c3" }
  ]
}
```

Flow: `agent_spawn` → `QueuedTaskRun.credential_bindings` →
`spawn_task_execution` → `spawn_agent_once` →
`resolve_credential_env_with_bindings`

## 3. Files Changed

| File | Change |
|------|--------|
| `autonoetic-types/src/agent.rs` | Added `label` field to `CredentialRecord` |
| `autonoetic-types/src/runtime_lock.rs` | Added `credential_id` to `LockedCredentialMount` |
| `autonoetic-types/src/workflow.rs` | Added `credential_bindings` to `QueuedTaskRun` |
| `autonoetic-gateway/src/runtime/tools/credential.rs` | `credential_setup` accepts `label`, dedup scoped to (service, label) |
| `autonoetic-gateway/src/runtime/tools/agent.rs` | `agent_spawn` accepts `credential_bindings` |
| `autonoetic-gateway/src/runtime/script_execute.rs` | New `resolve_credential_env_with_bindings` with ID resolution + spawn override |
| `autonoetic-gateway/src/runtime/install_contract.rs` | `LockedCredentialMount` construction includes `credential_id: None` |
| `autonoetic-gateway/src/execution.rs` | `spawn_agent_once` passes bindings to credential resolution |
| `autonoetic-gateway/src/scheduler.rs` | Wires `credential_bindings` through queued task → execution |
| `autonoetic-gateway/src/scheduler/gateway_store/credentials.rs` | Reads/writes `label` column |
| `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs` | Migration v40: `label` column |
| `autonoetic-gateway/src/scheduler/approval.rs` | `CredentialRecord` construction includes `label: None` |
| `autonoetic/src/cli/agent.rs` | `CredentialRecord` construction includes `label: None` |

## 4. Usage

### Two agents with separate moltbook identities

```json
// 1. Register first agent
{ "tool": "credential_setup", "service": "moltbook", "label": "agent-a", "steps": [...] }
// → credential_id: "cred_moltbook_abc123"

// 2. Register second agent
{ "tool": "credential_setup", "service": "moltbook", "label": "agent-b", "steps": [...] }
// → credential_id: "cred_moltbook_def456" (new credential, not deduped)

// 3. Spawn agent A with its credential
{ "tool": "agent_spawn", "agent_id": "moltbook.default",
  "credential_bindings": [
    { "service": "moltbook", "credential_id": "cred_moltbook_abc123" }
  ] }
```

### Pin credential in runtime.lock

For agents that always use the same credential:
```yaml
credentials:
  - service: "moltbook"
    credential_id: "cred_moltbook_abc123"
```

## 5. Security

- Credential selection uses `credential_id`, never secret values. LLM context
  never sees secrets.
- `allowed_hosts` on `CredentialRecord` continues to prevent exfiltration.
- Labels are metadata only — they do not affect access control.
