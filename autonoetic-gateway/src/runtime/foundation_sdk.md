# SDK Reference for Sandboxed Code

When your code runs inside `sandbox.exec`, it executes in a Python environment with the `autonoetic_sdk` package available.

## Import

```python
import autonoetic_sdk
sdk = autonoetic_sdk.init()
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

| Method | Signature | Description |
|---|---|---|
| `checkpoint` | `sdk.state.checkpoint(data: dict) -> dict` | Persist a state checkpoint (e.g., progress tracking). |
| `get_checkpoint` | `sdk.state.get_checkpoint() -> dict` | Retrieve the last saved checkpoint. Returns `{}` if none exists. |

## Event Operations (`sdk.events`)

| Method | Signature | Description |
|---|---|---|
| `emit` | `sdk.events.emit(event_type: str, data: dict) -> dict` | Emit a structured event to the session event log. |

## Example: Fibonacci Calculator with Persisted State

```python
import autonoetic_sdk

sdk = autonoetic_sdk.init()

def main():
    state = sdk.state.get_checkpoint() or {}
    a = state.get("a", 0)
    b = state.get("b", 1)

    next_fib = a + b
    print(f"Next Fibonacci number: {next_fib}")

    sdk.state.checkpoint({"a": b, "b": next_fib})

if __name__ == "__main__":
    main()
```

## Important Notes

- **There is no `sdk.knowledge` module.** Use `sdk.memory.remember()` / `sdk.memory.recall()` for persistence.
- **Memory visibility**: `sdk.memory.remember()` stores data with `session` visibility by default — any agent in the same root session can read it via `sdk.memory.recall()` or the native `knowledge.recall`/`knowledge.search` tools. Use Tier 1 `sdk.memory.write()` for private scratch data that should not be shared.
- The SDK bridge only supports the methods listed above. Calling unsupported methods (e.g., `sdk.secrets.get`, `sdk.message.send`) will raise `AutonoeticSdkError`.
- The SDK is injected via `PYTHONPATH` and communicates with the gateway over a Unix socket. No network access is required.
