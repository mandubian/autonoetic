# RFC: Credential Egress — `allowed_hosts` as a Routing Input, Not a Bypass

**Status:** Implemented. §3 core on
`feat/credential-egress-host-approval` — routing change in
`credential.rs`, host-naming R++4 phrase in `approval_hardening.rs`,
tests in `tests/credential/credential.rs`. §4.2 card surfacing completed
on `feat/credential-prompt-card-hosts` — scope on the prompt card, the
TUI secret-entry panel, `gateway approvals list`, and the pending summary
(see #1105). §4.1 resolved in #1106 (documented `any` + fail-shut guard
for the dangerous combination). §4.3 resolved in #1110 (manifest field
round-trip + diagnosability).
Proposed out of the classic-harness validation study (credential-register
case); the study run completed only because `executor.default` happens to
declare `remote_access.targets: [{kind: "any"}]`; every other installed
agent is locked out of `credential_request`.

**Related:** `autonoetic-gateway/src/runtime/tools/credential.rs`
(`credential_request`), `autonoetic-gateway/src/runtime/network_policy.rs`
(`enforce_remote_target_policy`), `autonoetic-gateway/src/scheduler/
approval_hardening.rs` (R++4 confirm phrases),
`autonoetic-gateway/src/runtime/session_timeline.rs`
(`approval_timeline_extra_from_action` — the `grant_hosts` card),
`docs/guide/remote-access-approval.md`, `docs/reference/credentials.md`.

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

1. **`executor.default`'s `targets: [{kind: "any"}]`** — resolved in #1106:
   kept `any` with a documented rationale (general-purpose role; an
   enumeration would fail shut on the first undeclared host and recreate
   the "pipeline for one GET" failure this RFC criticizes), and hardened
   the shape that made `any` dangerous: `any` + `preapproved` without a
   wildcard NetworkAccess capability is now a fail-shut manifest
   inconsistency (`remote_any_preapproval_requires_wildcard_capability`)
   across sandbox_exec / artifact_exec / web / credential paths, with a
   shipped-roster contract test and a pin that executor keeps
   `approval_mode: required`. Other agents' `targets: []` stay as-is
   (they become meaningful again: pre-committed scope).
2. **Surface `allowed_hosts` on the CredentialPrompt card summary** (not
   buried in payload) so secret entry and egress scope are approved
   *knowingly*, even though scope alone never authorizes egress.
3. **Declaration-load discrepancy** — root-caused and fixed (#1110):
   `AgentManifest` had no `remote_access` field, so `create_from_intent`'s
   canonical SKILL.md (rendered from the struct) silently DROPPED the
   block — factory-built agents like the study's credential_onboarding
   were installed without the declaration their source shipped, and the
   denial correctly read "missing" because the INSTALLED copy had none.
   The field now round-trips parse → render; create_from_intent inherits
   it from the artifact SKILL.md, skill_install keeps the fetched one.
   The loader additionally warns (instead of silently swallowing) on
   unreadable SKILL.md / unparsable frontmatter / deserialize failure,
   and pins distinguish empty-targets (`undeclared_remote_target`) from
   missing/malformed.

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
