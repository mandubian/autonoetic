use autonoetic_types::agent::{
    AgentManifest, RemoteAccessApprovalMode, RemoteAccessDeclaration,
};
use autonoetic_types::background::GrantTarget;
use autonoetic_types::capability::Capability;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationRequirement {
    Optional,
    Required,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicyViolation {
    pub error_type: &'static str,
    pub message: String,
    pub repair_hint: Option<String>,
}

impl NetworkPolicyViolation {
    fn new(error_type: &'static str, message: String, repair_hint: Option<String>) -> Self {
        Self {
            error_type,
            message,
            repair_hint,
        }
    }
}

/// Load `metadata.autonoetic.remote_access` (or top-level `remote_access`) from SKILL.md.
pub fn load_manifest_remote_access_declaration(
    agent_dir: &Path,
) -> Option<RemoteAccessDeclaration> {
    let skill_path = agent_dir.join("SKILL.md");
    let skill = std::fs::read_to_string(skill_path).ok()?;
    let frontmatter = skill.split("---").nth(1)?;
    let root = serde_yaml::from_str::<serde_yaml::Value>(frontmatter).ok()?;

    let direct = root.get("remote_access").cloned();
    let nested = root
        .get("metadata")
        .and_then(|m| m.get("autonoetic"))
        .and_then(|a| a.get("remote_access"))
        .cloned();

    direct
        .or(nested)
        .and_then(|v| serde_yaml::from_value::<RemoteAccessDeclaration>(v).ok())
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Returns true when the declaration allows the provided host/URL target.
pub fn declaration_allows_target(
    declaration: &RemoteAccessDeclaration,
    host: &str,
    request_url: Option<&str>,
) -> bool {
    let mut candidates = Vec::new();
    let normalized_host = normalize_host(host);
    if normalized_host.is_empty() {
        return false;
    }
    candidates.push(normalized_host.clone());

    if let Some(url_raw) = request_url {
        if let Ok(parsed) = reqwest::Url::parse(url_raw) {
            if let Some(parsed_host) = parsed.host_str() {
                let parsed_host = normalize_host(parsed_host);
                if !parsed_host.is_empty() && !candidates.contains(&parsed_host) {
                    candidates.push(parsed_host.clone());
                }
                if let Some(port) = parsed.port_or_known_default() {
                    candidates.push(format!("{}:{}", parsed_host, port));
                }
            }
        }
        candidates.push(url_raw.to_string());
    }

    if declaration.targets.is_empty() {
        return false;
    }

    declaration.targets.iter().any(|target| {
        candidates
            .iter()
            .any(|candidate| target.matches(candidate))
    })
}

/// Enforce remote target declaration constraints for a concrete outbound request.
///
/// This is the shared declaration resolver used by `sandbox.exec`, `web.*`,
/// and credential HTTP paths.
pub fn enforce_remote_target_policy(
    manifest: &AgentManifest,
    agent_dir: &Path,
    host: &str,
    request_url: Option<&str>,
    declaration_requirement: DeclarationRequirement,
) -> Result<Option<RemoteAccessDeclaration>, NetworkPolicyViolation> {
    let declaration = load_manifest_remote_access_declaration(agent_dir);
    let Some(decl) = declaration else {
        return match declaration_requirement {
            DeclarationRequirement::Optional => Ok(None),
            DeclarationRequirement::Required => Err(NetworkPolicyViolation::new(
                "missing_remote_access_declaration",
                format!(
                    "Agent `{}` attempted outbound network access to `{}` without metadata.autonoetic.remote_access declaration in SKILL.md.",
                    manifest.agent.id, host
                ),
                Some(
                    "Declare metadata.autonoetic.remote_access with targets and explicit network patterns."
                        .to_string(),
                ),
            )),
        };
    };

    let has_network_capability = manifest
        .capabilities
        .iter()
        .any(|c| matches!(c, Capability::NetworkAccess { .. }));
    if matches!(decl.approval_mode, RemoteAccessApprovalMode::Preapproved)
        && !has_network_capability
    {
        return Err(NetworkPolicyViolation::new(
            "remote_preapproval_requires_network_capability",
            format!(
                "Agent `{}` declared remote_access.approval_mode=preapproved but does not have NetworkAccess capability.",
                manifest.agent.id
            ),
            Some(
                "Either add NetworkAccess capability or set metadata.autonoetic.remote_access.approval_mode to required.".to_string(),
            ),
        ));
    }

    if !declaration_allows_target(&decl, host, request_url) {
        return Err(NetworkPolicyViolation::new(
            "undeclared_remote_target",
            format!(
                "Outbound target `{}` is not covered by metadata.autonoetic.remote_access target declarations.",
                host
            ),
            Some(
                "Add an explicit remote_access target (any/exact_host/host_suffix/host_and_port/url_prefix) for this destination.".to_string(),
            ),
        ));
    }

    if has_network_capability
        && !crate::runtime::network_host_contract::network_access_allows_host(manifest, host)
    {
        return Err(NetworkPolicyViolation::new(
            "undeclared_network_host",
            format!(
                "Outbound target `{}` is not covered by NetworkAccess capability hosts for agent `{}`.",
                host, manifest.agent.id
            ),
            Some(
                "Add the host to NetworkAccess capability hosts, or declare open_web: true only for genuine open-web agents.".to_string(),
            ),
        ));
    }

    Ok(Some(decl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};

    fn manifest(network: bool) -> AgentManifest {
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
            singleton: false,
            resident_idle_ttl_secs: None,
        },
            capabilities: if network {
                vec![Capability::NetworkAccess {
                    hosts: vec!["*".to_string()],
                }]
            } else {
                vec![]
            },
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
    fn declaration_target_exact_host_matches() {
        let decl = RemoteAccessDeclaration {
            approval_mode: RemoteAccessApprovalMode::Required,
            targets: vec![GrantTarget::ExactHost("api.example.com".to_string())],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec![],
            package_manager_commands: vec![],
        };
        assert!(declaration_allows_target(
            &decl,
            "api.example.com",
            Some("https://api.example.com/v1")
        ));
        assert!(!declaration_allows_target(
            &decl,
            "api.other.com",
            Some("https://api.other.com/v1")
        ));
    }

    #[test]
    fn declaration_target_url_prefix_matches() {
        let decl = RemoteAccessDeclaration {
            approval_mode: RemoteAccessApprovalMode::Required,
            targets: vec![GrantTarget::UrlPrefix(
                "https://api.example.com/public/".to_string(),
            )],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec![],
            package_manager_commands: vec![],
        };
        assert!(declaration_allows_target(
            &decl,
            "api.example.com",
            Some("https://api.example.com/public/users")
        ));
        assert!(!declaration_allows_target(
            &decl,
            "api.example.com",
            Some("https://api.example.com/private/users")
        ));
    }

    #[test]
    fn declaration_target_any_matches_anything() {
        let decl = RemoteAccessDeclaration {
            approval_mode: RemoteAccessApprovalMode::Required,
            targets: vec![GrantTarget::Any],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec![],
            package_manager_commands: vec![],
        };
        assert!(declaration_allows_target(
            &decl,
            "any.host.example",
            Some("https://any.host.example/path")
        ));
        assert!(declaration_allows_target(&decl, "10.20.30.40", None));
    }

    #[test]
    fn preapproved_requires_network_capability() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("SKILL.md"),
            r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: preapproved
      targets:
        - kind: exact_host
          value: "api.example.com"
---
"#,
        )
        .expect("skill write");

        let err = enforce_remote_target_policy(
            &manifest(false),
            tmp.path(),
            "api.example.com",
            Some("https://api.example.com/v1"),
            DeclarationRequirement::Required,
        )
        .expect_err("should fail");
        assert_eq!(
            err.error_type,
            "remote_preapproval_requires_network_capability"
        );
    }

    #[test]
    fn missing_declaration_optional_allows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = enforce_remote_target_policy(
            &manifest(true),
            tmp.path(),
            "api.example.com",
            Some("https://api.example.com/v1"),
            DeclarationRequirement::Optional,
        )
        .expect("optional declaration should allow transitional path");
        assert!(result.is_none());
    }

    fn manifest_with_hosts(hosts: Vec<String>) -> AgentManifest {
        let mut m = manifest(false);
        m.capabilities = vec![Capability::NetworkAccess { hosts }];
        m
    }

    #[test]
    fn network_access_blocks_host_not_in_capability() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("SKILL.md"),
            r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: required
      targets:
        - kind: any
---
"#,
        )
        .expect("skill write");

        let err = enforce_remote_target_policy(
            &manifest_with_hosts(vec!["api.example.com".to_string()]),
            tmp.path(),
            "evil.com",
            Some("https://evil.com/secret"),
            DeclarationRequirement::Required,
        )
        .expect_err("host outside NetworkAccess should be blocked");
        assert_eq!(err.error_type, "undeclared_network_host");
    }
}
