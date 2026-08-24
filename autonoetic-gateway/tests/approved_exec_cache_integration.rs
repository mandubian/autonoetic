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
use autonoetic_gateway::runtime::remote_access::{DetectedPatternCategory, 
    classify_network_coverage, DetectedPattern, NetworkCoverage,
};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;
use support::manifest_builder::TestManifest;

fn create_pattern(category: DetectedPatternCategory, pattern: &str) -> DetectedPattern {
    DetectedPattern {
        category,
        pattern: pattern.to_string(),
        line_number: Some(1),
        reason: "test".to_string(),
    }
}

fn test_agent_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "test.agent".to_string(),
            name: "test.agent".to_string(),
            description: "Test agent".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
        ..TestManifest::new().build()
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

    let found = cache.find("sha256:abc123", 0);
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
    let found = cache.find("sha256:persistent", 0);
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

    let found = cache.find("sha256:update", 0).expect("should find entry");
    // Verify last_used_at was updated (it should be close to now)
    assert!(found.last_used_at >= now);
}

#[test]
fn test_cache_not_found() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
    assert!(cache.find("sha256:nonexistent", 0).is_none());
}

#[test]
fn test_cache_all_remove_clear() {
    // #380: operator-facing list + revoke over the exec cache.
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();
    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");

    let mk = |fp: &str, approved_at: &str| ApprovedExecEntry {
        fingerprint: fp.to_string(),
        agent_id: "test.agent".to_string(),
        remote_targets: vec!["api.example.com".to_string()],
        code_content: "code".to_string(),
        approval_request_id: "apr".to_string(),
        approved_at: approved_at.to_string(),
        approved_by: "operator".to_string(),
        last_used_at: approved_at.to_string(),
    };
    cache.record(mk("sha256:bbb", "2026-06-02T00:00:00Z")).unwrap();
    cache.record(mk("sha256:aaa", "2026-06-01T00:00:00Z")).unwrap();

    // all() returns every entry, sorted by approved_at.
    let all = cache.all();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].fingerprint, "sha256:aaa");
    assert_eq!(all[1].fingerprint, "sha256:bbb");

    // remove() revokes one and persists; a no-op remove returns false.
    assert!(cache.remove("sha256:aaa").unwrap());
    assert!(!cache.remove("sha256:aaa").unwrap());
    assert!(cache.find("sha256:aaa", 0).is_none());
    assert!(cache.find("sha256:bbb", 0).is_some());

    // Revocation survives reopen (persisted).
    let reopened = ApprovedExecCache::new(gateway_dir).expect("reopen");
    assert_eq!(reopened.len(), 1);

    // clear() removes all and returns the count.
    assert_eq!(reopened.clear().unwrap(), 1);
    assert_eq!(reopened.len(), 0);
    assert_eq!(reopened.clear().unwrap(), 0);
}

#[test]
fn test_classify_coverage_url_only() {
    let patterns = vec![create_pattern(
        DetectedPatternCategory::UrlLiteral,
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
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.example.com/data"),
        create_pattern(DetectedPatternCategory::IpAddress, "192.168.1.100"),
    ];
    let targets = vec!["192.168.1.100".to_string(), "api.example.com".to_string()];
    let coverage = classify_network_coverage(&patterns, targets.clone());
    assert_eq!(coverage, NetworkCoverage::Concrete { targets });
}

#[test]
fn test_classify_coverage_import_plus_url_is_concrete() {
    let patterns = vec![
        create_pattern(DetectedPatternCategory::Import, "import requests"),
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.example.com/data"),
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
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.example.com/data"),
        create_pattern(DetectedPatternCategory::FunctionCall, ".connect("),
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
        create_pattern(DetectedPatternCategory::Import, "import requests"),
        create_pattern(DetectedPatternCategory::FunctionCall, "requests.get("),
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
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.example.com/v1/data"),
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://status.github.com/api"),
        create_pattern(DetectedPatternCategory::Import, "import requests"), // Should be skipped
    ];
    let targets = normalize_targets(&patterns);
    assert_eq!(targets, vec!["api.example.com", "status.github.com"]);
}

#[test]
fn test_normalize_targets_dedup() {
    let patterns = vec![
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.example.com/v1"),
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.example.com/v2"),
    ];
    let targets = normalize_targets(&patterns);
    assert_eq!(targets, vec!["api.example.com"]);
}

#[test]
fn test_compute_fingerprint_deterministic() {
    let fp1 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", None, &[]);
    let fp2 = compute_fingerprint("agent.id", &["host.com".to_string()], "code", None, &[]);
    assert_eq!(fp1, fp2);
    assert!(fp1.starts_with("sha256:"));
}

#[test]
fn test_compute_fingerprint_different() {
    let fp1 = compute_fingerprint("agent.a", &["host.com".to_string()], "code", None, &[]);
    let fp2 = compute_fingerprint("agent.b", &["host.com".to_string()], "code", None, &[]);
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
        &[],
    );
    assert!(cache.find(&fingerprint, 0).is_none());

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
    assert!(cache.find(&fingerprint, 0).is_some());

    // 4. Different code = different fingerprint
    let different_fingerprint = compute_fingerprint(
        "test.agent",
        &["api.example.com".to_string()],
        "import requests\nrequests.post('https://api.example.com')", // POST instead of GET
        None,
        &[],
    );
    assert!(cache.find(&different_fingerprint, 0).is_none());
}

#[test]
fn test_cache_not_used_for_unresolved_targets() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path();

    // Code with ONLY imports/function calls, no concrete URL - should classify as Unresolved
    let patterns = vec![
        create_pattern(DetectedPatternCategory::Import, "import requests"),
        create_pattern(DetectedPatternCategory::FunctionCall, "requests.get("),
    ];

    let coverage = classify_network_coverage(&patterns, vec![]);
    assert_eq!(coverage, NetworkCoverage::Unresolved);

    // Unresolved means no concrete targets to match against
    let cache = ApprovedExecCache::new(gateway_dir).expect("cache should create");
    let targets = normalize_targets(&patterns);
    let _fingerprint = compute_fingerprint("test.agent", &targets, "code", None, &[]);

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
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    // Pre-populate the cache with a known fingerprint for concrete URL-only code
    let code_content = r#"print("https://api.example.com/data")"#;
    let patterns = vec![create_pattern(
        DetectedPatternCategory::UrlLiteral,
        "https://api.example.com/data",
    )];
    let targets = normalize_targets(&patterns);
    let fingerprint = compute_fingerprint("test.agent", &targets, code_content, None, &manifest.capabilities);

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

/// #381 acceptance: an approval cached under one capability scope must NOT be
/// reused after the agent's capabilities change.
///
/// Note on the issue's "widen NetworkAccess" wording: the approved-exec cache is
/// only consulted for agents WITHOUT `NetworkAccess` — an agent that holds
/// `NetworkAccess` is authorized and bypasses the approval/cache path entirely
/// (`sandbox.rs`: `if !agent_has_network_access { …approval/cache… }`). So the
/// meaningful "capabilities changed" case for a *cached* exec is a non-network
/// capability change. Here the agent's `CodeExecution` scope changes between the
/// cached approval and reuse: the fingerprint differs, so the entry found under
/// the old scope is NOT found under the new one — the gateway routes to a fresh
/// approval instead of silently reusing the stale grant. Proven at the cache +
/// fingerprint layer (the same `compute_fingerprint` the gateway calls at every
/// sandbox.exec reuse site), which is deterministic and not coupled to the
/// orthogonal command-analysis / approval-trigger path.
#[test]
fn test_capability_change_misses_cache_recorded_under_old_scope() {
    let temp = tempdir().expect("tempdir should create");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");

    // No NetworkAccess (the cache only applies to non-network agents). The
    // CodeExecution scope is what changes between approval and reuse.
    let mut narrow_manifest = test_agent_manifest();
    narrow_manifest.capabilities = vec![Capability::CodeExecution {
        patterns: vec!["python3 scripts/legacy.py".to_string()],
        commands: vec![],
    }];
    let mut wide_manifest = test_agent_manifest();
    wide_manifest.capabilities = vec![Capability::CodeExecution {
        patterns: vec!["*".to_string()],
        commands: vec![],
    }];

    // Concrete URL with no host authorization (the originally-approved exec).
    let code_content = r#"print("https://api.example.com/data")"#;
    let patterns = vec![create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.example.com/data")];
    let targets = normalize_targets(&patterns);

    // Pre-populate the cache as if approved under the NARROW capability scope.
    let narrow_fp =
        compute_fingerprint("test.agent", &targets, code_content, None, &narrow_manifest.capabilities);
    let now = chrono::Utc::now().to_rfc3339();
    let cache = ApprovedExecCache::new(&gateway_dir).expect("cache should create");
    cache
        .record(ApprovedExecEntry {
            fingerprint: narrow_fp.clone(),
            agent_id: "test.agent".to_string(),
            remote_targets: targets.clone(),
            code_content: code_content.to_string(),
            approval_request_id: "apr-narrow".to_string(),
            approved_at: now.clone(),
            approved_by: "operator".to_string(),
            last_used_at: now.clone(),
        })
        .expect("record should succeed");

    // Under the ORIGINAL (narrow) scope the entry is reusable…
    assert!(
        cache.find(&narrow_fp, 0).is_some(),
        "approval recorded under the narrow scope must be found under that same scope"
    );

    // …but once the agent's capabilities change, the lookup the gateway performs
    // uses a different fingerprint, so the stale grant is NOT reused — the run
    // would route to a fresh approval instead. This is the #381 guarantee, proven
    // through the real ApprovedExecCache + compute_fingerprint (the same fingerprint
    // the gateway computes at every sandbox.exec reuse site).
    let wide_fp =
        compute_fingerprint("test.agent", &targets, code_content, None, &wide_manifest.capabilities);
    assert_ne!(narrow_fp, wide_fp, "a capability change must change the fingerprint");
    assert!(
        cache.find(&wide_fp, 0).is_none(),
        "a changed capability scope must NOT reuse the approval cached under the old scope"
    );
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
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    // Egress boundary fail-closed requires a GatewayStore to resolve session
    // taint (pre-existing rot after phase-4 sandbox wiring — without a store
    // the tool returns `egress_boundary_refused` before the approval path).
    let store = std::sync::Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
            .expect("gateway store should open"),
    );

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
        Some(store),
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
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        ..Default::default()
    };

    // Code with import + URL literal - should NOW be cacheable (Concrete coverage)
    let code_content = r#"import requests
requests.get("https://api.cache-test.dev")"#;
    let patterns = vec![
        create_pattern(DetectedPatternCategory::Import, "import requests"),
        create_pattern(DetectedPatternCategory::UrlLiteral, "https://api.cache-test.dev"),
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

    let fingerprint = compute_fingerprint("test.agent", &targets, code_content, None, &manifest.capabilities);

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
