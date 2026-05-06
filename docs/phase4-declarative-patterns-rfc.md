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

Add a new optional declaration under agent manifest metadata:

```yaml
remote_access:
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
   from code and command text.
2. **Declared coverage check**:
   - if a signal maps to a declared pattern category and is covered -> allowed
   - if a signal is not covered by declared patterns -> `undeclared_remote_pattern`
     deny (structured tool error)
3. **Detector selection**:
   - if `enabled_languages` is empty -> all registered import detectors run
   - if set -> only those detectors run (pluggable modular selection)

Special case:

- Literal URL/IP signals remain concrete targets for approval and grant scope.
  Declared patterns do not bypass approval checks.

## Compatibility and Migration

Phase migration to avoid breaking all agents at once:

1. **Current state (implemented)**: declaration-gated enforcement.
   - if `remote_access` is declared and a signal is undeclared => fail-shut deny
   - if `remote_access` is absent => legacy behavior (approval flow only)
2. **Next stage**: move selected agents to mandatory declaration.
3. **Final stage**: default fail-shut for undeclared signals (constitutional target).

## Required Code Changes

- `autonoetic-types/src/agent.rs`
  - add `RemoteAccessDeclaration` manifest type
- `autonoetic-gateway/src/runtime/remote_access.rs`
  - add declared-pattern matcher and broaden signal extraction for Python, JS/TS,
    Rust, and Go network idioms
- `autonoetic-gateway/src/runtime/tools/sandbox.rs`
  - load target manifest declaration from `SKILL.md` frontmatter
  - return structured undeclared-pattern errors

- `agents/specialists/packager.default/SKILL.md`
- `agents/specialists/researcher.default/SKILL.md`
  - declare remote-access patterns for fail-shut enforcement

## Test Plan

Add `autonoetic-gateway/tests/constitution_dumb_gateway_declared_patterns.rs`:

- declared pattern + observed usage -> pass to approval flow
- observed undeclared pattern -> fail-shut structured deny
- package-manager undeclared command -> fail-shut
- concrete URL literal still produces concrete host coverage for approval scope

## Security Notes

- No fallback to gateway-internal LLM interpretation.
- No permissive default values for missing declaration in enforce mode.
- Pattern matching should be exact/prefix-bounded and deterministic.
