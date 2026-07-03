//! Credential management tools — credential.check, credential.request, credential.setup.
//!
//! Phase A (MVP): Pre-configured credentials.
//! - credential.check: Query stored credentials by service name
//! - credential.request: Gateway-side HTTP client using stored credentials
//!
//! Phase B (Automated Registration):
//! - credential.setup: Multi-step server-side credential registration flow
//!   Extended with `skill_url` to ingest a skill.md onboarding spec (HTTPS URL
//!   or local filename from gateway skills/ directory), `user_input` step support
//!   (gateway returns early, agent calls user.ask), and `resume_vars` to continue
//!   after user input is collected.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::network_policy::DeclarationRequirement;
use crate::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::{AgentManifest, CredentialRecord, CredentialSetupStep};
use autonoetic_types::background::ScheduledAction;
use autonoetic_types::capability::Capability;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn vault_dir(gateway_dir: Option<&Path>, agent_dir: &Path) -> PathBuf {
    gateway_dir
        .and_then(|gd| gd.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| agent_dir.to_path_buf())
}

/// Execute a blocking HTTP request inside `block_in_place` to avoid deadlocking
/// the tokio runtime. `reqwest::blocking::Client` creates its own internal runtime,
/// which can deadlock when called directly from an async context.
fn blocking_http_request<F, R>(f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| {
            handle.block_on(async { tokio::task::spawn_blocking(f).await })
        })
        .map_err(|_| anyhow::anyhow!("blocking HTTP request panicked"))?
    } else {
        f()
    }
}

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(CredentialCheckTool));
    registry.register(Box::new(CredentialRequestTool));
    registry.register(Box::new(CredentialRefreshTool));
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
        "credential_check"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "credential_check".to_string(),
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
            let message = format!("Credential access denied for service: {}", args.service);
            return Ok(json!({
                "ok": false,
                "error_type": "permission",
                "message": message,
                "repair_hint": "Request CredentialAccess for this service or choose an authorized service.",
                "error": "credential_access_requires_approval",
                "approval_required": true,
                "reason": format!("Access to {} credentials requires approval", args.service),
            })
            .to_string());
        }

        let Some(store) = gateway_store else {
            return Ok(autonoetic_types::tool_error::ToolError::resource(
                "Gateway store not available",
                None::<String>,
            )
            .to_error_response());
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
    /// Approval request ID from a previous approval-required response.
    approval_ref: Option<String>,
}

impl NativeTool for CredentialRequestTool {
    fn name(&self) -> &'static str {
        "credential_request"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "credential_request".to_string(),
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
                    },
                    "approval_ref": {
                        "type": "string",
                        "description": "Approval request ID (from previous approval_required response). Provide this after operator approval to run a network-policy-blocked request."
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
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: CredentialRequestArgs = serde_json::from_str(arguments_json)?;
        crate::runtime::tools::ensure_safe_credential_id_reference(&args.credential_id)?;

        let Some(store) = gateway_store else {
            return Ok(autonoetic_types::tool_error::ToolError::resource(
                "Gateway store not available",
                None::<String>,
            )
            .to_error_response());
        };

        // Look up the credential to check service-scoped authorization
        let Some(cred) = store.get_credential(&args.credential_id)? else {
            return Ok(autonoetic_types::tool_error::ToolError::not_found(
                format!("credential '{}'", args.credential_id),
                Some(
                    "Use credential.check to list available credentials for this service."
                        .to_string(),
                ),
            )
            .to_error_response());
        };

        // Check service-scoped authorization
        let service_allowed = manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::CredentialAccess { services } if services.iter().any(|s| s == "*" || s == &cred.service))
        });
        if !service_allowed {
            let message = format!("Credential access denied for service: {}", cred.service);
            return Ok(json!({
                "ok": false,
                "error_type": "permission",
                "message": message,
                "repair_hint": "Request CredentialAccess for this service or choose an authorized service.",
                "error": "credential_access_requires_approval",
                "approval_required": true,
                "reason": format!("Access to {} credentials requires approval", cred.service),
            })
            .to_string());
        }

        let url_host = extract_host(&args.url)?;
        let approval_validated = if let Some(approval_ref) = args.approval_ref.as_deref() {
            match store.get_approval(approval_ref)? {
                Some(approval)
                    if approval.status
                        == Some(autonoetic_types::background::ApprovalStatus::Approved) =>
                {
                    match approval.action {
                        ScheduledAction::CredentialRequest {
                            credential_id,
                            url,
                            method,
                            headers,
                            body,
                            inject_secret_as,
                            ..
                        } => {
                            let method_matches = method.as_deref() == args.method.as_deref();
                            let headers_matches = headers == args.headers;
                            let body_matches = body == args.body;
                            let inject_matches = inject_secret_as == args.inject_secret_as;
                            if credential_id == args.credential_id
                                && url == args.url
                                && method_matches
                                && headers_matches
                                && body_matches
                                && inject_matches
                            {
                                true
                            } else {
                                return Ok(autonoetic_types::tool_error::ToolError::validation(
                                    "approval_ref does not match this credential.request payload",
                                    Some("Ensure all parameters match the original request that created the approval.".to_string()),
                                ).to_error_response());
                            }
                        }
                        _ => {
                            return Ok(autonoetic_types::tool_error::ToolError::validation(
                                format!("approval_ref '{}' is not for credential.request", approval_ref),
                                Some("Use the approval_ref from a credential.request approval response.".to_string()),
                            ).to_error_response());
                        }
                    }
                }
                _ => {
                    return Ok(autonoetic_types::tool_error::ToolError::not_found(
                        format!("approval '{}'", approval_ref),
                        Some("The approval may not exist, may have expired, or may not yet be decided.".to_string()),
                    ).to_error_response());
                }
            }
        } else {
            false
        };

        // Session approval grants: if the operator already approved a request
        // targeting this host within the same root session, skip new approval.
        let approval_validated = if approval_validated {
            true
        } else if let Some(sid) = _session_id {
            let root_sid = crate::runtime::content_store::root_session_id(sid);
            if !url_host.is_empty()
                && store.session_grants_cover_targets(&root_sid, &[url_host.clone()])
            {
                tracing::info!(
                    target: "credential_request",
                    agent_id = %manifest.agent.id,
                    root_session_id = %root_sid,
                    host = %url_host,
                    "Session grant covers host — auto-approving credential request"
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        if !approval_validated {
            if let Err(violation) = crate::runtime::network_policy::enforce_remote_target_policy(
                manifest,
                agent_dir,
                &url_host,
                Some(&args.url),
                DeclarationRequirement::Required,
            ) {
                let action = ScheduledAction::CredentialRequest {
                    credential_id: args.credential_id.clone(),
                    url: args.url.clone(),
                    method: args.method.clone(),
                    headers: args.headers.clone(),
                    body: args.body.clone(),
                    inject_secret_as: args.inject_secret_as.clone(),
                    payload: Some(json!({
                        "host": url_host.clone(),
                        "retry_field": "approval_ref",
                        "source_tool": "credential_request",
                        "policy_violation": violation.error_type,
                    })),
                };
                let reason = format!(
                    "Credential request to {} requires approval (policy: {})",
                    url_host, violation.error_type
                );

                let gate = crate::runtime::human_gate::GateService::new(store.clone());
                let gate_result = gate.check(
                    crate::runtime::human_gate::GateRequest {
                        kind: crate::runtime::human_gate::GateKind::Approval {
                            action: action.clone(),
                            targets: vec![url_host.clone()],
                            match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
                        },
                        manifest,
                        session_id: _session_id,
                        run_context: _run_context,
                        config: _config,
                        context: crate::runtime::human_gate::DecisionContext::tier2(
                            format!("credential request to {}", url_host),
                            format!(
                                "remote target policy not satisfied for {} (policy: {})",
                                url_host, violation.error_type
                            ),
                            format!("uses stored credential for {}", url_host),
                            "Approve if this host is an expected credential target for the agent's task; reject if the host is unexpected",
                        ),
                        summary: format!("Credential request to {}", url_host),
                        approval_ref: None,
                        pre_validated: false,
                        cache_backfill: None,
                        request_id: None,
                        turn_id: None,
                    },
                )?;
                match gate_result {
                    crate::runtime::human_gate::GateResult::Cleared { .. } => {}
                    crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
                        return Ok(json!({
                            "ok": false,
                            "approval_required": true,
                            "approval_already_pending": true,
                            "request_id": gate_id,
                            "suspended": true,
                            "reason": reason,
                            "repair_hint": "Wait for the existing approval to be resolved.",
                            "approval": {
                                "kind": "credential_request",
                                "summary": format!("Credential request to {}", url_host),
                                "retry_field": "approval_ref"
                            }
                        })
                        .to_string());
                    }
                    crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
                        return Ok(json!({
                            "ok": false,
                            "error_type": "permission",
                            "message": format!(
                                "Execution suspended pending operator approval ({}). Retry credential.request with approval_ref after approval.",
                                gate_id
                            ),
                            "repair_hint": "Wait for approval and retry this exact request using approval_ref.",
                            "error": "approval_required",
                            "approval_required": true,
                            "request_id": gate_id,
                            "suspended": true,
                            "reason": reason,
                            "approval": {
                                "kind": "credential_request",
                                "summary": format!("Credential request to {}", url_host),
                                "reason": format!(
                                    "Credential request to {} requires approval because remote target policy is not declared for this host.",
                                    url_host
                                ),
                                "retry_field": "approval_ref"
                            }
                        }).to_string());
                    }
                    other => {
                        tracing::warn!(
                            target: "credential_request",
                            gate_result = ?other,
                            "Unexpected gate result for credential.request remote target gate"
                        );
                    }
                }
            }
        }

        // Check network policy unless this exact request has been explicitly approved.
        if !policy.can_connect_net(&url_host).is_allowed() && !approval_validated {
            let action = ScheduledAction::CredentialRequest {
                credential_id: args.credential_id.clone(),
                url: args.url.clone(),
                method: args.method.clone(),
                headers: args.headers.clone(),
                body: args.body.clone(),
                inject_secret_as: args.inject_secret_as.clone(),
                payload: Some(json!({
                    "host": url_host.clone(),
                    "retry_field": "approval_ref",
                    "source_tool": "credential_request",
                    "policy_violation": "network_access_denied",
                })),
            };
            let reason = format!("HTTP request to {} requires approval", url_host);

            let gate = crate::runtime::human_gate::GateService::new(store.clone());
            let gate_result = gate.check(
                crate::runtime::human_gate::GateRequest {
                    kind: crate::runtime::human_gate::GateKind::Approval {
                        action: action.clone(),
                        targets: vec![url_host.clone()],
                        match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
                    },
                    manifest,
                    session_id: _session_id,
                    run_context: _run_context,
                    config: _config,
                    context: crate::runtime::human_gate::DecisionContext::tier2(
                        format!("credential request to {}", url_host),
                        format!(
                            "{} is not in an approved network grant (NetworkAccess policy)",
                            url_host
                        ),
                        format!("uses stored credential for {}", url_host),
                        "Approve if this host is an expected credential target for the agent's task; reject if the host is unexpected",
                    ),
                    summary: format!("Credential request to {}", url_host),
                    approval_ref: None,
                    pre_validated: false,
                    cache_backfill: None,
                    request_id: None,
                    turn_id: None,
                },
            )?;
            match gate_result {
                crate::runtime::human_gate::GateResult::Cleared { .. } => {}
                crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
                    return Ok(json!({
                        "ok": false,
                        "approval_required": true,
                        "approval_already_pending": true,
                        "request_id": gate_id,
                        "suspended": true,
                        "reason": reason,
                        "repair_hint": "Wait for the existing approval to be resolved.",
                        "approval": {
                            "kind": "credential_request",
                            "summary": format!("Credential request to {}", url_host),
                            "retry_field": "approval_ref"
                        }
                    })
                    .to_string());
                }
                crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
                    return Ok(json!({
                        "ok": false,
                        "error_type": "permission",
                        "message": format!(
                            "Execution suspended pending operator approval ({}). Retry credential.request with approval_ref after approval.",
                            gate_id
                        ),
                        "repair_hint": "Wait for approval and retry this exact request using approval_ref.",
                        "error": "network_access_denied",
                        "approval_required": true,
                        "request_id": gate_id,
                        "suspended": true,
                        "reason": reason,
                        "approval": {
                            "kind": "credential_request",
                            "summary": format!("Credential request to {}", url_host),
                            "reason": format!("HTTP request to {} requires approval", url_host),
                            "retry_field": "approval_ref"
                        }
                    }).to_string());
                }
                other => {
                    tracing::warn!(
                        target: "credential_request",
                        gate_result = ?other,
                        "Unexpected gate result for credential.request network gate"
                    );
                }
            }
        }

        // Bind credential to destination host: the URL host must match
        // one of the credential's allowed_hosts (if configured).
        if !cred.allowed_hosts.is_empty()
            && !cred
                .allowed_hosts
                .iter()
                .any(|h| h == "*" || normalize_allowed_host(h) == url_host)
        {
            return Ok(autonoetic_types::tool_error::ToolError::permission(
                format!(
                    "Credential '{}' for service '{}' is not authorized for host '{}'. Allowed hosts: {:?}",
                    args.credential_id, cred.service, url_host, cred.allowed_hosts
                ),
            ).to_error_response());
        }

        // Check expiry using proper DateTime parsing — fail-closed on parse errors
        if let Some(ref exp_str) = cred.expires_at {
            match chrono::DateTime::parse_from_rfc3339(exp_str) {
                Ok(expiry) => {
                    let now = chrono::Utc::now();
                    if expiry < now {
                        return Ok(autonoetic_types::tool_error::ToolError::resource(
                            format!("Credential expired at {}", exp_str),
                            Some("Use credential.refresh to obtain a new token, or set up a new credential.".to_string()),
                        ).to_error_response());
                    }
                }
                Err(_) => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        format!("Credential has unparseable expiry timestamp: {}", exp_str),
                        None::<String>,
                    )
                    .to_error_response());
                }
            }
        }

        // Vault auto-init + resolve path
        let vdir = vault_dir(_gateway_dir, agent_dir);
        crate::vault::ensure_default_key(&vdir)?;
        let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::vault::default_vault_path(&vdir));

        let secret_value: Option<String> = {
            use secrecy::ExposeSecret;
            crate::vault::Vault::load_from_file(&vault_path)
                .ok()
                .and_then(|v| {
                    v.get_secret(&cred.secret_name)
                        .map(|s| s.expose_secret().to_string())
                })
        };

        if secret_value.is_none() {
            return Ok(autonoetic_types::tool_error::ToolError::not_found(
                format!("secret '{}' in vault for credential {}", cred.secret_name, args.credential_id),
                Some("The credential record exists but the vault secret is missing. Re-register the credential.".to_string()),
            ).to_error_response());
        }

        let method = args.method.as_deref().unwrap_or("GET").to_string();
        let url = args.url.clone();
        let headers = args.headers.clone();
        let body = args.body.clone();
        let mut secret_value_clone = secret_value.clone();
        let mut cred = cred.clone();
        let effective_inject = cred.inject_as.clone().or(args.inject_secret_as.clone());
        let v_path = vault_path.clone();
        let mut refreshed = false;

        let (status, body) = 'request: loop {
            let method = method.clone();
            let url = url.clone();
            let headers = headers.clone();
            let body = body.clone();
            let svc = secret_value_clone.clone();
            let eff = effective_inject.clone();

            let (status, body) =
                blocking_http_request(move || -> anyhow::Result<(u16, String)> {
                    let client = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(30))
                        .build()?;

                    let mut req =
                        client.request(reqwest::Method::from_bytes(method.as_bytes())?, &url);

                    if let Some(headers) = &headers {
                        for (k, v) in headers {
                            req = req.header(k, v);
                        }
                    }

                    if let Some(ref secret) = svc {
                        let inject = eff.as_ref().map(String::as_str).unwrap_or("bearer");
                        if inject == "bearer" || inject == "Authorization" {
                            req = req.header("Authorization", format!("Bearer {}", secret));
                        } else if inject.starts_with("header:") {
                            req = req.header(&inject["header:".len()..], secret);
                        } else {
                            req = req.header("Authorization", format!("Bearer {}", secret));
                        }
                    }

                    if let Some(ref b) = body {
                        req = req.json(b);
                    }

                    let resp = req.send()?;
                    let status = resp.status().as_u16();
                    let body = resp.text()?;
                    Ok((status, body))
                })?;

            if status == 401 && !refreshed && cred.refresh_url.is_some() {
                match try_auto_refresh(&cred, &store, &v_path) {
                    Ok(updated_cred) => {
                        cred = updated_cred;
                        let vault = crate::Vault::load_from_file(&v_path)?;
                        secret_value_clone = vault
                            .get_secret(&cred.secret_name)
                            .map(|s| s.expose_secret().to_string());
                        refreshed = true;
                        continue 'request;
                    }
                    Err(_) => {
                        break 'request (status, body);
                    }
                }
            }

            break 'request (status, body);
        };

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

/// Normalize an `allowed_hosts` entry for host-only comparison against
/// `extract_host(url)`. Strips an optional port and handles IPv6 forms
/// (e.g. `"localhost:9876"` → `"localhost"`, `"[::1]:8443"` → `"::1"`).
/// The `"*"` wildcard is returned unchanged.
fn normalize_allowed_host(entry: &str) -> String {
    if entry == "*" {
        return entry.to_string();
    }
    url::Url::parse(&format!("http://{}", entry))
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| entry.to_string())
}

enum SkillUrlKind {
    Remote { url: String, host: String },
    Local { path_hint: String },
}

fn classify_skill_url(raw: &str) -> SkillUrlKind {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        let host = url::Url::parse(raw)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_default();
        SkillUrlKind::Remote {
            url: raw.to_string(),
            host,
        }
    } else {
        SkillUrlKind::Local {
            path_hint: raw.to_string(),
        }
    }
}

fn skills_dir(gateway_dir: &Path) -> PathBuf {
    gateway_dir.join("skills")
}

fn validate_local_skill_path(gateway_dir: &Path, path_hint: &str) -> anyhow::Result<PathBuf> {
    let base = skills_dir(gateway_dir);
    let canonical_base = base.canonicalize().unwrap_or_else(|_| base.clone());
    let candidate = if path_hint.starts_with("file://") {
        let url = url::Url::parse(path_hint)
            .map_err(|e| anyhow::anyhow!("invalid file:// skill_url: {}", e))?;
        url.to_file_path().map_err(|_| {
            anyhow::anyhow!("file:// skill_url must point to a local filesystem path")
        })?
    } else {
        let normalized = path_hint.trim_start_matches("./");
        let normalized = normalized.strip_prefix("skills/").unwrap_or(normalized);
        if !normalized.ends_with(".md") {
            anyhow::bail!("local skill_url must be a .md file in the gateway skills/ directory");
        }
        let path = PathBuf::from(normalized);
        if path.is_absolute() {
            anyhow::bail!(
                "absolute local skill_url paths are not allowed; use a path relative to gateway skills/ or a file:// URL under that directory"
            );
        }
        base.join(path)
    };
    if candidate.extension().and_then(|ext| ext.to_str()) != Some("md") {
        anyhow::bail!("local skill_url must be a .md file in the gateway skills/ directory");
    }
    let canonical_target = match std::fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(_) => {
            let resolved = if candidate.is_absolute() {
                candidate.clone()
            } else {
                canonical_base.join(&candidate)
            };
            if !resolved.starts_with(&canonical_base) {
                anyhow::bail!("skill_url path escapes the gateway skills/ directory");
            }
            anyhow::bail!(
                "skill file not found in gateway skills/ directory: {}",
                path_hint
            );
        }
    };
    if !canonical_target.starts_with(&canonical_base) {
        anyhow::bail!("skill_url path escapes the gateway skills/ directory");
    }
    let metadata = std::fs::symlink_metadata(&canonical_target)?;
    if metadata.file_type().is_symlink() {
        let link_target = std::fs::read_link(&canonical_target)?;
        let resolved_link = if link_target.is_absolute() {
            link_target
        } else {
            canonical_target
                .parent()
                .unwrap_or(&canonical_base)
                .join(link_target)
        };
        let canonical_link = match std::fs::canonicalize(&resolved_link) {
            Ok(p) => p,
            Err(_) => {
                anyhow::bail!("symlink in skills/ directory points to invalid target");
            }
        };
        if !canonical_link.starts_with(&canonical_base) {
            anyhow::bail!("symlink in skills/ directory escapes the gateway skills/ directory");
        }
    }
    Ok(canonical_target)
}

// ---------------------------------------------------------------------------
// credential.refresh
// ---------------------------------------------------------------------------

/// Refresh an expired or stale credential using a stored refresh token.
///
/// Sends a POST to `refresh_url` with the refresh token, extracts the new
/// access token (and optionally a new refresh token + expiry) from the
/// response, updates the vault and credential record.
struct CredentialRefreshTool;

impl NativeTool for CredentialRefreshTool {
    fn name(&self) -> &'static str {
        "credential_refresh"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "credential_refresh".to_string(),
            description: "Refresh an expired credential using a stored refresh token. The gateway sends a request to the configured refresh_url, extracts the new access token from the response, and updates the vault. Returns the updated credential metadata without exposing secrets.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "credential_id": {
                        "type": "string",
                        "description": "The credential to refresh."
                    }
                },
                "required": ["credential_id"],
                "additionalProperties": false
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
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: serde_json::Value = serde_json::from_str(arguments_json)?;
        let credential_id = args["credential_id"].as_str().unwrap_or("").to_string();
        if credential_id.is_empty() {
            return Ok(autonoetic_types::tool_error::ToolError::validation(
                "credential_id is required",
                Some("Provide the credential_id of the credential to refresh.".to_string()),
            )
            .to_error_response());
        }
        crate::runtime::tools::ensure_safe_credential_id_reference(&credential_id)?;

        let Some(store) = gateway_store else {
            return Ok(autonoetic_types::tool_error::ToolError::resource(
                "GatewayStore not available",
                None::<String>,
            )
            .to_error_response());
        };

        let cap_service = manifest.capabilities.iter().find_map(|c| {
            if let Capability::CredentialAccess { services } = c {
                Some(services.clone())
            } else {
                None
            }
        });
        let allowed_services = match cap_service {
            Some(s) => s,
            None => {
                return Ok(autonoetic_types::tool_error::ToolError::permission(
                    "CredentialAccess capability required",
                )
                .to_error_response());
            }
        };

        let cred = match store.get_credential(&credential_id)? {
            Some(c) => c,
            None => {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("credential '{}'", credential_id),
                    Some("Use credential.check to list available credentials.".to_string()),
                )
                .to_error_response());
            }
        };

        if !allowed_services
            .iter()
            .any(|s| s == "*" || s == &cred.service)
        {
            return Ok(autonoetic_types::tool_error::ToolError::permission(format!(
                "CredentialAccess does not permit service '{}'",
                cred.service
            ))
            .to_error_response());
        }

        let refresh_url = match &cred.refresh_url {
            Some(u) => u.clone(),
            None => {
                return Ok(autonoetic_types::tool_error::ToolError::validation(
                    "Credential has no refresh_url configured",
                    Some(
                        "Use credential.setup with refresh metadata to enable token refresh."
                            .to_string(),
                    ),
                )
                .to_error_response());
            }
        };

        let refresh_token_secret = match &cred.refresh_token_secret_name {
            Some(s) => s.clone(),
            None => {
                return Ok(autonoetic_types::tool_error::ToolError::validation(
                    "Credential has no refresh_token_secret_name",
                    Some(
                        "Store a refresh token during credential.setup to enable refresh."
                            .to_string(),
                    ),
                )
                .to_error_response());
            }
        };

        let v_dir = vault_dir(gateway_dir, agent_dir);
        crate::vault::ensure_default_key(&v_dir)?;
        let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::vault::default_vault_path(&v_dir));

        let mut vault = crate::Vault::load_from_file(&vault_path)?;
        let refresh_token = match vault.get_secret(&refresh_token_secret) {
            Some(s) => s.expose_secret().to_string(),
            None => {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("refresh token '{}' in vault", refresh_token_secret),
                    Some("The refresh token was not stored in the vault. Re-register the credential with a refresh token.".to_string()),
                ).to_error_response());
            }
        };

        let extract_access = cred
            .refresh_extract_access_token
            .as_deref()
            .unwrap_or("access_token");
        let extract_refresh = cred.refresh_extract_refresh_token.as_deref();
        let extract_expires = cred.refresh_extract_expires_in.as_deref();
        let refresh_method = cred.refresh_method.as_deref().unwrap_or("POST").to_string();
        let refresh_headers = cred.refresh_headers.clone();

        let rt = refresh_token.clone();
        let ru = refresh_url.clone();
        let rm = refresh_method.clone();
        let rh = refresh_headers.clone();
        let (status, body) = blocking_http_request(move || -> anyhow::Result<(u16, String)> {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?;
            let mut req = client.request(reqwest::Method::from_bytes(rm.as_bytes())?, &ru);
            if let Some(headers) = &rh {
                for (k, v) in headers {
                    req = req.header(k, v);
                }
            }
            req = req.json(&serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": rt,
            }));
            let resp = req.send()?;
            let status = resp.status().as_u16();
            let body = resp.text()?;
            Ok((status, body))
        })?;

        if status >= 400 {
            return Ok(autonoetic_types::tool_error::ToolError::execution(
                format!("Refresh endpoint returned HTTP {}", status),
                Some("The refresh endpoint rejected the request. Check if the refresh token is still valid.".to_string()),
            ).to_error_response());
        }

        let body_value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();

        let new_access_token = match extract_json_path(&body_value, extract_access) {
            Some(t) => t,
            None => {
                return Ok(autonoetic_types::tool_error::ToolError::execution(
                    format!(
                        "Could not extract access token from refresh response at path '{}'",
                        extract_access
                    ),
                    Some(
                        "Check the credential's refresh_extract_access_token configuration."
                            .to_string(),
                    ),
                )
                .to_error_response());
            }
        };

        vault.load_secret(&cred.secret_name, new_access_token);

        if let (Some(rp), Some(new_rt)) = (
            extract_refresh,
            extract_refresh.and_then(|p| extract_json_path(&body_value, p)),
        ) {
            if !rp.is_empty() {
                vault.load_secret(&refresh_token_secret, new_rt);
            }
        }

        let mut updated_cred = cred.clone();
        if let Some(expires_path) = extract_expires {
            if let Some(expires_in_str) = extract_json_path(&body_value, expires_path) {
                if let Ok(secs) = expires_in_str.parse::<i64>() {
                    let new_expiry = chrono::Utc::now() + chrono::Duration::seconds(secs);
                    updated_cred.expires_at = Some(new_expiry.to_rfc3339());
                }
            }
        }

        vault.persist_to_file(&vault_path)?;
        store.upsert_credential(&updated_cred)?;

        Ok(json!({
            "ok": true,
            "credential_id": credential_id,
            "refreshed": true,
            "new_expires_at": updated_cred.expires_at,
        })
        .to_string())
    }
}

/// Attempt a token refresh for a credential with refresh metadata.
/// Called internally when `credential.request` gets a 401.
/// Returns Ok(updated_credential) on success, Err on failure.
fn try_auto_refresh(
    cred: &CredentialRecord,
    store: &crate::scheduler::gateway_store::GatewayStore,
    vault_path: &Path,
) -> anyhow::Result<CredentialRecord> {
    let refresh_url = cred
        .refresh_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no refresh_url"))?;
    let rt_secret_name = cred
        .refresh_token_secret_name
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no refresh_token_secret_name"))?;

    let mut vault = crate::Vault::load_from_file(vault_path)?;
    let refresh_token = vault
        .get_secret(rt_secret_name)
        .ok_or_else(|| anyhow::anyhow!("refresh token not found in vault"))?
        .expose_secret()
        .to_string();

    let extract_access = cred
        .refresh_extract_access_token
        .as_deref()
        .unwrap_or("access_token");
    let extract_refresh = cred.refresh_extract_refresh_token.as_deref();
    let extract_expires = cred.refresh_extract_expires_in.as_deref();
    let refresh_method = cred.refresh_method.as_deref().unwrap_or("POST").to_string();
    let refresh_headers = cred.refresh_headers.clone();

    let rt = refresh_token.clone();
    let ru = refresh_url.clone();
    let rm = refresh_method.clone();
    let rh = refresh_headers.clone();
    let (status, body) = blocking_http_request(move || -> anyhow::Result<(u16, String)> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let mut req = client.request(reqwest::Method::from_bytes(rm.as_bytes())?, &ru);
        if let Some(headers) = &rh {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }
        req = req.json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": rt,
        }));
        let resp = req.send()?;
        Ok((resp.status().as_u16(), resp.text()?))
    })?;

    if status >= 400 {
        return Err(autonoetic_types::tool_error::tagged::Tagged::execution(
            anyhow::anyhow!("Refresh endpoint returned HTTP {}. The refresh endpoint rejected the request. Check if the refresh token is still valid.", status),
        ).into());
    }

    let body_value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();

    let new_access_token = extract_json_path(&body_value, extract_access).ok_or_else(|| {
        anyhow::anyhow!(
            "Could not extract access token at path '{}'",
            extract_access
        )
    })?;

    vault.load_secret(&cred.secret_name, new_access_token);

    if let (Some(rp), Some(new_rt)) = (
        extract_refresh,
        extract_refresh.and_then(|p| extract_json_path(&body_value, p)),
    ) {
        if !rp.is_empty() {
            vault.load_secret(rt_secret_name, new_rt);
        }
    }

    let mut updated_cred = cred.clone();
    if let Some(expires_path) = extract_expires {
        if let Some(expires_in_str) = extract_json_path(&body_value, expires_path) {
            if let Ok(secs) = expires_in_str.parse::<i64>() {
                let new_expiry = chrono::Utc::now() + chrono::Duration::seconds(secs);
                updated_cred.expires_at = Some(new_expiry.to_rfc3339());
            }
        }
    }

    vault.persist_to_file(vault_path)?;
    store.upsert_credential(&updated_cred)?;
    Ok(updated_cred)
}

// ---------------------------------------------------------------------------
// credential.setup
// ---------------------------------------------------------------------------

/// Multi-step server-side credential registration flow.
///
/// Supports two modes:
/// 1. **Direct**: caller supplies `service` + `steps` explicitly.
/// 2. **Skill URL**: caller supplies `skill_url` pointing to a remote skill.md whose
///    `autonoetic.onboarding` section is parsed and executed by the gateway.
///
/// When a `user_input` step is reached the tool returns early with
/// `suspended_for_user_input: true` and the question.  The agent should call
/// `user.ask` with that question, collect the answer, and call `credential.setup`
/// again with `credential_id` + `resume_vars: { var_name: answer }`.
struct CredentialSetupTool;

#[derive(Debug, Serialize, Deserialize)]
struct CredentialSetupArgs {
    /// Service name — may be omitted when `skill_url` is provided (extracted from skill.md).
    service: Option<String>,
    /// Explicit setup steps — may be omitted when `skill_url` is provided.
    #[serde(default)]
    steps: Option<Vec<CredentialSetupStep>>,
    /// Optional: credential ID to use. Generated if not provided.
    credential_id: Option<String>,
    /// Optional: expiry timestamp for the credential.
    expires_at: Option<String>,
    /// Optional: how to inject the secret (bearer, header:X-Custom, env var name).
    inject_as: Option<String>,
    /// Optional: hosts this credential is bound to.
    allowed_hosts: Option<Vec<String>>,
    /// Optional: approval reference after operator provides secrets via approval channel.
    approval_ref: Option<String>,
    /// URL to a skill.md whose `autonoetic.onboarding` section drives registration.
    skill_url: Option<String>,
    /// Answers from a prior `user_input` suspension, keyed by `var_name`.
    /// Required when resuming with `credential_id`.
    resume_vars: Option<HashMap<String, String>>,
    /// Optional label distinguishing multiple credentials for the same service.
    /// When provided, dedup is scoped to (service, label) instead of just service.
    #[serde(default)]
    label: Option<String>,
}

/// Persisted state between `credential.setup` calls for multi-step onboarding.
#[derive(Debug, Serialize, Deserialize)]
struct CredentialSetupState {
    /// Full step list (serialized so we can resume without re-fetching skill.md).
    steps: Vec<CredentialSetupStep>,
    /// Index of the `UserInput` step we paused at (resume starts at `current_step + 1`).
    current_step: usize,
    /// Accumulated user-supplied variable values.
    vars: HashMap<String, String>,
    /// Accumulated public-facing data extracted from API responses.
    public_data: serde_json::Map<String, serde_json::Value>,
    /// Resolved service name.
    service: String,
    /// How the secret is injected (e.g. "bearer").
    inject_as: Option<String>,
    /// Hosts bound to the credential.
    allowed_hosts: Vec<String>,
    /// Optional base URL for relative step URLs.
    base_url: Option<String>,
    /// Credential expiry.
    expires_at: Option<String>,
}

// ---------------------------------------------------------------------------
// skill.md frontmatter types (local only — not exported)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SkillMdFrontmatter {
    #[serde(default)]
    autonoetic: Option<SkillMdAutonoetic>,
}

#[derive(Debug, Deserialize)]
struct SkillMdAutonoetic {
    base_url: Option<String>,
    credential: Option<SkillCredentialSpec>,
    onboarding: Option<SkillOnboarding>,
}

#[derive(Debug, Deserialize)]
struct SkillCredentialSpec {
    service: Option<String>,
    inject_as: Option<String>,
    allowed_hosts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SkillOnboarding {
    steps: Vec<SkillStep>,
}

/// A step as declared in the skill.md YAML frontmatter.
/// Uses `type` instead of `step_type` to match the spec format.
#[derive(Debug, Deserialize)]
struct SkillStep {
    #[serde(rename = "type")]
    step_type: String,
    url: Option<String>,
    method: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<serde_json::Value>,
    #[serde(default)]
    extract_secrets: HashMap<String, String>,
    #[serde(default)]
    extract_public: HashMap<String, String>,
    question: Option<String>,
    var: Option<String>,
    message: Option<String>,
    secret_fields: Option<Vec<autonoetic_types::agent::SecretFieldSpec>>,
    instruction: Option<String>,
    #[serde(default)]
    data_refs: Vec<String>,
}

impl SkillStep {
    fn into_credential_step(self, base_url: &str) -> anyhow::Result<CredentialSetupStep> {
        match self.step_type.as_str() {
            "api_call" => {
                let url = self
                    .url
                    .ok_or_else(|| anyhow::anyhow!("api_call step missing 'url'"))?;
                let full_url = if url.starts_with("http://") || url.starts_with("https://") {
                    url
                } else {
                    format!("{}{}", base_url.trim_end_matches('/'), url)
                };
                Ok(CredentialSetupStep::ApiCall {
                    method: self.method,
                    url: full_url,
                    headers: self.headers,
                    body: self.body,
                    extract_secrets: self.extract_secrets,
                    extract_public: self.extract_public,
                })
            }
            "user_input" => {
                let question = self
                    .question
                    .ok_or_else(|| anyhow::anyhow!("user_input step missing 'question'"))?;
                let var_name = self
                    .var
                    .ok_or_else(|| anyhow::anyhow!("user_input step missing 'var'"))?;
                Ok(CredentialSetupStep::UserInput { question, var_name })
            }
            "user_prompt" => Ok(CredentialSetupStep::UserPrompt {
                message: self.message.unwrap_or_default(),
                secret_fields: self.secret_fields.unwrap_or_default(),
            }),
            "user_action" => Ok(CredentialSetupStep::UserAction {
                instruction: self.instruction.unwrap_or_default(),
                data_refs: self.data_refs,
            }),
            t => {
                return Err(autonoetic_types::tool_error::tagged::Tagged::validation(
                    anyhow::anyhow!("Unknown step type in skill.md onboarding: '{}'", t),
                )
                .into());
            }
        }
    }
}

impl NativeTool for CredentialSetupTool {
    fn name(&self) -> &'static str {
        "credential_setup"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "credential_setup".to_string(),
            description: "Register a new credential through a multi-step setup flow. \
                Provide `skill_url` to ingest a skill.md spec and let the gateway \
                execute all onboarding steps server-side — secrets are never returned to the LLM. \
                skill_url accepts an HTTPS URL (fetched remotely, subject to approval) or a \
                gateway-local .md path such as 'github.md' / 'skills/github.md'; \
                file:// URLs are accepted only when they resolve under the gateway skills/ directory. \
                When a user_input step is reached the tool returns with `suspended_for_user_input: true` \
                and a `question`. Call `user.ask` with that question, then call `credential.setup` \
                again with `credential_id` + `resume_vars: { var_name: answer }` to continue. \
                Alternatively supply `service` + `steps` directly for programmatic setup.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill_url": {
                        "type": "string",
                        "description": "HTTPS URL to a skill.md (fetched remotely) or a gateway-local .md path such as 'github.md' / 'skills/github.md'. file:// URLs are accepted only when they resolve under the gateway skills/ directory. When set, service/steps are extracted from the spec."
                    },
                    "service": {
                        "type": "string",
                        "description": "Service name (e.g. 'github', 'stripe'). Required when not using skill_url."
                    },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_type": {
                                    "type": "string",
                                    "enum": ["api_call", "user_prompt", "user_action", "user_input"]
                                },
                                "method": {"type": "string"},
                                "url": {"type": "string"},
                                "headers": {"type": "object"},
                                "body": {"type": "object"},
                                "extract_secrets": {"type": "object"},
                                "extract_public": {"type": "object"},
                                "question": {"type": "string"},
                                "var_name": {"type": "string"},
                                "message": {"type": "string"},
                                "secret_fields": {"type": "array"},
                                "instruction": {"type": "string"},
                                "data_refs": {"type": "array"}
                            }
                        },
                        "description": "Ordered setup steps. Required when not using skill_url."
                    },
                    "credential_id": {
                        "type": "string",
                        "description": "Credential ID — required when resuming after user_input suspension."
                    },
                    "resume_vars": {
                        "type": "object",
                        "description": "User-collected variable values. Required when resuming: { var_name: answer }."
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
                    },
                    "approval_ref": {
                        "type": "string",
                        "description": "Approval request ID from a completed credential.setup approval (UserPrompt or remote-access gate)."
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional label distinguishing multiple credentials for the same service (e.g. 'agent-a', 'agent-b'). Dedup is scoped to (service, label)."
                    }
                }
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
        let setup_label = args.label.clone();

        let Some(store) = gateway_store else {
            return Ok(autonoetic_types::tool_error::ToolError::resource(
                "Gateway store not available",
                None::<String>,
            )
            .to_error_response());
        };

        let mut approved_setup_remote_url: Option<String> = None;

        // ------------------------------------------------------------------
        // Path 1: resume after approval
        // ------------------------------------------------------------------
        if let Some(ref approval_ref) = args.approval_ref {
            if let Some(approval) = store.get_approval(approval_ref)? {
                if approval.status == Some(autonoetic_types::background::ApprovalStatus::Approved) {
                    match &approval.action {
                        autonoetic_types::background::ScheduledAction::CredentialPrompt {
                            credential_id,
                            ..
                        } => {
                            if let Some(cred) = store.get_credential(credential_id)? {
                                return Ok(json!({
                                    "ok": true,
                                    "credential_id": cred.credential_id,
                                    "service": cred.service,
                                    "secrets_stored": 1,
                                    "resumed_from_approval": true,
                                })
                                .to_string());
                            }
                        }
                        autonoetic_types::background::ScheduledAction::CredentialRequest {
                            url,
                            payload,
                            ..
                        } => {
                            let is_setup_remote_gate = payload
                                .as_ref()
                                .and_then(|p| p.get("source_tool"))
                                .and_then(|v| v.as_str())
                                == Some("credential_setup");
                            if is_setup_remote_gate {
                                approved_setup_remote_url = Some(url.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
            if approved_setup_remote_url.is_none() {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("approval '{}' for credential setup", approval_ref),
                    Some("The approval reference may be invalid, not yet approved, or not for a credential setup flow.".to_string()),
                ).to_error_response());
            }
        }

        // ------------------------------------------------------------------
        // Path 2: resume after user_input suspension
        // ------------------------------------------------------------------
        if let (Some(ref cred_id), Some(ref resume_vars)) = (&args.credential_id, &args.resume_vars)
        {
            let Some(state_json) = store.load_credential_setup_state(cred_id)? else {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("suspended setup state for credential '{}'", cred_id),
                    Some(
                        "No in-progress setup was found. Start a new credential.setup call."
                            .to_string(),
                    ),
                )
                .to_error_response());
            };
            let mut state: CredentialSetupState = serde_json::from_str(&state_json)?;

            // Authorization check using the service from saved state.
            let service_allowed = manifest.capabilities.iter().any(|c| {
                matches!(c, Capability::CredentialAccess { services }
                    if services.iter().any(|s| s == "*" || s == &state.service))
            });
            if !service_allowed {
                return Ok(autonoetic_types::tool_error::ToolError::permission(format!(
                    "Credential setup denied for service: {}",
                    state.service
                ))
                .to_error_response());
            }

            // Merge the user's answers and advance past the UserInput step.
            for (k, v) in resume_vars {
                state.vars.insert(k.clone(), v.clone());
            }
            let resume_from = state.current_step + 1;

            // Vault auto-init + load.
            let vdir = vault_dir(_gateway_dir, _agent_dir);
            crate::vault::ensure_default_key(&vdir)?;
            let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
                .ok()
                .map(PathBuf::from)
                .unwrap_or_else(|| crate::vault::default_vault_path(&vdir));
            let mut vault = crate::vault::Vault::load_from_file(&vault_path)?;

            return execute_steps(
                &state.steps,
                resume_from,
                state.vars,
                state.public_data,
                &state.service,
                state.inject_as.as_deref(),
                &state.allowed_hosts,
                state.base_url.as_deref(),
                state.expires_at.as_deref(),
                cred_id,
                manifest,
                policy,
                store.clone(),
                &mut vault,
                &vault_path,
                _session_id,
                _turn_id,
                _config,
                _run_context,
                None,
            );
        }

        // ------------------------------------------------------------------
        // Path 3: fresh start — may use skill_url or explicit service/steps
        // ------------------------------------------------------------------
        let (service, steps, inject_as, allowed_hosts, base_url) = if let Some(ref raw_url) =
            args.skill_url
        {
            let (content, url_host, base_url) = match classify_skill_url(raw_url) {
                SkillUrlKind::Remote { url, host } => {
                    let url_host = host;
                    let url = url;

                    let skill_url_is_approved = approved_setup_remote_url
                        .as_deref()
                        .map(|u| extract_host(u).unwrap_or_default() == url_host)
                        .unwrap_or(false);
                    if !skill_url_is_approved {
                        let policy_violation =
                            crate::runtime::network_policy::enforce_remote_target_policy(
                                manifest,
                                _agent_dir,
                                &url_host,
                                Some(&url),
                                DeclarationRequirement::Required,
                            )
                            .err()
                            .map(|v| v.error_type.to_string())
                            .or_else(|| {
                                if url_host.is_empty()
                                    || !policy.can_connect_net(&url_host).is_allowed()
                                {
                                    Some("network_access_denied".to_string())
                                } else {
                                    None
                                }
                            });

                        if let Some(violation_type) = policy_violation {
                            let action = ScheduledAction::CredentialRequest {
                                credential_id: args.credential_id.clone().unwrap_or_default(),
                                url: url.clone(),
                                method: Some("GET".to_string()),
                                headers: None,
                                body: None,
                                inject_secret_as: None,
                                payload: Some(json!({
                                    "host": url_host.clone(),
                                    "retry_field": "approval_ref",
                                    "source_tool": "credential_setup",
                                    "setup_phase": "skill_url",
                                    "policy_violation": violation_type,
                                })),
                            };
                            let reason = format!(
                                "Fetching skill.md from {} requires approval (policy: {})",
                                url_host, violation_type
                            );
                            let gate = crate::runtime::human_gate::GateService::new(store.clone());
                            let gate_result = gate.check(
                                crate::runtime::human_gate::GateRequest {
                                    kind: crate::runtime::human_gate::GateKind::Approval {
                                        action: action.clone(),
                                        targets: vec![url_host.clone()],
                                        match_strategy: crate::runtime::human_gate::MatchStrategy::HostLevel,
                                    },
                                    manifest,
                                    session_id: _session_id,
                                    run_context: _run_context,
                                    config: _config,
                                    context: crate::runtime::human_gate::DecisionContext::tier2(
                                        format!("fetch skill.md from {}", url_host),
                                        format!(
                                            "remote target policy not satisfied for {} (policy: {})",
                                            url_host, violation_type
                                        ),
                                        format!("downloads agent skill manifest from {}", url_host),
                                        "Approve if this host is the expected source of the agent's skill manifest; reject if the host is unexpected",
                                    ),
                                    summary: format!("Fetch skill.md from {}", url_host),
                                    approval_ref: None,
                                    pre_validated: false,
                                    cache_backfill: None,
                        request_id: None,
                        turn_id: None,
                                },
                            )?;
                            match gate_result {
                                crate::runtime::human_gate::GateResult::Cleared { .. } => {}
                                crate::runtime::human_gate::GateResult::AlreadyPending {
                                    gate_id,
                                    ..
                                } => {
                                    return Ok(json!({
                                        "ok": false,
                                        "approval_required": true,
                                        "approval_already_pending": true,
                                        "request_id": gate_id,
                                        "suspended": true,
                                        "reason": reason,
                                        "repair_hint": "Wait for the existing approval to be resolved.",
                                        "approval": {
                                            "kind": "credential_setup_remote_access",
                                            "summary": format!("Fetch skill.md from {}", url_host),
                                            "retry_field": "approval_ref"
                                        }
                                    }).to_string());
                                }
                                crate::runtime::human_gate::GateResult::Suspended {
                                    gate_id,
                                    ..
                                } => {
                                    return Ok(json!({
                                        "ok": false,
                                        "error_type": violation_type,
                                        "message": format!(
                                            "Execution suspended pending operator approval ({}). Retry credential.setup with approval_ref after approval.",
                                            gate_id
                                        ),
                                        "repair_hint": "Wait for approval and retry credential.setup with the same skill_url plus approval_ref.",
                                        "approval_required": true,
                                        "request_id": gate_id,
                                        "suspended": true,
                                        "reason": reason,
                                        "approval": {
                                            "kind": "credential_setup_remote_access",
                                            "summary": format!("Fetch skill.md from {}", url_host),
                                            "reason": format!("Remote target policy: {} for {}", violation_type, url_host),
                                            "retry_field": "approval_ref"
                                        }
                                    }).to_string());
                                }
                                other => {
                                    tracing::warn!(
                                        target: "credential",
                                        gate_result = ?other,
                                        "Unexpected gate result for credential_setup skill_url gate"
                                    );
                                }
                            }
                        }
                    }

                    let url_clone = url.clone();
                    let (http_status, content) =
                        blocking_http_request(move || -> anyhow::Result<(u16, String)> {
                            let client = reqwest::blocking::Client::builder()
                                .timeout(std::time::Duration::from_secs(15))
                                .build()?;
                            let resp = client.get(url_clone.as_str()).send()?;
                            let status = resp.status().as_u16();
                            if !resp.status().is_success() {
                                let _ = resp.text()?;
                                return Ok((status, String::new()));
                            }
                            let content = resp.text()?;
                            Ok((status, content))
                        })?;

                    if !(200..300).contains(&(http_status as i32)) {
                        return Ok(autonoetic_types::tool_error::ToolError::execution(
                            format!(
                                "Failed to fetch skill.md from {}: HTTP {}",
                                url, http_status
                            ),
                            Some("Verify the skill_url is correct and accessible.".to_string()),
                        )
                        .to_error_response());
                    }

                    let base_url = extract_base_url(&url);
                    (content, url_host, Some(base_url))
                }
                SkillUrlKind::Local { path_hint } => {
                    let gateway_dir = _gateway_dir.ok_or_else(|| {
                        anyhow::anyhow!(
                            "local skill_url requires a gateway directory; \
                             place the .md file in {{agents_dir}}/.gateway/skills/"
                        )
                    })?;
                    let path = validate_local_skill_path(gateway_dir, &path_hint)?;
                    let content = std::fs::read_to_string(&path).map_err(|e| {
                        anyhow::anyhow!(
                            "failed to read skill file from gateway skills/ directory: {}",
                            e
                        )
                    })?;
                    let base_url = None;
                    (content, String::new(), base_url)
                }
            };

            let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
            let parsed = match matter.parse::<SkillMdFrontmatter>(&content) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        format!("Failed to parse skill.md content: {}", e),
                        Some("Ensure the skill.md has valid YAML frontmatter.".to_string()),
                    )
                    .to_error_response());
                }
            };
            let fm = match parsed.data {
                Some(d) => d,
                None => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                            "No YAML frontmatter found in skill.md",
                            Some("The skill.md must start with a YAML frontmatter block (--- delimited).".to_string()),
                        ).to_error_response());
                }
            };

            let autonoetic = fm.autonoetic.unwrap_or(SkillMdAutonoetic {
                base_url: None,
                credential: None,
                onboarding: None,
            });
            let resolved_base_url = base_url.or_else(|| autonoetic.base_url.clone());
            let cred_spec = autonoetic.credential.unwrap_or(SkillCredentialSpec {
                service: None,
                inject_as: None,
                allowed_hosts: None,
            });
            let service = cred_spec
                .service
                .or_else(|| args.service.clone())
                .unwrap_or_else(|| url_host.clone());
            let inject_as = cred_spec.inject_as.or_else(|| args.inject_as.clone());
            let allowed_hosts = cred_spec
                .allowed_hosts
                .or_else(|| args.allowed_hosts.clone())
                .unwrap_or_else(|| {
                    if url_host.is_empty() {
                        vec![]
                    } else {
                        vec![url_host.clone()]
                    }
                });

            let raw_steps = autonoetic.onboarding.map(|o| o.steps).unwrap_or_default();
            let mut steps: Vec<CredentialSetupStep> = Vec::with_capacity(raw_steps.len());
            for raw in raw_steps {
                match raw.into_credential_step(resolved_base_url.as_deref().unwrap_or("")) {
                    Ok(s) => steps.push(s),
                    Err(e) => {
                        return Ok(autonoetic_types::tool_error::ToolError::validation(
                            format!("Invalid onboarding step in skill.md: {}", e),
                            Some(
                                "Fix the step definition in the skill.md onboarding section."
                                    .to_string(),
                            ),
                        )
                        .to_error_response());
                    }
                }
            }

            (service, steps, inject_as, allowed_hosts, resolved_base_url)
        } else {
            // Explicit service + steps.
            let service = match args.service {
                Some(s) => s,
                None => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        "Either 'skill_url' or 'service' + 'steps' must be provided",
                        Some(
                            "Provide a skill_url or specify service and steps directly."
                                .to_string(),
                        ),
                    )
                    .to_error_response());
                }
            };
            let steps = args.steps.unwrap_or_default();
            let inject_as = args.inject_as.clone();
            let allowed_hosts = args.allowed_hosts.clone().unwrap_or_default();
            (service, steps, inject_as, allowed_hosts, None)
        };

        // Service-scoped authorization check.
        let service_allowed = manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::CredentialAccess { services }
                if services.iter().any(|s| s == "*" || s == &service))
        });
        if !service_allowed {
            return Ok(autonoetic_types::tool_error::ToolError::permission(format!(
                "Credential setup denied for service: {}",
                service
            ))
            .to_error_response());
        }

        // Dedup: if a credential already exists for this (service, label),
        // return it instead of creating a duplicate.
        {
            let existing = store.list_credentials_by_service(&service)?;
            let matched = existing.iter().find(|c| c.label == args.label);
            if let Some(cred) = matched {
                let mut response = json!({
                    "ok": true,
                    "credential_id": cred.credential_id,
                    "service": cred.service,
                    "existing": true,
                    "note": "Credential already exists for this service/label — reusing existing credential.",
                });
                if let Some(inject_as) = &cred.inject_as {
                    response["inject_as"] = json!(inject_as);
                }
                if let Some(label) = &cred.label {
                    response["label"] = json!(label);
                }
                return Ok(response.to_string());
            }
        }

        // Network policy pre-check for all ApiCall step URLs.
        for step in &steps {
            if let CredentialSetupStep::ApiCall {
                method,
                url,
                headers,
                body,
                ..
            } = step
            {
                let host = extract_host(url)?;
                let step_url_is_approved = approved_setup_remote_url
                    .as_deref()
                    .map(|u| extract_host(u).unwrap_or_default() == host)
                    .unwrap_or(false);
                if !step_url_is_approved {
                    let policy_violation =
                        crate::runtime::network_policy::enforce_remote_target_policy(
                            manifest,
                            _agent_dir,
                            &host,
                            Some(url),
                            DeclarationRequirement::Required,
                        )
                        .err()
                        .map(|v| v.error_type.to_string())
                        .or_else(|| {
                            if host.is_empty() || !policy.can_connect_net(&host).is_allowed() {
                                Some("network_access_denied".to_string())
                            } else {
                                None
                            }
                        });

                    if let Some(violation_type) = policy_violation {
                        let display_host = if host.is_empty() {
                            "<empty host>"
                        } else {
                            &host
                        };
                        let action = ScheduledAction::CredentialRequest {
                            credential_id: args.credential_id.clone().unwrap_or_default(),
                            url: url.clone(),
                            method: method.clone(),
                            headers: Some(headers.clone()),
                            body: body.clone(),
                            inject_secret_as: None,
                            payload: Some(json!({
                                "host": host.clone(),
                                "retry_field": "approval_ref",
                                "source_tool": "credential_setup",
                                "setup_phase": "api_call_precheck",
                                "policy_violation": violation_type,
                            })),
                        };
                        let reason = format!(
                            "API call to {} requires approval (policy: {})",
                            display_host, violation_type
                        );
                        let gate = crate::runtime::human_gate::GateService::new(store.clone());
                        let gate_result = gate.check(
                            crate::runtime::human_gate::GateRequest {
                                kind: crate::runtime::human_gate::GateKind::Approval {
                                    action: action.clone(),
                                    targets: vec![host.clone()],
                                    match_strategy: crate::runtime::human_gate::MatchStrategy::HostLevel,
                                },
                                manifest,
                                session_id: _session_id,
                                run_context: _run_context,
                                config: _config,
                                context: crate::runtime::human_gate::DecisionContext::tier2(
                                    format!("credential setup API call to {}", display_host),
                                    format!(
                                        "remote target policy not satisfied for {} (policy: {})",
                                        display_host, violation_type
                                    ),
                                    format!("registers/sets a credential against {}", display_host),
                                    "Approve if this host is the expected credential-setup endpoint for the agent's task; reject if the host is unexpected",
                                ),
                                summary: format!("Credential setup API call to {}", display_host),
                                approval_ref: None,
                                pre_validated: false,
                                cache_backfill: None,
                        request_id: None,
                        turn_id: None,
                            },
                        )?;
                        match gate_result {
                            crate::runtime::human_gate::GateResult::Cleared { .. } => {}
                            crate::runtime::human_gate::GateResult::AlreadyPending {
                                gate_id,
                                ..
                            } => {
                                return Ok(json!({
                                    "ok": false,
                                    "approval_required": true,
                                    "approval_already_pending": true,
                                    "request_id": gate_id,
                                    "suspended": true,
                                    "reason": reason,
                                    "repair_hint": "Wait for the existing approval to be resolved.",
                                    "approval": {
                                        "kind": "credential_setup_remote_access",
                                        "summary": format!("Credential setup API call to {}", display_host),
                                        "retry_field": "approval_ref"
                                    }
                                }).to_string());
                            }
                            crate::runtime::human_gate::GateResult::Suspended {
                                gate_id, ..
                            } => {
                                return Ok(json!({
                                    "ok": false,
                                    "error_type": violation_type,
                                    "message": format!(
                                        "Execution suspended pending operator approval ({}). Retry credential.setup with approval_ref after approval.",
                                        gate_id
                                    ),
                                    "repair_hint": "Wait for approval and retry credential.setup with approval_ref.",
                                    "approval_required": true,
                                    "request_id": gate_id,
                                    "suspended": true,
                                    "reason": reason,
                                    "approval": {
                                        "kind": "credential_setup_remote_access",
                                        "summary": format!("Credential setup API call to {}", display_host),
                                        "reason": format!("Remote target policy: {} for {}", violation_type, display_host),
                                        "retry_field": "approval_ref"
                                    }
                                }).to_string());
                            }
                            other => {
                                tracing::warn!(
                                    target: "credential",
                                    gate_result = ?other,
                                    "Unexpected gate result for credential_setup api_call gate"
                                );
                            }
                        }
                    }
                }
            }
        }

        // Vault auto-init + load.
        let vdir = vault_dir(_gateway_dir, _agent_dir);
        crate::vault::ensure_default_key(&vdir)?;
        let vault_path = std::env::var("AUTONOETIC_VAULT_PATH")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::vault::default_vault_path(&vdir));
        let mut vault = crate::vault::Vault::load_from_file(&vault_path)?;

        let credential_id = args.credential_id.clone().unwrap_or_else(|| {
            format!(
                "cred_{}_{}",
                service,
                uuid::Uuid::new_v4().to_string().replace('-', "")
            )
        });
        crate::runtime::tools::ensure_safe_credential_id_reference(&credential_id)?;

        execute_steps(
            &steps,
            0,
            HashMap::new(),
            serde_json::Map::new(),
            &service,
            inject_as.as_deref(),
            &allowed_hosts,
            base_url.as_deref(),
            args.expires_at.as_deref(),
            &credential_id,
            manifest,
            policy,
            store.clone(),
            &mut vault,
            &vault_path,
            _session_id,
            _turn_id,
            _config,
            _run_context,
            setup_label.as_deref(),
        )
    }
}

// ---------------------------------------------------------------------------
// Core execution loop (shared between fresh start and resume)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_steps(
    steps: &[CredentialSetupStep],
    start_from: usize,
    vars: HashMap<String, String>,
    mut public_data: serde_json::Map<String, serde_json::Value>,
    service: &str,
    inject_as: Option<&str>,
    allowed_hosts: &[String],
    base_url: Option<&str>,
    expires_at: Option<&str>,
    credential_id: &str,
    manifest: &AgentManifest,
    _policy: &PolicyEngine,
    store: std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>,
    vault: &mut crate::vault::Vault,
    vault_path: &Path,
    session_id: Option<&str>,
    turn_id: Option<&str>,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    run_context: Option<&crate::runtime::active_execution_registry::NativeToolRunContext>,
    label: Option<&str>,
) -> anyhow::Result<String> {
    let mut secret_names: Vec<String> = Vec::new();
    let mut step_results: Vec<serde_json::Value> = Vec::new();

    for (i, step) in steps.iter().enumerate() {
        if i < start_from {
            // Re-scan already-completed steps to collect previously extracted secrets
            // into `secret_names` so the credential record is created correctly.
            if let CredentialSetupStep::ApiCall {
                extract_secrets, ..
            } = step
            {
                for name in extract_secrets.keys() {
                    if vault.get_secret(name).is_some() {
                        if !secret_names.contains(name) {
                            secret_names.push(name.clone());
                        }
                    }
                }
            }
            continue;
        }

        match step {
            CredentialSetupStep::ApiCall {
                method,
                url,
                headers,
                body,
                extract_secrets,
                extract_public,
            } => {
                // Template substitution — secrets resolved server-side, never to LLM.
                let resolved_url = resolve_template_str(url, &vars, &public_data, manifest, vault);
                let resolved_headers: HashMap<String, String> = headers
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            resolve_template_str(v, &vars, &public_data, manifest, vault),
                        )
                    })
                    .collect();
                let resolved_body: Option<serde_json::Value> = body
                    .as_ref()
                    .map(|b| resolve_template_value(b, &vars, &public_data, manifest, vault));

                let http_method = method.as_deref().unwrap_or("POST").to_string();
                let resolved_url_clone = resolved_url.clone();
                let resolved_headers_clone = resolved_headers.clone();
                let resolved_body_clone = resolved_body.clone();

                let (status, resp_body) =
                    blocking_http_request(move || -> anyhow::Result<(u16, String)> {
                        let client = reqwest::blocking::Client::builder()
                            .timeout(std::time::Duration::from_secs(30))
                            .build()?;

                        let mut req = client.request(
                            reqwest::Method::from_bytes(http_method.as_bytes())?,
                            &resolved_url_clone,
                        );
                        for (k, v) in &resolved_headers_clone {
                            req = req.header(k.as_str(), v.as_str());
                        }
                        if let Some(ref b) = resolved_body_clone {
                            req = req.json(b);
                        }

                        let resp = req.send()?;
                        let status = resp.status().as_u16();
                        let resp_body = resp.text()?;
                        Ok((status, resp_body))
                    })?;
                let resp_value: serde_json::Value =
                    serde_json::from_str(&resp_body).unwrap_or(json!(resp_body));

                // Extract secrets into vault (server-side only).
                for (name, path) in extract_secrets {
                    if let Some(val) = extract_json_path(&resp_value, path) {
                        vault.set_secret(name, val.clone());
                        if !secret_names.contains(name) {
                            secret_names.push(name.clone());
                        }
                    }
                }

                // Extract public data — block paths that overlap with secrets.
                let secret_paths: std::collections::HashSet<String> = extract_secrets
                    .values()
                    .map(|s| {
                        s.trim_start_matches('$')
                            .trim_start_matches('.')
                            .to_string()
                    })
                    .collect();
                for (field_name, path) in extract_public {
                    let normalized = path
                        .trim_start_matches('$')
                        .trim_start_matches('.')
                        .to_string();
                    if secret_paths.contains(&normalized) {
                        continue;
                    }
                    if let Some(val) = extract_json_path(&resp_value, path) {
                        public_data.insert(field_name.clone(), json!(val));
                    }
                }

                step_results.push(json!({
                    "step": i,
                    "step_type": "api_call",
                    "status": status,
                    "url": resolved_url,
                }));
            }

            CredentialSetupStep::UserInput { question, var_name } => {
                // Resolve templates in the question (e.g. {{public.tweet_text}}).
                let resolved_question =
                    resolve_template_str(question, &vars, &public_data, manifest, vault);

                // Persist execution state so the agent can resume.
                let state = CredentialSetupState {
                    steps: steps.to_vec(),
                    current_step: i,
                    vars: vars.clone(),
                    public_data: public_data.clone(),
                    service: service.to_string(),
                    inject_as: inject_as.map(str::to_string),
                    allowed_hosts: allowed_hosts.to_vec(),
                    base_url: base_url.map(str::to_string),
                    expires_at: expires_at.map(str::to_string),
                };
                let state_json = serde_json::to_string(&state)?;
                store.save_credential_setup_state(credential_id, &state_json)?;

                // Persist any secrets extracted so far.
                vault.persist_to_file(vault_path)?;

                let message = "credential.setup suspended for user input; call user.ask with the provided question, then resume with credential_id and resume_vars";
                return Ok(json!({
                    "ok": false,
                    "error_type": "conflict",
                    "message": message,
                    "repair_hint": "Ask the user the question, collect values for var_name, then call credential.setup with credential_id + resume_vars.",
                    "suspended_for_user_input": true,
                    "credential_id": credential_id,
                    "question": resolved_question,
                    "var_name": var_name,
                    "public_data": public_data,
                    "reason": "user_input step — call user.ask with the question, then resume with credential_id + resume_vars"
                })
                .to_string());
            }

            CredentialSetupStep::UserPrompt {
                message,
                secret_fields,
            } => {
                // Route the credential prompt through GateService so it participates
                // in typed DecisionContext enforcement, dedup, and the root-scoped
                // identical-action join (#724).
                let approval_action = ScheduledAction::CredentialPrompt {
                    service: service.to_string(),
                    credential_id: credential_id.to_string(),
                    message: message.clone(),
                    secret_fields: secret_fields.clone(),
                    payload: Some(json!({
                        "inject_as": inject_as,
                        "allowed_hosts": allowed_hosts,
                        "expires_at": expires_at,
                    })),
                };

                let field_names: Vec<&str> = secret_fields
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect();

                let gate = crate::runtime::human_gate::GateService::new(store.clone());
                let gate_result = gate.check(crate::runtime::human_gate::GateRequest {
                    kind: crate::runtime::human_gate::GateKind::Approval {
                        action: approval_action.clone(),
                        targets: Vec::new(),
                        match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
                    },
                    manifest,
                    session_id,
                    run_context,
                    config,
                    context: crate::runtime::human_gate::DecisionContext::tier2(
                        format!("Credential setup for '{}'", service),
                        "Human input required for secret fields",
                        format!("Prompt asks for: {}", field_names.join(", ")),
                        "Approve to allow the credential setup prompt; the operator must still provide the requested secret fields",
                    ),
                    summary: format!("Credential setup prompt for '{}'", service),
                    approval_ref: None,
                    request_id: None,
                    pre_validated: false,
                    cache_backfill: None,
                    turn_id,
                })?;

                let request_id = match &gate_result {
                    crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. }
                    | crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
                        gate_id.clone()
                    }
                    crate::runtime::human_gate::GateResult::Cleared { .. }
                    | crate::runtime::human_gate::GateResult::PolicyAllowed => {
                        // An identical prompt was already approved. If the credential
                        // now exists we can resume immediately; otherwise the
                        // operator must provide the secrets directly — there is no
                        // pending approval row to attach them to, so synthesizing a
                        // fake request_id and reporting `approval_required: true`
                        // would dead-end the workflow (#724 Part B review).
                        if let Some(cred) = store.get_credential(credential_id)? {
                            return Ok(json!({
                                "ok": true,
                                "credential_id": cred.credential_id,
                                "service": cred.service,
                                "secrets_stored": 1,
                                "resumed_from_approval": true,
                            })
                            .to_string());
                        }
                        // No approval row exists and none will be minted. Suspend
                        // for direct secret input rather than approval — do NOT
                        // report a request_id or approval_required here.
                        step_results.push(json!({
                            "step": i,
                            "step_type": "user_prompt",
                            "message": message,
                            "secret_fields": secret_fields,
                            "status": "awaiting_secret_input",
                        }));
                        vault.persist_to_file(vault_path)?;
                        return Ok(json!({
                            "ok": false,
                            "error_type": "permission",
                            "message": "credential.setup suspended: equivalent prompt was already cleared but the secret is still missing",
                            "repair_hint": format!(
                                "Ask the operator for the requested secret field(s) ({}), then call \
                                 credential.setup with credential_id + resume_vars to provide them.",
                                field_names.join(", ")
                            ),
                            "suspended": true,
                            "approval_required": false,
                            "request_id": serde_json::Value::Null,
                            "credential_id": credential_id,
                            "service": service,
                            "steps": step_results,
                            "reason": "UserPrompt step cleared by GateService but secret fields are still empty",
                        })
                        .to_string());
                    }
                };

                step_results.push(json!({
                    "step": i,
                    "step_type": "user_prompt",
                    "message": message,
                    "secret_fields": secret_fields,
                    "status": "awaiting_human_input",
                    "approval_request_id": request_id.clone(),
                }));

                vault.persist_to_file(vault_path)?;

                let approval_request_id = step_results
                    .iter()
                    .filter_map(|s| {
                        s.get("approval_request_id")
                            .and_then(|v| v.as_str().map(String::from))
                    })
                    .next();
                let message = "credential.setup suspended pending human input for secret fields";
                return Ok(json!({
                    "ok": false,
                    "error_type": "permission",
                    "message": message,
                    "repair_hint": "Wait for approval or provide requested secret fields, then resume credential.setup.",
                    "suspended": true,
                    "approval_required": true,
                    "request_id": approval_request_id,
                    "credential_id": credential_id,
                    "service": service,
                    "steps": step_results,
                    "reason": "UserPrompt step requires human input for secret fields"
                })
                .to_string());
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

    // All steps complete — persist vault and create credential record.
    // If secret_names was populated by ApiCall extract_secrets, use those.
    // Otherwise, if the flow only had UserInput steps, treat all collected
    // vars as secrets (they're the credential values the user provided).
    if secret_names.is_empty() && !vars.is_empty() {
        for (name, value) in &vars {
            vault.set_secret(name, value.clone());
            if !secret_names.contains(name) {
                secret_names.push(name.clone());
            }
        }
    }

    vault.persist_to_file(vault_path)?;
    let _ = store.delete_credential_setup_state(credential_id);

    if !secret_names.is_empty() {
        let cred = CredentialRecord {
            credential_id: credential_id.to_string(),
            service: service.to_string(),
            secret_name: secret_names[0].clone(),
            inject_as: inject_as.map(str::to_string),
            created_by_agent: Some(manifest.agent.id.clone()),
            expires_at: expires_at.map(str::to_string),
            shared_with: vec![],
            allowed_hosts: allowed_hosts.to_vec(),
            refresh_token_secret_name: None,
            refresh_url: None,
            refresh_method: None,
            refresh_headers: None,
            refresh_extract_access_token: None,
            refresh_extract_refresh_token: None,
            refresh_extract_expires_in: None,
            label: label.map(str::to_string),
        };
        store.upsert_credential(&cred)?;
    }

    Ok(json!({
        "ok": true,
        "credential_id": credential_id,
        "service": service,
        "secrets_stored": secret_names.len(),
        "public_data": public_data,
        "steps": step_results,
    })
    .to_string())
}

// ---------------------------------------------------------------------------
// Template substitution helpers
// ---------------------------------------------------------------------------

/// Resolve `{{vars.x}}`, `{{public.x}}`, `{{agent.id}}`, `{{agent.model}}`,
/// and `{{secrets.x}}` (server-side only — never emitted to LLM) in a string.
pub(crate) fn resolve_template_str(
    s: &str,
    vars: &HashMap<String, String>,
    public_data: &serde_json::Map<String, serde_json::Value>,
    manifest: &AgentManifest,
    vault: &crate::vault::Vault,
) -> String {
    use secrecy::ExposeSecret;

    let mut result = s.to_string();

    for (k, v) in vars {
        result = result.replace(&format!("{{{{vars.{}}}}}", k), v);
    }
    for (k, v) in public_data {
        if let serde_json::Value::String(sv) = v {
            result = result.replace(&format!("{{{{public.{}}}}}", k), sv);
        }
    }
    result = result.replace("{{agent.id}}", &manifest.agent.id);
    if let Some(llm) = &manifest.llm_config {
        result = result.replace("{{agent.model}}", &llm.model);
    }

    // Handle {{secrets.KEY}} — server-side substitution.
    let mut output = String::with_capacity(result.len());
    let prefix = "{{secrets.";
    let suffix = "}}";
    let mut search_from = 0usize;
    loop {
        match result[search_from..].find(prefix) {
            None => {
                output.push_str(&result[search_from..]);
                break;
            }
            Some(rel) => {
                let abs_start = search_from + rel;
                output.push_str(&result[search_from..abs_start]);
                let key_start = abs_start + prefix.len();
                match result[key_start..].find(suffix) {
                    None => {
                        output.push_str(&result[abs_start..]);
                        break;
                    }
                    Some(key_len) => {
                        let key = &result[key_start..key_start + key_len];
                        match vault.get_secret(key) {
                            Some(s) => output.push_str(s.expose_secret()),
                            None => {
                                // Leave unresolved.
                                output.push_str(prefix);
                                output.push_str(key);
                                output.push_str(suffix);
                            }
                        }
                        search_from = key_start + key_len + suffix.len();
                    }
                }
            }
        }
    }
    output
}

fn resolve_template_value(
    value: &serde_json::Value,
    vars: &HashMap<String, String>,
    public_data: &serde_json::Map<String, serde_json::Value>,
    manifest: &AgentManifest,
    vault: &crate::vault::Vault,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(resolve_template_str(s, vars, public_data, manifest, vault))
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        resolve_template_value(v, vars, public_data, manifest, vault),
                    )
                })
                .collect(),
        ),
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| resolve_template_value(v, vars, public_data, manifest, vault))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// JSON path extraction
// ---------------------------------------------------------------------------

/// Extract a value from JSON at a dot-separated or $-prefixed path.
pub(crate) fn extract_json_path(value: &serde_json::Value, path: &str) -> Option<String> {
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

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

/// Extract `https://host` base URL from a full URL.
fn extract_base_url(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        let mut base = parsed.scheme().to_string() + "://";
        if let Some(host) = parsed.host_str() {
            base.push_str(host);
        }
        if let Some(port) = parsed.port() {
            base.push(':');
            base.push_str(&port.to_string());
        }
        base
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_allowed_host, skills_dir, validate_local_skill_path};

    #[test]
    fn normalize_allowed_host_strips_port() {
        assert_eq!(normalize_allowed_host("localhost"), "localhost");
        assert_eq!(normalize_allowed_host("localhost:9876"), "localhost");
        assert_eq!(normalize_allowed_host("example.com:8443"), "example.com");
        assert_eq!(normalize_allowed_host("api.example.com"), "api.example.com");
    }

    #[test]
    fn normalize_allowed_host_preserves_wildcard() {
        assert_eq!(normalize_allowed_host("*"), "*");
    }

    #[test]
    fn normalize_allowed_host_handles_ipv6() {
        // url::Url normalizes IPv6 to brackets-stripped form via host_str()
        assert_eq!(normalize_allowed_host("[::1]:8443"), "[::1]");
        assert_eq!(normalize_allowed_host("[::1]"), "[::1]");
    }

    #[test]
    fn normalize_allowed_host_falls_back_on_parse_failure() {
        // Unparseable entries are returned verbatim so the comparison still
        // fails closed rather than silently matching.
        assert_eq!(normalize_allowed_host(""), "");
    }

    #[test]
    fn validate_local_skill_path_accepts_skills_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway_dir = tmp.path();
        let skill_path = skills_dir(gateway_dir).join("moltbook").join("SKILL.md");
        std::fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
        std::fs::write(&skill_path, "---\n---\n").unwrap();

        let resolved = validate_local_skill_path(gateway_dir, "skills/moltbook/SKILL.md").unwrap();
        assert_eq!(resolved, std::fs::canonicalize(skill_path).unwrap());
    }

    #[test]
    fn validate_local_skill_path_accepts_file_url_inside_skills_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let gateway_dir = tmp.path();
        let skill_path = skills_dir(gateway_dir).join("moltbook.md");
        std::fs::create_dir_all(skills_dir(gateway_dir)).unwrap();
        std::fs::write(&skill_path, "---\n---\n").unwrap();
        let file_url = url::Url::from_file_path(&skill_path).unwrap().to_string();

        let resolved = validate_local_skill_path(gateway_dir, &file_url).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(skill_path).unwrap());
    }
}
