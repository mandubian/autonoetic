# Sandbox network reachability: ceiling vs grant

**Issue:** #1022 (audit) · **Umbrella:** #1025 · **Date:** 2026-08-03

This documents the audit #1022 asked for — *when the remote-access analyzer
detects nothing at all, does a `NetworkAccess`-capable agent still gate, or does
it run with the network namespace shared and no prompt?* — the answer, and the
policy now enforced.

## The question, answered

**The window was open, for `sandbox_exec`.** A network-capable agent running code
that static analysis found no signal in executed with the host network namespace
shared (`bwrap --share-net`) and no operator prompt.

## The chain, as it was

`sandbox_exec` (`autonoetic-gateway/src/runtime/tools/sandbox.rs`) seeded the
isolation baseline from the agent's capabilities and let the gate only *widen*
it:

```rust
let mut overrides = BwrapIsolationOverrides::from_capabilities(&manifest.capabilities);
//                  ^ share_net = true when NetworkAccess { hosts: [..] } is declared
if safe_inspection_bypass || (network_sink_excluded && !network_declassified) {
    overrides.share_net = false;
} else if approval_validated_for_command {
    overrides.share_net = true;
}
```

The gate could therefore never be the thing that *granted* network — it could
only turn on what the capability had already turned on. Whenever neither branch
fired, `share_net` kept the capability's value. Four facts compose into the hole:

1. **The gate is driven by detection, not by capability.**
   `requires_network_gate = remote_analysis.requires_approval || (network_sink_excluded && manifest_grants_share_net)`.
   Zero detected patterns ⇒ `requires_approval == false`.
2. **An untainted session does not exclude the Network sink.**
   `require_boundary_session_taint` returns `EgressLabel::unrestricted()` when a
   session has no recorded taint row, so `network_sink_excluded == false` and the
   second disjunct is false too. `requires_network_gate == false` — no prompt.
   (Pinned by `fresh_session_taint_allows_the_network_sink`.)
3. **`approval_validated_for_command` stays false**, because nothing raised a
   gate to clear. So neither the force-off branch nor the widen branch runs.
4. **The baseline survives**: `share_net == true`, inherited from the ceiling.
   `append_bwrap_isolation_flags` emits `--share-net`.

Net effect:

```text
zero detected signals  →  no gate  →  no prompt
                       →  share_net = true (from the capability)
                       →  bwrap --share-net: the whole host network namespace
```

The contradiction is internal to `sandbox_exec`: this tool's policy is that
network reachability is **operator-gated** — a detected signal must be approved,
and an undeclared pattern fails shut with `undeclared_remote_pattern`. The
zero-signal path bypassed that policy silently, and it bypassed it in the exact
case where the analyzer is weakest.

## Reachability is not theoretical

Static analysis is regex + a curated list. Measured against the analyzer on
`main`, ordinary code that performs real network egress leaves **no signal at
all**:

| snippet | detected? |
|---|---|
| `subprocess.run(["aws","s3","cp","s3://bucket/key","/tmp/key"])` | **no** — no URL literal, no socket primitive, no listed import, no recognised network command |
| `mod = __import__(os.environ["M"]); mod.request("GET", os.environ["U"])` | **no** — dynamic import, host from the environment |
| `subprocess.run(["sh","-c", base64.b64decode(os.environ["P"]).decode()])` | **no** |

The first row matters most: it is a plain CLI invocation, not an evasion attempt.
No obfuscation is required to reach the window — the detector table merely has to
not know about the tool being used.

For contrast, the analyzer *does* catch more than one might assume, via the
language-agnostic socket primitives and URL literals: `sf.connect(account=...)`
(snowflake), `c.bind()` (ldap3), and `Elasticsearch("http://es.internal:9200")`
are all detected even though none of those libraries is on the import list.
Precision is better than the "add a row per library" framing suggests — but
`aws s3 cp` shows it is not a boundary.

## The policy now enforced

**`share_net` is granted per exec, never inherited.** The `NetworkAccess`
capability is a **ceiling** ("may this agent ever reach the network"); an
approval is a **grant** ("does *this* exec reach it"). The rule lives in one
pure function, `autonoetic-gateway/src/runtime/network_grant.rs`:

| condition (first match wins) | `share_net` | reason |
|---|---|---|
| `force_network_off` — `Evaluation` capability / promotion gate | false | `forced_off` |
| `safe_inspection_bypass` | false | `safe_inspection` |
| `network_sink_excluded && !network_declassified` | false | `taint_not_declassified` |
| `approval_validated` | **true** | `granted_by_approval` |
| otherwise | false | `no_grant` |

`capability_allows_network` appears in no row. It is carried only as
`capability_ceiling_unused`, for observability.

`approval_validated` covers every explicit per-exec grant: a validated
`approval_ref`, a gate cleared this turn, a declared
`remote_access.approval_mode: preapproved` bypass, and an approved-exec cache hit
(a prior operator approval of the same fingerprint).

### Why this and not "gate on capability presence"

#1022 offered the alternative: make every network-capable exec ask unless
preapproved. Rejected — it prices a real hole at the cost of a prompt on every
exec by a network-capable agent, including the majority that touch no network.
Granting per exec closes the same window with **zero additional prompts**: the
set of things the operator is asked about is unchanged, and what changes is only
what an *unasked* exec is permitted to reach.

It also moves in #1024's direction. Network becomes something the gateway
*grants* when it builds the sandbox rather than something the agent's capability
*implies*, which is the same shift #1024 needs at per-host granularity.

## What this buys, and what it does not

**Closed:** a missed detection is now a runtime connection refusal instead of
silent namespace-wide reachability. The analyzer no longer stands between
agent-supplied code and the network; it decides *what the operator is asked
about*, which is a precision/UX property — the direction #1025 set out to move
in.

**Still open:** `--share-net` remains all-or-nothing *once granted*. An exec
approved for `api.example.com` can reach any host. Host granularity is still
enforced only by what the analyzer asked about, so **#1024 (per-host runtime
enforcement) remains load-bearing** — it is what makes the granted case as
mechanical as the ungranted one now is.

**Unchanged:** agents without `NetworkAccess` were already fail-closed at the
capability layer (`--unshare-all`, no `--share-net`) and are unaffected. The
egress/taint DLP layer is complementary and untouched: it bounds what data may
leave, not what hosts are reachable.

## Sibling exec paths: capability-driven by design

The audit covered the other paths that build isolation overrides. They are **not
changed**, because for them capability-as-grant is the deliberate policy rather
than an accident:

| path | network policy | assessment |
|---|---|---|
| `sandbox_exec` (`runtime/tools/sandbox.rs`) | operator-gated | **fixed** — the gate is the control, so the ungated case must not reach the network |
| `artifact_exec` (`runtime/tools/artifact_exec.rs`) | capability-driven, explicitly: `agent_has_network_access && requires_approval && !approval_validated` → auto-approve, logged as *"Agent has NetworkAccess capability — auto-approving"* | consistent as-is; the capability *is* the grant on this path, so a fail-closed zero-signal case would be arbitrary rather than principled |
| `artifact_exec` ticket path (`execute_with_ticket`) | already grant-based: `share_net = !ticket.approved_domains.is_empty()` | already correct |
| script mode (`runtime/script_execute.rs`) | capability-driven, no analyzer and no gate | consistent as-is: a fixed entrypoint reviewed once at install, with `revision.detected_network_hosts` covered by the declared `NetworkAccess.hosts` (P-1.5) |
| promotion gate | `promotion_gate_overrides()` → `force_network_off` | already correct (R+16) |
| `sandbox_network: sealed` / `recording` | `setup_sealed_proxy_for_exec` sets `share_net = true` after the decision, so the sandbox can reach the host-loopback fixture proxy (and skips setup under `force_network_off`) | unchanged and deliberate: egress is mediated by the proxy, which serves fixtures or refuses with `unfixtured_target` |

Whether `artifact_exec`'s capability-as-grant policy should itself become
operator-gated is a separate question from this audit; it is a *documented*
policy, not a silent bypass, so it is out of scope here.

`BwrapIsolationOverrides::from_capabilities` keeps its ceiling semantics for the
capability-driven paths, and its doc comment now says so — with a pointer here,
so a future call site does not re-seed a gated path from the ceiling.

## Observability

An exec that runs network-off while holding the capability is logged rather than
silent:

```
target: sandbox_exec
"Agent declares NetworkAccess but this exec has no network grant — running with
 the network namespace unshared"  reason=no_grant  pattern_count=0
```

And when such an exec then fails on a connection error,
`apply_network_isolation_failure_to_result` turns it into an `ok=false` /
`error_type: network_isolated` result whose message names the likely cause and
the fix — make the target visible (a literal URL/host, listed in
`metadata.autonoetic.remote_access.targets`) and retry so the operator can
approve it. This is what keeps the new fail-closed failures diagnosable instead
of looking like a broken sandbox.

## Detector seam (#1039)

Static analysis is a **precision/UX layer** over this grant rule: it decides
*what the operator is asked about*, not whether the namespace opens. Call sites
that need analysis go through [`RemoteAccessDetector`]
(`runtime/remote_access.rs`); [`RemoteAccessAnalyzer`] is the default
mechanical implementation. Category vocabulary is a closed typed enum
([`DetectedPatternCategory`]) with the historical snake_case wire strings, so a
second detector cannot invent labels that silently bypass gating tables.

A future AI (or other) detector may plug in behind that trait — preferably by
**unioning** patterns with the mechanical analyzer. Because missed detection
fail-closes (`share_net=false` without a grant), an additive detector can only
cause *more* gates. It must never set `approval_validated` or `share_net` from
discretionary judgment. Extracting `RemoteAccessGateOutcome` from `sandbox.rs`
and an optional `GrantAdvice` side-channel remain follow-ups on #1039.

## Tests

- `autonoetic-gateway/src/runtime/network_grant.rs` (unit) — the decision matrix,
  including an exhaustive sweep of all 64 input combinations asserting
  `share_net ⇒ a valid grant`.
- `autonoetic-gateway/tests/constitution/sandbox_network_grant_fail_closed.rs` —
  each link of the chain above, then the composed regression pin (network-capable
  agent + untainted session + zero signals ⇒ no `--share-net` in the bwrap argv),
  the approved counterpart, and the ceiling/grant distinction.
