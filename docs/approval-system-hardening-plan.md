# Approval System Hardening Plan

**Status:** Draft — 2026-04-24
**Scope:** Harden the Gateway approval system (see `docs/approval-system.md`) against integrity, scoping, and operator-fatigue risks identified in the 2026-04-24 review.
**Design goals (operator-facing):**
1. **Obvious** — every approval shows exactly what was requested, by whom, against what scope, and how it relates to prior approvals.
2. **Non-repetitive** — past operator decisions are re-used whenever their scope safely covers a new request.
3. **Strict** — re-use never silently widens scope; integrity is cryptographically enforced; audit is complete.

This plan decomposes the work into four phases. Each phase lists the concrete changes, the files touched, the tests to add, and the acceptance criteria. A tracking issue on GitHub mirrors this doc; per-workstream sub-issues point back to the relevant section here.

---

## Background

The current approval system is described in `docs/approval-system.md`. The review on 2026-04-24 identified the following gaps (ranked by impact):

| # | Risk | Severity | Code anchor |
|---|---|---|---|
| 1 | TOCTOU between approval decision and checkpoint resume — checkpoint file on disk is not cryptographically bound to the approved action | **Critical** | `autonoetic-gateway/src/runtime/checkpoint.rs` (save/load), `autonoetic-gateway/src/runtime/tools/sandbox.rs:395` (`validate_approval_ref_context`) |
| 2 | Session-fork grant leakage — grants keyed only on `root_session_id`, so all children/siblings share them | **High** | `autonoetic-gateway/src/scheduler/gateway_store/approvals.rs:259` (`session_grants_cover_targets`), `migrate.rs:634` (schema v4) |
| 3 | Host-only matching — no subdomain/port/path scoping. `api.github.com` vs. `api-v2.github.com` re-prompts; grant for a host covers every path | **High** | `autonoetic-gateway/src/scheduler/gateway_store/approvals.rs:259`, `autonoetic-gateway/src/runtime/approved_exec_cache.rs` (`normalize_targets`) |
| 4 | No approval scope/duration — binary yes/no only; no "this session only" or "for 10 minutes" | **High** | `autonoetic-types/src/background.rs` (`ApprovalRequest`), approval CLI |
| 5 | No similarity/diff UX — repeated near-identical intents produce independent approvals; approval fatigue | **Medium** | CLI `approvals list/show`, `autonoetic-gateway/src/scheduler/approval.rs` |
| 6 | No operator-side revocation — only emergency-stop can revoke a granted host | **Medium** | CLI, `approvals.rs` grant CRUD |
| 7 | Checkpoint orphan files on withdraw/reject — disk leak | **Medium** | `autonoetic-gateway/src/scheduler/approval.rs` (reject/cancel paths), `checkpoint.rs` reaper |
| 8 | No analytics / rate visibility — no way to spot approval spam from a single agent | **Medium** | CLI, `approvals.rs` queries |
| 9 | Doc gaps vs. code behaviour (exact-match, root-scoping, no signing) | **Low** | `docs/separation-of-powers.md`, `docs/approval-system.md` |

---

## Phase 1 — Integrity (Critical)

**Goal:** close the TOCTOU window between operator decision and execution.

### 1.1 HMAC-sign continuation files

**Problem.** When an approval is pending, the gateway writes a signed `SessionCheckpoint` to `.gateway/checkpoints/<session_id>/<turn_id>.checkpoint.json` (see `autonoetic-gateway/src/runtime/checkpoint.rs`). On resume, the gateway loads the checkpoint and executes the action stored in the `PendingToolState`. The checkpoint is HMAC-SHA256 signed, so local filesystem tampering is detected before execution. `validate_approval_ref_context` (`sandbox.rs:395`) checks agent/session identity and must still re-verify that the pending action matches the approved action.

**Change.**
- Signed `SessionCheckpoint` files are already HMAC-SHA256 signed with a per-gateway key derived from `GatewayConfig::continuation_key`.
- On `load_checkpoint`, verify HMAC before returning. On mismatch: refuse resume, log a `background.checkpoint_tampered` causal event, cancel the approval with reason `integrity_violation`, and surface an operator-visible alert.
- In `validate_approval_ref_context` (or a new adjacent check), additionally assert that the checkpoint's stored `ScheduledAction` equals the `ScheduledAction` from the approval row retrieved via `get_approval(approval_ref)`. Use structural equality on the `ScheduledAction` enum, not string-diff on the command.

**Files.**
- `autonoetic-gateway/src/runtime/checkpoint.rs` — HMAC-signed `SessionCheckpoint` save/load.
- `autonoetic-gateway/src/runtime/tools/sandbox.rs` — action-equality check at resume.
- `autonoetic-gateway/src/config.rs` (or equivalent) — key source.
- `autonoetic-gateway/src/causal_chain.rs` — new `background.checkpoint_tampered` event kind if not generic.

**Tests.**
- Unit: tamper payload after save → load returns `Err(IntegrityError)`.
- Unit: action mismatch between approval row and continuation → resume refused.
- Integration: extend `turn_continuation_approval_integration.rs` with a tamper case that edits the checkpoint file between approve and resume.

**Acceptance.**
- [ ] HMAC verification failures cannot execute the action.
- [ ] Action-mismatch failures cannot execute the action.
- [ ] Tamper produces a causal event and cancels the approval.
- [ ] Existing tests still pass on unmodified continuations.

**Out of scope.** Key rotation protocol, threshold signing, disk-encryption of continuation payloads.

---

## Phase 2 — Scope model (High)

**Goal:** make re-use precise — grants re-used only within the scope the operator actually approved.

### 2.1 Session-scoped grants (instead of root-scoped)

**Problem.** `session_approval_grants` currently uniques on `(root_session_id, agent_id, host)` (`migrate.rs:644`). All children and siblings under a root session inherit grants. This is convenient for cooperating specialists on the same workflow but wrong when a fork is untrusted.

**Change.** Introduce grant `scope` as an explicit field:

```
grant_scope ∈ { Session, RootSession }
```

- Schema migration v8: add `session_id TEXT NOT NULL`, `scope TEXT NOT NULL DEFAULT 'root_session'` (default preserves current behaviour during migration); new unique index `(session_id, agent_id, host)` for `scope='session'` rows and existing `(root_session_id, ..., host)` for `scope='root_session'`.
- `insert_session_grant` gains a `scope: GrantScope` argument. When the operator approves via CLI, the default remains `RootSession` (backward-compatible). New CLI flag `--scope session` narrows to the specific child session.
- `session_grants_cover_targets` is replaced by `grants_cover_targets(session_id, root_session_id, targets)` that unions the two scopes' matches.

**Files.**
- `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs` — migration v8.
- `autonoetic-gateway/src/scheduler/gateway_store/approvals.rs` — CRUD + lookup.
- `autonoetic-gateway/src/scheduler/approval.rs` — read grant scope from the approval decision.
- `autonoetic/src/commands/gateway/approvals.rs` (CLI) — `--scope session|root` on `approve`.
- `autonoetic-types/src/background.rs` — `GrantScope` enum.

**Tests.**
- Two child sessions under the same root: scope=`Session` grant on child A does not cover a request from child B. Scope=`RootSession` does.
- Migration test: existing grants remain effective post-upgrade (default `RootSession`).

**Acceptance.**
- [ ] Grants recorded with explicit scope.
- [ ] CLI allows narrowing.
- [ ] Default behaviour unchanged unless operator opts in.
- [ ] Emergency stop still cleans up both scopes.

### 2.2 Pattern-based target matching

**Problem.** `session_grants_cover_targets` does exact string match on hosts only (`approvals.rs:259`). No subdomain, port, or path discrimination. `normalize_targets` in `approved_exec_cache.rs` strips schemes and paths before comparison. Two side-effects:

- *Fatigue:* `api.github.com` and `api-v2.github.com` both re-prompt even though the operator clearly approves a class of targets.
- *Scope creep:* a grant for `api.github.com` implicitly covers `api.github.com/admin` just as much as `api.github.com/public`.

**Change.** Introduce a typed target model:

```rust
enum GrantTarget {
    ExactHost(String),          // "api.github.com"
    HostSuffix(String),         // "*.github.com" — matches any subdomain of github.com
    HostAndPort(String, u16),   // "api.github.com:443"
    UrlPrefix(String),          // "https://api.github.com/public/"
}
```

- Store targets in a new `session_approval_grant_targets` table (1..N per grant row) so a single approval can produce multiple targets with distinct kinds.
- `normalize_targets` extends to preserve port + path for matching (still lowercase host).
- Matching logic: a request target is covered if *every* extracted normalized target has at least one matching grant target.
- CLI `approvals show` lists the grant targets explicitly, one per line, with their kind.

**Files.**
- `autonoetic-types/src/background.rs` — `GrantTarget`.
- `autonoetic-gateway/src/runtime/approved_exec_cache.rs` — richer normalization.
- `autonoetic-gateway/src/scheduler/gateway_store/approvals.rs` — targets table + matcher.
- `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs` — migration v9.
- CLI `approve` gains optional `--target` flags to narrow the grant before recording.

**Tests.**
- Exact-host matches exact-host, rejects subdomain unless suffix is used.
- Host-suffix `*.github.com` matches `api.github.com` but not `github.com.evil.example`.
- UrlPrefix matches `/public/x` but not `/admin`.
- Default (no `--target`) continues to record the detected host set as `ExactHost` each — behaviour-preserving for existing flows.

**Acceptance.**
- [ ] Grants can record and match all four kinds.
- [ ] CLI can narrow a grant at approval time.
- [ ] `approvals show` displays the kind of each target.
- [ ] Matching is rejected when any required target is not covered.

### 2.3 Approval expiry and duration

**Problem.** Grants are immortal within a session. Operators cannot approve "for the next 10 minutes" to cover a short-running task without paying for long-lived trust.

**Change.**
- Add `expires_at: Option<DateTime<Utc>>` to `session_approval_grants`.
- CLI `approve --ttl 10m` / `--ttl 1h` / `--until 2026-04-24T18:00:00Z`.
- Matcher filters out expired grants. A periodic janitor (reuse the existing retention-policy job) removes them.
- Expired grants still appear in `approvals list --include-expired` for audit.

**Files.**
- `migrate.rs` — add column in migration v10.
- `approvals.rs` — matcher filter + janitor call-site.
- CLI — flags + formatting.

**Tests.**
- Grant with `ttl=60s` covers an immediate request, does not cover a request after mock-clock advance.
- Expired grants are not leaked into responses.

**Acceptance.**
- [ ] Expiry respected by matcher.
- [ ] Janitor removes expired rows.
- [ ] CLI flags documented.

---

## Phase 3 — Operator experience (Medium)

**Goal:** reduce fatigue without silently widening trust.

### 3.1 Approval similarity & diff

> **Removed (#565).** The similarity score was write-only: nothing consumed
> `similar_to_request_id` / `similarity_score` for sandbox-exec approvals.
> Only wiki proposals used the computation for an advisory warning, and that
> small Jaccard check is now inlined in `human_gate.rs`. The dedicated module
> `approval_similarity.rs` and the `approvals` table columns were deleted.

- ~~On approval creation, compute a similarity score against the N most recent approvals for the same agent (same root session and globally).~~
- ~~Similarity signal surface: in `approvals list`, annotate with `~apr-xxxx (92%)` when a near-match exists. In `approvals show`, print a unified diff of command/target differences and summarize recent decisions on similar approvals ("3 rejected, 1 approved in last hour").~~
- ~~Similarity is defined over the `ScheduledAction` structural fields, not raw text, so whitespace/comments do not defeat it.~~

**Files.**
- ~~New `autonoetic-gateway/src/scheduler/approval_similarity.rs`.~~
- ~~`approval.rs` — wire into creation path to record a `similar_to` column.~~
- ~~CLI — render diff.~~

**Acceptance.**
- [x] Removed: module deleted, columns dropped, no remaining references.

### 3.2 Operator revocation

- New CLI command: `gateway approvals revoke --session <id> [--host <host>] [--all]`. Deletes matching grants, logs a causal event, preserves the approval audit row.
- Optional `--reason "<text>"` stored for audit.

**Files.**
- CLI.
- `approvals.rs` — revoke helpers.
- `causal_chain.rs` — `background.grant_revoked` event.

**Acceptance.**
- [ ] Operator can revoke without emergency-stop.
- [ ] Audit event recorded.

### 3.3 Checkpoint cleanup on reject/withdraw

- When an approval is rejected or withdrawn, mark the bound checkpoint for reaping.
- Add a startup-time reaper that removes checkpoint files whose referenced approval is in a terminal state (rejected/cancelled) or whose approval row is missing.

**Files.**
- `approval.rs` — reject/withdraw paths.
- `checkpoint.rs` — reaper utility.
- Gateway startup — call reaper.

**Acceptance.**
- [ ] No orphan checkpoint files after reject/withdraw in tests.
- [ ] Reaper clears pre-existing orphans on startup.

### 3.4 Approval analytics

- `gateway approvals stats [--agent <id>] [--since 1h] [--session <id>]` — shows counts per agent, per host, approved vs. rejected, top hosts, rate trend.
- Backing query on the existing `approvals` table; no schema change required.

**Files.**
- CLI.
- `approvals.rs` — aggregation queries.

**Acceptance.**
- [ ] Operator has a single command to spot unusual approval rates.

---

## Phase 4 — Documentation (Low but required)

Update the following to reflect the new behaviour and the threat model:

- `docs/approval-system.md` — new sections for grant scope, target kinds, expiry, similarity, revocation. Replace "host-only, root-scoped" statements with the new, more precise model.
- `docs/separation-of-powers.md` — document the integrity guarantees (HMAC'd checkpoints, action-equality check) and the threat model explicitly (in-scope: malicious agent; out-of-scope: local-filesystem attacker with vault key).
- `docs/remote-access-approval.md` — update examples to show target-kind usage.
- `docs/config-reference.md` — new config knobs (continuation key, retention for grants, default grant scope).

---

## Rollout & ordering

The four phases are intentionally ordered so earlier phases do not depend on later ones, but every phase improves safety:

1. **Phase 1** ships first because it closes an integrity hole that exists today.
2. **Phase 2** ships as a single coherent PR — schema, matcher, and CLI move together. The default grant scope remains `RootSession` to avoid behaviour changes for existing users.
3. **Phase 3** is independent and can ship incrementally; 3.3 is a straight bug fix and can land first.
4. **Phase 4** accompanies each preceding PR; no separate rollout.

## Non-goals

- Cross-operator approval delegation / multi-sig.
- Remote approval transport (beyond existing HTTP API).
- Approval batching / workflow-level "approve all" — deferred; would compose with similarity (3.1) but needs its own design.
- Full ABAC / policy-as-code — this plan extends the current RBAC+grants model.

## Tracking issues

The work is tracked on GitHub under an umbrella issue. Each phase item above has a corresponding sub-issue that links back to the section here for detail. Sub-issues are the authoritative source for status; this doc is the design reference.
