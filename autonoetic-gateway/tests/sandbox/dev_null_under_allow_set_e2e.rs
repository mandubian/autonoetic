//! Full-sandbox e2e: `/dev/null` must exist under the default flag pair.
//!
//! `#[ignore]`d because it requires a working **bubblewrap** (`bwrap`) on the
//! host and actually spawns a shell inside the sandbox — NOT run in CI. The
//! CI-safe coverage is `allow_set_provides_dev_even_when_dev_mode_is_legacy`
//! in `sandbox::driver::bubblewrap`, which asserts the *argv*. This file proves
//! what argv assertions cannot: that the rendered flags really do yield a
//! usable `/dev/null`, and that without them an ordinary shell redirection
//! dies.
//!
//! Run locally with:
//! ```bash
//! cargo nextest run -p autonoetic-gateway --test sandbox dev_null_under_allow_set \
//!   --run-ignored ignored-only --nocapture
//! ```
//!
//! The defect (session-eb6abde5): `dev_mode: legacy` means "emit no /dev
//! override", which was safe only while `host_fs: legacy` ro-bound the host `/`
//! and carried /dev in with it. Under `host_fs: allow_set` — now the default —
//! the root is a bare `--tmpfs /` plus an explicit bind list with no /dev, so
//! every `2>/dev/null` in a dependency-install script died with
//! `sh: cannot create /dev/null: Directory nonexistent`.

use autonoetic_gateway::sandbox::{append_bwrap_isolation_flags, BwrapIsolationOverrides};

/// Exercises the two things a real install script does with /dev/null: redirect
/// into it, and read from it. Prints a sentinel so a shell that never started
/// is distinguishable from one that ran and failed.
const DEV_PROBE: &str = r#"echo probe >/dev/null 2>&1 && cat /dev/null && echo DEV_NULL_OK"#;

fn allow_set_overrides() -> BwrapIsolationOverrides {
    BwrapIsolationOverrides {
        share_net: false,
        force_network_off: true,
        host_fs_allow_set: true,
    }
}

/// The filesystem shape `host_fs: allow_set` produces: an empty tmpfs root plus
/// an explicit read-only bind list. Deliberately does NOT include `/dev` — that
/// is the thing under test, and it must come from the isolation flags.
fn allow_set_root_argv() -> Vec<String> {
    let mut argv = vec!["--tmpfs".to_string(), "/".to_string()];
    for root in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"] {
        if std::path::Path::new(root).exists() {
            argv.extend(
                ["--ro-bind", root, root]
                    .iter()
                    .map(ToString::to_string),
            );
        }
    }
    argv
}

fn run_probe(argv: Vec<String>) -> (bool, String, String) {
    let mut full = argv;
    full.extend(
        ["/bin/sh", "-c", DEV_PROBE]
            .iter()
            .map(ToString::to_string),
    );
    let out = std::process::Command::new("bwrap")
        .args(&full)
        .output()
        .expect("bwrap must be installed to run this ignored e2e");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

#[test]
#[ignore = "requires host bwrap and spawns a shell in the sandbox"]
fn allow_set_sandbox_has_a_usable_dev_null() {
    let mut argv = allow_set_root_argv();
    append_bwrap_isolation_flags(&mut argv, Some(&allow_set_overrides()));

    let (ok, stdout, stderr) = run_probe(argv);
    assert!(
        ok && stdout.contains("DEV_NULL_OK"),
        "redirecting to /dev/null must work under the default flag pair \
         (host_fs: allow_set + dev_mode: legacy). stdout={stdout:?} stderr={stderr:?}"
    );
}

/// The same sandbox with the /dev provision removed — the pre-fix shape. This
/// is what makes the test above meaningful: it fails exactly the way the real
/// packager did, so the guard has been shown to catch its defect.
#[test]
#[ignore = "requires host bwrap and spawns a shell in the sandbox"]
fn without_the_dev_provision_the_same_redirection_fails() {
    let mut argv = allow_set_root_argv();
    argv.push("--unshare-all".to_string());

    let (ok, stdout, stderr) = run_probe(argv);
    assert!(
        !ok && !stdout.contains("DEV_NULL_OK"),
        "a root with no /dev must not satisfy the probe — if this passes, the \
         test is no longer exercising the defect. stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("/dev/null"),
        "the failure must be about /dev/null, not some unrelated setup error: \
         stderr={stderr:?}"
    );
}
