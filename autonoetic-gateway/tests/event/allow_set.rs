//! #1002 slice 4 — real bubblewrap exec under `host_fs: allow_set`: the
//! sandbox sees the gateway-asserted set and nothing else of the host.
//! Host-dependent: skipped when `bwrap` is not installed (same gate as
//! `sandbox_capture`).

use std::process::Command;

const BWRAP: &str = "bwrap";

fn is_bwrap_available() -> bool {
    Command::new(BWRAP)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn probe(spec_argv: &[String], sandbox_cmd: &str) -> std::io::Result<(i32, String)> {
    // The driver's argv already ends with `-- sh -c <entrypoint>`; replace
    // entrypoint by rebuilding: driver argv terminates at '--'.
    let sep = spec_argv.iter().position(|a| a == "--").expect("sep");
    let mut full: Vec<String> = spec_argv[..sep].to_vec();
    full.extend(vec!["--".to_string(), "sh".to_string(), "-c".to_string(), sandbox_cmd.to_string()]);
    let out = Command::new(BWRAP).args(&full).output()?;
    Ok((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned()))
}



#[test]
fn allow_set_exec_sees_only_the_asserted_set() {
    if !is_bwrap_available() {
        eprintln!("bubblewrap not found, skipping test");
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let workspace_root = td.path().join("workspace");
    std::fs::create_dir_all(&workspace_root).unwrap();
    std::fs::write(workspace_root.join("hello.txt"), "hello").unwrap();
    let gw = td.path().join("runtime");
    std::fs::create_dir_all(&gw).unwrap();

    let overrides = autonoetic_gateway::sandbox::BwrapIsolationOverrides {
        share_net: false,
        force_network_off: false,
        host_fs_allow_set: true,
    };
    use autonoetic_gateway::sandbox::driver::SpawnSpec;
    let spec = SpawnSpec {
        agent_dir: workspace_root.to_str().unwrap(),
        gateway_dir: &gw,
        entrypoint: "true",
        mounts: &[],
        overrides: Some(&overrides),
        extra_env: &[],
        bridge: &Default::default(),
    };
    let (_prog, argv) = autonoetic_gateway::sandbox::driver::SandboxDriverKind::Bubblewrap
        .driver()
        .expect("bubblewrap driver")
        .build_command(&spec)
        .expect("allow-set argv");

    // Workspace is there and writable.
    let (code, out) = probe(&argv, "echo ok && cat hello.txt").unwrap();
    assert_eq!(code, 0, "workspace must work: {out}");
    assert_eq!(out.trim(), "ok\nhello");

    // Host root is NOT visible: reading /etc/passwd fails with no such file.
    let (code, _) = probe(&argv, "cat /etc/passwd 2>&1").unwrap();
    assert_ne!(code, 0, "host /etc must not be readable under allow_set");

    // Host dirs beyond the mount set are absent — the gateway dir, and the
    // operator's home (the sandbox inherits the env's HOME but not the dir).
    let (code, out) = probe(&argv, "ls \"$HOME\" 2>&1").unwrap();
    assert_ne!(code, 0, "host home must not resolve under allow_set: {out}");
    // The gateway dir's *secrets* must be absent — the SDK tree may be bound
    // (it is the PYTHONPATH source), so probe the vault key, not the dir.
    let (code, _) = probe(
        &argv,
        &format!("cat {}/vault.key 2>&1", gw.to_str().unwrap()),
    )
    .unwrap();
    assert_ne!(code, 0, "gateway secrets must not resolve under allow_set");
    let (code, _) = probe(
        &argv,
        &format!("cat {}/gateway.db 2>&1", gw.to_str().unwrap()),
    )
    .unwrap();
    assert_ne!(code, 0, "gateway.db must not resolve under allow_set");
}
