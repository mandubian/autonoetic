//! Sandbox runner supporting bubblewrap, docker, and firecracker.

use crate::exec_request::ExecutionKind;
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::EntryStatus;
use autonoetic_types::config::SandboxConfig;
use sha2::{Digest, Sha256};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
};

pub const SDK_BRIDGE_RATE_LIMIT_PER_SEC: u32 = 100;
pub const SDK_BRIDGE_MAX_PAYLOAD_BYTES: usize = 1_048_576;
/// Reuse SDK bridge sockets across sequential `sandbox_exec` calls for the same
/// `(agent_dir, root_session_id)` within this idle window.
const SDK_BRIDGE_IDLE_TTL: Duration = Duration::from_secs(300);

const DOCKER_IMAGE_ENV: &str = "AUTONOETIC_DOCKER_IMAGE";
const FIRECRACKER_CONFIG_ENV: &str = "AUTONOETIC_FIRECRACKER_CONFIG";
pub(crate) const BWRAP_WORKSPACE_DIR: &str = "/tmp";
/// In-container path the SDK socket is mounted at for the docker driver (P1).
/// Bubblewrap exposes it under `BWRAP_WORKSPACE_DIR`; docker bind-mounts to a
/// fixed path outside `/workspace` so the agent_dir mount can't shadow it.
const DOCKER_SDK_SOCKET_PATH: &str = "/run/autonoetic-sdk.sock";
/// In-container path the Python SDK source is mounted at for the docker driver.
/// (For bubblewrap the host `/` is ro-bind-mounted, so the host SDK path is
/// already visible; docker images are separate, so the SDK is mounted in.)
const DOCKER_SDK_PYTHONPATH: &str = "/opt/autonoetic-sdk";
const PYTHONPATH_ENV: &str = "PYTHONPATH";
const PYTHON_SDK_PATH_ENV: &str = "AUTONOETIC_PYTHON_SDK_PATH";
const CCOS_SOCKET_ENV: &str = "CCOS_SOCKET_PATH";
const BWRAP_SHARE_NET_ENV: &str = "AUTONOETIC_BWRAP_SHARE_NET";
const BWRAP_DEV_MODE_ENV: &str = "AUTONOETIC_BWRAP_DEV_MODE";
const ALLOW_SANDBOX_ENV_OVERRIDES_ENV: &str = "AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES";

static SANDBOX_CONFIG: OnceLock<SandboxConfig> = OnceLock::new();
static SDK_DEPLOYED_PATH: OnceLock<String> = OnceLock::new();
/// Additional host paths to mask inside every bubblewrap sandbox. Stopgap for
/// #1002: the host `/` is ro-bind-mounted, so without masking a sandboxed
/// process can read gateway-internal files. The gateway directory's sensitive
/// contents (vault key, session DB, identity key, sessions/, …) are ALWAYS
/// masked — derived per-spawn from the agent dir (see [`bwrap_deny_path_flags`]).
/// This list is for the operator config file and any other paths the operator
/// chooses to add. Populated once at startup via [`init_sandbox_host_deny_paths`].
static SANDBOX_HOST_DENY_PATHS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();

/// Per-execution overrides for bubblewrap isolation flags.
/// Derived from the executing agent's capabilities.
#[derive(Debug, Clone, Default)]
pub struct BwrapIsolationOverrides {
    pub share_net: bool,
    pub force_network_off: bool,
}

impl BwrapIsolationOverrides {
    /// Isolation flags derived from the agent's capability **ceiling**: the
    /// resulting `share_net` says "this agent is permitted network", not "this
    /// execution was granted network".
    ///
    /// Valid only for exec paths whose network policy *is* capability-driven —
    /// script-mode agents (a fixed entrypoint reviewed once at install, with
    /// `revision.detected_network_hosts` covered by the declared
    /// `NetworkAccess.hosts` per P-1.5) and `artifact_exec` (which auto-approves
    /// on capability presence by design).
    ///
    /// Do **not** use this to seed the baseline for an exec path that gates
    /// network on operator approval: agent-supplied code (`sandbox_exec`) must
    /// decide per exec via [`crate::runtime::network_grant::decide_share_net`],
    /// because a ceiling-seeded baseline silently survives whenever no gate is
    /// raised — the #1022 window. See `docs/sandbox-network-grant.md`.
    pub fn from_capabilities(caps: &[Capability]) -> Self {
        let share_net = caps
            .iter()
            .any(|cap| matches!(cap, Capability::NetworkAccess { hosts } if !hosts.is_empty()));
        Self {
            share_net,
            force_network_off: false,
        }
    }

    pub fn promotion_gate_overrides() -> Self {
        Self {
            share_net: false,
            force_network_off: true,
        }
    }
}

pub struct SdkBridgeRateLimiter {
    calls_this_second: AtomicU32,
    second_start_epoch: AtomicU64,
    rate_limit: u32,
    max_payload_bytes: usize,
}

impl SdkBridgeRateLimiter {
    pub fn new(rate_limit: u32, max_payload_bytes: usize) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            calls_this_second: AtomicU32::new(0),
            second_start_epoch: AtomicU64::new(now),
            rate_limit,
            max_payload_bytes,
        }
    }

    pub fn check_rate(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.check_rate_at(now)
    }

    pub fn check_rate_at(&self, now_secs: u64) -> bool {
        let current_window = self.second_start_epoch.load(Ordering::Relaxed);
        if now_secs != current_window {
            self.second_start_epoch.store(now_secs, Ordering::Relaxed);
            self.calls_this_second.store(1, Ordering::Relaxed);
            return true;
        }
        let count = self.calls_this_second.fetch_add(1, Ordering::Relaxed);
        count < self.rate_limit
    }

    pub fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    pub fn rate_limit(&self) -> u32 {
        self.rate_limit
    }
}

/// Initialize sandbox config from gateway config. Call once at startup.
/// Config values are authoritative. Env overrides are ignored unless
/// AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true.
pub fn init_sandbox_config(config: &SandboxConfig) {
    SANDBOX_CONFIG.get_or_init(|| config.clone());
}

/// Initialise the deployed-SDK path from the gateway directory so sandbox
/// runners can find the SDK without relying on the source tree.
pub fn init_sdk_deployed_path(gateway_dir: &Path) {
    let py_sdk = gateway_dir.join("sdk").join("python");
    if py_sdk.exists() {
        let _ = SDK_DEPLOYED_PATH.set(py_sdk.to_string_lossy().to_string());
    }
}

/// Register additional host paths to mask (hide) inside every bubblewrap
/// sandbox. Stopgap for #1002: the default bubblewrap driver ro-binds the
/// whole host `/`, so a sandboxed agent with code-execution could otherwise
/// read the gateway config (provider/endpoint config, `continuation_key`),
/// sibling agents' state, or any operator file. The gateway directory's own
/// sensitive files are masked unconditionally (derived per-spawn); this list
/// is for the config file and operator-specified paths. Non-existent paths are
/// silently skipped at mount-build time. Idempotent — first call wins.
///
/// Paths are normalized via [`normalize_deny_paths`] before storage: relative
/// paths are made absolute (bwrap dests are namespace-absolute against the
/// ro-mounted `/`, so a relative path would silently fail to mask), and
/// symlinked targets are added alongside the link path so a config reachable
/// via its real path can't escape masking.
pub fn init_sandbox_host_deny_paths(paths: Vec<PathBuf>) {
    let _ = SANDBOX_HOST_DENY_PATHS.set(normalize_deny_paths(&paths));
}

/// Normalize a set of deny paths for use as bwrap mount destinations:
/// - **Make absolute** — resolve relative paths against the gateway CWD. bwrap
///   interprets bind destinations against the sandbox namespace (rooted at the
///   ro-mounted host `/`), so a relative path would not mask the real file.
/// - **Add canonical targets** — if a path is a symlink, `canonicalize` it and
///   include the real target so the file can't be read via its underlying path
///   (e.g. `~/.autonoetic/config.yaml` → `/etc/autonoetic/config.yaml`).
/// - **Dedup** — identical absolute/canonical forms collapse to one entry.
///
/// Existence is *not* required here — the mount-builder
/// ([`push_deny_file`]/[`push_deny_dir`]) skips non-existent paths at spawn
/// time. A path that doesn't resolve canonically (missing now, created later)
/// still contributes its absolute form for the common case.
fn normalize_deny_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        let abs = if p.is_absolute() {
            p.clone()
        } else {
            cwd.join(p)
        };
        // Cover symlinked configs: mask the real target too, so the file can't
        // be read via its canonical path around the link.
        let canon = std::fs::canonicalize(&abs).ok();
        if !out.contains(&abs) {
            out.push(abs);
        }
        if let Some(canon) = canon {
            if !out.contains(&canon) {
                out.push(canon);
            }
        }
    }
    out
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct SdkBridgeCacheKey {
    agent_dir: String,
    root_session_id: Option<String>,
}

struct SdkBridgeShared {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
    socket_path_host: PathBuf,
    socket_name: String,
    ref_count: AtomicUsize,
    idle_deadline: Mutex<Option<Instant>>,
}

impl SdkBridgeShared {
    fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket_path_host);
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
        let _ = fs::remove_file(&self.socket_path_host);
    }
}

struct SdkBridgeGuard {
    shared: Arc<SdkBridgeShared>,
}

impl Drop for SdkBridgeGuard {
    fn drop(&mut self) {
        let prev = self.shared.ref_count.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            if let Ok(mut deadline) = self.shared.idle_deadline.lock() {
                *deadline = Some(Instant::now() + SDK_BRIDGE_IDLE_TTL);
            }
        }
    }
}

static SDK_BRIDGE_CACHE: LazyLock<Mutex<HashMap<SdkBridgeCacheKey, Arc<SdkBridgeShared>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn purge_idle_sdk_bridges(cache: &mut HashMap<SdkBridgeCacheKey, Arc<SdkBridgeShared>>) {
    let now = Instant::now();
    cache.retain(|_, shared| {
        if shared.ref_count.load(Ordering::SeqCst) > 0 {
            return true;
        }
        let expired = shared
            .idle_deadline
            .lock()
            .ok()
            .and_then(|d| *d)
            .map(|deadline| now >= deadline)
            .unwrap_or(true);
        if expired {
            shared.shutdown();
            false
        } else {
            true
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDriverKind {
    Bubblewrap,
    Docker,
    MicroVm,
    /// In-process WebAssembly (WASI) tier — the portable execution backend
    /// (RFC `docs/rfc/portable-wasm-execution-tier.md`, P4). Selected via
    /// `sandbox: "wasm"`; runs declared modules in-process through
    /// [`SandboxRunner::run_to_output`] when built with the `wasm-tier` feature
    /// (without it, selecting this driver returns a clear build-feature error).
    Wasm,
}

impl SandboxDriverKind {
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "bubblewrap" | "bwrap" => Ok(Self::Bubblewrap),
            "docker" => Ok(Self::Docker),
            "microvm" | "firecracker" => Ok(Self::MicroVm),
            "wasm" | "wasi" => Ok(Self::Wasm),
            other => anyhow::bail!("Unsupported sandbox driver '{}'", other),
        }
    }

    /// Whether this driver guarantees the run has **no network access** under the
    /// given isolation overrides. Single source of truth for "is this execution
    /// physically offline" — used by the promotion gate to decide whether a
    /// deterministic test suite can be trusted to run in isolation (P-3.10)
    /// instead of being statically pre-denied on mere import detection.
    ///
    /// - **Bubblewrap**: offline iff `force_network_off` (the gate sets it via
    ///   [`BwrapIsolationOverrides::promotion_gate_overrides`]); enforced by
    ///   `--unshare-all` with no `--share-net`.
    /// - **Docker**: always offline — `docker_command` hardcodes `--network none`.
    /// - **Wasm**: always offline — the in-process WASI preview1 tier exposes no
    ///   sockets (only a preopened workspace dir, args, env).
    /// - **MicroVm**: NOT guaranteed — network is whatever the operator's
    ///   firecracker `--config-file` declares; the gateway passes only that file
    ///   and cannot assert the absence of a NIC. Conservative `false`.
    pub fn guarantees_network_off(self, overrides: &BwrapIsolationOverrides) -> bool {
        match self {
            Self::Bubblewrap => overrides.force_network_off,
            Self::Docker => true,
            Self::Wasm => true,
            Self::MicroVm => false,
        }
    }
}

/// Dependency runtime ecosystem used to install generated code dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyRuntime {
    Python,
    NodeJs,
}

/// Thin dependency-install plan applied inside sandbox workspace before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyPlan {
    pub runtime: DependencyRuntime,
    pub packages: Vec<String>,
}

pub struct SandboxRunner {
    pub process: Child,
    pub driver: SandboxDriverKind,
    _sdk_bridge: Option<SdkBridgeGuard>,
}

/// Captured result of a completed sandbox execution, tier-agnostic.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl SandboxRunner {
    /// Run a request to completion and capture its output, dispatching by tier:
    /// the **process** drivers (bubblewrap/docker/microvm) spawn a child and
    /// wait; the **wasm** tier runs in-process via the WASI backend. This is the
    /// unified entry the agent execution path migrates onto (P4 inc 2c); the
    /// `spawn_*` methods remain the process-only path used today.
    pub fn run_to_output(
        driver: SandboxDriverKind,
        agent_dir: &str,
        request: &ExecutionKind,
        dependencies: Option<&DependencyPlan>,
        overrides: Option<&BwrapIsolationOverrides>,
        extra_env: &[(String, String)],
        root_session_id: Option<&str>,
        stdin: Option<Vec<u8>>,
    ) -> anyhow::Result<ExecOutput> {
        if driver == SandboxDriverKind::Wasm {
            return run_wasm_request(agent_dir, request, extra_env, stdin);
        }
        let mut runner = Self::spawn_with_driver_and_dependencies_and_env(
            driver,
            agent_dir,
            request,
            dependencies,
            overrides,
            extra_env,
            root_session_id,
        )?;
        // Feed stdin (closed by dropping the handle) before draining the child.
        if let Some(input) = stdin {
            if let Some(mut child_stdin) = runner.process.stdin.take() {
                use std::io::Write;
                child_stdin.write_all(&input)?;
            }
        }
        let out = runner.process.wait_with_output()?;
        Ok(ExecOutput {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    /// Spawn with the default bubblewrap driver.
    pub fn spawn(agent_dir: &str, entrypoint: &str) -> anyhow::Result<Self> {
        Self::spawn_with_driver(SandboxDriverKind::Bubblewrap, agent_dir, entrypoint)
    }

    /// Spawn using the manifest-declared driver name.
    pub fn spawn_for_driver(
        driver_name: &str,
        agent_dir: &str,
        entrypoint: &str,
    ) -> anyhow::Result<Self> {
        let driver = SandboxDriverKind::parse(driver_name)?;
        Self::spawn_with_driver(driver, agent_dir, entrypoint)
    }

    /// Spawn using the selected driver and optional dependency install plan.
    pub fn spawn_with_driver(
        driver: SandboxDriverKind,
        agent_dir: &str,
        entrypoint: &str,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_driver_and_dependencies(driver, agent_dir, entrypoint, None, None)
    }

    /// Spawn with optional dependency management.
    ///
    /// The install phase is executed inside the sandbox workspace with no host-level fallback.
    pub fn spawn_with_driver_and_dependencies(
        driver: SandboxDriverKind,
        agent_dir: &str,
        entrypoint: &str,
        dependencies: Option<&DependencyPlan>,
        overrides: Option<&BwrapIsolationOverrides>,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_driver_and_dependencies_and_env(
            driver,
            agent_dir,
            &ExecutionKind::shell(entrypoint),
            dependencies,
            overrides,
            &[],
            None,
        )
    }

    pub fn spawn_with_driver_and_dependencies_and_env(
        driver: SandboxDriverKind,
        agent_dir: &str,
        request: &ExecutionKind,
        dependencies: Option<&DependencyPlan>,
        overrides: Option<&BwrapIsolationOverrides>,
        extra_env: &[(String, String)],
        root_session_id: Option<&str>,
    ) -> anyhow::Result<Self> {
        // Process backend: render the intent request to a shell line.
        let entrypoint = request.render_process_command()?;
        anyhow::ensure!(
            !entrypoint.trim().is_empty(),
            "entrypoint must not be empty"
        );
        if dependencies.is_some() && driver == SandboxDriverKind::MicroVm {
            anyhow::bail!("MicroVM dependency bootstrap is not implemented yet");
        }

        // Wire the SDK socket transport once; the helper produces driver-specific
        // plumbing (bubblewrap bind mount vs docker `-v`/`-e`). Now wired for
        // bubblewrap AND docker (P1) — previously bubblewrap-only.
        let wiring = wire_sdk_bridge(driver, agent_dir, root_session_id)?;
        let socket_path_sandbox = wiring.socket_path_sandbox.clone();
        let mut socket_mounts: Vec<SandboxMount> = Vec::new();
        if let Some(mount) = wiring.bwrap_mount {
            socket_mounts.push(mount);
        }

        let composed_entrypoint = compose_entrypoint(&entrypoint, dependencies)?;
        let (program, args) = match driver {
            SandboxDriverKind::Bubblewrap => {
                if dependencies.is_some() {
                    bubblewrap_shell_command(
                        agent_dir,
                        &composed_entrypoint,
                        &socket_mounts,
                        overrides,
                    )?
                } else {
                    bubblewrap_shell_command(agent_dir, &entrypoint, &socket_mounts, overrides)?
                }
            }
            SandboxDriverKind::Docker => {
                // Container env does NOT inherit the gateway process env, so the
                // socket path / PYTHONPATH / extra_env must be passed as `-e`.
                let mut docker_env = wiring.docker_env.clone();
                merge_docker_env(&mut docker_env, extra_env);
                docker_command(
                    agent_dir,
                    &composed_entrypoint,
                    &wiring.docker_volumes,
                    &docker_env,
                )?
            }
            SandboxDriverKind::MicroVm => microvm_command(&composed_entrypoint)?,
            // The WASM tier is in-process (not a host `(program, args)`); its
            // wasmtime-backed execution lands in a later P4 increment.
            SandboxDriverKind::Wasm => {
                anyhow::bail!("wasm tier runs in-process via SandboxRunner::run_to_output, not the process spawn path")
            }
        };

        let mut command = Command::new(&program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        apply_child_env(&mut command, driver, socket_path_sandbox.as_deref(), extra_env);

        let child = spawn_driver_process(&mut command, &program)?;
        Ok(Self {
            process: child,
            driver,
            _sdk_bridge: wiring.guard,
        })
    }

    /// Spawn sandbox with session content automatically mounted.
    /// Session content files (from content.write) are mounted at their original paths.
    pub fn spawn_with_session_content(
        driver: SandboxDriverKind,
        agent_dir: &str,
        entrypoint: &str,
        dependencies: Option<&DependencyPlan>,
        session_content_mounts: Vec<SandboxMount>,
        overrides: Option<&BwrapIsolationOverrides>,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_session_content_and_env(
            driver,
            agent_dir,
            &ExecutionKind::shell(entrypoint),
            dependencies,
            session_content_mounts,
            overrides,
            &[],
            None,
        )
    }

    pub fn spawn_with_session_content_and_env(
        driver: SandboxDriverKind,
        agent_dir: &str,
        request: &ExecutionKind,
        dependencies: Option<&DependencyPlan>,
        session_content_mounts: Vec<SandboxMount>,
        overrides: Option<&BwrapIsolationOverrides>,
        extra_env: &[(String, String)],
        root_session_id: Option<&str>,
    ) -> anyhow::Result<Self> {
        // Process backend: render the intent request to a shell line.
        let entrypoint = request.render_process_command()?;
        anyhow::ensure!(
            !entrypoint.trim().is_empty(),
            "entrypoint must not be empty"
        );
        if dependencies.is_some() && driver == SandboxDriverKind::MicroVm {
            anyhow::bail!("MicroVM dependency bootstrap is not implemented yet");
        }

        let mut all_mounts = session_content_mounts;

        // SDK socket transport, wired for bubblewrap AND docker (P1).
        let wiring = wire_sdk_bridge(driver, agent_dir, root_session_id)?;
        let socket_path_sandbox = wiring.socket_path_sandbox.clone();
        if let Some(mount) = wiring.bwrap_mount {
            all_mounts.push(mount);
        }

        let composed_entrypoint = compose_entrypoint(&entrypoint, dependencies)?;
        let (program, args) = match driver {
            SandboxDriverKind::Bubblewrap => {
                bubblewrap_shell_command(agent_dir, &composed_entrypoint, &all_mounts, overrides)?
            }
            SandboxDriverKind::Docker => {
                let mut docker_env = wiring.docker_env.clone();
                merge_docker_env(&mut docker_env, extra_env);
                docker_command(
                    agent_dir,
                    &composed_entrypoint,
                    &wiring.docker_volumes,
                    &docker_env,
                )?
            }
            SandboxDriverKind::MicroVm => microvm_command(&composed_entrypoint)?,
            SandboxDriverKind::Wasm => {
                anyhow::bail!("wasm tier runs in-process via SandboxRunner::run_to_output, not the process spawn path")
            }
        };

        let mut command = Command::new(&program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        apply_child_env(&mut command, driver, socket_path_sandbox.as_deref(), extra_env);

        let child = spawn_driver_process(&mut command, &program)?;
        Ok(Self {
            process: child,
            driver,
            _sdk_bridge: wiring.guard,
        })
    }
}

struct StartedSdkBridge {
    /// Socket file name (e.g. `autonoetic-<hash>.sock`). The in-sandbox mount
    /// path is driver-specific and computed by the caller via
    /// [`sdk_socket_sandbox_path`], so the bridge itself stays driver-agnostic.
    socket_name: String,
    guard: SdkBridgeGuard,
}

fn start_sdk_bridge(
    agent_dir: &str,
    root_session_id: Option<String>,
) -> anyhow::Result<StartedSdkBridge> {
    let key = SdkBridgeCacheKey {
        agent_dir: agent_dir.to_string(),
        root_session_id: root_session_id.clone(),
    };

    let mut cache = SDK_BRIDGE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    purge_idle_sdk_bridges(&mut cache);

    if let Some(shared) = cache.get(&key) {
        let active = shared.ref_count.load(Ordering::SeqCst) > 0;
        let idle_valid = shared
            .idle_deadline
            .lock()
            .ok()
            .and_then(|d| *d)
            .map(|deadline| Instant::now() < deadline)
            .unwrap_or(false);
        if active || idle_valid {
            shared.ref_count.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut deadline) = shared.idle_deadline.lock() {
                *deadline = None;
            }
            return Ok(StartedSdkBridge {
                socket_name: shared.socket_name.clone(),
                guard: SdkBridgeGuard {
                    shared: Arc::clone(shared),
                },
            });
        }
        shared.shutdown();
        cache.remove(&key);
    }

    let mut hasher = Sha256::new();
    hasher.update(agent_dir.as_bytes());
    hasher.update(std::process::id().to_ne_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let short_hash = &hash[..16];
    let socket_name = format!("autonoetic-{}.sock", short_hash);
    let host_socket_path = PathBuf::from("/tmp").join(&socket_name);
    if host_socket_path.exists() {
        fs::remove_file(&host_socket_path)?;
    }
    let listener = UnixListener::bind(&host_socket_path)?;
    listener.set_nonblocking(true)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let agent_dir_buf = PathBuf::from(agent_dir);
    let gateway_dir_buf = gateway_dir_from_agent_dir(&agent_dir_buf)?;
    let root_session_id_for_bridge = root_session_id;
    let rate_limiter = Arc::new(SdkBridgeRateLimiter::new(
        SDK_BRIDGE_RATE_LIMIT_PER_SEC,
        SDK_BRIDGE_MAX_PAYLOAD_BYTES,
    ));

    let handle = thread::spawn(move || {
        run_sdk_bridge_loop(
            listener,
            &agent_dir_buf,
            &gateway_dir_buf,
            stop_flag,
            root_session_id_for_bridge,
            rate_limiter,
        );
    });

    let shared = Arc::new(SdkBridgeShared {
        stop,
        handle: Mutex::new(Some(handle)),
        socket_path_host: host_socket_path,
        socket_name: socket_name.clone(),
        ref_count: AtomicUsize::new(1),
        idle_deadline: Mutex::new(None),
    });
    cache.insert(key, Arc::clone(&shared));

    Ok(StartedSdkBridge {
        socket_name,
        guard: SdkBridgeGuard { shared },
    })
}

/// In-sandbox path where the SDK socket is exposed for a given driver.
/// Bubblewrap binds it under the workspace dir; docker bind-mounts it to a
/// fixed path. Returns `None` for drivers that don't run the bridge yet
/// (microvm — deferred to P5).
fn sdk_socket_sandbox_path(driver: SandboxDriverKind, socket_name: &str) -> Option<String> {
    match driver {
        SandboxDriverKind::Bubblewrap => Some(format!("{}/{}", BWRAP_WORKSPACE_DIR, socket_name)),
        SandboxDriverKind::Docker => Some(DOCKER_SDK_SOCKET_PATH.to_string()),
        // microvm deferred (P5); wasm uses host-function imports, not a socket bridge (P4).
        SandboxDriverKind::MicroVm | SandboxDriverKind::Wasm => None,
    }
}

/// Centralized SDK-bridge wiring shared by every spawn path. Starts the bridge
/// (the socket transport) once and produces the driver-specific plumbing:
/// bubblewrap takes a bind `SandboxMount` + host-inherited env; docker takes
/// `docker run` `-v`/`-e` flags (its container env does **not** inherit the
/// gateway process env, so socket path + PYTHONPATH must be passed explicitly).
/// The bridge is not started for drivers without socket support yet (microvm).
#[derive(Default)]
struct SdkBridgeWiring {
    guard: Option<SdkBridgeGuard>,
    /// In-sandbox socket path; `Some` whenever the bridge was started.
    socket_path_sandbox: Option<String>,
    /// Bubblewrap: bind mount to add to the mount list.
    bwrap_mount: Option<SandboxMount>,
    /// Docker: extra `(host, container, readonly)` volumes for `docker run -v`.
    docker_volumes: Vec<(String, String, bool)>,
    /// Docker: env vars for `docker run -e` (the container won't inherit them).
    docker_env: Vec<(String, String)>,
}

/// Spawn the sandbox driver process, mapping a missing-driver `ENOENT` to a
/// clear, terminal error instead of the bare `No such file or directory
/// (os error 2)` that `Command::spawn` returns when the binary is absent.
///
/// The `resource:` tag makes it a recoverable structured tool error (the agent
/// sees it and can stop cleanly), and the `[sandbox_driver_unavailable]` marker
/// routes it through `classify_message` to a non-retryable
/// `GateUnableToEvaluate` failure so it does not burn the divergence budget.
/// See issue #600.
fn spawn_driver_process(command: &mut Command, program: &str) -> anyhow::Result<Child> {
    match command.spawn() {
        Ok(child) => Ok(child),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "sandbox_unavailable: sandbox driver '{program}' not found on PATH — this host is missing the \
             sandbox backend this agent requires. Install it (bubblewrap provides 'bwrap', Docker \
             provides 'docker') or run `autonoetic gateway preflight` to inspect host \
             capabilities. [sandbox_driver_unavailable]"
        ),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("failed to spawn sandbox driver '{program}'"))),
    }
}

fn wire_sdk_bridge(
    driver: SandboxDriverKind,
    agent_dir: &str,
    root_session_id: Option<&str>,
) -> anyhow::Result<SdkBridgeWiring> {
    let mut wiring = SdkBridgeWiring::default();
    // Drivers without socket support yet (microvm) run no bridge.
    if sdk_socket_sandbox_path(driver, "").is_none() {
        return Ok(wiring);
    }

    let bridge = start_sdk_bridge(agent_dir, root_session_id.map(|s| s.to_string()))?;
    let host_socket = bridge.guard.shared.socket_path_host.clone();
    let sandbox_socket = sdk_socket_sandbox_path(driver, &bridge.socket_name)
        .expect("driver supports the bridge (checked above)");
    wiring.socket_path_sandbox = Some(sandbox_socket.clone());

    match driver {
        SandboxDriverKind::Bubblewrap => {
            wiring.bwrap_mount = Some(SandboxMount {
                source: host_socket,
                dest: sandbox_socket,
                readonly: false,
            });
        }
        SandboxDriverKind::Docker => {
            wiring.docker_volumes.push((
                host_socket.to_string_lossy().to_string(),
                sandbox_socket.clone(),
                false,
            ));
            wiring
                .docker_env
                .push((CCOS_SOCKET_ENV.to_string(), sandbox_socket));
            // The Python SDK isn't in the docker image; mount it read-only and
            // point PYTHONPATH at the mount so the in-container client resolves.
            if let Some(sdk_path) = resolve_python_sdk_path() {
                wiring.docker_volumes.push((
                    sdk_path,
                    DOCKER_SDK_PYTHONPATH.to_string(),
                    true,
                ));
                wiring
                    .docker_env
                    .push((PYTHONPATH_ENV.to_string(), DOCKER_SDK_PYTHONPATH.to_string()));
            }
        }
        // Not reached (the early guard returns for drivers without a socket
        // bridge), but the match must stay exhaustive.
        SandboxDriverKind::MicroVm | SandboxDriverKind::Wasm => {}
    }
    wiring.guard = Some(bridge.guard);
    Ok(wiring)
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

/// Apply child-process env per driver. Bubblewrap inherits the gateway env, so
/// the SDK PYTHONPATH, socket path, and `extra_env` go on the `Command`. Docker
/// bakes its env into the `docker run` argv (`-e`, in `docker_command`) since the
/// container doesn't inherit this process's env, so nothing is set here. MicroVm
/// keeps prior behavior (`extra_env` on the `Command`).
fn apply_child_env(
    command: &mut Command,
    driver: SandboxDriverKind,
    socket_path_sandbox: Option<&str>,
    extra_env: &[(String, String)],
) {
    match driver {
        SandboxDriverKind::Bubblewrap => {
            if let Some(sdk_path) = resolve_python_sdk_path() {
                inject_pythonpath(command, &sdk_path);
            }
            if let Some(path) = socket_path_sandbox {
                command.env(CCOS_SOCKET_ENV, path);
            }
            for (key, value) in extra_env {
                if key == PYTHONPATH_ENV {
                    inject_pythonpath_value(command, value);
                } else {
                    command.env(key, value);
                }
            }
        }
        SandboxDriverKind::Docker => {}
        SandboxDriverKind::MicroVm => {
            for (key, value) in extra_env {
                command.env(key, value);
            }
        }
        // Wasm runs in-process (no child `Command`); env is applied to the
        // wasmtime store in the WASM backend, not here. Not reached today (the
        // spawn match bails first), but the match must stay exhaustive.
        SandboxDriverKind::Wasm => {}
    }
}

/// Run a request on the WASM tier (`wasm-tier` feature): resolve the `Code`
/// entry to a `.wasm` file under the agent dir and execute it via the WASI
/// backend, preopening the agent dir. Free-form shell / inline source are
/// rejected — the portable tier runs declared code, not arbitrary shell.
#[cfg(feature = "wasm-tier")]
fn run_wasm_request(
    agent_dir: &str,
    request: &ExecutionKind,
    extra_env: &[(String, String)],
    stdin: Option<Vec<u8>>,
) -> anyhow::Result<ExecOutput> {
    use crate::exec_request::CodeSource;
    let (entry, args) = match request {
        ExecutionKind::Code {
            source: CodeSource::Entry(path),
            args,
            ..
        } => (path.clone(), args.clone()),
        ExecutionKind::Code {
            source: CodeSource::Inline(_),
            ..
        } => anyhow::bail!("wasm tier: inline source is not supported yet (declare a .wasm entry)"),
        ExecutionKind::Shell { .. } => {
            anyhow::bail!("wasm tier does not support free-form shell execution")
        }
    };
    // Keep the module strictly under the agent dir: reject absolute paths and any
    // `..` traversal so a manifest entry can't read/execute outside the bundle.
    let entry_path = Path::new(&entry);
    anyhow::ensure!(
        entry_path.is_relative()
            && !entry_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "wasm entry must be a relative path within the agent dir (no `..`): {entry}"
    );
    let wasm_path = Path::new(agent_dir).join(&entry);
    let wasm = std::fs::read(&wasm_path)
        .map_err(|e| anyhow::anyhow!("reading wasm entry {}: {e}", wasm_path.display()))?;
    let out = crate::wasm_backend::run_wasi_module(
        &wasm,
        Path::new(agent_dir),
        // Same guest workspace path the process tiers use, so input-file env
        // vars (built against BWRAP_WORKSPACE_DIR) resolve inside the module too.
        BWRAP_WORKSPACE_DIR,
        &args,
        extra_env,
        stdin.unwrap_or_default(),
        &crate::wasm_backend::WasmLimits::default(),
    )?;
    Ok(ExecOutput {
        exit_code: out.exit_code,
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

#[cfg(not(feature = "wasm-tier"))]
fn run_wasm_request(
    _agent_dir: &str,
    _request: &ExecutionKind,
    _extra_env: &[(String, String)],
    _stdin: Option<Vec<u8>>,
) -> anyhow::Result<ExecOutput> {
    anyhow::bail!("wasm sandbox tier requires the `wasm-tier` build feature")
}

fn run_sdk_bridge_loop(
    listener: UnixListener,
    agent_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    stop: Arc<AtomicBool>,
    root_session_id: Option<String>,
    rate_limiter: Arc<SdkBridgeRateLimiter>,
) {
    let abuse_seq = Arc::new(AtomicU64::new(1));
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(_e) = handle_sdk_client(
                    stream,
                    agent_dir,
                    gateway_dir,
                    root_session_id.as_deref(),
                    &rate_limiter,
                    &abuse_seq,
                ) {
                    // Ignore bridge client failures in thin compatibility mode.
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn handle_sdk_client(
    mut stream: UnixStream,
    agent_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    root_session_id: Option<&str>,
    rate_limiter: &SdkBridgeRateLimiter,
    abuse_seq: &AtomicU64,
) -> anyhow::Result<()> {
    let max_bytes = rate_limiter.max_payload_bytes();
    let mut buf = vec![0u8; max_bytes + 1];
    let n = {
        let mut reader = BufReader::new(&stream).take((max_bytes + 1) as u64);
        reader.read(&mut buf)?
    };
    let line = String::from_utf8_lossy(&buf[..n]);
    if line.trim().is_empty() {
        return Ok(());
    }

    if n > max_bytes {
        let seq = abuse_seq.fetch_add(1, Ordering::Relaxed);
        let _ = log_sdk_bridge_abuse(
            agent_dir,
            "payload_too_large",
            &format!("payload exceeds {} byte limit", max_bytes),
            seq,
        );
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {
                "code": -32001,
                "message": "payload_too_large",
                "data": {"error_type": "sdk_bridge_abuse", "max_bytes": max_bytes}
            }
        });
        let payload = serde_json::to_string(&error_resp)? + "\n";
        stream.write_all(payload.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    let request: serde_json::Value = serde_json::from_str(&line)?;
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = request
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = request
        .get("params")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    if !rate_limiter.check_rate() {
        let seq = abuse_seq.fetch_add(1, Ordering::Relaxed);
        let _ = log_sdk_bridge_abuse(
            agent_dir,
            "rate_limited",
            &format!(
                "sdk bridge call '{}' exceeded rate limit of {}/sec",
                method,
                rate_limiter.rate_limit()
            ),
            seq,
        );
        let error_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32002,
                "message": "rate_limited",
                "data": {"error_type": "sdk_bridge_abuse", "rate_limit_per_sec": rate_limiter.rate_limit}
            }
        });
        let payload = serde_json::to_string(&error_resp)? + "\n";
        stream.write_all(payload.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    let response =
        match dispatch_sdk_method(&method, &params, agent_dir, gateway_dir, root_session_id) {
            Ok(result) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }),
            Err(err) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32000,
                    "message": err.to_string(),
                    "data": {
                        "error_type": "policy_violation"
                    }
                }
            }),
        };

    let payload = serde_json::to_string(&response)? + "\n";
    stream.write_all(payload.as_bytes())?;
    stream.flush()?;
    Ok(())
}

pub fn validate_sdk_relative_path(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!path.trim().is_empty(), "path must not be empty");
    anyhow::ensure!(!path.starts_with('/'), "absolute paths are not allowed");
    anyhow::ensure!(
        !path
            .split('/')
            .any(|part| part == ".." || part.is_empty() || part == "."),
        "path traversal is not allowed"
    );
    Ok(())
}

pub fn gateway_dir_from_agent_dir(agent_dir: &std::path::Path) -> anyhow::Result<PathBuf> {
    let agents_root = agent_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("agent directory is missing agents-root parent"))?;
    let gateway_dir = agents_root.join(".gateway");
    fs::create_dir_all(&gateway_dir)?;
    Ok(gateway_dir)
}

fn agent_id_from_agent_dir(agent_dir: &std::path::Path) -> anyhow::Result<String> {
    let id = agent_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("unable to derive agent id from agent directory"))?;
    Ok(id.to_string())
}

fn next_sdk_event_seq(log_path: &std::path::Path) -> anyhow::Result<u64> {
    if !log_path.exists() {
        return Ok(1);
    }
    let entries = crate::causal_chain::CausalLogger::read_entries(log_path)?;
    Ok(entries.last().map(|e| e.event_seq + 1).unwrap_or(1))
}

fn log_sdk_memory_event(
    agent_dir: &std::path::Path,
    action: &str,
    payload: serde_json::Value,
) -> anyhow::Result<()> {
    let actor_id = agent_id_from_agent_dir(agent_dir)?;
    let log_path = agent_dir.join("history").join("causal_chain.jsonl");
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let logger = crate::causal_chain::CausalLogger::new(&log_path)?;
    let event_seq = next_sdk_event_seq(&log_path)?;
    logger.log(
        &actor_id,
        "sdk-bridge",
        None,
        event_seq,
        "memory",
        action,
        EntryStatus::Success,
        Some(crate::log_redaction::RedactedPayload::from_raw(payload)),
    )
}

fn log_sdk_bridge_abuse(
    agent_dir: &std::path::Path,
    violation: &str,
    detail: &str,
    event_seq: u64,
) -> anyhow::Result<()> {
    let actor_id = agent_id_from_agent_dir(agent_dir)?;
    let log_path = agent_dir.join("history").join("causal_chain.jsonl");
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let logger = crate::causal_chain::CausalLogger::new(&log_path)?;
    logger.log(
        &actor_id,
        "sdk-bridge",
        None,
        event_seq,
        "abuse",
        violation,
        EntryStatus::Denied,
        Some(crate::log_redaction::RedactedPayload::from_raw(
            serde_json::json!({
                "detail": detail,
                "violation": violation,
            }),
        )),
    )
}

fn load_json_file(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    let body = fs::read_to_string(path)?;
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    Ok(parsed)
}

fn write_json_file(path: &std::path::Path, value: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn list_state_keys(state_dir: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    if !state_dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(state_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            out.push(format!("state/{}", entry.file_name().to_string_lossy()));
        }
    }
    out.sort();
    Ok(out)
}

fn dispatch_sdk_method(
    method: &str,
    params: &serde_json::Map<String, serde_json::Value>,
    agent_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    root_session_id: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let method = method.replace('.', "_");
    match method.as_str() {
        "memory_read" => {
            let path = params
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("memory.read requires path"))?;
            validate_sdk_relative_path(path)?;
            let content = fs::read_to_string(agent_dir.join(path))?;
            Ok(serde_json::json!({ "content": content }))
        }
        "memory_write" => {
            let path = params
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("memory.write requires path"))?;
            validate_sdk_relative_path(path)?;
            let content = params
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("memory.write requires content"))?;
            let target = agent_dir.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, content)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "memory_list_keys" => {
            let keys = list_state_keys(&agent_dir.join("state"))?;
            Ok(serde_json::json!({ "keys": keys }))
        }
        "memory_remember" => {
            let key = params
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("memory.remember requires key"))?;
            let value = params
                .get("value")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("memory.remember requires value"))?;
            let scope = params
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("sdk");
            let agent_id = agent_id_from_agent_dir(agent_dir)?;
            let mem = crate::runtime::memory::Tier2Memory::open_for_agent(
                gateway_dir,
                None,
                &agent_id,
                root_session_id,
            )?;
            let source_ref = format!("sdk_bridge:{}", agent_id);
            let content = serde_json::to_string(&value)?;
            let mut memory = autonoetic_types::memory::MemoryObject::new(
                key.to_string(),
                scope.to_string(),
                agent_id.clone(),
                agent_id,
                source_ref,
                content,
            );
            if let Some(sid) = root_session_id {
                if !sid.trim().is_empty() {
                    memory.visibility = autonoetic_types::memory::MemoryVisibility::Session {
                        session_id: sid.to_string(),
                    };
                }
            }
            let memory = crate::runtime::tools::block_on_memory(mem.save_memory(&memory))?;
            let _ = log_sdk_memory_event(
                agent_dir,
                "remember",
                serde_json::json!({
                    "memory_id": memory.memory_id,
                    "scope": memory.scope,
                    "source_ref": memory.source_ref,
                }),
            );
            Ok(serde_json::json!({
                "ok": true,
                "memory_id": memory.memory_id,
                "scope": memory.scope,
                "source_ref": memory.source_ref,
            }))
        }
        "memory_recall" => {
            let key = params
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("memory.recall requires key"))?;
            let agent_id = agent_id_from_agent_dir(agent_dir)?;
            let mem = crate::runtime::memory::Tier2Memory::open_for_agent(
                gateway_dir,
                None,
                &agent_id,
                root_session_id,
            )?;
            match crate::runtime::tools::block_on_memory(mem.recall(key)) {
                Ok(memory) => {
                    let parsed = serde_json::from_str::<serde_json::Value>(&memory.content)
                        .unwrap_or_else(|_| serde_json::Value::String(memory.content.clone()));
                    let _ = log_sdk_memory_event(
                        agent_dir,
                        "recall",
                        serde_json::json!({
                            "memory_id": memory.memory_id,
                            "scope": memory.scope,
                            "source_ref": memory.source_ref,
                        }),
                    );
                    Ok(serde_json::json!({
                        "value": parsed,
                        "scope": memory.scope,
                        "source_ref": memory.source_ref,
                    }))
                }
                Err(_) => Ok(serde_json::json!({ "value": serde_json::Value::Null })),
            }
        }
        "memory_search" => {
            let query = params
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("memory.search requires query"))?
                .to_ascii_lowercase();
            let scope = params
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("sdk");
            let agent_id = agent_id_from_agent_dir(agent_dir)?;
            let mem = crate::runtime::memory::Tier2Memory::open_for_agent(
                gateway_dir,
                None,
                &agent_id,
                root_session_id,
            )?;
            let search_results = crate::runtime::tools::block_on_memory(mem.search(scope, None))?;
            let mut results = Vec::<String>::new();
            for memory in search_results {
                let hay = format!("{} {}", memory.memory_id, memory.content).to_ascii_lowercase();
                if hay.contains(&query) {
                    results.push(format!("{}: {}", memory.memory_id, memory.content));
                }
            }
            let _ = log_sdk_memory_event(
                agent_dir,
                "search",
                serde_json::json!({
                    "scope": scope,
                    "query": query,
                    "count": results.len(),
                }),
            );
            Ok(serde_json::json!({ "results": results }))
        }
        "state_checkpoint" => {
            let data = params
                .get("data")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("state.checkpoint requires data"))?;
            let checkpoint = serde_json::json!({ "data": data });
            write_json_file(
                &agent_dir.join("state").join("sdk_checkpoint.json"),
                &checkpoint,
            )?;
            Ok(serde_json::json!({ "ok": true }))
        }
        "state_get_checkpoint" => {
            let path = agent_dir.join("state").join("sdk_checkpoint.json");
            let payload = load_json_file(&path)?;
            Ok(
                serde_json::json!({ "data": payload.get("data").cloned().unwrap_or(serde_json::Value::Null) }),
            )
        }
        "events_emit" => {
            let event_type = params
                .get("type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("events.emit requires type"))?;
            let data = params
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let events_path = agent_dir.join("history").join("sdk_events.jsonl");
            if let Some(parent) = events_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let event = serde_json::json!({ "type": event_type, "data": data });
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(events_path)?;
            writeln!(file, "{}", serde_json::to_string(&event)?)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        other => anyhow::bail!("unsupported SDK method '{}'", other),
    }
}

fn resolve_python_sdk_path() -> Option<String> {
    // 1. Explicit env var override
    if let Ok(path) = std::env::var(PYTHON_SDK_PATH_ENV) {
        if !path.trim().is_empty() {
            return Some(path);
        }
    }

    // 2. Deployed SDK snapshot in .gateway/sdk/python/ (set by bootstrap)
    if let Some(path) = SDK_DEPLOYED_PATH.get() {
        if Path::new(path).exists() {
            return Some(path.clone());
        }
    }

    // 3. Fallback to source tree (developer mode)
    let local: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("autonoetic-sdk")
        .join("python");
    if local.exists() {
        return Some(local.to_string_lossy().to_string());
    }

    None
}

fn inject_pythonpath(command: &mut Command, sdk_path: &str) {
    match std::env::var(PYTHONPATH_ENV) {
        Ok(existing) if !existing.trim().is_empty() => {
            command.env(PYTHONPATH_ENV, format!("{}:{}", sdk_path, existing));
        }
        _ => {
            command.env(PYTHONPATH_ENV, sdk_path);
        }
    }
}

fn inject_pythonpath_value(command: &mut Command, extra_path: &str) {
    let current = command
        .get_envs()
        .find(|(k, _)| *k == PYTHONPATH_ENV)
        .and_then(|(_, v)| v.map(|s| s.to_string_lossy().to_string()));
    match current {
        Some(existing) => {
            command.env(PYTHONPATH_ENV, format!("{}:{}", extra_path, existing));
        }
        None => {
            command.env(PYTHONPATH_ENV, extra_path.to_string());
        }
    }
}

fn split_entrypoint(entrypoint: &str) -> anyhow::Result<(String, Vec<String>)> {
    let parts: Vec<&str> = entrypoint.split_whitespace().collect();
    anyhow::ensure!(!parts.is_empty(), "entrypoint must not be empty");
    let program = parts[0].to_string();
    let args = parts[1..].iter().map(|s| s.to_string()).collect();
    Ok((program, args))
}

/// Sensitive files inside the gateway directory that a sandboxed process must
/// never read: the credential vault key + encrypted blob, the SQLite session
/// DB (and its WAL/shm sidecars), and the Ed25519 attestation identity key.
/// The public sidecar and `sdk/` / `wiki/` / `constitution/` are deliberately
/// NOT listed — `sdk/` is the SDK PYTHONPATH source (needed in-sandbox), and
/// the constitution is public agent-readable law.
const BWRAP_GATEWAY_SENSITIVE_FILES: &[&str] = &[
    "vault.key",
    "vault.enc.json",
    "gateway.db",
    "gateway.db-shm",
    "gateway.db-wal",
    "state_attestation.ed25519",
];

/// Sensitive subdirectories inside the gateway directory that a sandboxed
/// process has no legitimate reason to read directly — the agent reaches all
/// of these through tools, not the filesystem. Masked with an empty tmpfs.
const BWRAP_GATEWAY_SENSITIVE_DIRS: &[&str] = &[
    "sessions",
    "scheduler",
    "checkpoints",
    "history",
    "logs",
    "revisions",
];

/// Push a bubblewrap flag that shadows a single host FILE with `/dev/null`
/// (reads return EOF), so the sandboxed process cannot read it. No-op when the
/// path doesn't exist on the host — bwrap resolves sources against the host
/// filesystem, so the dest must resolve against the ro-mounted `/` too.
fn push_deny_file(flags: &mut Vec<String>, p: &std::path::Path) {
    if p.exists() {
        flags.push("--ro-bind".to_string());
        flags.push("/dev/null".to_string());
        flags.push(p.to_string_lossy().to_string());
    }
}

/// Push a bubblewrap flag that shadows a host DIRECTORY with an empty tmpfs,
/// so the sandboxed process cannot read or list it. No-op when the path
/// doesn't exist on the host.
fn push_deny_dir(flags: &mut Vec<String>, p: &std::path::Path) {
    if p.exists() {
        flags.push("--tmpfs".to_string());
        flags.push(p.to_string_lossy().to_string());
    }
}

/// Build the bubblewrap argv slice that masks gateway-internal secrets and any
/// operator-registered deny paths, so a sandboxed process cannot read them
/// through the ro-mounted host `/` (stopgap for #1002). The gateway directory
/// is derived from the agent dir as its sibling `.gateway`; its `sdk/` subtree
/// is intentionally left accessible (the sandbox reads its PYTHONPATH from that
/// host path). Operator paths come from [`init_sandbox_host_deny_paths`].
fn bwrap_deny_path_flags(agent_dir: &str) -> Vec<String> {
    let mut flags = Vec::new();

    if let Some(gateway_dir) = std::path::Path::new(agent_dir)
        .parent()
        .map(|agents_root| agents_root.join(".gateway"))
    {
        for name in BWRAP_GATEWAY_SENSITIVE_FILES {
            push_deny_file(&mut flags, &gateway_dir.join(name));
        }
        for name in BWRAP_GATEWAY_SENSITIVE_DIRS {
            push_deny_dir(&mut flags, &gateway_dir.join(name));
        }
    }

    if let Some(extra) = SANDBOX_HOST_DENY_PATHS.get() {
        for p in extra {
            if p.is_dir() {
                push_deny_dir(&mut flags, p);
            } else {
                push_deny_file(&mut flags, p);
            }
        }
    }

    flags
}

fn bubblewrap_command(
    agent_dir: &str,
    entrypoint: &str,
    overrides: Option<&BwrapIsolationOverrides>,
) -> anyhow::Result<(String, Vec<String>)> {
    let (program, args) = split_entrypoint(entrypoint)?;
    let mut argv = vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--bind".to_string(),
        agent_dir.to_string(),
        BWRAP_WORKSPACE_DIR.to_string(),
        "--chdir".to_string(),
        BWRAP_WORKSPACE_DIR.to_string(),
    ];
    append_bwrap_isolation_flags(&mut argv, overrides);
    // Mask gateway-internal secrets + operator deny paths (stopgap for #1002:
    // the host `/` is ro-bind-mounted above). Must come after the ro-bind of
    // `/` so the destinations resolve, and before any explicit re-expose
    // mounts so they can layer back on top.
    argv.extend(bwrap_deny_path_flags(agent_dir));
    argv.push("--".to_string());
    argv.push(program);
    argv.extend(args);
    Ok(("bwrap".to_string(), argv))
}

/// Extra bind mount for sandbox (source_path → dest_path).
#[derive(Debug, Clone)]
pub struct SandboxMount {
    pub source: std::path::PathBuf,
    pub dest: String,
    pub readonly: bool,
}

fn bubblewrap_shell_command(
    agent_dir: &str,
    shell_command: &str,
    extra_mounts: &[SandboxMount],
    overrides: Option<&BwrapIsolationOverrides>,
) -> anyhow::Result<(String, Vec<String>)> {
    anyhow::ensure!(
        !shell_command.trim().is_empty(),
        "shell command must not be empty"
    );
    let mut argv = vec![
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--bind".to_string(),
        agent_dir.to_string(),
        BWRAP_WORKSPACE_DIR.to_string(),
        "--chdir".to_string(),
        BWRAP_WORKSPACE_DIR.to_string(),
    ];
    append_bwrap_isolation_flags(&mut argv, overrides);

    // Mask gateway-internal secrets + operator deny paths (stopgap for #1002:
    // the host `/` is ro-bind-mounted above) BEFORE explicit content/SDK
    // mounts so those can layer back on top of the masked paths when needed.
    argv.extend(bwrap_deny_path_flags(agent_dir));

    // Add extra bind mounts for session content
    for mount in extra_mounts {
        // Create the source directory if needed
        if let Some(parent) = mount.source.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Ensure the source file/directory exists (create empty if not)
        if !mount.source.exists() {
            if mount.source.extension().is_some() {
                let _ = std::fs::write(&mount.source, "");
            } else {
                let _ = std::fs::create_dir_all(&mount.source);
            }
        }
        let bind_flag = if mount.readonly {
            "--ro-bind".to_string()
        } else {
            "--bind".to_string()
        };
        argv.push(bind_flag);
        argv.push(mount.source.to_string_lossy().to_string());
        argv.push(mount.dest.clone());
    }

    argv.extend(vec![
        "--".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        shell_command.to_string(),
    ]);
    Ok(("bwrap".to_string(), argv))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BwrapDevMode {
    /// Keep legacy behavior (no explicit /dev mount override).
    Legacy,
    /// Mount bubblewrap minimal writable /dev.
    Minimal,
    /// Bind host /dev into sandbox (least isolated, most compatible).
    HostBind,
}

pub fn append_bwrap_isolation_flags(
    argv: &mut Vec<String>,
    overrides: Option<&BwrapIsolationOverrides>,
) {
    argv.push("--unshare-all".to_string());

    let force_off = overrides.map(|o| o.force_network_off).unwrap_or(false);
    let share_net = if force_off {
        false
    } else {
        overrides
            .map(|o| o.share_net)
            .unwrap_or_else(bwrap_share_net_enabled)
    };

    if share_net {
        argv.push("--share-net".to_string());
    }

    match bwrap_dev_mode() {
        BwrapDevMode::Legacy => {}
        BwrapDevMode::Minimal => {
            argv.push("--dev".to_string());
            argv.push("/dev".to_string());
        }
        BwrapDevMode::HostBind => {
            argv.push("--dev-bind".to_string());
            argv.push("/dev".to_string());
            argv.push("/dev".to_string());
        }
    }
}

fn bwrap_share_net_enabled() -> bool {
    // Env overrides are gated behind an explicit opt-in.
    if sandbox_env_overrides_allowed() {
        if let Some(val) = parse_env_bool(std::env::var(BWRAP_SHARE_NET_ENV).ok().as_deref()) {
            return val;
        }
    } else if std::env::var(BWRAP_SHARE_NET_ENV).ok().is_some() {
        tracing::warn!(
            env = BWRAP_SHARE_NET_ENV,
            gate = ALLOW_SANDBOX_ENV_OVERRIDES_ENV,
            "Ignoring sandbox network env override in strict mode"
        );
    }
    // Config value (if initialized)
    SANDBOX_CONFIG.get().map(|c| c.share_net).unwrap_or(false)
}

fn bwrap_dev_mode() -> BwrapDevMode {
    // Env overrides are gated behind an explicit opt-in.
    if sandbox_env_overrides_allowed() {
        if let Some(val) = std::env::var(BWRAP_DEV_MODE_ENV).ok() {
            if !val.trim().is_empty() {
                return parse_bwrap_dev_mode(Some(&val));
            }
        }
    } else if std::env::var(BWRAP_DEV_MODE_ENV).ok().is_some() {
        tracing::warn!(
            env = BWRAP_DEV_MODE_ENV,
            gate = ALLOW_SANDBOX_ENV_OVERRIDES_ENV,
            "Ignoring sandbox dev-mode env override in strict mode"
        );
    }
    // Config value (if initialized)
    if let Some(config) = SANDBOX_CONFIG.get() {
        return parse_bwrap_dev_mode(Some(&config.dev_mode));
    }
    BwrapDevMode::Legacy
}

fn sandbox_env_overrides_allowed() -> bool {
    parse_env_bool(
        std::env::var(ALLOW_SANDBOX_ENV_OVERRIDES_ENV)
            .ok()
            .as_deref(),
    )
    .unwrap_or(false)
}

fn parse_env_bool(value: Option<&str>) -> Option<bool> {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        None => None,
        Some(v) if v.is_empty() => None,
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Some(true),
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Some(false),
        _ => None,
    }
}

fn parse_bwrap_dev_mode(value: Option<&str>) -> BwrapDevMode {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        None => BwrapDevMode::Legacy,
        Some(v) if v.is_empty() => BwrapDevMode::Legacy,
        Some(v) if matches!(v.as_str(), "legacy" | "none") => BwrapDevMode::Legacy,
        Some(v) if matches!(v.as_str(), "minimal" | "dev") => BwrapDevMode::Minimal,
        Some(v) if matches!(v.as_str(), "host" | "host-bind" | "dev-bind") => {
            BwrapDevMode::HostBind
        }
        Some(other) => {
            tracing::warn!(
                env = BWRAP_DEV_MODE_ENV,
                value = %other,
                "Unknown bwrap dev mode, falling back to legacy"
            );
            BwrapDevMode::Legacy
        }
    }
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

fn microvm_command(_entrypoint: &str) -> anyhow::Result<(String, Vec<String>)> {
    let cfg = std::env::var(FIRECRACKER_CONFIG_ENV).map_err(|_| {
        anyhow::anyhow!(
            "Missing required environment variable {}",
            FIRECRACKER_CONFIG_ENV
        )
    })?;
    let argv = vec!["--config-file".to_string(), cfg];
    Ok(("firecracker".to_string(), argv))
}

fn compose_entrypoint(entrypoint: &str, deps: Option<&DependencyPlan>) -> anyhow::Result<String> {
    let Some(plan) = deps else {
        return Ok(entrypoint.to_string());
    };

    // Check if entrypoint already has the runtime command prepended
    // This handles cases like "python3 /tmp/script.py" where the user already specified the runtime
    let has_python = entrypoint.starts_with("python3 ") || entrypoint.starts_with("python ");
    let has_node = entrypoint.starts_with("node ");

    // If no packages needed, just run the entrypoint
    if plan.packages.is_empty() {
        // If entrypoint already starts with the runtime, use it as-is
        match plan.runtime {
            DependencyRuntime::Python if has_python => return Ok(entrypoint.to_string()),
            DependencyRuntime::NodeJs if has_node => return Ok(entrypoint.to_string()),
            DependencyRuntime::Python => return Ok(format!("python3 {entrypoint}")),
            DependencyRuntime::NodeJs => return Ok(format!("node {entrypoint}")),
        }
    }

    for pkg in &plan.packages {
        validate_dependency_package(pkg)?;
    }
    let joined = plan.packages.join(" ");

    // If entrypoint already has runtime, just run it after installing packages
    let run_cmd = match plan.runtime {
        DependencyRuntime::Python if has_python => entrypoint.to_string(),
        DependencyRuntime::NodeJs if has_node => entrypoint.to_string(),
        DependencyRuntime::Python => format!("python3 {entrypoint}"),
        DependencyRuntime::NodeJs => format!("node {entrypoint}"),
    };

    let composed = match plan.runtime {
        DependencyRuntime::Python => format!(
            "python3 -m venv .autonoetic_venv && ./.autonoetic_venv/bin/pip install --disable-pip-version-check --no-input --no-cache-dir {joined} && {run_cmd}"
        ),
        DependencyRuntime::NodeJs => format!(
            "npm install --no-save --prefix .autonoetic_node {joined} && NODE_PATH=.autonoetic_node/node_modules {run_cmd}"
        ),
    };
    Ok(composed)
}

fn validate_dependency_package(pkg: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !pkg.trim().is_empty(),
        "dependency package name must not be empty"
    );
    // Keep package token grammar tight to avoid shell injection in thin bootstrap strings.
    let allowed = pkg.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '.' | '_' | '-' | '<' | '>' | '=' | '!' | '~' | '[' | ']' | ',' | '@' | '/'
            )
    });
    anyhow::ensure!(allowed, "invalid dependency token '{}'", pkg);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    #[test]
    fn spawn_driver_process_explains_missing_driver() {
        // A driver binary that cannot exist on PATH → clear, tagged terminal
        // error instead of a bare "No such file or directory". (#600)
        let mut cmd = Command::new("autonoetic-no-such-sandbox-driver-xyz");
        let err = spawn_driver_process(&mut cmd, "autonoetic-no-such-sandbox-driver-xyz")
            .expect_err("spawning a missing driver must fail");
        let msg = err.to_string();
        assert!(msg.contains("not found on PATH"), "got: {msg}");
        assert!(msg.contains("sandbox_driver_unavailable"), "got: {msg}");
        assert!(msg.contains("preflight"), "got: {msg}");
    }

    #[test]
    fn test_parse_driver_kind() {
        assert_eq!(
            SandboxDriverKind::parse("bubblewrap").expect("bubblewrap should parse"),
            SandboxDriverKind::Bubblewrap
        );
        assert_eq!(
            SandboxDriverKind::parse("docker").expect("docker should parse"),
            SandboxDriverKind::Docker
        );
        assert_eq!(
            SandboxDriverKind::parse("microvm").expect("microvm should parse"),
            SandboxDriverKind::MicroVm
        );
        assert_eq!(
            SandboxDriverKind::parse("wasm").expect("wasm should parse"),
            SandboxDriverKind::Wasm
        );
        assert_eq!(
            SandboxDriverKind::parse("wasi").expect("wasi alias should parse"),
            SandboxDriverKind::Wasm
        );
        assert!(SandboxDriverKind::parse("nope").is_err());
    }

    #[test]
    fn wasm_uses_run_to_output_not_the_process_spawn_path() {
        // The process spawn path is for bwrap/docker/microvm; wasm runs in-process
        // via run_to_output, so spawn_for_driver("wasm") bails with that guidance.
        let result = SandboxRunner::spawn_for_driver("wasm", "/tmp/agent", "python main.py");
        assert!(result.is_err(), "wasm must not use the process spawn path");
        let err = result.err().unwrap().to_string();
        assert!(err.contains("run_to_output"), "got: {err}");
    }

    #[test]
    fn test_bubblewrap_command_shape() {
        // Use an isolated tempdir so the deny-path masking (derived from the
        // agent dir's sibling `.gateway`) has nothing to mask — keeps the
        // fixed argv positions stable regardless of the host's /tmp state.
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let (_bin, argv) = bubblewrap_command(&agent_dir_str, "python main.py", None)
            .expect("bubblewrap command should build");
        assert_eq!(argv[0], "--ro-bind");
        assert_eq!(argv[3], "--bind");
        assert_eq!(argv[4], agent_dir_str);
        assert_eq!(argv[5], "/tmp");
        assert_eq!(argv[6], "--chdir");
        assert_eq!(argv[7], "/tmp");
        assert_eq!(argv[8], "--unshare-all");
        assert_eq!(argv[9], "--");
        assert_eq!(argv[10], "python");
        assert_eq!(argv[11], "main.py");
    }

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
            (CCOS_SOCKET_ENV.to_string(), DOCKER_SDK_SOCKET_PATH.to_string()),
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

    #[test]
    fn test_sdk_socket_sandbox_path_per_driver() {
        let expected_bwrap = format!("{}/s.sock", BWRAP_WORKSPACE_DIR);
        assert_eq!(
            sdk_socket_sandbox_path(SandboxDriverKind::Bubblewrap, "s.sock"),
            Some(expected_bwrap)
        );
        assert_eq!(
            sdk_socket_sandbox_path(SandboxDriverKind::Docker, "s.sock"),
            Some(DOCKER_SDK_SOCKET_PATH.to_string())
        );
        // microvm has no bridge yet (P5)
        assert!(sdk_socket_sandbox_path(SandboxDriverKind::MicroVm, "s.sock").is_none());
        // wasm uses host-function imports, not a socket bridge.
        assert!(sdk_socket_sandbox_path(SandboxDriverKind::Wasm, "s.sock").is_none());
    }

    #[test]
    fn test_microvm_command_requires_env() {
        let old = std::env::var(FIRECRACKER_CONFIG_ENV).ok();
        std::env::remove_var(FIRECRACKER_CONFIG_ENV);
        let err = microvm_command("ignored").expect_err("microvm command should fail without env");
        assert!(
            err.to_string().contains(FIRECRACKER_CONFIG_ENV),
            "error should mention missing firecracker env"
        );
        if let Some(v) = old {
            std::env::set_var(FIRECRACKER_CONFIG_ENV, v);
        }
    }

    #[test]
    fn test_compose_python_dependencies() {
        let plan = DependencyPlan {
            runtime: DependencyRuntime::Python,
            packages: vec!["requests==2.32.3".to_string()],
        };
        let cmd =
            compose_entrypoint("python main.py", Some(&plan)).expect("compose should succeed");
        assert!(cmd.contains("python3 -m venv .autonoetic_venv"));
        assert!(cmd.contains("pip install"));
        assert!(cmd.contains("requests==2.32.3"));
        assert!(cmd.ends_with("python main.py"));
    }

    #[test]
    fn test_compose_node_dependencies() {
        let plan = DependencyPlan {
            runtime: DependencyRuntime::NodeJs,
            packages: vec!["lodash@4.17.21".to_string()],
        };
        let cmd = compose_entrypoint("node app.js", Some(&plan)).expect("compose should succeed");
        assert!(cmd.contains("npm install --no-save --prefix .autonoetic_node"));
        assert!(cmd.contains("NODE_PATH=.autonoetic_node/node_modules"));
        assert!(cmd.ends_with("node app.js"));
    }

    #[test]
    fn test_dependency_token_validation_rejects_unsafe_chars() {
        let err =
            validate_dependency_package("foo;rm -rf /").expect_err("unsafe token should fail");
        assert!(err.to_string().contains("invalid dependency token"));
    }

    #[test]
    fn test_compose_preserves_existing_runtime_prefix() {
        // When command already has python3 prefix and no packages needed
        let plan = DependencyPlan {
            runtime: DependencyRuntime::Python,
            packages: vec![], // No packages
        };
        let cmd = compose_entrypoint("python3 /tmp/script.py", Some(&plan))
            .expect("compose should succeed");
        // Should NOT double-prefix: "python3 python3 /tmp/script.py"
        assert_eq!(cmd, "python3 /tmp/script.py");
    }

    #[test]
    fn test_compose_with_runtime_prefix_and_packages() {
        // When command already has python3 prefix and packages need installing
        let plan = DependencyPlan {
            runtime: DependencyRuntime::Python,
            packages: vec!["requests".to_string()],
        };
        let cmd = compose_entrypoint("python3 /tmp/script.py", Some(&plan))
            .expect("compose should succeed");
        assert!(cmd.contains("pip install"));
        assert!(cmd.contains("requests"));
        // Should end with original command, not "python3 python3"
        assert!(cmd.ends_with("python3 /tmp/script.py"));
    }

    #[test]
    fn test_bubblewrap_shell_command_shape() {
        // Isolated tempdir: no `.gateway` secrets to mask, so argv positions
        // are stable.
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let (_bin, argv) = bubblewrap_shell_command(&agent_dir_str, "echo hi", &[], None)
            .expect("shell command should build");
        assert_eq!(argv[0], "--ro-bind");
        assert_eq!(argv[3], "--bind");
        assert_eq!(argv[4], agent_dir_str);
        assert_eq!(argv[5], "/tmp");
        assert_eq!(argv[6], "--chdir");
        assert_eq!(argv[7], "/tmp");
        assert_eq!(argv[8], "--unshare-all");
        assert_eq!(argv[9], "--");
        assert_eq!(argv[10], "sh");
        assert_eq!(argv[11], "-c"); // Non-login shell
        assert_eq!(argv[12], "echo hi");
    }

    /// Stopgap for #1002: the bubblewrap sandbox masks gateway-internal secrets
    /// (vault key, session DB, identity key, sessions/, …) so a sandboxed
    /// process cannot read them through the ro-mounted host `/`. The SDK
    /// subtree (`sdk/`) must stay visible because PYTHONPATH points there.
    #[test]
    fn bwrap_deny_flags_mask_gateway_secrets_but_not_sdk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents_root = tmp.path();
        let gateway_dir = agents_root.join(".gateway");
        let agent_dir = agents_root.join("demo.agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(gateway_dir.join("sdk")).unwrap();
        std::fs::create_dir_all(gateway_dir.join("sessions")).unwrap();
        std::fs::write(gateway_dir.join("vault.key"), "secret").unwrap();
        std::fs::write(gateway_dir.join("gateway.db"), "db").unwrap();
        std::fs::write(gateway_dir.join("state_attestation.ed25519"), "k").unwrap();

        let flags = bwrap_deny_path_flags(agent_dir.to_str().unwrap());
        let joined = flags.join(" ");

        // Sensitive files are shadowed with /dev/null.
        assert!(
            joined.contains(&format!(
                "--ro-bind /dev/null {}",
                gateway_dir.join("vault.key").display()
            )),
            "vault.key must be masked, got: {joined}"
        );
        assert!(
            joined.contains(&format!(
                "--ro-bind /dev/null {}",
                gateway_dir.join("gateway.db").display()
            )),
            "gateway.db must be masked, got: {joined}"
        );
        assert!(
            joined.contains(&format!(
                "--ro-bind /dev/null {}",
                gateway_dir.join("state_attestation.ed25519").display()
            )),
            "identity key must be masked, got: {joined}"
        );
        // Sensitive dirs are shadowed with an empty tmpfs.
        assert!(
            joined.contains(&format!("--tmpfs {}", gateway_dir.join("sessions").display())),
            "sessions/ must be masked, got: {joined}"
        );
        // The SDK subtree and constitution must NOT be masked — the sandbox
        // reads its PYTHONPATH from `<gateway_dir>/sdk` and the constitution
        // is public agent-readable law.
        assert!(
            !joined.contains(&format!("{}", gateway_dir.join("sdk").display())),
            "sdk/ must stay accessible, got: {joined}"
        );
    }

    /// Non-existent gateway paths produce no deny flags (no false mountpoints,
    /// no perturbing the argv shape for fresh gateways).
    #[test]
    fn bwrap_deny_flags_skip_nonexistent_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        // No .gateway at all → nothing to mask.
        let flags = bwrap_deny_path_flags(agent_dir.to_str().unwrap());
        assert!(flags.is_empty(), "expected no deny flags, got: {flags:?}");
    }

    /// The full shell-command builder inserts the deny slice between the
    /// isolation flags and the `--` separator (and before explicit re-expose
    /// mounts, so they can overlay masked paths).
    #[test]
    fn bubblewrap_shell_command_includes_deny_slice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let gateway_dir = tmp.path().join(".gateway");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&gateway_dir).unwrap();
        std::fs::write(gateway_dir.join("vault.key"), "secret").unwrap();

        let (_bin, argv) = bubblewrap_shell_command(agent_dir.to_str().unwrap(), "echo hi", &[], None)
            .expect("shell command should build");
        let unshare = argv.iter().position(|a| a == "--unshare-all").unwrap();
        let sep = argv.iter().position(|a| a == "--").unwrap();
        // The deny slice sits between isolation flags and the separator.
        let deny_slice = &argv[unshare + 1..sep];
        assert!(
            deny_slice.iter().any(|a| a == "--ro-bind"),
            "expected deny flags before the separator, argv: {argv:?}"
        );
    }

    /// Relative deny paths are resolved to absolute form — bwrap destinations
    /// are namespace-absolute, so a relative path would silently fail to mask.
    #[test]
    fn normalize_deny_paths_makes_relative_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let out = normalize_deny_paths(&[PathBuf::from("config.yaml")]);
        assert!(
            out.contains(&cwd.join("config.yaml")),
            "relative path must be made absolute, got: {out:?}"
        );
        // No relative entries survive.
        assert!(out.iter().all(|p| p.is_absolute()));
    }

    /// A symlinked config is masked at BOTH the link path and its canonical
    /// target, so the file can't be read via its real path around the link.
    #[test]
    fn normalize_deny_paths_adds_symlink_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real_config.yaml");
        std::fs::write(&real, "provider: x").unwrap();
        let link = tmp.path().join("config.yaml");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let out = normalize_deny_paths(&[link.clone()]);
        let canon = std::fs::canonicalize(&real).unwrap();
        assert!(out.contains(&link), "link path must be present, got: {out:?}");
        assert!(
            out.contains(&canon),
            "canonical target must also be masked, got: {out:?}"
        );
    }

    /// A non-existent path still contributes its absolute form (it may be
    /// created later), and produces no canonical entry.
    #[test]
    fn normalize_deny_paths_missing_path_is_absolute_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does_not_exist.yaml");
        let out = normalize_deny_paths(&[missing.clone()]);
        assert_eq!(out, vec![missing], "missing path → absolute form only");
    }

    /// Duplicate absolute/canonical forms collapse to one entry.
    #[test]
    fn normalize_deny_paths_dedups() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("f.yaml");
        std::fs::write(&p, "x").unwrap();
        // Pass the same path twice: the result must hold no duplicate entries,
        // whether or not abs and canonical forms differ on this platform
        // (macOS tempdirs live under a /var symlink).
        let out = normalize_deny_paths(&[p.clone(), p.clone()]);
        let unique: std::collections::HashSet<&PathBuf> = out.iter().collect();
        assert_eq!(
            unique.len(),
            out.len(),
            "no duplicate entries, got: {out:?}"
        );
        assert!(out.contains(&p));
    }

    #[test]
    fn test_parse_env_bool() {
        assert_eq!(parse_env_bool(Some("1")), Some(true));
        assert_eq!(parse_env_bool(Some("true")), Some(true));
        assert_eq!(parse_env_bool(Some("yes")), Some(true));
        assert_eq!(parse_env_bool(Some("on")), Some(true));
        assert_eq!(parse_env_bool(Some("0")), Some(false));
        assert_eq!(parse_env_bool(Some("false")), Some(false));
        assert_eq!(parse_env_bool(Some("no")), Some(false));
        assert_eq!(parse_env_bool(Some("off")), Some(false));
        assert_eq!(parse_env_bool(Some("wat")), None);
        assert_eq!(parse_env_bool(None), None);
    }

    #[test]
    fn test_parse_bwrap_dev_mode() {
        assert_eq!(parse_bwrap_dev_mode(None), BwrapDevMode::Legacy);
        assert_eq!(parse_bwrap_dev_mode(Some("legacy")), BwrapDevMode::Legacy);
        assert_eq!(parse_bwrap_dev_mode(Some("none")), BwrapDevMode::Legacy);
        assert_eq!(parse_bwrap_dev_mode(Some("minimal")), BwrapDevMode::Minimal);
        assert_eq!(parse_bwrap_dev_mode(Some("dev")), BwrapDevMode::Minimal);
        assert_eq!(parse_bwrap_dev_mode(Some("host")), BwrapDevMode::HostBind);
        assert_eq!(
            parse_bwrap_dev_mode(Some("host-bind")),
            BwrapDevMode::HostBind
        );
        assert_eq!(
            parse_bwrap_dev_mode(Some("dev-bind")),
            BwrapDevMode::HostBind
        );
        assert_eq!(parse_bwrap_dev_mode(Some("unknown")), BwrapDevMode::Legacy);
    }

    #[test]
    fn test_sdk_dispatch_memory_session_visibility() {
        use crate::runtime::memory::{SqliteMemoryStore, Tier2Memory};
        use autonoetic_types::memory::MemoryVisibility;
        use std::sync::Arc;

        let temp = tempfile::tempdir().expect("tempdir should create");
        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("create gateway dir");

        let gw_store =
            Arc::new(crate::scheduler::gateway_store::GatewayStore::open(&gateway_dir).unwrap());
        let mem_store: Arc<dyn crate::runtime::memory::MemoryStore> =
            Arc::new(SqliteMemoryStore::new(gw_store));

        let writer = Tier2Memory::with_store(
            Arc::clone(&mem_store),
            "writer-agent",
            Some("root-session-1".into()),
        );
        let reader_same_session = Tier2Memory::with_store(
            Arc::clone(&mem_store),
            "reader-agent",
            Some("root-session-1".into()),
        );
        let reader_diff_session =
            Tier2Memory::with_store(mem_store, "reader-agent", Some("other-session".into()));

        let mut m = autonoetic_types::memory::MemoryObject::new(
            "session_fact".into(),
            "test".into(),
            "writer-agent".into(),
            "writer-agent".into(),
            "sdk_bridge:writer-agent".into(),
            "secret_value".into(),
        );
        m.visibility = MemoryVisibility::Session {
            session_id: "root-session-1".into(),
        };
        crate::runtime::tools::block_on_memory(writer.save_memory(&m)).unwrap();

        let recalled =
            crate::runtime::tools::block_on_memory(reader_same_session.recall("session_fact"));
        assert!(
            recalled.is_ok(),
            "reader in same session should read the memory"
        );
        assert_eq!(recalled.unwrap().content, "secret_value");

        let recalled_wrong =
            crate::runtime::tools::block_on_memory(reader_diff_session.recall("session_fact"));
        assert!(
            recalled_wrong.is_err(),
            "reader in different session should not read session-scoped memory"
        );
    }

    #[test]
    fn test_sdk_dispatch_checkpoint_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir should create");
        let agent_dir = temp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("create gateway dir");
        let checkpoint_params =
            serde_json::Map::from_iter(vec![("data".to_string(), json!({"cursor": 42}))]);
        let written = dispatch_sdk_method(
            "state_checkpoint",
            &checkpoint_params,
            &agent_dir,
            &gateway_dir,
            None,
        )
        .expect("checkpoint should succeed");
        assert_eq!(written["ok"], json!(true));

        let loaded = dispatch_sdk_method(
            "state_get_checkpoint",
            &serde_json::Map::new(),
            &agent_dir,
            &gateway_dir,
            None,
        )
        .expect("load checkpoint should succeed");
        assert_eq!(loaded["data"]["cursor"], json!(42));
    }
}
