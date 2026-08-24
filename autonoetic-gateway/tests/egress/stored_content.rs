//! Phase 3 (#908) stored-content acceptance: memories + execution traces
//! inherit taint, remote recall withholds canaries, local sees them, legacy
//! unlabeled respects fail-closed mode, execution_search filters.

use std::path::PathBuf;
use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::active_execution_registry::{
    ActiveExecutionRegistry, NativeToolRunContext,
};
use autonoetic_gateway::runtime::egress_stored::{
    filter_or_indicate_for_sink, resolve_stored_label, FilteredStoredContent,
};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::egress::{
    EgressConfig, EgressLabel, IndicationVerbosity, NamedEgressLabel, Sink,
};
use autonoetic_types::memory::{MemoryObject, MemoryVisibility};

const CANARY: &str = "CANARY-LOCAL-ONLY-SECRET-908";

fn test_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".into(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".into(),
            gateway_version: "0.1.0".into(),
            sdk_version: "0.1.0".into(),
            runtime_type: "stateful".into(),
            sandbox: "bubblewrap".into(),
            runtime_lock: "runtime.lock".into(),
        },
        agent: AgentIdentity {
            id: "coder.default".into(),
            name: "coder".into(),
            description: "test".into(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![
            Capability::ReadAccess {
                scopes: vec!["*".into()],
            },
            Capability::WriteAccess {
                scopes: vec!["*".into()],
            },
        ],
        ..Default::default()
    }
}

fn run_ctx(sink: Option<Sink>, taint: Option<EgressLabel>) -> NativeToolRunContext {
    NativeToolRunContext {
        registry: ActiveExecutionRegistry::new(),
        root_session_id: "root-908".into(),
        workflow_id: None,
        task_id: None,
        session_id: "root-908/coder".into(),
        agent_id: "coder.default".into(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: None,
        discovered_tools: None,
            annotation_counter: None,
        tool_discovery_catalog: None,
        wake_hint: None,
        wake_hints_map: None,
        egress_taint: taint,
        egress_query_sink: sink,
    }
}

fn cfg_with_legacy(legacy: NamedEgressLabel) -> GatewayConfig {
    let mut c = GatewayConfig::default();
    c.egress = EgressConfig {
        legacy_unlabeled: legacy,
        ..Default::default()
    };
    c
}

#[test]
fn store_inherits_session_taint() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    store.set_session_egress_taint("sess-taint", &EgressLabel::local_only())?;

    let mut mem = MemoryObject::new(
        "mem-tainted".into(),
        "lessons".into(),
        "coder.default".into(),
        "coder.default".into(),
        "session:sess-taint".into(),
        CANARY.into(),
    );
    mem.visibility = MemoryVisibility::Global;
    mem.egress_label = Some(EgressLabel::local_only());
    store.memory_upsert(&mem)?;

    let got = store.memory_get("mem-tainted")?.expect("memory");
    assert_eq!(got.egress_label, Some(EgressLabel::local_only()));
    assert!(got.content.contains(CANARY));
    Ok(())
}

#[test]
fn remote_recall_withholds_canary_local_sees_it() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let mut mem = MemoryObject::new(
        "mem-canary".into(),
        "lessons".into(),
        "coder.default".into(),
        "coder.default".into(),
        "session:s1".into(),
        CANARY.into(),
    );
    mem.visibility = MemoryVisibility::Global;
    mem.egress_label = Some(EgressLabel::local_only());
    store.memory_upsert(&mem)?;

    let label = resolve_stored_label(mem.egress_label.as_ref(), &EgressConfig::default());
    let remote = filter_or_indicate_for_sink(
        &mem.content,
        &label,
        Sink::RemoteModel,
        Some("knowledge_recall"),
        IndicationVerbosity::Descriptive,
    );
    match remote {
        FilteredStoredContent::Withheld { indication } => {
            assert!(!indication.contains(CANARY));
        }
        FilteredStoredContent::Allowed(_) => panic!("remote must withhold local_only"),
    }

    let local = filter_or_indicate_for_sink(
        &mem.content,
        &label,
        Sink::LocalModel,
        Some("knowledge_recall"),
        IndicationVerbosity::Descriptive,
    );
    assert_eq!(local, FilteredStoredContent::Allowed(CANARY.into()));
    Ok(())
}

#[test]
fn legacy_unlabeled_fail_closed_withholds_from_remote() {
    let cfg = EgressConfig {
        legacy_unlabeled: NamedEgressLabel::NoRemoteModel,
        ..Default::default()
    };
    let label = resolve_stored_label(None, &cfg);
    assert_eq!(label, EgressLabel::no_remote_model());
    match filter_or_indicate_for_sink(
        CANARY,
        &label,
        Sink::RemoteModel,
        Some("knowledge_recall"),
        IndicationVerbosity::Descriptive,
    ) {
        FilteredStoredContent::Withheld { indication } => {
            assert!(!indication.contains(CANARY));
        }
        FilteredStoredContent::Allowed(_) => panic!("fail-closed legacy must withhold"),
    }
}

#[test]
fn knowledge_store_tool_stamps_taint_and_recall_filters() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let gw = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gw)?;
    let store = Arc::new(GatewayStore::open(&gw)?);
    let registry = default_registry();
    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let config = cfg_with_legacy(NamedEgressLabel::Unrestricted);
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;

    let store_ctx = run_ctx(None, Some(EgressLabel::local_only()));
    let store_args = serde_json::json!({
        "id": "mem-tool-canary",
        "content": CANARY,
        "scope": "lessons",
        "visibility": "global"
    })
    .to_string();
    let stored = registry.execute(
        "knowledge_store",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw),
        &store_args,
        Some("root-908/coder"),
        None,
        Some(&config),
        Some(store.clone()),
        Some(&store_ctx),
    )?;
    let stored_v: serde_json::Value = serde_json::from_str(&stored)?;
    assert_eq!(stored_v["ok"], true);

    let persisted = store.memory_get("mem-tool-canary")?.expect("persisted");
    assert_eq!(persisted.egress_label, Some(EgressLabel::local_only()));

    let recall_ctx = run_ctx(Some(Sink::RemoteModel), None);
    let recall = registry.execute(
        "knowledge_recall",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw),
        r#"{"id":"mem-tool-canary"}"#,
        Some("root-908/coder"),
        None,
        Some(&config),
        Some(store.clone()),
        Some(&recall_ctx),
    )?;
    assert!(!recall.contains(CANARY), "remote recall leaked canary: {recall}");
    assert!(
        recall.contains("withheld") || recall.contains("egress_withheld"),
        "expected withhold signal: {recall}"
    );

    let local_ctx = run_ctx(Some(Sink::LocalModel), None);
    let recall_local = registry.execute(
        "knowledge_recall",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gw),
        r#"{"id":"mem-tool-canary"}"#,
        Some("root-908/coder"),
        None,
        Some(&config),
        Some(store.clone()),
        Some(&local_ctx),
    )?;
    assert!(
        recall_local.contains(CANARY),
        "local recall should see canary: {recall_local}"
    );
    Ok(())
}

#[test]
fn execution_search_filters_stdout_for_remote() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    store.create_execution_trace(&ExecutionTraceRecord {
        trace_id: "trc-canary".into(),
        event_id: None,
        agent_id: "coder.default".into(),
        session_id: "root-908/coder".into(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox_exec".into(),
        command: Some("cat mail".into()),
        exit_code: Some(0),
        stdout: Some(CANARY.into()),
        stderr: None,
        duration_ms: 1,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: Some(format!(r#"{{"ok":true,"stdout":"{CANARY}"}}"#)),
        egress_label: Some(EgressLabel::local_only()),
        mount_set: None,
    })?;

    let registry = default_registry();
    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let config = GatewayConfig::default();
    let agent_dir = PathBuf::from(".");
    let ctx = run_ctx(Some(Sink::RemoteModel), None);

    let out = registry.execute(
        "execution_search",
        &manifest,
        &policy,
        &agent_dir,
        None,
        r#"{"limit":10}"#,
        Some("root-908/coder"),
        None,
        Some(&config),
        Some(store),
        Some(&ctx),
    )?;
    // The trace must actually be *found* — otherwise "no canary" would also be
    // satisfied by a scope refusal or an empty result set, and this test would
    // stop proving that the label filter is what withheld it.
    assert!(
        out.contains("trc-canary"),
        "the trace should be in scope and returned: {out}"
    );
    assert!(
        !out.contains(CANARY),
        "execution_search leaked canary to remote sink: {out}"
    );
    Ok(())
}

#[test]
fn execution_search_filters_error_summary_for_remote() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    store.create_execution_trace(&ExecutionTraceRecord {
        trace_id: "trc-canary-err".into(),
        event_id: None,
        agent_id: "coder.default".into(),
        session_id: "root-908/coder".into(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox_exec".into(),
        command: Some("cat mail".into()),
        exit_code: Some(1),
        stdout: None,
        stderr: Some("boom".into()),
        duration_ms: 1,
        success: 0,
        error_type: Some("runtime".into()),
        error_summary: Some(CANARY.into()),
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
        egress_label: Some(EgressLabel::local_only()),
        mount_set: None,
    })?;

    let registry = default_registry();
    let manifest = test_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let config = GatewayConfig::default();
    let agent_dir = PathBuf::from(".");

    let remote_out = registry.execute(
        "execution_search",
        &manifest,
        &policy,
        &agent_dir,
        None,
        r#"{"limit":10}"#,
        Some("root-908/coder"),
        None,
        Some(&config),
        Some(store.clone()),
        Some(&run_ctx(Some(Sink::RemoteModel), None)),
    )?;
    assert!(
        remote_out.contains("trc-canary-err"),
        "the trace should be in scope and returned: {remote_out}"
    );
    assert!(
        !remote_out.contains(CANARY),
        "execution_search leaked error_summary to remote sink: {remote_out}"
    );
    assert!(
        remote_out.contains("withheld"),
        "expected a withheld indication for error_summary: {remote_out}"
    );

    let local_out = registry.execute(
        "execution_search",
        &manifest,
        &policy,
        &agent_dir,
        None,
        r#"{"limit":10}"#,
        Some("root-908/coder"),
        None,
        Some(&config),
        Some(store),
        Some(&run_ctx(Some(Sink::LocalModel), None)),
    )?;
    assert!(
        local_out.contains(CANARY),
        "local sink should see error_summary: {local_out}"
    );
    Ok(())
}

#[test]
fn memory_relabel_updates_and_is_auditable() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let mut mem = MemoryObject::new(
        "mem-legacy".into(),
        "lessons".into(),
        "coder.default".into(),
        "coder.default".into(),
        "session:s".into(),
        "legacy content".into(),
    );
    mem.egress_label = None;
    store.memory_upsert(&mem)?;

    let n = store.memory_relabel(&EgressLabel::local_only(), Some("lessons"), true)?;
    assert_eq!(n, 1);
    let got = store.memory_get("mem-legacy")?.expect("mem");
    assert_eq!(got.egress_label, Some(EgressLabel::local_only()));

    autonoetic_gateway::runtime::egress_labeler::emit_relabel(
        &store,
        "operator",
        "operator",
        "memories",
        n,
        &EgressLabel::local_only(),
        Some("lessons"),
        None,
    );
    let events = store.search_causal_events(Some("operator"), None, 20)?;
    assert!(
        events.iter().any(|e| e.action == "egress.relabel"),
        "expected egress.relabel event"
    );
    Ok(())
}

#[test]
fn mixed_acceptance_smoke_remote_paths_never_see_local_only() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = Arc::new(GatewayStore::open(tmp.path())?);
    let mut mem = MemoryObject::new(
        "mem-smoke".into(),
        "digest.lesson".into(),
        "coder.default".into(),
        "coder.default".into(),
        "session:smoke".into(),
        format!("{CANARY} lesson about mail"),
    );
    mem.tags = vec![
        "source:post_session_digest".into(),
        "agent:coder.default".into(),
        "session:smoke".into(),
    ];
    mem.visibility = MemoryVisibility::Global;
    mem.egress_label = Some(EgressLabel::local_only());
    store.memory_upsert(&mem)?;

    let snippet = autonoetic_gateway::runtime::context::build_memory_context_snippet(
        &store,
        "coder.default",
        5,
        Some("mail"),
        Some(Sink::RemoteModel),
        Some(&EgressConfig::default()),
    )
    .expect("snippet");
    assert!(
        !snippet.contains(CANARY),
        "memory priming leaked canary: {snippet}"
    );
    Ok(())
}
