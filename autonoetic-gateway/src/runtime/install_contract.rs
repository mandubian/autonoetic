//! Install Contract Helpers — canonical defaults, scaffolding, and diagnostics.
//!
//! This module is the single source of truth for:
//! - Gateway-owned canonical defaults (runtime engine, versions, sandbox)
//! - Runtime lock scaffolding (gateway/sdk/sandbox fields filled deterministically)
//! - Install validation helpers (shape checking before typed deserialization)
//! - Diagnostic examples (canonical SKILL.md and runtime.lock examples)
//!
//! Design rule: This is deterministic infrastructure, not a policy engine.
//! No inference from code meaning, no guessing agent intent.

use autonoetic_types::agent::{AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::layer::ArtifactLayer;
use autonoetic_types::runtime_lock::{
    LockedArtifact, LockedCredentialMount, LockedDependencySet, LockedGateway, LockedLayerMount,
    LockedSandbox, LockedSdk, RuntimeLock,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

// ─── Canonical defaults ────────────────────────────────────────

pub const DEFAULT_ENGINE: &str = "autonoetic";
pub const DEFAULT_RUNTIME_TYPE: &str = "stateful";
pub const DEFAULT_SANDBOX: &str = "bubblewrap";
pub const DEFAULT_RUNTIME_LOCK_FILENAME: &str = "runtime.lock";
pub const PLACEHOLDER_SHA: &str = "replace-me";

/// SHA-256 source fingerprint of this gateway build, computed from version + git commit
/// at compile time by build.rs.
pub const GATEWAY_BUILD_SHA256: &str = env!("GATEWAY_BUILD_SHA256");

/// Human-readable build tag (e.g. "0.1.0+a1b2c3d4e5f6").
pub const GATEWAY_BUILD_TAG: &str = env!("GATEWAY_BUILD_TAG");

pub fn gateway_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn sdk_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

pub fn default_gateway_artifact() -> String {
    "marketplace://gateway/autonoetic-gateway".to_string()
}

pub fn running_binary_sha256() -> anyhow::Result<String> {
    let exe_path = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("Failed to resolve current gateway executable path: {}", e))?;

    let bytes = match std::fs::read(&exe_path) {
        Ok(b) => b,
        Err(_) => {
            // On Linux, the original binary may have been deleted (e.g., after cargo rebuild).
            // Try reading via /proc/self/exe which remains valid even after unlink.
            let proc_path = std::path::PathBuf::from("/proc/self/exe");
            std::fs::read(&proc_path).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to read gateway executable (tried '{}' and '/proc/self/exe'): {}",
                    exe_path.display(),
                    e
                )
            })?
        }
    };
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

// ─── RuntimeDeclaration helpers ────────────────────────────────

pub fn default_runtime_declaration() -> RuntimeDeclaration {
    RuntimeDeclaration {
        engine: DEFAULT_ENGINE.to_string(),
        gateway_version: gateway_version(),
        sdk_version: sdk_version(),
        runtime_type: DEFAULT_RUNTIME_TYPE.to_string(),
        sandbox: DEFAULT_SANDBOX.to_string(),
        runtime_lock: DEFAULT_RUNTIME_LOCK_FILENAME.to_string(),
    }
}

// ─── RuntimeLock scaffolding ───────────────────────────────────

pub fn scaffold_runtime_lock(
    agent_dependencies: Option<Vec<LockedDependencySet>>,
    agent_artifacts: Option<Vec<LockedArtifact>>,
    artifact_layers: &[ArtifactLayer],
) -> anyhow::Result<RuntimeLock> {
    scaffold_runtime_lock_with_scopes(agent_dependencies, agent_artifacts, artifact_layers, None, None)
}

/// Like `scaffold_runtime_lock` but also populates `approval_scope` on each layer
/// by reading the layer manifests from the given gateway directory.
pub fn scaffold_runtime_lock_with_scopes(
    agent_dependencies: Option<Vec<LockedDependencySet>>,
    agent_artifacts: Option<Vec<LockedArtifact>>,
    artifact_layers: &[ArtifactLayer],
    gateway_dir: Option<&std::path::Path>,
    credential_services: Option<Vec<String>>,
) -> anyhow::Result<RuntimeLock> {
    Ok(RuntimeLock {
        gateway: LockedGateway {
            artifact: default_gateway_artifact(),
            version: gateway_version(),
            sha256: GATEWAY_BUILD_SHA256.to_string(),
            binary_sha256: Some(running_binary_sha256()?),
            build_tag: Some(GATEWAY_BUILD_TAG.to_string()),
            signature: None,
        },
        sdk: LockedSdk {
            version: sdk_version(),
        },
        sandbox: LockedSandbox {
            backend: DEFAULT_SANDBOX.to_string(),
        },
        dependencies: agent_dependencies.unwrap_or_default(),
        artifacts: agent_artifacts.unwrap_or_default(),
        layers: artifact_layers
            .iter()
            .map(|l| -> anyhow::Result<LockedLayerMount> {
                let approval_scope = match gateway_dir {
                    Some(gw_dir) => {
                        let manifest_path = gw_dir
                            .join("layers")
                            .join(&l.layer_id)
                            .join("manifest.json");
                        let content = std::fs::read_to_string(&manifest_path).map_err(|e| {
                            anyhow::anyhow!(
                                "failed to read layer manifest for layer '{}' at '{}': {}",
                                l.layer_id,
                                manifest_path.display(),
                                e
                            )
                        })?;
                        let manifest: autonoetic_types::layer::LayerManifest =
                            serde_json::from_str(&content).map_err(|e| {
                                anyhow::anyhow!(
                                    "failed to parse layer manifest for layer '{}' at '{}': {}",
                                    l.layer_id,
                                    manifest_path.display(),
                                    e
                                )
                            })?;
                        manifest.approval_scope
                    }
                    None => None,
                };
                Ok(LockedLayerMount {
                    layer_id: l.layer_id.clone(),
                    digest: l.digest.clone(),
                    mount_path: l.mount_path.clone(),
                    approval_scope,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        credentials: credential_services
            .unwrap_or_default()
            .into_iter()
            .map(|service| LockedCredentialMount { service, credential_id: None })
            .collect(),
    })
}

pub fn default_runtime_lock(artifact_layers: &[ArtifactLayer]) -> anyhow::Result<RuntimeLock> {
    scaffold_runtime_lock(None, None, artifact_layers)
}

// ─── Example rendering for diagnostics ─────────────────────────

pub fn render_skill_metadata_example() -> String {
    r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "my.agent"
  name: "My Agent"
  description: "What this agent does"
llm_preset: smart
capabilities:
  - type: "ReadAccess"
    scopes: ["*"]
  - type: "WriteAccess"
    scopes: ["self.*"]
execution_mode: "reasoning"
---
# Your agent instructions go here in markdown.
"#
    .to_string()
}

pub fn render_runtime_lock_example() -> String {
    format!(
        r#"gateway:
  artifact: "marketplace://gateway/autonoetic-gateway"
  version: "{version}"
  sha256: "{sha}"
  build_tag: "{build_tag}"
sdk:
  version: "{version}"
sandbox:
  backend: "bubblewrap"
dependencies: []
artifacts: []
layers: []
credentials: []
"#,
        version = gateway_version(),
        sha = GATEWAY_BUILD_SHA256,
        build_tag = GATEWAY_BUILD_TAG,
    )
}

pub fn render_skill_document(
    manifest: &AgentManifest,
    instructions: &str,
) -> anyhow::Result<String> {
    #[derive(serde::Serialize)]
    struct MetadataWrapper<'a> {
        autonoetic: &'a AgentManifest,
    }
    #[derive(serde::Serialize)]
    struct SkillFrontmatter<'a> {
        name: &'a str,
        description: &'a str,
        metadata: MetadataWrapper<'a>,
    }

    let name = if manifest.agent.name.is_empty() {
        &manifest.agent.id
    } else {
        &manifest.agent.name
    };
    let wrapper = SkillFrontmatter {
        name,
        description: &manifest.agent.description,
        metadata: MetadataWrapper {
            autonoetic: manifest,
        },
    };

    let mut frontmatter = serde_yaml::to_string(&wrapper).map_err(|e| {
        anyhow::anyhow!("Failed to serialize canonical SKILL.md frontmatter: {}", e)
    })?;
    if let Some(stripped) = frontmatter.strip_prefix("---\n") {
        frontmatter = stripped.to_string();
    }
    if !frontmatter.ends_with('\n') {
        frontmatter.push('\n');
    }
    let body = instructions.trim();
    Ok(format!("---\n{}---\n\n{}\n", frontmatter, body))
}

pub fn render_runtime_lock_document(lock: &RuntimeLock) -> anyhow::Result<String> {
    let mut doc = serde_yaml::to_string(lock)
        .map_err(|e| anyhow::anyhow!("Failed to serialize canonical runtime.lock: {}", e))?;
    if let Some(stripped) = doc.strip_prefix("---\n") {
        doc = stripped.to_string();
    }
    if !doc.ends_with('\n') {
        doc.push('\n');
    }
    Ok(doc)
}

// ─── Schema description for agent.revision.schema tool ─────────

pub fn install_schema_description() -> String {
    r#"# Agent Install Contract

## Ownership Split

**Agent-owned (free-form):**
- Markdown body of SKILL.md (instructions, role, workflow notes)

**Agent-provided (semantic intent):**
- agent.id, description, execution_mode, script_entry, llm_preset (+ optional llm_overrides), capabilities
- Optional: io (including io.output_policy), middleware

**Gateway-owned (canonicalized):**
- SKILL.md metadata shape and field types
- runtime.engine, runtime.gateway_version, runtime.sdk_version
- Final canonical SKILL.md metadata serialization
- Final canonical runtime.lock serialization

**Runtime lock — Gateway-owned (autofilled):**
- gateway (artifact, version, source sha256, binary_sha256, build_tag)
- sdk (version)
- sandbox (backend)
- artifacts, layers (from artifact)

**Runtime lock — Agent-provided intent:**
- dependencies (package hints)
- artifacts (optional agent-provided artifact references)

## Accepted SKILL.md Shapes

Two frontmatter shapes are accepted:

**Top-level Autonoetic shape:**
- `runtime` (object, required)
  - `engine`, `gateway_version`, `sdk_version`, `type`, `runtime_lock`
- `agent` (object, required)
  - `id` (required), `name`, `description`

**Metadata-wrapped shape (AgentSkills-compatible):**
- `name` (string, required)
- `description` (string, required)
- `metadata.autonoetic.runtime` (object, required)
  - `engine`, `gateway_version`, `sdk_version`, `type`, `runtime_lock`
- `metadata.autonoetic.agent` (object, required)
  - `id` (required), `name`, `description`

## runtime.lock — What You Need to Provide

**Agent-owned (optional):**
- `dependencies` (array) — package hints like `[{runtime: "python3", packages: ["pip"]}]`
- `artifacts` (array) — agent-provided artifact references

**Gateway-autofilled (do not hand-author):**
- `gateway`, `sdk`, `sandbox`, `layers` — filled automatically by the gateway during install
- If you provide a partial `runtime.lock`, missing gateway-owned sections are scaffolded

## Guidance
- Use `artifact_id` to pass file bundles to agent.revision.create
- The gateway fills gateway/sdk/sandbox/layers automatically
- You only need to provide agent identity and intent
- For a minimal agent bundle, a `runtime.lock` with `dependencies: []` and `artifacts: []` is sufficient
"#
    .to_string()
}

// ─── Validation helpers for agent.revision.create ──────────────

#[derive(Debug, Default, serde::Deserialize)]
pub struct RuntimeLockPartial {
    #[serde(default)]
    pub dependencies: Option<Vec<serde_yaml::Value>>,
    #[serde(default)]
    pub artifacts: Option<Vec<serde_yaml::Value>>,
}

pub fn validate_runtime_lock_shape(yaml: &serde_yaml::Value) -> Vec<String> {
    let mut missing = Vec::new();

    let obj = yaml.as_mapping();
    let Some(obj) = obj else {
        return vec!["runtime.lock must be a YAML mapping".to_string()];
    };

    let deps_key = serde_yaml::Value::String("dependencies".into());
    if let Some(deps) = obj.get(&deps_key) {
        if !deps.is_sequence() && !deps.is_null() {
            missing.push("dependencies (must be a sequence)".to_string());
        } else if let Some(deps_arr) = deps.as_sequence() {
            for (i, dep) in deps_arr.iter().enumerate() {
                if dep.as_mapping().is_none() {
                    missing.push(format!("dependencies[{}] must be a mapping", i));
                } else {
                    let dm = dep.as_mapping().unwrap();
                    let runtime_key = serde_yaml::Value::String("runtime".into());
                    if dm.get(&runtime_key).is_none() {
                        missing.push(format!("dependencies[{}].runtime", i));
                    }
                }
            }
        }
    }

    let arts_key = serde_yaml::Value::String("artifacts".into());
    if let Some(arts) = obj.get(&arts_key) {
        if !arts.is_sequence() && !arts.is_null() {
            missing.push("artifacts (must be a sequence)".to_string());
        } else if let Some(arts_arr) = arts.as_sequence() {
            for (i, art) in arts_arr.iter().enumerate() {
                if art.as_mapping().is_none() {
                    missing.push(format!("artifacts[{}] must be a mapping", i));
                }
            }
        }
    }

    missing
}

pub fn validate_skill_frontmatter_shape(frontmatter: &serde_yaml::Value) -> Vec<String> {
    let mut missing = Vec::new();

    let obj = frontmatter.as_mapping();
    let Some(obj) = obj else {
        return vec!["SKILL.md frontmatter must be a YAML mapping".to_string()];
    };

    let type_key = serde_yaml::Value::String("type".into());
    let runtime_lock_key = serde_yaml::Value::String("runtime_lock".into());
    let id_key = serde_yaml::Value::String("id".into());

    let (runtime_map, agent_map, used_metadata_path) = if obj
        .get(&serde_yaml::Value::String("runtime".into()))
        .is_some()
    {
        let rt = obj
            .get(&serde_yaml::Value::String("runtime".into()))
            .unwrap();
        let ag = obj.get(&serde_yaml::Value::String("agent".into()));
        (rt.as_mapping(), ag.and_then(|v| v.as_mapping()), false)
    } else if let Some(meta) = obj
        .get(&serde_yaml::Value::String("metadata".into()))
        .and_then(|v| v.as_mapping())
    {
        let autonoetic = meta
            .get(&serde_yaml::Value::String("autonoetic".into()))
            .and_then(|v| v.as_mapping());
        if let Some(auto) = autonoetic {
            (
                auto.get(&serde_yaml::Value::String("runtime".into()))
                    .and_then(|v| v.as_mapping()),
                auto.get(&serde_yaml::Value::String("agent".into()))
                    .and_then(|v| v.as_mapping()),
                true,
            )
        } else {
            missing.push("metadata.autonoetic".to_string());
            (None, None, true)
        }
    } else {
        missing.push("runtime (or metadata.autonoetic.runtime)".to_string());
        (None, None, false)
    };

    if used_metadata_path {
        let name_key = serde_yaml::Value::String("name".into());
        let desc_key = serde_yaml::Value::String("description".into());
        if obj.get(&name_key).is_none() {
            missing.push("name".to_string());
        } else if !obj
            .get(&name_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("name (must be a non-empty string)".to_string());
        }
        if obj.get(&desc_key).is_none() {
            missing.push("description".to_string());
        } else if !obj
            .get(&desc_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("description (must be a non-empty string)".to_string());
        }
    }

    if let Some(rt) = runtime_map {
        let engine_key = serde_yaml::Value::String("engine".into());
        let gw_ver_key = serde_yaml::Value::String("gateway_version".into());
        let sdk_ver_key = serde_yaml::Value::String("sdk_version".into());
        if rt.get(&engine_key).is_none() {
            missing.push("runtime.engine".to_string());
        } else if !rt
            .get(&engine_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("runtime.engine (must be a non-empty string)".to_string());
        }
        if rt.get(&gw_ver_key).is_none() {
            missing.push("runtime.gateway_version".to_string());
        } else if !rt
            .get(&gw_ver_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("runtime.gateway_version (must be a non-empty string)".to_string());
        }
        if rt.get(&sdk_ver_key).is_none() {
            missing.push("runtime.sdk_version".to_string());
        } else if !rt
            .get(&sdk_ver_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("runtime.sdk_version (must be a non-empty string)".to_string());
        }
        if rt.get(&type_key).is_none() {
            missing.push("runtime.type".to_string());
        } else if !rt
            .get(&type_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("runtime.type (must be a non-empty string)".to_string());
        }
        if rt.get(&runtime_lock_key).is_none() {
            missing.push("runtime.runtime_lock".to_string());
        } else if !rt
            .get(&runtime_lock_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("runtime.runtime_lock (must be a non-empty string)".to_string());
        }
    } else if !missing.iter().any(|m| m.contains("runtime")) {
        missing.push("runtime (must be a mapping)".to_string());
    }

    if let Some(ag) = agent_map {
        if ag.get(&id_key).is_none() {
            missing.push("agent.id".to_string());
        } else if !ag
            .get(&id_key)
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            missing.push("agent.id (must be a non-empty string)".to_string());
        }
    } else if obj
        .get(&serde_yaml::Value::String("agent".into()))
        .is_some()
    {
        // agent key exists but is not a mapping
        missing.push("agent (must be a mapping)".to_string());
    } else if !missing.iter().any(|m| m == "metadata.autonoetic") {
        missing.push("agent (or metadata.autonoetic.agent)".to_string());
    }

    missing
}

pub fn extract_frontmatter_raw(content: &str) -> anyhow::Result<serde_yaml::Value> {
    use gray_matter::{engine::YAML, Matter};
    let matter = Matter::<YAML>::new();
    let parsed = matter
        .parse(content)
        .map_err(|e| anyhow::anyhow!("Failed to parse SKILL.md frontmatter: {}", e))?;

    let pod: gray_matter::Pod = parsed
        .data
        .ok_or_else(|| anyhow::anyhow!("No YAML frontmatter found in SKILL.md"))?;

    let value: serde_yaml::Value = pod
        .deserialize::<serde_yaml::Value>()
        .map_err(|e| anyhow::anyhow!("Failed to deserialize frontmatter: {}", e))?;

    Ok(value)
}

const DEPENDENCY_FILES: &[(&str, &str)] = &[
    ("requirements.txt", "python/pip"),
    ("pyproject.toml", "python"),
    ("package.json", "node/npm"),
    ("go.mod", "go"),
    ("Cargo.toml", "rust/cargo"),
    ("Gemfile", "ruby/bundler"),
];

/// Python 3.11 standard library top-level module names (deduplicated).
/// Used by detect_external_python_imports to distinguish third-party from stdlib.
const PYTHON_STDLIB: &[&str] = &[
    // Core builtins & runtime
    "_thread",
    "atexit",
    "builtins",
    "code",
    "codeop",
    "compileall",
    "copyreg",
    "dis",
    "ensurepip",
    "errno",
    "faulthandler",
    "gc",
    "graphlib",
    "importlib",
    "inspect",
    "modulefinder",
    "pkgutil",
    "platform",
    "py_compile",
    "pydoc",
    "reprlib",
    "runpy",
    "site",
    "test",
    "venv",
    "warnings",
    "zipapp",
    "zipimport",
    // Data structures & types
    "array",
    "bisect",
    "calendar",
    "collections",
    "contextlib",
    "copy",
    "dataclasses",
    "datetime",
    "decimal",
    "enum",
    "fractions",
    "functools",
    "heapq",
    "io",
    "itertools",
    "math",
    "numbers",
    "operator",
    "pprint",
    "queue",
    "random",
    "statistics",
    "string",
    "struct",
    "textwrap",
    "types",
    "typing",
    "uuid",
    "weakref",
    // Text, encoding & data formats
    "base64",
    "binascii",
    "codecs",
    "csv",
    "difflib",
    "gettext",
    "hashlib",
    "hmac",
    "json",
    "locale",
    "pickle",
    "re",
    "secrets",
    "shlex",
    "shelve",
    "unicodedata",
    // Files & OS
    "configparser",
    "ctypes",
    "curses",
    "fcntl",
    "filecmp",
    "fileinput",
    "genericpath",
    "glob",
    "grp",
    "mmap",
    "netrc",
    "nis",
    "os",
    "pathlib",
    "pipes",
    "posix",
    "posixpath",
    "pty",
    "pwd",
    "readline",
    "resource",
    "rlcompleter",
    "shutil",
    "signal",
    "stat",
    "sys",
    "syslog",
    "tarfile",
    "tempfile",
    "termios",
    "tty",
    "zipfile",
    // Concurrency
    "asyncio",
    "concurrent",
    "multiprocessing",
    "select",
    "selectors",
    "subprocess",
    "threading",
    // Network & web
    "cgi",
    "cgitb",
    "email",
    "ftplib",
    "html",
    "http",
    "imaplib",
    "ipaddress",
    "nntplib",
    "poplib",
    "smtpd",
    "smtplib",
    "socket",
    "socketserver",
    "ssl",
    "telnetlib",
    "urllib",
    "webbrowser",
    "wsgiref",
    "xml",
    "xmlrpc",
    // Databases
    "sqlite3",
    // Dev, debug & testing
    "abc",
    "argparse",
    "ast",
    "bdb",
    "cProfile",
    "doctest",
    "formatter",
    "getopt",
    "logging",
    "optparse",
    "pdb",
    "profile",
    "pstats",
    "timeit",
    "traceback",
    "unittest",
    // Media & misc
    "audioop",
    "cmath",
    "chunk",
    "colorsys",
    "imghdr",
    "sndhdr",
    "wave",
    // Other common stdlib
    "time",
    "copy",
    // Gateway-provided SDK (injected via PYTHONPATH; not available on PyPI)
    "autonoetic_sdk",
];

#[derive(Debug, Default)]
pub struct BundleHealthReport {
    pub dependency_files: Vec<String>,
    pub has_unresolved_dependencies: bool,
    pub detected_external_imports: Vec<String>,
    pub declares_network_access: bool,
    pub declares_code_execution: bool,
    pub warnings: Vec<String>,
}

/// Run a full bundle health check for an agent being installed.
///
/// `script_entry`: when `Some(path)`, import scanning is limited to that file only,
/// avoiding false positives from test helpers in the bundle. Pass `None` to scan all Python files.
pub fn analyze_bundle_health(
    file_map: &BTreeMap<String, Vec<u8>>,
    capabilities: &[Capability],
    has_layers: bool,
    script_entry: Option<&str>,
) -> BundleHealthReport {
    let mut report = BundleHealthReport::default();

    let found_dep_files: Vec<(&str, &str)> = DEPENDENCY_FILES
        .iter()
        .filter(|(f, _)| file_map.contains_key(*f))
        .copied()
        .collect();

    report.dependency_files = found_dep_files.iter().map(|(f, _)| f.to_string()).collect();
    report.has_unresolved_dependencies = !found_dep_files.is_empty() && !has_layers;

    if report.has_unresolved_dependencies {
        report.warnings.push(format!(
            "Dependency files found ({}) but no layers in artifact. \
             Run packager.default to install dependencies as layers before evaluation.",
            found_dep_files
                .iter()
                .map(|(f, eco)| format!("{f} ({eco})"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let external_imports = detect_external_python_imports(file_map, script_entry);
    report.detected_external_imports = external_imports.clone();
    if !external_imports.is_empty() && !has_layers {
        report.warnings.push(format!(
            "External Python imports detected: {}. \
             These modules are not in the standard library and no dependency layers are present.",
            external_imports.join(", ")
        ));
    }

    for cap in capabilities {
        match cap {
            Capability::NetworkAccess { .. } => report.declares_network_access = true,
            Capability::CodeExecution { .. } | Capability::ArtifactExecution => {
                report.declares_code_execution = true
            }
            _ => {}
        }
    }

    report
}

/// Scan Python files for external (non-stdlib, non-local) imports.
///
/// When `script_entry` is `Some(filename)`, only that file is scanned to avoid
/// false positives from test files and dev tooling bundled alongside the agent.
/// Pass `None` to scan all `.py` files.
pub fn detect_external_python_imports(
    file_map: &BTreeMap<String, Vec<u8>>,
    script_entry: Option<&str>,
) -> Vec<String> {
    let mut external = BTreeSet::new();
    for (path, content) in file_map {
        if !path.ends_with(".py") {
            continue;
        }
        // When script_entry is given, limit scanning to that file only
        if let Some(entry) = script_entry {
            if path != entry {
                continue;
            }
        }
        let text = String::from_utf8_lossy(content);
        for line in text.lines() {
            let trimmed = line.trim();
            let module = if trimmed.starts_with("import ") {
                trimmed
                    .strip_prefix("import ")
                    .and_then(|s| s.split_whitespace().next())
            } else if trimmed.starts_with("from ") {
                trimmed
                    .strip_prefix("from ")
                    .and_then(|s| s.split_whitespace().next())
            } else {
                None
            };
            if let Some(module) = module {
                let top_level = module.split('.').next().unwrap_or(module);
                if top_level.is_empty() {
                    continue;
                }
                if PYTHON_STDLIB.contains(&top_level) {
                    continue;
                }
                let local_file = format!("{top_level}.py");
                if file_map.contains_key(&local_file) {
                    continue;
                }
                external.insert(top_level.to_string());
            }
        }
    }
    external.into_iter().collect()
}

pub fn is_high_risk_capability(cap: &Capability) -> bool {
    matches!(
        cap,
        Capability::NetworkAccess { .. }
            | Capability::CodeExecution { .. }
            | Capability::ArtifactExecution
            | Capability::AgentSpawn { .. }
    )
}

/// Capabilities that require a reviewed artifact (code execution boundary).
/// Used to gate artifact-free reasoning agents: if any capability requires artifact
/// review, the agent MUST be created with an artifact_id for full eval + audit.
pub fn requires_artifact_review(cap: &Capability) -> bool {
    matches!(
        cap,
        Capability::CodeExecution { .. }
            | Capability::ArtifactExecution
            | Capability::AgentSpawn { .. }
    )
}

pub fn format_install_validation_error(
    skill_missing: &[String],
    lock_missing: Option<&[String]>,
    parse_error: Option<&str>,
) -> String {
    let mut msg = String::from("Install validation failed:\n");

    if !skill_missing.is_empty() {
        msg.push_str("\nSKILL.md issues:\n");
        for path in skill_missing {
            msg.push_str(&format!("  - Missing or invalid: `{}`\n", path));
        }
    }

    if let Some(lm) = lock_missing {
        if !lm.is_empty() {
            msg.push_str("\nruntime.lock issues:\n");
            for path in lm {
                msg.push_str(&format!("  - Missing or invalid: `{}`\n", path));
            }
        }
    }

    if let Some(err) = parse_error {
        msg.push_str(&format!("\nParse error: {}\n", err));
    }

    msg.push_str("\nCanonical examples:\n");
    msg.push_str(&format!(
        "\n--- SKILL.md example ---\n{}\n",
        render_skill_metadata_example()
    ));
    msg.push_str(&format!(
        "--- runtime.lock example ---\n{}\n",
        render_runtime_lock_example()
    ));
    msg.push_str("\nUse agent.revision.schema for full schema documentation.\n");

    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_runtime_declaration() {
        let rt = default_runtime_declaration();
        assert_eq!(rt.engine, "autonoetic");
        assert_eq!(rt.runtime_type, "stateful");
        assert_eq!(rt.sandbox, "bubblewrap");
        assert_eq!(rt.runtime_lock, "runtime.lock");
    }

    #[test]
    fn test_scaffold_runtime_lock_no_layers() {
        let lock = scaffold_runtime_lock(None, None, &[]).expect("runtime lock scaffold");
        assert_eq!(
            lock.gateway.artifact,
            "marketplace://gateway/autonoetic-gateway"
        );
        assert_eq!(lock.gateway.sha256, GATEWAY_BUILD_SHA256);
        assert!(
            lock.gateway
                .binary_sha256
                .as_deref()
                .unwrap_or_default()
                .starts_with("sha256:"),
            "binary_sha256 should be populated"
        );
        assert_eq!(
            lock.gateway.build_tag.as_deref(),
            Some(GATEWAY_BUILD_TAG),
            "build_tag should be populated from build metadata"
        );
        assert_eq!(lock.sandbox.backend, "bubblewrap");
        assert!(lock.dependencies.is_empty());
        assert!(lock.layers.is_empty());
    }

    #[test]
    fn test_scaffold_runtime_lock_with_layers() {
        let layers = vec![ArtifactLayer {
            layer_id: "layer_abc".into(),
            name: "python-deps".into(),
            mount_path: "/deps".into(),
            digest: "sha256:xyz".into(),
        }];
        let lock = scaffold_runtime_lock(None, None, &layers).expect("runtime lock scaffold");
        assert_eq!(lock.layers.len(), 1);
        assert_eq!(lock.layers[0].layer_id, "layer_abc");
        assert_eq!(lock.layers[0].mount_path, "/deps");
    }

    #[test]
    fn test_validate_runtime_lock_shape_valid() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
gateway:
  artifact: "marketplace://gw"
  version: "0.1.0"
  sha256: "abc"
sdk:
  version: "0.1.0"
sandbox:
  backend: "bubblewrap"
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_runtime_lock_shape_bad_dependency() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
dependencies:
  - "not_a_mapping"
  - runtime: "python3"
    packages: ["pip"]
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert_eq!(missing, vec!["dependencies[0] must be a mapping"]);
    }

    #[test]
    fn test_validate_runtime_lock_shape_missing_dep_runtime() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
dependencies:
  - packages: ["pip"]
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert_eq!(missing, vec!["dependencies[0].runtime"]);
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_valid() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  runtime_lock: "runtime.lock"
agent:
  id: "my.agent"
  name: "My Agent"
  description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_missing_agent() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  runtime_lock: "runtime.lock"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert_eq!(missing, vec!["agent (or metadata.autonoetic.agent)"]);
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_missing_agent_id() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  runtime_lock: "runtime.lock"
agent:
  name: "My Agent"
  description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert_eq!(missing, vec!["agent.id"]);
    }

    #[test]
    fn test_extract_frontmatter_raw_valid() {
        let content = r#"---
name: "test.agent"
runtime:
  engine: "autonoetic"
---
# Body
"#;
        let value = extract_frontmatter_raw(content).unwrap();
        assert!(value.get("name").is_some());
        assert!(value.get("runtime").is_some());
    }

    #[test]
    fn test_extract_frontmatter_raw_no_frontmatter() {
        let content = "# Just markdown, no frontmatter";
        let result = extract_frontmatter_raw(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_format_install_validation_error() {
        let skill_missing = vec!["agent.id".to_string()];
        let lock_missing = vec!["gateway.sha256".to_string()];
        let msg = format_install_validation_error(&skill_missing, Some(&lock_missing), None);
        assert!(msg.contains("SKILL.md issues"));
        assert!(msg.contains("agent.id"));
        assert!(msg.contains("runtime.lock issues"));
        assert!(msg.contains("gateway.sha256"));
        assert!(msg.contains("SKILL.md example"));
        assert!(msg.contains("runtime.lock example"));
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_metadata_wrapped_missing_name() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
description: "A skill"
metadata:
  autonoetic:
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.agent"
      name: "My Agent"
      description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.contains(&"name".to_string()));
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_metadata_wrapped_missing_description() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
name: "my.agent"
metadata:
  autonoetic:
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.agent"
      name: "My Agent"
      description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.contains(&"description".to_string()));
    }

    #[test]
    fn test_validate_skill_frontmatter_shape_metadata_wrapped_valid() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
name: "my.agent"
description: "A skill"
metadata:
  autonoetic:
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      runtime_lock: "runtime.lock"
    agent:
      id: "my.agent"
      name: "My Agent"
      description: "Desc"
"#,
        )
        .unwrap();
        let missing = validate_skill_frontmatter_shape(&yaml);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_validate_runtime_lock_shape_dependencies_wrong_type() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
dependencies: "not_a_sequence"
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert!(missing.contains(&"dependencies (must be a sequence)".to_string()));
    }

    #[test]
    fn test_validate_runtime_lock_shape_artifacts_wrong_type() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
artifacts: "not_a_sequence"
"#,
        )
        .unwrap();
        let missing = validate_runtime_lock_shape(&yaml);
        assert!(missing.contains(&"artifacts (must be a sequence)".to_string()));
    }

    #[test]
    fn test_schema_description_mentions_both_shapes() {
        let desc = install_schema_description();
        assert!(desc.contains("Top-level Autonoetic shape"));
        assert!(desc.contains("Metadata-wrapped shape"));
        assert!(desc.contains("Gateway-autofilled"));
        assert!(desc.contains("dependencies"));
        assert!(!desc.contains("`gateway` (object, required)"));
    }

    #[test]
    fn test_render_skill_document_round_trip() {
        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: default_runtime_declaration(),
            agent: autonoetic_types::agent::AgentIdentity {
                id: "roundtrip.agent".to_string(),
                name: "Roundtrip Agent".to_string(),
                description: "Round trip".to_string(),
            singleton: false,
        },
            capabilities: vec![],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: autonoetic_types::agent::ExecutionMode::Reasoning,
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };
        let rendered = render_skill_document(&manifest, "# Instructions").unwrap();
        assert!(rendered.starts_with("---\n"));
        assert!(
            rendered.contains("name: Roundtrip Agent\n"),
            "missing top-level name, got:\n{rendered}"
        );
        assert!(
            rendered.contains("metadata:\n  autonoetic:\n"),
            "missing metadata.autonoetic wrapper, got:\n{rendered}"
        );
        assert!(
            rendered.contains("id: roundtrip.agent"),
            "missing agent id, got:\n{rendered}"
        );
        assert!(rendered.contains("# Instructions"));
    }

    #[test]
    fn test_detect_external_python_imports_finds_requests() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "weather_agent.py".to_string(),
            b"import requests\nimport json\ndef fetch(): pass\n".to_vec(),
        );
        let external = detect_external_python_imports(&file_map, None);
        assert!(external.contains(&"requests".to_string()));
        assert!(!external.contains(&"json".to_string()));
    }

    #[test]
    fn test_detect_external_python_imports_ignores_stdlib() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "agent.py".to_string(),
            b"import os\nimport sys\nfrom pathlib import Path\n".to_vec(),
        );
        let external = detect_external_python_imports(&file_map, None);
        assert!(external.is_empty());
    }

    #[test]
    fn test_detect_external_python_imports_ignores_local_modules() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "main.py".to_string(),
            b"import mymodule\nfrom utils import helper\n".to_vec(),
        );
        file_map.insert("mymodule.py".to_string(), b"# local module\n".to_vec());
        file_map.insert("utils.py".to_string(), b"# local module\n".to_vec());
        let external = detect_external_python_imports(&file_map, None);
        assert!(external.is_empty());
    }

    #[test]
    fn test_detect_external_python_imports_ignores_autonoetic_sdk() {
        let mut file_map = BTreeMap::new();
        file_map.insert(
            "agent.py".to_string(),
            b"import os\nfrom autonoetic_sdk import load_invocation\n".to_vec(),
        );
        let external = detect_external_python_imports(&file_map, None);
        assert!(!external.contains(&"autonoetic_sdk".to_string()));
    }

    #[test]
    fn test_analyze_bundle_health_warns_on_requirements_without_layers() {
        let mut file_map = BTreeMap::new();
        file_map.insert("requirements.txt".to_string(), b"requests\n".to_vec());
        let report = analyze_bundle_health(&file_map, &[], false, None);
        assert!(report.has_unresolved_dependencies);
        assert!(report
            .dependency_files
            .contains(&"requirements.txt".to_string()));
        assert!(!report.warnings.is_empty());
    }

    #[test]
    fn test_analyze_bundle_health_no_warnings_when_layers_present() {
        let mut file_map = BTreeMap::new();
        file_map.insert("requirements.txt".to_string(), b"requests\n".to_vec());
        let _layers = vec![autonoetic_types::layer::ArtifactLayer {
            layer_id: "layer1".to_string(),
            name: "pip".to_string(),
            mount_path: "/deps".to_string(),
            digest: "sha256:abc".to_string(),
        }];
        let report = analyze_bundle_health(&file_map, &[], true, None);
        assert!(!report.has_unresolved_dependencies);
        assert!(report
            .warnings
            .iter()
            .all(|w| !w.contains("Dependency files found")));
    }

    #[test]
    fn test_analyze_bundle_health_detects_high_risk_capabilities() {
        let file_map = BTreeMap::new();
        let caps = vec![
            Capability::NetworkAccess {
                hosts: vec!["api.example.com".to_string()],
            },
            Capability::CodeExecution {
                patterns: vec!["python*".to_string()],
                commands: vec![],
            },
        ];
        let report = analyze_bundle_health(&file_map, &caps, false, None);
        assert!(report.declares_network_access);
        assert!(report.declares_code_execution);
    }

    #[test]
    fn test_is_high_risk_capability() {
        assert!(is_high_risk_capability(&Capability::NetworkAccess {
            hosts: vec!["*".to_string()]
        }));
        assert!(is_high_risk_capability(&Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }));
        assert!(is_high_risk_capability(&Capability::AgentSpawn {
            max_children: 10,
            max_spawn_depth: 0
        }));
        assert!(!is_high_risk_capability(&Capability::ReadAccess {
            scopes: vec!["*".to_string()]
        }));
    }

    #[test]
    fn test_gateway_build_sha256_is_not_placeholder() {
        assert_ne!(
            GATEWAY_BUILD_SHA256, PLACEHOLDER_SHA,
            "GATEWAY_BUILD_SHA256 should not be the placeholder"
        );
        assert!(
            GATEWAY_BUILD_SHA256.starts_with("sha256:"),
            "GATEWAY_BUILD_SHA256 should start with 'sha256:', got: {GATEWAY_BUILD_SHA256}"
        );
        // The digest part should be 64 hex chars (SHA-256)
        let digest = GATEWAY_BUILD_SHA256.strip_prefix("sha256:").unwrap();
        assert_eq!(
            digest.len(),
            64,
            "digest should be 64 hex chars, got {} chars: {digest}",
            digest.len()
        );
    }

    #[test]
    fn test_scaffold_runtime_lock_uses_build_sha() {
        let lock = scaffold_runtime_lock(None, None, &[]).expect("runtime lock scaffold");
        assert_ne!(
            lock.gateway.sha256, PLACEHOLDER_SHA,
            "scaffolded lock should use build SHA, not placeholder"
        );
        assert!(
            lock.gateway.sha256.starts_with("sha256:"),
            "gateway.sha256 should start with 'sha256:'"
        );
        assert!(
            lock.gateway
                .binary_sha256
                .as_deref()
                .unwrap_or_default()
                .starts_with("sha256:"),
            "gateway.binary_sha256 should start with 'sha256:'"
        );
        assert_eq!(lock.gateway.build_tag.as_deref(), Some(GATEWAY_BUILD_TAG));
    }

    #[test]
    fn test_render_skill_document_omits_null_optional_fields() {
        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: default_runtime_declaration(),
            agent: autonoetic_types::agent::AgentIdentity {
                id: "null.field.test".to_string(),
                name: "Null Field Test".to_string(),
                description: "Tests null field omission".to_string(),
            singleton: false,
        },
            capabilities: vec![],
            llm_overrides: None,
            llm_preset: None,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: autonoetic_types::agent::ExecutionMode::Reasoning,
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };
        let rendered = render_skill_document(&manifest, "# Test").unwrap();
        assert!(
            !rendered.contains("llm_config: null"),
            "rendered SKILL.md should not contain 'llm_config: null', got:\n{rendered}"
        );
        assert!(
            !rendered.contains("limits: null"),
            "rendered SKILL.md should not contain 'limits: null', got:\n{rendered}"
        );
        assert!(
            !rendered.contains("background: null"),
            "rendered SKILL.md should not contain 'background: null'"
        );
        assert!(
            !rendered.contains("gateway_url: null"),
            "rendered SKILL.md should not contain 'gateway_url: null'"
        );
        assert!(
            !rendered.contains("script_entry: null"),
            "rendered SKILL.md should not contain 'script_entry: null'"
        );
        assert!(
            !rendered.contains("allowed_tool_tiers"),
            "rendered SKILL.md should not contain empty 'allowed_tool_tiers'"
        );
    }
}
