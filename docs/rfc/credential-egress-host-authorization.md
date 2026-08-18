# RFC: Credential Egress — `allowed_hosts` as a Routing Input, Not a Bypass

**Status:** Draft — 2026-08-17. Proposed out of the classic-harness
validation study (credential-register case); no implementation yet.

**Origin:** `docs/rfc/classic-harness-usecase-validation.md` §3.5 finding 4
—the credential-register smoke completed only because `executor.default`
happens to declare `remote_access.targets: [{kind: "any"}]`; every other
installed agent is locked out of `credential_request`.

**Related:** `autonoetic-gateway/src/runtime/tools/credential.rs`
(`credential_request`), `autonoetic-gateway/src/runtime/network_policy.rs`
(`enforce_remote_target_policy`), `autonoetic-gateway/src/scheduler/
approval_hardening.rs` (R++4 confirm phrases),
`autonoetic-gateway/src/runtime/session_timeline.rs`
(`approval_timeline_extra_from_action` — the `grant_hosts` card),
`docs/remote-access-approval.md`, `docs/credential-management.md`.

---

## 1. Problem

A registered credential carries an operator-facing host scope —
`allowed_hosts`, set at `credential_setup` time and stored on the
credential row. But when the credential is *used*, that scope plays no
part in the remote-target policy: `credential_request` hard-requires the
**calling agent's** static `metadata.autonoetic.remote_access.targets`
declaration (`DeclarationRequirement::Required` +
`CapabilityHostCheck::Enforce`, credential.rs). Two disconnected host
authorities govern one request, and neither knows about the other.

Consequences, all observed in the study run:

1. **Most agents are locked out.** `credential_onboarding.default` — the
   agent that *created* the credential, with
   `allowed_hosts: ["127.0.0.1"]` it just wrote — cannot call
   `credential_request`: its `targets: []` fails the policy with a hard
   error before any operator gate. The planner's own routing table's
   lawful next move is an agent-factory rebuild to widen a static
   declaration: a full pipeline to make one GET.
2. **For the one agent that isn't locked out, the static layer adds
   nothing.** `executor.default` declares `targets: [{kind: "any"}]`, so
   the declaration check passes for *every* host and the effective control
   is the runtime host approval — the model this RFC proposes to make
   explicit and uniform.
3. **Rubber-stamp surface.** The CredentialPrompt approval card (secret
   entry) is where `allowed_hosts` is operator-approved today, but the
   hosts sit in the payload, not on the card summary ("Prompt asks for:
   api_key"). An operator approving secret entry is not consciously
   approving an egress scope.

## 2. Threat model — why `allowed_hosts` must not silently satisfy the policy

The gateway injects the secret into the request. "Which hosts may this
credential be used against" is therefore literally "which hosts may
receive the secret."

The two lists are different trust objects:

| | `remote_access.targets` | credential `allowed_hosts` |
|---|---|---|
| Authored | static, in the immutable revision (SKILL.md) | runtime tool argument, authored by the requesting agent |
| Reviewed | promotion gates (evaluator/auditor evidence, P-2.25 capability delta) **before first run** | the CredentialPrompt gate — a card about secret entry |
| Widened by | new revision through the gates only | any `credential_setup` call + one approval |

**Rejected: auto-satisfy.** If a host in `allowed_hosts` bypassed the
remote-target policy, a prompt-injected agent registers a credential with
`allowed_hosts: ["evil.example.com"]`; the operator rubber-stamps a card
that reads *"Prompt asks for: api_key"*; the vault delivers the secret to
the attacker. The static layer exists precisely so that runtime-approved
values cannot silently widen egress scope.

**Also rejected: status quo.** A tool that only one agent can call (and
that one only because it pre-committed to `any`) is dead code for the
rest, and "rebuild the agent to add a host" fails any proportionality
test. The absurdity check in the study exists for exactly this shape.

## 3. Proposal — route, don't bypass

Reuse the pattern the codebase already has twice: web tools'
`DeferToCaller` (the `host_allowed` check feeds GateService approval
minting instead of hard-erroring — the #579 regression behind #933) and
sandbox's `grant_hosts` approval card
(`approval_timeline_extra_from_action`, session_timeline.rs).

For `credential_request`:

1. **Keep the credential-scope check first** (already enforced): the URL
   host must be in the credential's `allowed_hosts` — non-covered hosts
   are denied outright, no gate.
2. **Replace the hard declaration error with an approval mint** when the
   calling agent's static declaration does not cover the host: create a
   normal host approval (`ScheduledAction::CredentialRequest` already
   exists) with the host **on the card**: "vault credential
   `<credential_id>` (service `<service>`) will be sent to `<host>`".
   Approval creates a session-scoped, revocable grant through the existing
   session-grant machinery (`gateway grants revoke`); the approval flood
   cap applies as usual.
3. **R++4 phrase names the host.** The confirmation phrase for this
   approval class becomes e.g. `use <service> credential at <host>` —
   today the host appears nowhere the operator must retype.
4. **The static layer stays authoritative for code.** `sandbox_exec` /
   `artifact_exec` remote-access analysis and the SKILL.md declaration
   regime are unchanged: pre-committed scope remains the model for
   *programmatic* egress. This RFC only concerns the one-shot,
   per-credential API call surface.

Security invariant preserved: **no host receives a vault secret without
an explicit, host-named operator decision** — either the pre-committed
static declaration or a runtime approval whose card and confirm phrase
name that exact host.

## 4. Follow-ups (out of scope here, recorded so they are not lost)

1. **Tighten `executor.default`'s `targets: [{kind: "any"}]`** once this
   RFC lands — under the new model the runtime approval carries the
   security, and `any` stops being the only way to make the tool usable.
   Other agents' `targets: []` stay as-is (they become meaningful again:
   pre-committed scope).
2. **Surface `allowed_hosts` on the CredentialPrompt card summary** (not
   buried in payload) so secret entry and egress scope are approved
   *knowingly*, even though scope alone never authorizes egress.
3. **Investigate the declaration-load discrepancy** from the study run:
   `credential_onboarding.default` failed with
   `missing_remote_access_declaration` despite shipping a (empty-targets)
   `remote_access` block — the block either failed to deserialize or was
   read from a different directory. Expected shape would be
   `undeclared_remote_target`.

## 5. Alternatives considered

| Alternative | Why rejected |
|---|---|
| `allowed_hosts` auto-satisfies the policy | Rubber-stamp exfiltration (§2) |
| Status quo + rebuild flow for new hosts | Proportionality; dead tool for most agents |
| Operator config-level allowlist per service | Another host authority to keep coherent; no per-decision accountability; the approval system already does scoped, revocable, auditable |

## 6. Implementation sketch

- `credential.rs` (`credential_request`, the
  `enforce_remote_target_policy` call at ~L486): on
  `missing_remote_access_declaration` / `undeclared_remote_target`, fall
  into the existing `GateService` arm (the credential_request network gate
  at ~L690 already exists and already minted the approval the study's
  passing run used) instead of returning the policy error — mirroring the
  web tools' `DeferToCaller` conversion.
- `approval_hardening.rs`: extend the `CredentialRequest` phrase class to
  include the host.
- `session_timeline.rs` / card rendering: host + credential id in the
  summary line (the `grant_hosts` pattern).
- Tests: unit tests for phrase construction and card payload; an
  integration test asserting (a) non-covered host → hard denial, (b)
  covered host + no static declaration → approval minted, not error,
  (c) approval → request executes with injected secret, (d) revoked
  grant → next request re-gates.
