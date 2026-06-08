# TypeScript SDK Reference

The TypeScript SDK provides a typed client for agent-to-gateway communication over JSON-RPC.

## Setup

```typescript
import { Client } from 'autonoetic-sdk';

const client = new Client({
  socketPath: process.env.AUTONOETIC_SOCKET,
});
```

## Methods

The TypeScript SDK mirrors the JSON-RPC surface. Key methods:

- `client.call(method, params)` — generic JSON-RPC call
- `client.tools.list()` — list available tools
- `client.tools.invoke(name, args)` — invoke a tool

## Building

```bash
cd autonoetic-sdk/typescript && npm run build
```

## Notes

- The TypeScript SDK is less feature-complete than the Python SDK for `sandbox.exec` usage.
- For sandboxed code execution, prefer the Python SDK (`sdk.memory`, `sdk.state`, `sdk.events`).
- The TypeScript SDK is primarily useful for external integrations and CLI tooling.
