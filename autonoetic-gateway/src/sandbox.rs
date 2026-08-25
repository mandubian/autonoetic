//! Sandbox runner: orchestrates one sandboxed execution end to end.
//!
//! This module owns what is *common* to every execution — the SDK bridge socket
//! and its dispatch, dependency composition, spawning and waiting on the child.
//! What differs per backend (command construction, SDK socket plumbing, child
//! env, network guarantees, dependency support) lives behind the
//! [`driver::SandboxDriver`] trait, one impl per file under [`driver`]. Nothing
//! here matches on which driver is in play.

pub mod driver;

pub use driver::bubblewrap::{append_bwrap_isolation_flags, BWRAP_WORKSPACE_DIR};
pub use driver::{
    builtin_registry, DriverTier, InProcessRequest, SandboxDriver, SandboxDriverKind,
    SandboxDriverRegistry, SpawnSpec,
};

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

pub(crate) const PYTHONPATH_ENV: &str = "PYTHONPATH";
const PYTHON_SDK_PATH_ENV: &str = "AUTONOETIC_PYTHON_SDK_PATH";
pub(crate) const CCOS_SOCKET_ENV: &str = "CCOS_SOCKET_PATH";
pub(crate) const ALLOW_SANDBOX_ENV_OVERRIDES_ENV: &str = "AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES";

static SANDBOX_CONFIG: OnceLock<SandboxConfig> = OnceLock::new();
static SDK_DEPLOYED_PATH: OnceLock<String> = OnceLock::new();

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

/// The startup sandbox config, if [`init_sandbox_config`] has run. Drivers read
/// their operator-set defaults (network sharing, `/dev` mode) from here.
pub(crate) fn sandbox_config() -> Option<&'static SandboxConfig> {
    SANDBOX_CONFIG.get()
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
/// Paths are normalized before storage: relative paths are made absolute (bwrap
/// dests are namespace-absolute against the ro-mounted host `/`, so a relative
/// path would silently fail to mask), and symlinked targets are added alongside
/// the link path so a config reachable via its real path can't escape masking.
/// See `driver::bubblewrap` for the masking mechanics.
pub fn init_sandbox_host_deny_paths(paths: Vec<PathBuf>) {
    driver::bubblewrap::init_host_deny_paths(paths);
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

pub(crate) struct SdkBridgeGuard {
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
    /// **process** drivers spawn a child and wait; **in-process** drivers (the
    /// wasm tier) run inside the gateway. This is the unified entry the agent
    /// execution path migrates onto (P4 inc 2c); the `spawn_*` methods remain
    /// the process-only path used today.
    pub fn run_to_output(
        driver: SandboxDriverKind,
        agent_dir: &str,
        gateway_dir: &Path,
        request: &ExecutionKind,
        dependencies: Option<&DependencyPlan>,
        overrides: Option<&BwrapIsolationOverrides>,
        extra_env: &[(String, String)],
        root_session_id: Option<&str>,
        stdin: Option<Vec<u8>>,
    ) -> anyhow::Result<ExecOutput> {
        let backend = driver.driver()?;
        if backend.tier() == DriverTier::InProcess {
            return backend.run_in_process(&InProcessRequest {
                agent_dir,
                request,
                extra_env,
                stdin,
            });
        }
        let mut runner = Self::spawn_with_driver_and_dependencies_and_env(
            driver,
            agent_dir,
            gateway_dir,
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
    pub fn spawn(agent_dir: &str, gateway_dir: &Path, entrypoint: &str) -> anyhow::Result<Self> {
        Self::spawn_with_driver(
            SandboxDriverKind::Bubblewrap,
            agent_dir,
            gateway_dir,
            entrypoint,
        )
    }

    /// Spawn using the manifest-declared driver name.
    pub fn spawn_for_driver(
        driver_name: &str,
        agent_dir: &str,
        gateway_dir: &Path,
        entrypoint: &str,
    ) -> anyhow::Result<Self> {
        let driver = SandboxDriverKind::parse(driver_name)?;
        Self::spawn_with_driver(driver, agent_dir, gateway_dir, entrypoint)
    }

    /// Spawn using the selected driver and optional dependency install plan.
    pub fn spawn_with_driver(
        driver: SandboxDriverKind,
        agent_dir: &str,
        gateway_dir: &Path,
        entrypoint: &str,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_driver_and_dependencies(
            driver,
            agent_dir,
            gateway_dir,
            entrypoint,
            None,
            None,
        )
    }

    /// Spawn with optional dependency management.
    ///
    /// The install phase is executed inside the sandbox workspace with no host-level fallback.
    pub fn spawn_with_driver_and_dependencies(
        driver: SandboxDriverKind,
        agent_dir: &str,
        gateway_dir: &Path,
        entrypoint: &str,
        dependencies: Option<&DependencyPlan>,
        overrides: Option<&BwrapIsolationOverrides>,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_driver_and_dependencies_and_env(
            driver,
            agent_dir,
            gateway_dir,
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
        gateway_dir: &Path,
        request: &ExecutionKind,
        dependencies: Option<&DependencyPlan>,
        overrides: Option<&BwrapIsolationOverrides>,
        extra_env: &[(String, String)],
        root_session_id: Option<&str>,
    ) -> anyhow::Result<Self> {
        Self::spawn_process(
            driver,
            agent_dir,
            gateway_dir,
            request,
            dependencies,
            Vec::new(),
            overrides,
            extra_env,
            root_session_id,
        )
    }

    /// Spawn sandbox with session content automatically mounted.
    /// Session content files (from content.write) are mounted at their original paths.
    pub fn spawn_with_session_content(
        driver: SandboxDriverKind,
        agent_dir: &str,
        gateway_dir: &Path,
        entrypoint: &str,
        dependencies: Option<&DependencyPlan>,
        session_content_mounts: Vec<SandboxMount>,
        overrides: Option<&BwrapIsolationOverrides>,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_session_content_and_env(
            driver,
            agent_dir,
            gateway_dir,
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
        gateway_dir: &Path,
        request: &ExecutionKind,
        dependencies: Option<&DependencyPlan>,
        session_content_mounts: Vec<SandboxMount>,
        overrides: Option<&BwrapIsolationOverrides>,
        extra_env: &[(String, String)],
        root_session_id: Option<&str>,
    ) -> anyhow::Result<Self> {
        Self::spawn_process(
            driver,
            agent_dir,
            gateway_dir,
            request,
            dependencies,
            session_content_mounts,
            overrides,
            extra_env,
            root_session_id,
        )
    }

    /// The single process-tier spawn path. Everything here is driver-agnostic:
    /// render the request, compose dependencies, start the SDK bridge, then ask
    /// the driver for its `(program, argv)` and its child env.
    #[allow(clippy::too_many_arguments)]
    fn spawn_process(
        driver: SandboxDriverKind,
        agent_dir: &str,
        gateway_dir: &Path,
        request: &ExecutionKind,
        dependencies: Option<&DependencyPlan>,
        session_content_mounts: Vec<SandboxMount>,
        overrides: Option<&BwrapIsolationOverrides>,
        extra_env: &[(String, String)],
        root_session_id: Option<&str>,
    ) -> anyhow::Result<Self> {
        let backend = driver.driver()?;
        anyhow::ensure!(
            backend.tier() == DriverTier::Process,
            "{} tier runs in-process via SandboxRunner::run_to_output, not the process spawn path",
            backend.names()[0]
        );

        // Process backend: render the intent request to a shell line.
        let entrypoint = request.render_process_command()?;
        anyhow::ensure!(
            !entrypoint.trim().is_empty(),
            "entrypoint must not be empty"
        );
        if let Some(plan) = dependencies {
            backend.check_dependency_support(plan)?;
        }

        // Wire the SDK socket transport once; the driver contributes its own
        // plumbing (bubblewrap bind mount vs docker `-v`/`-e`).
        let wiring = wire_sdk_bridge(backend.as_ref(), agent_dir, gateway_dir, root_session_id)?;
        let mut mounts = session_content_mounts;
        mounts.extend(wiring.mounts.iter().cloned());

        let composed_entrypoint = compose_entrypoint(&entrypoint, dependencies)?;
        let (program, args) = backend.build_command(&SpawnSpec {
            agent_dir,
            gateway_dir,
            entrypoint: &composed_entrypoint,
            mounts: &mounts,
            overrides,
            extra_env,
            bridge: &wiring,
        })?;

        let mut command = Command::new(&program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        backend.apply_child_env(
            &mut command,
            wiring.socket_path_sandbox.as_deref(),
            extra_env,
        );

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
    gateway_dir: &Path,
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
    // The authoritative gateway dir, threaded from the execution engine. This
    // used to be `gateway_dir_from_agent_dir(agent_dir)` — `agent_dir.parent()`
    // plus `.gateway` — which in production resolved to a nonexistent path
    // *inside* the agent's own revision directory (and created it). The SDK
    // bridge's `memory_remember` / `memory_recall` / `memory_search` handlers
    // resolve against this path, so the hop pointed agent memory at a stray
    // per-revision directory instead of the real gateway store. Same defect
    // class as #1145.
    let gateway_dir_buf = gateway_dir.to_path_buf();
    fs::create_dir_all(&gateway_dir_buf)?;
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

/// Driver-supplied plumbing that exposes the SDK bridge socket inside a
/// sandbox. Each driver fills only what its backend needs — bubblewrap a bind
/// mount plus host-inherited env, container drivers `-v`/`-e` flags — via
/// [`SandboxDriver::wire_sdk_bridge`]. Left entirely empty for drivers that run
/// no bridge.
#[derive(Default)]
pub struct SdkBridgeWiring {
    pub(crate) guard: Option<SdkBridgeGuard>,
    /// In-sandbox socket path; `Some` whenever the bridge was started.
    pub socket_path_sandbox: Option<String>,
    /// Extra bind mounts the driver needs inside the sandbox.
    pub mounts: Vec<SandboxMount>,
    /// Extra `(host, container, readonly)` volumes for container drivers.
    pub volumes: Vec<(String, String, bool)>,
    /// Env vars a container driver must bake into its argv (the container
    /// won't inherit the gateway process env).
    pub env: Vec<(String, String)>,
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

/// Start the SDK bridge once and let the driver wire it into its sandbox.
/// Drivers that run no bridge (microvm; the wasm tier uses host-function
/// imports) short-circuit before the socket is created.
fn wire_sdk_bridge(
    driver: &dyn SandboxDriver,
    agent_dir: &str,
    gateway_dir: &Path,
    root_session_id: Option<&str>,
) -> anyhow::Result<SdkBridgeWiring> {
    let mut wiring = SdkBridgeWiring::default();
    if !driver.runs_sdk_bridge() {
        return Ok(wiring);
    }

    let bridge = start_sdk_bridge(agent_dir, gateway_dir, root_session_id.map(|s| s.to_string()))?;
    let host_socket = bridge.guard.shared.socket_path_host.clone();
    let sandbox_socket = driver
        .sdk_socket_path(&bridge.socket_name)
        .expect("driver runs the bridge (checked above)");
    wiring.socket_path_sandbox = Some(sandbox_socket.clone());
    driver.wire_sdk_bridge(&host_socket, &sandbox_socket, &mut wiring);
    wiring.guard = Some(bridge.guard);
    Ok(wiring)
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

pub(crate) fn resolve_python_sdk_path() -> Option<String> {
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

pub(crate) fn inject_pythonpath(command: &mut Command, sdk_path: &str) {
    match std::env::var(PYTHONPATH_ENV) {
        Ok(existing) if !existing.trim().is_empty() => {
            command.env(PYTHONPATH_ENV, format!("{}:{}", sdk_path, existing));
        }
        _ => {
            command.env(PYTHONPATH_ENV, sdk_path);
        }
    }
}

pub(crate) fn inject_pythonpath_value(command: &mut Command, extra_path: &str) {
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

/// Extra bind mount for sandbox (source_path → dest_path).
#[derive(Debug, Clone)]
pub struct SandboxMount {
    pub source: std::path::PathBuf,
    pub dest: String,
    pub readonly: bool,
}

// ---------------------------------------------------------------------------
// Declared host mounts (#1002 slices 2-3)
// ---------------------------------------------------------------------------

/// One declared mount the operator's allowlist does not cover, with the
/// reason and the grant that would satisfy it — denials teach (RFC
/// sandbox-mount-allow-set §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountDenial {
    /// The requesting manifest's raw `host_path` (pre-expansion).
    pub host_path: String,
    /// Canonicalized form that was checked.
    pub canonical_path: String,
    pub reason: String,
}

/// Resolve a manifest's `runtime.mounts` against the operator's
/// `sandbox.allowed_mount_roots`. A mount is granted iff its canonicalized
/// host path is equal to or under a canonicalized allowed root; anything else
/// — outside every root, or a path that doesn't exist on host (bwrap cannot
/// bind a missing source) — is denied loudly. The manifest alone never widens
/// filesystem reach; the config allowlist is the grant, like
/// `NetworkAccess.hosts`.
pub fn resolve_declared_mounts(
    declared: &[autonoetic_types::agent::DeclaredMount],
    allowed_roots: &[String],
    allowed_rw_roots: &[String],
    protected_paths: &[PathBuf],
) -> (Vec<SandboxMount>, Vec<MountDenial>) {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let mut granted = Vec::new();
    let mut denied = Vec::new();

    let mut unresolvable_roots = 0usize;
    let mut canon_roots = |roots: &[String]| -> Vec<PathBuf> {
        roots
            .iter()
            .filter_map(|root| expand_tilde(root, &home))
            .filter_map(|root| match std::fs::canonicalize(&root) {
                Ok(p) => Some(p),
                Err(_) => {
                    unresolvable_roots += 1;
                    None
                }
            })
            .collect()
    };
    let canonical_roots = canon_roots(allowed_roots);
    let canonical_rw_roots = canon_roots(allowed_rw_roots);

    let covered_by = |canonical: &Path, roots: &[PathBuf]| {
        roots.iter().any(|root| canonical.starts_with(root))
    };
    // Deny beats grant (#1163 review finding 1): a declared mount that is at,
    // above, or inside a protected path would shadow the bwrap deny-mask
    // emitted earlier in the argv (mask flags precede extra mounts so
    // gateway-owned mounts can layer over them — a privilege declared mounts
    // must not inherit). Gateway-owned paths are already normalized.
    let overlaps_protected = |canonical: &Path| {
        protected_paths
            .iter()
            .any(|pp| canonical.starts_with(pp) || pp.starts_with(canonical))
    };
    let root_hint = |denial: &mut MountDenial| {
        if unresolvable_roots > 0 {
            denial.reason.push_str(&format!(
                " (note: {unresolvable_roots} allowed_mount_roots entr                 {:?} could not be resolved on this host — check for typos)",
                if unresolvable_roots == 1 { "y" } else { "ies" }
            ));
        }
    };

    for mount in declared {
        let raw = mount.host_path.trim();
        if raw.is_empty() {
            denied.push(MountDenial {
                host_path: mount.host_path.clone(),
                canonical_path: String::new(),
                reason: "empty host_path".to_string(),
            });
            continue;
        }
        let expanded = match expand_tilde(raw, &home) {
            Some(p) => p,
            None => {
                denied.push(MountDenial {
                    host_path: mount.host_path.clone(),
                    canonical_path: raw.to_string(),
                    reason: format!("cannot expand '~' (HOME unset) for {raw}"),
                });
                continue;
            }
        };
        let canonical = match std::fs::canonicalize(&expanded) {
            Ok(p) => p,
            Err(_) => {
                let mut d = MountDenial {
                    host_path: mount.host_path.clone(),
                    canonical_path: expanded.to_string_lossy().to_string(),
                    reason: [
                        "host path does not exist (or is not reachable); create it ",
                        "or fix the declaration",
                    ]
                    .concat()
                        .to_string(),
                };
                root_hint(&mut d);
                denied.push(d);
                continue;
            }
        };
        if overlaps_protected(&canonical) {
            denied.push(MountDenial {
                host_path: mount.host_path.clone(),
                canonical_path: canonical.to_string_lossy().to_string(),
                reason: [
                        "declared mount overlaps a protected path (the gateway ",
                        "directory or an operator deny-list entry) — mounts never shadow ",
                        "the sandbox secret mask",
                    ]
                    .concat()
                    .to_string(),
            });
            continue;
        }
        if !covered_by(&canonical, &canonical_roots) && !covered_by(&canonical, &canonical_rw_roots)
        {
            let mut d = MountDenial {
                host_path: mount.host_path.clone(),
                canonical_path: canonical.to_string_lossy().to_string(),
                reason: [
                        "not under any sandbox.allowed_mount_roots entry — ask the ",
                        "operator to extend the allowlist (config) or remove the ",
                        "declaration",
                    ]
                    .concat()
                    .to_string(),
            };
            root_hint(&mut d);
            denied.push(d);
            continue;
        }
        // Mode ceiling (#1163 review finding 2): readonly is granted as
        // declared; rw additionally requires an rw root. A manifest asking
        // for rw under an ro-only root is a widening attempt — deny with a
        // reason that names the rw allowlist, never silently downgrade.
        let effective_readonly = if mount.readonly {
            true
        } else if covered_by(&canonical, &canonical_rw_roots) {
            false
        } else {
            let mut d = MountDenial {
                host_path: mount.host_path.clone(),
                canonical_path: canonical.to_string_lossy().to_string(),
                reason: [
                        "writable mount requires the path to also be under ",
                        "sandbox.allowed_mount_roots_rw — read-only roots cannot be ",
                        "widened by a manifest",
                    ]
                    .concat()
                    .to_string(),
            };
            root_hint(&mut d);
            denied.push(d);
            continue;
        };
        // The mount lands at its canonical path inside the sandbox: symlinked
        // declarations resolve to their target (a `~`-declared path under a
        // symlinked home lands at the real path).
        let dest = canonical.to_string_lossy().to_string();
        granted.push(SandboxMount {
            source: canonical,
            dest,
            readonly: effective_readonly,
        });
    }
    (granted, denied)
}

fn expand_tilde(p: &str, home: &Option<PathBuf>) -> Option<PathBuf> {
    if p == "~" {
        return home.clone();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home.as_ref().map(|h| h.join(rest));
    }
    if p.starts_with('~') {
        return None;
    }
    Some(PathBuf::from(p))
}

/// Compose the gateway-asserted mount-set record for one sandbox execution
/// (#1002): the paths the gateway itself decided this exec can see — the
/// blanket ro-bound host root while bubblewrap legacy mode is in effect, the
/// writable workspace, and every explicit mount. Capped at [`MOUNT_SET_CAP`]
/// entries with an explicit `truncated:+N` marker. Tools report this in their
/// result; the processor persists it on the execution trace.
pub fn compose_mount_set(
    driver: driver::SandboxDriverKind,
    agent_dir: &str,
    mounts: &[SandboxMount],
) -> Vec<String> {
    let mut entries: Vec<String> = Vec::with_capacity(2 + mounts.len());
    if driver == driver::SandboxDriverKind::Bubblewrap {
        entries.push("ro:host_root".to_string());
    }
    entries.push(format!("rw:{agent_dir}"));
    for mount in mounts {
        entries.push(format!(
            "{}:{}",
            if mount.readonly { "ro" } else { "rw" },
            mount.source.display()
        ));
    }
    if entries.len() > MOUNT_SET_CAP {
        // Reserve a slot for the marker so the capped list, marker included,
        // never exceeds the stated cap.
        let overflow = entries.len() - (MOUNT_SET_CAP - 1);
        entries.truncate(MOUNT_SET_CAP - 1);
        entries.push(format!("truncated:+{overflow}"));
    }
    entries
}

/// Upper bound on persisted mount-set entries — one row per exec must stay a
/// row, not a directory listing.
pub const MOUNT_SET_CAP: usize = 64;

pub(crate) fn sandbox_env_overrides_allowed() -> bool {
    parse_env_bool(
        std::env::var(ALLOW_SANDBOX_ENV_OVERRIDES_ENV)
            .ok()
            .as_deref(),
    )
    .unwrap_or(false)
}

pub(crate) fn parse_env_bool(value: Option<&str>) -> Option<bool> {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        None => None,
        Some(v) if v.is_empty() => None,
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Some(true),
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Some(false),
        _ => None,
    }
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

/// #1002: the shared mount-set composer — what every sandbox executor
/// reports and what the durable trace persists.
#[cfg(test)]
mod compose_mount_set_tests {
    use super::*;

    fn mount(src: &str, ro: bool) -> SandboxMount {
        SandboxMount {
            source: std::path::PathBuf::from(src),
            dest: format!("/tmp/{src}"),
            readonly: ro,
        }
    }

    /// Bubblewrap legacy mode ro-binds the whole host `/` — the record must
    /// say so explicitly, or the set understates visibility.
    #[test]
    fn bubblewrap_includes_host_root_marker() {
        let set = compose_mount_set(
            driver::SandboxDriverKind::Bubblewrap,
            "/agents/coder",
            &[mount("notes.md", true)],
        );
        assert_eq!(set[0], "ro:host_root");
        assert!(set.contains(&"rw:/agents/coder".to_string()));
        assert!(set.contains(&"ro:notes.md".to_string()));
    }

    /// Only bubblewrap ro-binds `/`; the other process tiers must not claim
    /// host-root visibility they don't have.
    #[test]
    fn non_bubblewrap_tiers_omit_host_root_marker() {
        for kind in [
            driver::SandboxDriverKind::Docker,
            driver::SandboxDriverKind::MicroVm,
        ] {
            let set = compose_mount_set(kind, "/agents/coder", &[]);
            assert!(
                !set.contains(&"ro:host_root".to_string()),
                "{kind:?} must not claim the host root"
            );
        }
    }

    /// The cap reserves a slot for the marker: the capped list, marker
    /// included, never exceeds MOUNT_SET_CAP, and the marker counts what was
    /// dropped (the off-by-one #1160's review caught — now pinned).
    #[test]
    fn cap_includes_the_marker_in_the_budget() {
        let mounts: Vec<SandboxMount> = (0..MOUNT_SET_CAP + 10)
            .map(|i| mount(&format!("m{i}"), true))
            .collect();
        let set = compose_mount_set(
            driver::SandboxDriverKind::Bubblewrap,
            "/agents/coder",
            &mounts,
        );
        assert_eq!(
            set.len(),
            MOUNT_SET_CAP,
            "capped list + marker must not exceed the cap"
        );
        let marker = set.last().unwrap();
        assert!(
            marker.starts_with("truncated:+"),
            "last entry must be the overflow marker: {marker}"
        );
        // 74 mounts + 2 header entries = 76; 63 kept (incl. headers) → 13 dropped.
        assert_eq!(marker, "truncated:+13");
    }

    /// Under the cap: no marker, entries verbatim.
    #[test]
    fn under_cap_has_no_marker() {
        let set = compose_mount_set(
            driver::SandboxDriverKind::Bubblewrap,
            "/a",
            &[mount("x", false), mount("y", true)],
        );
        assert_eq!(set.len(), 4);
        assert!(set.iter().all(|e| !e.starts_with("truncated:")));
        assert!(set.contains(&"rw:x".to_string()));
        assert!(set.contains(&"ro:y".to_string()));
    }
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
    fn wasm_uses_run_to_output_not_the_process_spawn_path() {
        // The process spawn path is for bwrap/docker/microvm; wasm runs in-process
        // via run_to_output, so spawn_for_driver("wasm") bails with that guidance.
        let result = SandboxRunner::spawn_for_driver(
            "wasm",
            "/tmp/agent",
            Path::new("/tmp/runtime"),
            "python main.py",
        );
        assert!(result.is_err(), "wasm must not use the process spawn path");
        let err = result.err().unwrap().to_string();
        assert!(err.contains("run_to_output"), "got: {err}");
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

#[cfg(test)]
mod declared_mount_tests {
    use super::*;

    fn declared(path: &str, readonly: bool) -> autonoetic_types::agent::DeclaredMount {
        autonoetic_types::agent::DeclaredMount {
            host_path: path.to_string(),
            readonly,
        }
    }

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn resolve(
        mounts: &[autonoetic_types::agent::DeclaredMount],
        ro_roots: &[String],
        rw_roots: &[String],
        protected: &[PathBuf],
    ) -> (Vec<SandboxMount>, Vec<MountDenial>) {
        resolve_declared_mounts(mounts, ro_roots, rw_roots, protected)
    }

    fn s(p: &std::path::Path) -> String {
        p.to_string_lossy().to_string()
    }

    /// Root `/a`, mount `/a/b/c` — the strict-subpath case operators actually
    /// configure (#1163 review finding 5).
    #[test]
    fn strict_subpath_of_root_is_granted() {
        let tmp = tmpdir();
        let root = tmp.path().join("root");
        let nested = root.join("b");
        let file_dir = nested.join("c");
        std::fs::create_dir_all(&file_dir).unwrap();
        let (granted, denied) = resolve(
            &[declared(&s(&file_dir), true)],
            &[s(&root)],
            &[],
            &[],
        );
        assert!(denied.is_empty(), "unexpected denials: {denied:?}");
        assert_eq!(granted.len(), 1);
        assert!(granted[0].readonly);
    }

    /// `/root` must NOT cover `/rootdir` — component-wise prefix only. Pins
    /// the property a string-prefix refactor would silently break (#1163
    /// review finding 5).
    #[test]
    fn root_does_not_cover_sibling_with_shared_prefix() {
        let tmp = tmpdir();
        let root = tmp.path().join("root");
        let sibling = tmp.path().join("rootdir");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let (granted, denied) = resolve(
            &[declared(&s(&sibling), true)],
            &[s(&root)],
            &[],
            &[],
        );
        assert!(granted.is_empty());
        assert_eq!(denied.len(), 1);
    }

    /// Root itself granted (exact match).
    #[test]
    fn root_exact_match_is_granted() {
        let tmp = tmpdir();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let (granted, denied) = resolve(&[declared(&s(&root), true)], &[s(&root)], &[], &[]);
        assert!(denied.is_empty());
        assert_eq!(granted.len(), 1);
    }

    /// Outside every root → denied, naming the grant.
    #[test]
    fn mount_outside_roots_is_denied_with_reason() {
        let tmp = tmpdir();
        let secret = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&secret).unwrap();
        let root = tmp.path().join("granted");
        std::fs::create_dir_all(&root).unwrap();
        let (granted, denied) = resolve(&[declared(&s(&secret), false)], &[s(&root)], &[], &[]);
        assert!(granted.is_empty());
        assert_eq!(denied.len(), 1);
        assert!(
            denied[0].reason.contains("allowed_mount_roots"),
            "{}",
            denied[0].reason
        );
    }

    /// Empty allowlist denies everything (fail-closed default).
    #[test]
    fn empty_allowlist_denies_all() {
        let tmp = tmpdir();
        let p = tmp.path().join("x");
        std::fs::create_dir_all(&p).unwrap();
        let (granted, denied) = resolve(&[declared(&s(&p), true)], &[], &[], &[]);
        assert!(granted.is_empty());
        assert_eq!(denied.len(), 1);
    }

    /// Non-existent host path → denied; assert the count before indexing so a
    /// regression fails on the assert, not a panic (#1163 review finding 5).
    #[test]
    fn nonexistent_path_is_denied() {
        let tmp = tmpdir();
        let missing = tmp.path().join("nope");
        let (granted, denied) = resolve(
            &[declared(&missing.to_string_lossy(), true)],
            &[s(tmp.path())],
            &[],
            &[],
        );
        assert!(granted.is_empty());
        assert_eq!(denied.len(), 1);
        assert!(
            denied[0].reason.contains("does not exist"),
            "{}",
            denied[0].reason
        );
    }

    /// A symlinked declared path is checked (and mounted) at its canonical
    /// target — indirection cannot smuggle reach (#1163 review finding 1's
    /// cousin, pinned here).
    #[test]
    fn symlinked_path_is_checked_canonically() {
        let tmp = tmpdir();
        let inside = tmp.path().join("inside");
        std::fs::create_dir_all(&inside).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let link = tmp.path().join("linkdir");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let (granted, denied) = resolve(&[declared(&s(&link), true)], &[s(&inside)], &[], &[]);
        assert!(granted.is_empty());
        assert_eq!(denied.len(), 1);
    }

    /// DENY BEATS GRANT (#1163 review finding 1): a declared mount at, above,
    /// or inside the gateway dir is rejected even when the allowlist covers
    /// it — mounts never shadow the bwrap secret mask. `vault.key`,
    /// `gateway.db`, `state_attestation.ed25519` stay masked.
    #[test]
    fn mount_overlapping_gateway_dir_is_denied_even_when_allowlisted() {
        let tmp = tmpdir();
        let gw = tmp.path().join("runtime");
        std::fs::create_dir_all(gw.join("sessions")).unwrap();

        // (a) inside the gateway dir
        let (granted, denied) = resolve(
            &[declared(&s(&gw.join("sessions")), true)],
            &[s(tmp.path())],
            &[],
            &[gw.clone()],
        );
        assert!(granted.is_empty(), "inside-gateway grant must be refused");
        assert_eq!(denied.len(), 1);
        assert!(denied[0].reason.contains("protected"), "{}", denied[0].reason);

        // (b) ABOVE the gateway dir — the shadowing repro shape from the
        // review (`--ro-bind <proj> <proj>` after the mask flags).
        let (granted, denied) = resolve(
            &[declared(&s(tmp.path()), true)],
            &[s(tmp.path())],
            &[],
            &[gw.clone()],
        );
        assert!(granted.is_empty(), "above-gateway grant must be refused");
        assert_eq!(denied.len(), 1);
        assert!(denied[0].reason.contains("protected"));

        // (c) the gateway dir itself
        let (granted, denied) = resolve(
            &[declared(&s(&gw), true)],
            &[s(tmp.path())],
            &[],
            &[gw.clone()],
        );
        assert!(granted.is_empty(), "exact-gateway grant must be refused");
        assert_eq!(denied.len(), 1);
    }

    /// Operator deny-list entries are protected exactly like the gateway dir
    /// (#1163 review finding 1 — the operator config file with provider
    /// credentials is in SANDBOX_HOST_DENY_PATHS).
    #[test]
    fn mount_overlapping_host_deny_path_is_denied() {
        let tmp = tmpdir();
        let cfg_file = tmp.path().join("config.yaml");
        std::fs::write(&cfg_file, "provider: x").unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();

        let (granted, denied) = resolve(
            &[declared(&s(tmp.path()), true)],
            &[s(tmp.path())],
            &[],
            &[cfg_file.clone()],
        );
        assert!(granted.is_empty(), "a dir containing a deny-listed file must not be grantable");
        assert_eq!(denied.len(), 1);
        assert!(denied[0].reason.contains("protected"));
    }

    /// MODE CEILING (#1163 review finding 2): readonly is the default grant;
    /// rw requires the path to also be under `allowed_mount_roots_rw`.
    /// The manifest can narrow (ro under an rw root) but never widen.
    #[test]
    fn rw_requires_an_rw_root() {
        let tmp = tmpdir();
        let ro_root = tmp.path().join("ro");
        let rw_root = tmp.path().join("rwx");
        std::fs::create_dir_all(&ro_root).unwrap();
        std::fs::create_dir_all(&rw_root).unwrap();

        // ro root only + manifest readonly:true → granted ro.
        let (granted, denied) = resolve(&[declared(&s(&ro_root), true)], &[s(&ro_root)], &[], &[]);
        assert!(denied.is_empty());
        assert!(granted[0].readonly);

        // ro root only + manifest readonly:false → DENIED, teaching the rw
        // allowlist. Never silently downgraded.
        let (granted, denied) = resolve(&[declared(&s(&ro_root), false)], &[s(&ro_root)], &[], &[]);
        assert!(granted.is_empty());
        assert_eq!(denied.len(), 1);
        assert!(
            denied[0].reason.contains("allowed_mount_roots_rw"),
            "{}",
            denied[0].reason
        );

        // rw root covers the path + manifest readonly:false → granted rw.
        let (granted, denied) =
            resolve(&[declared(&s(&rw_root), false)], &[s(&rw_root)], &[s(&rw_root)], &[]);
        assert!(denied.is_empty());
        assert!(!granted[0].readonly);

        // rw root covers the path, manifest narrows to ro → granted ro.
        let (granted, denied) =
            resolve(&[declared(&s(&rw_root), true)], &[s(&rw_root)], &[s(&rw_root)], &[]);
        assert!(denied.is_empty());
        assert!(granted[0].readonly);
    }

    /// Denial reasons are agent-facing prose (#1163 review finding 4): no
    /// long whitespace runs from broken string continuations, no double
    /// spaces — every branch of the resolver is exercised and its reason
    /// printed-checked here, so a mangled literal fails a test instead of
    /// reaching the agent verbatim.
    #[test]
    fn denial_reasons_are_clean_prose() {
        let tmp = tmpdir();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let protected = tmp.path().join("runtime");
        std::fs::create_dir_all(&protected).unwrap();

        let cases: Vec<(&str, Vec<autonoetic_types::agent::DeclaredMount>, Vec<String>)> = vec![
            (
                "empty",
                vec![declared("  ", true)],
                vec![root.to_string_lossy().to_string()],
            ),
            (
                "nonexistent",
                vec![declared(
                    &tmp.path().join("missing").to_string_lossy(),
                    true,
                )],
                vec![tmp.path().to_string_lossy().to_string()],
            ),
            (
                "protected",
                vec![declared(&protected.to_string_lossy(), true)],
                vec![tmp.path().to_string_lossy().to_string()],
            ),
            (
                "outside_roots",
                vec![declared(
                    &tmp.path().join("elsewhere").to_string_lossy(),
                    true,
                )],
                vec![root.to_string_lossy().to_string()],
            ),
        ];
        // Create the dirs the outside/protected cases need.
        std::fs::create_dir_all(tmp.path().join("elsewhere")).unwrap();

        for (label, mounts, roots) in cases {
            let (_, denied) = resolve(&mounts, &roots, &[], &[protected.clone()]);
            assert_eq!(denied.len(), 1, "case {label}");
            let reason = &denied[0].reason;
            assert!(
                !reason.contains("  "),
                "case {label} reason has a double-space run: {reason:?}"
            );
            assert!(
                reason.chars().filter(|c| *c == ' ').count() <= reason.chars().count() / 4,
                "case {label} reason looks space-padded: {reason:?}"
            );
        }

        // The rw-ceiling branch (needs a real path under an ro root).
        let (granted, denied) = resolve(
            &[declared(&root.to_string_lossy(), false)],
            &[root.to_string_lossy().to_string()],
            &[],
            &[],
        );
        assert!(granted.is_empty());
        let reason = &denied[0].reason;
        assert!(
            !reason.contains("  "),
            "rw-ceiling reason has a double-space run: {reason:?}"
        );
        assert!(
            reason.contains("allowed_mount_roots_rw"),
            "rw-ceiling reason must name the ceiling: {reason:?}"
        );
    }

    /// An unresolvable allowed root surfaces in the denial reason — denials
    /// teach, including about the operator's own typos (#1163 review nit).
    #[test]
    fn unresolvable_root_is_surfaced_in_denials() {
        let tmp = tmpdir();
        let p = tmp.path().join("x");
        std::fs::create_dir_all(&p).unwrap();
        let bogus_root = tmp.path().join("typo-root");
        let (granted, denied) = resolve(&[declared(&s(&p), true)], &[s(&bogus_root)], &[], &[]);
        assert!(granted.is_empty());
        assert_eq!(denied.len(), 1);
        assert!(
            denied[0].reason.contains("could not be resolved"),
            "{}",
            denied[0].reason
        );
    }
}

