use std::path::Path;
use std::sync::Arc;

use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::tools::{NativeTool, NativeToolRunContext};

pub struct AgentInspectTool;

/// Directory names that are never useful to surface to the LLM:
/// build/cache artifacts, virtual environments, VCS metadata. The walker
/// does not recurse into these — they are dropped entirely (both from the
/// `files` list and from `source`).
const EXCLUDED_DIRECTORY_NAMES: &[&str] = &[
    "__pycache__",
    ".venv",
    "venv",
    "env",
    "node_modules",
    "target",
    ".git",
    ".hg",
    ".svn",
    "dist",
    "build",
    ".next",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".cache",
    "__pypackages__",
    ".idea",
    ".vscode",
];

/// File extensions that are always excluded — compiled bytecode, shared
/// libraries, executables, archives. These are listed without the leading dot
/// and matched case-insensitively against the file's extension.
const EXCLUDED_FILE_EXTENSIONS: &[&str] = &[
    "pyc", "pyo", "pyd", // Python bytecode / extension modules
    "so", "dylib", "dll", // Shared libraries
    "o", "a", "obj", "lib", // Object/static archives
    "class", "jar", "war", // JVM
    "wasm", // WebAssembly binaries
    "exe", "bin", // Executables
    "zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", // Archives
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tiff", "tif", // Images
    "mp3", "mp4", "wav", "ogg", "flac", "mov", "avi", "webm", // Media
    "pdf", "ttf", "otf", "woff", "woff2", "eot", // Documents/fonts
    "db", "sqlite", "sqlite3", "mdb", // Databases
];

/// Whole-filename suffixes (after the last `.`) that should be skipped. Used
/// for Unix socket files and other non-content artifacts whose presence in a
/// revision dir is purely a runtime side-effect.
const EXCLUDED_FILE_SUFFIXES: &[&str] = &[".sock"];

/// Maximum size of a single file included in `source` (per-file cap).
/// Larger files are truncated and listed under `truncated_files` with their
/// original byte size.
const MAX_PER_FILE_BYTES: usize = 64 * 1024;

/// Maximum aggregate size across all files in `source` (response-level cap).
/// Once exceeded, remaining files are listed under `skipped_files` with
/// `reason="total_size_cap"` instead of being inlined.
const MAX_TOTAL_SOURCE_BYTES: usize = 256 * 1024;

/// A file is treated as binary (and skipped from `source`) when it contains
/// any NUL byte in the first this-many bytes scanned, OR when it is not valid
/// UTF-8. NUL bytes very rarely occur in legitimate source/config files and
/// strongly indicate compiled output.
const BINARY_SNIFF_BYTES: usize = 4096;

impl NativeTool for AgentInspectTool {
    fn name(&self) -> &'static str {
        "agent_inspect"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::ReadAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Inspect an installed agent's metadata, capabilities, and optionally its source code. Resolves the agent's current active revision. Source code is only returned for locally-trusted agents.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent ID to inspect (e.g., 'daily-trading-signal', 'coder.default')"
                    },
                    "include_source": {
                        "type": "boolean",
                        "description": "Include full source file contents. Only returned for locally-trusted agents. Default: false."
                    },
                    "include_layers": {
                        "type": "boolean",
                        "description": "Include dependency layer metadata from the artifact bundle. Default: false."
                    }
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: serde_json::Value = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}. Usage: {} {{\"agent_id\": \"<agent-name>\"}}", self.name(), e, self.name()))?;

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!(
                "{}: missing required field 'agent_id'. Usage: {} {{\"agent_id\": \"<agent-name>\"}} (e.g., \"daily-trading-signal\", \"coder.default\")",
                self.name(),
                self.name(),
            ))?;

        crate::runtime::tools::validate_agent_id(agent_id)?;

        let include_source = args
            .get("include_source")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_layers = args
            .get("include_layers")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let store = gateway_store
            .ok_or_else(|| anyhow::anyhow!("GatewayStore is required"))?;
        let gateway_dir = gateway_dir
            .ok_or_else(|| anyhow::anyhow!("gateway_dir is required"))?;

        let alias = store
            .resolve_alias(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' is not installed (no alias found)", agent_id))?;

        let rev = store
            .get_agent_revision(&alias.revision_id)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Revision '{}' for agent '{}' not found",
                    alias.revision_id,
                    agent_id
                )
            })?;

        let is_local = rev.trust_domain == "local";

        let revision_dir = gateway_dir
            .join("revisions")
            .join("agents")
            .join(agent_id)
            .join(&rev.revision_id);

        if !revision_dir.exists() {
            return Err(anyhow::anyhow!(
                "Revision directory for agent '{}' does not exist on disk",
                agent_id
            ));
        }

        let walk = walk_revision_dir(&revision_dir)?;

        let skill_content = walk
            .files
            .iter()
            .find(|f| f.rel_path == "SKILL.md")
            .map(|f| String::from_utf8_lossy(&f.bytes).to_string());

        let parsed_manifest = skill_content
            .as_deref()
            .and_then(|s| crate::runtime::parser::SkillParser::parse(s).ok());

        let (skill_meta, file_list) = {
            let mut files: Vec<String> =
                walk.files.iter().map(|f| f.rel_path.clone()).collect();
            files.sort();

            let meta = if let Some((ref m, _)) = parsed_manifest {
                serde_json::json!({
                    "agent": {
                        "id": m.agent.id,
                        "name": m.agent.name,
                        "description": m.agent.description,
                    },
                    "capabilities": m.capabilities.iter().map(|c| serde_json::to_value(c).unwrap_or_default()).collect::<Vec<_>>(),
                    "execution_mode": serde_json::to_value(&m.execution_mode).unwrap_or_default(),
                    "script_entry": m.script_entry,
                })
            } else {
                serde_json::json!(null)
            };

            (meta, files)
        };

        let mut out = serde_json::json!({
            "ok": true,
            "agent_id": agent_id,
            "alias": {
                "revision_id": alias.revision_id,
                "short_ref": format!("{}@rev_{}", agent_id, rev.short_id),
                "updated_at": alias.updated_at,
            },
            "revision": {
                "revision_id": rev.revision_id,
                "status": format!("{:?}", rev.status),
                "created_at": rev.created_at,
                "created_by_type": rev.created_by_type,
                "created_by_id": rev.created_by_id,
                "trust_domain": rev.trust_domain,
                "source_kind": rev.source_kind,
                "base_revision_id": rev.base_revision_id,
                "artifact_id": rev.artifact_id,
            },
            "skill": skill_meta,
            "files": file_list,
        });

        // Always surface what the walker excluded, even when include_source is
        // false, so the caller can reason about what's on disk vs. what's
        // returned. Empty arrays/maps are omitted.
        if !walk.excluded_dirs.is_empty() {
            out.as_object_mut().map(|o| {
                o.insert(
                    "excluded_directories".to_string(),
                    serde_json::to_value(&walk.excluded_dirs).unwrap(),
                );
            });
        }
        if !walk.excluded_files.is_empty() {
            out.as_object_mut().map(|o| {
                o.insert(
                    "excluded_files".to_string(),
                    serde_json::to_value(&walk.excluded_files).unwrap(),
                );
            });
        }

        if include_source && is_local {
            let SourceBuild {
                source,
                truncated_files,
                skipped_files,
                total_bytes,
            } = build_source_map(&walk.files);
            out.as_object_mut().map(|o| {
                o.insert("source".to_string(), serde_json::to_value(&source).unwrap());
                if !truncated_files.is_empty() {
                    o.insert(
                        "truncated_files".to_string(),
                        serde_json::to_value(&truncated_files).unwrap(),
                    );
                }
                if !skipped_files.is_empty() {
                    o.insert(
                        "skipped_files".to_string(),
                        serde_json::to_value(&skipped_files).unwrap(),
                    );
                }
                o.insert(
                    "source_total_bytes".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(total_bytes)),
                );
            });
        } else if include_source && !is_local {
            out.as_object_mut().map(|o| {
                o.insert(
                    "source".to_string(),
                    serde_json::json!({
                        "omitted": true,
                        "reason": format!("Agent trust domain is '{}' — source code is restricted to local agents only", rev.trust_domain),
                    }),
                );
            });
        }

        if include_layers {
            if let Some(ref art_id) = rev.artifact_id {
                let artifact_store =
                    crate::artifact_store::ArtifactStore::new(gateway_dir)?;
                match artifact_store.inspect(art_id) {
                    Ok(bundle) => {
                        let layers: Vec<serde_json::Value> = bundle
                            .layers
                            .iter()
                            .map(|l| {
                                serde_json::json!({
                                    "layer_id": l.layer_id,
                                    "name": l.name,
                                    "mount_path": l.mount_path,
                                    "digest": l.digest,
                                })
                            })
                            .collect();
                        out.as_object_mut().map(|o| {
                            o.insert("layers".to_string(), serde_json::json!(layers));
                        });
                    }
                    Err(e) => {
                        out.as_object_mut().map(|o| {
                            o.insert(
                                "layers".to_string(),
                                serde_json::json!({
                                    "error": format!("Could not load artifact layers: {}", e),
                                }),
                            );
                        });
                    }
                }
            } else {
                out.as_object_mut().map(|o| {
                    o.insert("layers".to_string(), serde_json::json!([]));
                });
            }
        }

        serde_json::to_string(&out).map_err(Into::into)
    }
}

/// A single file collected by `walk_revision_dir`. The bytes are read eagerly
/// because the revision dir is small (text source + manifests) once excluded
/// directories are pruned.
struct WalkedFile {
    rel_path: String,
    bytes: Vec<u8>,
}

/// Result of walking a revision directory: the files retained for inspection
/// plus diagnostic lists of what was pruned and why.
struct WalkResult {
    files: Vec<WalkedFile>,
    /// Relative paths of directories that were not recursed into (e.g.
    /// `"venv"`, `"__pycache__"`). Stored once per excluded dir, not per file.
    excluded_dirs: Vec<String>,
    /// Relative paths of files dropped by the name/extension/suffix filter,
    /// each annotated with the exclusion reason.
    excluded_files: Vec<ExcludedFile>,
}

#[derive(serde::Serialize)]
struct ExcludedFile {
    path: String,
    reason: String,
}

/// Walk the revision directory, pruning known-junk subdirectories and files.
/// The returned `WalkResult` contains only the files retained for inspection
/// (i.e. ones that could reasonably be source/config/text) plus diagnostic
/// records of what was filtered.
fn walk_revision_dir(root: &Path) -> anyhow::Result<WalkResult> {
    fn walk(
        base: &Path,
        current: &Path,
        out: &mut WalkResult,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            let rel = path
                .strip_prefix(base)
                .map_err(|e| anyhow::anyhow!("Failed to compute relative path: {}", e))?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            // Resolve once: kind of filesystem entry, with symlinks NOT followed.
            // Symlinks are skipped entirely — a malicious or accidental symlink
            // into the user's home directory must never leak content.
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                out.excluded_files.push(ExcludedFile {
                    path: rel_str,
                    reason: "symlink".to_string(),
                });
                continue;
            }
            if file_type.is_dir() {
                if EXCLUDED_DIRECTORY_NAMES.contains(&name_str.as_ref()) {
                    out.excluded_dirs.push(rel_str);
                    continue;
                }
                walk(base, &path, out)?;
                continue;
            }
            if !file_type.is_file() {
                // Sockets, fifos, block/char devices.
                out.excluded_files.push(ExcludedFile {
                    path: rel_str,
                    reason: "not_a_regular_file".to_string(),
                });
                continue;
            }

            // Filename-based filter (suffixes like `.sock`).
            if EXCLUDED_FILE_SUFFIXES
                .iter()
                .any(|suf| name_str.ends_with(suf))
            {
                out.excluded_files.push(ExcludedFile {
                    path: rel_str,
                    reason: "excluded_suffix".to_string(),
                });
                continue;
            }

            // Extension-based filter (case-insensitive).
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext_lc = ext.to_ascii_lowercase();
                if EXCLUDED_FILE_EXTENSIONS.contains(&ext_lc.as_str()) {
                    out.excluded_files.push(ExcludedFile {
                        path: rel_str,
                        reason: format!("excluded_extension:{}", ext_lc),
                    });
                    continue;
                }
            }

            let bytes = std::fs::read(&path)?;
            out.files.push(WalkedFile {
                rel_path: rel_str,
                bytes,
            });
        }
        Ok(())
    }

    let mut result = WalkResult {
        files: Vec::new(),
        excluded_dirs: Vec::new(),
        excluded_files: Vec::new(),
    };
    walk(root, root, &mut result)?;
    // Stable ordering for deterministic test output and reproducible digests.
    result.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    result.excluded_dirs.sort();
    result.excluded_files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

/// Output of assembling the `source` map under the per-file and total-size
/// caps. `truncated_files` lists files that were included but cut short;
/// `skipped_files` lists files that were dropped entirely (binary, oversized
/// past the total cap, etc.).
struct SourceBuild {
    source: std::collections::BTreeMap<String, String>,
    truncated_files: Vec<TruncatedFile>,
    skipped_files: Vec<ExcludedFile>,
    total_bytes: usize,
}

#[derive(serde::Serialize)]
struct TruncatedFile {
    path: String,
    original_bytes: usize,
    included_bytes: usize,
}

/// Build the `source` map, applying:
///   1. Binary detection (skipped),
///   2. Per-file truncation at `MAX_PER_FILE_BYTES`,
///   3. Total-size cap at `MAX_TOTAL_SOURCE_BYTES` (remaining files skipped).
fn build_source_map(files: &[WalkedFile]) -> SourceBuild {
    let mut source = std::collections::BTreeMap::new();
    let mut truncated = Vec::new();
    let mut skipped = Vec::new();
    let mut total: usize = 0;

    for file in files {
        let original_bytes = file.bytes.len();

        if is_binary_payload(&file.bytes) {
            skipped.push(ExcludedFile {
                path: file.rel_path.clone(),
                reason: "binary_content".to_string(),
            });
            continue;
        }

        // Apply per-file cap before measuring against the total budget so a
        // single huge file can't consume the entire response on its own.
        let (slice, was_truncated) = if original_bytes > MAX_PER_FILE_BYTES {
            (&file.bytes[..MAX_PER_FILE_BYTES], true)
        } else {
            (&file.bytes[..], false)
        };

        // Respect the total cap. If adding this file would push past the cap,
        // skip it entirely rather than producing a partial mid-file cut that
        // the caller has no way to interpret.
        if total.saturating_add(slice.len()) > MAX_TOTAL_SOURCE_BYTES {
            skipped.push(ExcludedFile {
                path: file.rel_path.clone(),
                reason: "total_size_cap".to_string(),
            });
            continue;
        }

        // Decode the (possibly truncated) slice as UTF-8 lossily. A
        // mid-multibyte cut becomes a single replacement char rather than an
        // error, which is the desired behaviour.
        let text = String::from_utf8_lossy(slice).to_string();
        total = total.saturating_add(text.len());
        source.insert(file.rel_path.clone(), text);

        if was_truncated {
            truncated.push(TruncatedFile {
                path: file.rel_path.clone(),
                original_bytes,
                included_bytes: slice.len(),
            });
        }
    }

    SourceBuild {
        source,
        truncated_files: truncated,
        skipped_files: skipped,
        total_bytes: total,
    }
}

/// Cheap binary heuristic: a NUL byte anywhere in the first `BINARY_SNIFF_BYTES`
/// of a file is treated as conclusive evidence of binary content. Legitimate
/// text/source files (including UTF-8, UTF-16-without-BOM is rare in source
/// trees) do not contain interior NUL bytes.
fn is_binary_payload(bytes: &[u8]) -> bool {
    let sniff_len = bytes.len().min(BINARY_SNIFF_BYTES);
    bytes[..sniff_len].iter().any(|b| *b == 0)
}

pub fn register_tools(registry: &mut crate::runtime::tools::NativeToolRegistry) {
    registry.register(Box::new(AgentInspectTool));
}
