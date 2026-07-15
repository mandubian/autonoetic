#![allow(dead_code)]

pub mod agents;
pub mod promotion_trace;

use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcResponse, JsonRpcRouter};
use autonoetic_gateway::scheduler::{
    approve_request, gateway_store::GatewayStore, load_approval_requests, run_scheduler_tick,
};
use autonoetic_gateway::server::jsonrpc::start_jsonrpc_server;
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::agent_revision::{
    AgentAliasRecord, AgentRevisionRecord, AgentRevisionStatus,
};
use autonoetic_types::background::{ApprovalDecision, ApprovalRequest};
use autonoetic_types::causal_chain::CausalChainEntry;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::principal::PrincipalKind;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

pub struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
    coupled_gate: Option<(&'static str, Option<String>)>,
}

impl EnvGuard {
    pub fn set(key: &'static str, value: impl Into<String>) -> Self {
        const LLM_BASE_URL_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
        const LLM_API_KEY_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_API_KEY";
        const ALLOW_LLM_ENV_OVERRIDES_ENV: &str = "AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES";

        let previous = std::env::var(key).ok();
        std::env::set_var(key, value.into());
        let coupled_gate = if matches!(key, LLM_BASE_URL_OVERRIDE_ENV | LLM_API_KEY_OVERRIDE_ENV) {
            let gate_previous = std::env::var(ALLOW_LLM_ENV_OVERRIDES_ENV).ok();
            std::env::set_var(ALLOW_LLM_ENV_OVERRIDES_ENV, "1");
            Some((ALLOW_LLM_ENV_OVERRIDES_ENV, gate_previous))
        } else {
            None
        };
        Self {
            key,
            previous,
            coupled_gate,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
        if let Some((gate_key, gate_previous)) = self.coupled_gate.take() {
            if let Some(value) = gate_previous {
                std::env::set_var(gate_key, value);
            } else {
                std::env::remove_var(gate_key);
            }
        }
    }
}

pub struct TestWorkspace {
    tempdir: TempDir,
    pub agents_dir: PathBuf,
}

impl TestWorkspace {
    pub fn new() -> anyhow::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let agents_dir = tempdir.path().join("agents");
        std::fs::create_dir_all(&agents_dir)?;
        Ok(Self {
            tempdir,
            agents_dir,
        })
    }

    pub fn path(&self) -> &Path {
        self.tempdir.path()
    }

    pub fn gateway_config(&self) -> GatewayConfig {
        GatewayConfig {
            agents_dir: self.agents_dir.clone(),
            ..GatewayConfig::default()
        }
    }
}

type StubResponder = Arc<
    dyn Fn(String, serde_json::Value) -> Pin<Box<dyn Future<Output = serde_json::Value> + Send>>
        + Send
        + Sync,
>;

pub struct OpenAiStub {
    addr: SocketAddr,
    captured_bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl OpenAiStub {
    pub async fn spawn<F, Fut>(responder: F) -> anyhow::Result<Self>
    where
        F: Fn(String, serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = serde_json::Value> + Send + 'static,
    {
        let responder: StubResponder =
            Arc::new(move |raw_body, body_json| Box::pin(responder(raw_body, body_json)));
        let captured_bodies = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let captured = Arc::clone(&captured_bodies);
        let handle = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await?;
                let responder = Arc::clone(&responder);
                let captured = Arc::clone(&captured);
                tokio::spawn(async move {
                    if let Err(err) = handle_stub_connection(&mut stream, captured, responder).await
                    {
                        tracing::warn!(error = %err, "stub connection failed");
                    }
                });
            }
            #[allow(unreachable_code)]
            Ok(())
        });

        let stub = Self {
            addr,
            captured_bodies,
            handle,
        };

        // Verify the stub is accepting TCP connections
        stub.wait_until_ready().await?;

        Ok(stub)
    }

    /// Wait until the stub server is ready to accept TCP connections.
    async fn wait_until_ready(&self) -> anyhow::Result<()> {
        use tokio::time::{timeout, Duration};

        // Try to establish a TCP connection to verify the server is ready
        for _ in 0..10 {
            match timeout(
                Duration::from_millis(50),
                tokio::net::TcpStream::connect(self.addr),
            )
            .await
            {
                Ok(Ok(_)) => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        // TCP listener should be ready immediately after bind()
        Ok(())
    }

    pub fn completion_url(&self) -> String {
        format!("http://{}/v1/chat/completions", self.addr)
    }

    pub fn captured_bodies(&self) -> Vec<serde_json::Value> {
        self.captured_bodies.lock().unwrap().clone()
    }
}

impl Drop for OpenAiStub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn handle_stub_connection(
    stream: &mut TcpStream,
    captured_bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    responder: StubResponder,
) -> anyhow::Result<()> {
    let mut header_buf = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        header_buf.push(byte[0]);
        if header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let headers = String::from_utf8(header_buf)?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("missing Content-Length header"))?;

    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body).await?;
    let raw_body = String::from_utf8(body.clone())?;
    let body_json: serde_json::Value = serde_json::from_slice(&body)?;
    captured_bodies.lock().unwrap().push(body_json.clone());

    let response_body = responder(raw_body, body_json).await;
    let encoded = serde_json::to_vec(&response_body)?;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        encoded.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&encoded).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn spawn_gateway_server(
    mut config: GatewayConfig,
) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    config.port = addr.port();

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let router = JsonRpcRouter::new(config, Some(store));
    let handle = tokio::spawn(async move { start_jsonrpc_server(addr, router, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Ok((addr, handle))
}

pub async fn spawn_gateway_server_with_store(
    mut config: GatewayConfig,
) -> anyhow::Result<(
    SocketAddr,
    Arc<GatewayStore>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    config.port = addr.port();

    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let router = JsonRpcRouter::new(config, Some(store.clone()));
    let handle = tokio::spawn(async move { start_jsonrpc_server(addr, router, None).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    Ok((addr, store, handle))
}

fn extract_script_entry_from_skill(skill_text: &str) -> Option<String> {
    let t = skill_text.trim_start();
    if !t.starts_with("---") {
        return None;
    }
    let rest = &t[3..];
    let first_nl = rest.find(|c: char| c == '\n' || c == '\r')?;
    let after_first = &rest[first_nl..];
    let end = after_first.find("\n---")?;
    let frontmatter = &skill_text[3 + first_nl..3 + first_nl + end];
    let yaml: serde_yaml::Value = serde_yaml::from_str(frontmatter).ok()?;
    yaml.get("script_entry")?.as_str().map(|s| s.to_string())
}

pub fn seed_agent_revision(
    store: &GatewayStore,
    config: &GatewayConfig,
    agent_id: &str,
    agent_dir: &Path,
) -> anyhow::Result<String> {
    let gateway_dir = config.agents_dir.join(".gateway");
    let revision_id = format!("rev_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let rev_dir = gateway_dir
        .join("revisions")
        .join("agents")
        .join(agent_id)
        .join(&revision_id);
    if rev_dir.exists() {
        std::fs::remove_dir_all(&rev_dir)?;
    }
    std::fs::create_dir_all(&rev_dir)?;
    for entry in std::fs::read_dir(agent_dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        if path.is_dir() {
            let _ = copy_dir_all_test(&path, &rev_dir.join(file_name));
        } else {
            std::fs::copy(&path, rev_dir.join(file_name))?;
        }
    }

    let skill_path = rev_dir.join("SKILL.md");
    if skill_path.exists() {
        let skill_text = std::fs::read_to_string(&skill_path)?;
        if let Some(entry) = extract_script_entry_from_skill(&skill_text) {
            let entry_path = rev_dir.join(&entry);
            if entry_path.is_file() {
                let mut perms = std::fs::metadata(&entry_path)?.permissions();
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&entry_path, perms)?;
            }
        }
    }

    let rec = AgentRevisionRecord {
        revision_id: revision_id.clone(),
        agent_id: agent_id.to_string(),
        base_revision_id: None,
        artifact_id: None,
        content_digest: format!("sha256:seed-{agent_id}"),
        runtime_lock_hash: "sha256:seed-lock".to_string(),
        manifest_hash: "sha256:seed-manifest".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by_type: PrincipalKind::Human.tag().to_string(),
        created_by_id: "support".to_string(),
        requested_by_type: None,
        requested_by_id: None,
        source_kind: "test".to_string(),
        source_ref: None,
        origin_node_id: "gateway".to_string(),
        trust_domain: "local".to_string(),
        status: AgentRevisionStatus::Ready,
        metadata_json: serde_json::json!({}),
        short_id: String::new(),
        detected_network_hosts: None,
        signature: None,
        signer_id: None,
    };
    store.insert_agent_revision(&rec)?;
    let alias = AgentAliasRecord {
        alias_id: agent_id.to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.clone(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        updated_by_type: PrincipalKind::Human.tag().to_string(),
        updated_by_id: "support".to_string(),
        reason: Some("test seed".to_string()),
        suspended_at: None,
        suspended_reason: None,
        suspended_by: None,
    };
    store.upsert_agent_alias(&alias)?;
    Ok(revision_id)
}

fn copy_dir_all_test(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        if path.is_dir() {
            copy_dir_all_test(&path, &dst.join(file_name))?;
        } else {
            std::fs::copy(&path, dst.join(file_name))?;
        }
    }
    Ok(())
}

pub struct JsonRpcClient {
    lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    write_half: tokio::net::tcp::OwnedWriteHalf,
}

impl JsonRpcClient {
    pub async fn connect(addr: SocketAddr) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            lines: BufReader::new(read_half).lines(),
            write_half,
        })
    }

    pub async fn send(&mut self, request: JsonRpcRequest) -> anyhow::Result<()> {
        let msg = serde_json::to_string(&request)?;
        self.write_half.write_all(msg.as_bytes()).await?;
        self.write_half.write_all(b"\n").await?;
        self.write_half.flush().await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> anyhow::Result<JsonRpcResponse> {
        let line = self.lines.next_line().await?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "missing JSON-RPC response",
            )
        })?;
        Ok(serde_json::from_str(&line)?)
    }

    pub async fn event_ingest(
        &mut self,
        id: impl Into<String>,
        target_agent_id: &str,
        session_id: &str,
        event_type: &str,
        message: &str,
        metadata: Option<serde_json::Value>,
    ) -> anyhow::Result<JsonRpcResponse> {
        let mut params = serde_json::json!({
            "target_agent_id": target_agent_id,
            "session_id": session_id,
            "event_type": event_type,
            "message": message,
        });
        if let Some(metadata) = metadata {
            params["metadata"] = metadata;
        }
        self.send(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            method: "event.ingest".to_string(),
            params,
            auth_token: std::env::var("AUTONOETIC_SHARED_SECRET").ok(),
        })
        .await?;
        self.recv().await
    }

    pub async fn agent_spawn(
        &mut self,
        id: impl Into<String>,
        target_agent_id: &str,
        message: &str,
        metadata: Option<serde_json::Value>,
        session_id: Option<&str>,
    ) -> anyhow::Result<JsonRpcResponse> {
        let mut params = serde_json::json!({
            "agent_id": target_agent_id,
            "message": message,
        });
        if let Some(metadata) = metadata {
            params["metadata"] = metadata;
        }
        if let Some(session_id) = session_id {
            params["session_id"] = serde_json::json!(session_id);
        }
        self.send(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.into(),
            method: "agent_spawn".to_string(),
            params,
            auth_token: std::env::var("AUTONOETIC_SHARED_SECRET").ok(),
        })
        .await?;
        self.recv().await
    }
}

pub fn read_jsonl_entries<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(anyhow::Error::from))
        .collect()
}

pub fn read_causal_entries(path: &Path) -> anyhow::Result<Vec<CausalChainEntry>> {
    read_jsonl_entries(path)
}

pub async fn require_single_pending_approval(
    execution: Arc<GatewayExecutionService>,
    config: &GatewayConfig,
) -> anyhow::Result<ApprovalRequest> {
    for _ in 0..5 {
        run_scheduler_tick(execution.clone()).await?;
        let approvals = load_approval_requests(config, execution.gateway_store().as_deref())?;
        if approvals.len() == 1 {
            return Ok(approvals.into_iter().next().expect("approval should exist"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let approvals = load_approval_requests(config, None)?;
    anyhow::ensure!(
        approvals.len() == 1,
        "expected exactly 1 pending approval request, found {}",
        approvals.len()
    );
    Ok(approvals.into_iter().next().expect("approval should exist"))
}

pub async fn approve_pending_request_and_tick(
    execution: Arc<GatewayExecutionService>,
    config: &GatewayConfig,
    request: &ApprovalRequest,
    decided_by: &str,
    reason: Option<String>,
) -> anyhow::Result<ApprovalDecision> {
    let decision = approve_request(
        config,
        execution.gateway_store().as_deref(),
        &request.request_id,
        decided_by,
        reason,
        None,
        None,
        None,
    )?;
    run_scheduler_tick(execution).await?;
    Ok(decision)
}
