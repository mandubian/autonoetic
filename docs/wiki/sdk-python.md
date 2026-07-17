# Python SDK Reference (sandbox_exec and script-mode agents)

When your code runs inside **`sandbox_exec`** or as a **`execution_mode: script`** agent entrypoint, it executes in a Python environment with the `autonoetic_sdk` package available. Always call **`sdk = autonoetic_sdk.init()`** first.

## Import

```python
import autonoetic_sdk
sdk = autonoetic_sdk.init()
invocation = autonoetic_sdk.load_invocation()
task = invocation.input
metadata = invocation.metadata
```

## Memory Operations (`sdk.memory`)

| Method | Signature | Description |
|---|---|---|
| `remember` | `sdk.memory.remember(key: str, value: Any, scope: str = "sdk") -> Any` | Persist a key-value pair to durable storage. **Visibility defaults to `session`**: any agent in the same root session can read it. |
| `recall` | `sdk.memory.recall(key: str) -> Any` | Retrieve a previously stored value by key. Returns `None` if not found or not visible. |
| `read` | `sdk.memory.read(path: str) -> str` | Read file-like content from Tier 1 memory by path. |
| `write` | `sdk.memory.write(path: str, content: str) -> str` | Write file-like content to Tier 1 memory (private scratch pad). |
| `list_keys` | `sdk.memory.list_keys() -> list[str]` | List all stored keys in the current scope. |
| `search` | `sdk.memory.search(query: str) -> list` | Case-insensitive text search over memory contents within the current session. |

## State Operations (`sdk.state`)

State is a persisted key-value blob that survives across turns. Use it for counters, cursors, accumulators, or any data that must persist between executions of the same agent.

| Method | Signature | Description |
|---|---|---|
| `get` | `sdk.state.get(key: str, default=None) -> Any` | Read a single key from persisted state. Returns `default` if key is missing or no state exists. |
| `set` | `sdk.state.set(key: str, value: Any) -> dict` | Write a single key-value pair to persisted state (read-modify-write). |
| `checkpoint` | `sdk.state.checkpoint(data: dict) -> dict` | Replace the entire state blob with `data`. |
| `get_checkpoint` | `sdk.state.get_checkpoint() -> dict` | Retrieve the full state blob. Returns `None` if none exists. |

## Event Operations (`sdk.events`)

| Method | Signature | Description |
|---|---|---|
| `emit` | `sdk.events.emit(event_type: str, data: dict) -> dict` | Emit a structured event to the session event log. |

## Example: Fibonacci Calculator with Persisted State

```python
import autonoetic_sdk

sdk = autonoetic_sdk.init()

def main():
    a = sdk.state.get("a", 0)
    b = sdk.state.get("b", 1)

    next_fib = a + b
    print(f"Next Fibonacci number: {next_fib}")

    sdk.state.set("a", b)
    sdk.state.set("b", next_fib)

if __name__ == "__main__":
    main()
```

## Important Notes

- **There is no `sdk.knowledge` module.** Use `sdk.memory.remember()` / `sdk.memory.recall()` for persistence.
- **Memory visibility**: `sdk.memory.remember()` stores data with `session` visibility by default — any agent in the same root session can read it via `sdk.memory.recall()` or the native `knowledge_recall`/`knowledge_search` tools. Use Tier 1 `sdk.memory.write()` for private scratch data that should not be shared.
- The SDK bridge only supports the methods listed in the tables above. Do not invent or guess method names — every available method is documented here. Unsupported calls raise `AutonoeticSdkError`.
- The SDK is injected via `PYTHONPATH` and communicates with the gateway over a Unix socket. No network access is required.
- Script agents can read normalized runtime input via `autonoetic_sdk.load_input()` / `load_invocation()`. Delegation metadata, when present, is exposed separately via `invocation.metadata`.

## Credential Injection via Environment Variables

When a script needs an API key or secret, **read it from an environment variable** — never from a command-line argument or hardcoded value:

```python
import os
api_key = os.environ.get("OPENWEATHER_API_KEY")
```

The gateway injects credentials into the sandbox via the `credential_env` parameter on `sandbox_exec` and `artifact_exec`. The secret is resolved server-side from the encrypted vault and never exposed to LLM context. When delegating execution that requires credentials, include the `credential_env` mapping so the secret is available at runtime.
