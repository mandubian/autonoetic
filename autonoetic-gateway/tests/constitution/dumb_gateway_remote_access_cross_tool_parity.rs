//! Constitution pin: outbound remote-access declaration failures must be
//! consistent across sandbox, web, and credential HTTP paths.


use std::path::Path;
use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, CredentialRecord};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::{json, Value};
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn manifest(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "cross-tool parity test agent".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        ..TestManifest::new().build()
    }
}

fn sandbox_manifest(agent_id: &str) -> AgentManifest {
    manifest(
        agent_id,
        vec![
            Capability::CodeExecution {
                patterns: vec!["*".to_string()],
                commands: vec![],
            },
            Capability::NetworkAccess {
                hosts: vec!["*".to_string()],
            },
        ],
    )
}

fn web_manifest(agent_id: &str) -> AgentManifest {
    manifest(
        agent_id,
        vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }],
    )
}

fn credential_manifest(agent_id: &str) -> AgentManifest {
    manifest(
        agent_id,
        vec![
            Capability::CredentialAccess {
                services: vec!["github".to_string()],
            },
            Capability::NetworkAccess {
                hosts: vec!["*".to_string()],
            },
        ],
    )
}

fn remote_access_exact_host_skill(host: &str) -> String {
    format!(
        r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: "required"
      targets:
        - kind: "exact_host"
          value: "{host}"
      enabled_languages: []
      python_imports: []
      js_imports: []
      rust_imports: []
      go_imports: []
      function_calls: []
      shell_commands: ["curl", "wget"]
      package_manager_commands: []
---
"#
    )
}

fn write_skill(agent_dir: &Path, skill: Option<&str>) -> anyhow::Result<()> {
    if let Some(skill) = skill {
        std::fs::write(agent_dir.join("SKILL.md"), skill)?;
    }
    Ok(())
}

fn run_sandbox_exec(
    manifest: &AgentManifest,
    skill: Option<&str>,
    command: &str,
) -> anyhow::Result<Value> {
    let temp = tempdir()?;
    write_skill(temp.path(), skill)?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    let cfg = GatewayConfig {
        runtime_dir: agents_dir.join(".gateway"),
        agents_dir,
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(temp.path())?);
    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    let out = registry.execute(
        "sandbox_exec",
        manifest,
        &policy,
        temp.path(),
        Some(temp.path()),
        &json!({ "command": command }).to_string(),
        Some("root-1/session-1"),
        None,
        Some(&cfg),
        Some(store),
        None,
    )?;
    Ok(serde_json::from_str(&out)?)
}

fn run_web_fetch(
    manifest: &AgentManifest,
    skill: Option<&str>,
    url: &str,
) -> anyhow::Result<Result<Value, String>> {
    let temp = tempdir()?;
    write_skill(temp.path(), skill)?;
    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    match registry.execute(
        "web_fetch",
        manifest,
        &policy,
        temp.path(),
        None,
        &json!({
            "url": url,
            "timeout_secs": 2,
            "max_chars": 128
        })
        .to_string(),
        None,
        None,
        None,
        None,
        None,
    ) {
        Ok(body) => Ok(Ok(serde_json::from_str(&body)?)),
        Err(err) => Ok(Err(err.to_string())),
    }
}

fn seed_credential(store: &GatewayStore, credential_id: &str) -> anyhow::Result<()> {
    let cred = CredentialRecord {
        credential_id: credential_id.to_string(),
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
    store.upsert_credential(&cred)?;
    Ok(())
}

fn run_credential_request(
    manifest: &AgentManifest,
    skill: Option<&str>,
    url: &str,
) -> anyhow::Result<Value> {
    let temp = tempdir()?;
    write_skill(temp.path(), skill)?;
    let store = Arc::new(GatewayStore::open(temp.path())?);
    seed_credential(&store, "cred_cross_tool")?;

    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    let out = registry.execute(
        "credential_request",
        manifest,
        &policy,
        temp.path(),
        None,
        &json!({
            "credential_id": "cred_cross_tool",
            "url": url
        })
        .to_string(),
        Some("root-1/session-1"),
        None,
        None,
        Some(store),
        None,
    )?;
    Ok(serde_json::from_str(&out)?)
}

fn run_credential_setup(
    manifest: &AgentManifest,
    skill: Option<&str>,
    url: &str,
) -> anyhow::Result<Value> {
    let temp = tempdir()?;
    write_skill(temp.path(), skill)?;
    let store = Arc::new(GatewayStore::open(temp.path())?);
    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    let out = registry.execute(
        "credential_setup",
        manifest,
        &policy,
        temp.path(),
        None,
        &json!({
            "service": "github",
            "steps": [{
                "step_type": "api_call",
                "method": "GET",
                "url": url
            }]
        })
        .to_string(),
        Some("root-1/session-1"),
        None,
        None,
        Some(store),
        None,
    )?;
    Ok(serde_json::from_str(&out)?)
}

#[test]
fn missing_declaration_fails_shut_across_outbound_tools() -> anyhow::Result<()> {
    let url = "http://127.0.0.1:65535/parity";

    let sandbox = run_sandbox_exec(
        &sandbox_manifest("parity.sandbox.missing"),
        None,
        &format!("curl {}", url),
    )?;
    assert_eq!(sandbox["ok"], false);
    assert_eq!(sandbox["error_type"], "missing_remote_access_declaration");

    let web = run_web_fetch(&web_manifest("parity.web.missing"), None, url)?;
    let web_err = web.expect_err("web_fetch should fail shut");
    assert!(
        web_err.contains("without a parsable metadata.autonoetic.remote_access declaration"),
        "unexpected web error: {web_err}"
    );

    let credential_request = run_credential_request(
        &credential_manifest("parity.cred.request.missing"),
        None,
        url,
    )?;
    // Post-#1103 contract: with an empty allowed_hosts scope, credential_request
    // surfaces out-of-scope hosts as a credential-scope permission denial —
    // never a declaration-layer type — so the error taxonomy doesn't depend on
    // which layer tripped first. The constitutional property (fail shut, no
    // secret sent) is preserved; the shape is deliberately different from the
    // sandbox/web declaration types.
    assert_eq!(credential_request["ok"], false);
    assert_eq!(credential_request["error_type"], "permission");
    assert!(
        credential_request["message"]
            .as_str()
            .is_some_and(|m| m.contains("not authorized for host")),
        "credential_request missing-declaration denial: {credential_request}"
    );

    let credential_setup =
        run_credential_setup(&credential_manifest("parity.cred.setup.missing"), None, url)?;
    assert_eq!(credential_setup["ok"], false);
    assert_eq!(
        credential_setup["error_type"],
        "missing_remote_access_declaration"
    );

    Ok(())
}

#[test]
fn undeclared_target_fails_shut_across_outbound_tools() -> anyhow::Result<()> {
    let url = "http://127.0.0.1:65535/parity";
    let skill = remote_access_exact_host_skill("api.allowed.example");

    let sandbox = run_sandbox_exec(
        &sandbox_manifest("parity.sandbox.target"),
        Some(skill.as_str()),
        &format!("curl {}", url),
    )?;
    assert_eq!(sandbox["ok"], false);
    assert_eq!(sandbox["error_type"], "undeclared_remote_pattern");

    let web = run_web_fetch(
        &web_manifest("parity.web.target"),
        Some(skill.as_str()),
        url,
    )?;
    let web_err = web.expect_err("web_fetch should fail shut");
    assert!(
        web_err.contains("is not covered by metadata.autonoetic.remote_access target declarations"),
        "unexpected web error: {web_err}"
    );

    let credential_request = run_credential_request(
        &credential_manifest("parity.cred.request.target"),
        Some(skill.as_str()),
        url,
    )?;
    // Same post-#1103 shape as the missing-declaration case: empty
    // allowed_hosts ⇒ every host is out of the credential's scope ⇒
    // credential-scope permission denial, not a declaration-layer type.
    assert_eq!(credential_request["ok"], false);
    assert_eq!(credential_request["error_type"], "permission");
    assert!(
        credential_request["message"]
            .as_str()
            .is_some_and(|m| m.contains("not authorized for host")),
        "credential_request undeclared-target denial: {credential_request}"
    );

    let credential_setup = run_credential_setup(
        &credential_manifest("parity.cred.setup.target"),
        Some(skill.as_str()),
        url,
    )?;
    assert_eq!(credential_setup["ok"], false);
    assert_eq!(credential_setup["error_type"], "undeclared_remote_target");

    Ok(())
}
