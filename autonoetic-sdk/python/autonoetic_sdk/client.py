"""Autonoetic sandbox SDK client.

Supports two modes:
1. Local agents: Unix socket to gateway (default)
2. Remote agents: HTTP to gateway (set AUTONOETIC_HTTP_URL or pass http_url=)
"""

from __future__ import annotations

import json
import os
import socket
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from . import errors

CCOS_SOCKET_ENV = "CCOS_SOCKET_PATH"
CCOS_HTTP_ENV = "AUTONOETIC_HTTP_URL"
CCOS_TOKEN_ENV = "AUTONOETIC_SHARED_SECRET"
AUTONOETIC_INPUT_ENV = "AUTONOETIC_INPUT"
AUTONOETIC_META_ENV = "AUTONOETIC_META"
AUTONOETIC_INPUT_PATH_ENV = "AUTONOETIC_INPUT_PATH"
AUTONOETIC_INPUT_FILE_ENV = "AUTONOETIC_INPUT_FILE"
AUTONOETIC_META_PATH_ENV = "AUTONOETIC_META_PATH"
AUTONOETIC_META_FILE_ENV = "AUTONOETIC_META_FILE"


def _read_text_from_env_path(*env_names: str) -> str | None:
    for env_name in env_names:
        value = os.getenv(env_name)
        if value and value.strip():
            return Path(value).read_text(encoding="utf-8")
    return None


def _strip_legacy_metadata_wrappers(input_raw: str, metadata_raw: str | None) -> str:
    if not metadata_raw:
        return input_raw

    suffix = f"\n\nDelegation metadata: {metadata_raw}"
    if input_raw.endswith(suffix):
        return input_raw[: -len(suffix)]

    task_marker = "\n\n[Task]\n"
    metadata_marker = "\n\n[Metadata]\n"
    if input_raw.startswith("[Context]\n") and input_raw.endswith(metadata_raw):
        task_start = input_raw.find(task_marker)
        metadata_start = input_raw.rfind(metadata_marker)
        if task_start != -1 and metadata_start != -1 and task_start < metadata_start:
            return input_raw[task_start + len(task_marker) : metadata_start]

    return input_raw


def _parse_json_or_text(raw: str | None) -> Any:
    if raw is None:
        return None
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return raw


@dataclass
class Invocation:
    input_raw: str | None
    input: Any
    metadata_raw: str | None
    metadata: Any

    @property
    def has_runtime_input(self) -> bool:
        return self.input_raw is not None


def load_invocation() -> Invocation:
    metadata_raw = _read_text_from_env_path(AUTONOETIC_META_PATH_ENV, AUTONOETIC_META_FILE_ENV)
    if metadata_raw is None:
        metadata_raw = os.getenv(AUTONOETIC_META_ENV)

    input_raw = _read_text_from_env_path(AUTONOETIC_INPUT_PATH_ENV, AUTONOETIC_INPUT_FILE_ENV)
    if input_raw is None:
        input_raw = os.getenv(AUTONOETIC_INPUT_ENV)

    normalized_input_raw = _strip_legacy_metadata_wrappers(input_raw, metadata_raw) if input_raw else None
    return Invocation(
        input_raw=normalized_input_raw,
        input=_parse_json_or_text(normalized_input_raw),
        metadata_raw=metadata_raw,
        metadata=_parse_json_or_text(metadata_raw),
    )


def load_input(default: Any = None) -> Any:
    invocation = load_invocation()
    return default if invocation.input is None else invocation.input


def load_metadata(default: Any = None) -> Any:
    invocation = load_invocation()
    return default if invocation.metadata is None else invocation.metadata


def _require_socket_path(path: str | None) -> str:
    if path is not None and path.strip():
        return path
    env = os.getenv(CCOS_SOCKET_ENV)
    if env and env.strip():
        return env
    raise RuntimeError(f"Missing required socket path: pass init(socket_path=...) or set {CCOS_SOCKET_ENV}")


def _get_http_url() -> str | None:
    return os.getenv(CCOS_HTTP_ENV)


def _get_token() -> str | None:
    return os.getenv(CCOS_TOKEN_ENV)


@dataclass
class _RpcClient:
    socket_path: str | None
    http_url: str | None = None
    http_token: str | None = None
    _next_id: int = 1

    def request(self, method: str, params: dict[str, Any]) -> Any:
        # Route content.* methods to HTTP if available, otherwise fall back to socket
        if self.http_url and method.startswith(("content.", "knowledge.")):
            return self._http_request(method, params)
        return self._socket_request(method, params)

    def _socket_request(self, method: str, params: dict[str, Any]) -> Any:
        if not self.socket_path:
            raise RuntimeError("No socket path configured for local gateway connection")
            
        payload = {
            "jsonrpc": "2.0",
            "id": self._next_id,
            "method": method,
            "params": params,
        }
        self._next_id += 1

        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
            conn.connect(self.socket_path)
            conn.sendall(json.dumps(payload).encode("utf-8") + b"\n")
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(4096)
                if not chunk:
                    raise RuntimeError(f"Gateway closed socket before response for method {method}")
                data += chunk

        response = json.loads(data.decode("utf-8").strip())
        if "error" in response and response["error"] is not None:
            self._raise_mapped_error(response["error"])
        if "result" not in response:
            raise RuntimeError(f"Invalid JSON-RPC response (missing result) for method {method}")
        return response["result"]

    def _http_request(self, method: str, params: dict[str, Any]) -> Any:
        """Make HTTP request to gateway's content API."""
        try:
            import urllib.request
            import urllib.error
        except ImportError:
            raise RuntimeError("urllib not available for HTTP requests")
        
        # Map JSON-RPC method to HTTP endpoint
        url = self._map_method_to_url(method)
        
        # Build request
        data = json.dumps(params).encode("utf-8")
        
        req = urllib.request.Request(url, data=data, headers={
            "Content-Type": "application/json",
        })
        
        if self.http_token:
            req.add_header("Authorization", f"Bearer {self.http_token}")
        
        try:
            with urllib.request.urlopen(req) as response:
                result = json.loads(response.read().decode("utf-8"))
                return result
        except urllib.error.HTTPError as e:
            error_body = e.read().decode("utf-8") if e.fp else str(e)
            try:
                error_json = json.loads(error_body)
                error_msg = error_json.get("error", error_body)
            except json.JSONDecodeError:
                error_msg = error_body
            raise errors.AutonoeticSdkError(f"HTTP {e.code}: {error_msg}")
    
    def _map_method_to_url(self, method: str) -> str:
        """Map JSON-RPC method to HTTP endpoint."""
        if not self.http_url:
            raise RuntimeError("No HTTP URL configured")
        base = self.http_url.rstrip("/")
        method_map = {
            "content.write": f"{base}/api/content/write",
            "content.read": f"{base}/api/content/read",
            "content.names": f"{base}/api/content/names",
        }
        if method not in method_map:
            raise errors.AutonoeticSdkError(f"Method {method} not available via HTTP")
        return method_map[method]

    @staticmethod
    def _raise_mapped_error(err: dict[str, Any]) -> None:
        message = str(err.get("message", "Unknown gateway error"))
        data = err.get("data") or {}
        err_type = str(data.get("error_type", "")).lower()
        if err_type == "policy_violation":
            raise errors.PolicyViolation(message)
        if err_type == "rate_limit_exceeded":
            raise errors.RateLimitExceeded(message)
        if err_type == "approval_required":
            secret = str(data.get("secret_name", ""))
            raise errors.ApprovalRequiredError(secret, message)
        raise errors.AutonoeticSdkError(message)


class _MemoryApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def read(self, path: str) -> str:
        result = self._rpc.request("memory.read", {"path": path})
        return str(result["content"])

    def write(self, path: str, content: bytes | str) -> Any:
        if isinstance(content, bytes):
            content = content.decode("utf-8")
        return self._rpc.request("memory.write", {"path": path, "content": content})

    def list_keys(self) -> list[str]:
        result = self._rpc.request("memory.list_keys", {})
        return list(result["keys"])

    def remember(self, key: str, value: Any, scope: str = "sdk") -> Any:
        return self._rpc.request(
            "memory.remember", {"key": key, "value": value, "scope": scope}
        )

    def recall(self, key: str) -> Any:
        result = self._rpc.request("memory.recall", {"key": key})
        return result.get("value")

    def search(self, query: str) -> list[str]:
        result = self._rpc.request("memory.search", {"query": query})
        return list(result.get("results", []))


class _StateApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def get(self, key: str, default: Any = None) -> Any:
        blob = self._rpc.request("state.get_checkpoint", {}).get("data")
        if blob is None or not isinstance(blob, dict):
            return default
        return blob.get(key, default)

    def set(self, key: str, value: Any) -> Any:
        blob = self._rpc.request("state.get_checkpoint", {}).get("data")
        if blob is None or not isinstance(blob, dict):
            blob = {}
        blob[key] = value
        return self._rpc.request("state.checkpoint", {"data": blob})

    def checkpoint(self, data: Any) -> Any:
        return self._rpc.request("state.checkpoint", {"data": data})

    def get_checkpoint(self) -> Any:
        result = self._rpc.request("state.get_checkpoint", {})
        return result.get("data")


class _SecretsApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def get(self, name: str) -> str:
        result = self._rpc.request("secrets.get", {"name": name})
        return str(result["value"])


class _MessageApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def send(self, agent_id: str, payload: Any) -> Any:
        return self._rpc.request("message.send", {"agent_id": agent_id, "payload": payload})

    def ask(self, agent_id: str, question: str) -> Any:
        result = self._rpc.request("message.ask", {"agent_id": agent_id, "question": question})
        return result.get("answer")


class _FilesApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def download(self, url: str) -> Any:
        return self._rpc.request("files.download", {"url": url})

    def upload(self, path: str, target: str) -> Any:
        return self._rpc.request("files.upload", {"path": path, "target": target})

    def read(self, name_or_handle: str) -> Any:
        """Read content from the content-addressable store by name or handle."""
        return self._rpc.request("content.read", {"name_or_handle": name_or_handle})

    def write(self, name: str, content: bytes | str) -> Any:
        """Write content to the content-addressable store. Returns handle."""
        if isinstance(content, bytes):
            content = content.decode("utf-8")
        return self._rpc.request("content.write", {"name": name, "content": content})


class _ArtifactsApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def put(self, path: str, visibility: str = "private") -> Any:
        return self._rpc.request("artifacts.put", {"path": path, "visibility": visibility})

    def mount(self, ref: str, target_path: str) -> Any:
        return self._rpc.request("artifacts.mount", {"ref": ref, "target_path": target_path})

    def share(self, ref: str, agent_id: str) -> Any:
        return self._rpc.request("artifacts.share", {"ref": ref, "agent_id": agent_id})


class _EventsApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def emit(self, type: str, data: Any) -> Any:
        return self._rpc.request("events.emit", {"type": type, "data": data})


class _TasksApi:
    def __init__(self, rpc: _RpcClient) -> None:
        self._rpc = rpc

    def post(self, title: str, description: str, assignee: str | None = None) -> Any:
        return self._rpc.request(
            "tasks.post",
            {"title": title, "description": description, "assignee": assignee},
        )

    def claim(self) -> Any:
        result = self._rpc.request("tasks.claim", {})
        return result.get("task")

    def complete(self, task_id: str, result: Any) -> Any:
        return self._rpc.request("tasks.complete", {"task_id": task_id, "result": result})

    def list(self, status: str | None = None) -> list[Any]:
        result = self._rpc.request("tasks.list", {"status": status})
        return list(result.get("tasks", []))


class AutonoeticSdk:
    """Top-level SDK surface for sandbox scripts.

    Automatically detects mode:
    - If AUTONOETIC_HTTP_URL is set, uses HTTP for content/knowledge operations
    - Otherwise uses Unix socket (sandbox mode)
    """

    def __init__(self, socket_path: str | None = None, http_url: str | None = None) -> None:
        # Detect mode from environment or explicit parameters
        resolved_http_url = http_url or _get_http_url()
        resolved_token = _get_token()
        resolved_socket = socket_path if not resolved_http_url else None
        
        if not resolved_http_url and not resolved_socket:
            resolved_socket = _require_socket_path(None)
        
        rpc = _RpcClient(
            socket_path=resolved_socket,
            http_url=resolved_http_url,
            http_token=resolved_token,
        )
        self.memory = _MemoryApi(rpc)
        self.state = _StateApi(rpc)
        self.secrets = _SecretsApi(rpc)
        self.message = _MessageApi(rpc)
        self.files = _FilesApi(rpc)
        self.artifacts = _ArtifactsApi(rpc)
        self.events = _EventsApi(rpc)
        self.tasks = _TasksApi(rpc)
        
        # Store mode info for introspection
        self._mode = "http" if resolved_http_url else "local"
        self._http_url = resolved_http_url


# Backward-compatible alias used by generated worker scripts.
class Client(AutonoeticSdk):
    pass


def init(socket_path: str | None = None, http_url: str | None = None) -> AutonoeticSdk:
    """Initialize the SDK from an explicit path or environment variables.

    Args:
        socket_path: Unix socket path (local mode). Falls back to CCOS_SOCKET_PATH.
        http_url: HTTP URL for remote gateway. Falls back to AUTONOETIC_HTTP_URL.
        
    When http_url is provided, content/knowledge operations use HTTP REST API.
    """
    return AutonoeticSdk(socket_path=socket_path, http_url=http_url)


def init_remote(gateway_url: str, token: str | None = None) -> AutonoeticSdk:
    """Initialize the SDK for remote agents using HTTP.

    Args:
        gateway_url: Full URL to the gateway (e.g., "http://gateway-host:8080")
        token: Optional auth token (falls back to AUTONOETIC_SHARED_SECRET)
        
    This is the recommended way for agents running on different machines
    to connect to a remote gateway's content API.
    """
    os.environ[CCOS_HTTP_ENV] = gateway_url
    if token:
        os.environ[CCOS_TOKEN_ENV] = token
    return AutonoeticSdk(http_url=gateway_url)
