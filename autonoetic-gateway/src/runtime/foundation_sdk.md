# SDK Reference for Sandboxed Code

Included in your foundation when you **execute** script/sandbox code (`CodeExecution`), **delegate** builds (`AgentSpawn`), or **statically review** script artifacts (`architect`, `static_evaluator` roles). Single source of truth for API names — do not invent methods outside this reference. The SDK bridge only supports the methods in the tables below; unsupported calls raise `AutonoeticSdkError`.

Code running inside **`sandbox.exec`** or as an **`execution_mode: script`** entrypoint has the `autonoetic_sdk` package available. Always call `autonoetic_sdk.init()` first — there is no module-level `autonoetic_sdk.memory`.

```python
import autonoetic_sdk
sdk = autonoetic_sdk.init()
invocation = autonoetic_sdk.load_invocation()  # task = invocation.input, metadata = invocation.metadata
```

For the common case where you only need the input payload, `autonoetic_sdk.load_input()` is a shortcut that returns the normalized input directly.

## Memory Operations (`sdk.memory`)

| Method | Signature | Description |
|---|---|---|
| `remember` | `sdk.memory.remember(key: str, value: Any, scope: str = "sdk") -> Any` | Persist a key-value pair. Visibility defaults to **`session`**: any agent in the same root session can read it via `recall` or the native `knowledge_recall`/`knowledge_search` tools. |
| `recall` | `sdk.memory.recall(key: str) -> Any` | Retrieve a stored value by key. `None` if not found or not visible. |
| `read` | `sdk.memory.read(path: str) -> str` | Read file-like content from Tier 1 (private scratch) memory by path. |
| `write` | `sdk.memory.write(path: str, content: str) -> str` | Write file-like content to Tier 1 (private scratch) memory — use for data that must NOT be shared with the session. |
| `list_keys` | `sdk.memory.list_keys() -> list[str]` | List all stored keys in the current scope. |
| `search` | `sdk.memory.search(query: str) -> list` | Case-insensitive text search over memory within the current session. |

There is **no `sdk.knowledge` module** — use `sdk.memory` for persistence.

## State Operations (`sdk.state`)

| Method | Signature | Description |
|---|---|---|
| `get` | `sdk.state.get(key: str, default=None) -> Any` | Read a single key from persisted state. Returns `default` if missing or no state exists. |
| `set` | `sdk.state.set(key: str, value: Any) -> dict` | Write a single key-value pair (read-modify-write). |
| `checkpoint` | `sdk.state.checkpoint(data: dict) -> dict` | Replace the entire state blob with `data`. |
| `get_checkpoint` | `sdk.state.get_checkpoint() -> dict` | Retrieve the full state blob, or `None` if none exists. |

State persists across script invocations (e.g. cron reruns):

```python
import autonoetic_sdk

sdk = autonoetic_sdk.init()
count = sdk.state.get("count", 0)
sdk.state.set("count", count + 1)
```

## Event Operations (`sdk.events`)

| Method | Signature | Description |
|---|---|---|
| `emit` | `sdk.events.emit(event_type: str, data: dict) -> dict` | Emit a structured event to the session event log. |

## Credentials

Read secrets from **environment variables** — never from a command-line argument or hardcoded value:

```python
import os
api_key = os.environ.get("OPENWEATHER_API_KEY")
```

The gateway injects them via the `credential_env` parameter on `sandbox.exec` / `artifact.exec`, resolved server-side from the encrypted vault and never exposed to LLM context. When delegating execution that needs a secret, include the `credential_env` mapping.
