# RFC: Sandbox host-filesystem mount allow-set (#1002)

Date: 2026-08-24
Status: Draft — decision points flagged in §7
Refs: #1002, #1001 (closed), #988 (closed), RFC data-envelopes §11, #903

---

## 0. Problem

Every bubblewrap exec starts with `--ro-bind / /` (`sandbox/driver/bubblewrap.rs::base_argv`).
A sandboxed process can therefore read any host path readable by the gateway user:
`~/.ssh`, browser profiles, other projects, operator dotfiles — regardless of the
agent's declared capabilities. The deny-list mask (#1145/#1150, `runtime_dir` secrets)
shades exactly the paths someone remembered to list. This is a **confinement** defect
before it is an egress one.

Egress is the secondary benefit: today's label-plane triggers rest on static analysis
of command strings (`EgressPathMatcher`), which RFC §11 concedes is defeatable by
indirection. If labeled paths are *mounted* rather than assumed-visible, "this exec
could see a labeled path" becomes a fact the gateway asserted when it built the
sandbox — enforcement, not inference. That is what makes #1001's workspace trigger
sound instead of best-effort.

## 1. Non-goals

- Per-path write-side taint (rejected in #988's decision: evadable, fake precision).
- Revoking the writable workspace bind: the workspace is the exec's working surface.
- Changing what `credential_env` injects (vault protection is orthogonal and intact).
- A new capability type before the manifest surface proves necessary (§3, DP-2).

## 2. Allow-set composition (bubblewrap)

An exec's visible filesystem becomes exactly:

| # | Mount | Source of truth |
|---|---|---|
| 1 | The agent workspace (rw, as today) | `agent_dir` → `BWRAP_WORKSPACE_DIR` |
| 2 | Runtime toolchain roots (ro) | resolved interpreter + its sysroot (`/usr`, `/lib`, `/bin`, `/etc/alternatives`, …) — enumerated once in the driver, not per-exec |
| 3 | Pinned dependency layers (ro) | `layer_store.rs` layer manifests (already modeled) |
| 4 | SDK source tree (ro) | `resolve_python_sdk_path()` (PYTHONPATH target) |
| 5 | Session content + artifact mounts | existing `SandboxMount` flow (`load_session_content_mounts`, runtime.lock mounts) — unchanged |
| 6 | SDK bridge socket | existing `wire_sdk_bridge` mount — unchanged |
| 7 | **Declared custom mounts** (§3) | manifest + operator allowlist |
| 8 | Minimal `/dev`, `/proc/self/fd`, `/tmp` scratch | existing dev-mode machinery; tmpfs for scratch |

Everything not in the set is simply absent (ENOENT-ish) — with the legibility carve-out
in §5.

**Toolchain honesty:** the enumeration in #2 is the load-bearing boring part. Linux
Python needs `/usr`, `/lib*`, `/etc/ld.so.cache`, sometimes `/opt`, plus whatever the
pinned interpreter path implies. The set is seeded from a driver const list plus
*derived* roots (parent dirs of the resolved interpreter), and covered by an
integration test that runs a real exec in allow-set mode and asserts `python3 -c`
works and `ls ~/.ssh` fails.

## 3. Declared custom mounts — "the `allowed_hosts` of filesystem reach"

Manifest (`metadata.autonoetic.runtime`):

```yaml
runtime:
  sandbox: bubblewrap
  mounts:                      # requested reach, ro unless declared rw
    - host_path: ~/mail
      readonly: true
```

Requests are granted only against the operator's **config allowlist** — mirroring
`NetworkAccess.allowed_hosts`:

```yaml
sandbox:
  host_fs: allow_set           # legacy | allow_set  (see §4)
  allowed_mount_roots:         # operator-granted reach; manifest requests are
    - ~/mail                   # intersected with this list
    - /var/data/datasets
```

Intersection semantics: a manifest mount is mounted iff its canonicalized host path
is equal to or under an allowed root. A requested-but-not-allowed mount is a
**structured refusal naming the missing grant**, not a silent drop (§5).

## 4. The flip: `host_fs: legacy | allow_set`

- `legacy` (default): today's `--ro-bind / /` plus the #1150 secret mask. Zero
  behavior change on upgrade — the deprecation window.
- `allow_set`: composition from §2. Emit a startup warning in legacy mode once
  #1002 ships, pointing at this RFC.

**DP-1 (maintainer):** window length and whether `allow_set` becomes the default
before or at launch. Recommendation: not before launch — this is a Tier-0-hardening
track that ships behind the flag, with `legacy` removal as its own follow-up PR once
the fleet has run `allow_set` in CI nightly for a cycle.

## 5. Error legibility — denials that teach

A missing path under `allow_set` must not surface as bare ENOENT. Two layers:

1. **Pre-exec mount check:** if the command's statically-detected path operands
   (best-effort) reference a host path outside the set, the exec result carries a
   `mount_denied` block: the path, the nearest allowed root, and the
   `available_actions` affordances (declare in manifest; ask operator to extend
   `allowed_mount_roots`; run in `legacy` mode) — same shape as capability denials
   (`denial_affordances.rs`).
2. **In-sandbox ENOENT hint:** the bwrap wrapper script (we already compose
   entrypoints) prepends a `PATH_HINT` env var listing the scratch dir and workspace;
   docs tell the agent unexplained ENOENT means unmounted reach, and the affordance
   above is how to ask.

The static operand scan is explicitly *advisory* (same honesty as #1023: hosts
declarations durable, function lists advisory). The enforcement is the mount set
itself; the scan only improves the error, never the guarantee.

## 6. Undeclared reach is a decision, not a success

Mirroring `share_net` approvals: when a manifest requests a mount not covered by the
allowlist, the gateway mints an approval (`MountRequest` payload: canonical path, ro/rw,
agent_id, session) instead of silently dropping it. Operator approves → the path is
added to the session's mount grants (session-scoped, expiring like approval grants —
same TTL machinery as `session_approval_grants`); manifest edit is the durable path.

**DP-2 (maintainer):** approval-only vs also a `MountAccess` capability with
`allowed_roots` (NetworkAccess-shape). Recommendation: start approval + config
allowlist; add the capability only if the approval volume gets noisy in practice.

## 7. Tier matrix

| Tier | Behavior |
|---|---|
| bubblewrap | Full §2 composition. The reference implementation. |
| docker | Already namespaced (no `/` bind). Maps: declared mounts → `-v ro` volumes; allow-set mode simply means *no* extra host binds beyond today's. Confinement parity is inherited, not engineered. |
| microvm | Rootfs is already an image; host paths were never visible. Declared mounts → virtiofs/9p mounts. Parity inherited. |
| wasm | No host filesystem at all; `mounts` manifest fields are rejected at parse time for wasm-tier agents (loud, not ignored). |

Rule: **a manifest mount request valid under tier A and silently useless under tier B
must fail loudly under B.** `check_dependency_support` gets a sibling:
`check_mount_support(plan)` in the `SandboxDriver` trait.

## 8. Observability

- The exec causal event and `execution_traces` row gain `mount_set` (the canonical
  host paths mounted, ro/rw flags) — "what could this exec see?" is answerable after
  the fact. Emits in both `legacy` and `allow_set` modes (in legacy it records
  `host_root: ro` plus the explicit mounts — still useful).
- `EgressPathMatcher`'s static path scan gains a second input: the mount set. A
  labeled path **in the mount set** is a mechanical trigger (gateway-asserted fact);
  a labeled path merely *referenced in the command* stays the advisory heuristic.
  This is the #1001 trigger integration.

## 9. Implementation slices (PR-sized, in order)

1. **Observability first (non-breaking):** `mount_set` in exec traces/causal events.
2. **Manifest + config surface (non-breaking):** `runtime.mounts`, `sandbox.allowed_mount_roots`,
   intersection + loud refusal; mounts *added* under `legacy` (additive only).
3. **`SandboxDriver::check_mount_support`** + wasm loud rejection + registry tests.
4. **`host_fs: allow_set`** in bubblewrap: §2 composition, dev/tmp handling,
   integration tests (toolchain works, `~/.ssh` absent), e2e probe test.
5. **Approval path** for undeclared reach (MountRequest → session mount grants).
6. **Nightly `allow_set` job** — the fleet validation that eventually unlocks DP-1.

Slices 1–3 can land before any decision in §7 DPs is final; slice 4 is gated on DP-1.

## 10. Risks

- **Breakage under `allow_set`** is the intended behavior surfacing; the legibility
  work (§5) is what keeps it humane. CI nightly on `allow_set` is the guard.
- **Two modes = drift.** Mitigation: `legacy` mode is a single `base_argv` branch,
  and the guard test asserts the `allow_set` composition contains no `--ro-bind / /`.
- **Docker/microvm parity claims** must be tested per tier (slice 4 tests include
  docker; microvm stays behind its P5 flag).
