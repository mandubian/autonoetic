//! Gateway-injected credentials must not survive into `execution_traces`.
//!
//! The gateway resolves a `LockedCredentialMount` into the sandbox as an env
//! var, so the *command* stays a reference (`$GITHUB_TOKEN`) and is safe. The
//! process *output* is captured verbatim — and `curl -v`, `set -x`, or a config
//! dump resolves that variable and prints the literal. Without write-time
//! masking the gateway hands out a secret and stores it back in the clear.
//!
//! Unlike an approval's `action_payload`, a trace is a pure record: nothing
//! executes it. So the write-time redaction `action_payload` cannot have is
//! available here, applied at the single write chokepoint.

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::causal_chain::ExecutionTraceRecord;

const INJECTED: &str = "ghp_16C7e42F292c6912E7710c838347Ae178B4a";

fn trace_with(stdout: &str, stderr: &str, command: &str) -> ExecutionTraceRecord {
    ExecutionTraceRecord {
        trace_id: format!("tr-{}", uuid::Uuid::new_v4()),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: "root/coder".to_string(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox_exec".to_string(),
        command: Some(command.to_string()),
        exit_code: Some(0),
        stdout: Some(stdout.to_string()),
        stderr: Some(stderr.to_string()),
        duration_ms: 12,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
        egress_label: None,
        mount_set: None,
    }
}

#[test]
fn a_credential_echoed_by_a_verbose_command_is_masked_at_write() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    // What `curl -v` actually prints: the command referenced the env var, the
    // output resolved it.
    let trace = trace_with(
        "* Connected to api.github.com\n> GET /user HTTP/2",
        &format!("> Authorization: Bearer {INJECTED}\n< HTTP/2 200"),
        "curl -v -H \"Authorization: Bearer $GITHUB_TOKEN\" https://api.github.com/user",
    );
    let trace_id = trace.trace_id.clone();
    store.create_execution_trace(&trace)?;

    let stored = store.get_execution_trace(&trace_id)?.expect("trace was written");

    let blob = serde_json::to_string(&stored)?;
    assert!(
        !blob.contains(INJECTED),
        "a gateway-injected credential survived into execution_traces:\n{blob}"
    );

    // Fidelity is why these rows exist untruncated — masking must not gut them.
    let stderr = stored.stderr.as_deref().unwrap_or_default();
    assert!(
        stderr.contains("Authorization") && stderr.contains("HTTP/2 200"),
        "masking destroyed the diagnostic value of the trace: {stderr}"
    );
    assert!(
        stored.stdout.as_deref().unwrap_or_default().contains("Connected to api.github.com"),
        "benign stdout must be untouched"
    );
    // The command referenced the variable, never the literal — unchanged.
    assert!(
        stored.command.as_deref().unwrap_or_default().contains("$GITHUB_TOKEN"),
        "an env-var reference is not a secret and must survive verbatim"
    );
    Ok(())
}

#[test]
fn an_ordinary_trace_is_stored_byte_for_byte() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = GatewayStore::open(&gateway_dir)?;

    let stdout = "PASS tests/test_math.py::test_add\n1 passed in 0.02s";
    let trace = trace_with(stdout, "", "python3 -m pytest -q");
    let trace_id = trace.trace_id.clone();
    store.create_execution_trace(&trace)?;

    let stored = store.get_execution_trace(&trace_id)?.expect("trace was written");
    assert_eq!(stored.stdout.as_deref(), Some(stdout));
    assert_eq!(stored.command.as_deref(), Some("python3 -m pytest -q"));
    Ok(())
}
