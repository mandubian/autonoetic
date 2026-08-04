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
| `targets` | list of `GrantTarget` | **Gating.** Outbound host rules the declaration covers — the authoritative contract |
| `enabled_languages` | list of `python`\|`javascript`\|`rust`\|`go` | Restrict analyzer detection to these languages — both import detectors and language-tagged function-call heuristics (empty = all; see below) |
| `shell_commands` | list of strings | **Gating.** Shell commands expected (e.g. `curl`, `wget`) |
| `package_manager_commands` | list of strings | **Gating.** Package-manager commands (e.g. `pip install`) |
| `python_imports` | list of strings | *Advisory.* Python modules expected (e.g. `imaplib`, `requests`) |
| `js_imports` / `rust_imports` / `go_imports` | list of strings | *Advisory.* Same, per language |
| `function_calls` | list of strings | *Advisory.* Call-pattern prefixes expected (e.g. `"requests.get("`) |

**There is no `hosts` or `patterns` field.** Declaring network hosts belongs in
`targets`.

### What you must declare (and what you needn't)

Only the **gating** fields above can fail an exec shut with
`undeclared_remote_pattern`:

- a concrete URL or IP in the code must be covered by `targets`;
- a network shell command must be named in `shell_commands`;
- installing packages requires `package_manager_commands` to be non-empty.

Each of those is a statement of intent you can write from *what you are trying to
do*. `targets` is the durable contract — declare the hosts.

The **advisory** fields (`python_imports`, `js_imports`, `rust_imports`,
`go_imports`, `function_calls`) are hints. They sharpen the approval prompt and
make declaration drift visible, but an import or call the declaration doesn't name
will **not** refuse the exec (#1023). They used to gate, which forced agents to
mirror the analyzer's internal pattern strings — a contract agents could not keep.
The evidence: in session-912c7791 the coder declared
`function_calls: ["imaplib.fetch("]` while the analyzer detects the bare `fetch(`,
so the declaration could never match — after ~30 turns spent guessing at the
schema. Since the gateway now resolves network **sinks** structurally
(`docs/network-sink-detection.md`), asking the agent to re-declare what the gateway
already derives was pure friction.

Demoting them removes no protection, because the declaration was never the network
boundary. Between agent code and the network there remain: the `NetworkAccess`
capability ceiling (with install-time `detected_network_hosts` coverage, P-1.5),
`targets` gating concrete hosts, operator approval at the gate — which shows the
advisory signals too — and the per-exec grant from #1022, without which the sandbox
has no network namespace at all (`docs/sandbox-network-grant.md`).

### `enabled_languages` — what it scopes

`enabled_languages` is the analyzer's language allowlist. It scopes **both**
halves of static detection:

- **Import detectors** — only the listed languages' detectors run.
- **Function-call heuristics** — call patterns are language-tagged, and only the
  listed languages' tags fire. So `axios.*(`, `(http|https).(get|request)(`,
  `net.connect(`, `WebSocket(` and the JS global `fetch(` are JavaScript-only;
  `urlopen(`, `requests.*(`, `httpx.*(` are Python-only; `(reqwest|ureq)::*(`
  and `TcpStream::connect(` are Rust-only; `http.(Get|Post|Head)(` and `.Do(`
  are Go-only.

Language-**agnostic** call patterns always run regardless of the allowlist:
socket primitives (`.connect(`, `.send(`, `.recv(`, `.bind(`, `.listen(`,
`.accept(`), `.get(`/`.post(` with an `http` argument, and `connect(…ws://` /
`wss://`.

When `enabled_languages` is empty there is no allowlist: every import detector
runs, and the function-call scope is inferred from the code's own import signals
(which language's imports fired). With no import signal at all the language is
unknown and **every** tagged pattern runs — detection is never silently lost.

Practical effect: declaring `enabled_languages: ["python"]` stops JS-shaped
heuristics from firing on Python source, which is what keeps `mail.fetch(b"1",
"(RFC822)")` (imaplib) from being reported as a "Fetch API call" (#1020).

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
