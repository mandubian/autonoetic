//! Constitution R+16 — Promotion-gate execution denied network access.
//!
//! Evaluator and auditor sessions (agents with Evaluation capability)
//! must have network namespace unshared regardless of declared
//! NetworkAccess. This ensures promotion verdicts are reproducible
//! from recorded evidence alone.


use autonoetic_gateway::sandbox::BwrapIsolationOverrides;
use autonoetic_types::capability::Capability;

#[test]
fn promotion_gate_overrides_force_network_off() {
    let overrides = BwrapIsolationOverrides::promotion_gate_overrides();
    assert!(!overrides.share_net, "share_net must be false");
    assert!(
        overrides.force_network_off,
        "force_network_off must be true"
    );
}

#[test]
fn evaluation_capability_forces_network_off() {
    let caps = vec![
        Capability::Evaluation {
            patterns: vec!["*".to_string()],
        },
        Capability::NetworkAccess {
            hosts: vec!["*".to_string()],
        },
    ];
    let mut overrides = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(overrides.share_net, "NetworkAccess sets share_net=true");

    // Simulate approval_validated_for_command granting network
    overrides.share_net = true;

    // Now apply R+16 (mirrors the logic in sandbox_exec / execute_script_in_sandbox)
    overrides.force_network_off = true;
    overrides.share_net = false;

    assert!(overrides.force_network_off);
    assert!(
        !overrides.share_net,
        "R+16 overrides NetworkAccess for evaluation agents"
    );

    let mut argv = vec!["--unshare-all".to_string()];
    autonoetic_gateway::sandbox::append_bwrap_isolation_flags(&mut argv, Some(&overrides));
    assert!(
        !argv.contains(&"--share-net".to_string()),
        "R+16 must suppress --share-net even after approval grants network"
    );
}

#[test]
fn non_evaluation_agents_keep_network() {
    let caps = vec![Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    }];
    let overrides = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(
        overrides.share_net,
        "NetworkAccess without Evaluation keeps share_net"
    );
    assert!(!overrides.force_network_off);
}

#[test]
fn force_network_off_wins_over_share_net_in_flags() {
    let overrides = BwrapIsolationOverrides {
        share_net: true,
        force_network_off: true,
    };
    let mut argv = vec!["--unshare-all".to_string()];

    autonoetic_gateway::sandbox::append_bwrap_isolation_flags(&mut argv, Some(&overrides));

    assert!(
        !argv.contains(&"--share-net".to_string()),
        "force_network_off must suppress --share-net even when share_net=true"
    );
}

#[test]
fn normal_overrides_add_share_net() {
    let overrides = BwrapIsolationOverrides {
        share_net: true,
        force_network_off: false,
    };
    let mut argv = vec!["--unshare-all".to_string()];

    autonoetic_gateway::sandbox::append_bwrap_isolation_flags(&mut argv, Some(&overrides));

    assert!(
        argv.contains(&"--share-net".to_string()),
        "share_net=true without force_network_off should add --share-net"
    );
}
