//! Fix-now security core of the "Agent Genesis — One Door" RFC
//! (docs/design/agent-genesis-one-door.md, Parts A/B/D/E.1):
//!
//! - `skill_install` always lands a **Candidate** revision and never
//!   promotes/moves an alias (one door — issue #793).
//! - The revision carries durable import provenance: `source_kind`,
//!   `source_ref` (url + sha256 of the fetched bytes), and `created_by_*`
//!   naming the installing agent, plus an `agent_install`/`skill_imported`
//!   causal event (issue #796).
//! - `execution_mode: script` imports are rejected before any disk write
//!   (issue #797).
//! - `trust_mode: strict` drops high-risk capabilities that were *inferred*
//!   from `allowed-tools` (not explicitly declared); `generous` keeps them
//!   (issue #794).
//!
//! All tests in this file share one `TestWorkspace` (see `workspace()`) and
//! run `#[serial]`: `constitution_digest::initialize_constitution` panics if
//! re-initialized with a different `gateway_dir` in the same process, so a
//! fresh tempdir per test is not an option here — every scenario below uses a
//! distinct `agent_id` instead to avoid collisions on the shared workspace.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::agent_revision::AgentRevisionStatus;
use autonoetic_types::capability::Capability;
use serial_test::serial;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, OnceLock};
use std::thread;

const INSTALLER_ID: &str = "installer-test-agent";

fn workspace() -> &'static support::TestWorkspace {
    static WORKSPACE: OnceLock<support::TestWorkspace> = OnceLock::new();
    WORKSPACE.get_or_init(|| support::TestWorkspace::new().expect("workspace should create"))
}

fn installer_manifest(capabilities: Vec<Capability>) -> AgentManifest {
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
            id: INSTALLER_ID.to_string(),
            name: INSTALLER_ID.to_string(),
            description: "skill_install test caller".to_string(),
            singleton: false,
        },
        capabilities,
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
    }
}

/// One-shot HTTP server serving `body` for a single request, on 127.0.0.1.
fn spawn_one_shot_http_server(body: String) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should expose local addr");
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buf = [0_u8; 4096];
            let _ = stream.read(&mut request_buf);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/markdown\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), handle)
}

fn remote_skill(execution_mode: &str, capabilities_yaml: &str, extra: &str) -> String {
    format!(
        "---\nversion: \"1.0\"\nruntime:\n  engine: \"autonoetic\"\n  gateway_version: \"0.1.0\"\n  sdk_version: \"0.1.0\"\n  type: \"stateful\"\n  sandbox: \"bubblewrap\"\n  runtime_lock: \"runtime.lock\"\nagent:\n  id: \"placeholder\"\n  name: \"placeholder\"\n  description: \"remote skill under test\"\nexecution_mode: {execution_mode}\ncapabilities:\n{capabilities_yaml}\n{extra}---\n# Remote skill body\n\nDo the thing.\n"
    )
}

fn declared_network_skill() -> String {
    remote_skill(
        "reasoning",
        "  - type: NetworkAccess\n    hosts: [\"example.com\"]\n",
        "",
    )
}

fn inferred_high_risk_skill() -> String {
    remote_skill(
        "reasoning",
        "  []\n",
        "agentskills_import:\n  allowed_tools: [\"Bash\", \"WebFetch\"]\n  needs_tool_bridging: false\n",
    )
}

fn script_mode_skill() -> String {
    remote_skill("script", "  []\n", "script_entry: \"main.py\"\n")
}

/// Run `skill_install` through the native tool registry (the real dispatch
/// path) against the shared workspace, returning the parsed JSON response,
/// the gateway config, and the opened store for further assertions.
fn run_skill_install(
    new_agent_id: &str,
    skill_body: String,
    trust_mode: Option<&str>,
) -> (serde_json::Value, Arc<GatewayStore>) {
    let (base_url, handle) = spawn_one_shot_http_server(skill_body);
    let url = format!("{base_url}/SKILL.md");
    let out = run_skill_install_url(new_agent_id, &url, trust_mode);
    handle.join().expect("mock server thread should join");
    out
}

/// Same dispatch path as `run_skill_install` but against a caller-provided
/// URL, without a mock server — for requests the gateway must reject before
/// any fetch happens.
fn run_skill_install_url(
    new_agent_id: &str,
    url: &str,
    trust_mode: Option<&str>,
) -> (serde_json::Value, Arc<GatewayStore>) {
    let manifest = installer_manifest(vec![Capability::SkillInstall {
        allowed_sources: vec!["127.0.0.1".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let ws = workspace();
    let mut config = ws.gateway_config();
    // The repo-tip constitution (2026.07.08) currently has markdown newer
    // than its signed lock (re-signing happens on the machine holding the
    // key), and `bootstrap_constitution_snapshot` fail-closes on that
    // mismatch. Pin this suite to the newest self-consistent signed version
    // so the one-door semantics are observable green regardless of the tip
    // lock's signing state. Constitution *content* is irrelevant to these
    // tests; only lock integrity is exercised.
    config.constitution.source_path =
        "docs/constitution/versions/2026.07.02/constitution.md".into();
    config.constitution.lock_path =
        "docs/constitution/versions/2026.07.02/gateway-constitution.lock.json".into();
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    let caller_dir = config.agents_dir.join(INSTALLER_ID);
    std::fs::create_dir_all(&caller_dir).expect("caller dir should create");

    let mut args = serde_json::json!({
        "url": url,
        "agent_id": new_agent_id,
    });
    if let Some(tm) = trust_mode {
        args["trust_mode"] = serde_json::json!(tm);
    }

    let gateway_store = Arc::new(GatewayStore::open(&gateway_dir).expect("store should open"));

    let result = registry
        .execute(
            "skill_install",
            &manifest,
            &policy,
            &caller_dir,
            Some(&gateway_dir),
            &args.to_string(),
            Some("root-test"),
            None,
            Some(&config),
            Some(gateway_store.clone()),
            None,
        )
        .expect("skill_install should return a response");

    let parsed: serde_json::Value =
        serde_json::from_str(&result).expect("skill_install response should be JSON");
    (parsed, gateway_store)
}

/// (a) One door: a declared-NetworkAccess skill installed via `generous`
/// lands as a Candidate — never promoted, alias untouched.
#[serial]
#[test]
fn one_door_generous_install_stays_candidate_and_unpromoted() {
    let agent_id = "genesis-one-door-generous.default";
    let (resp, store) = run_skill_install(agent_id, declared_network_skill(), Some("generous"));

    assert_eq!(resp["ok"], serde_json::json!(true), "resp: {resp}");
    assert_eq!(resp["activated"], serde_json::json!(false), "resp: {resp}");
    assert_eq!(resp["status"], serde_json::json!("candidate"), "resp: {resp}");
    let revision_id = resp["revision_id"]
        .as_str()
        .expect("revision_id should be a string")
        .to_string();
    assert!(!revision_id.is_empty());

    let rev = store
        .get_agent_revision(&revision_id)
        .expect("query should succeed")
        .expect("revision should exist");
    assert_eq!(rev.status, AgentRevisionStatus::Candidate);

    let alias = store
        .get_agent_alias(agent_id)
        .expect("alias query should succeed");
    assert!(
        alias.is_none(),
        "skill_install must never move an alias — one door (RFC Part A)"
    );
}

/// (b) Provenance: source_kind/source_ref/created_by_id on the revision, and
/// an `agent_install`/`skill_imported` causal event, both durable.
#[serial]
#[test]
fn provenance_recorded_on_revision_and_causal_event() {
    let agent_id = "genesis-one-door-provenance.default";
    let (resp, store) = run_skill_install(agent_id, declared_network_skill(), Some("generous"));

    let revision_id = resp["revision_id"].as_str().unwrap().to_string();
    let rev = store
        .get_agent_revision(&revision_id)
        .unwrap()
        .expect("revision should exist");

    assert_eq!(rev.source_kind, "skill_install");
    let source_ref = rev.source_ref.expect("source_ref should be set");
    assert!(
        source_ref.contains("/SKILL.md"),
        "source_ref should contain the fetch url, got: {source_ref}"
    );
    assert!(
        source_ref.contains("#sha256="),
        "source_ref should carry a sha256 fragment, got: {source_ref}"
    );
    assert_eq!(
        rev.created_by_id, INSTALLER_ID,
        "created_by_id should be the installing agent's id, not the generic 'cli'"
    );
    assert_eq!(rev.created_by_type, "autonoetic_agent");

    let events = store
        .search_causal_events(None, Some(INSTALLER_ID), 200)
        .expect("causal event search should succeed");
    let event = events
        .iter()
        .find(|e| e.category == "agent_install" && e.action == "skill_imported" && e.target.as_deref() == Some(agent_id))
        .unwrap_or_else(|| panic!("expected agent_install/skill_imported causal event, got: {events:?}"));

    let payload: serde_json::Value =
        serde_json::from_str(event.payload.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(payload["agent_id"], serde_json::json!(agent_id));
    assert_eq!(payload["trust_mode"], serde_json::json!("generous"));
    assert!(payload["sha256"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(payload["url"].as_str().unwrap().contains("/SKILL.md"));
}

/// Transport safety (#802 review): a non-loopback plain-HTTP URL is
/// rejected before any fetch or disk write — a remote SKILL.md is a whole
/// agent definition, so plaintext transport would allow MITM substitution.
#[serial]
#[test]
fn remote_plain_http_rejected_before_fetch() {
    let agent_id = "genesis-one-door-http-rejected.default";
    let (resp, _store) =
        run_skill_install_url(agent_id, "http://skills.example.com/SKILL.md", None);

    assert_eq!(resp["ok"], serde_json::json!(false), "resp: {resp}");
    assert_eq!(
        resp["error"],
        serde_json::json!("skill_install_insecure_scheme"),
        "resp: {resp}"
    );

    let ws = workspace();
    let target_dir = ws.agents_dir.join(agent_id.replace('.', "-"));
    assert!(
        !target_dir.exists(),
        "scheme rejection must happen before any disk write, found: {}",
        target_dir.display()
    );
}

/// (c) Script-mode imports are rejected before any disk write.
#[serial]
#[test]
fn script_mode_import_rejected_before_disk_write() {
    let agent_id = "genesis-one-door-script-rejected.default";
    let (resp, _store) = run_skill_install(agent_id, script_mode_skill(), None);

    assert_eq!(resp["ok"], serde_json::json!(false), "resp: {resp}");
    assert_eq!(
        resp["error"],
        serde_json::json!("skill_install_script_mode_rejected"),
        "resp: {resp}"
    );

    let ws = workspace();
    let dir_name = agent_id.replace('.', "-");
    let target_dir = ws.agents_dir.join(&dir_name);
    assert!(
        !target_dir.exists(),
        "script-mode rejection must happen before any disk write, found: {}",
        target_dir.display()
    );
}

/// (d) `strict` drops inferred high-risk capabilities (`CodeExecution`,
/// `NetworkAccess`); `generous` keeps them. Neither mode ever promotes
/// (covered by the one-door test above).
#[serial]
#[test]
fn strict_drops_inferred_high_risk_capabilities_generous_keeps_them() {
    let strict_id = "genesis-one-door-strict-clamp.default";
    let (strict_resp, strict_store) = run_skill_install(strict_id, inferred_high_risk_skill(), None);
    assert_eq!(strict_resp["trust_mode"], serde_json::json!("strict"));
    let strict_rev_id = strict_resp["revision_id"].as_str().unwrap().to_string();
    let strict_rev = strict_store.get_agent_revision(&strict_rev_id).unwrap().unwrap();
    let strict_caps: Vec<String> = serde_json::from_value(
        strict_rev.metadata_json["manifest"]["capabilities"].clone(),
    )
    .expect("capabilities should be a string array");
    assert!(
        !strict_caps.contains(&"CodeExecution".to_string()),
        "strict must drop inferred CodeExecution, got: {strict_caps:?}"
    );
    assert!(
        !strict_caps.contains(&"NetworkAccess".to_string()),
        "strict must drop inferred NetworkAccess, got: {strict_caps:?}"
    );
    assert!(
        strict_caps.contains(&"ApprovalQueue".to_string()),
        "strict must still add ApprovalQueue, got: {strict_caps:?}"
    );

    let generous_id = "genesis-one-door-generous-clamp.default";
    let (generous_resp, generous_store) =
        run_skill_install(generous_id, inferred_high_risk_skill(), Some("generous"));
    let generous_rev_id = generous_resp["revision_id"].as_str().unwrap().to_string();
    let generous_rev = generous_store
        .get_agent_revision(&generous_rev_id)
        .unwrap()
        .unwrap();
    let generous_caps: Vec<String> = serde_json::from_value(
        generous_rev.metadata_json["manifest"]["capabilities"].clone(),
    )
    .expect("capabilities should be a string array");
    assert!(
        generous_caps.contains(&"CodeExecution".to_string()),
        "generous must keep inferred CodeExecution, got: {generous_caps:?}"
    );
    assert!(
        generous_caps.contains(&"NetworkAccess".to_string()),
        "generous must keep inferred NetworkAccess, got: {generous_caps:?}"
    );
}

/// A REAL AgentSkills skill: standard frontmatter with a top-level
/// `allowed-tools:` list. The parser pre-infers capabilities INTO
/// `manifest.capabilities` on this path (unlike the native-form fixture
/// above, whose capabilities stay empty), so this pins that strict-mode's
/// clamp keys off the parser-recorded `capabilities_inferred` bit rather
/// than guessing from capability-set emptiness — the exact hole a
/// native-form-only fixture would hide.
fn standard_agentskills_high_risk_skill() -> String {
    "---\nname: \"placeholder\"\ndescription: \"standard-format remote skill under test\"\nallowed-tools: [\"Bash\", \"WebFetch\"]\n---\n# Remote skill body\n\nDo the thing.\n".to_string()
}

#[serial]
#[test]
fn strict_clamp_applies_to_standard_frontmatter_parser_inferred_capabilities() {
    let agent_id = "genesis-one-door-standard-strict.default";
    let (resp, store) = run_skill_install(agent_id, standard_agentskills_high_risk_skill(), None);
    assert_eq!(resp["trust_mode"], serde_json::json!("strict"));
    let rev_id = resp["revision_id"].as_str().unwrap().to_string();
    let rev = store.get_agent_revision(&rev_id).unwrap().unwrap();
    let caps: Vec<String> =
        serde_json::from_value(rev.metadata_json["manifest"]["capabilities"].clone())
            .expect("capabilities should be a string array");
    assert!(
        !caps.contains(&"CodeExecution".to_string()),
        "strict must drop parser-inferred CodeExecution (standard frontmatter), got: {caps:?}"
    );
    assert!(
        !caps.contains(&"NetworkAccess".to_string()),
        "strict must drop parser-inferred NetworkAccess (standard frontmatter), got: {caps:?}"
    );
}
