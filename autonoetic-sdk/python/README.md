# autonoetic_sdk (Python)

Python sandbox SDK for Autonoetic.

```python
import autonoetic_sdk

sdk = autonoetic_sdk.init()
text = sdk.memory.read("task.md")
```

The SDK expects a Unix socket path in `CCOS_SOCKET_PATH` (or explicit `init(socket_path=...)`).

For script agents, the runtime also injects input helpers:

```python
import autonoetic_sdk

invocation = autonoetic_sdk.load_invocation()
task = invocation.input
metadata = invocation.metadata
```

`load_invocation()` reads `AUTONOETIC_INPUT_PATH` / `AUTONOETIC_INPUT` for normalized task input and `AUTONOETIC_META_PATH` / `AUTONOETIC_META` for delegation metadata.
