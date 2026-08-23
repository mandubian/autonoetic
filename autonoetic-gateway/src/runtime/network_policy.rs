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
///
/// Every failure mode here collapses to `None`, which downstream renders as
/// `missing_remote_access_declaration` ("no declaration exists"). That is a
/// different repair path than "declaration exists but is malformed/empty"
/// (#1110), so each swallow is logged: an unreadable SKILL.md points at
/// agent_dir resolution; a frontmatter parse error at YAML syntax; a
/// `remote_access` key that fails deserialization at the block's own fields
/// (`deny_unknown_fields`: e.g. `hosts:` instead of `targets:`).
pub fn load_manifest_remote_access_declaration(
    agent_dir: &Path,
) -> Option<RemoteAccessDeclaration> {
    let skill_path = agent_dir.join("SKILL.md");
    let skill = match std::fs::read_to_string(&skill_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "network_policy",
                skill_path = %skill_path.display(),
                error = %e,
                "SKILL.md unreadable while loading remote_access declaration; \
                 treating the declaration as absent"
            );
            return None;
        }
    };
    let Some(frontmatter) = skill.split("---").nth(1) else {
        tracing::warn!(
            target: "network_policy",
            skill_path = %skill_path.display(),
            "SKILL.md has no frontmatter segment while loading remote_access \
             declaration; treating the declaration as absent"
        );
        return None;
    };
    let root = match serde_yaml::from_str::<serde_yaml::Value>(frontmatter) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "network_policy",
                skill_path = %skill_path.display(),
                error = %e,
                "SKILL.md frontmatter failed to parse as YAML while loading \
                 remote_access declaration; treating the declaration as absent"
            );
            return None;
        }
    };

    let direct = root.get("remote_access").cloned();
    let nested = root
        .get("metadata")
        .and_then(|m| m.get("autonoetic"))
        .and_then(|a| a.get("remote_access"))
        .cloned();

    match direct.or(nested) {
        None => None,
        Some(raw) => match serde_yaml::from_value::<RemoteAccessDeclaration>(raw) {
            Ok(decl) => Some(decl),
            Err(e) => {
                // The worst case (#1110): the operator shipped a declaration,
                // but a field-name typo or bad casing silently erased it and
                // every denial read as "no declaration exists".
                tracing::warn!(
                    target: "network_policy",
                    skill_path = %skill_path.display(),
                    error = %e,
                    "remote_access declaration failed to deserialize; treating \
                     it as absent (renders as missing_remote_access_declaration). \
                     Fix the block's fields/nesting in SKILL.md."
                );
                None
            }
        },
    }
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// #1106 — `targets: [any]` with `approval_mode: preapproved` is a silent
/// any-host auto-approval: the declaration declares every host, and the
/// preapproved branches (sandbox_exec / artifact_exec) auto-approve on mere
/// NetworkAccess capability *presence*, without consulting the capability's
/// host list. The only shape in which the combination adds nothing is a
/// wildcard capability (`hosts: ["*"]`) **with `open_web: true`** — the
/// capability is then already the live any-host authority (genuine open-web
/// roles; a wildcard without `open_web` grants nothing at runtime and is
/// rejected at install time, so it must not launder an any-declaration
/// through here either). Anything else means the static declaration is
/// doing the widening, and it must not. Like the preapproved-requires-
/// capability check this is a manifest inconsistency: fail shut, never
/// overridable by an operator approval.
pub fn validate_any_preapproval_shape(
    manifest: &AgentManifest,
    decl: &RemoteAccessDeclaration,
) -> Result<(), NetworkPolicyViolation> {
    let declares_any = decl
        .targets
        .iter()
        .any(|t| matches!(t, autonoetic_types::background::GrantTarget::Any));
    if !matches!(decl.approval_mode, RemoteAccessApprovalMode::Preapproved) || !declares_any {
        return Ok(());
    }
    let wildcard_capability = manifest.open_web
        && manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::NetworkAccess { hosts } if hosts.iter().any(|h| h.trim() == "*"))
        });
    if wildcard_capability {
        return Ok(());
    }
    Err(NetworkPolicyViolation::new(
        "remote_any_preapproval_requires_wildcard_capability",
        format!(
            "Agent `{}` declared remote_access targets:[any] with approval_mode=preapproved, but its NetworkAccess capability hosts are not a wildcard — the static declaration is silently widening auto-approval to every host.",
            manifest.agent.id
        ),
        Some(
            "Set approval_mode to required (the operator approval is the control, host named on the card), or name concrete targets, or declare NetworkAccess hosts: [\"*\"] with open_web: true if the role is genuinely open-web.".to_string(),
        ),
    ))
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
///
/// `check_capability_hosts` controls whether the NetworkAccess capability
/// host list is hard-enforced here (`undeclared_network_host`). Callers that
/// route capability denials into their own approval flow (web tools: the
/// `host_allowed` check feeds GateService approval minting) must pass
/// `DeferToCaller`; enforcing here would short-circuit the ask-the-operator
/// path with a hard error (the #579 regression behind #933). Callers whose
/// only capability gate is this function pass `Enforce`.
pub fn enforce_remote_target_policy(
    manifest: &AgentManifest,
    agent_dir: &Path,
    host: &str,
    request_url: Option<&str>,
    declaration_requirement: DeclarationRequirement,
    capability_host_check: CapabilityHostCheck,
) -> Result<Option<RemoteAccessDeclaration>, NetworkPolicyViolation> {
    let declaration = load_manifest_remote_access_declaration(agent_dir);
    let Some(decl) = declaration else {
        return match declaration_requirement {
            DeclarationRequirement::Optional => Ok(None),
            DeclarationRequirement::Required => Err(NetworkPolicyViolation::new(
                "missing_remote_access_declaration",
                format!(
                    "Agent `{}` attempted outbound network access to `{}` without a parsable metadata.autonoetic.remote_access declaration in SKILL.md (absent, unreadable, or failed to deserialize — see the gateway log).",
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

    validate_any_preapproval_shape(manifest, &decl)?;

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

    if capability_host_check == CapabilityHostCheck::Enforce
        && has_network_capability
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

/// Whether the NetworkAccess capability host list is hard-enforced inside
/// [`enforce_remote_target_policy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityHostCheck {
    /// Hard-fail with `undeclared_network_host` when the host is not covered.
    Enforce,
    /// Skip the check — the caller enforces the capability itself and routes
    /// denials into an approval flow (ask-the-operator), so a hard error here
    /// would bypass it.
    DeferToCaller,
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};

    fn manifest(network: bool) -> AgentManifest {
        AgentManifest {
            remote_access: None,
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
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
            egress: None,
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
    fn any_preapproval_without_wildcard_capability_fails_shut() {
        // #1106: any + preapproved + narrow capability = silent any-host
        // auto-approval via the capability-presence preapproved branches.
        let decl = RemoteAccessDeclaration {
            approval_mode: RemoteAccessApprovalMode::Preapproved,
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
        let mut m = manifest(true);
        m.capabilities = vec![Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string()],
        }];
        let err = validate_any_preapproval_shape(&m, &decl).unwrap_err();
        assert_eq!(
            err.error_type, "remote_any_preapproval_requires_wildcard_capability",
            "narrow capability must not launder an any-declaration: {}",
            err.message
        );
    }

    #[test]
    fn any_preapproval_with_wildcard_capability_is_redundant_not_forbidden() {
        // Genuine open-web roles (open_web: true, hosts: ["*"]): the capability
        // is already the any-host authority — the declaration adds nothing.
        let decl = RemoteAccessDeclaration {
            approval_mode: RemoteAccessApprovalMode::Preapproved,
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
        let mut m = manifest(true);
        m.capabilities = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        m.open_web = true;
        validate_any_preapproval_shape(&m, &decl).expect("wildcard capability");
    }

    #[test]
    fn wildcard_without_open_web_does_not_launder_any_preapproval() {
        // A wildcard capability without open_web grants nothing at runtime
        // (network_access_allows_host returns false) and is rejected at
        // install time — it must not satisfy the guard either, or a manifest
        // that slipped past install validation would keep the silent
        // any-host auto-approval.
        let decl = RemoteAccessDeclaration {
            approval_mode: RemoteAccessApprovalMode::Preapproved,
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
        let mut m = manifest(true);
        m.capabilities = vec![Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        }];
        m.open_web = false;
        assert!(validate_any_preapproval_shape(&m, &decl).is_err());
    }

    #[test]
    fn any_targets_with_required_mode_is_the_endorsed_shape() {
        // executor.default's documented shape (#1106): declaration-wide,
        // approval-per-host. The operator approval is the control.
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
        let m = manifest(false);
        validate_any_preapproval_shape(&m, &decl).expect("required mode");
    }

    #[test]
    fn enumerated_targets_preapproved_narrow_capability_is_consistent() {
        // Declaration and capability agree on the same host set — no
        // declaration-side widening, guard stays out of the way.
        let decl = RemoteAccessDeclaration {
            approval_mode: RemoteAccessApprovalMode::Preapproved,
            targets: vec![GrantTarget::ExactHost("api.github.com".to_string())],
            enabled_languages: vec![],
            python_imports: vec![],
            js_imports: vec![],
            rust_imports: vec![],
            go_imports: vec![],
            function_calls: vec![],
            shell_commands: vec![],
            package_manager_commands: vec![],
        };
        let mut m = manifest(true);
        m.capabilities = vec![Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string()],
        }];
        validate_any_preapproval_shape(&m, &decl).expect("matching shapes");
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
            CapabilityHostCheck::Enforce,
        )
        .expect_err("should fail");
        assert_eq!(
            err.error_type,
            "remote_preapproval_requires_network_capability"
        );
    }

    #[test]
    fn empty_targets_declaration_yields_undeclared_not_missing() {
        // #1110 pin: a shipped remote_access block with targets: [] is a
        // PRESENT declaration that covers nothing — the denial must be
        // `undeclared_remote_target` (widen the declaration), never
        // `missing_remote_access_declaration` ("no declaration exists" —
        // a different repair path).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("SKILL.md"),
            r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: required
      targets: []
---
"#,
        )
        .expect("skill write");
        assert!(
            load_manifest_remote_access_declaration(tmp.path()).is_some(),
            "an empty-targets block must deserialize, not silently vanish"
        );
        let err = enforce_remote_target_policy(
            &manifest(true),
            tmp.path(),
            "127.0.0.1",
            Some("http://127.0.0.1:8080/api"),
            DeclarationRequirement::Required,
            CapabilityHostCheck::DeferToCaller,
        )
        .expect_err("empty targets cover nothing");
        assert_eq!(err.error_type, "undeclared_remote_target", "{}", err.message);
    }

    #[test]
    fn malformed_declaration_block_is_logged_and_treated_as_missing() {
        // #1110 worst case: `deny_unknown_fields` means one wrong field name
        // (hosts: instead of targets:) silently erased the whole block and
        // every denial read as "no declaration exists". The loader now warns
        // (observability) and still fails as missing — pinned here so the
        // collapse stays deliberate and the error message keeps pointing at
        // the gateway log.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("SKILL.md"),
            r#"---
metadata:
  autonoetic:
    remote_access:
      approval_mode: required
      hosts:
        - "api.example.com"
---
"#,
        )
        .expect("skill write");
        assert!(load_manifest_remote_access_declaration(tmp.path()).is_none());
        let err = enforce_remote_target_policy(
            &manifest(true),
            tmp.path(),
            "api.example.com",
            None,
            DeclarationRequirement::Required,
            CapabilityHostCheck::DeferToCaller,
        )
        .expect_err("malformed block collapses to missing");
        assert_eq!(
            err.error_type, "missing_remote_access_declaration",
            "{}",
            err.message
        );
        assert!(
            err.message.contains("failed to deserialize"),
            "the message must name the parse possibility, not claim flat absence: {}",
            err.message
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
            CapabilityHostCheck::Enforce,
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
            CapabilityHostCheck::Enforce,
        )
        .expect_err("host outside NetworkAccess should be blocked");
        assert_eq!(err.error_type, "undeclared_network_host");
    }
}
