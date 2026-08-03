# Phase 4.3 / 4.4 RFC — Declarative Remote-Access Patterns

## Goal

Move remote-access and package-manager detection policy out of hard-coded gateway
lists into agent-declared manifest patterns, while keeping fail-shut behavior.

This addresses constitutional Phase 4 items:

- 4.3 remote-access static analyzer
- 4.4 package-manager command redirection (same policy family)

## Problem

Today `remote_access.rs` contains hard-coded pattern lists (`requests`, `urllib`,
`pip install`, `npm install`, etc.). That makes the gateway the policy author.
It also causes drift between what agents actually declare and what the gateway
infers.

## Proposed Manifest Surface

Add a declaration under agent manifest metadata:

```yaml
remote_access:
  approval_mode: required   # required | preapproved
  targets:
    - kind: any
    - kind: host_suffix
      value: "*.example.com"
    - kind: exact_host
      value: "api.github.com"
    - kind: host_and_port
      value:
        host: "registry.npmjs.org"
        port: 443
    - kind: url_prefix
      value: "https://api.github.com/public/"
  enabled_languages: [python, javascript]
  python_imports:
    - requests
    - urllib
  js_imports:
    - axios
  rust_imports:
    - reqwest
  go_imports:
    - net/http
  function_calls:
    - requests.get
    - axios.get
    - reqwest::get
    - http.Get
  shell_commands:
    - curl
    - wget
    - git clone
  package_manager_commands:
    - pip install
    - npm install
```

## Enforcement Model

Two-pass, fail-shut:

1. **Observed signals**: analyzer extracts import/function/url/ip/command signals
   from code and command text, plus **network sinks** (#1021) — calls resolved
   through the code's own import bindings to the closed stdlib/builtin primitive
   set (`socket`, `http.client`, `urllib.request`, `imaplib`, … / `net`, `tls`,
   `http(s)`, `dgram`, …). Sinks are structural: an unlisted or brand-new client
   library is detected because it bottoms out on a primitive, with no code change.
   See `docs/network-sink-detection.md`.
2. **Declared coverage check**:
   - if a signal maps to a declared pattern category and is covered -> allowed
   - if a signal is not covered by declared patterns -> `undeclared_remote_pattern`
     deny (structured tool error)
  - concrete URL/IP targets must match `remote_access.targets` (typed target rules)
  - the `network_sink` category is **detection-only**: it raises the approval gate
    but is not part of the declared-coverage check, so sinks add nothing an agent
    must enumerate (what must be declared is #1023's decision)
3. **Language scoping** (`enabled_languages`) — applies to import detectors,
   language-tagged function-call heuristics, *and* sink resolution:
   - if `enabled_languages` is empty -> all registered import detectors run, and
     the function-call scope is inferred from the code's own import signals
     (no signal -> language unknown -> every tagged pattern runs)
   - if set -> only those import detectors run (pluggable modular selection), and
     only those languages' call-pattern tags fire
   - language-agnostic call patterns (socket primitives, `.get(`/`.post(` with an
     `http` argument, `connect(...ws[s]://`) always run regardless of scope
4. **Approval policy**:
   - `approval_mode: required` => normal approval flow
   - `approval_mode: preapproved` => auto-approval only when the agent also has
     coarse `NetworkAccess` capability (otherwise fail-shut)

Special case:

- Literal URL/IP signals remain concrete targets for approval and grant scope.
  Declared patterns do not bypass approval checks.
- A shared resolver now evaluates the same declaration target rules for
  `sandbox.exec`, `web_search`/`web_fetch`/`web_call`, and credential HTTP flows.

## Compatibility and Migration

Phase migration to avoid breaking all agents at once:

1. **Declaration-gated enforcement (implemented)**:
   - if `remote_access` is declared and a signal is undeclared => fail-shut deny
2. **Default fail-shut for undeclared signals (implemented)**:
   - if remote-access signals are observed and `remote_access` is absent,
     `sandbox.exec` fails shut with `missing_remote_access_declaration`.
3. **Host + approval policy declarations (implemented)**:
   - `remote_access.targets` are enforced for concrete URL/IP signals.
   - `remote_access.approval_mode` is enforced with capability intersection.
   - migrated manifests include `packager.default`, `researcher.default`,
     `registration.default`, and `executor.default`.
4. **Cross-tool resolver adoption (implemented)**:
   - a shared network-policy resolver now runs in all outbound network tool paths.
   - `sandbox.exec`, `web.*`, and credential HTTP flows all fail shut with
     `missing_remote_access_declaration` when declaration is absent.

## Required Code Changes

- `autonoetic-types/src/agent.rs`
  - add `RemoteAccessDeclaration` policy fields
    (`approval_mode`, `targets`)
- `autonoetic-gateway/src/runtime/network_policy.rs`
  - shared declaration loader + typed target matcher
- `autonoetic-gateway/src/runtime/remote_access.rs`
  - add declared-pattern matcher and broaden signal extraction for Python, JS/TS,
    Rust, and Go network idioms
- `autonoetic-gateway/src/runtime/tools/sandbox.rs`
  - load target manifest declaration from `SKILL.md` frontmatter
  - return structured undeclared-pattern errors
- `autonoetic-gateway/src/runtime/tools/web.rs`
- `autonoetic-gateway/src/runtime/tools/credential.rs`
  - enforce shared target resolver for outbound network calls

- `agents/specialists/packager.default/SKILL.md`
- `agents/specialists/researcher.default/SKILL.md`
- `agents/specialists/registration.default/SKILL.md`
- `agents/specialists/executor.default/SKILL.md`
  - declare remote-access patterns for fail-shut enforcement

## Test Plan

Add `autonoetic-gateway/tests/constitution_dumb_gateway_declared_patterns.rs`:

- declared pattern + observed usage -> pass to approval flow
- observed undeclared pattern -> fail-shut structured deny
- observed concrete target outside declaration target rules -> fail-shut structured deny
- package-manager undeclared command -> fail-shut
- concrete URL literal still produces concrete host coverage for approval scope
- cross-tool parity: same declaration + same host => same allow/deny in
  `sandbox.exec`, `web.*`, and credential HTTP checks

## Security Notes

- No fallback to gateway-internal LLM interpretation.
- No permissive default values for missing declaration in enforce mode.
- Pattern matching should be exact/prefix-bounded and deterministic.
