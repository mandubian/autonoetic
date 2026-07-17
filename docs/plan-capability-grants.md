# Plan Capability Grants

**Status:** Shipped (PR #499, June 2026)

## What It Is

When the operator approves a plan, the gateway mechanically materializes the
plan's declared network envelope into a **session approval grant**. Subsequent
tool calls (`sandbox_exec`, `web_fetch`, `artifact_exec`, `artifact_prepare`,
credential URL gating) that target hosts within that envelope are auto-approved
against the plan grant — the operator approves the envelope once, not each tool
call.

This adds a **5th dedup layer** ahead of explicit session grants:

| # | Layer | Scope | Cross-session? |
|---|-------|-------|----------------|
| 1 | **Exec cache** (fingerprint) | Identical command + targets | Yes |
| 2 | **Plan grants** (this feature) | Plan-scoped envelope | No (per-root-session) |
| 3 | **Session grants** (explicit operator) | Root-session or session-scoped | No |
| 4 | **Existing approved/pending approvals** | Domain-level match | No |
| 5 | **Approval flood cap** | Pending-request ceiling | No |

A plan approval **is** a capability budget. Tool calls spend against it
silently. Re-approve only on budget expansion.

## How It Works

### Materialization (on plan approval)

When `planframe_approve` succeeds, the gateway calls `materialize_plan_grants()`:

1. For each step in the plan, resolve `step.agent_id` to the installed agent's
   manifest via `AgentRepository`.
2. Collect all hosts declared in each agent's `NetworkAccess` capability.
   - Wildcards (`"*"`) are **skipped** — they don't materialize to a concrete,
     matchable grant and would defeat the dedup's concreteness rule.
   - Empty/whitespace `agent_id` is treated as unset (matching the amend
     step-merging logic — the LLM may emit a placeholder).
   - Not-yet-installed agents are silently skipped; their tool calls go through
     normal approval.
3. Insert a single `RootSession`-scoped grant row with one `ExactHost` target
   per unique host, carrying `source_approval_id = plan_id`.
4. The grant is **best-effort**: any failure (missing config, DB error, agent
   not found) silently returns `grants_materialized: 0` — the approval still
   succeeds. The plan is the authorization; the grant is an optimization.

### Revocation (on envelope-expanding amend)

When `planframe_amend` creates a new revision whose envelope changed, the
gateway calls `revoke_session_grants_by_source()`:

- Revokes **only** grants whose `source_approval_id` matches the plan ID.
- Revokes **only** when both conditions hold:
  1. The prior revision was `Approved` (there are materialized grants to revoke).
  2. The diff `requires_regate()` (the envelope expanded — new/removed step,
     owner/agent change, weakened/removed validation).
- Cosmetic amends (objective rewording, title change, progress reason) and
  envelope-equivalent amends keep existing grants intact.
- The response surfaces `grants_revoked: 0|1` (binary — the prior revision's
  envelope was a single grant row).

### What happens to grants on other events

| Event | Grant lifecycle |
|-------|-----------------|
| Plan approved | Grant materialized (`grants_materialized: 0|1`) |
| Plan amended (envelope expanded) | Prior grant revoked (`grants_revoked: 0|1`); re-approval re-materializes |
| Plan amended (cosmetic) | Existing grants kept intact |
| Emergency stop | All grants for the root session revoked (existing behaviour) |
| Session end | All grants cleaned up (existing behaviour) |
| Grant expiry TTL | Expired via existing janitor (existing behaviour) |

## Mechanical, Never LLM-Judged

The entire grant lifecycle is **deterministic gateway computation**:

- The envelope is derived from each plan step's `agent_id` → declared
  `Capability::NetworkAccess.hosts` — never from an LLM-declared intent or
  natural-language field.
- The diff that triggers revocation is `PlanEnvelopeDiff`, computed
  structurally over step sets, owners, agents, and validation gates — never
  from an LLM summary.
- Wildcards are skipped rather than expanded because an LLM-expanded wildcard
  would be a trust decision, not a mechanical one.

This means the grant system is **conservative**: if the design is wrong (host
missed, agent not installed, wildcard skipped), it errs on the side of more
approval prompts, never fewer.

## CLI Visibility

Plan-scoped grants use the same `session_approval_grants` table as explicit
operator grants, distinguished by `source_approval_id`. They appear in standard
grant queries:

```bash
# List all grants for a root session (includes plan grants)
autonoetic gateway grants list --root-session <id>

# Revoke a specific plan grant by its grant ID
autonoetic gateway grants revoke --grant-id <grant_id>

# Revoke all grants for a host (includes plan grants matching that host)
autonoetic gateway grants revoke --root-session <id> --host api.example.com
```

There is no separate `--plan-scoped` flag; plan grants share the same CRUD
surface. They are distinguished in `grants list` output by `source_approval_id`.

## Design Notes

### Why not a separate table?

Plan grants reuse the `session_approval_grants` + `session_approval_grant_targets`
tables rather than introducing a new schema. This means:

- The existing dedup matcher (`grants_cover_targets`) covers plan grants without
  modification — it sees them as ordinary grants.
- Revocation reuses `revoke_session_grants_by_source`, a surgical helper that
  targets only grants with a matching `source_approval_id`. This avoids the
  blunt `revoke_session_grants` that deletes by host only.
- Expiry, scope, and target-kind patterns (`ExactHost`, `HostSuffix`, etc.)
  all work natively.

### Why RootSession scope?

Plan grants are always `RootSession`-scoped because a plan is a workflow-level
contract — all agents participating in the workflow (across child sessions)
should benefit from the same envelope. This matches the intent that a single
plan approval replaces N separate tool-level approval prompts.

### Why binary return (0/1)?

The entire approved envelope is materialized as a **single grant row** with
multiple `ExactHost` targets. This is why both `grants_materialized` and
`grants_revoked` return 0 or 1 (not a host count): the unit is the grant row,
not the host. The host count is an internal detail.

### What about agents without NetworkAccess?

If a plan step references an agent that declares no `NetworkAccess` capability,
it contributes no hosts to the grant. The step's tool calls that require network
access will be rejected at the capability layer (policy check in `policy.rs`),
not at the grant layer — the plan grant is irrelevant because the tool never
reaches the dedup chain.

## Related

- `docs/design/operator-legibility.md` — design rationale (§6 Pillar C)
- `docs/wiki/approval-system.md` — the broader approval dedup system
- `docs/remote-access-approval.md` — static analysis for remote access detection
- `autonoetic-gateway/src/runtime/tools/plan_frame.rs` — `materialize_plan_grants()`, revoke wiring
- `autonoetic-gateway/src/scheduler/gateway_store/approvals.rs` — `revoke_session_grants_by_source()`
