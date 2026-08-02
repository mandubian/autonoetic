# Agent SKILL.md frontmatter — schema reference

This is the canonical schema for the YAML frontmatter a `SKILL.md` carries. The
gateway validates this shape at install time (`validate_skill_frontmatter_shape`)
and parses it at runtime. Hand-crafted frontmatter that doesn't match is rejected
**loudly** — it cannot silently become an empty declaration.

Why strictness matters: `RemoteAccessDeclaration` fields that are misspelled or
invented (e.g. `hosts:`/`patterns:` instead of `targets:`/`function_calls:`) used
to be silently dropped by serde, leaving an empty declaration. The agent then
got blocked at runtime with no actionable signal, and agents burned many turns
guessing at the schema. The declaration is now validated at install time with a
precise, self-describing error.

## `capabilities` — list of capability objects

Each entry is an object `{type, …}`, **never** a bare string (bare strings are
rejected with an explicit error). Common shapes:

```yaml
capabilities:
  - type: SandboxFunctions
    allowed: ["content.", "knowledge."]
  - type: CodeExecution
    patterns: ["python3 ", "bash -c "]
    commands: ["curl", "wget"]
  - type: NetworkAccess
    hosts: ["api.example.com"]          # STRING list. "*" requires open_web: true
  - type: CredentialAccess
    services: ["*"]
  - type: ReadAccess
    scopes: ["self.*"]
  - type: WriteAccess
    scopes: ["self.*"]
  - type: ArtifactExecution
```

`NetworkAccess.hosts` is a **list of host strings**. A list of `{host, port}`
maps fails parse (`invalid type: map, expected a string`).

## `remote_access` — declaration block (top-level OR under `metadata.autonoetic`)

`RemoteAccessDeclaration` has exactly these fields and uses
`#[serde(deny_unknown_fields)]` — unknown keys fail install validation:

| Field | Type | Purpose |
|---|---|---|
| `approval_mode` | `"required"` \| `"preapproved"` | `required` = operator approval per request; `preapproved` = auto-approve when `NetworkAccess` covers the host |
| `targets` | list of `GrantTarget` | Outbound host rules the declaration covers |
| `enabled_languages` | list of `python`\|`javascript`\|`rust`\|`go` | Restrict import detectors to these (empty = all) |
| `python_imports` | list of strings | Python modules expected (e.g. `imaplib`, `requests`) |
| `js_imports` / `rust_imports` / `go_imports` | list of strings | Same, per language |
| `function_calls` | list of strings | Call-pattern prefixes expected (e.g. `"imaplib.fetch("`, `"fetch("`) |
| `shell_commands` | list of strings | Shell commands expected (e.g. `curl`, `wget`) |
| `package_manager_commands` | list of strings | Package-manager commands (e.g. `pip install`) |

**There is no `hosts` or `patterns` field.** Declaring network hosts belongs in
`targets`; declaring call patterns belongs in `function_calls`.

### `GrantTarget` shapes (`tag = "kind", content = "value"`)

```yaml
targets:
  - kind: any
  - kind: exact_host
    value: api.example.com
  - kind: host_suffix
    value: "*.github.com"
  - kind: host_and_port
    value: {host: imap.gmail.com, port: 993}
  - kind: url_prefix
    value: "https://api.github.com/public/"
```

## Worked example — IMAP reader agent

```yaml
name: "gmail-imap-reader"
description: "Reads a Gmail mailbox over IMAP and prints a JSON summary."
execution_mode: script
script_entry: "fetch_gmail.py"
script_input_mode: input
capabilities:
  - type: NetworkAccess
    hosts: ["imap.gmail.com"]
remote_access:
  approval_mode: "preapproved"
  targets:
    - kind: host_and_port
      value: {host: imap.gmail.com, port: 993}
  python_imports: ["imaplib"]
credentials:
  - env_var: APP_PASSWORD
    required: true
```

No `function_calls` entries are needed here: the analyzer flags the `import imaplib`
signal (covered by `python_imports`), but it does not flag `imaplib` method calls —
the `fetch(` heuristic only matches the standalone/global (JavaScript) form, so
`mail.fetch(` is not mis-attributed to the Fetch API. Only declare a
`function_calls` entry for a call pattern the analyzer actually detects (e.g.
`requests.get(`, `.connect(`).
