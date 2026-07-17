# TypeScript SDK Reference

The TypeScript SDK is a sandbox SDK for agent scripts running inside Autonoetic sandboxes. It provides typed APIs for memory, state, secrets, messaging, files, artifacts, events, and tasks over a Unix socket JSON-RPC bridge.

Package name: `autonoetic_sdk`.

## Setup

```typescript
import { AutonoeticSdk, init, loadInvocation, loadInput, loadMetadata } from 'autonoetic_sdk';

const sdk = init();
const invocation = loadInvocation();
const input = loadInput();
const metadata = loadMetadata();
```

The SDK reads the gateway socket path from `CCOS_SOCKET_PATH` by default. You can also pass it explicitly:

```typescript
const sdk = init({ socketPath: '/path/to/gateway.sock' });
```

## Input Loading

- `loadInvocation()` — returns `{ inputRaw, input, metadataRaw, metadata, hasRuntimeInput }`
- `loadInput(defaultValue?)` — parsed task input (or default if absent)
- `loadMetadata(defaultValue?)` — parsed delegation metadata (or default if absent)

## Memory API (`sdk.memory`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `read` | `sdk.memory.read(path: string): Promise<string>` | Read file-like Tier 1 memory by path |
| `write` | `sdk.memory.write(path: string, content: string \| Uint8Array): Promise<JsonValue>` | Write file-like Tier 1 memory |
| `listKeys` | `sdk.memory.listKeys(): Promise<string[]>` | List stored memory keys |
| `remember` | `sdk.memory.remember(key: string, value: JsonValue): Promise<JsonValue>` | Persist a key-value pair to durable memory |
| `recall` | `sdk.memory.recall(key: string): Promise<JsonValue>` | Recall a stored value by key |
| `search` | `sdk.memory.search(query: string): Promise<string[]>` | Search memory contents |

## State API (`sdk.state`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `checkpoint` | `sdk.state.checkpoint(data: JsonValue): Promise<JsonValue>` | Replace the entire state blob |
| `getCheckpoint` | `sdk.state.getCheckpoint(): Promise<JsonValue>` | Retrieve the full state blob |

## Secrets API (`sdk.secrets`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `get` | `sdk.secrets.get(name: string): Promise<string>` | Read a secret value from the vault by name |

## Message API (`sdk.message`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `send` | `sdk.message.send(agentId: string, payload: JsonValue): Promise<JsonValue>` | Send a message to another agent |
| `ask` | `sdk.message.ask(agentId: string, question: string): Promise<JsonValue>` | Ask another agent a question and await an answer |

## Files API (`sdk.files`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `download` | `sdk.files.download(url: string): Promise<JsonValue>` | Download a file from a URL |
| `upload` | `sdk.files.upload(path: string, target: string): Promise<JsonValue>` | Upload a file to a target |

## Artifacts API (`sdk.artifacts`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `put` | `sdk.artifacts.put(path: string, visibility = 'private'): Promise<JsonValue>` | Put a file into the content store as an artifact |
| `mount` | `sdk.artifacts.mount(ref: string, targetPath: string): Promise<JsonValue>` | Mount an artifact reference into the sandbox |
| `share` | `sdk.artifacts.share(ref: string, agentId: string): Promise<JsonValue>` | Share an artifact with another agent |

## Events API (`sdk.events`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `emit` | `sdk.events.emit(type: string, data: JsonValue): Promise<JsonValue>` | Emit a structured event to the session event log |

## Tasks API (`sdk.tasks`)

| Method | Signature | Description |
|--------|-----------|-------------|
| `post` | `sdk.tasks.post(title: string, description: string, assignee?: string \| null): Promise<JsonValue>` | Post a new task |
| `claim` | `sdk.tasks.claim(): Promise<JsonValue>` | Claim a task from the queue |
| `complete` | `sdk.tasks.complete(taskId: string, result: JsonValue): Promise<JsonValue>` | Mark a task as complete |
| `list` | `sdk.tasks.list(status?: string \| null): Promise<JsonValue[]>` | List tasks, optionally filtered by status |

## Building

```bash
cd autonoetic-sdk/typescript && npm run build
```

## Notes

- The TypeScript SDK is a sandbox SDK, not a generic gateway client. It does not expose a generic `tools.invoke` surface.
- Use `sdk.memory` / `sdk.state` for persistence, `sdk.secrets` for vault access, and `sdk.message` for cross-agent communication.
- For script-mode agents, input is delivered via `loadInvocation()` / `loadInput()`.
