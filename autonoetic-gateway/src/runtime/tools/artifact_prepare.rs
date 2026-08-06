use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::approved_exec_cache::{
    compute_fingerprint, normalize_targets, ApprovedExecCache,
};
use crate::runtime::remote_access::{classify_network_coverage, default_remote_access_detector};
use crate::runtime::tools::{
    build_approval_details, CredentialEnvMapping, NativeTool, NativeToolRegistry,
};
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ApprovalStatus, ScheduledAction,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ArtifactPrepareTool));
}

#[derive(Debug, Deserialize)]
struct RequiredCredential {
    credential_id: String,
    env_var: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactPrepareArgs {
    artifact_ref: String,
    entrypoint: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    required_credentials: Option<Vec<RequiredCredential>>,
}

pub struct ArtifactPrepareTool;

impl NativeTool for ArtifactPrepareTool {
    fn name(&self) -> &'static str {
        "artifact_prepare"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ArtifactExecution))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "One-pass preflight check for artifact execution. Analyzes artifact source for remote access, resolves credentials from the vault, and creates a single approval covering all domains + credential injection. Returns a deployment_ticket that artifact_exec can use to execute without further approvals or credential setup.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact_ref": {
                        "type": "string",
                        "description": "Artifact ref to prepare for execution (e.g., 'ar.aabb1234ef56')"
                    },
                    "entrypoint": {
                        "type": "string",
                        "description": "Entrypoint file within the artifact (e.g., 'main.py')"
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments that will be passed to the entrypoint (used for approval context, not executed)"
                    },
                    "required_credentials": {
                        "type": "array",
                        "description": "Credentials to inject during execution. Each entry maps a vault credential to an environment variable.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "credential_id": {
                                    "type": "string",
                                    "description": "Credential ID from credential_check or planner delegation"
                                },
                                "env_var": {
                                    "type": "string",
                                    "description": "Environment variable name (e.g., 'API_KEY')"
                                }
                            },
                            "required": ["credential_id", "env_var"]
                        }
                    }
                },
                "required": ["artifact_ref", "entrypoint"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<GatewayStore>>,
        run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: ArtifactPrepareArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let gw_dir = gateway_dir
            .ok_or_else(|| anyhow::anyhow!("artifact.prepare requires a gateway directory"))?;
        let store = gateway_store
            .ok_or_else(|| anyhow::anyhow!("artifact.prepare requires a GatewayStore"))?;
        let cfg =
            config.ok_or_else(|| anyhow::anyhow!("artifact.prepare requires a GatewayConfig"))?;
        let sid =
            session_id.ok_or_else(|| anyhow::anyhow!("artifact.prepare requires a session_id"))?;

        let resolved = crate::runtime::tools::artifact::resolve_artifact_ref_or_canonical(
            &args.artifact_ref,
            sid,
            &store,
            gw_dir,
        )?;
        let artifact_id = resolved.artifact_id;

        let artifact_store = crate::artifact_store::ArtifactStore::new(gw_dir)?;
        let bundle = artifact_store.inspect(&artifact_id)?;
        anyhow::ensure!(
            bundle.files.iter().any(|f| f.name == args.entrypoint),
            "entrypoint '{}' not found in artifact '{}'",
            args.entrypoint,
            artifact_id
        );

        let resolved_files = artifact_store.resolve_files(&artifact_id)?;
        let mut artifact_code = String::new();
        let mut workspace_files: Vec<(String, String)> = Vec::new();
        for (name, content) in &resolved_files {
            if let Ok(text) = std::str::from_utf8(content) {
                if name == &args.entrypoint {
                    artifact_code = text.to_string();
                }
                workspace_files.push((name.clone(), text.to_string()));
            }
        }
        anyhow::ensure!(
            !artifact_code.is_empty(),
            "entrypoint '{}' could not be read as text from artifact '{}'",
            args.entrypoint,
            artifact_id
        );

        let remote_analysis =
            default_remote_access_detector()
                .analyze_code_with_workspace(&artifact_code, &workspace_files);

        let agent_has_network_access = manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { .. }));

        let needs_approval = remote_analysis.requires_approval && !agent_has_network_access;

        let mut resolved_credential_env: Vec<CredentialEnvMapping> = Vec::new();
        if let Some(required) = &args.required_credentials {
            let vault_dir = gw_dir.parent().unwrap_or(gw_dir);
            crate::vault::ensure_default_key(vault_dir)?;
            let vault_path = crate::vault::default_vault_path(vault_dir);
            let vault = crate::vault::Vault::load_from_file(&vault_path)?;

            for rc in required {
                crate::runtime::tools::ensure_safe_credential_id_reference(&rc.credential_id)?;
                let cred = store.get_credential(&rc.credential_id)?.ok_or_else(|| {
                    anyhow::anyhow!("required_credentials: credential reference not found in store")
                })?;
                if vault.get_secret(&cred.secret_name).is_none() {
                    return Ok(ToolError::resource(
                        "required_credentials: secret for referenced credential not found in vault",
                        None::<String>,
                    )
                    .to_error_response());
                }
                tracing::info!(
                    target: "artifact_prepare",
                    credential_id = %rc.credential_id,
                    env_var = %rc.env_var,
                    "Credential resolved"
                );
                resolved_credential_env.push(CredentialEnvMapping {
                    credential_id: rc.credential_id.clone(),
                    env_var: rc.env_var.clone(),
                });
            }
        }

        if !needs_approval {
            let ticket_id = autonoetic_types::id_format::short_random_id_hex("dtk-", 12);
            store_deployment_ticket(
                &store,
                &ticket_id,
                &artifact_id,
                &args.entrypoint,
                &resolved_credential_env,
                &[],
            )?;
            return Ok(serde_json::json!({
                "ok": true,
                "deployment_ticket": ticket_id,
                "artifact_ref": args.artifact_ref,
                "entrypoint": args.entrypoint,
                "remote_access": {
                    "detected": remote_analysis.requires_approval,
                    "auto_approved": agent_has_network_access,
                    "domains": [],
                },
                "credentials_resolved": resolved_credential_env.len(),
                "message": "Artifact is ready to execute. Use deployment_ticket with artifact.exec."
            })
            .to_string());
        }

        let detected_patterns = remote_analysis.detected_patterns.clone();
        let concrete_targets = normalize_targets(&detected_patterns);
        let coverage = classify_network_coverage(&detected_patterns, concrete_targets.clone());

        let targets = match &coverage {
            crate::runtime::remote_access::NetworkCoverage::Concrete { targets } => targets.clone(),
            _ => Vec::new(),
        };

        let root_sid = crate::runtime::content_store::root_session_id(sid);

        let mut pre_validated = false;
        let mut fingerprint_for_backfill: Option<String> = None;
        if let crate::runtime::remote_access::NetworkCoverage::Concrete { targets } = &coverage {
            let fingerprint = compute_fingerprint(
                &manifest.agent.id,
                targets,
                &artifact_code,
                Some(&bundle.artifact_canonical_digest),
                &manifest.capabilities,
            );
            fingerprint_for_backfill = Some(fingerprint.clone());
            if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                if cache
                    .find(&fingerprint, cfg.default_grant_ttl_secs)
                    .is_some()
                {
                    pre_validated = true;
                }
            }
        }

        let credential_summary: Vec<serde_json::Value> = resolved_credential_env
            .iter()
            .map(|c| {
                serde_json::json!({
                    "credential_id": c.credential_id,
                    "env_var": c.env_var,
                })
            })
            .collect();

        let command = format!("{} {}", args.entrypoint, args.args.join(" "));
        let summary = if resolved_credential_env.is_empty() {
            format!("Artifact {}: {}", artifact_id, remote_analysis.summary)
        } else {
            format!(
                "Artifact {}: {} + {} credential(s) injected as env vars",
                artifact_id,
                remote_analysis.summary,
                resolved_credential_env.len()
            )
        };

        let action = ScheduledAction::SandboxExec {
            command,
            dependencies: None,
            requires_approval: true,
            evidence_ref: None,
            detected_hosts: Some(targets.clone()),
            intent: None,
        };

        let gate = crate::runtime::human_gate::GateService::new(store.clone());
        let gate_result = gate.check(
            crate::runtime::human_gate::GateRequest {
                kind: crate::runtime::human_gate::GateKind::Approval {
                    action: action.clone(),
                    targets: targets.clone(),
                    match_strategy: crate::runtime::human_gate::MatchStrategy::HostLevel,
                },
                manifest,
                session_id: Some(sid),
                run_context,
                config: Some(cfg),
                context: crate::runtime::human_gate::DecisionContext::tier2(
                    format!("prepare + exec artifact {} ({})", artifact_id, args.entrypoint),
                    if targets.is_empty() {
                        "preparing a stored artifact for execution requires operator approval".to_string()
                    } else {
                        format!(
                            "artifact execution reaching host(s) [{}] not covered by an approved network grant",
                            targets.join(", ")
                        )
                    },
                    if resolved_credential_env.is_empty() {
                        format!("runs artifact {} in the sandbox; effects depend on the entrypoint", artifact_id)
                    } else {
                        format!(
                            "runs artifact {} in the sandbox with {} credential(s) injected as env vars; the credentials are exposed to the executed code",
                            artifact_id,
                            resolved_credential_env.len()
                        )
                    },
                    "Approve if the artifact, entrypoint, network targets, and any injected credentials are expected for this agent's task; reject or escalate if any are unexpected",
                )
                .with_analysis(summary.clone()),
                summary: summary.clone(),
                approval_ref: None,
                pre_validated,
                cache_backfill: None,
                request_id: None,
                turn_id: None,
            },
        )?;

        match gate_result {
            crate::runtime::human_gate::GateResult::Cleared { source, .. } => {
                if source == crate::runtime::human_gate::ClearanceSource::SessionGrant {
                    if let Some(fp) = fingerprint_for_backfill {
                        if let Ok(cache) = ApprovedExecCache::new(gw_dir) {
                            if cache.find(&fp, 0).is_none() {
                                let entry =
                                    crate::runtime::approved_exec_cache::ApprovedExecEntry {
                                        fingerprint: fp,
                                        agent_id: manifest.agent.id.clone(),
                                        remote_targets: targets.clone(),
                                        code_content: artifact_code.clone(),
                                        approval_request_id: String::new(),
                                        approved_at: chrono::Utc::now().to_rfc3339(),
                                        approved_by: "operator".to_string(),
                                        last_used_at: chrono::Utc::now().to_rfc3339(),
                                    };
                                let _ = cache.record(entry);
                            }
                        }
                    }
                }
                let ticket_id = autonoetic_types::id_format::short_random_id_hex("dtk-", 12);
                store_deployment_ticket(
                    &store,
                    &ticket_id,
                    &artifact_id,
                    &args.entrypoint,
                    &resolved_credential_env,
                    &targets,
                )?;
                Ok(serde_json::json!({
                    "ok": true,
                    "deployment_ticket": ticket_id,
                    "artifact_ref": args.artifact_ref,
                    "entrypoint": args.entrypoint,
                    "remote_access": {
                        "detected": true,
                        "auto_approved": true,
                        "reason": if source == crate::runtime::human_gate::ClearanceSource::SessionGrant {
                            "session_grant"
                        } else {
                            "cached"
                        },
                        "domains": targets,
                    },
                    "credentials_resolved": resolved_credential_env.len(),
                    "message": "Artifact is ready to execute. Use deployment_ticket with artifact.exec."
                })
                .to_string())
            }
            crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
                let pending = store
                    .get_approval(&gate_id)?
                    .unwrap_or_else(|| ApprovalRequest {
                        request_id: gate_id.clone(),
                        agent_id: manifest.agent.id.clone(),
                        session_id: sid.to_string(),
                        root_session_id: None,
                        workflow_id: None,
                        task_id: None,
                        action: action.clone(),
                        created_at: String::new(),
                        status: None,
                        decided_at: None,
                        decided_by: None,
                        reason: Some(summary.clone()),
                        evidence_ref: None,
                        decision_reason: None,
                        approval_level: crate::scheduler::approval::resolve_approval_level(
                            cfg, &action,
                        ),
                        min_dwell_ms: None,
                        confirm_phrase: None,
                        code_excerpts: None,
                        risk_summary: None,

                        expires_at: None,
                    });
                let approval = build_approval_details(
                    &pending,
                    "artifact_prepare",
                    summary.clone(),
                    "approval_ref",
                    serde_json::json!({
                        "artifact_ref": args.artifact_ref,
                        "entrypoint": args.entrypoint,
                        "approval_already_pending": true,
                    }),
                );
                Ok(serde_json::json!({
                    "ok": false,
                    "artifact_ref": args.artifact_ref,
                    "entrypoint": args.entrypoint,
                    "approval_required": true,
                    "approval_already_pending": true,
                    "suspended": true,
                    "message": format!("Approval {} is already pending.", gate_id),
                    "approval": approval
                })
                .to_string())
            }
            crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
                // Populate code excerpts + risk summary for operator inspection.
                let excerpts =
                    crate::runtime::code_excerpts::build_code_excerpts(&artifact_id, gw_dir);
                let _ = store.set_approval_code_excerpts(&gate_id, excerpts.as_deref(), None);
                let artifact_store_ref = crate::ArtifactStore::new(gw_dir).ok();
                let risk_summary = crate::runtime::code_excerpts::build_risk_summary(
                    Some(&targets),
                    None,
                    &artifact_id,
                    artifact_store_ref.as_ref(),
                );
                if let Some(rs) = risk_summary {
                    let _ = store.set_approval_code_excerpts(&gate_id, None, Some(&rs));
                }

                let approval = build_approval_details(
                    &ApprovalRequest {
                        request_id: gate_id.clone(),
                        agent_id: manifest.agent.id.clone(),
                        session_id: sid.to_string(),
                        root_session_id: None,
                        workflow_id: None,
                        task_id: None,
                        action: action.clone(),
                        created_at: String::new(),
                        status: None,
                        decided_at: None,
                        decided_by: None,
                        reason: Some(summary.clone()),
                        evidence_ref: None,
                        decision_reason: None,
                        approval_level: crate::scheduler::approval::resolve_approval_level(
                            cfg, &action,
                        ),
                        min_dwell_ms: None,
                        confirm_phrase: None,
                        code_excerpts: None,
                        risk_summary: None,

                        expires_at: None,
                    },
                    "artifact_prepare",
                    summary.clone(),
                    "approval_ref",
                    serde_json::json!({
                        "artifact_ref": args.artifact_ref,
                        "entrypoint": args.entrypoint,
                        "remote_access_detected": true,
                        "detected_patterns": detected_patterns,
                        "normalized_targets": targets,
                        "credentials": credential_summary,
                    }),
                );

                let pending_ticket_id = autonoetic_types::id_format::short_random_id_hex("dtk-", 12);
                store_deployment_ticket(
                    &store,
                    &pending_ticket_id,
                    &artifact_id,
                    &args.entrypoint,
                    &resolved_credential_env,
                    &targets,
                )?;

                Ok(serde_json::json!({
                    "ok": false,
                    "deployment_ticket": pending_ticket_id,
                    "artifact_ref": args.artifact_ref,
                    "entrypoint": args.entrypoint,
                    "remote_access": {
                        "detected": true,
                        "auto_approved": false,
                        "domains": targets,
                    },
                    "credentials_resolved": resolved_credential_env.len(),
                    "approval_required": true,
                    "request_id": gate_id,
                    "suspended": true,
                    "message": format!("Approval required for {} domain(s) + {} credential(s). Approve {}, then use deployment_ticket with artifact.exec.",
                        targets.len(), resolved_credential_env.len(), gate_id),
                    "approval": approval
                })
                .to_string())
            }
            other => {
                return Err(anyhow::anyhow!(
                    "Unexpected gate result for artifact.prepare: {:?}",
                    other
                ));
            }
        }
    }
}

fn store_deployment_ticket(
    store: &Arc<GatewayStore>,
    ticket_id: &str,
    artifact_id: &str,
    entrypoint: &str,
    credential_env: &[CredentialEnvMapping],
    approved_domains: &[String],
) -> anyhow::Result<()> {
    let cred_mappings: Vec<HashMap<&str, &str>> = credential_env
        .iter()
        .map(|c| {
            let mut m = HashMap::new();
            m.insert("credential_id", c.credential_id.as_str());
            m.insert("env_var", c.env_var.as_str());
            m
        })
        .collect();
    let ticket_data = serde_json::json!({
        "artifact_id": artifact_id,
        "entrypoint": entrypoint,
        "credential_env": cred_mappings,
        "approved_domains": approved_domains,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    store.save_credential_setup_state(ticket_id, &ticket_data.to_string())?;
    Ok(())
}

pub fn resolve_deployment_ticket(
    store: &GatewayStore,
    ticket_id: &str,
) -> anyhow::Result<Option<DeploymentTicket>> {
    let raw = match store.load_credential_setup_state(ticket_id)? {
        Some(v) => v,
        None => return Ok(None),
    };
    let data: serde_json::Value = serde_json::from_str(&raw)?;
    let artifact_id = data["artifact_id"].as_str().unwrap_or_default().to_string();
    let entrypoint = data["entrypoint"].as_str().unwrap_or_default().to_string();
    let credential_env = data["credential_env"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    Some(CredentialEnvMapping {
                        credential_id: v.get("credential_id")?.as_str()?.to_string(),
                        env_var: v.get("env_var")?.as_str()?.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let approved_domains = data["approved_domains"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Some(DeploymentTicket {
        artifact_id,
        entrypoint,
        credential_env,
        approved_domains,
    }))
}

pub struct DeploymentTicket {
    pub artifact_id: String,
    pub entrypoint: String,
    pub credential_env: Vec<CredentialEnvMapping>,
    pub approved_domains: Vec<String>,
}
