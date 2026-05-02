import net from "node:net";
import fs from "node:fs";

const CCOS_SOCKET_ENV = "CCOS_SOCKET_PATH";
const AUTONOETIC_INPUT_ENV = "AUTONOETIC_INPUT";
const AUTONOETIC_META_ENV = "AUTONOETIC_META";
const AUTONOETIC_INPUT_PATH_ENV = "AUTONOETIC_INPUT_PATH";
const AUTONOETIC_INPUT_FILE_ENV = "AUTONOETIC_INPUT_FILE";
const AUTONOETIC_META_PATH_ENV = "AUTONOETIC_META_PATH";
const AUTONOETIC_META_FILE_ENV = "AUTONOETIC_META_FILE";

export class AutonoeticSdkError extends Error {}
export class PolicyViolation extends AutonoeticSdkError {}
export class RateLimitExceeded extends AutonoeticSdkError {}
export class ApprovalRequiredError extends AutonoeticSdkError {
  public readonly secretName: string;

  constructor(secretName: string, message: string) {
    super(message);
    this.secretName = secretName;
  }
}

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export type Invocation = {
  inputRaw: string | null;
  input: JsonValue | string | null;
  metadataRaw: string | null;
  metadata: JsonValue | string | null;
  hasRuntimeInput: boolean;
};

function readTextFromEnvPath(...envNames: string[]): string | null {
  for (const envName of envNames) {
    const value = process.env[envName];
    if (value && value.trim().length > 0) {
      return fs.readFileSync(value, "utf8");
    }
  }
  return null;
}

function stripLegacyMetadataWrappers(inputRaw: string, metadataRaw: string | null): string {
  if (!metadataRaw) {
    return inputRaw;
  }

  const suffix = `\n\nDelegation metadata: ${metadataRaw}`;
  if (inputRaw.endsWith(suffix)) {
    return inputRaw.slice(0, -suffix.length);
  }

  const taskMarker = "\n\n[Task]\n";
  const metadataMarker = "\n\n[Metadata]\n";
  if (inputRaw.startsWith("[Context]\n") && inputRaw.endsWith(metadataRaw)) {
    const taskStart = inputRaw.indexOf(taskMarker);
    const metadataStart = inputRaw.lastIndexOf(metadataMarker);
    if (taskStart !== -1 && metadataStart !== -1 && taskStart < metadataStart) {
      return inputRaw.slice(taskStart + taskMarker.length, metadataStart);
    }
  }

  return inputRaw;
}

function parseJsonOrText(raw: string | null): JsonValue | string | null {
  if (raw === null) {
    return null;
  }
  try {
    return JSON.parse(raw) as JsonValue;
  } catch {
    return raw;
  }
}

export function loadInvocation(): Invocation {
  const metadataRaw =
    readTextFromEnvPath(AUTONOETIC_META_PATH_ENV, AUTONOETIC_META_FILE_ENV) ??
    process.env[AUTONOETIC_META_ENV] ??
    null;
  const inputRaw =
    readTextFromEnvPath(AUTONOETIC_INPUT_PATH_ENV, AUTONOETIC_INPUT_FILE_ENV) ??
    process.env[AUTONOETIC_INPUT_ENV] ??
    null;
  const normalizedInput = inputRaw === null ? null : stripLegacyMetadataWrappers(inputRaw, metadataRaw);
  return {
    inputRaw: normalizedInput,
    input: parseJsonOrText(normalizedInput),
    metadataRaw,
    metadata: parseJsonOrText(metadataRaw),
    hasRuntimeInput: normalizedInput !== null,
  };
}

export function loadInput<T extends JsonValue | string | null = JsonValue | string | null>(
  defaultValue: T | null = null,
): T | JsonValue | string | null {
  const invocation = loadInvocation();
  return invocation.input === null ? defaultValue : invocation.input;
}

export function loadMetadata<T extends JsonValue | string | null = JsonValue | string | null>(
  defaultValue: T | null = null,
): T | JsonValue | string | null {
  const invocation = loadInvocation();
  return invocation.metadata === null ? defaultValue : invocation.metadata;
}

type JsonRpcResponse = {
  jsonrpc: string;
  id: JsonValue;
  result?: JsonValue;
  error?: {
    code: number;
    message: string;
    data?: { [key: string]: JsonValue };
  };
};

class RpcClient {
  private nextId = 1;
  private readonly socketPath: string;

  constructor(socketPath: string) {
    this.socketPath = socketPath;
  }

  async request(method: string, params: { [key: string]: JsonValue }): Promise<JsonValue> {
    const id = this.nextId++;
    const payload = JSON.stringify({
      jsonrpc: "2.0",
      id,
      method,
      params,
    }) + "\n";

    const raw = await new Promise<string>((resolve, reject) => {
      const conn = net.createConnection(this.socketPath);
      let data = "";

      conn.on("connect", () => {
        conn.write(payload);
      });
      conn.on("data", (chunk) => {
        data += chunk.toString("utf8");
        if (data.includes("\n")) {
          conn.end();
          resolve(data.split("\n")[0]);
        }
      });
      conn.on("error", (err) => reject(err));
      conn.on("end", () => {
        if (!data.includes("\n")) {
          reject(new Error(`Gateway closed socket before response for method ${method}`));
        }
      });
    });

    const response = JSON.parse(raw) as JsonRpcResponse;
    if (response.error) {
      throw mapError(response.error);
    }
    if (response.result === undefined) {
      throw new AutonoeticSdkError(`Invalid JSON-RPC response (missing result) for method ${method}`);
    }
    return response.result;
  }
}

function requireSocketPath(explicit?: string): string {
  if (explicit && explicit.trim().length > 0) {
    return explicit;
  }
  const env = process.env[CCOS_SOCKET_ENV];
  if (env && env.trim().length > 0) {
    return env;
  }
  throw new Error(`Missing required socket path: pass init({ socketPath }) or set ${CCOS_SOCKET_ENV}`);
}

function mapError(error: { code: number; message: string; data?: { [key: string]: JsonValue } }): Error {
  const errorType = String(error.data?.error_type ?? "").toLowerCase();
  if (errorType === "policy_violation") {
    return new PolicyViolation(error.message);
  }
  if (errorType === "rate_limit_exceeded") {
    return new RateLimitExceeded(error.message);
  }
  if (errorType === "approval_required") {
    return new ApprovalRequiredError(String(error.data?.secret_name ?? ""), error.message);
  }
  return new AutonoeticSdkError(error.message);
}

class MemoryApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async read(path: string): Promise<string> {
    const result = (await this.rpc.request("memory.read", { path })) as { content: JsonValue };
    return String(result.content);
  }

  async write(path: string, content: string | Uint8Array): Promise<JsonValue> {
    const text = typeof content === "string" ? content : Buffer.from(content).toString("utf8");
    return this.rpc.request("memory.write", { path, content: text });
  }

  async listKeys(): Promise<string[]> {
    const result = (await this.rpc.request("memory.list_keys", {})) as { keys: JsonValue };
    return (result.keys as JsonValue[]).map((v) => String(v));
  }

  async remember(key: string, value: JsonValue): Promise<JsonValue> {
    return this.rpc.request("memory.remember", { key, value });
  }

  async recall(key: string): Promise<JsonValue> {
    const result = (await this.rpc.request("memory.recall", { key })) as { value?: JsonValue };
    return result.value ?? null;
  }

  async search(query: string): Promise<string[]> {
    const result = (await this.rpc.request("memory.search", { query })) as { results?: JsonValue };
    const values = (result.results ?? []) as JsonValue[];
    return values.map((v) => String(v));
  }
}

class StateApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async checkpoint(data: JsonValue): Promise<JsonValue> {
    return this.rpc.request("state.checkpoint", { data });
  }

  async getCheckpoint(): Promise<JsonValue> {
    const result = (await this.rpc.request("state.get_checkpoint", {})) as { data?: JsonValue };
    return result.data ?? null;
  }
}

class SecretsApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async get(name: string): Promise<string> {
    const result = (await this.rpc.request("secrets.get", { name })) as { value: JsonValue };
    return String(result.value);
  }
}

class MessageApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async send(agentId: string, payload: JsonValue): Promise<JsonValue> {
    return this.rpc.request("message.send", { agent_id: agentId, payload });
  }

  async ask(agentId: string, question: string): Promise<JsonValue> {
    const result = (await this.rpc.request("message.ask", {
      agent_id: agentId,
      question,
    })) as { answer?: JsonValue };
    return result.answer ?? null;
  }
}

class FilesApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async download(url: string): Promise<JsonValue> {
    return this.rpc.request("files.download", { url });
  }

  async upload(path: string, target: string): Promise<JsonValue> {
    return this.rpc.request("files.upload", { path, target });
  }
}

class ArtifactsApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async put(path: string, visibility = "private"): Promise<JsonValue> {
    return this.rpc.request("artifacts.put", { path, visibility });
  }

  async mount(ref: string, targetPath: string): Promise<JsonValue> {
    return this.rpc.request("artifacts.mount", { ref, target_path: targetPath });
  }

  async share(ref: string, agentId: string): Promise<JsonValue> {
    return this.rpc.request("artifacts.share", { ref, agent_id: agentId });
  }
}

class EventsApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async emit(type: string, data: JsonValue): Promise<JsonValue> {
    return this.rpc.request("events.emit", { type, data });
  }
}

class TasksApi {
  private readonly rpc: RpcClient;
  constructor(rpc: RpcClient) {
    this.rpc = rpc;
  }

  async post(title: string, description: string, assignee: string | null = null): Promise<JsonValue> {
    return this.rpc.request("tasks.post", { title, description, assignee });
  }

  async claim(): Promise<JsonValue> {
    const result = (await this.rpc.request("tasks.claim", {})) as { task?: JsonValue };
    return result.task ?? null;
  }

  async complete(taskId: string, result: JsonValue): Promise<JsonValue> {
    return this.rpc.request("tasks.complete", { task_id: taskId, result });
  }

  async list(status: string | null = null): Promise<JsonValue[]> {
    const result = (await this.rpc.request("tasks.list", { status })) as { tasks?: JsonValue };
    return (result.tasks ?? []) as JsonValue[];
  }
}

export class AutonoeticSdk {
  public readonly memory: MemoryApi;
  public readonly state: StateApi;
  public readonly secrets: SecretsApi;
  public readonly message: MessageApi;
  public readonly files: FilesApi;
  public readonly artifacts: ArtifactsApi;
  public readonly events: EventsApi;
  public readonly tasks: TasksApi;

  constructor(socketPath: string) {
    const rpc = new RpcClient(socketPath);
    this.memory = new MemoryApi(rpc);
    this.state = new StateApi(rpc);
    this.secrets = new SecretsApi(rpc);
    this.message = new MessageApi(rpc);
    this.files = new FilesApi(rpc);
    this.artifacts = new ArtifactsApi(rpc);
    this.events = new EventsApi(rpc);
    this.tasks = new TasksApi(rpc);
  }
}

export function init(opts?: { socketPath?: string }): AutonoeticSdk {
  return new AutonoeticSdk(requireSocketPath(opts?.socketPath));
}
