//! Issue #857: the per-host probe budget (#853) extended to the read-style web
//! tools.
//!
//! #853 caps an agent re-probing one dead host via `sandbox_exec` (content-aware
//! strikes, refuse the next probe). But its host detection runs static analysis
//! over the *script*, so `web_fetch` and idempotent `web_call` GETs — the same
//! divergence shape against a host — never reached it. These tests drive the
//! real tool-dispatch path (`registry.execute`) to prove:
//!
//! 1. `web_fetch` consults the budget and refuses a probe against an already
//!    exhausted host *before* any network work, with the stable
//!    `host_budget_exhausted` code.
//! 2. `web_call` GET behaves the same (it is a read probe).
//! 3. `web_call` POST is exempt — a mutation is not a probe, so the budget does
//!    not gate it (it proceeds to the network and fails there instead).
//!
//! The pre-seeded strikes stand in for *any* tool's prior probes of the host:
//! the budget is shared, so a `sandbox_exec` (or earlier web) probe run that
//! exhausted the host is what a later `web_fetch` runs into here.

use std::path::Path;
use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::host_probe_budget::content_hash;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::{json, Value};
use tempfile::tempdir;

const SID: &str = "root-1/session-1";
const HOST: &str = "127.0.0.1";
/// A dead port on localhost — connections are refused immediately, so any code
/// path that *reaches* the network fails fast (no DNS, no hanging).
const URL: &str = "http://127.0.0.1:65535/probe";

fn web_manifest(agent_id: &str) -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "web probe-budget test agent".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::NetworkAccess {
            // Declare the exact host: a bare `*` only grants open-web when
            // `open_web: true` (fail-shut), which would defeat the point.
            hosts: vec![HOST.to_string()],
        }],
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        }
}

/// A `remote_access` declaration covering `HOST`, so `enforce_remote_target_for_web`
/// (which runs *before* the probe-budget check) is satisfied and the budget gate
/// is what the test actually exercises.
fn remote_access_skill() -> String {
    format!(
        r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: "required"
      targets:
        - kind: "exact_host"
          value: "{HOST}"
      enabled_languages: []
      python_imports: []
      js_imports: []
      rust_imports: []
      go_imports: []
      function_calls: []
      shell_commands: []
      package_manager_commands: []
---
"#
    )
}

/// A temp agent dir with the SKILL.md declaration, plus a store whose per-host
/// probe budget is already exhausted for `HOST` (via `cap` recorded strikes —
/// the tool that produced them is irrelevant; the budget is shared).
fn exhausted_env(agent_dir: &Path, cap: usize) -> Arc<GatewayStore> {
    std::fs::write(agent_dir.join("SKILL.md"), remote_access_skill()).expect("write skill");
    let store = Arc::new(GatewayStore::open(&agent_dir.join(".gateway")).expect("open store"));
    store.host_probe_budget.set_cap(cap);
    let hash = content_hash("");
    for _ in 0..cap {
        store.host_probe_budget.record(SID, HOST, false, &hash);
    }
    assert_eq!(
        store.host_probe_budget.exhausted(SID, HOST),
        Some(cap as u32),
        "precondition: host must be exhausted before the tool call"
    );
    store
}

/// Run one web tool against `URL`. Returns `Ok(value)` when the tool produced a
/// JSON response string, `Err(msg)` when `execute` returned an error (e.g. a
/// network failure) — mirroring the cross-tool parity test's convention.
fn run_web(
    tool: &str,
    manifest: &AgentManifest,
    agent_dir: &Path,
    store: Arc<GatewayStore>,
    args: Value,
) -> Result<Value, String> {
    let cfg = GatewayConfig::default();
    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    match registry.execute(
        tool,
        manifest,
        &policy,
        agent_dir,
        Some(agent_dir),
        &args.to_string(),
        Some(SID),
        None,
        Some(&cfg),
        Some(store),
        None,
    ) {
        Ok(body) => Ok(serde_json::from_str(&body).expect("tool response is JSON")),
        Err(err) => Err(err.to_string()),
    }
}

fn is_budget_refusal(res: &Result<Value, String>) -> bool {
    matches!(res, Ok(v) if v["ok"] == false && v["error"] == "host_budget_exhausted")
}

#[test]
fn web_fetch_refused_when_host_probe_budget_exhausted() {
    let temp = tempdir().expect("tempdir");
    let store = exhausted_env(temp.path(), 3);
    let manifest = web_manifest("web.probe.fetch");

    let res = run_web(
        "web_fetch",
        &manifest,
        temp.path(),
        store,
        json!({ "url": URL, "timeout_secs": 2, "max_chars": 512 }),
    );

    assert!(
        is_budget_refusal(&res),
        "web_fetch must be refused with host_budget_exhausted, got: {res:?}"
    );
    let v = res.unwrap();
    assert_eq!(v["error_type"], "quota_exceeded");
    assert!(
        v["message"].as_str().unwrap_or("").contains(HOST),
        "refusal message should name the exhausted host: {v}"
    );
}

#[test]
fn web_call_get_refused_but_post_exempt() {
    // GET is a read probe → budgeted → refused when the host is exhausted.
    let get_temp = tempdir().expect("tempdir");
    let get_store = exhausted_env(get_temp.path(), 3);
    let get_res = run_web(
        "web_call",
        &web_manifest("web.probe.call.get"),
        get_temp.path(),
        get_store,
        json!({ "url": URL, "method": "GET", "timeout_secs": 2 }),
    );
    assert!(
        is_budget_refusal(&get_res),
        "web_call GET must be refused like web_fetch, got: {get_res:?}"
    );

    // POST is a mutation, not a probe → exempt. With the same exhausted host it
    // must NOT be refused by the budget; it proceeds to the network and fails
    // there (connection refused on the dead port) instead.
    let post_temp = tempdir().expect("tempdir");
    let post_store = exhausted_env(post_temp.path(), 3);
    let post_res = run_web(
        "web_call",
        &web_manifest("web.probe.call.post"),
        post_temp.path(),
        post_store,
        json!({ "url": URL, "method": "POST", "timeout_secs": 2, "body": {"k": "v"} }),
    );
    assert!(
        !is_budget_refusal(&post_res),
        "web_call POST is a mutation and must bypass the probe budget, got: {post_res:?}"
    );
}

#[test]
fn web_fetch_repeated_failures_exhaust_budget() {
    // A fresh budget (not pre-seeded): failures must accumulate strikes on their
    // own, matching #853's "a wasted probe is a failure OR a duplicate success".
    let temp = tempdir().expect("tempdir");
    std::fs::write(temp.path().join("SKILL.md"), remote_access_skill()).expect("skill");
    let store = Arc::new(GatewayStore::open(&temp.path().join(".gateway")).expect("store"));
    store.host_probe_budget.set_cap(2);
    let manifest = web_manifest("web.probe.fail");

    let fetch = || {
        run_web(
            "web_fetch",
            &manifest,
            temp.path(),
            store.clone(),
            json!({ "url": URL, "timeout_secs": 2, "max_chars": 512 }),
        )
    };

    // Two failing fetches (dead port → connection refused) are wasted probes:
    // each strikes the host, but surfaces as a network error, not a refusal.
    for i in 1..=2 {
        let res = fetch();
        assert!(
            matches!(res, Err(_)),
            "failing fetch {i} should surface as a network error, got: {res:?}"
        );
        assert!(!is_budget_refusal(&res));
    }
    // The host has now struck up to the cap → the next probe is refused before
    // any network work, proving failed web probes feed the shared per-host budget.
    let refused = fetch();
    assert!(
        is_budget_refusal(&refused),
        "after cap failed probes the next fetch must be refused, got: {refused:?}"
    );
}

/// End-to-end through a real local HTTP server: proves the *record* path is
/// wired (a successful `web_fetch` feeds the budget with a hash of its body) and
/// that the budget is genuinely **content-aware**, not a blunt `(tool, host)`
/// counter. A stream of novel responses against one host is never refused; only
/// once the same content repeats to the cap does the next probe get refused.
#[test]
fn web_fetch_content_aware_budget_via_real_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    let body = Arc::new(Mutex::new(String::new()));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    {
        let body = body.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf); // drain the request line/headers
                let b = body.lock().unwrap().clone();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    b.len(),
                    b
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
    }

    let temp = tempdir().expect("tempdir");
    std::fs::write(temp.path().join("SKILL.md"), remote_access_skill()).expect("skill");
    let store = Arc::new(GatewayStore::open(&temp.path().join(".gateway")).expect("store"));
    store.host_probe_budget.set_cap(2);
    let manifest = web_manifest("web.probe.realserver");
    let url = format!("http://127.0.0.1:{port}/probe");

    let fetch = || {
        run_web(
            "web_fetch",
            &manifest,
            temp.path(),
            store.clone(),
            json!({ "url": url.as_str(), "timeout_secs": 5, "max_chars": 4096 }),
        )
    };
    let is_ok = |res: &Result<Value, String>| matches!(res, Ok(v) if v["ok"] == true);

    // Phase 1 — a genuinely new page each call. Content-awareness means every
    // novel response resets the host's strikes, so productive fetching is never
    // refused however many times one host is hit. This is exactly the case a
    // blunt (tool, host) recurrence counter would wrongly trip on.
    for i in 0..6 {
        *body.lock().unwrap() = format!("novel-page-{i}");
        let res = fetch();
        assert!(is_ok(&res), "novel response {i} must succeed, got: {res:?}");
    }

    // Phase 2 — the SPA-shell shape: identical body every call. The switch is
    // novel (reset), then each duplicate strikes; at the cap the next probe is
    // refused before any network work.
    *body.lock().unwrap() = String::from("<html>same SPA shell</html>");
    assert!(is_ok(&fetch()), "switching to a new body is novel → progress");
    assert!(is_ok(&fetch()), "first repeat is a strike, still under cap");
    assert!(is_ok(&fetch()), "second repeat reaches the cap, still returns content");
    let refused = fetch();
    assert!(
        is_budget_refusal(&refused),
        "after cap duplicate responses the next probe must be refused, got: {refused:?}"
    );
}
