//! Integration tests for capability-driven sandbox isolation.
//!
//! Tests:
//! - BwrapIsolationOverrides::from_capabilities derives share_net from NetworkAccess
//! - append_bwrap_isolation_flags includes --share-net when override is set
//! - append_bwrap_isolation_flags falls back to global config when override is None

use autonoetic_gateway::sandbox::{append_bwrap_isolation_flags, BwrapIsolationOverrides};
use autonoetic_types::capability::Capability;

#[test]
fn test_overrides_from_capabilities_with_network_access() {
    let caps = vec![
        Capability::CodeExecution {
            patterns: vec!["*".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["api.example.com".to_string()],
        },
    ];
    let overrides = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(
        overrides.share_net,
        "NetworkAccess should set share_net=true"
    );
}

#[test]
fn test_overrides_from_capabilities_without_network_access() {
    let caps = vec![
        Capability::CodeExecution {
            patterns: vec!["*".to_string()],
        },
        Capability::ReadAccess {
            scopes: vec!["self.*".to_string()],
        },
    ];
    let overrides = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(
        !overrides.share_net,
        "No NetworkAccess should set share_net=false"
    );
}

#[test]
fn test_overrides_from_capabilities_empty() {
    let caps: Vec<Capability> = vec![];
    let overrides = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(
        !overrides.share_net,
        "Empty capabilities should set share_net=false"
    );
}

#[test]
fn test_overrides_from_capabilities_empty_hosts() {
    let caps = vec![Capability::NetworkAccess { hosts: vec![] }];
    let overrides = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(
        !overrides.share_net,
        "NetworkAccess with empty hosts should set share_net=false"
    );
}

#[test]
fn test_overrides_from_capabilities_multiple_hosts() {
    let caps = vec![Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string(), "cdn.example.com".to_string()],
    }];
    let overrides = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(
        overrides.share_net,
        "NetworkAccess with hosts should set share_net=true"
    );
}

#[test]
fn test_append_bwrap_isolation_flags_with_override_share_net() {
    let overrides = BwrapIsolationOverrides { share_net: true };
    let mut argv = vec![];
    append_bwrap_isolation_flags(&mut argv, Some(&overrides));
    assert!(
        argv.contains(&"--unshare-all".to_string()),
        "Should include --unshare-all"
    );
    assert!(
        argv.contains(&"--share-net".to_string()),
        "Should include --share-net when override is true"
    );
}

#[test]
fn test_append_bwrap_isolation_flags_with_override_no_share_net() {
    let overrides = BwrapIsolationOverrides { share_net: false };
    let mut argv = vec![];
    append_bwrap_isolation_flags(&mut argv, Some(&overrides));
    assert!(
        argv.contains(&"--unshare-all".to_string()),
        "Should include --unshare-all"
    );
    assert!(
        !argv.contains(&"--share-net".to_string()),
        "Should NOT include --share-net when override is false"
    );
}

#[test]
fn test_append_bwrap_isolation_flags_without_override_uses_global() {
    // When overrides is None, it falls back to bwrap_share_net_enabled()
    // which reads the global SANDBOX_CONFIG or env var.
    // This test just verifies the function doesn't panic with None.
    let mut argv = vec![];
    append_bwrap_isolation_flags(&mut argv, None);
    assert!(
        argv.contains(&"--unshare-all".to_string()),
        "Should include --unshare-all"
    );
}
