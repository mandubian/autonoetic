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
    let mut provisioned_roots: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut bound_dests: Vec<String> = Vec::new();
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
        // Provision the destination before binding — bwrap must be able to
        // mkdir the mount target inside the namespace, or the whole sandbox
        // dies at setup before any command runs (observed as
        // `bwrap: Can't mkdir /opt/wrapper-out: Read-only file system` under
        // legacy's `--ro-bind / /`, killing even `echo ok`).
        let provisioning =
            mount_destination_flags(mode, &mount.dest, &bound_dests, &mut provisioned_roots)?;
        argv.extend(provisioning);
        let bind_flag = if mount.readonly {
            "--ro-bind".to_string()
        } else {
            "--bind".to_string()
        };
        argv.push(bind_flag);
        argv.push(mount.source.to_string_lossy().to_string());
        argv.push(mount.dest.clone());
        bound_dests.push(mount.dest.clone());
    }

    argv.extend(vec![
        "--".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        shell_command.to_string(),
    ]);
    Ok(("bwrap".to_string(), argv))
}

/// Host directories the legacy mode may replace with a tmpfs in order to
/// create a *missing* mount destination. Conventional add-on roots only:
/// provisioning replaces the directory inside the namespace, and while
/// [`legacy_provisioning_flags`] re-binds the entries that were there, that
/// re-bind resolves symlinks into real entries — acceptable for `/opt`-shaped
/// directories, not for `/etc` or `/var`, whose symlink webs commands depend
/// on. Anything else refuses and names `allow_set`.
const LEGACY_PROVISIONABLE_ROOTS: &[&str] = &["/opt", "/mnt", "/srv", "/media"];

/// Upper bound on the entries re-bound onto a provisioning tmpfs. A directory
/// with more than this is not a mount root, it is a system directory: refuse
/// instead of emitting hundreds of binds.
const LEGACY_PROVISION_MAX_ENTRIES: usize = 64;

/// Provision a mount destination so bwrap can create it inside the namespace.
///
/// Fail-loud with a teaching message rather than letting bwrap die at setup:
/// a sandbox that cannot assemble its mounts fails *every* command (including
/// `echo ok`), and the raw bwrap stderr gives the agent no lawful next move.
///
/// The rule is "can bwrap mkdir this destination?", and the answer turns on
/// whether the destination *already exists*:
///
/// - Workspace subtree: nothing. The workspace bind is a writable host
///   directory, so bwrap can mkdir freely.
/// - Destination already present in the namespace: nothing. bwrap binds *over*
///   an existing entry without any mkdir — including inside a read-only bind
///   (verified: `--ro-bind /usr /usr --ro-bind layer /usr/share` mounts fine).
///   This is why an existing destination is never refused: it worked before
///   this function existed, and still does.
/// - Missing destination inside a gateway-asserted read-only bind: refusal.
///   bwrap must mkdir there and cannot (`Can't mkdir /usr/nope: Read-only file
///   system`).
/// - Missing destination, `AllowSet`: nothing. The root is an empty writable
///   tmpfs, so bwrap creates every ancestor itself.
/// - Missing destination, `Legacy`: `/` is ro-bound, so the deepest *existing*
///   ancestor is replaced by a tmpfs — the immediate parent is not enough
///   (`--tmpfs /opt/foo` dies with the same `Can't mkdir` when `/opt/foo` is
///   itself missing). Only [`LEGACY_PROVISIONABLE_ROOTS`] may be replaced, and
///   the entries that were there are re-bound read-only so the tmpfs does not
///   silently empty the directory. Everything else refuses and names the flip.
fn mount_destination_flags(
    mode: HostFsMode,
    dest: &str,
    bound_dests: &[String],
    provisioned_roots: &mut std::collections::HashSet<String>,
) -> anyhow::Result<Vec<String>> {
    let workspace = BWRAP_WORKSPACE_DIR;
    if dest == workspace || dest.starts_with(&format!("{workspace}/")) {
        return Ok(Vec::new());
    }

    // Overlap with an earlier mount of this exec, in *either* direction: a
    // later bind onto the same or a containing destination does not fail — it
    // silently shadows the earlier layer, which is worse than refusing
    // (verified: `--ro-bind A /opt/x --ro-bind B /opt/x` leaves only B).
    if let Some(earlier) = bound_dests.iter().find(|d| paths_overlap(dest, d)) {
        anyhow::bail!(
            "sandbox mount destination '{dest}' overlaps the earlier mount '{earlier}' \
             in this exec — the later bind would silently hide the earlier one. \
             Give each mount_as an independent path"
        );
    }

    let dest_exists = std::path::Path::new(dest).exists();

    // Gateway-asserted read-only binds. Binding over an entry that is already
    // there needs no mkdir and is fine; creating a new one inside them cannot
    // work in either mode.
    if let Some(root) = asserted_ro_bind_containing(dest) {
        if dest_exists {
            return Ok(Vec::new());
        }
        anyhow::bail!(
            "sandbox mount destination '{dest}' does not exist and would have to be \
             created inside the read-only bind '{root}' (bwrap: Can't mkdir). \
             Choose a mount_as outside {ALLOW_SET_TOOLCHAIN_ROOTS:?}"
        );
    }

    match mode {
        HostFsMode::AllowSet => Ok(Vec::new()),
        HostFsMode::Legacy if dest_exists => Ok(Vec::new()),
        HostFsMode::Legacy => legacy_provisioning_flags(dest, bound_dests, provisioned_roots),
    }
}

/// True when `a` and `b` name the same path or one contains the other.
fn paths_overlap(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// The gateway-asserted read-only bind containing `dest`, if any.
fn asserted_ro_bind_containing(dest: &str) -> Option<&'static str> {
    ALLOW_SET_TOOLCHAIN_ROOTS
        .iter()
        .chain(ALLOW_SET_NAME_RESOLUTION.iter())
        .find(|root| dest == **root || dest.starts_with(&format!("{root}/")))
        .copied()
}

/// `--tmpfs` the deepest existing ancestor of a missing legacy destination,
/// re-binding what was there so nothing silently disappears.
fn legacy_provisioning_flags(
    dest: &str,
    bound_dests: &[String],
    provisioned_roots: &mut std::collections::HashSet<String>,
) -> anyhow::Result<Vec<String>> {
    let mut ancestor = std::path::Path::new(dest).parent();
    while let Some(candidate) = ancestor {
        if candidate.exists() {
            break;
        }
        ancestor = candidate.parent();
    }
    let Some(root) = ancestor.map(|a| a.to_string_lossy().to_string()) else {
        anyhow::bail!(
            "sandbox mount destination '{dest}' has no existing ancestor directory on the \
             host; use an absolute mount_as under {LEGACY_PROVISIONABLE_ROOTS:?} or set \
             sandbox.host_fs: allow_set"
        );
    };

    if !LEGACY_PROVISIONABLE_ROOTS.contains(&root.as_str()) {
        anyhow::bail!(
            "sandbox mount destination '{dest}' cannot be created under the deprecated \
             whole-host ro-bind: the gateway would have to replace '{root}' with a tmpfs, \
             which it only does for {LEGACY_PROVISIONABLE_ROOTS:?}. Use a mount_as under \
             one of those, or one that already exists on the host, or set \
             sandbox.host_fs: allow_set (the default), where any destination is creatable"
        );
    }

    // Already provisioned for an earlier mount: bwrap can mkdir on that tmpfs.
    if provisioned_roots.contains(&root) {
        return Ok(Vec::new());
    }
    // A tmpfs over `root` is mounted *after* any earlier bind beneath it, so it
    // would bury that mount. Refuse instead of silently hiding it.
    if let Some(earlier) = bound_dests
        .iter()
        .find(|d| **d == root || d.starts_with(&format!("{root}/")))
    {
        anyhow::bail!(
            "sandbox mount destination '{dest}' needs '{root}' replaced with a tmpfs, \
             which would hide the earlier mount '{earlier}'. Order the mounts so the \
             deepest destination comes first, or set sandbox.host_fs: allow_set"
        );
    }

    // Preserve what the host had there: the tmpfs shadows `root` inside the
    // namespace, so every entry that was visible is re-bound read-only (which
    // is what the whole-host ro-bind gave it anyway).
    let mut entries: Vec<String> = match std::fs::read_dir(&root) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path().to_string_lossy().to_string())
            .collect(),
        Err(e) => anyhow::bail!(
            "sandbox mount destination '{dest}' needs '{root}' provisioned, but it \
             cannot be read ({e}); set sandbox.host_fs: allow_set"
        ),
    };
    if entries.len() > LEGACY_PROVISION_MAX_ENTRIES {
        anyhow::bail!(
            "sandbox mount destination '{dest}' needs '{root}' replaced with a tmpfs, but \
             '{root}' holds {} entries (limit {LEGACY_PROVISION_MAX_ENTRIES}) that would \
             have to be re-bound; set sandbox.host_fs: allow_set",
            entries.len()
        );
    }
    entries.sort();

    provisioned_roots.insert(root.clone());
    let mut flags = vec!["--tmpfs".to_string(), root];
    for entry in entries {
        flags.push("--ro-bind".to_string());
        flags.push(entry.clone());
        flags.push(entry);
    }
    Ok(flags)
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

    /// The provisionable root that exists on this host, or `None` (a build
    /// container may have neither `/opt` nor `/mnt`), in which case the
    /// provisioning tests have nothing lawful to provision and skip.
    fn host_provisionable_root() -> Option<&'static str> {
        LEGACY_PROVISIONABLE_ROOTS
            .iter()
            .copied()
            .find(|root| std::path::Path::new(root).exists())
    }

    fn host_entries(root: &str) -> Vec<String> {
        let mut entries: Vec<String> = std::fs::read_dir(root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        entries
    }

    #[test]
    fn legacy_mode_provisions_tmpfs_parent_for_layer_mount() {
        // The adapter-session failure: a layer with mount_as /opt/wrapper-out
        // under legacy's `--ro-bind / /` used to die at setup with
        // `bwrap: Can't mkdir /opt/wrapper-out: Read-only file system`,
        // failing every command including `echo ok`.
        let Some(root) = host_provisionable_root() else {
            return;
        };
        if host_entries(root).len() > LEGACY_PROVISION_MAX_ENTRIES {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let gateway_dir = tmp.path().join("runtime");
        let dest = format!("{root}/autonoetic-wrapper-out-test");
        let mounts = vec![SandboxMount {
            source: tmp.path().join("layer"),
            dest: dest.clone(),
            readonly: true,
        }];
        let (_bin, argv) = bubblewrap_shell_command(
            &agent_dir_str,
            &gateway_dir,
            "echo ok",
            &mounts,
            None, // legacy
        )
        .expect("shell command should build");
        let tmpfs_pos = argv
            .iter()
            .position(|a| a == "--tmpfs")
            .expect("tmpfs provisioning expected");
        assert_eq!(argv[tmpfs_pos + 1], root);
        let bind_pos = argv
            .windows(3)
            .position(|w| w[0] == "--ro-bind" && w[2] == dest)
            .expect("layer bind expected");
        assert!(
            tmpfs_pos < bind_pos,
            "tmpfs parent must precede the bind onto it"
        );
        // Nothing the host had there may silently disappear behind the tmpfs.
        for entry in host_entries(root) {
            let restored = argv
                .windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == entry && w[2] == entry);
            assert!(restored, "entry {entry} must be re-bound over the tmpfs: {argv:?}");
        }
    }

    #[test]
    fn legacy_mode_provisions_the_deepest_existing_ancestor() {
        // `--tmpfs <immediate parent>` is not enough: bwrap has to mkdir that
        // parent too, and cannot inside the ro-bound root — verified as
        // `bwrap: Can't mkdir /opt/foo: Read-only file system`. The tmpfs has
        // to land on the deepest ancestor that actually exists.
        let Some(root) = host_provisionable_root() else {
            return;
        };
        if host_entries(root).len() > LEGACY_PROVISION_MAX_ENTRIES {
            return;
        }
        let mut provisioned = std::collections::HashSet::new();
        let flags = mount_destination_flags(
            HostFsMode::Legacy,
            &format!("{root}/autonoetic-missing-a/autonoetic-missing-b"),
            &[],
            &mut provisioned,
        )
        .expect("deep destination must be provisionable");
        assert_eq!(flags[0], "--tmpfs");
        assert_eq!(flags[1], root, "tmpfs must land on the existing ancestor");
    }

    #[test]
    fn legacy_mode_refuses_to_replace_a_system_directory() {
        // `--tmpfs /etc` empties /etc inside the sandbox (verified: no passwd
        // db, no resolv.conf, no CA certs) — a silently broken environment in
        // exchange for one mount. Refuse and name the flip instead.
        let mut provisioned = std::collections::HashSet::new();
        let err = mount_destination_flags(
            HostFsMode::Legacy,
            "/etc/autonoetic-layer-test",
            &[],
            &mut provisioned,
        )
        .expect_err("/etc must not be replaced with a tmpfs");
        assert!(err.to_string().contains("allow_set"), "{err}");
    }

    #[test]
    fn existing_destination_needs_no_provisioning_and_is_never_refused() {
        // bwrap binds *over* an existing entry with no mkdir, including inside
        // a read-only bind (verified: `--ro-bind /usr /usr --ro-bind layer
        // /usr/share` mounts fine). This worked before the provisioning logic
        // existed and must keep working — including under /usr.
        for dest in ["/usr/bin", "/etc"] {
            for mode in [HostFsMode::Legacy, HostFsMode::AllowSet] {
                let mut provisioned = std::collections::HashSet::new();
                let flags = mount_destination_flags(mode, dest, &[], &mut provisioned)
                    .unwrap_or_else(|e| panic!("existing dest {dest} must be accepted: {e}"));
                assert!(flags.is_empty(), "no provisioning for existing {dest}");
            }
        }
    }

    #[test]
    fn missing_destination_inside_a_readonly_bind_is_refused() {
        for mode in [HostFsMode::Legacy, HostFsMode::AllowSet] {
            let mut provisioned = std::collections::HashSet::new();
            let err = mount_destination_flags(
                mode,
                "/usr/autonoetic-missing-xyz",
                &[],
                &mut provisioned,
            )
            .expect_err("missing dest inside a ro bind must refuse");
            assert!(err.to_string().contains("Can't mkdir"), "{err}");
        }
    }

    #[test]
    fn overlapping_mount_destinations_are_refused_in_both_directions() {
        // Verified: a second bind onto the same dest leaves only the second
        // layer visible, and binding a *parent* after a child buries the child.
        // Neither errors in bwrap, so the refusal has to happen here.
        for (dest, earlier) in [
            ("/opt/x", "/opt/x"),          // exact duplicate
            ("/opt/a", "/opt/a/b"),        // earlier mount nested under this one
            ("/opt/a/b/c", "/opt/a"),      // this one nested under an earlier mount
        ] {
            let mut provisioned = std::collections::HashSet::new();
            let err = mount_destination_flags(
                HostFsMode::AllowSet,
                dest,
                &[earlier.to_string()],
                &mut provisioned,
            )
            .expect_err("overlapping destinations must refuse");
            let msg = err.to_string();
            assert!(
                msg.contains(earlier),
                "the refusal must name the conflicting earlier mount '{earlier}': {msg}"
            );
        }
    }

    #[test]
    fn legacy_provisioning_refuses_to_bury_an_earlier_mount() {
        let Some(root) = host_provisionable_root() else {
            return;
        };
        let mut provisioned = std::collections::HashSet::new();
        let err = mount_destination_flags(
            HostFsMode::Legacy,
            &format!("{root}/autonoetic-second"),
            &[format!("{root}/autonoetic-first")],
            &mut provisioned,
        )
        .expect_err("a tmpfs that would hide an earlier mount must refuse");
        assert!(err.to_string().contains("autonoetic-first"), "{err}");
    }

    #[test]
    fn legacy_mode_provisions_each_parent_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let gateway_dir = tmp.path().join("runtime");
        let Some(root) = host_provisionable_root() else {
            return;
        };
        if host_entries(root).len() > LEGACY_PROVISION_MAX_ENTRIES {
            return;
        }
        let mounts = vec![
            SandboxMount {
                source: tmp.path().join("l1"),
                dest: format!("{root}/autonoetic-a"),
                readonly: true,
            },
            SandboxMount {
                source: tmp.path().join("l2"),
                dest: format!("{root}/autonoetic-b"),
                readonly: true,
            },
        ];
        let (_bin, argv) =
            bubblewrap_shell_command(&agent_dir_str, &gateway_dir, "echo ok", &mounts, None)
                .expect("shell command should build");
        let count = argv.iter().filter(|a| *a == "--tmpfs").count();
        assert_eq!(count, 1, "shared parent provisioned exactly once: {argv:?}");
    }

    #[test]
    fn legacy_mode_refuses_root_level_mount_destination() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let gateway_dir = tmp.path().join("runtime");
        let mounts = vec![SandboxMount {
            source: tmp.path().join("layer"),
            dest: "/wrapper-out".to_string(),
            readonly: true,
        }];
        let err = bubblewrap_shell_command(
            &agent_dir_str,
            &gateway_dir,
            "echo ok",
            &mounts,
            None,
        )
        .expect_err("root-level mount_as must be refused in legacy mode");
        assert!(err.to_string().contains("allow_set"), "{err}");
    }

    #[test]
    fn allow_set_mode_needs_no_provisioning_for_fresh_dest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let agent_dir_str = agent_dir.to_str().unwrap().to_string();
        let gateway_dir = tmp.path().join("runtime");
        let mounts = vec![SandboxMount {
            source: tmp.path().join("layer"),
            dest: "/opt/wrapper-out".to_string(),
            readonly: true,
        }];
        let overrides = BwrapIsolationOverrides {
            host_fs_allow_set: true,
            ..Default::default()
        };
        let (_bin, argv) = bubblewrap_shell_command(
            &agent_dir_str,
            &gateway_dir,
            "echo ok",
            &mounts,
            Some(&overrides),
        )
        .expect("shell command should build");
        // allow_set's base argv starts with its own `--tmpfs /`; the only
        // tmpfs present must be that root, never a per-mount provisioning.
        let tmpfs_parents: Vec<&String> = argv
            .windows(2)
            .filter(|w| w[0] == "--tmpfs")
            .map(|w| &w[1])
            .collect();
        assert_eq!(
            tmpfs_parents,
            vec!["/"],
            "only the allow_set root tmpfs expected: {argv:?}"
        );
    }

    #[test]
    fn mount_destination_refuses_new_dirs_under_a_toolchain_root() {
        let mut provisioned = std::collections::HashSet::new();
        for mode in [HostFsMode::Legacy, HostFsMode::AllowSet] {
            let err = mount_destination_flags(
                mode,
                "/usr/share/autonoetic-layer-xyz",
                &[],
                &mut provisioned,
            )
            .expect_err("a new directory under a toolchain root must refuse");
            let msg = err.to_string();
            assert!(msg.contains("/usr"), "{msg}");
            assert!(msg.contains("read-only bind"), "{msg}");
        }
    }

    #[test]
    fn mount_destination_allows_workspace_subtree() {
        let mut provisioned = std::collections::HashSet::new();
        for mode in [HostFsMode::Legacy, HostFsMode::AllowSet] {
            let flags = mount_destination_flags(
                mode,
                "/tmp/autonoetic_content/session/file.txt",
                &[],
                &mut provisioned,
            )
            .expect("workspace subtree needs no provisioning");
            assert!(flags.is_empty());
        }
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
