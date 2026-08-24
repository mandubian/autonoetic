# Plan: Secret-safe service registration from remote skill.md

## Context

A first demo session showed the system can execute the Moltbook registration flow
end-to-end, but `web_call` returns API secrets to the LLM context and the agent stores
them in `knowledge_store` in plain text.

Three goals:
1. Secrets from external API responses must never reach the LLM.
2. Pointing autonoetic at a remote `skill.md` URL should be enough to complete registration.
3. The gateway stays **generic and dumb** — it executes specs, not service-specific logic.

## Implementation status (2026-04-14)

### Done
- Vault auto-init and default vault path are implemented and wired into `credential_setup` and `credential_request`.
- `CredentialSetupStep::UserInput` is implemented in shared types.
- `credential_setup_state` persistence table + GatewayStore CRUD are implemented.
- `credential_setup` supports:
  - `skill_url` ingestion from remote `skill.md`
  - `resume_vars` for multi-step resume after `user_ask`
  - template substitution in URL/headers/body/question
  - secret extraction to vault with public-data extraction to tool response
  - suspension return contract (`suspended_for_user_input`, `question`, `var_name`, `credential_id`)
- Setup state cleanup on completion is implemented.
- Setup state cleanup on credential deletion is implemented in `GatewayStore::delete_credential`.
- Demo agent instructions in `examples/moltbook_registration/sample_agent/SKILL.md` now use generic `credential_setup(skill_url)` + `user_ask` + resume loop.

### Done with caveat
- `autonoetic-gateway/src/bin/mock_moltbook_skill.md` is intentionally kept as the external/raw demo skill format.
- A converted reference version exists in `autonoetic-gateway/src/bin/mock_moltbook_skill_autonoetic.md`.

### Verification
- `cargo build -p autonoetic-gateway` passes.
- `cargo test -p autonoetic-gateway` passes.

### Key findings from code audit

**Existing infrastructure that we reuse (no changes needed):**
- `user_ask` tool — creates `UserInteraction`, returns `{ interaction_required: true, interaction_id }`.
- Execution engine (`lifecycle.rs:2002-2019`) — detects `interaction_required: true` in **any** tool result (tool-name-agnostic), checkpoints with `YieldReason::UserInputRequired`, returns `TurnOutcome::SuspendedUserInput`.
- Resume logic (`execution.rs:2833-2879`) — restores checkpoint and injects the user's answer when a `UserInteraction` is answered. **Note:** `execution.rs:2813` hardcodes `tc.name != "user_ask"` — resume only works for `user_ask` tool calls. This is fine because the agent calls `user_ask` directly (not `credential_setup`).
- `CredentialSetupStep::ApiCall` — executes HTTP calls server-side, extracts secrets to vault, returns only public data.
- `Vault::load_from_file` — already returns empty vault for non-existent files (line 85-87).
- `GatewayStore::upsert_credential` — creates/updates credential records.
- `credential_request` — authenticated HTTP with vault-stored Bearer injection.
- `gray_matter` — already a dependency for YAML frontmatter parsing.
- `extract_json_path` — already in `credential.rs` for `$.field` path parsing.

**What the plan does NOT change:**
- The execution engine / turn continuation system.
- The `user_ask` / `user_interaction_status` tools.
- The `UserInteraction` / `UserInteractionAnswer` types.

---

## Design: `credential_setup` drives, agent calls `user_ask`

The core idea: `credential_setup` executes onboarding steps server-side. When it
hits a `user_input` step it **returns early** to the LLM with the question. The LLM
then calls `user_ask` (existing tool, existing suspension infrastructure), collects
the answer, and calls `credential_setup` again with the collected `vars` to resume.

No execution engine changes. No new interaction-creation code. The gateway stays dumb.

### Flow walkthrough (Moltbook example)

```
1. Agent: credential_setup(skill_url: "http://localhost:8765/skill.md")
   Gateway: fetches skill.md → parses onboarding → executes steps 0-1:
     step 0: api_call POST /register-agent
       → vault stores secret, public_data gets agent_id
     step 1: user_input "Enter your X username:"
       → STOP. Save state. Return to LLM:
         { suspended_for_user_input: true, credential_id: "cred_moltbook_abc",
           question: "Enter your X username:", var_name: "x_username",
           public_data: { agent_id: "moltbook_agent_xyz" } }

2. Agent: user_ask(question: "Enter your X username:")
   Execution engine: suspends turn, checkpoints, waits.
   User answers: "@handle"
   Execution engine: resumes turn with answer.

3. Agent: credential_setup(credential_id: "cred_moltbook_abc", resume_vars: { x_username: "@handle" })
   Gateway: loads saved state → resumes from step 2:
     step 2: api_call POST /human-claim
       headers: { Authorization: "Bearer {{secrets.moltbook_secret}}" }  ← resolved from vault
       body: { human_x_username: "{{vars.x_username}}" }                 ← "@handle"
       → public_data gets tweet_text
     step 3: user_input "Post this tweet: {{public.tweet_text}} ..."
       → STOP. Save state. Return to LLM.

4. Agent: user_ask(question: "Post this tweet: ... Paste the URL:")
   User answers: "https://x.com/..."

5. Agent: credential_setup(credential_id: "cred_moltbook_abc", resume_vars: { tweet_url: "https://x.com/..." })
   Gateway: resumes from step 4:
     step 4: api_call POST /verify-human-claim
     step 5: api_call POST /setup-heartbeat
   → All done. Return:
     { ok: true, credential_id: "cred_moltbook_abc", public_data: { agent_id, tweet_text }, secrets_stored: 1 }
```

**The LLM never sees `sk_molt_...`.** It only sees: `credential_id`, `agent_id`, `tweet_text`,
user input values. All HTTP calls with the secret happen gateway-side via template substitution.

---

## Changes (in implementation order)

### 1. Vault auto-init (`autonoetic-gateway/src/vault.rs`)

Two new public functions:

```rust
pub fn ensure_default_key(agents_dir: &Path) -> anyhow::Result<()>
pub fn default_vault_path(agents_dir: &Path) -> PathBuf
```

`ensure_default_key`: if neither `AUTONOETIC_VAULT_KEY` nor `AUTONOETIC_VAULT_KEY_PATH` is
set, generates a random 32-byte key → hex-encodes → writes to
`{runtime_dir}/vault.key` → sets `AUTONOETIC_VAULT_KEY_PATH` in env.

`default_vault_path`: returns `{runtime_dir}/vault.enc.json`.

### 2. Vault auto-init in `credential_setup` and `credential_request` (`tools/credential.rs`)

Replace the hard-fail vault block in both tools:

```rust
// Before:
let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
    .ok().map(PathBuf::from)
    .ok_or_else(|| anyhow::anyhow!("AUTONOETIC_VAULT_PATH must be set"))?;

// After:
crate::vault::ensure_default_key(_agent_dir)?;
let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
    .ok().map(PathBuf::from)
    .unwrap_or_else(|| crate::vault::default_vault_path(_agent_dir));
```

### 3. `UserInput` variant on `CredentialSetupStep` (`autonoetic-types/src/agent.rs`)

```rust
pub enum CredentialSetupStep {
    ApiCall { ... },      // existing
    UserPrompt { ... },   // existing (operator secrets via approval channel)
    UserAction { ... },   // existing (display-only marker)
    UserInput {           // NEW: tells credential_setup to pause and ask the agent to collect input
        question: String,
        var_name: String,
    },
}
```

This variant is just a marker — `credential_setup` returns early when it encounters it.
No `UserInteraction` record creation inside `credential_setup`.

### 4. State persistence for multi-step resume (`gateway_store`)

New table `credential_setup_state`:
```sql
CREATE TABLE IF NOT EXISTS credential_setup_state (
    credential_id TEXT PRIMARY KEY,
    state_json TEXT NOT NULL,     -- { steps, current_step, vars, public_data, service, inject_as, ... }
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
)
```

CRUD functions on `GatewayStore`:
- `save_credential_setup_state(credential_id, state_json)`
- `load_credential_setup_state(credential_id) -> Option<state_json>`
- `delete_credential_setup_state(credential_id)`

Dropped when `credential_setup` completes or the credential is deleted.

### 5. `credential_setup` extended (`tools/credential.rs`)

**New parameters:**

```rust
struct CredentialSetupArgs {
    // existing fields...
    skill_url: Option<String>,     // fetch and parse onboarding from remote skill.md
    resume_vars: Option<HashMap<String, String>>,  // user-collected vars for resuming
}
```

**`skill_url` handling** (when set):
1. Policy-check URL host against agent's `NetworkAccess`.
2. `reqwest::blocking::get(url)` to fetch the skill.md.
3. Parse frontmatter with `gray_matter`.
4. Extract `autonoetic.onboarding.steps` → convert to `Vec<CredentialSetupStep>`.
5. Extract `autonoetic.credential.{service, inject_as, allowed_hosts}`.
6. Set `base_url` from `autonoetic.base_url` for resolving relative step URLs.
7. Proceed with step execution.

**Template substitution** (before each `ApiCall` step):
Resolve `{{...}}` in `body` values, `headers` values, and `question` strings:
- `{{vars.name}}` → from accumulated vars map
- `{{public.name}}` → from accumulated public_data map
- `{{agent.id}}` / `{{agent.model}}` → from agent manifest
- `{{secrets.name}}` → fetched from vault server-side, **never returned to LLM**

Unresolved templates are left unresolved (not hard fail).

**Execution loop:**
```
for each step starting from current_step:
  match step:
    ApiCall → resolve templates, execute HTTP, extract secrets/public
    UserInput → save state, return { suspended_for_user_input, question, var_name }
    UserPrompt → existing behavior (operator approval)
    UserAction → existing behavior (no-op marker)
```

**Resume handling** (when `credential_id` is set and `resume_vars` is provided):
1. Load state from `credential_setup_state`.
2. Merge `resume_vars` into `vars`.
3. Advance `current_step` past the `UserInput` step.
4. Continue execution loop.

Make `extract_json_path` `pub(crate)` so `web.rs` can also reuse it if needed later.

### 6. Structured onboarding reference in converted file

Keep `autonoetic-gateway/src/bin/mock_moltbook_skill.md` as the external/raw demo skill.
Place the machine-readable converted version in:
`autonoetic-gateway/src/bin/mock_moltbook_skill_autonoetic.md`.

Converted format:

```yaml
---
name: Moltbook Agent Skill
autonoetic:
  version: "1.0"
  base_url: "http://localhost:8765"
  credential:
    service: moltbook
    inject_as: bearer
    allowed_hosts: [localhost]
  onboarding:
    steps:
      - type: api_call
        url: /api/register-agent
        method: POST
        body: { name: "{{agent.id}}", model: "{{agent.model}}" }
        extract_secrets: { moltbook_secret: "$.secret" }
        extract_public: { agent_id: "$.agent_id" }
      - type: user_input
        question: "Enter your X/Twitter username (e.g. @handle):"
        var: x_username
      - type: api_call
        url: /api/human-claim
        method: POST
        headers: { Authorization: "Bearer {{secrets.moltbook_secret}}" }
        body: { human_x_username: "{{vars.x_username}}" }
        extract_public: { tweet_text: "$.verification_tweet_text" }
      - type: user_input
        question: "Post this tweet:\n\n{{public.tweet_text}}\n\nPaste the tweet URL:"
        var: tweet_url
      - type: api_call
        url: /api/verify-human-claim
        method: POST
        headers: { Authorization: "Bearer {{secrets.moltbook_secret}}" }
        body: { tweet_url: "{{vars.tweet_url}}" }
      - type: api_call
        url: /api/setup-heartbeat
        method: POST
        headers: { Authorization: "Bearer {{secrets.moltbook_secret}}" }
        body: { prompt_id: heartbeat, interval_hours: 24 }
  operations:
    - name: post-to-feed
      url: /api/post-to-feed
      method: POST
      auth: bearer
      body_schema: { content: string }
---
```

### 7. Example SKILL.md update

`examples/moltbook_registration/sample_agent/SKILL.md` becomes generic:

```
Call credential_setup with skill_url: "http://localhost:8765/skill.md".

When it returns suspended_for_user_input, call user_ask with the question it gives you.
When the user answers, call credential_setup again with credential_id and resume_vars
containing the user's answer mapped to the var_name.

Repeat until credential_setup returns ok: true. Then use credential_request with the
returned credential_id for any further API calls.
```

No Moltbook-specific steps in the agent prompt.

---

## What was removed from the previous plan (superfluous)

1. **`store_secret_fields` on `web_call`** — Not needed for the primary path
   (`credential_setup` handles secret extraction server-side). A nice generic primitive
   but not required for the skill.md ingestion goal. Can be added later independently.
2. **Creating `UserInteraction` records inside `credential_setup`** — Not needed.
   The agent calls `user_ask` itself, reusing the existing suspension infrastructure.
3. **Execution engine changes** — Not needed. The `user_ask` name check at
   `execution.rs:2813` is not a problem because the agent calls `user_ask` directly.

---

## Future evolution: agents creating tools

The `onboarding:` format is the bridge between today and the future:
- Today: a human writes the skill.md spec, the gateway executes it.
- Tomorrow: an agent reads a service's docs, writes the `onboarding:` spec, and
  calls `credential_setup(skill_url)`.
- Further: an agent identifies a missing `CredentialSetupStep` variant (e.g., OAuth2 PKCE),
  writes the Rust implementation, compiles it via `sandbox_exec`, and proposes a gateway
  revision through the `AgentRevision` system.

The gateway tool registry and the step type enum are the extension points that make this
possible without architectural changes.

---

## Critical files

| File | Change |
|---|---|
| `autonoetic-gateway/src/vault.rs` | `ensure_default_key` + `default_vault_path` |
| `autonoetic-types/src/agent.rs` | `CredentialSetupStep::UserInput { question, var_name }` |
| `autonoetic-gateway/src/scheduler/gateway_store/` | `credential_setup_state` table + CRUD |
| `autonoetic-gateway/src/runtime/tools/credential.rs` | vault auto-init; `skill_url` + `resume_vars` params; `UserInput` step returns early; template substitution in ApiCall bodies/headers; `extract_json_path` → `pub(crate)` |
| `autonoetic-gateway/src/bin/mock_moltbook_skill.md` | External/raw demo skill (kept unchanged) |
| `autonoetic-gateway/src/bin/mock_moltbook_skill_autonoetic.md` | Converted structured `autonoetic:` frontmatter reference |
| `examples/moltbook_registration/sample_agent/SKILL.md` | Generic `credential_setup(skill_url)` workflow |

## Verification

1. `cargo build -p autonoetic-gateway` — no errors
2. `cargo test -p autonoetic-gateway` — existing tests pass
3. `bash examples/moltbook_registration/run.sh` (no vault env vars)
   - Gateway auto-creates `runtime/vault.key` and `runtime/vault.enc.json`
   - Agent calls `credential_setup(skill_url)` → suspends → `user_ask` → resumes twice
   - Session log: no `sk_molt_...` secret ever appears in LLM messages
   - `curl http://localhost:8765/status` → agent verified
4. `credential_request` works with returned `credential_id` for post-to-feed calls
