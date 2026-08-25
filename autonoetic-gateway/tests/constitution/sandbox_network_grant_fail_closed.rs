//! #1022 — `sandbox_exec` network reachability is granted per exec, never
//! inherited from the `NetworkAccess` capability.
//!
//! `sandbox_exec` gates network on operator approval: a detected signal raises a
//! gate, and only a cleared gate (or `approval_ref` / declared preapproval /
//! approved-exec cache hit) lets the exec proceed with the network. The baseline
//! for `share_net`, however, used to be seeded from the capability ceiling
//! (`BwrapIsolationOverrides::from_capabilities`), and the gate could only
//! *widen* it. An exec that raised no gate therefore kept the ceiling's value —
//! so for an agent holding `NetworkAccess { hosts: [...] }` in a session whose
//! taint still allows `Sink::Network`, "static analysis found nothing" meant
//! "namespace-wide `--share-net`, no operator prompt".
//!
//! These tests pin each link of that chain, then pin it closed.
//!
//! Sibling exec paths are deliberately NOT covered here: `artifact_exec` and
//! script-mode execution are capability-driven *by design* (artifact_exec
//! auto-approves on capability presence). See `docs/internals/sandbox/network-grant.md`.

use autonoetic_gateway::runtime::egress_labeler::require_boundary_session_taint;
use autonoetic_gateway::runtime::network_grant::{
    decide_share_net, ShareNetInputs, ShareNetReason,
};
use autonoetic_gateway::runtime::remote_access::RemoteAccessAnalyzer;
use autonoetic_gateway::sandbox::{append_bwrap_isolation_flags, BwrapIsolationOverrides};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::capability::Capability;
use autonoetic_types::egress::Sink;

/// A network-capable agent's capability ceiling.
fn network_capable() -> Vec<Capability> {
    vec![Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    }]
}

fn ceiling_allows_network(caps: &[Capability]) -> bool {
    caps.iter()
        .any(|c| matches!(c, Capability::NetworkAccess { hosts } if !hosts.is_empty()))
}

/// Link 1 of the chain: a session with no recorded taint resolves to
/// `unrestricted`, which allows `Sink::Network`. So `network_sink_excluded` is
/// false by default and the taint layer does not force the network off — the
/// egress layer only closes this window for sessions already tainted away from
/// Network.
#[test]
fn fresh_session_taint_allows_the_network_sink() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let store = GatewayStore::open(tmp.path())?;

    let taint = require_boundary_session_taint(None, Some(&store), Some("sess-untainted"))?;
    assert!(
        taint.allows(Sink::Network),
        "an untainted session resolves to unrestricted, so `network_sink_excluded` is false \
         and the taint layer cannot be what keeps the network off"
    );
    Ok(())
}

/// Link 2: ordinary, non-obfuscated code that performs real network egress can
/// be entirely invisible to static analysis, so no gate is raised and the
/// operator is never asked. `aws s3 cp` through `subprocess` is the mundane case
/// — no URL literal, no socket primitive, no listed import, no recognised
/// network command.
#[test]
fn real_network_egress_can_be_invisible_to_static_analysis() {
    let aws_cli = r#"
import subprocess
subprocess.run(["aws", "s3", "cp", "s3://bucket/key", "/tmp/key"])
"#;
    let analysis = RemoteAccessAnalyzer::analyze_code(aws_cli);
    assert!(
        !analysis.requires_approval,
        "expected the aws-cli egress to be invisible to the analyzer; if this now fires the \
         detector improved — keep the fail-closed assertions below, they do not depend on it. \
         Detected: {:?}",
        analysis.detected_patterns
    );

    let dynamic_import = r#"
import os
mod = __import__(os.environ["M"])
print(mod.request("GET", os.environ["U"]).text)
"#;
    let analysis = RemoteAccessAnalyzer::analyze_code(dynamic_import);
    assert!(
        !analysis.requires_approval,
        "dynamic import with a host from the environment leaves no regex signal. Detected: {:?}",
        analysis.detected_patterns
    );
}

/// The regression pin. Links 1 + 2 composed with the real decision: a
/// network-capable agent, an untainted session, zero detected signals — hence no
/// gate and no grant — must execute with the network namespace unshared. The
/// capability must not leak into the exec's reachability.
#[test]
fn network_capable_agent_without_a_grant_gets_no_share_net() {
    let caps = network_capable();
    let decision = decide_share_net(ShareNetInputs {
        capability_allows_network: ceiling_allows_network(&caps),
        approval_validated: false,
        safe_inspection_bypass: false,
        // Untainted session (link 1) — the taint layer is not what closes this.
        network_sink_excluded: false,
        network_declassified: false,
        force_network_off: false,
    });

    assert!(
        !decision.share_net,
        "an exec with no per-exec grant must not inherit the network namespace from the \
         NetworkAccess capability (#1022)"
    );
    assert_eq!(decision.reason, ShareNetReason::NoGrant);
    assert!(
        decision.capability_ceiling_unused,
        "the unused ceiling must be reported so the resulting connection failure is \
         attributable to a missing grant rather than looking like a broken sandbox"
    );

    // The mechanical end of the chain: no `--share-net` reaches bubblewrap.
    let overrides = BwrapIsolationOverrides {
        share_net: decision.share_net,
        force_network_off: false,
host_fs_allow_set: false,
    };
    let mut argv: Vec<String> = Vec::new();
    append_bwrap_isolation_flags(&mut argv, Some(&overrides));
    assert!(
        argv.contains(&"--unshare-all".to_string()),
        "argv: {argv:?}"
    );
    assert!(
        !argv.contains(&"--share-net".to_string()),
        "ungranted exec must run with the network namespace unshared; argv: {argv:?}"
    );
}

/// The counterpart: an approved exec still reaches the network. Closing the
/// window must not break the path the gate exists to serve.
#[test]
fn approved_exec_still_reaches_the_network() {
    let caps = network_capable();
    let decision = decide_share_net(ShareNetInputs {
        capability_allows_network: ceiling_allows_network(&caps),
        approval_validated: true,
        safe_inspection_bypass: false,
        network_sink_excluded: false,
        network_declassified: false,
        force_network_off: false,
    });

    assert!(decision.share_net);
    assert_eq!(decision.reason, ShareNetReason::GrantedByApproval);
    assert!(!decision.capability_ceiling_unused);

    let overrides = BwrapIsolationOverrides {
        share_net: decision.share_net,
        force_network_off: false,
host_fs_allow_set: false,
    };
    let mut argv: Vec<String> = Vec::new();
    append_bwrap_isolation_flags(&mut argv, Some(&overrides));
    assert!(
        argv.contains(&"--share-net".to_string()),
        "an approved exec must still get the network namespace; argv: {argv:?}"
    );
}

/// `from_capabilities` keeps its ceiling semantics for the paths that are
/// legitimately capability-driven (script mode, artifact_exec). This test pins
/// that the ceiling and the per-exec grant are now *different values* — the
/// distinction the fix rests on — so a future refactor cannot quietly re-seed
/// the `sandbox_exec` baseline from the ceiling again.
#[test]
fn capability_ceiling_and_per_exec_grant_are_distinct() {
    let caps = network_capable();

    let ceiling = BwrapIsolationOverrides::from_capabilities(&caps);
    assert!(
        ceiling.share_net,
        "from_capabilities still reports the ceiling for capability-driven exec paths"
    );

    let grant = decide_share_net(ShareNetInputs {
        capability_allows_network: ceiling_allows_network(&caps),
        approval_validated: false,
        safe_inspection_bypass: false,
        network_sink_excluded: false,
        network_declassified: false,
        force_network_off: false,
    });
    assert!(
        !grant.share_net,
        "the same capabilities must yield no network for an ungranted sandbox_exec"
    );
}
