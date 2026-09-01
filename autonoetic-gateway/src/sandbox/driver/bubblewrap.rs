//! Bubblewrap driver — the default sandbox backend.
//!
//! Runs the entrypoint under `bwrap` with the host `/` ro-bound, the agent dir
//! bound as the workspace, and `--unshare-all` isolation. Gateway-internal
//! secrets reachable through that ro-bound `/` are masked per spawn (#1002).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::{DriverTier, SandboxDriver, SandboxDriverKind, SpawnSpec};
use crate::sandbox::{
    inject_pythonpath, inject_pythonpath_value, resolve_python_sdk_path, sandbox_config,
    sandbox_env_overrides_allowed, BwrapIsolationOverrides, SandboxMount, SdkBridgeWiring,
    ALLOW_SANDBOX_ENV_OVERRIDES_ENV, CCOS_SOCKET_ENV, PYTHONPATH_ENV,
};

/// Workspace path the agent dir is bound at inside the sandbox. Also the guest
/// workspace path the WASM tier mirrors, so input-file env vars built against it
/// resolve on both tiers.
pub const BWRAP_WORKSPACE_DIR: &str = "/tmp";

const BWRAP_SHARE_NET_ENV: &str = "AUTONOETIC_BWRAP_SHARE_NET";
const BWRAP_DEV_MODE_ENV: &str = "AUTONOETIC_BWRAP_DEV_MODE";

/// Additional host paths to mask inside every bubblewrap sandbox. Stopgap for
/// #1002: the host `/` is ro-bind-mounted, so without masking a sandboxed
/// process can read gateway-internal files. The gateway directory's sensitive
/// contents (vault key, session DB, identity key, sessions/, …) are ALWAYS
/// masked, using the gateway dir the engine threads in (see
/// [`bwrap_deny_path_flags`]).
/// This list is for the operator config file and any other paths the operator
/// chooses to add. Populated once at startup via
/// [`crate::sandbox::init_sandbox_host_deny_paths`].
static SANDBOX_HOST_DENY_PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// See [`crate::sandbox::init_sandbox_host_deny_paths`] for the operator-facing
/// contract; this is the storage behind it.
pub(crate) fn init_host_deny_paths(paths: Vec<PathBuf>) {
    let _ = SANDBOX_HOST_DENY_PATHS.set(normalize_deny_paths(&paths));
}

/// Canonicalized operator-registered deny paths, for overlap checks outside
/// this driver (#1002: a declared mount must not shadow any of these). Empty
/// when [`crate::sandbox::init_sandbox_host_deny_paths`] was never called.
pub fn canonical_host_deny_paths() -> Vec<PathBuf> {
    SANDBOX_HOST_DENY_PATHS
        .get()
        .map(|paths| {
            paths
                .iter()
                .filter_map(|p| std::fs::canonicalize(p).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
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

pub struct BubblewrapDriver;

impl SandboxDriver for BubblewrapDriver {
    fn kind(&self) -> SandboxDriverKind {
        SandboxDriverKind::Bubblewrap
    }

    fn names(&self) -> &'static [&'static str] {
        &["bubblewrap", "bwrap"]
    }

    fn tier(&self) -> DriverTier {
        DriverTier::Process
    }

    /// Offline iff `force_network_off` (the promotion gate sets it via
    /// [`BwrapIsolationOverrides::promotion_gate_overrides`]); enforced by
    /// `--unshare-all` with no `--share-net`.
    fn guarantees_network_off(&self, overrides: &BwrapIsolationOverrides) -> bool {
        overrides.force_network_off
    }

    fn sdk_socket_path(&self, socket_name: &str) -> Option<String> {
        Some(format!("{}/{}", BWRAP_WORKSPACE_DIR, socket_name))
    }

    /// The host `/` is ro-bound, so the SDK source is already visible; only the
    /// socket needs a bind mount. Env is inherited from the gateway process and
    /// applied in [`Self::apply_child_env`].
    fn wire_sdk_bridge(
        &self,
        host_socket: &Path,
        sandbox_socket: &str,
        wiring: &mut SdkBridgeWiring,
    ) {
        wiring.mounts.push(SandboxMount {
            source: host_socket.to_path_buf(),
            dest: sandbox_socket.to_string(),
            readonly: false,
        });
    }

    fn build_command(&self, spec: &SpawnSpec<'_>) -> anyhow::Result<(String, Vec<String>)> {
        bubblewrap_shell_command(
            spec.agent_dir,
            spec.gateway_dir,
            spec.entrypoint,
            spec.mounts,
            spec.overrides,
        )
    }

    /// Bubblewrap inherits the gateway env, so the SDK PYTHONPATH, socket path,
    /// and `extra_env` go on the `Command`.
    fn apply_child_env(
        &self,
        command: &mut Command,
        socket_path_sandbox: Option<&str>,
        extra_env: &[(String, String)],
    ) {
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
}

/// Shell form: run `shell_command` under `sh -c`, with extra bind mounts.
fn bubblewrap_shell_command(
    agent_dir: &str,
    gateway_dir: &Path,
    shell_command: &str,
    extra_mounts: &[SandboxMount],
    overrides: Option<&BwrapIsolationOverrides>,
) -> anyhow::Result<(String, Vec<String>)> {
    anyhow::ensure!(
        !shell_command.trim().is_empty(),
        "shell command must not be empty"
    );
    let mode = host_fs_mode(overrides);
    let mut argv = base_argv(agent_dir, mode);
    append_bwrap_isolation_flags(&mut argv, overrides);

    if mode == HostFsMode::Legacy {
        // Mask gateway-internal secrets + operator deny paths (stopgap for
        // #1002) BEFORE explicit content/SDK mounts so those can layer back on
        // top of the masked paths when needed. Skipped in AllowSet mode: the
        // masked destinations don't exist in the namespace (and bwrap errors
        // binding to a missing parent).
        argv.extend(bwrap_deny_path_flags(gateway_dir));
    }

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

/// Mount prologue shared by both command forms: ro-bind the host `/`, bind the
/// agent dir as the workspace, chdir into it.
/// Host-filesystem exposure for the bubblewrap tier (#1002 slice 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostFsMode {
    /// Blanket `--ro-bind / /` (deprecated; the secret mask is the stopgap).
    Legacy,
    /// Only the gateway-asserted allow-set: workspace, toolchain roots, SDK
    /// tree, session/declared mounts. Nothing else of the host exists.
    AllowSet,
}

fn host_fs_mode(overrides: Option<&BwrapIsolationOverrides>) -> HostFsMode {
    if overrides.map(|o| o.host_fs_allow_set).unwrap_or(false) {
        HostFsMode::AllowSet
    } else {
        HostFsMode::Legacy
    }
}

/// Toolchain roots bound read-only in [`HostFsMode::AllowSet`]. Candidates
/// that don't exist on this host are skipped; symlinked paths (/bin → usr/bin
/// on merged-/usr systems) are canonicalized as SOURCES but bound at their
/// ORIGINAL path so commands referencing either spelling resolve.
const ALLOW_SET_TOOLCHAIN_ROOTS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/sbin",
    "/etc/ld.so.cache",
];

/// Name-resolution files — needed whenever the sandbox may reach the network
/// (`--share-net`) and harmless otherwise (tiny ro file binds).
const ALLOW_SET_NAME_RESOLUTION: &[&str] = &["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"];

fn base_argv(agent_dir: &str, mode: HostFsMode) -> Vec<String> {
    let mut argv = Vec::new();
    match mode {
        HostFsMode::Legacy => {
            argv.push("--ro-bind".to_string());
            argv.push("/".to_string());
            argv.push("/".to_string());
        }
        HostFsMode::AllowSet => {
            // bwrap's default root is the HOST root, so "nothing visible"
            // requires an explicit empty root first; every bind below layers
            // on top of it. Ordered: empty root → toolchain → mounts.
            argv.push("--tmpfs".to_string());
            argv.push("/".to_string());
            argv.push("--proc".to_string());
            argv.push("/proc".to_string());
            for candidate in ALLOW_SET_TOOLCHAIN_ROOTS
                .iter()
                .chain(ALLOW_SET_NAME_RESOLUTION.iter())
            {
                let p = Path::new(candidate);
                if !p.exists() {
                    continue;
                }
                let source = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                argv.push("--ro-bind".to_string());
                argv.push(source.to_string_lossy().to_string());
                argv.push(candidate.to_string());
            }
            // The Python SDK tree (PYTHONPATH source, resolved by
            // `resolve_python_sdk_path` and injected in `apply_child_env`) —
            // bound AT its path, never the whole gateway dir, so secrets stay
            // out (#1174 review: without this, `import autonoetic_sdk` fails
            // under allow_set — the legacy ro-bind of `/` used to supply it).
            if let Some(sdk_path) = resolve_python_sdk_path() {
                let p = PathBuf::from(&sdk_path);
                if p.exists() {
                    let source = std::fs::canonicalize(&p).unwrap_or(p.clone());
                    argv.push("--ro-bind".to_string());
                    argv.push(source.to_string_lossy().to_string());
                    argv.push(sdk_path.clone());
                } else {
                    tracing::warn!(
                        target: "sandbox",
                        sdk_path = %sdk_path,
                        "PYTHONPATH SDK path does not exist on the host;                          `import autonoetic_sdk` may fail inside allow_set sandboxes"
                    );
                }
            }
        }
    }
    argv.push("--bind".to_string());
    argv.push(agent_dir.to_string());
    argv.push(BWRAP_WORKSPACE_DIR.to_string());
    argv.push("--chdir".to_string());
    argv.push(BWRAP_WORKSPACE_DIR.to_string());
    argv
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
        if let Some(val) =
            crate::sandbox::parse_env_bool(std::env::var(BWRAP_SHARE_NET_ENV).ok().as_deref())
        {
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
    sandbox_config().map(|c| c.share_net).unwrap_or(false)
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
    if let Some(config) = sandbox_config() {
        return parse_bwrap_dev_mode(Some(&config.dev_mode));
    }
    BwrapDevMode::Legacy
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
/// The revision store's top-level directory name inside the gateway dir. Named
/// here so the mask assertion below can reference the same constant the mask
/// itself uses, instead of re-spelling the layout.
pub(crate) const REVISIONS_DIR_NAME: &str = "revisions";

const BWRAP_GATEWAY_SENSITIVE_DIRS: &[&str] = &[
    "sessions",
    "scheduler",
    "checkpoints",
    "history",
    "logs",
    REVISIONS_DIR_NAME,
];

/// Push a bubblewrap flag that shadows a single host FILE with `/dev/null`
/// (reads return EOF), so the sandboxed process cannot read it. No-op when the
/// path doesn't exist on the host — bwrap resolves sources against the host
/// filesystem, so the dest must resolve against the ro-mounted `/` too.
fn push_deny_file(flags: &mut Vec<String>, p: &Path) {
    if p.exists() {
        flags.push("--ro-bind".to_string());
        flags.push("/dev/null".to_string());
        flags.push(p.to_string_lossy().to_string());
    }
}

/// Push a bubblewrap flag that shadows a host DIRECTORY with an empty tmpfs,
/// so the sandboxed process cannot read or list it. No-op when the path
/// doesn't exist on the host.
fn push_deny_dir(flags: &mut Vec<String>, p: &Path) {
    if p.exists() {
        flags.push("--tmpfs".to_string());
        flags.push(p.to_string_lossy().to_string());
    }
}

/// Build the bubblewrap argv slice that masks gateway-internal secrets and any
/// operator-registered deny paths, so a sandboxed process cannot read them
/// through the ro-mounted host `/` (stopgap for #1002). Its `sdk/` subtree is
/// intentionally left accessible (the sandbox reads its PYTHONPATH from that
/// host path). Operator paths come from
/// [`crate::sandbox::init_sandbox_host_deny_paths`].
///
/// `gateway_dir` is the authoritative one the execution engine resolved, passed
/// down via [`SpawnSpec`]. It used to be derived here as
/// `agent_dir.parent().join(".gateway")`, which assumed `agent_dir` was an
/// ingest-dir child. In production it is the *revision* dir
/// (`<gateway_dir>/revisions/agents/<id>/<rev>/`), so the derivation resolved to
/// a path that does not exist — and since [`push_deny_file`]/[`push_deny_dir`]
/// skip non-existent paths, the whole mask silently emitted **zero flags**,
/// leaving `vault.key`, `vault.enc.json`, `gateway.db`, the Ed25519 identity key
/// and every session transcript readable from inside the sandbox (#1145).
fn bwrap_deny_path_flags(gateway_dir: &Path) -> Vec<String> {
    let mut flags = Vec::new();

    for name in BWRAP_GATEWAY_SENSITIVE_FILES {
        push_deny_file(&mut flags, &gateway_dir.join(name));
    }
    for name in BWRAP_GATEWAY_SENSITIVE_DIRS {
        push_deny_dir(&mut flags, &gateway_dir.join(name));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bubblewrap_shell_command_shape() {
        // Empty gateway dir: no secrets to mask, so argv positions are stable.
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        let gateway_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let (_bin, argv) = bubblewrap_shell_command(&agent_dir_str, &gateway_dir, "echo hi", &[], None)
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

    /// The driver's `build_command` must produce exactly the shell form — the
    /// trait seam is a dispatch change, not a behavior change.
    #[test]
    fn driver_build_command_matches_shell_builder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        let gateway_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let wiring = SdkBridgeWiring::default();
        let (program, argv) = BubblewrapDriver
            .build_command(&SpawnSpec {
                agent_dir: &agent_dir_str,
                gateway_dir: &gateway_dir,
                entrypoint: "echo hi",
                mounts: &[],
                overrides: None,
                extra_env: &[],
                bridge: &wiring,
            })
            .expect("driver should build the command");
        let (expected_program, expected_argv) =
            bubblewrap_shell_command(&agent_dir_str, &gateway_dir, "echo hi", &[], None)
                .expect("shell command");
        assert_eq!(program, expected_program);
        assert_eq!(argv, expected_argv);
    }

    /// Stopgap for #1002: the bubblewrap sandbox masks gateway-internal secrets
    /// (vault key, session DB, identity key, sessions/, …) so a sandboxed
    /// process cannot read them through the ro-mounted host `/`. The SDK
    /// subtree (`sdk/`) must stay visible because PYTHONPATH points there.
    ///
    /// The fixture puts `agent_dir` where production puts it — *inside* the
    /// revision store — because that is what broke this mask for real (#1145).
    /// The pre-fix code derived the gateway dir as `agent_dir.parent()/.gateway`,
    /// which from a revision dir resolves to nothing and silently produced an
    /// empty flag list. The old version of this test passed a
    /// `<agents_root>/demo.agent` path instead, so it asserted the mask worked
    /// in a layout the runtime never supplies.
    #[test]
    fn bwrap_deny_flags_mask_gateway_secrets_but_not_sdk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let gateway_dir = tmp.path().join("runtime");
        let agent_dir =
            crate::agent::agent_revision_dir(&gateway_dir, "demo.agent", "rev-abc123");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(gateway_dir.join("sdk")).unwrap();
        std::fs::create_dir_all(gateway_dir.join("sessions")).unwrap();
        std::fs::write(gateway_dir.join("vault.key"), "secret").unwrap();
        std::fs::write(gateway_dir.join("vault.enc.json"), "blob").unwrap();
        std::fs::write(gateway_dir.join("gateway.db"), "db").unwrap();
        std::fs::write(gateway_dir.join("state_attestation.ed25519"), "k").unwrap();

        let flags = bwrap_deny_path_flags(&gateway_dir);
        let joined = flags.join(" ");

        // The regression this test exists for: a non-empty mask.
        assert!(
            !flags.is_empty(),
            "the mask must emit flags for a revision-shaped agent_dir (#1145)"
        );

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
        assert!(
            joined.contains(&format!(
                "--ro-bind /dev/null {}",
                gateway_dir.join("vault.enc.json").display()
            )),
            "the encrypted vault blob must be masked, got: {joined}"
        );
        // Sensitive dirs are shadowed with an empty tmpfs.
        assert!(
            joined.contains(&format!("--tmpfs {}", gateway_dir.join("sessions").display())),
            "sessions/ must be masked, got: {joined}"
        );
        // The revision store: an agent runs from its own revision (bound in as
        // the workspace before these flags apply) but must not be able to browse
        // every other agent's promoted code on the host path.
        assert!(
            joined.contains(&format!(
                "--tmpfs {}",
                gateway_dir.join(REVISIONS_DIR_NAME).display()
            )),
            "revisions/ must be masked, got: {joined}"
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
        // A gateway dir that exists but holds none of the sensitive entries.
        let gateway_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let flags = bwrap_deny_path_flags(&gateway_dir);
        assert!(flags.is_empty(), "expected no deny flags, got: {flags:?}");
    }

    /// #1002 slice 4: allow-set mode mounts the gateway-asserted set — no
    /// blanket `--ro-bind / /`, no deny flags (nothing to mask: the host
    /// root isn't there), toolchain roots bound read-only.
    #[test]
    fn bubblewrap_allow_set_mode_argv_shape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let gw = tmp.path().join("runtime");
        std::fs::create_dir_all(&gw).unwrap();

        let overrides = BwrapIsolationOverrides {
            share_net: false,
            force_network_off: false,
            host_fs_allow_set: true,
        };
        let (_bin, argv) = bubblewrap_shell_command(
            agent_dir.to_str().unwrap(),
            &gw,
            "echo hi",
            &[],
            Some(&overrides),
        )
        .expect("allow-set command builds");

        assert!(
            !argv.windows(2).any(|w| w[0] == "--ro-bind" && w[1] == "/"),
            "allow-set mode must not ro-bind the host root: {argv:?}"
        );
        // Workspace bind present.
        let workspace_pos = argv.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(argv[workspace_pos + 2], "/tmp");
        // The deny mask is absent in allow-set mode: the mask mechanism is
        // `--ro-bind /dev/null <masked-path>` or `--tmpfs <masked-path>` —
        // and the only --tmpfs is the empty root at `/` itself (#1002 slice 4
        // found this the hard way: bwrap's default root is the HOST).
        assert!(!argv.iter().any(|a| a == "/dev/null"), "no /dev/null mask: {argv:?}");
        assert!(
            !argv.iter().any(|a| !a.starts_with("--") && a.contains("vault")),
            "no vault mask in allow-set mode: {argv:?}"
        );
        // Toolchain root binds exist (host-dependent, but /usr is universal).
        let usr = argv
            .windows(2)
            .any(|w| w[0] == "--ro-bind" && w[1].ends_with("/usr"));
        assert!(usr, "toolchain roots must be bound in allow-set mode: {argv:?}");
    }

    /// Legacy mode keeps the blanket bind: the deny mask can only shadow what
    /// is mounted, which is exactly the pre-#1002 contract.
    #[test]
    fn bubblewrap_legacy_mode_keeps_blanket_root_bind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let gw = tmp.path().join("runtime");
        std::fs::create_dir_all(&gw).unwrap();

        let overrides = BwrapIsolationOverrides {
            share_net: false,
            force_network_off: false,
            host_fs_allow_set: false,
        };
        let (_bin, argv) = bubblewrap_shell_command(
            agent_dir.to_str().unwrap(),
            &gw,
            "echo hi",
            &[],
            Some(&overrides),
        )
        .expect("legacy command builds");
        assert!(
            argv.windows(2).any(|w| w[0] == "--ro-bind" && w[1] == "/"),
            "legacy mode must keep the blanket host-root bind: {argv:?}"
        );
    }

    /// The full shell-command builder inserts the deny slice between the
    /// isolation flags and the `--` separator (and before explicit re-expose
    /// mounts, so they can overlay masked paths).
    #[test]
    fn bubblewrap_shell_command_includes_deny_slice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let gateway_dir = tmp.path().join("runtime");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&gateway_dir).unwrap();
        std::fs::write(gateway_dir.join("vault.key"), "secret").unwrap();

        let (_bin, argv) = bubblewrap_shell_command(
            agent_dir.to_str().unwrap(),
            &gateway_dir,
            "echo hi",
            &[],
            None,
        )
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
        assert_eq!(unique.len(), out.len(), "no duplicate entries, got: {out:?}");
        assert!(out.contains(&p));
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
}
