use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn autonoetic(args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_autonoetic");
    let mut cmd = Command::new(bin);
    cmd.args(args);
    cmd.envs(envs.iter().copied());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn()
        .expect("should spawn")
        .wait_with_output()
        .expect("should complete")
}

fn tmp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "autonoetic-e2e-{}",
        uuid::Uuid::new_v4().to_string()[..8].to_string()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn test_init_config_creates_valid_yaml() {
    let tmp = tmp_dir();
    let config_path = tmp.join("config.yaml");
    let out = autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "init-config",
            "--output",
            config_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        out.status.success(),
        "init-config failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(config_path.exists(), "config.yaml should be created");
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("llm_presets"),
        "config should have llm_presets"
    );
    assert!(
        content.contains("agents_dir"),
        "config should have agents_dir"
    );
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_bootstrap_creates_agents_and_aliases() {
    let tmp = tmp_dir();
    let config_path = tmp.join("config.yaml");

    let out = autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "init-config",
            "--output",
            config_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(out.status.success(), "init-config should succeed");

    let out = autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "bootstrap",
            "--config",
            config_path.to_str().unwrap(),
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "bootstrap failed: stdout={}\nstderr={}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("Bootstrap complete"),
        "should show completion"
    );
    assert!(stdout.contains("activated"), "should show activated count");

    let agents_dir = tmp.join("agents");
    assert!(agents_dir.exists(), "agents dir should exist");

    let gateway_dir = tmp.join("runtime");
    assert!(gateway_dir.exists(), "runtime dir should exist");

    let out = autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "alias",
            "list",
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "alias list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("planner.default"),
        "planner.default alias should exist: {}",
        stdout
    );
    assert!(
        stdout.contains("planner.collaborative"),
        "planner.collaborative alias should exist: {}",
        stdout
    );
    assert!(
        stdout.contains("coder.default"),
        "coder.default alias should exist: {}",
        stdout
    );
    assert!(
        stdout.contains("executor.default"),
        "executor.default alias should exist: {}",
        stdout
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_bootstrap_creates_revisions_in_gateway_store() {
    let tmp = tmp_dir();
    let config_path = tmp.join("config.yaml");

    autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "init-config",
            "--output",
            config_path.to_str().unwrap(),
        ],
        &[],
    );
    autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "bootstrap",
            "--config",
            config_path.to_str().unwrap(),
        ],
        &[],
    );

    let gateway_db = tmp.join("runtime").join("gateway.db");
    assert!(gateway_db.exists(), "gateway.db should exist");

    let conn = rusqlite::Connection::open_with_flags(
        &gateway_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("should open gateway.db");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_aliases", [], |row| row.get(0))
        .expect("query should succeed");
    assert!(
        count >= 10,
        "should have at least 10 aliases, got {}",
        count
    );

    let rev_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_revisions", [], |row| row.get(0))
        .expect("query should succeed");
    assert!(
        rev_count >= 10,
        "should have at least 10 revisions, got {}",
        rev_count
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_bootstrap_idempotent_on_second_run() {
    let tmp = tmp_dir();
    let config_path = tmp.join("config.yaml");

    autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "init-config",
            "--output",
            config_path.to_str().unwrap(),
        ],
        &[],
    );

    let out1 = autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "bootstrap",
            "--config",
            config_path.to_str().unwrap(),
        ],
        &[],
    );
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(
        stdout1.contains("installed"),
        "first bootstrap should install"
    );

    let out2 = autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "bootstrap",
            "--config",
            config_path.to_str().unwrap(),
        ],
        &[],
    );
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("skipped"),
        "second bootstrap should skip existing"
    );
    assert!(
        !stdout2.contains("installed") || stdout2.contains("0 installed"),
        "second bootstrap should not install new"
    );

    let conn = rusqlite::Connection::open_with_flags(
        &tmp.join("runtime").join("gateway.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("should open gateway.db");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_aliases", [], |row| row.get(0))
        .expect("query should succeed");
    assert!(
        count >= 10,
        "should still have at least 10 aliases after second run, got {}",
        count
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn test_gateway_starts_with_bootstrapped_agents() {
    let tmp = tmp_dir();
    let config_path = tmp.join("config.yaml");

    autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "init-config",
            "--output",
            config_path.to_str().unwrap(),
        ],
        &[],
    );
    autonoetic(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "agent",
            "bootstrap",
            "--config",
            config_path.to_str().unwrap(),
        ],
        &[],
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_autonoetic"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "gateway",
            "start",
        ])
        .env("AUTONOETIC_SHARED_SECRET", "test-secret")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("gateway should start");

    std::thread::sleep(Duration::from_secs(3));

    let _ = child.kill();
    let _ = child.wait();

    let _ = fs::remove_dir_all(&tmp);
}
