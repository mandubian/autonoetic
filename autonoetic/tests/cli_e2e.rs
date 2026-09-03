use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn run_autonoetic(args: &[&str], stdin_input: Option<&str>) -> Output {
    run_autonoetic_with_env(args, stdin_input, &[])
}

fn run_autonoetic_with_env(
    args: &[&str],
    stdin_input: Option<&str>,
    envs: &[(&str, &str)],
) -> Output {
    let bin = env!("CARGO_BIN_EXE_autonoetic");
    let mut command = Command::new(bin);
    command.args(args);
    command.envs(envs.iter().copied());
    command.env("AUTONOETIC_VAULT_KEY", "000102030405060708090a0b0c0d0e0f000102030405060708090a0b0c0d0e0f");
    command.env("AUTONOETIC_SHARED_SECRET", "test-secret");
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    if stdin_input.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command
        .spawn()
        .expect("autonoetic test process should spawn");
    if let Some(input) = stdin_input {
        child
            .stdin
            .as_mut()
            .expect("stdin pipe should be present")
            .write_all(input.as_bytes())
            .expect("stdin should be writable");
    }
    child
        .wait_with_output()
        .expect("autonoetic test process should complete")
}

/// Serve the gateway JSON-RPC from the given store on the given port (#1119:
/// operator commands speak RPC to a running gateway, so tests invoking
/// `gateway pending`/read paths need a live server). Shuts down on drop.
struct GatewayServerGuard {
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for GatewayServerGuard {
    fn drop(&mut self) {
        // Detach: the serving thread runs until the test process exits
        // (joining would deadlock — the accept loop never returns).
        self.handle.take();
    }
}

fn serve_gateway_for_test(
    agents_dir: &Path,
    port: u16,
    store: Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>,
) -> GatewayServerGuard {
    let config = autonoetic_types::config::GatewayConfig {
        agents_dir: agents_dir.to_path_buf(),
        port,
        ..autonoetic_types::config::GatewayConfig::default()
    };
    let handle = std::thread::spawn(move || {
        // Router construction requires a runtime context (execution service
        // spawns scheduler tasks), so build it inside the server thread.
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let _guard = runtime.enter();
        let router = autonoetic_gateway::router::JsonRpcRouter::new(config, Some(store));
        drop(_guard);
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let _ = runtime.block_on(async move {
            autonoetic_gateway::server::jsonrpc::start_jsonrpc_server(addr, router, None).await
        });
    });
    // Wait until the listener is accepting (poll the port).
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).is_ok() {
            return GatewayServerGuard { handle: Some(handle) };
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test gateway did not start listening on port {port}");
}

struct ChildGuard {
    child: Option<Child>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ChildGuard {
    fn stdin_mut(&mut self) -> &mut std::process::ChildStdin {
        self.child
            .as_mut()
            .expect("child should be present")
            .stdin
            .as_mut()
            .expect("stdin pipe should be present")
    }

    fn wait_with_output(&mut self) -> Output {
        self.child
            .take()
            .expect("child should be present")
            .wait_with_output()
            .expect("child should complete")
    }
}

fn spawn_autonoetic(
    args: &[&str],
    envs: &[(&str, &str)],
    stdin_piped: bool,
    capture_output: bool,
) -> ChildGuard {
    let bin = env!("CARGO_BIN_EXE_autonoetic");
    let mut command = Command::new(bin);
    command.args(args);
    command.envs(envs.iter().copied());
    command.env("AUTONOETIC_VAULT_KEY", "000102030405060708090a0b0c0d0e0f000102030405060708090a0b0c0d0e0f");
    command.env("AUTONOETIC_SHARED_SECRET", "test-secret");
    command.stdout(if capture_output {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stderr(if capture_output {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    command.stdin(if stdin_piped {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    ChildGuard {
        child: Some(
            command
                .spawn()
                .expect("autonoetic test process should spawn"),
        ),
    }
}

fn pick_unused_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("port probe should bind")
        .local_addr()
        .expect("port probe should expose local addr")
        .port()
}

fn wait_for_port(addr: SocketAddr, timeout: Duration) {
    let start = Instant::now();
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for {}", addr);
        thread::sleep(Duration::from_millis(25));
    }
}

/// Where `write_config` puts the gateway's runtime dir: a sibling of
/// `agents_dir`, matching the real layout. Tests that open the store directly
/// must use this, not `agents_dir.join(".gateway")` — the CLI reads
/// `config.runtime_dir` and would otherwise be talking to a different store.
fn test_gateway_dir(agents_dir: &Path) -> std::path::PathBuf {
    agents_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("runtime")
}

fn write_config(
    config_path: &Path,
    agents_dir: &Path,
    port: u16,
    ofp_port: u16,
    max_pending_spawns_per_agent: usize,
) {
    // Declared explicitly rather than left to the default: these tests open the
    // store directly and must agree with the CLI about where it is. See
    // `test_gateway_dir` for the matching accessor.
    let yaml = format!(
        "agents_dir: \"{}\"\n\
         runtime_dir: \"{}\"\n\
         port: {}\n\
         ofp_port: {}\n\
         http_port: 0\n\
         tls: false\n\
         max_pending_spawns_per_agent: {}\n\
         max_concurrent_spawns: 4\n\
         background_scheduler_enabled: false\n\
         digest_agent:\n  \
           enabled: false\n\
         llm_presets:\n  \
           fallback:\n    \
             provider: \"ollama\"\n    \
             model: \"test-model\"\n    \
             temperature: 0.2\n",
        agents_dir.display(),
        test_gateway_dir(agents_dir).display(),
        port,
        ofp_port,
        max_pending_spawns_per_agent
    );
    std::fs::write(config_path, yaml).expect("config should write");
}

fn write_memory_agent(agent_dir: &Path, agent_id: &str) {
    std::fs::create_dir_all(agent_dir).expect("agent dir should create");
    let skill = format!(
        r#"---
name: "{agent_id}"
description: "Terminal chat memory agent"
metadata:
  autonoetic:
    version: "1.0"
    agent:
      id: "{agent_id}"
      name: "{agent_id}"
      description: "Terminal chat memory agent"
    llm_config:
      provider: "openai"
      model: "test-model"
      temperature: 0.0
    capabilities:
      - type: "WriteAccess"
        scopes: ["*"]
      - type: "ReadAccess"
        scopes: ["*"]
---
# Terminal Memory Agent
Use memory tools when needed.
"#
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill).expect("skill should write");
}

fn write_builder_agent(agent_dir: &Path, agent_id: &str) {
    std::fs::create_dir_all(agent_dir).expect("agent dir should create");
    let skill = [
        "---".to_string(),
        format!("name: \"{}\"", agent_id),
        "description: \"Terminal chat builder agent\"".to_string(),
        "metadata:".to_string(),
        "  autonoetic:".to_string(),
        "    version: \"1.0\"".to_string(),
        "    runtime:".to_string(),
        "      engine: \"autonoetic\"".to_string(),
        "      gateway_version: \"0.1.0\"".to_string(),
        "      sdk_version: \"0.1.0\"".to_string(),
        "      type: \"stateful\"".to_string(),
        "      sandbox: \"bubblewrap\"".to_string(),
        "      runtime_lock: \"runtime.lock\"".to_string(),
        "    agent:".to_string(),
        format!("      id: \"{}\"", agent_id),
        format!("      name: \"{}\"", agent_id),
        "      description: \"Terminal chat builder agent\"".to_string(),
        "    llm_config:".to_string(),
        "      provider: \"openai\"".to_string(),
        "      model: \"test-model\"".to_string(),
        "      temperature: 0.0".to_string(),
        "    capabilities:".to_string(),
        "      - type: \"AgentSpawn\"".to_string(),
        "        max_children: 8".to_string(),
        "---".to_string(),
        "# Terminal Builder Agent".to_string(),
        "Use `agent.install` when the user asks for a durable worker.".to_string(),
        String::new(),
    ]
    .join("\n");
    std::fs::write(agent_dir.join("SKILL.md"), skill).expect("skill should write");
}

fn write_planner_agent(agent_dir: &Path, agent_id: &str) {
    std::fs::create_dir_all(agent_dir).expect("agent dir should create");
    let skill = format!(
        r#"---
name: "{agent_id}"
description: "Planner E2E agent"
metadata:
  autonoetic:
    version: "1.0"
    agent:
      id: "{agent_id}"
      name: "{agent_id}"
      description: "Planner E2E agent"
    llm_config:
      provider: "openai"
      model: "test-model"
      temperature: 0.0
    capabilities:
      - type: "AgentSpawn"
        max_children: 8
---
# Planner E2E Agent
Delegate using agent.spawn when specialist work is needed.
"#
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill).expect("skill should write");
}

fn write_researcher_agent(agent_dir: &Path, agent_id: &str) {
    std::fs::create_dir_all(agent_dir).expect("agent dir should create");
    let skill = format!(
        r#"---
name: "{agent_id}"
description: "Researcher E2E agent"
metadata:
  autonoetic:
    version: "1.0"
    agent:
      id: "{agent_id}"
      name: "{agent_id}"
      description: "Researcher E2E agent"
    llm_config:
      provider: "openai"
      model: "test-model"
      temperature: 0.0
---
# Researcher E2E Agent
Return concise research findings.
"#
    );
    std::fs::write(agent_dir.join("SKILL.md"), skill).expect("skill should write");
}

fn spawn_openai_stub(captured_bodies: Arc<Mutex<Vec<serde_json::Value>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("stub listener should bind");
    let addr = listener
        .local_addr()
        .expect("stub listener should expose addr");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let captured = captured_bodies.clone();
            match stream {
                Ok(mut stream) => {
                    if let Err(err) = handle_stub_connection(&mut stream, captured) {
                        panic!("stub connection failed: {err}");
                    }
                }
                Err(err) => panic!("stub accept failed: {err}"),
            }
        }
    });
    addr
}

fn handle_stub_connection(
    stream: &mut TcpStream,
    captured_bodies: Arc<Mutex<Vec<serde_json::Value>>>,
) -> anyhow::Result<()> {
    let mut header_buf = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
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
    stream.read_exact(&mut body)?;
    let body_json: serde_json::Value = serde_json::from_slice(&body)?;
    captured_bodies.lock().unwrap().push(body_json.clone());

    let latest_user_message = body_json
        .get("messages")
        .and_then(|value| value.as_array())
        .and_then(|messages| {
            messages.iter().rev().find_map(|message| {
                if message.get("role").and_then(|value| value.as_str()) == Some("user") {
                    message
                        .get("content")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();
    let has_tool_result = body_json
        .get("messages")
        .and_then(|value| value.as_array())
        .map(|messages| {
            messages
                .iter()
                .any(|message| message.get("role").and_then(|value| value.as_str()) == Some("tool"))
        })
        .unwrap_or(false);
    let tool_result_count = body_json
        .get("messages")
        .and_then(|value| value.as_array())
        .map(|messages| {
            messages
                .iter()
                .filter(|message| {
                    message.get("role").and_then(|value| value.as_str()) == Some("tool")
                })
                .count()
        })
        .unwrap_or(0);
    let has_validation_tool_result = body_json
        .get("messages")
        .and_then(|value| value.as_array())
        .map(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(|value| value.as_str()) == Some("tool")
                    && message
                        .get("content")
                        .and_then(|value| value.as_str())
                        .map(|content| {
                            content.contains("\"error_type\":\"validation\"")
                                || content.contains("\"error_type\": \"validation\"")
                        })
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if latest_user_message.contains("delay message") {
        thread::sleep(Duration::from_millis(300));
    }

    let response_body = if latest_user_message.contains("delegate to specialist")
        && !has_tool_result
    {
        let spawn_args = serde_json::json!({
            "agent_id": "researcher.default",
            "message": "Investigate the request and summarize key findings."
        });

        serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_spawn_researcher",
                        "type": "function",
                        "function": {
                            "name": "agent.spawn",
                            "arguments": spawn_args.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8}
        })
    } else if latest_user_message.contains("delegate to specialist") && has_tool_result {
        serde_json::json!({
            "choices": [{
                "message": { "content": "Delegated to researcher.default and received findings." },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8}
        })
    } else if latest_user_message.contains("repair invalid agent install") && !has_tool_result {
        let invalid_install_args = serde_json::json!({
            "agent_id": "",
            "name": "repair_worker",
            "description": "Broken worker payload",
            "instructions": "# Broken Worker\nThis payload should fail validation.",
            "files": [
                {
                    "path": "state/seed.txt",
                    "content": "seed"
                }
            ],
            "arm_immediately": false
        });

        serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_install_invalid",
                        "type": "function",
                        "function": {
                            "name": "agent.install",
                            "arguments": invalid_install_args.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8}
        })
    } else if latest_user_message.contains("repair invalid agent install")
        && tool_result_count == 1
        && has_validation_tool_result
    {
        let corrected_install_args = serde_json::json!({
            "agent_id": "repair_worker",
            "name": "repair_worker",
            "description": "Worker installed after validation repair.",
            "instructions": "# Repair Worker\nInstalled after a corrected agent.install retry.",
            "files": [
                {
                    "path": "state/seed.txt",
                    "content": "seed"
                }
            ],
            "arm_immediately": false,
            "promotion_gate": {
                "evaluator_pass": true,
                "auditor_pass": true
            }
        });

        serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_install_corrected",
                        "type": "function",
                        "function": {
                            "name": "agent.install",
                            "arguments": corrected_install_args.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8}
        })
    } else if latest_user_message.contains("repair invalid agent install") && tool_result_count >= 2
    {
        serde_json::json!({
            "choices": [{
                "message": { "content": "Installed repair_worker after retry." },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 8}
        })
    } else if latest_user_message.contains("please store this data") && !has_tool_result {
        serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "content.write",
                            "arguments": "{\"name\":\"secret.txt\",\"content\":\"secret_value_123\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        })
    } else if latest_user_message.contains("please store this data") && has_tool_result {
        serde_json::json!({
            "choices": [{
                "message": { "content": "I stored it" },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        })
    } else if latest_user_message.contains("what is the data?") && !has_tool_result {
        serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_2",
                        "type": "function",
                        "function": {
                            "name": "content.read",
                            "arguments": "{\"name\":\"secret.txt\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        })
    } else if latest_user_message.contains("what is the data?") && has_tool_result {
        serde_json::json!({
            "choices": [{
                "message": { "content": "The data is secret_value_123" },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        })
    } else if latest_user_message.contains("delay message") {
        serde_json::json!({
            "choices": [{
                "message": { "content": "Delayed reply" },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        })
    } else {
        serde_json::json!({
            "choices": [{
                "message": { "content": "stub assistant reply" },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        })
    };

    let encoded = serde_json::to_vec(&response_body)?;
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        encoded.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(&encoded)?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

#[test]
fn test_agent_init_then_interactive_run_exits_cleanly() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config_path = temp.path().join("config.yaml");
    let agents_dir = temp.path().join("agents");
    write_config(&config_path, &agents_dir, 4000, 4200, 4);

    let config_arg = config_path.to_string_lossy().to_string();

    let init = run_autonoetic(
        &[
            "--config",
            config_arg.as_str(),
            "agent",
            "init",
            "agent_e2e",
            "--template",
            "coder",
        ],
        None,
    );
    assert!(
        init.status.success(),
        "agent init failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );

    let skill_path = agents_dir.join("agent_e2e").join("SKILL.md");
    let runtime_lock_path = agents_dir.join("agent_e2e").join("runtime.lock");
    assert!(
        skill_path.exists(),
        "SKILL.md should be generated by agent init"
    );
    assert!(
        runtime_lock_path.exists(),
        "runtime.lock should be generated by agent init"
    );

    // Keep the run command hermetic: select a local provider so no API key is required.
    let skill = std::fs::read_to_string(&skill_path).expect("SKILL.md should read");
    let patched = skill.replace("provider: \"openai\"", "provider: \"ollama\"");
    std::fs::write(&skill_path, patched).expect("SKILL.md should update");

    let run = run_autonoetic(
        &[
            "--config",
            config_arg.as_str(),
            "agent",
            "run",
            "agent_e2e",
            "--interactive",
        ],
        Some("/exit\n"),
    );
    assert!(
        run.status.success(),
        "agent run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("Interactive mode enabled. Type /exit to quit."),
        "interactive banner should be printed, got stdout:\n{}",
        stdout
    );
}

// Ignored in CI: full-stack e2e that spawns the gateway daemon + an OpenAI
// stub and asserts on chat/routing output. The assertions drift as the
// chat/routing/digest pipeline evolves and the flow is timing-sensitive under
// CI's loaded, single-threaded runner. Run locally with `--ignored`.
#[test]
#[ignore = "full-stack chat e2e; spawns gateway+LLM stub, drifts as pipeline evolves — run with --ignored"]
fn test_terminal_chat_routes_through_gateway_ingress_and_preserves_session() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config_path = temp.path().join("config.yaml");
    let agents_dir = temp.path().join("agents");
    let agent_id = "memory_chat";
    let jsonrpc_port = pick_unused_port();
    let ofp_port = pick_unused_port();
    write_config(&config_path, &agents_dir, jsonrpc_port, ofp_port, 4);
    write_memory_agent(&agents_dir.join(agent_id), agent_id);

    let captured_bodies = Arc::new(Mutex::new(Vec::new()));
    let stub_addr = spawn_openai_stub(captured_bodies.clone());
    let config_arg = config_path.to_string_lossy().to_string();
    let stub_url = format!("http://{}/v1/chat/completions", stub_addr);
    let gateway_env = [
        ("AUTONOETIC_NODE_ID", "test-gateway"),
        ("AUTONOETIC_NODE_NAME", "Test Gateway"),
        ("AUTONOETIC_SHARED_SECRET", "test-secret"),
        ("AUTONOETIC_LLM_BASE_URL", stub_url.as_str()),
        ("AUTONOETIC_LLM_API_KEY", "test-key"),
    ];
    let gateway_args = ["--config", config_arg.as_str(), "gateway", "start"];
    let _gateway = spawn_autonoetic(&gateway_args, &gateway_env, false, false);
    wait_for_port(
        format!("127.0.0.1:{}", jsonrpc_port)
            .parse()
            .expect("gateway addr should parse"),
        Duration::from_secs(5),
    );

    let session_id = "terminal-session-1";
    let channel_id = "terminal:tester:memory_chat";
    let chat = run_autonoetic(
        &[
            "--config",
            config_arg.as_str(),
            "chat",
            agent_id,
            "--sender-id",
            "tester",
            "--channel-id",
            channel_id,
            "--session-id",
            session_id,
            "--test-mode",
        ],
        Some("please store this data\nwhat is the data?\n/exit\n"),
    );
    assert!(
        chat.status.success(),
        "chat failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&chat.stdout),
        String::from_utf8_lossy(&chat.stderr)
    );
    let stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(
        stdout.contains("I stored it"),
        "expected write reply, got stdout:\n{}",
        stdout
    );
    assert!(
        stdout.contains("The data is secret_value_123"),
        "expected recall reply, got stdout:\n{}",
        stdout
    );

    // Content is now stored in the content-addressable store, not in state/ directory
    // Check that the content was stored in the gateway's content store
    let gateway_dir = temp.path().join("runtime");
    let content_store_dir = gateway_dir.join("content");
    assert!(
        content_store_dir.exists(),
        "content store should exist at {:?}",
        content_store_dir
    );
    // The content store uses SHA-256 hashes as filenames, so we just verify the store exists
    // and contains at least one file
    let content_files: Vec<_> = std::fs::read_dir(&content_store_dir)
        .expect("content store should be readable")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !content_files.is_empty(),
        "content store should contain at least one file"
    );

    let gateway_log = std::fs::read_to_string(
        gateway_dir.join("history").join("causal_chain.jsonl"),
    )
    .expect("gateway causal log should exist");
    assert!(gateway_log.contains(session_id));
    assert!(gateway_log.contains("\"action\":\"event.ingest.requested\""));
    assert!(gateway_log.contains("\"action\":\"event.ingest.completed\""));

    let agent_log = std::fs::read_to_string(
        agents_dir
            .join(agent_id)
            .join("history")
            .join("causal_chain.jsonl"),
    )
    .expect("agent causal log should exist");
    assert!(agent_log.contains(session_id));
    assert!(agent_log.contains("\"tool_name\":\"content.write\""));
    assert!(agent_log.contains("\"tool_name\":\"content.read\""));

    let request_dump = captured_bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::to_string(body).expect("request body should encode"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(request_dump.contains(channel_id));
    assert!(request_dump.contains("sender_id"));
    assert!(request_dump.contains("tester"));
    assert!(request_dump.contains(session_id));
}

#[test]
#[ignore = "full-stack chat e2e; spawns gateway+LLM stub, drifts as pipeline evolves — run with --ignored"]
fn test_terminal_chat_surfaces_gateway_backpressure_errors() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config_path = temp.path().join("config.yaml");
    let agents_dir = temp.path().join("agents");
    let agent_id = "memory_chat";
    let jsonrpc_port = pick_unused_port();
    let ofp_port = pick_unused_port();
    write_config(&config_path, &agents_dir, jsonrpc_port, ofp_port, 1);
    write_memory_agent(&agents_dir.join(agent_id), agent_id);

    let captured_bodies = Arc::new(Mutex::new(Vec::new()));
    let stub_addr = spawn_openai_stub(captured_bodies.clone());
    let config_arg = config_path.to_string_lossy().to_string();
    let stub_url = format!("http://{}/v1/chat/completions", stub_addr);
    let gateway_env = [
        ("AUTONOETIC_NODE_ID", "test-gateway"),
        ("AUTONOETIC_NODE_NAME", "Test Gateway"),
        ("AUTONOETIC_SHARED_SECRET", "test-secret"),
        ("AUTONOETIC_LLM_BASE_URL", stub_url.as_str()),
        ("AUTONOETIC_LLM_API_KEY", "test-key"),
    ];
    let gateway_args = ["--config", config_arg.as_str(), "gateway", "start"];
    let _gateway = spawn_autonoetic(&gateway_args, &gateway_env, false, false);
    wait_for_port(
        format!("127.0.0.1:{}", jsonrpc_port)
            .parse()
            .expect("gateway addr should parse"),
        Duration::from_secs(5),
    );

    let mut slow_chat = spawn_autonoetic(
        &[
            "--config",
            config_arg.as_str(),
            "chat",
            agent_id,
            "--session-id",
            "terminal-session-slow",
            "--test-mode",
        ],
        &[],
        true,
        true,
    );
    slow_chat
        .stdin_mut()
        .write_all(b"delay message\n/exit\n")
        .expect("slow chat stdin should write");

    // Wait for the slow chat to reach the stub and occupy the gateway's pending execution slot.
    let start = Instant::now();
    while captured_bodies.lock().unwrap().is_empty() {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("Timed out waiting for slow chat to reach LLM stub during backpressure test");
        }
        thread::sleep(Duration::from_millis(10));
    }

    let fast_chat = run_autonoetic(
        &[
            "--config",
            config_arg.as_str(),
            "chat",
            agent_id,
            "--session-id",
            "terminal-session-fast",
            "--test-mode",
        ],
        Some("please store this data\n/exit\n"),
    );
    assert!(
        !fast_chat.status.success(),
        "expected backpressure failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&fast_chat.stdout),
        String::from_utf8_lossy(&fast_chat.stderr)
    );
    let stderr = String::from_utf8_lossy(&fast_chat.stderr);
    assert!(
        stderr.contains("pending execution queue is full"),
        "expected gateway backpressure error, got stderr:\n{}",
        stderr
    );

    let slow_output = slow_chat.wait_with_output();
    assert!(
        slow_output.status.success(),
        "slow chat should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&slow_output.stdout),
        String::from_utf8_lossy(&slow_output.stderr)
    );
}

#[test]
#[ignore = "full-stack chat e2e; spawns gateway+LLM stub, drifts as pipeline evolves — run with --ignored"]
fn test_terminal_chat_repairs_invalid_agent_install_in_session() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config_path = temp.path().join("config.yaml");
    let agents_dir = temp.path().join("agents");
    // Use specialized_builder.default which is an evolution role with agent.install permission
    let agent_id = "specialized_builder.default";
    let jsonrpc_port = pick_unused_port();
    let ofp_port = pick_unused_port();
    write_config(&config_path, &agents_dir, jsonrpc_port, ofp_port, 4);
    write_builder_agent(&agents_dir.join(agent_id), agent_id);

    let captured_bodies = Arc::new(Mutex::new(Vec::new()));
    let stub_addr = spawn_openai_stub(captured_bodies.clone());
    let config_arg = config_path.to_string_lossy().to_string();
    let stub_url = format!("http://{}/v1/chat/completions", stub_addr);
    let gateway_env = [
        ("AUTONOETIC_NODE_ID", "test-gateway"),
        ("AUTONOETIC_NODE_NAME", "Test Gateway"),
        ("AUTONOETIC_SHARED_SECRET", "test-secret"),
        ("AUTONOETIC_LLM_BASE_URL", stub_url.as_str()),
        ("AUTONOETIC_LLM_API_KEY", "test-key"),
    ];
    let gateway_args = ["--config", config_arg.as_str(), "gateway", "start"];
    let _gateway = spawn_autonoetic(&gateway_args, &gateway_env, false, false);
    wait_for_port(
        format!("127.0.0.1:{}", jsonrpc_port)
            .parse()
            .expect("gateway addr should parse"),
        Duration::from_secs(5),
    );

    let chat = run_autonoetic(
        &[
            "--config",
            config_arg.as_str(),
            "chat",
            agent_id,
            "--session-id",
            "terminal-session-repair",
            "--test-mode",
        ],
        Some("repair invalid agent install\n/exit\n"),
    );
    assert!(
        chat.status.success(),
        "chat failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&chat.stdout),
        String::from_utf8_lossy(&chat.stderr)
    );

    let stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(
        stdout.contains("Installed repair_worker after retry."),
        "expected repair completion reply, got stdout:\n{}",
        stdout
    );

    let installed_worker = agents_dir.join("repair_worker");
    assert!(
        installed_worker.join("SKILL.md").exists(),
        "expected repaired install to create child worker SKILL.md"
    );
    assert!(
        installed_worker.join("state").join("seed.txt").exists(),
        "expected repaired install to create declared worker file"
    );

    let captured = captured_bodies.lock().unwrap();
    let saw_validation_feedback = captured.iter().any(|body| {
        body.get("messages")
            .and_then(|value| value.as_array())
            .map(|messages| {
                messages.iter().any(|message| {
                    message.get("role").and_then(|value| value.as_str()) == Some("tool")
                        && message
                            .get("content")
                            .and_then(|value| value.as_str())
                            .map(|content| {
                                content.contains("\"error_type\":\"validation\"")
                                    || content.contains("agent_id must not be empty")
                            })
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
    let request_dump = captured
        .iter()
        .map(|body| serde_json::to_string(body).expect("request body should encode"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        saw_validation_feedback,
        "expected validation tool_result to be fed back to model, got requests:\n{}",
        request_dump
    );
    assert!(
        request_dump.contains("call_install_invalid"),
        "expected first invalid install tool call"
    );
    assert!(
        request_dump.contains("call_install_corrected"),
        "expected corrected install tool call"
    );
}

#[test]
#[ignore = "full-stack chat e2e; spawns gateway+LLM stub, drifts as pipeline evolves — run with --ignored"]
fn test_terminal_chat_implicit_routing_to_planner_and_specialist_spawn() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config_path = temp.path().join("config.yaml");
    let agents_dir = temp.path().join("agents");
    let jsonrpc_port = pick_unused_port();
    let ofp_port = pick_unused_port();
    write_config(&config_path, &agents_dir, jsonrpc_port, ofp_port, 4);

    // Force explicit default lead to avoid ambiguity in this regression.
    let config_body = std::fs::read_to_string(&config_path).expect("config should read");
    std::fs::write(&config_path, format!("{config_body}port: 4000\n"))
        .expect("config should update");

    write_planner_agent(&agents_dir.join("planner.default"), "planner.default");
    write_researcher_agent(&agents_dir.join("researcher.default"), "researcher.default");

    let captured_bodies = Arc::new(Mutex::new(Vec::new()));
    let stub_addr = spawn_openai_stub(captured_bodies.clone());
    let config_arg = config_path.to_string_lossy().to_string();
    let stub_url = format!("http://{}/v1/chat/completions", stub_addr);
    let gateway_env = [
        ("AUTONOETIC_NODE_ID", "test-gateway"),
        ("AUTONOETIC_NODE_NAME", "Test Gateway"),
        ("AUTONOETIC_SHARED_SECRET", "test-secret"),
        ("AUTONOETIC_LLM_BASE_URL", stub_url.as_str()),
        ("AUTONOETIC_LLM_API_KEY", "test-key"),
    ];
    let gateway_args = ["--config", config_arg.as_str(), "gateway", "start"];
    let _gateway = spawn_autonoetic(&gateway_args, &gateway_env, false, false);
    wait_for_port(
        format!("127.0.0.1:{}", jsonrpc_port)
            .parse()
            .expect("gateway addr should parse"),
        Duration::from_secs(5),
    );

    let session_id = "terminal-session-planner-spawn";
    let chat = run_autonoetic(
        &[
            "--config",
            config_arg.as_str(),
            "chat",
            "--sender-id",
            "tester",
            "--session-id",
            session_id,
            "--test-mode",
        ],
        Some("delegate to specialist\n/exit\n"),
    );
    assert!(
        chat.status.success(),
        "chat failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&chat.stdout),
        String::from_utf8_lossy(&chat.stderr)
    );
    let stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(
        stdout.contains("Delegated to researcher.default"),
        "expected planner delegation reply, got stdout:\n{}",
        stdout
    );

    let planner_log = std::fs::read_to_string(
        agents_dir
            .join("planner.default")
            .join("history")
            .join("causal_chain.jsonl"),
    )
    .expect("planner causal log should exist");
    assert!(planner_log.contains(session_id));
    assert!(planner_log.contains("\"tool_name\":\"agent.spawn\""));

    let researcher_log = std::fs::read_to_string(
        agents_dir
            .join("researcher.default")
            .join("history")
            .join("causal_chain.jsonl"),
    )
    .expect("researcher causal log should exist");
    assert!(researcher_log.contains(session_id));

    let request_dump = captured_bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| serde_json::to_string(body).expect("request body should encode"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(request_dump.contains("delegate to specialist"));
}

#[test]
fn trace_digest_prints_post_session_narrative() {
    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir");
    let config_path = temp.path().join("config.yaml");
    write_config(&config_path, &agents_dir, 4011, 4211, 4);

    let gateway_dir = temp.path().join("runtime");
    let cs =
        autonoetic_gateway::runtime::content_store::ContentStore::new(&gateway_dir).expect("store");
    let root = "cli-trace-digest-root";
    let narrative = b"## E2E narrative\nvisible from trace digest CLI.\n";
    let handle = cs.write(narrative).expect("write narrative");
    cs.register_name(
        root,
        autonoetic_gateway::runtime::post_session_digest::POST_SESSION_NARRATIVE_CONTENT_NAME,
        &handle,
    )
    .expect("register narrative name");

    let out = run_autonoetic(
        &[
            "--config",
            config_path.to_str().expect("utf8 path"),
            "trace",
            "digest",
            root,
        ],
        None,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("E2E narrative"),
        "expected narrative in stdout, got:\n{stdout}"
    );
}

/// #722 Stage 3: `gateway pending` lists a seeded approval for the root session
/// (JSON), and reports an empty queue for an unrelated root.
#[test]
fn gateway_pending_lists_unified_queue() {
    use autonoetic_types::background::{ApprovalLevel, ApprovalRequest, ScheduledAction};

    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir");
    let config_path = temp.path().join("config.yaml");
    write_config(&config_path, &agents_dir, 4013, 4213, 4);

    let gateway_dir = temp.path().join("runtime");
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
        .expect("store opens");

    let root = "cli-pending-root";
    let mut approval = ApprovalRequest {
        request_id: "apr-cli-1".to_string(),
        agent_id: "researcher.default".to_string(),
        session_id: root.to_string(),
        action: ScheduledAction::WebFetch {
            url: "https://example.org/data".to_string(),
            timeout_secs: None,
            max_chars: None,
            detected_hosts: Some(vec!["example.org".to_string()]),
            payload: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: Some("fetch a dataset".to_string()),
        evidence_ref: None,
        workflow_id: None,
        task_id: None,
        root_session_id: Some(root.to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    store.create_approval(&mut approval).expect("seed approval");

    // `gateway pending` reads from a running gateway's JSON-RPC since #1119 —
    // serve this store over the configured port for the CLI invocations.
    let _server = serve_gateway_for_test(&agents_dir, 4013, Arc::new(store));

    // Owning root: JSON output should list the approval.
    let out = run_autonoetic(
        &[
            "--config",
            config_path.to_str().expect("utf8 path"),
            "gateway",
            "pending",
            "--root-session",
            root,
            "--json",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("pending --json emits valid JSON");
    let arr = parsed.as_array().expect("pending JSON is an array");
    assert_eq!(arr.len(), 1, "one pending item, got:\n{stdout}");
    assert_eq!(arr[0]["kind"], "approval");
    assert_eq!(arr[0]["id"], "apr-cli-1");
    assert_eq!(arr[0]["answer"]["method"], "approvals.approve");

    // Unrelated root: empty queue.
    let out2 = run_autonoetic(
        &[
            "--config",
            config_path.to_str().expect("utf8 path"),
            "gateway",
            "pending",
            "--root-session",
            "some-other-root",
        ],
        None,
    );
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("No pending operator decisions"),
        "expected empty message, got:\n{stdout2}"
    );
}

/// `agent revision list` — candidates were invisible outside the TUI until now:
/// the CLI had only `create` and `promote`, so an operator could not see what the
/// promotion gate was holding without opening the session room (#818).
#[test]
fn agent_revision_list_filters_by_status_and_reports_truncation() {
    use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
    use autonoetic_types::principal::PrincipalKind;

    let temp = tempfile::tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).expect("agents dir");
    let config_path = temp.path().join("config.yaml");
    write_config(&config_path, &agents_dir, 4017, 4217, 4);

    let gateway_dir = temp.path().join("runtime");
    let store = autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
        .expect("store opens");

    let seed = |revision_id: &str, agent_id: &str, status: AgentRevisionStatus, created: &str| {
        let rec = AgentRevisionRecord {
            revision_id: revision_id.to_string(),
            agent_id: agent_id.to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: format!("sha256:{revision_id}"),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: created.to_string(),
            created_by_type: PrincipalKind::AutonoeticAgent.tag().to_string(),
            created_by_id: "specialized_builder.default".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "test".to_string(),
            source_ref: None,
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status,
            metadata_json: serde_json::json!({}),
            short_id: revision_id.chars().rev().take(8).collect(),
            detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store.insert_agent_revision(&rec).expect("seed revision");
    };

    seed(
        "rev_sha256:aaaa000000000000000000000000000000000000000000000000000000000001",
        "batch-fetcher.default",
        AgentRevisionStatus::Candidate,
        "2026-07-25T10:00:00Z",
    );
    seed(
        "rev_sha256:bbbb000000000000000000000000000000000000000000000000000000000002",
        "coder.default",
        AgentRevisionStatus::Ready,
        "2026-07-25T09:00:00Z",
    );

    let cfg = config_path.to_str().expect("utf8 path");

    // Unfiltered: both revisions, and the human view points at the useful filter.
    let out = run_autonoetic(&["--config", cfg, "agent", "revision", "list"], None);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "list failed:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("Candidate"), "got:\n{stdout}");
    assert!(stdout.contains("Ready"), "got:\n{stdout}");
    assert!(
        stdout.contains("--status candidate"),
        "human output should point at the candidate filter, got:\n{stdout}"
    );

    // Filtered to candidates: the Ready revision must not appear.
    let out = run_autonoetic(
        &[
            "--config",
            cfg,
            "agent",
            "revision",
            "list",
            "--status",
            "candidate",
            "--json",
        ],
        None,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "filtered list failed: {stdout}");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json output");
    let rows = parsed["revisions"].as_array().expect("revisions array");
    assert_eq!(rows.len(), 1, "only the candidate should match: {stdout}");
    assert_eq!(rows[0]["agent_id"], "batch-fetcher.default");
    assert_eq!(rows[0]["status"], "Candidate");

    // Case-insensitive: an operator types lowercase, the record says `Candidate`.
    let out = run_autonoetic(
        &[
            "--config",
            cfg,
            "agent",
            "revision",
            "list",
            "--status",
            "CANDIDATE",
            "--json",
        ],
        None,
    );
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).expect("json");
    assert_eq!(parsed["revisions"].as_array().map(|r| r.len()), Some(1));

    // Truncation is stated, not silent — a clipped list that reads as complete
    // is how an operator misses the thing waiting on them.
    let out = run_autonoetic(
        &["--config", cfg, "agent", "revision", "list", "--limit", "1"],
        None,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 of 2 shown"),
        "truncation should be reported, got:\n{stdout}"
    );

    // `--limit 0` is rejected at the argument layer rather than silently printing
    // "no revisions" while two exist (#890 review).
    let out = run_autonoetic(
        &["--config", cfg, "agent", "revision", "list", "--limit", "0"],
        None,
    );
    assert!(!out.status.success(), "--limit 0 should be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid value") || stderr.contains("0"),
        "rejection should explain itself, got:\n{stderr}"
    );

    // An empty result says so rather than printing a bare header.
    let out = run_autonoetic(
        &[
            "--config", cfg, "agent", "revision", "list", "--status", "rejected",
        ],
        None,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No revisions with status 'rejected'"),
        "got:\n{stdout}"
    );
}

#[test]
fn test_egress_declassify_intake_file_then_approve() {
    let temp = tempfile::tempdir().expect("tempdir should create");
    let config_path = temp.path().join("config.yaml");
    let agents_dir = temp.path().join("agents");
    write_config(&config_path, &agents_dir, 4010, 4210, 4);
    let cfg = config_path.to_string_lossy().to_string();

    // File: leaves a pending approval, no grant materialized.
    let file = run_autonoetic(
        &[
            "--config", cfg.as_str(),
            "gateway", "egress-declassify",
            "--root-session", "root-decl",
            "--target", "source_pattern:session:root-decl:host:example.com",
            "--sink", "network",
            "--reason", "allow fetch to example.com",
        ],
        None,
    );
    assert!(
        file.status.success(),
        "egress-declassify failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&file.stdout),
        String::from_utf8_lossy(&file.stderr)
    );
    let stdout = String::from_utf8_lossy(&file.stdout);
    assert!(
        stdout.contains("Filed declassification request") && stdout.contains("pending"),
        "got:\n{stdout}"
    );
    let request_id = stdout
        .split_whitespace()
        .find(|w| w.starts_with("apr-"))
        .expect("filed output should name the request id")
        .to_string();

    // The pending request shows up in the unified pending view (served over
    // JSON-RPC since #1119 — spawn a gateway on this port over the store the
    // CLI just wrote).
    let gateway_dir = test_gateway_dir(&agents_dir);
    let store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
            .expect("store opens"),
    );
    let _server = serve_gateway_for_test(&agents_dir, 4010, store);
    let pending = run_autonoetic(
        &[
            "--config", cfg.as_str(),
            "gateway", "pending", "--root-session", "root-decl", "--json",
        ],
        None,
    );
    assert!(pending.status.success());
    let stdout = String::from_utf8_lossy(&pending.stdout);
    assert!(
        stdout.contains("egress_declassify"),
        "pending should list the declassify request, got:\n{stdout}"
    );

    // Decide it through the normal approval surface. EgressDeclassify is a
    // high-risk class — the P-2.24 dwell (3s) must elapse between filing and
    // decision.
    std::thread::sleep(Duration::from_millis(3200));
    let approve = run_autonoetic(
        &[
            "--config", cfg.as_str(),
            "gateway", "approvals", "approve", &request_id,
            "--reason", "confirmed widen",
        ],
        None,
    );
    assert!(
        approve.status.success(),
        "approvals approve failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&approve.stdout),
        String::from_utf8_lossy(&approve.stderr)
    );

    // The grant is live: audit shows egress.declassified with the host target.
    let audit = run_autonoetic(
        &[
            "--config", cfg.as_str(),
            "gateway", "egress-audit", "root-decl", "--json",
        ],
        None,
    );
    assert!(audit.status.success());
    let stdout = String::from_utf8_lossy(&audit.stdout);
    assert!(
        stdout.contains("egress.declassified")
            && stdout.contains("session:root-decl:host:example.com"),
        "audit should render the declassification, got:\n{stdout}"
    );

    // And the grant is revocable through the shared grants surface.
    let revoke = run_autonoetic(
        &[
            "--config", cfg.as_str(),
            "gateway", "grants", "revoke", "root-decl",
            "--host", "example.com",
        ],
        None,
    );
    assert!(
        revoke.status.success(),
        "grants revoke failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&revoke.stdout),
        String::from_utf8_lossy(&revoke.stderr)
    );
    let stdout = String::from_utf8_lossy(&revoke.stdout);
    assert!(
        stdout.contains("egress declassification"),
        "revoke should report the declassification grant, got:\n{stdout}"
    );

    // Invalid target kind is rejected with a helpful error.
    let bad = run_autonoetic(
        &[
            "--config", cfg.as_str(),
            "gateway", "egress-declassify",
            "--root-session", "root-decl",
            "--target", "bogus:thing",
            "--sink", "network",
        ],
        None,
    );
    assert!(!bad.status.success(), "invalid target kind must fail");
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("invalid --target kind"),
        "rejection should explain itself, got:\n{stderr}"
    );

    // A --session outside the declared root is rejected: pending surfaces
    // derive the root from the session id, so a mismatched pair would orphan
    // the request.
    let bad_session = run_autonoetic(
        &[
            "--config", cfg.as_str(),
            "gateway", "egress-declassify",
            "--root-session", "root-decl",
            "--session", "other-root/sess",
            "--target", "memory_id:mem-1",
            "--sink", "remote_model",
        ],
        None,
    );
    assert!(
        !bad_session.status.success(),
        "out-of-root --session must fail"
    );
    let stderr = String::from_utf8_lossy(&bad_session.stderr);
    assert!(
        stderr.contains("not under root session"),
        "rejection should explain itself, got:\n{stderr}"
    );
}
