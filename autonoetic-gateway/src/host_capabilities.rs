//! Host capability preflight.
//!
//! The gateway runs agents on whichever sandbox tier each agent's manifest
//! declares (`runtime.sandbox`: bubblewrap / docker / microvm / wasm) and in
//! whichever language the agent ships. Most tiers and language toolchains need a
//! host tool present on `PATH` (`bwrap`, `docker`, `javy`, …); the wasm tier is
//! in-process and instead needs the `wasm-tier` build feature.
//!
//! This module probes what the host can actually run so the operator learns
//! about a missing toolchain at gateway start (or on demand via the preflight
//! command) rather than at the first agent spawn that needs it. Probing is
//! read-only: it scans `PATH` for an executable, it does not run anything.

/// Whether an executable named `tool` exists on `PATH`.
///
/// Read-only and cheap: scans `PATH` entries for a file with the execute bit,
/// without spawning the tool (so it can't have side effects or hang).
pub fn tool_on_path(tool: &str) -> bool {
    cached_path_dirs()
        .iter()
        .any(|dir| is_executable(&dir.join(tool)))
}

/// Process-lifetime cache of the `PATH` directory list (#591). `PATH` does not
/// change during a gateway run, so we split it once instead of on every
/// `tool_on_path` check. Only the directory list is cached; executable presence
/// is still probed per call so a tool installed mid-run is still discovered.
fn cached_path_dirs() -> &'static [std::path::PathBuf] {
    static PATH_DIRS: std::sync::OnceLock<Vec<std::path::PathBuf>> = std::sync::OnceLock::new();
    PATH_DIRS.get_or_init(|| {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default()
    })
}

/// `tool_on_path` against an explicit `PATH` value — the testable core, so tests
/// never mutate the process-global `PATH` (which would race the rest of the suite).
#[cfg(test)]
fn tool_on_given_path(path: Option<&std::ffi::OsStr>, tool: &str) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| is_executable(&dir.join(tool)))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Whether this build embeds the in-process wasm tier (`wasm-tier` feature).
pub const fn wasm_tier_built() -> bool {
    cfg!(feature = "wasm-tier")
}

/// Convenience probes used across the codebase and tests.
pub fn is_bwrap_available() -> bool {
    tool_on_path("bwrap")
}
pub fn is_docker_available() -> bool {
    tool_on_path("docker")
}
/// Javy is the JS→wasm compiler used to bundle JavaScript agents for the wasm tier.
pub fn is_javy_available() -> bool {
    tool_on_path("javy")
}

/// One probed capability: a sandbox tier or a language toolchain, the host
/// requirement it depends on, and whether that requirement is satisfied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Capability {
    /// What the capability enables (e.g. `"sandbox: docker"`, `"language: javascript (wasm via javy)"`).
    pub name: String,
    /// The host requirement it depends on (e.g. `"docker on PATH"`, `"wasm-tier build feature"`).
    pub requirement: String,
    /// Whether the requirement is satisfied on this host/build.
    pub available: bool,
}

/// A snapshot of what this host+build can run: sandbox tiers and language toolchains.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostCapabilities {
    pub sandbox_tiers: Vec<Capability>,
    pub languages: Vec<Capability>,
}

impl HostCapabilities {
    /// Probe the host and the build features for every supported tier/toolchain.
    pub fn gather() -> Self {
        let sandbox_tiers = vec![
            Capability {
                name: "sandbox: bubblewrap".into(),
                requirement: "bwrap on PATH".into(),
                available: is_bwrap_available(),
            },
            Capability {
                name: "sandbox: docker".into(),
                requirement: "docker on PATH".into(),
                available: is_docker_available(),
            },
            Capability {
                name: "sandbox: microvm".into(),
                requirement: "firecracker on PATH".into(),
                available: tool_on_path("firecracker"),
            },
            Capability {
                name: "sandbox: wasm".into(),
                requirement: "wasm-tier build feature".into(),
                available: wasm_tier_built(),
            },
        ];
        let languages = vec![
            Capability {
                name: "language: python".into(),
                requirement: "python3 on PATH".into(),
                available: tool_on_path("python3"),
            },
            Capability {
                name: "language: javascript (wasm via javy)".into(),
                requirement: "javy on PATH".into(),
                available: is_javy_available(),
            },
            Capability {
                name: "language: javascript (process via node)".into(),
                requirement: "node on PATH".into(),
                available: tool_on_path("node"),
            },
        ];
        Self {
            sandbox_tiers,
            languages,
        }
    }

    /// True when at least one execution tier is runnable — otherwise the gateway
    /// can host no agents at all.
    pub fn has_any_sandbox_tier(&self) -> bool {
        self.sandbox_tiers.iter().any(|c| c.available)
    }

    /// Human-readable lines for logging at startup or printing on demand.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push("Host capabilities — sandbox tiers:".to_string());
        for c in &self.sandbox_tiers {
            lines.push(format!("  {} {} ({})", mark(c.available), c.name, c.requirement));
        }
        lines.push("Host capabilities — language toolchains:".to_string());
        for c in &self.languages {
            lines.push(format!("  {} {} ({})", mark(c.available), c.name, c.requirement));
        }
        lines
    }
}

fn mark(available: bool) -> &'static str {
    if available {
        "[ok]"
    } else {
        "[--]"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn tool_on_path_finds_an_executable_and_ignores_non_exec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().as_os_str();
        let plain = dir.path().join("toolx");
        std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            !tool_on_given_path(Some(path), "toolx"),
            "non-executable file must not count"
        );
        assert!(!tool_on_given_path(Some(path), "does-not-exist"));
        assert!(!tool_on_given_path(None, "toolx"), "no PATH → nothing found");

        // Now make it executable → found.
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            tool_on_given_path(Some(path), "toolx"),
            "executable on PATH must be found"
        );
    }

    #[test]
    fn gather_reports_all_tiers_and_languages_with_stable_shape() {
        let caps = HostCapabilities::gather();
        assert_eq!(caps.sandbox_tiers.len(), 4);
        assert_eq!(caps.languages.len(), 3);
        // The wasm tier's availability is decided by the build feature, not PATH.
        let wasm = caps
            .sandbox_tiers
            .iter()
            .find(|c| c.name.contains("wasm"))
            .unwrap();
        assert_eq!(wasm.available, cfg!(feature = "wasm-tier"));
        // Summary covers every probed capability plus the two section headers.
        assert_eq!(caps.summary_lines().len(), 4 + 3 + 2);
    }
}
