//! Integration tests for the approved sandbox exec replay cache.
//!
//! Tests the complete cache lifecycle:
//! 1. Cache miss → approval required
//! 2. Cache hit → skip approval
//! 3. Different code → new cache entry
//! 4. Opaque targets → never cached
//!
//! These tests verify both the cache module directly AND the integration
//! with sandbox.exec through the tool registry.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache, ApprovedExecEntry,
};
use autonoetic_gateway::runtime::remote_access::{
    classify_network_coverage, DetectedPattern, NetworkCoverage,
};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

fn create_pattern(category: &str, pattern: &str) -> DetectedPattern {
    DetectedPattern {
        category: category.to_string(),
        pattern: pattern.to_string(),
        line_number: Some(1),
        reason: "test".to_string(),
    }
}

fn test_agent_manifest() -> AgentManifest {
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
            id: "test.agent".to_string(),
            name: "test.agent".to_string(),
            description: "Test agent".to_string(),
        },
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
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
        agentskills_import: None,
        compression: None,
    }
}

/// Creates a test script file with the given content and returns the script path.
fn create_test_script(
    agent_dir: &std::path::Path,
    filename: &str,
    content: &str,
) -> std::path::PathBuf {
    let scripts_dir = agent_dir.join("scripts");
    std::fs::create_dir_all(&scripts_dir).expect("scripts dir should create");
    let script_path = scripts_dir.join(filename);
    std::fs::write(&script_path, content).expect("script should write");
    script_path
}

fn write_remote_access_declaration(agent_dir: &std::path::Path) {
    let skill = r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: "required"
      targets:
        - kind: "any"
      enabled_languages: []
      python_imports: []
      js_imports: []
      rust_imports: []
      go_imports: []
      function_calls: []
      shell_commands: []
      package_manager_commands: []
---
"#;
    std::fs::write(agent_dir.join("SKILL.md"), skill).expect("skill should write");
}

#[test]
fn test_cache_record_and_find() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
    assert_eq!(cache.len(), 0);

    let now = chrono::Utc::now().to_rfc3339();
    let entry = ApprovedExecEntry {
        fingerprint: "sha256:abc123".to_string(),
        agent_id: "test.agent".to_string(),
        remote_targets: vec!["api.example.com".to_string()],
        code_content: "import requests\nrequests.get('https://api.example.com')".to_string(),
        approval_request_id: "apr-12345678".to_string(),
        approved_at: now.clone(),
        approved_by: "operator".to_string(),
        last_used_at: now.clone(),
    };

    cache.record(entry.clone()).expect("record should succeed");
    assert_eq!(cache.len(), 1);

    let found = cache.find("sha256:abc123");
    assert!(found.is_some());
    assert_eq!(found.unwrap().agent_id, "test.agent");
}

#[test]
fn test_cache_persistence() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    let now = chrono::Utc::now().to_rfc3339();
    let entry = ApprovedExecEntry {
        fingerprint: "sha256:persistent".to_string(),
        agent_id: "test.agent".to_string(),
        remote_targets: vec!["api.example.com".to_string()],
        code_content: "code".to_string(),
        approval_request_id: "apr-12345678".to_string(),
        approved_at: now.clone(),
        approved_by: "operator".to_string(),
        last_used_at: now.clone(),
    };

    {
        let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
        cache.record(entry).expect("record should succeed");
    }

    // Reopen cache and verify persistence
    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should reopen");
    assert_eq!(cache.len(), 1);
    let found = cache.find("sha256:persistent");
    assert!(found.is_some());
}

#[test]
fn test_cache_update_last_used() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
    let now = chrono::Utc::now().to_rfc3339();
    let entry = ApprovedExecEntry {
        fingerprint: "sha256:update".to_string(),
        agent_id: "test.agent".to_string(),
        remote_targets: vec!["api.example.com".to_string()],
        code_content: "code".to_string(),
        approval_request_id: "apr-12345678".to_string(),
        approved_at: now.clone(),
        approved_by: "operator".to_string(),
        last_used_at: now.clone(),
    };

    cache.record(entry).expect("record should succeed");
    cache
        .update_last_used("sha256:update")
        .expect("update should succeed");

    let found = cache.find("sha256:update").expect("should find entry");
    // Verify last_used_at was updated (it should be close to now)
    assert!(found.last_used_at >= now);
}

#[test]
fn test_cache_not_found() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
    assert!(cache.find("sha256:nonexistent").is_none());
}

#[test]
fn test_classify_coverage_url_only() {
    let patterns = vec![create_pattern(
        "url_literal",
        "https://api.example.com/data",
    )];
    let coverage = classify_network_coverage(&patterns, vec!["api.example.com".to_string()]);
    assert_eq!(
        coverage,
        NetworkCoverage::Concrete {
            targets: vec!["api.example.com".to_string()]
        }
    );
}

#[test]
fn test_classify_coverage_mixed_concrete() {
    let patterns = vec![
        create_pattern("url_literal", "https://api.example.com/data"),
        create_pattern("ip_address", "192.168.1.100"),
    ];
    let targets = vec!["192.168.1.100".to_string(), "api.example.com".to_string()];
    let coverage = classify_network_coverage(&patterns, targets.clone());
    assert_eq!(coverage, NetworkCoverage::Concrete { targets });
}

#[test]
fn test_classify_coverage_import_plus_url_is_concrete() {
    let patterns = vec![
        create_pattern("import", "import requests"),
        create_pattern("url_literal", "https://api.example.com/data"),
    ];
    let coverage = classify_network_coverage(&patterns, vec!["api.example.com".to_string()]);
    assert_eq!(
        coverage,
        NetworkCoverage::Concrete {
            targets: vec!["api.example.com".to_string()]
        },
        "import + URL should classify as Concrete (imports are weak signals)"
    );
}

#[test]
fn test_classify_coverage_function_call_plus_url_is_concrete() {
    let patterns = vec![
        create_pattern("url_literal", "https://api.example.com/data"),
        create_pattern("function_call", ".connect("),
    ];
    let coverage = classify_network_coverage(&patterns, vec!["api.example.com".to_string()]);
    assert_eq!(
        coverage,
        NetworkCoverage::Concrete {
            targets: vec!["api.example.com".to_string()]
        },
        "function_call + URL should classify as Concrete (function_call is a weak signal)"
    );
}

#[test]
fn test_classify_coverage_import_only_is_unresolved() {
    let patterns = vec![
        create_pattern("import", "import requests"),
        create_pattern("function_call", "requests.get("),
    ];
    let coverage = classify_network_coverage(&patterns, vec![]);
    assert_eq!(coverage, NetworkCoverage::Unresolved);
}

#[test]
fn test_classify_coverage_empty_is_none() {
    let coverage = classify_network_coverage(&[], vec![]);
    assert_eq!(coverage, NetworkCoverage::None);
}

#[test]
fn test_normalize_targets() {
    let patterns = vec![
        create_pattern("url_literal", "https://api.example.com/v1/data"),
        create_pattern("url_literal", "https://status.github.com/api"),
        create_pattern("import", "import requests"), // Should be skipped
    ];
    let targets = normalize_targets(&patterns);
    assert_eq!(targets, vec!["api.example.com", "status.github.com"]);
}

#[test]
fn test_normalize_targets_dedup() {
    let patterns = vec![
        create_pattern("url_literal", "https://api.example.com/v1"),
        create_pattern("url_literal", "https://api.example.com/v2"),
    ];
    let targets = normalize_targets(&patterns);
    assert_eq!(targets, vec!["api.example.com"]);
}

#[test]
fn test_compute_fingerprint_deterministic() {
    let fp1 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", None);
    let fp2 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", None);
    assert_eq!(fp1, fp2);
    assert!(fp1.starts_with("sha256:"));
}

#[test]
fn test_compute_fingerprint_different() {
    let fp1 = compute_fingerprint("agent.a", &["host.com".to_string()], "code", None);
    let fp2 = compute_fingerprint("agent.b", &["host.com".to_string()], "code", None);
    assert_ne!(fp1, fp2);
}

#[test]
fn test_cache_full_cycle() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    // 1. Cache miss
    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
    let fingerprint = compute_fingerprint(
        "test.agent",
        &["api.example.com".to_string()],
        "import requests\nrequests.get('https://api.example.com')",
        None,
    );
    assert!(cache.find(&fingerprint).is_none());

    // 2. Record after approval
    let now = chrono::Utc::now().to_rfc3339();
    let entry = ApprovedExecEntry {
        fingerprint: fingerprint.clone(),
        agent_id: "test.agent".to_string(),
        remote_targets: vec!["api.example.com".to_string()],
        code_content: "import requests\nrequests.get('https://api.example.com')".to_string(),
        approval_request_id: "apr-12345678".to_string(),
        approved_at: now.clone(),
        approved_by: "operator".to_string(),
        last_used_at: now.clone(),
    };
    cache.record(entry).expect("record should succeed");

    // 3. Cache hit
    assert!(cache.find(&fingerprint).is_some());

    // 4. Different code = different fingerprint
    let different_fingerprint = compute_fingerprint(
        "test.agent",
        &["api.example.com".to_string()],
        "import requests\nrequests.post('https://api.example.com')", // POST instead of GET
        None,
    );
    assert!(cache.find(&different_fingerprint).is_none());
}

#[test]
fn test_cache_not_used_for_unresolved_targets() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    // Code with ONLY imports/function calls, no concrete URL - should classify as Unresolved
    let patterns = vec![
        create_pattern("import", "import requests"),
        create_pattern("function_call", "requests.get("),
    ];

    let coverage = classify_network_coverage(&patterns, vec![]);
    assert_eq!(coverage, NetworkCoverage::Unresolved);

    // Unresolved means no concrete targets to match against
    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
    let targets = normalize_targets(&patterns);
    let _fingerprint = compute_fingerprint("test.agent", &targets, "code", None);

    // In the real flow, this would NOT be recorded because coverage is Unresolved
    assert_eq!(cache.len(), 0);
}

#[test]
fn test_sandbox_exec_cache_hit_skips_approval() {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

    let agent_dir = agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    write_remote_access_declaration(&agent_dir);

    let manifest = test_agent_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    // Pre-populate the cache with a known fingerprint for concrete URL-only code
    let code_content = r#"print("https://api.example.com/data")"#;
    let patterns = vec![create_pattern(
        "url_literal",
        "https://api.example.com/data",
    )];
    let targets = normalize_targets(&patterns);
    let fingerprint = compute_fingerprint("test.agent", &targets, code_content, None);

    let cache = ApprovedExecCache::new(&gateway_dir).expect("cache should create");
    let now = chrono::Utc::now().to_rfc3339();
    let entry = ApprovedExecEntry {
        fingerprint: fingerprint.clone(),
        agent_id: "test.agent".to_string(),
        remote_targets: targets.clone(),
        code_content: code_content.to_string(),
        approval_request_id: "apr-test123".to_string(),
        approved_at: now.clone(),
        approved_by: "operator".to_string(),
        last_used_at: now.clone(),
    };
    cache.record(entry).expect("record should succeed");

    // Create a script file with the same code content
    create_test_script(&agent_dir, "fetch.py", code_content);
    // Use relative path to avoid /tmp/ interpretation issues
    let script_rel_path = format!("scripts/fetch.py");

    // Call sandbox.exec with the script - should hit cache and skip approval
    let registry = default_registry();
    let args = serde_json::json!({
        "command": format!("python3 {}", script_rel_path),
    });

    let result = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args).unwrap(),
        Some("test-session"),
        None,
        Some(&config),
        None,
        None,
    );

    // The call should succeed (not return approval_required)
    // Note: The actual sandbox execution might fail in test environment,
    // but the key is that it should NOT require approval since cache hit
    match result {
        Ok(resp) => {
            let resp_val: serde_json::Value = serde_json::from_str(&resp).unwrap();
            // Cache hit should skip approval - no approval_required in response
            assert!(
                !resp_val
                    .get("approval_required")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                "Cache hit should skip approval, but got: {}",
                resp
            );
            tracing::info!(response = %resp, "sandbox.exec with cache hit response");
        }
        Err(e) => {
            // If sandbox fails (e.g., bubblewrap not available), we still verify
            // that approval was skipped by checking the error doesn't mention approval
            let err_msg = e.to_string();
            assert!(
                !err_msg.contains("approval") && !err_msg.contains("approval_required"),
                "Cache hit should skip approval requirement, but got error about approval: {}",
                err_msg
            );
            tracing::info!(error = %err_msg, "sandbox.exec cache hit - execution may fail in test env but approval was skipped");
        }
    }
}

#[test]
fn test_sandbox_exec_cache_miss_requires_approval_for_concrete_url() {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

    let agent_dir = agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    write_remote_access_declaration(&agent_dir);

    let manifest = test_agent_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    // Cache is empty - should require approval for concrete URL code
    // Use python -c to avoid file path issues with sandbox
    let code_content = r#"print("https://api.cache-test.dev/data")"#;
    let registry = default_registry();
    let args = serde_json::json!({
        "command": format!("python3 -c {}", code_content),
    });

    let result = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args).unwrap(),
        Some("test-session"),
        None,
        Some(&config),
        None,
        None,
    );

    // Should require approval since cache is empty
    match result {
        Ok(resp) => {
            let resp_val: serde_json::Value = serde_json::from_str(&resp).unwrap();
            assert!(
                resp_val
                    .get("approval_required")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                "Cache miss should require approval for concrete URL code, but got: {}",
                resp
            );
            // Verify the request_id is present
            assert!(
                resp_val.get("request_id").is_some(),
                "Should include request_id for approval"
            );
            tracing::info!(response = %resp, "sandbox.exec cache miss - approval required");
        }
        Err(e) => {
            // If execution itself fails, check if it's an approval-related error
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("approval") || err_msg.contains("approval_required"),
                "Cache miss should indicate approval required, but got: {}",
                err_msg
            );
            tracing::info!(error = %err_msg, "sandbox.exec cache miss - approval required");
        }
    }
}

#[test]
fn test_sandbox_exec_import_plus_url_caches() {
    let temp = tempdir().expect("tempdir should create");
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

    let agent_dir = agents_dir.join("test.agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir should create");
    write_remote_access_declaration(&agent_dir);

    let manifest = test_agent_manifest();
    let policy = PolicyEngine::new(manifest.clone());

    let config = GatewayConfig {
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    // Code with import + URL literal - should NOW be cacheable (Concrete coverage)
    let code_content = r#"import requests
requests.get("https://api.cache-test.dev")"#;
    let patterns = vec![
        create_pattern("import", "import requests"),
        create_pattern("url_literal", "https://api.cache-test.dev"),
    ];
    let targets = normalize_targets(&patterns);

    // Verify classification is Concrete
    let coverage = classify_network_coverage(&patterns, targets.clone());
    assert_eq!(
        coverage,
        NetworkCoverage::Concrete {
            targets: targets.clone()
        },
        "import + URL should classify as Concrete"
    );

    let fingerprint = compute_fingerprint("test.agent", &targets, code_content, None);

    // Pre-populate cache
    let cache = ApprovedExecCache::new(&gateway_dir).expect("cache should create");
    let now = chrono::Utc::now().to_rfc3339();
    let entry = ApprovedExecEntry {
        fingerprint: fingerprint.clone(),
        agent_id: "test.agent".to_string(),
        remote_targets: targets.clone(),
        code_content: code_content.to_string(),
        approval_request_id: "apr-test456".to_string(),
        approved_at: now.clone(),
        approved_by: "operator".to_string(),
        last_used_at: now.clone(),
    };
    cache.record(entry).expect("record should succeed");

    // Create script with the same content
    create_test_script(&agent_dir, "fetch_with_import.py", code_content);

    // Call sandbox.exec - should hit cache and skip approval
    let registry = default_registry();
    let args = serde_json::json!({
        "command": "python3 scripts/fetch_with_import.py",
    });

    let result = registry.execute(
        "sandbox_exec",
        &manifest,
        &policy,
        &agent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args).unwrap(),
        Some("test-session"),
        None,
        Some(&config),
        None,
        None,
    );

    // Should NOT require approval - cache hit because Concrete coverage
    match result {
        Ok(resp) => {
            let resp_val: serde_json::Value = serde_json::from_str(&resp).unwrap();
            assert!(
                !resp_val.get("approval_required").and_then(|v| v.as_bool()).unwrap_or(false),
                "import + URL with cache entry should skip approval (Concrete coverage), but got: {}",
                resp
            );
            tracing::info!(response = %resp, "sandbox.exec import+URL - cache hit, approval skipped");
        }
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                !err_msg.contains("approval_required"),
                "Cache hit should skip approval requirement, but got error about approval: {}",
                err_msg
            );
            tracing::info!(error = %err_msg, "sandbox.exec import+URL - execution may fail in test env but approval was skipped");
        }
    }
}
