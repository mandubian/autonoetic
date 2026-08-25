//! Full-sandbox e2e for the #1022 per-exec network grant.
//!
//! These tests are `#[ignore]`d because they require a working **bubblewrap**
//! (`bwrap`) on the host and actually open a socket from inside the sandbox —
//! they are NOT run in CI. The CI-safe decision coverage lives in
//! `tests/constitution/sandbox_network_grant_fail_closed.rs`; this file proves
//! the runtime property CI cannot: that the decision's two outcomes really are
//! the difference between "can open a connection" and "cannot".
//!
//! Run locally with:
//! ```bash
//! cargo test -p autonoetic-gateway --test sandbox_network_grant_bwrap_e2e -- --ignored --nocapture
//! ```
//!
//! What they prove: a `NetworkAccess`-capable agent whose exec has **no grant**
//! (the zero-signal case — nothing for the analyzer to raise a gate about) runs
//! in a sandbox where even a raw `socket.create_connection` fails. Before #1022
//! that exec inherited `--share-net` from the capability and the same connection
//! succeeded with no operator prompt.

use autonoetic_gateway::runtime::network_grant::{
    decide_share_net, ShareNetInputs, ShareNetReason,
};
use autonoetic_gateway::sandbox::{append_bwrap_isolation_flags, BwrapIsolationOverrides};
use autonoetic_types::capability::Capability;

/// Opens a TCP connection to a public resolver and reports the outcome. Chosen
/// over an HTTP client so the test exercises the network *namespace*, not DNS or
/// any library's proxy handling.
const NET_PROBE: &str = r#"
import socket
try:
    socket.create_connection(("1.1.1.1", 53), timeout=4).close()
    print("NETWORK_REACHABLE")
except Exception as e:
    print("NETWORK_BLOCKED:" + type(e).__name__)
"#;

fn network_capable() -> Vec<Capability> {
    vec![Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    }]
}

/// Run `NET_PROBE` under bwrap with isolation flags rendered from `overrides`,
/// and return the probe's stdout.
fn probe_under_bwrap(overrides: &BwrapIsolationOverrides) -> String {
    let mut argv: Vec<String> = Vec::new();
    append_bwrap_isolation_flags(&mut argv, Some(overrides));
    // Minimal filesystem for a python3 run; orthogonal to the network decision.
    argv.extend(
        ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
            .iter()
            .map(ToString::to_string),
    );
    argv.extend(["python3", "-c", NET_PROBE].iter().map(ToString::to_string));

    let out = std::process::Command::new("bwrap")
        .args(&argv)
        .output()
        .expect("bwrap must be installed to run this ignored e2e");
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        stdout.starts_with("NETWORK_"),
        "probe did not run; argv={argv:?} stdout={stdout:?} stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[test]
#[ignore = "requires host bwrap and opens a real socket"]
fn ungranted_network_capable_exec_cannot_open_a_connection() {
    let caps = network_capable();
    let decision = decide_share_net(ShareNetInputs {
        capability_allows_network: caps
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { hosts } if !hosts.is_empty())),
        // The zero-signal case: nothing detected, so no gate was raised and
        // nothing cleared one.
        approval_validated: false,
        safe_inspection_bypass: false,
        network_sink_excluded: false,
        network_declassified: false,
        force_network_off: false,
    });
    assert_eq!(decision.reason, ShareNetReason::NoGrant);
    assert!(decision.capability_ceiling_unused);

    let overrides = BwrapIsolationOverrides {
        share_net: decision.share_net,
        force_network_off: false,
host_fs_allow_set: false,
    };
    let stdout = probe_under_bwrap(&overrides);
    assert!(
        stdout.starts_with("NETWORK_BLOCKED"),
        "a network-capable agent's ungranted exec must not reach the network; got {stdout:?}"
    );
}

#[test]
#[ignore = "requires host bwrap and opens a real socket"]
fn granted_exec_can_open_a_connection() {
    let caps = network_capable();
    let decision = decide_share_net(ShareNetInputs {
        capability_allows_network: caps
            .iter()
            .any(|c| matches!(c, Capability::NetworkAccess { hosts } if !hosts.is_empty())),
        approval_validated: true,
        safe_inspection_bypass: false,
        network_sink_excluded: false,
        network_declassified: false,
        force_network_off: false,
    });
    assert_eq!(decision.reason, ShareNetReason::GrantedByApproval);

    let overrides = BwrapIsolationOverrides {
        share_net: decision.share_net,
        force_network_off: false,
host_fs_allow_set: false,
    };
    let stdout = probe_under_bwrap(&overrides);
    assert!(
        stdout.starts_with("NETWORK_REACHABLE"),
        "an approved exec must still reach the network — otherwise the fail-closed change \
         broke the path the gate exists to serve; got {stdout:?}"
    );
}
