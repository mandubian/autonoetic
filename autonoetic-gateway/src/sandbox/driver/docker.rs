//! Docker driver — container-tier sandbox backend.
//!
//! Unlike bubblewrap, the container does **not** inherit the gateway process
//! env and does not see the host filesystem, so everything the in-container SDK
//! client needs (socket path, PYTHONPATH, the SDK source itself) must be passed
//! explicitly as `-v`/`-e` on the `docker run` argv.

use std::path::Path;

use super::{DriverTier, SandboxDriver, SandboxDriverKind, SpawnSpec};
use crate::sandbox::{
    resolve_python_sdk_path, BwrapIsolationOverrides, SdkBridgeWiring, CCOS_SOCKET_ENV,
    PYTHONPATH_ENV,
};

const DOCKER_IMAGE_ENV: &str = "AUTONOETIC_DOCKER_IMAGE";

/// In-container path the SDK socket is mounted at (P1). Bubblewrap exposes it
/// under its workspace dir; docker bind-mounts to a fixed path outside
/// `/workspace` so the agent_dir mount can't shadow it.
pub const DOCKER_SDK_SOCKET_PATH: &str = "/run/autonoetic-sdk.sock";

/// In-container path the Python SDK source is mounted at. (For bubblewrap the
/// host `/` is ro-bind-mounted, so the host SDK path is already visible; docker
/// images are separate, so the SDK is mounted in.)
pub const DOCKER_SDK_PYTHONPATH: &str = "/opt/autonoetic-sdk";

pub struct DockerDriver;

impl SandboxDriver for DockerDriver {
    fn kind(&self) -> SandboxDriverKind {
        SandboxDriverKind::Docker
    }

    fn names(&self) -> &'static [&'static str] {
        &["docker"]
    }

    fn tier(&self) -> DriverTier {
        DriverTier::Process
    }

    /// Always offline — [`docker_command`] hardcodes `--network none`.
    fn guarantees_network_off(&self, _overrides: &BwrapIsolationOverrides) -> bool {
        true
    }

    fn sdk_socket_path(&self, _socket_name: &str) -> Option<String> {
        Some(DOCKER_SDK_SOCKET_PATH.to_string())
    }

    fn wire_sdk_bridge(
        &self,
        host_socket: &Path,
        sandbox_socket: &str,
        wiring: &mut SdkBridgeWiring,
    ) {
        wiring.volumes.push((
            host_socket.to_string_lossy().to_string(),
            sandbox_socket.to_string(),
            false,
        ));
        wiring
            .env
            .push((CCOS_SOCKET_ENV.to_string(), sandbox_socket.to_string()));
        // The Python SDK isn't in the docker image; mount it read-only and
        // point PYTHONPATH at the mount so the in-container client resolves.
        if let Some(sdk_path) = resolve_python_sdk_path() {
            wiring
                .volumes
                .push((sdk_path, DOCKER_SDK_PYTHONPATH.to_string(), true));
            wiring
                .env
                .push((PYTHONPATH_ENV.to_string(), DOCKER_SDK_PYTHONPATH.to_string()));
        }
    }

    fn build_command(&self, spec: &SpawnSpec<'_>) -> anyhow::Result<(String, Vec<String>)> {
        // Container env does NOT inherit the gateway process env, so the socket
        // path / PYTHONPATH / extra_env must all be passed as `-e`.
        let mut env = spec.bridge.env.clone();
        merge_docker_env(&mut env, spec.extra_env);
        docker_command(
            spec.agent_dir,
            spec.entrypoint,
            &spec.bridge.volumes,
            &env,
        )
    }

    // `apply_child_env` stays empty: docker bakes its env into the argv above.
}

/// Build the `docker run` invocation. `volumes` are extra `(host, container,
/// readonly)` bind mounts (e.g. the SDK socket + SDK source); `env` are vars
/// passed via `-e` (the container does not inherit the gateway process env, so
/// the SDK socket path / PYTHONPATH must be passed explicitly here).
fn docker_command(
    agent_dir: &str,
    entrypoint: &str,
    volumes: &[(String, String, bool)],
    env: &[(String, String)],
) -> anyhow::Result<(String, Vec<String>)> {
    let image = std::env::var(DOCKER_IMAGE_ENV).map_err(|_| {
        anyhow::anyhow!("Missing required environment variable {}", DOCKER_IMAGE_ENV)
    })?;
    let mut argv = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--network".to_string(),
        "none".to_string(),
        "--volume".to_string(),
        format!("{}:/workspace", agent_dir),
        "--workdir".to_string(),
        "/workspace".to_string(),
    ];
    for (host, container, readonly) in volumes {
        argv.push("--volume".to_string());
        argv.push(if *readonly {
            format!("{}:{}:ro", host, container)
        } else {
            format!("{}:{}", host, container)
        });
    }
    for (key, value) in env {
        argv.push("--env".to_string());
        argv.push(format!("{}={}", key, value));
    }
    argv.push(image);
    argv.push("sh".to_string());
    argv.push("-c".to_string()); // Non-login shell - don't source /etc/profile.d/*
    argv.push(entrypoint.to_string());
    Ok(("docker".to_string(), argv))
}

/// Merge `extra_env` into a docker env list, concatenating `PYTHONPATH` (the SDK
/// path must stay on the path) rather than overwriting it.
fn merge_docker_env(base: &mut Vec<(String, String)>, extra_env: &[(String, String)]) {
    for (key, value) in extra_env {
        if key == PYTHONPATH_ENV {
            if let Some(existing) = base.iter_mut().find(|(k, _)| k == PYTHONPATH_ENV) {
                existing.1 = format!("{}:{}", value, existing.1);
                continue;
            }
        }
        base.push((key.clone(), value.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial] // mutates AUTONOETIC_DOCKER_IMAGE (process-global)
    fn test_docker_command_requires_env() {
        let old = std::env::var(DOCKER_IMAGE_ENV).ok();
        std::env::remove_var(DOCKER_IMAGE_ENV);
        let err = docker_command("/tmp/agent", "python main.py", &[], &[])
            .expect_err("docker command should fail without env");
        assert!(
            err.to_string().contains(DOCKER_IMAGE_ENV),
            "error should mention missing docker env"
        );
        if let Some(v) = old {
            std::env::set_var(DOCKER_IMAGE_ENV, v);
        }
    }

    #[test]
    #[serial] // mutates AUTONOETIC_DOCKER_IMAGE (process-global)
    fn test_docker_command_emits_socket_volume_and_env() {
        // P1: the SDK socket + its env must reach the container via `-v`/`-e`
        // (the container does not inherit the gateway process env).
        let old = std::env::var(DOCKER_IMAGE_ENV).ok();
        std::env::set_var(DOCKER_IMAGE_ENV, "test-image:latest");
        let volumes = vec![
            (
                "/tmp/autonoetic-abc.sock".to_string(),
                DOCKER_SDK_SOCKET_PATH.to_string(),
                false,
            ),
            (
                "/host/sdk".to_string(),
                DOCKER_SDK_PYTHONPATH.to_string(),
                true,
            ),
        ];
        let env = vec![
            (
                CCOS_SOCKET_ENV.to_string(),
                DOCKER_SDK_SOCKET_PATH.to_string(),
            ),
            (PYTHONPATH_ENV.to_string(), DOCKER_SDK_PYTHONPATH.to_string()),
        ];
        let (program, argv) =
            docker_command("/tmp/agent", "python main.py", &volumes, &env).expect("docker command");
        assert_eq!(program, "docker");
        let joined = argv.join(" ");
        // socket mounted read-write, SDK source read-only
        assert!(joined.contains(&format!("/tmp/autonoetic-abc.sock:{}", DOCKER_SDK_SOCKET_PATH)));
        assert!(joined.contains(&format!("/host/sdk:{}:ro", DOCKER_SDK_PYTHONPATH)));
        // env passed via -e, not inherited
        assert!(joined.contains(&format!("{}={}", CCOS_SOCKET_ENV, DOCKER_SDK_SOCKET_PATH)));
        assert!(joined.contains(&format!("{}={}", PYTHONPATH_ENV, DOCKER_SDK_PYTHONPATH)));
        // image + shell entrypoint preserved, after the flags
        assert!(argv.contains(&"test-image:latest".to_string()));
        assert_eq!(argv.last().unwrap(), "python main.py");
        match old {
            Some(v) => std::env::set_var(DOCKER_IMAGE_ENV, v),
            None => std::env::remove_var(DOCKER_IMAGE_ENV),
        }
    }

    #[test]
    fn test_merge_docker_env_concatenates_pythonpath() {
        let mut base = vec![(PYTHONPATH_ENV.to_string(), "/opt/autonoetic-sdk".to_string())];
        merge_docker_env(
            &mut base,
            &[
                (PYTHONPATH_ENV.to_string(), "/extra".to_string()),
                ("FOO".to_string(), "bar".to_string()),
            ],
        );
        let pp = base.iter().find(|(k, _)| k == PYTHONPATH_ENV).unwrap();
        assert_eq!(pp.1, "/extra:/opt/autonoetic-sdk");
        assert!(base.iter().any(|(k, v)| k == "FOO" && v == "bar"));
    }

    /// The driver must fold `extra_env` into the bridge env rather than dropping
    /// it — the container inherits nothing, so anything not on the argv is lost.
    #[test]
    #[serial] // mutates AUTONOETIC_DOCKER_IMAGE (process-global)
    fn driver_build_command_bakes_extra_env_into_argv() {
        let old = std::env::var(DOCKER_IMAGE_ENV).ok();
        std::env::set_var(DOCKER_IMAGE_ENV, "test-image:latest");
        let mut wiring = SdkBridgeWiring::default();
        wiring.env.push((
            PYTHONPATH_ENV.to_string(),
            DOCKER_SDK_PYTHONPATH.to_string(),
        ));
        let extra_env = vec![
            ("FOO".to_string(), "bar".to_string()),
            (PYTHONPATH_ENV.to_string(), "/layer".to_string()),
        ];
        let (_program, argv) = DockerDriver
            .build_command(&SpawnSpec {
                agent_dir: "/tmp/agent",
                entrypoint: "python main.py",
                mounts: &[],
                overrides: None,
                extra_env: &extra_env,
                bridge: &wiring,
            })
            .expect("driver should build the command");
        let joined = argv.join(" ");
        assert!(joined.contains("FOO=bar"), "got: {joined}");
        // PYTHONPATH is concatenated, not overwritten.
        assert!(
            joined.contains(&format!("{}=/layer:{}", PYTHONPATH_ENV, DOCKER_SDK_PYTHONPATH)),
            "got: {joined}"
        );
        match old {
            Some(v) => std::env::set_var(DOCKER_IMAGE_ENV, v),
            None => std::env::remove_var(DOCKER_IMAGE_ENV),
        }
    }
}
