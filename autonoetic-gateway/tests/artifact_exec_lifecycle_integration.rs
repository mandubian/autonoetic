//! E2E test for artifact execution approval reuse lifecycle.
//!
//! Tests the full cycle:
//! 1. Analyze artifact source code → detect concrete targets
//! 2. Classify coverage → Concrete (import + URL allows reuse)
//! 3. Fingerprint stability → same artifact_id produces same fingerprint
//! 4. Cache hit on second run → approval reuse works
//! 5. artifact_exec tool is registered and gated by ArtifactExecution

mod support;

use autonoetic_gateway::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache,
};
use autonoetic_gateway::runtime::remote_access::{
    classify_network_coverage, NetworkCoverage, RemoteAccessAnalyzer,
};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use tempfile::tempdir;

fn manifest_with_artifact_execution(agent_id: &str) -> AgentManifest {
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
            description: "Test agent".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![
            Capability::ArtifactExecution,
            Capability::NetworkAccess {
                hosts: vec!["*".to_string()],
            },
        ],
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

fn manifest_without_network() -> AgentManifest {
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
            id: "executor.default".to_string(),
            name: "Executor".to_string(),
            description: "Test executor".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::ArtifactExecution],
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

const WEATHER_ARTIFACT_CODE: &str = r#"
# --- weather.py ---
import requests
import json

def fetch_weather(city):
    url = f"https://wttr.in/{city}"
    resp = requests.get(url)
    return resp.json()

if __name__ == "__main__":
    import sys
    city = sys.argv[1] if len(sys.argv) > 1 else "London"
    print(json.dumps(fetch_weather(city)))
"#;

#[test]
fn test_artifact_analysis_detects_concrete_targets() {
    let analysis = RemoteAccessAnalyzer::analyze_code(WEATHER_ARTIFACT_CODE);
    assert!(
        analysis.requires_approval,
        "weather artifact should require approval"
    );

    let targets = normalize_targets(&analysis.detected_patterns);
    assert!(
        targets.contains(&"wttr.in".to_string()),
        "should detect wttr.in, got: {:?}",
        targets
    );

    let categories: Vec<&str> = analysis
        .detected_patterns
        .iter()
        .map(|p| p.category.as_str())
        .collect();
    assert!(
        categories.contains(&"import"),
        "should detect import pattern"
    );
    assert!(
        categories.contains(&"url_literal"),
        "should detect url_literal pattern"
    );
}

#[test]
fn test_artifact_coverage_is_concrete_despite_imports() {
    let analysis = RemoteAccessAnalyzer::analyze_code(WEATHER_ARTIFACT_CODE);
    let targets = normalize_targets(&analysis.detected_patterns);

    let coverage = classify_network_coverage(&analysis.detected_patterns, targets.clone());
    assert_eq!(
        coverage,
        NetworkCoverage::Concrete {
            targets: vec!["wttr.in".to_string()]
        },
        "import + url_literal should classify as Concrete"
    );
}

#[test]
fn test_artifact_fingerprint_stable_across_shell_wrappers() {
    let manifest = manifest_without_network();

    let fp_shell_v1 = compute_fingerprint(
        &manifest.agent.id,
        &["wttr.in".to_string()],
        "python3 -c 'import requests; requests.get(\"https://wttr.in/Paris\")'",
        Some("art-weather-abc"),
        &manifest.capabilities,    );
    let fp_shell_v2 = compute_fingerprint(
        &manifest.agent.id,
        &["wttr.in".to_string()],
        "python3 /tmp/weather.py Paris",
        Some("art-weather-abc"),
        &manifest.capabilities,    );
    let fp_shell_v3 = compute_fingerprint(
        &manifest.agent.id,
        &["wttr.in".to_string()],
        "python3 /tmp/weather.py London",
        Some("art-weather-abc"),
        &manifest.capabilities,    );

    assert_eq!(
        fp_shell_v1, fp_shell_v2,
        "same artifact_id should produce same fingerprint across shell wrappers"
    );
    assert_eq!(
        fp_shell_v1, fp_shell_v3,
        "same artifact_id should produce same fingerprint regardless of args"
    );
}

#[test]
fn test_artifact_fingerprint_differs_across_artifacts() {
    let manifest = manifest_without_network();

    let fp_art_a = compute_fingerprint(
        &manifest.agent.id,
        &["wttr.in".to_string()],
        "code",
        Some("art-weather-v1"),
        &manifest.capabilities,    );
    let fp_art_b = compute_fingerprint(
        &manifest.agent.id,
        &["wttr.in".to_string()],
        "code",
        Some("art-weather-v2"),
        &manifest.capabilities,    );

    assert_ne!(
        fp_art_a, fp_art_b,
        "different artifact_ids should produce different fingerprints"
    );
}

#[test]
fn test_lifecycle_cache_reuse_simulated() {
    let temp = tempdir().expect("tempdir");
    let gateway_dir = temp.path();
    let manifest = manifest_without_network();

    // Step 1: First run — artifact analyzed, approval granted, cache recorded
    let analysis = RemoteAccessAnalyzer::analyze_code(WEATHER_ARTIFACT_CODE);
    let targets = normalize_targets(&analysis.detected_patterns);
    let coverage = classify_network_coverage(&analysis.detected_patterns, targets.clone());
    assert!(matches!(coverage, NetworkCoverage::Concrete { .. }));

    let fingerprint_first = compute_fingerprint(
        &manifest.agent.id,
        &targets,
        WEATHER_ARTIFACT_CODE,
        Some("art-weather-lifecycle"),
        &manifest.capabilities,    );

    let cache = ApprovedExecCache::new(gateway_dir).expect("cache create");
    let now = chrono::Utc::now().to_rfc3339();
    cache
        .record(
            autonoetic_gateway::runtime::approved_exec_cache::ApprovedExecEntry {
                fingerprint: fingerprint_first.clone(),
                agent_id: manifest.agent.id.clone(),
                remote_targets: targets.clone(),
                code_content: WEATHER_ARTIFACT_CODE.to_string(),
                approval_request_id: "apr-lifecycle-test".to_string(),
                approved_at: now.clone(),
                approved_by: "operator".to_string(),
                last_used_at: now.clone(),
            },
        )
        .expect("record");

    // Step 2: Second run — different shell wrapper, same artifact
    let fingerprint_second = compute_fingerprint(
        &manifest.agent.id,
        &["wttr.in".to_string()],
        "python3 /tmp/weather.py London",
        Some("art-weather-lifecycle"),
        &manifest.capabilities,    );

    assert_eq!(
        fingerprint_first, fingerprint_second,
        "fingerprint should match on second run"
    );

    let found = cache.find(&fingerprint_second);
    assert!(
        found.is_some(),
        "cache should hit on second run — approval reuse works"
    );
    assert_eq!(found.unwrap().remote_targets, vec!["wttr.in"]);
}

#[test]
fn test_artifact_exec_tool_registered_and_gated() {
    let registry = default_registry();
    assert!(registry.has_tool("artifact_exec"));

    let manifest = manifest_with_artifact_execution("coder.default");
    let defs = registry.available_definitions(&manifest);
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"artifact_exec"),
        "artifact_exec should be available with ArtifactExecution"
    );

    let shell_only = AgentManifest {
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
        ..manifest_with_artifact_execution("executor.shell-only")
    };
    let shell_names: Vec<String> = registry
        .available_definitions(&shell_only)
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert!(
        !shell_names.iter().any(|name| name == "artifact_exec"),
        "CodeExecution alone must not grant artifact_exec"
    );

    let manifest_no_exec = AgentManifest {
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
            id: "researcher.default".to_string(),
            name: "Researcher".to_string(),
            description: "No code execution".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
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
    };
    let defs = registry.available_definitions(&manifest_no_exec);
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        !names.contains(&"artifact_exec"),
        "artifact_exec should NOT be available without ArtifactExecution or a legacy promotion exec gate role"
    );
}

#[test]
fn test_artifact_exec_definition_exposes_input_parameter() {
    // Schema-contract guard: the `input` parameter must appear in the tool's
    // JSON schema so agents discover it without needing to know the
    // AUTONOETIC_INPUT env var name (the session-3739f831 discoverability gap).
    let registry = default_registry();
    let manifest = manifest_with_artifact_execution("coder.default");
    let defs = registry.available_definitions(&manifest);
    let artifact_exec_def = defs
        .iter()
        .find(|d| d.name == "artifact_exec")
        .expect("artifact_exec should be available with ArtifactExecution");

    let schema = &artifact_exec_def.input_schema;
    let properties = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("schema must have a properties object");
    assert!(
        properties.contains_key("input"),
        "artifact_exec schema must declare an `input` property; got: {properties:?}"
    );
    assert!(
        properties.contains_key("args"),
        "artifact_exec schema must still declare `args` (argv remains legitimate)"
    );
    // The description should mention load_input so agents can route correctly.
    let input_desc = properties
        .get("input")
        .and_then(|d| d.get("description"))
        .and_then(|d| d.as_str())
        .unwrap_or("");
    assert!(
        input_desc.contains("load_input"),
        "`input` description should name load_input() so agents can distinguish it from args; got: {input_desc:?}"
    );
}

fn unit_test_runner_gate_manifest() -> AgentManifest {
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
            id: "acme.custom_unit_test_runner".to_string(),
            name: "Custom Unit Test Runner".to_string(),
            description: "Federation unit-test gate".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![
            Capability::SandboxFunctions {
                allowed: vec![
                    "artifact_inspect".to_string(),
                    "artifact_exec".to_string(),
                    "promotion_".to_string(),
                ],
            },
            Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            },
        ],
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

#[test]
fn test_artifact_exec_available_for_promotion_gate_runner_without_evaluation() {
    let registry = default_registry();
    let manifest = unit_test_runner_gate_manifest();
    let defs = registry.available_definitions(&manifest);
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"artifact_exec"),
        "artifact_exec must be available for promotion exec gate declared in SandboxFunctions"
    );
    assert!(
        !names.contains(&"eval_suite_publish"),
        "promotion gate runner must not gain eval-suite tools via Evaluation cap"
    );
    assert!(
        !names.contains(&"sandbox_exec"),
        "promotion gate runner must not gain sandbox_exec without CodeExecution"
    );
}

#[test]
fn test_artifact_exec_not_available_for_static_evaluator() {
    let registry = default_registry();
    let manifest = AgentManifest {
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
            id: "static_evaluator.default".to_string(),
            name: "Static Evaluator".to_string(),
            description: "Static federation gate".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![
            Capability::SandboxFunctions {
                allowed: vec!["promotion_".to_string()],
            },
            Capability::ReadAccess {
                scopes: vec!["self.*".to_string()],
            },
        ],
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
    };
    let defs = registry.available_definitions(&manifest);
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        !names.contains(&"artifact_exec"),
        "static_evaluator must not gain artifact_exec via promotion verdict list"
    );
}
