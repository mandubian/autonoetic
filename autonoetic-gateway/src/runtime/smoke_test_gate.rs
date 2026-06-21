//! Install-time smoke-test gate — capability-driven, risk-proportional (P-2.28 / #578).

use autonoetic_types::capability::Capability;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmokeTestInvolvement {
    /// No smoke test required (pure reasoning / zero executable capabilities).
    NotRequired,
    /// Factory runs smoke test automatically; no operator input gate.
    AutoRun,
    /// Operator must confirm or override proposed test input before execution.
    OperatorDirected,
}

/// True when the revision declares executable behavior that must be smoke-tested
/// before first install (`NetworkAccess` or `CodeExecution`).
pub fn revision_requires_smoke_test(capabilities: &[Capability]) -> bool {
    capabilities.iter().any(|cap| {
        matches!(
            cap,
            Capability::NetworkAccess { .. } | Capability::CodeExecution { .. }
        )
    })
}

fn write_scope_is_external(scope: &str) -> bool {
    let scope = scope.trim();
    if scope.is_empty() {
        return false;
    }
    if scope == "self.*" {
        return false;
    }
    if scope.starts_with("self.") {
        return false;
    }
    true
}

/// Mechanical risk classification from declared capabilities + credential mounts.
pub fn smoke_test_involvement(
    capabilities: &[Capability],
    credential_services: &[String],
) -> SmokeTestInvolvement {
    if !revision_requires_smoke_test(capabilities) {
        return SmokeTestInvolvement::NotRequired;
    }
    let has_credentials = credential_services.iter().any(|s| !s.trim().is_empty());
    let external_write = capabilities.iter().any(|cap| {
        if let Capability::WriteAccess { scopes } = cap {
            scopes.iter().any(|s| write_scope_is_external(s))
        } else {
            false
        }
    });
    if has_credentials || external_write {
        SmokeTestInvolvement::OperatorDirected
    } else {
        SmokeTestInvolvement::AutoRun
    }
}

pub fn load_revision_credential_services(
    revision_dir: &Path,
    runtime_lock_rel: &str,
) -> Vec<String> {
    let lock_path = revision_dir.join(runtime_lock_rel);
    let Ok(bytes) = std::fs::read(&lock_path) else {
        return Vec::new();
    };
    let Ok(lock) = serde_yaml::from_slice::<autonoetic_types::runtime_lock::RuntimeLock>(&bytes) else {
        return Vec::new();
    };
    lock.credentials
        .iter()
        .map(|c| c.service.clone())
        .filter(|s| !s.trim().is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_reasoning_does_not_require_smoke_test() {
        let caps = vec![Capability::ReadAccess {
            scopes: vec!["self.*".to_string()],
        }];
        assert!(!revision_requires_smoke_test(&caps));
        assert_eq!(
            smoke_test_involvement(&caps, &[]),
            SmokeTestInvolvement::NotRequired
        );
    }

    #[test]
    fn network_access_requires_smoke_test_auto_run() {
        let caps = vec![Capability::NetworkAccess {
            hosts: vec!["api.open-meteo.com".to_string()],
        }];
        assert!(revision_requires_smoke_test(&caps));
        assert_eq!(
            smoke_test_involvement(&caps, &[]),
            SmokeTestInvolvement::AutoRun
        );
    }

    #[test]
    fn credentials_make_operator_directed() {
        let caps = vec![
            Capability::NetworkAccess {
                hosts: vec!["*".to_string()],
            },
            Capability::WriteAccess {
                scopes: vec!["self.*".to_string()],
            },
        ];
        assert_eq!(
            smoke_test_involvement(&caps, &["trading-api".to_string()]),
            SmokeTestInvolvement::OperatorDirected
        );
    }

    #[test]
    fn external_write_makes_operator_directed() {
        let caps = vec![
            Capability::CodeExecution {
                patterns: vec!["python3 ".to_string()],
                commands: vec![],
            },
            Capability::WriteAccess {
                scopes: vec!["skills/*".to_string()],
            },
        ];
        assert_eq!(
            smoke_test_involvement(&caps, &[]),
            SmokeTestInvolvement::OperatorDirected
        );
    }
}
