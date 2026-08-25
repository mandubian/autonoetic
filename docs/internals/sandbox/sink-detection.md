# Network sink detection

**Issue:** #1021 · **Umbrella:** #1025 · **Date:** 2026-08-03

How the gateway decides that code reaches the network, and why that question is
answered structurally rather than by a list of library names.

## The problem it replaces

`RemoteAccessAnalyzer`'s import detectors were hand-maintained lists of *library
names* — `requests`, `httpx`, `imaplib`, `boto3`, `psycopg`, … Every new or
newly-popular client was an "add a row" event, and #1019 added
`imaplib`/`poplib`/`nntplib`/`telnetlib` for exactly that reason. A list can
never keep up: grpc, kafka, elasticsearch, ldap3, asyncpg, snowflake, `psycopg2`
vs `psycopg`, generated clients.

It was not merely lagging on third-party packages. The Python list was missing
**`http.client` — Python's own stdlib HTTP client** — so this produced *zero*
signals:

```python
import http.client
conn = http.client.HTTPSConnection(target_host)
conn.request("GET", "/v1/data")
```

## The structural property

The network surface is **closed at the sink layer**. Whatever library you use,
it bottoms out on a small, stable set of platform primitives:

* **Python** — `socket`, `ssl`, `http.client`, `urllib.request`, `ftplib`,
  `imaplib`, `poplib`, `smtplib`, `nntplib`, `telnetlib`, asyncio streams,
  `xmlrpc.client`, `multiprocessing.connection`
* **Node** — `net`, `tls`, `http`, `https`, `http2`, `dgram`, `dns`

Resolve a call to one of those and the originating library's name stops
mattering. So detection now asks *"does this code reach a network primitive?"*
instead of *"does this code name a library we happen to know about?"*

## How resolution works

In `autonoetic-gateway/src/runtime/network_sinks.rs`. Comments and string bodies
are blanked first — replaced by spaces, with newlines preserved so reported line
numbers still match the original — then two passes:

1. **Collect bindings** introduced by imports.
   `import urllib.request as u` → `u → urllib.request`;
   `from http.client import HTTPConnection as HC` → `HC → http.client.HTTPConnection`;
   `const {connect} = require("net")` → `connect → net.connect`.
   Node's `node:` prefix is normalised, so `node:https` and `https` resolve alike.
2. **Resolve call heads** through those bindings into a canonical dotted path,
   then match against the language's sink table.

A call head that no import bound resolves to **nothing**. That is the precision
property name-matching lacks:

| call | resolves to | flagged |
|---|---|---|
| `u.urlopen(dest)` after `import urllib.request as u` | `urllib.request.urlopen` | yes |
| `mail.fetch(b"1", "(RFC822)")` (imaplib instance) | — (`mail` is bound to nothing) | no |
| `data.get("http://…")` (a dict) | — | no |
| `urlopen(...)` with no import at all | — | no |

The `mail.fetch(` false positive from #1019 is therefore impossible *by
construction* here, rather than by the byte-boundary special case that still
guards the name-based `fetch(` heuristic.

### Masking, and why the two passes differ

Sink-shaped *text* is not a sink *call*. Without masking, a comment reading
`# never call socket.socket() here`, or `print("socket.socket(")`, raised a real
`network_sink` signal — an approval gate, and a hard refuse under a taint that
excludes `Sink::Network`. (Found in the #1033 review; fixed with the masker.)

The passes read differently-masked source, which is not an accident:

| pass | comments | string bodies | why |
|---|---|---|---|
| bindings | masked | **kept** | a JS module specifier lives *inside* the quotes — `require("net")`. Blanking it would erase every JS binding and silently disable detection |
| calls | masked | masked | sink-shaped text inside a literal must not read as a call |

Consequences that fall out of this, each pinned by a test: a commented-out import
binds nothing; a `#` or `//` inside a string does not open a comment; template
literal `${…}` interpolations stay unmasked because they hold real code
(`` `${net.connect(80)}` `` is a genuine call); and an unterminated quote recovers
at end of line rather than swallowing the rest of the file.

What masking guarantees is a **line count, not a byte offset**. Each masked
character becomes one space and newlines survive, so the result has the same
character count as the input and its lines align — all a reported line number
needs, since it is counted inside the masked string that was scanned. Byte
offsets do *not* survive on non-ASCII source, where a masked multi-byte character
collapses to a one-byte space; nothing consumes them.

## What it changed, measured

| snippet | before | after |
|---|---|---|
| `import http.client` + `HTTPSConnection(host)` | undetected | `network_sink:http.client.HTTPSConnection` |
| `xmlrpc.client.ServerProxy(endpoint)` | undetected | detected |
| `asyncio.open_connection(h, p)` | undetected | detected |
| `import { request } from "node:https"` | undetected (`node:` matched no import regex) | detected |
| `mail.fetch(...)` | correct (no flag) | unchanged |
| inert local compute | clean | clean |

## Gating: detection only

Sinks are emitted under the **`network_sink`** category, which
`undeclared_patterns_against_manifest` does **not** gate. So a sink raises the
approval gate but adds nothing an agent must enumerate in its declaration — an
agent covering its hosts in `targets` does not additionally have to list every
sink it touches.

That boundary is deliberate. Narrowing (or widening) what must be declared is
**#1023's** decision, and #1023 resolves after this issue. #1021 makes sinks the
primary *detector*; it does not touch the declaration contract.

Two consequences worth knowing, both intended:

* Code that previously produced zero signals may now raise an approval gate. That
  is the point — and after #1022 the alternative was worse: an undetected exec
  got no network grant at all and failed on an opaque connection error. Detection
  turns that into a prompt the operator can approve.
* An agent with **no** `remote_access` block whose code hits a sink now gets
  `missing_remote_access_declaration` instead of running (networkless). That is
  the existing #1019 fail-loudly rule applying to newly-visible signals, not a new
  rule. No in-repo agent is affected.

## Language coverage, and why

Python and JavaScript — the two languages this runtime executes
(`exec_request::CodeLanguage`).

| language | closed sink set? | status |
|---|---|---|
| **Python** | yes | implemented |
| **JavaScript** | mostly — every npm client bottoms out on `net`/`tls`/`http(s)`/`http2`/`dgram`. The exception is the built-in **globals** `fetch`/`WebSocket`/`XMLHttpRequest`, which need no import and so offer no binding to anchor on; those stay name-based, on top of #1020's language scoping and #1019's byte-boundary guard | implemented |
| **Go** | **yes** — `net`, `net/http`, `crypto/tls`. Go also has the strongest import signal of any of these, since an unused import is a *compile error*, so an import proves use | deferred — viable and cheap, but Go is not executable here |
| **Rust** | **no** — no stdlib HTTP, and `tokio::net`/`reqwest`/`hyper` reach the network without touching `std::net`, so no closed stdlib sink set exists | not planned via this mechanism; the declared `Cargo.toml` dependency set is a better signal than parsing source |

The issue's original non-goal said the approach doesn't generalise beyond
Python. That holds for Rust, but not for Go, and only partly for JavaScript.

## Why a Rust resolver rather than a real parser

A `python3 -c` subprocess would give exact Python fidelity for ~15ms per
analysis (measured: 10 spawns in 151ms — negligible beside bwrap plus the
command). It was rejected on two grounds:

* **It cannot generalise.** JavaScript would need `node` on the host, Go a Go
  toolchain, Rust `rustc`. Since JavaScript is executable here, that mechanism
  permanently leaves half the executable surface uncovered.
* **It is not deterministic across hosts.** With a pattern fallback when `python3`
  is absent, the same code gates differently on different machines — against the
  gateway's Lawful Executor property.

The `analysis/` tier (`PythonAstAnalyzer` + `minimal_python_scan.py`) that #1021's
notes pointed at does contain a real `ast` walk, but it is **unwired**: it has no
call sites outside its own module and nothing reads
`code_analysis.capability_provider` to construct a provider. Its `_NET_BASES` is
also just another library-name list. Extending it would have changed no
production behaviour.

## Known limits

Deliberately not a parse. These escape resolution — and escaped the regex table
too, so none is a new gap:

* value aliasing — `s = socket.socket; s()`
* dynamic import — `__import__(name)`, `require(expr)`, `importlib`
* indirection — `getattr(mod, "urlopen")()`
* a sink reached only *inside* a third-party package (nothing in-file to resolve;
  workspace-local modules are covered one hop by `analyze_code_with_workspace`)
* shell-outs — `subprocess.run(["aws","s3","cp",…])` reaches the network with no
  Python sink at all. This is the #1022 evidence case, and it is why detection is
  a precision layer rather than a boundary: the per-exec network grant, not the
  analyzer, is what keeps an undetected exec off the network.
* Node's unanchored globals (`fetch`, `WebSocket`, `XMLHttpRequest`)
* JS regex literals are not masked — they cannot be told from division without
  parsing — so `/net\.connect\(/` after `require("net")` still reads as a call.
  A false positive, not a missed detection.

What is gone is the *library enumeration* treadmill: adding a client library no
longer requires a code change. `PythonImportDetector`'s list should not be
extended for new libraries — its remaining jobs are the coarse "a module was
imported at all" signal and matching the `python_imports` declaration field.

## Tests

* `runtime/network_sinks.rs` — 27 unit tests: alias/`from`-import/namespace/rename
  resolution per language, `node:` normalisation, unbound heads rejected,
  cross-language isolation, dedup; plus the masking set — sink text in
  strings/comments/docstrings/template literals rejected, module specifiers
  surviving masking, `${…}` interpolations still detected, commented-out imports
  binding nothing, comment markers inside strings, line numbers exact (including
  on non-ASCII source, where byte offsets shift) and character count preserved,
  unterminated strings recovering.
* `runtime/remote_access.rs` — 7 analyzer-level tests: the stdlib-sink gap,
  unlisted library via its sink, alias reported in the operator-facing reason,
  `network_sink` not gated as undeclared, `enabled_languages` scoping,
  `node:`-prefixed builtins, and no new false positives on inert code.
