//! Credential management tools — credential.check, credential.request, credential.setup.
//!
//! Phase A (MVP): Pre-configured credentials.
//! - credential.check: Query stored credentials by service name
//! - credential.request: Gateway-side HTTP client using stored credentials
//!
//! Phase B (Automated Registration):
//! - credential.setup: Multi-step server-side credential registration flow

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::{AgentManifest, CredentialRecord, CredentialSetupStep};
use autonoetic_types::capability::Capability;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(CredentialCheckTool));
    registry.register(Box::new(CredentialRequestTool));
    registry.register(Box::new(CredentialSetupTool));
}

// ---------------------------------------------------------------------------
// credential.check
// ---------------------------------------------------------------------------

/// Query stored credentials by service name.
/// Returns available credentials, their expiry status, and inject_as metadata.
struct CredentialCheckTool;

impl NativeTool for CredentialCheckTool {
    fn name(&self) -> &'static str {
        "credential.check"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "credential.check".to_string(),
            description: "Check available credentials for a service. Returns credential metadata (not the secret values). Use this before credential.request to verify a credential exists.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "service": {
                        "type": "string",
                        "description": "Service name (e.g. 'github', 'stripe', 'slack')"
                    }
                },
                "required": ["service"]
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::CredentialAccess { .. }))
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            service: String,
        }

        let args: Args = serde_json::from_str(arguments_json)?;

        // Check service-scoped authorization
        let service_allowed = manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::CredentialAccess { services } if services.iter().any(|s| s == "*" || s == &args.service))
        });
        if !service_allowed {
            return Ok(json!({
                "ok": false,
                "error": format!("Credential access denied for service: {}", args.service),
                "approval_required": true,
                "reason": format!("Access to {} credentials requires approval", args.service),
            })
            .to_string());
        }

        let Some(store) = gateway_store else {
            return Ok(json!({
                "ok": false,
                "error": "Gateway store not available"
            })
            .to_string());
        };

        let credentials = store.list_credentials_by_service(&args.service)?;

        let results: Vec<serde_json::Value> = credentials
            .iter()
            .map(|c| {
                let mut obj = json!({
                    "credential_id": c.credential_id,
                    "service": c.service,
                    "inject_as": c.inject_as,
                    "created_by_agent": c.created_by_agent,
                });
                // Check expiry using proper DateTime parsing
                if let Some(ref exp_str) = c.expires_at {
                    match chrono::DateTime::parse_from_rfc3339(exp_str) {
                        Ok(expiry) => {
                            let now = chrono::Utc::now();
                            obj["expired"] = json!(expiry < now);
                        }
                        Err(_) => {
                            obj["expired"] = json!(null);
                            obj["expires_at_parse_error"] = json!(true);
                        }
                    }
                    obj["expires_at"] = json!(exp_str);
                }
                obj
            })
            .collect();

        Ok(json!({
            "ok": true,
            "service": args.service,
            "credentials": results,
            "count": results.len()
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// credential.request
// ---------------------------------------------------------------------------

/// Make an authenticated HTTP request using a stored credential.
/// The gateway handles auth injection; the secret never reaches the LLM.
struct CredentialRequestTool;

#[derive(Debug, Serialize, Deserialize)]
struct CredentialRequestArgs {
    credential_id: String,
    method: Option<String>,
    url: String,
    headers: Option<std::collections::HashMap<String, String>>,
    body: Option<serde_json::Value>,
    /// Path to extract secret from vault (e.g. "token" for Bearer injection)
    inject_secret_as: Option<String>,
}

impl NativeTool for CredentialRequestTool {
    fn name(&self) -> &'static str {
        "credential.request"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "credential.request".to_string(),
            description: "Make an authenticated HTTP request using a stored credential. The gateway injects the secret (e.g. as Authorization header); the secret value never appears in the LLM context. Returns the HTTP response body.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "credential_id": {
                        "type": "string",
                        "description": "Credential ID from credential.check"
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                        "description": "HTTP method (default: GET)"
                    },
                    "url": {
                        "type": "string",
                        "description": "Full URL for the request"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Additional headers to include"
                    },
                    "body": {
                        "type": "object",
                        "description": "JSON body for POST/PUT/PATCH requests"
                    },
                    "inject_secret_as": {
                        "type": "string",
                        "description": "How to inject the secret: 'bearer', 'header:X-Custom-Header', or env var name"
                    }
                },
                "required": ["credential_id", "url"]
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::CredentialAccess { .. }))
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: CredentialRequestArgs = serde_json::from_str(arguments_json)?;

        let Some(store) = gateway_store else {
            return Ok(json!({
                "ok": false,
                "error": "Gateway store not available"
            })
            .to_string());
        };

        // Look up the credential to check service-scoped authorization
        let Some(cred) = store.get_credential(&args.credential_id)? else {
            return Ok(json!({
                "ok": false,
                "error": format!("Credential not found: {}", args.credential_id)
            })
            .to_string());
        };

        // Check service-scoped authorization
        let service_allowed = manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::CredentialAccess { services } if services.iter().any(|s| s == "*" || s == &cred.service))
        });
        if !service_allowed {
            return Ok(json!({
                "ok": false,
                "error": format!("Credential access denied for service: {}", cred.service),
                "approval_required": true,
                "reason": format!("Access to {} credentials requires approval", cred.service),
            })
            .to_string());
        }

        // Check network policy
        let url_host = extract_host(&args.url)?;
        if !policy.can_connect_net(&url_host) {
            return Ok(json!({
                "ok": false,
                "error": format!("Network access denied for host: {}", url_host),
                "approval_required": true,
                "reason": format!("HTTP request to {} requires approval", url_host),
            })
            .to_string());
        }

        // Bind credential to destination host: the URL host must match
        // one of the credential's allowed_hosts (if configured).
        if !cred.allowed_hosts.is_empty()
            && !cred
                .allowed_hosts
                .iter()
                .any(|h| h == "*" || h == &url_host)
        {
            return Ok(json!({
                "ok": false,
                "error": format!(
                    "Credential '{}' for service '{}' is not authorized for host '{}'. Allowed hosts: {:?}",
                    args.credential_id, cred.service, url_host, cred.allowed_hosts
                ),
            })
            .to_string());
        }

        // Check expiry using proper DateTime parsing — fail-closed on parse errors
        if let Some(ref exp_str) = cred.expires_at {
            match chrono::DateTime::parse_from_rfc3339(exp_str) {
                Ok(expiry) => {
                    let now = chrono::Utc::now();
                    if expiry < now {
                        return Ok(json!({
                            "ok": false,
                            "error": format!("Credential expired at {}", exp_str),
                            "expired": true,
                        })
                        .to_string());
                    }
                }
                Err(_) => {
                    return Ok(json!({
                        "ok": false,
                        "error": format!("Credential has unparseable expiry timestamp: {}", exp_str),
                        "expired": null,
                        "expires_at_parse_error": true,
                    })
                    .to_string());
                }
            }
        }

        // Fetch secret from Vault
        let vault_path = std::env::var("AUTONOETIC_VAULT_PATH").ok().and_then(|p| {
            let path = std::path::PathBuf::from(p);
            if path.exists() {
                Some(path)
            } else {
                None
            }
        });

        let secret_value: Option<String> = vault_path.as_ref().and_then(|vp| {
            use secrecy::ExposeSecret;
            let vault = crate::vault::Vault::load_from_file(vp).ok()?;
            vault
                .get_secret(&cred.secret_name)
                .map(|s| s.expose_secret().to_string())
        });

        if secret_value.is_none() {
            return Ok(json!({
                "ok": false,
                "error": format!("Secret '{}' not found in vault for credential {}", cred.secret_name, args.credential_id),
            })
            .to_string());
        }

        // Build the HTTP request
        let method = args.method.as_deref().unwrap_or("GET");
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let mut req = client.request(reqwest::Method::from_bytes(method.as_bytes())?, &args.url);

        // Add custom headers
        if let Some(headers) = &args.headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        // Inject secret based on credential's stored inject_as metadata.
        // Runtime args can only override if the credential has no inject_as configured.
        if let Some(ref secret) = secret_value {
            let effective_inject = cred.inject_as.as_ref().or(args.inject_secret_as.as_ref());

            if let Some(inject) = effective_inject {
                if inject == "bearer" || inject == "Authorization" {
                    req = req.header("Authorization", format!("Bearer {}", secret));
                } else if inject.starts_with("header:") {
                    let header_name = &inject["header:".len()..];
                    req = req.header(header_name, secret);
                } else {
                    // Default: Bearer token
                    req = req.header("Authorization", format!("Bearer {}", secret));
                }
            } else {
                // Default: Bearer token
                req = req.header("Authorization", format!("Bearer {}", secret));
            }
        }

        // Add body for POST/PUT/PATCH
        if let Some(body) = &args.body {
            req = req.json(body);
        }

        let resp = req.send()?;
        let status = resp.status().as_u16();
        let body = resp.text()?;

        // Sanitize response: redact secret value to prevent leakage into LLM context
        let sanitized_body = if let Some(ref secret) = secret_value {
            body.replace(secret.as_str(), "[REDACTED]")
        } else {
            body
        };

        // Try to parse as JSON for cleaner output
        let body_value: serde_json::Value =
            serde_json::from_str(&sanitized_body).unwrap_or(json!(sanitized_body));

        Ok(json!({
            "ok": true,
            "status": status,
            "body": body_value,
        })
        .to_string())
    }
}

fn extract_host(url: &str) -> anyhow::Result<String> {
    let parsed = url::Url::parse(url)?;
    Ok(parsed.host_str().unwrap_or("").to_string())
}

// ---------------------------------------------------------------------------
// credential.setup
// ---------------------------------------------------------------------------

/// Multi-step server-side credential registration flow.
/// Executes setup steps (API calls, user prompts, user actions) to register
/// new credentials. Secrets extracted from API responses are stored in the
/// vault and never returned to the LLM.
struct CredentialSetupTool;

#[derive(Debug, Serialize, Deserialize)]
struct CredentialSetupArgs {
    service: String,
    steps: Vec<CredentialSetupStep>,
    /// Optional: credential ID to use. Generated if not provided.
    credential_id: Option<String>,
    /// Optional: expiry timestamp for the credential.
    expires_at: Option<String>,
    /// Optional: how to inject the secret (bearer, header:X-Custom, env var name).
    inject_as: Option<String>,
    /// Optional: hosts this credential is bound to.
    allowed_hosts: Option<Vec<String>>,
}

impl NativeTool for CredentialSetupTool {
    fn name(&self) -> &'static str {
        "credential.setup"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "credential.setup".to_string(),
            description: "Register a new credential through a multi-step setup flow. The gateway executes steps server-side: making API calls, extracting secrets from responses, and storing them in the vault. Secrets never appear in the LLM context.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "service": {
                        "type": "string",
                        "description": "Service name (e.g. 'github', 'stripe', 'slack')"
                    },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_type": {
                                    "type": "string",
                                    "enum": ["api_call", "user_prompt", "user_action"]
                                },
                                "method": {"type": "string"},
                                "url": {"type": "string"},
                                "headers": {"type": "object"},
                                "body": {"type": "object"},
                                "extract_secrets": {"type": "object"},
                                "extract_public": {"type": "object"},
                                "message": {"type": "string"},
                                "secret_fields": {"type": "array"},
                                "instruction": {"type": "string"},
                                "data_refs": {"type": "array"}
                            }
                        },
                        "description": "Ordered sequence of setup steps to execute"
                    },
                    "credential_id": {
                        "type": "string",
                        "description": "Optional credential ID (generated if not provided)"
                    },
                    "expires_at": {
                        "type": "string",
                        "description": "Optional expiry timestamp (ISO 8601)"
                    },
                    "inject_as": {
                        "type": "string",
                        "description": "How to inject the secret: 'bearer', 'header:X-Custom-Header', or env var name"
                    },
                    "allowed_hosts": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Hosts this credential is bound to"
                    }
                },
                "required": ["service", "steps"]
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::CredentialAccess { .. }))
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: CredentialSetupArgs = serde_json::from_str(arguments_json)?;

        // Check service-scoped authorization
        let service_allowed = manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::CredentialAccess { services } if services.iter().any(|s| s == "*" || s == &args.service))
        });
        if !service_allowed {
            return Ok(json!({
                "ok": false,
                "error": format!("Credential setup denied for service: {}", args.service),
                "approval_required": true,
                "reason": format!("Setup for {} credentials requires approval", args.service),
            })
            .to_string());
        }

        let Some(store) = gateway_store else {
            return Ok(json!({
                "ok": false,
                "error": "Gateway store not available"
            })
            .to_string());
        };

        // Check network policy for all API call steps before executing any
        for step in &args.steps {
            if let CredentialSetupStep::ApiCall { url, .. } = step {
                let host = extract_host(url)?;
                if !policy.can_connect_net(&host) {
                    return Ok(json!({
                        "ok": false,
                        "error": format!("Network access denied for host: {}", host),
                        "approval_required": true,
                        "reason": format!("API call to {} requires approval", host),
                    })
                    .to_string());
                }
            }
        }

        // Load vault
        let vault_path = std::env::var("AUTONOETIC_VAULT_PATH").ok().and_then(|p| {
            let path = std::path::PathBuf::from(p);
            if path.exists() {
                Some(path)
            } else {
                None
            }
        });

        let mut vault = vault_path
            .as_ref()
            .and_then(|vp| crate::vault::Vault::load_from_file(vp).ok());

        if vault.is_none() {
            return Ok(json!({
                "ok": false,
                "error": "Vault not available. Set AUTONOETIC_VAULT_PATH to a valid vault file."
            })
            .to_string());
        }
        let vault = vault.as_mut().unwrap();

        // Execute steps
        let credential_id = args.credential_id.clone().unwrap_or_else(|| {
            format!(
                "cred_{}_{}",
                args.service,
                uuid::Uuid::new_v4().to_string().replace('-', "")
            )
        });
        let mut secret_names = Vec::new();
        let mut public_data = serde_json::Map::new();
        let mut step_results = Vec::new();

        for (i, step) in args.steps.iter().enumerate() {
            match step {
                CredentialSetupStep::ApiCall {
                    method,
                    url,
                    headers,
                    body,
                    extract_secrets,
                    extract_public,
                } => {
                    // Make the HTTP request
                    let http_method = method.as_deref().unwrap_or("POST");
                    let client = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(30))
                        .build()?;

                    let mut req =
                        client.request(reqwest::Method::from_bytes(http_method.as_bytes())?, url);

                    for (k, v) in headers {
                        req = req.header(k, v);
                    }

                    if let Some(b) = body {
                        req = req.json(b);
                    }

                    let resp = req.send()?;
                    let status = resp.status().as_u16();
                    let resp_body = resp.text()?;

                    // Parse response
                    let resp_value: serde_json::Value =
                        serde_json::from_str(&resp_body).unwrap_or(json!(resp_body));

                    // Extract secrets
                    for (secret_name, json_path) in extract_secrets {
                        if let Some(val) = extract_json_path(&resp_value, json_path) {
                            vault.set_secret(secret_name, val.clone());
                            secret_names.push(secret_name.clone());
                        }
                    }

                    // Extract public data
                    for (field_name, json_path) in extract_public {
                        if let Some(val) = extract_json_path(&resp_value, json_path) {
                            public_data.insert(field_name.clone(), json!(val));
                        }
                    }

                    step_results.push(json!({
                        "step": i,
                        "step_type": "api_call",
                        "status": status,
                        "url": url,
                    }));
                }
                CredentialSetupStep::UserPrompt {
                    message,
                    secret_fields,
                } => {
                    // For UserPrompt, we return the prompt details and suspend
                    // The actual secret entry happens through the approval/human channel
                    step_results.push(json!({
                        "step": i,
                        "step_type": "user_prompt",
                        "message": message,
                        "secret_fields": secret_fields,
                        "status": "awaiting_human_input",
                    }));
                }
                CredentialSetupStep::UserAction {
                    instruction,
                    data_refs,
                } => {
                    step_results.push(json!({
                        "step": i,
                        "step_type": "user_action",
                        "instruction": instruction,
                        "data_refs": data_refs,
                        "status": "completed",
                    }));
                }
            }
        }

        // Persist vault
        if let Some(vp) = &vault_path {
            vault.persist_to_file(vp)?;
        }

        // Create credential record if we extracted at least one secret
        if !secret_names.is_empty() {
            let cred = CredentialRecord {
                credential_id: credential_id.clone(),
                service: args.service.clone(),
                secret_name: secret_names[0].clone(),
                inject_as: args.inject_as.clone(),
                created_by_agent: Some(manifest.agent.id.clone()),
                expires_at: args.expires_at.clone(),
                shared_with: vec![],
                allowed_hosts: args.allowed_hosts.clone().unwrap_or_default(),
            };
            store.upsert_credential(&cred)?;
        }

        Ok(json!({
            "ok": true,
            "credential_id": credential_id,
            "service": args.service,
            "secrets_stored": secret_names.len(),
            "public_data": public_data,
            "steps": step_results,
        })
        .to_string())
    }
}

/// Extract a value from JSON at a dot-separated or $-prefixed path.
fn extract_json_path(value: &serde_json::Value, path: &str) -> Option<String> {
    let path = path.trim_start_matches('$').trim_start_matches('.');
    if path.is_empty() {
        return None;
    }
    let segments: Vec<&str> = path.split('.').collect();
    let mut cur = value;
    for seg in &segments {
        cur = cur.get(*seg)?;
    }
    match cur {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
