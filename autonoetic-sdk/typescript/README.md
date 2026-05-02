# autonoetic_sdk (TypeScript)

TypeScript sandbox SDK for Autonoetic.

```ts
import { init } from "autonoetic_sdk";

const sdk = init();
const text = await sdk.memory.read("task.md");
```

The SDK expects `CCOS_SOCKET_PATH` (or explicit `init({ socketPath })`).

For script agents, runtime input helpers are also available:

```ts
import { loadInvocation } from "autonoetic_sdk";

const invocation = loadInvocation();
const task = invocation.input;
const metadata = invocation.metadata;
```

`loadInvocation()` reads `AUTONOETIC_INPUT_PATH` / `AUTONOETIC_INPUT` for normalized task input and `AUTONOETIC_META_PATH` / `AUTONOETIC_META` for delegation metadata.
