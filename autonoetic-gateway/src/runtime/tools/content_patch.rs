use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::content_store::{ContentHandle, ContentStore, ContentVisibility};
use crate::runtime::guidance::{GuidanceBlock, GuidanceCondition};
use crate::runtime::tools::{NativeTool, NativeToolRegistry, ToolMetadata};
use crate::runtime::{fuzzy_match, v4a};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::ToolError;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(ContentPatchTool));
}

/// Repair hint appended to every patch failure, mirroring the anti-loop
/// escalation from studied agents: stop retrying variations, re-read, anchor
/// harder, or fall back to a full rewrite.
const ESCALATION_HINT: &str = "Stop retrying variations of the same snippet. Either (1) `resolve` the entry fresh to re-read current content, (2) use a longer, more unique `old_string` with surrounding context lines, or (3) `content_write` the whole entry if the region can't be uniquely anchored.";

/// The same name rule `content_write` enforces — keeps sandbox paths safe and
/// portable (no spaces, backslashes, etc.).
fn valid_content_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
}

pub struct ContentPatchTool;

impl NativeTool for ContentPatchTool {
    fn name(&self) -> &'static str {
        "content_patch"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::WriteAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Edit an existing content-store entry in place by sending ONLY the changed region — not the whole file. Prefer this over `content_write` for edits: it saves tokens and preserves the rest of the entry verbatim. `mode=\"replace\"` (default) does a fuzzy find-and-replace of `old_string`→`new_string` (tolerant of whitespace/indentation drift); the match must be unique unless `replace_all` is set. `mode=\"v4a\"` applies a multi-entry diff for edits spanning several entries. Returns the same `name`/`ref`/`sandbox_path` as `content_write`. Reach for `content_write` only to author a NEW entry or when the changed region can't be uniquely anchored.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["replace", "v4a"],
                        "description": "Edit mode. 'replace' (default): single-entry find-and-replace. 'v4a': multi-entry unified-diff-style patch."
                    },
                    "name": {
                        "type": "string",
                        "description": "[replace] The registered content name to edit (e.g. 'src/main.rs'). Must already exist."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "[replace] The exact snippet to find. Fuzzy-matched (whitespace/indentation tolerant). Must be unique unless replace_all=true."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "[replace] Replacement text for old_string."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "[replace] Replace every exact occurrence instead of requiring a unique match. Default false."
                    },
                    "patch": {
                        "type": "string",
                        "description": "[v4a] The V4A patch text: '*** Begin Patch' ... '*** End Patch' with '*** Update File: <name>' / '*** Add File: <name>' sections."
                    },
                    "visibility": {
                        "type": "string",
                        "enum": ["private", "session", "global"],
                        "description": "Override visibility for the written entry. Default: preserve the existing entry's visibility (or 'session' for new v4a Add files)."
                    },
                    "include_canonical_digest": {
                        "type": "boolean",
                        "description": "If true, include `canonical_digest` (sha256:...) in the response. Default false."
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            mode: Option<String>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default, deserialize_with = "crate::runtime::tools::deserialize_opt_string_lenient")]
            old_string: Option<String>,
            #[serde(default, deserialize_with = "crate::runtime::tools::deserialize_opt_string_lenient")]
            new_string: Option<String>,
            #[serde(default)]
            replace_all: bool,
            #[serde(default, deserialize_with = "crate::runtime::tools::deserialize_opt_string_lenient")]
            patch: Option<String>,
            #[serde(default)]
            visibility: Option<String>,
            #[serde(default)]
            include_canonical_digest: bool,
        }
        let args: Args = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        let Some(gw_dir) = gateway_dir else {
            return Ok(ToolError::resource(
                "Content store requires gateway directory to be configured",
                None::<String>,
            )
            .to_error_response());
        };
        let sid = session_id.unwrap_or(&manifest.agent.id);
        let store = ContentStore::new(gw_dir)?;

        let visibility_override = match args.visibility.as_deref() {
            None => None,
            Some("private") => Some(ContentVisibility::Private),
            Some("session") => Some(ContentVisibility::Session),
            Some("global") => Some(ContentVisibility::Global),
            Some(other) => {
                return Ok(ToolError::validation(
                    format!("Invalid visibility '{}'. Must be one of: private, session, global", other),
                    None::<String>,
                )
                .to_error_response());
            }
        };

        match args.mode.as_deref().unwrap_or("replace") {
            "replace" => self.run_replace(&store, sid, &args.name, &args.old_string, &args.new_string, args.replace_all, visibility_override, args.include_canonical_digest),
            "v4a" => self.run_v4a(&store, sid, &args.patch, visibility_override, args.include_canonical_digest),
            other => Ok(ToolError::validation(
                format!("Invalid mode '{}'. Must be 'replace' or 'v4a'.", other),
                None::<String>,
            )
            .to_error_response()),
        }
    }

    fn extract_metadata(&self, arguments_json: &str) -> ToolMetadata {
        let mut meta = ToolMetadata::default();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments_json) {
            if let Some(name) = v.get("name").and_then(|v| v.as_str()) {
                meta.path = Some(name.to_string());
            }
        }
        meta
    }

    fn guidance(&self) -> Vec<GuidanceBlock> {
        // The general doctrine is family-agnostic. The format hints (#465) take
        // the Hermes *insight* — only gpt/codex reliably drive the multi-entry
        // V4A diff; every other family drives `replace` better — but express it
        // openly: `replace` is the DEFAULT (gated `Not(gpt/codex)`), so local
        // models (qwen, minimax, nemotron, …) and unknown models all get it,
        // with no model allowlist to maintain. Only gpt/codex are special-cased.
        const V4A_FAMILIES: &[&str] = &["gpt", "codex"];
        let present = || {
            GuidanceCondition::All(vec![
                GuidanceCondition::Capability("write_access"),
                GuidanceCondition::ToolPresent("content_patch"),
            ])
        };
        vec![
            GuidanceBlock {
                id: "editing.content_patch",
                // Gating is belt-and-suspenders: collect_guidance only reaches
                // here when content_patch is available (WriteAccess), but the
                // explicit condition keeps the block self-describing.
                when: present(),
                priority: 10,
                prose: "**Editing existing content.** Two tools put content in the store: `content_write` \
authors a NEW entry; `content_patch` edits an EXISTING one. To change an entry you already wrote, \
prefer `content_patch` — send only the changed region, never the whole file. \
`content_patch(mode=\"replace\", name, old_string, new_string)` matches a unique snippet (tolerant of \
whitespace/indentation drift) and swaps it; include surrounding lines if a short snippet would match in \
several places. Use `mode=\"v4a\"` only when one logical edit spans several entries. Reach for \
`content_write` to edit only when authoring a brand-new entry, or when the region genuinely can't be \
uniquely anchored. If a patch fails to match, `resolve` the entry to re-read its current content before \
retrying — don't guess variations of the same snippet."
                    .to_string(),
            },
            GuidanceBlock {
                // Default for every family except gpt/codex (and for unknown models).
                id: "editing.content_patch.format.replace",
                when: GuidanceCondition::All(vec![
                    present(),
                    GuidanceCondition::Not(Box::new(GuidanceCondition::ModelFamily(V4A_FAMILIES))),
                ]),
                priority: 11,
                prose: "Edit format: prefer `mode=\"replace\"` — match a unique snippet and swap it. \
Reach for `mode=\"v4a\"` only when one edit genuinely spans several entries at once."
                    .to_string(),
            },
            GuidanceBlock {
                id: "editing.content_patch.format.v4a",
                when: GuidanceCondition::All(vec![
                    present(),
                    GuidanceCondition::ModelFamily(V4A_FAMILIES),
                ]),
                priority: 11,
                prose: "Edit format: for edits spanning several entries prefer `mode=\"v4a\"` (the \
multi-entry diff format you handle most reliably); use `mode=\"replace\"` for a single small swap."
                    .to_string(),
            },
        ]
    }
}

impl ContentPatchTool {
    #[allow(clippy::too_many_arguments)]
    fn run_replace(
        &self,
        store: &ContentStore,
        sid: &str,
        name: &Option<String>,
        old_string: &Option<String>,
        new_string: &Option<String>,
        replace_all: bool,
        visibility_override: Option<ContentVisibility>,
        include_canonical_digest: bool,
    ) -> anyhow::Result<String> {
        let Some(name) = name.as_deref().filter(|n| !n.trim().is_empty()) else {
            return Ok(ToolError::validation("'name' is required for mode='replace'", None::<String>).to_error_response());
        };
        let (Some(old_string), Some(new_string)) = (old_string.as_deref(), new_string.as_deref()) else {
            return Ok(ToolError::validation("'old_string' and 'new_string' are required for mode='replace'", None::<String>).to_error_response());
        };

        let old_handle = match store.resolve_name_with_root(sid, name) {
            Ok(h) => h,
            Err(_) => {
                return Ok(ToolError::not_found(
                    format!("content name '{}'", name),
                    Some("content_patch edits an existing entry by its registered name. Author new entries with content_write."),
                )
                .to_error_response());
            }
        };
        let current = match store.read_string(&old_handle) {
            Ok(s) => s,
            Err(_) => {
                return Ok(ToolError::validation(
                    format!("content '{}' is not valid UTF-8 text; content_patch edits text only", name),
                    None::<String>,
                )
                .to_error_response());
            }
        };

        let outcome = match fuzzy_match::find_and_replace(&current, old_string, new_string, replace_all) {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolError::validation(
                    format!("{e} in '{name}'"),
                    Some(ESCALATION_HINT),
                )
                .to_error_response());
            }
        };

        let visibility = visibility_override
            .or_else(|| current_visibility(store, sid, &old_handle))
            .unwrap_or(ContentVisibility::Session);

        let new_handle = store.write(outcome.content.as_bytes())?;
        store.register_name_with_visibility(sid, name, &new_handle, visibility)?;

        let diff = compact_diff(old_string, new_string);
        let mut out = write_result_json(name, &new_handle, outcome.content.len(), visibility, include_canonical_digest);
        out["strategy"] = serde_json::Value::String(outcome.strategy.as_str().to_string());
        out["replacements"] = serde_json::Value::from(outcome.replacements);
        out["diff"] = serde_json::Value::String(diff);
        serde_json::to_string(&out).map_err(Into::into)
    }

    fn run_v4a(
        &self,
        store: &ContentStore,
        sid: &str,
        patch: &Option<String>,
        visibility_override: Option<ContentVisibility>,
        include_canonical_digest: bool,
    ) -> anyhow::Result<String> {
        let Some(patch) = patch.as_deref().filter(|p| !p.trim().is_empty()) else {
            return Ok(ToolError::validation("'patch' is required for mode='v4a'", None::<String>).to_error_response());
        };

        let ops = match v4a::parse(patch) {
            Ok(ops) => ops,
            Err(e) => return Ok(ToolError::validation(format!("invalid v4a patch: {e}"), None::<String>).to_error_response()),
        };

        // Reject unsupported ops, validate Add names, and reject duplicate
        // operations on the same name — Phase 1 validates each op against the
        // original store state, so two ops on one name would apply against
        // stale content. Callers must combine them into one section.
        let mut seen: HashSet<&str> = HashSet::new();
        for op in &ops {
            let name = match op {
                v4a::V4aOp::Update { name, .. } => name.as_str(),
                v4a::V4aOp::Add { name, .. } => {
                    if !valid_content_name(name) {
                        return Ok(ToolError::validation(
                            format!("invalid Add File name '{name}': use only alphanumerics, '_', '-', '.', or '/'"),
                            None::<String>,
                        )
                        .to_error_response());
                    }
                    name.as_str()
                }
                v4a::V4aOp::Delete { .. } | v4a::V4aOp::Move { .. } => {
                    return Ok(ToolError::validation(
                        "v4a Delete/Move operations are not yet supported (the content store has no name unregister/rename). Use Update/Add only.",
                        None::<String>,
                    )
                    .to_error_response());
                }
            };
            if !seen.insert(name) {
                return Ok(ToolError::validation(
                    format!("v4a patch has multiple operations for '{name}'; combine them into a single Update/Add section"),
                    None::<String>,
                )
                .to_error_response());
            }
        }

        // Phase 1: compute every entry's final content in memory. No writes.
        struct Pending {
            name: String,
            content: String,
            visibility: ContentVisibility,
            diff: String,
        }
        let mut pending: Vec<Pending> = Vec::new();
        for op in &ops {
            match op {
                v4a::V4aOp::Update { name, hunks } => {
                    let old_handle = match store.resolve_name_with_root(sid, name) {
                        Ok(h) => h,
                        Err(_) => {
                            return Ok(ToolError::not_found(
                                format!("content name '{}' (v4a Update)", name),
                                Some("Update targets an existing entry; use Add File for new entries."),
                            )
                            .to_error_response());
                        }
                    };
                    let current = match store.read_string(&old_handle) {
                        Ok(s) => s,
                        Err(_) => {
                            return Ok(ToolError::validation(
                                format!("content '{}' is not valid UTF-8 text", name),
                                None::<String>,
                            )
                            .to_error_response());
                        }
                    };
                    let final_content = match v4a::apply_hunks(name, &current, hunks) {
                        Ok(c) => c,
                        Err(e) => {
                            return Ok(ToolError::validation(format!("{e}"), Some(ESCALATION_HINT)).to_error_response());
                        }
                    };
                    let visibility = visibility_override
                        .or_else(|| current_visibility(store, sid, &old_handle))
                        .unwrap_or(ContentVisibility::Session);
                    let diff = hunks
                        .iter()
                        .map(|h| compact_diff(&h.old_block, &h.new_block))
                        .collect::<Vec<_>>()
                        .join("\n");
                    pending.push(Pending { name: name.clone(), content: final_content, visibility, diff });
                }
                v4a::V4aOp::Add { name, content } => {
                    let visibility = visibility_override.unwrap_or(ContentVisibility::Session);
                    let diff = compact_diff("", content);
                    pending.push(Pending { name: name.clone(), content: content.clone(), visibility, diff });
                }
                v4a::V4aOp::Delete { .. } | v4a::V4aOp::Move { .. } => unreachable!("rejected above"),
            }
        }

        // Phase 2: commit. All hunks validated, so writes won't half-apply.
        let mut files = Vec::new();
        for p in pending {
            let handle = store.write(p.content.as_bytes())?;
            store.register_name_with_visibility(sid, &p.name, &handle, p.visibility)?;
            let mut entry = write_result_json(&p.name, &handle, p.content.len(), p.visibility, include_canonical_digest);
            entry["diff"] = serde_json::Value::String(p.diff);
            files.push(entry);
        }

        serde_json::to_string(&serde_json::json!({ "ok": true, "files": files })).map_err(Into::into)
    }
}

/// Look up the stored visibility for a handle, checking the current session
/// then its root session. Session/global-visible content is recorded in the
/// root manifest, so checking there avoids misclassifying (and re-registering)
/// an entry that resolved from the root. Note: private content is not
/// cross-session resolvable, so this cannot widen another session's private
/// entry. Returns `None` only when visibility is genuinely unrecorded.
fn current_visibility(store: &ContentStore, sid: &str, handle: &str) -> Option<ContentVisibility> {
    let manifest = store.load_manifest(sid).ok()?;
    if let Some(v) = manifest.visibility.get(handle).copied() {
        return Some(v);
    }
    let root = manifest.root_session_id.as_deref()?;
    if root == sid {
        return None;
    }
    store
        .load_manifest(root)
        .ok()
        .and_then(|m| m.visibility.get(handle).copied())
}

/// The shared `content_write`-shaped result for a written entry.
fn write_result_json(
    name: &str,
    handle: &ContentHandle,
    bytes: usize,
    visibility: ContentVisibility,
    include_canonical_digest: bool,
) -> serde_json::Value {
    let short_alias = ContentStore::get_short_alias(handle);
    let mut out = serde_json::json!({
        "ok": true,
        "name": name,
        "alias": short_alias,
        "ref": format!("cnt_{}", short_alias),
        "sandbox_path": format!("/tmp/{}", name),
        "bytes_written": bytes,
        "visibility": match visibility {
            ContentVisibility::Private => "private",
            ContentVisibility::Session => "session",
            ContentVisibility::Global => "global",
        },
    });
    if include_canonical_digest {
        out["canonical_digest"] = serde_json::Value::String(handle.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, ContentStore) {
        let dir = TempDir::new().unwrap();
        let store = ContentStore::new(dir.path()).unwrap();
        (dir, store)
    }

    fn seed(store: &ContentStore, sid: &str, name: &str, content: &str, vis: ContentVisibility) -> String {
        let h = store.write(content.as_bytes()).unwrap();
        store.register_name_with_visibility(sid, name, &h, vis).unwrap();
        h
    }

    fn read(store: &ContentStore, sid: &str, name: &str) -> String {
        String::from_utf8(store.read_by_name(sid, name).unwrap()).unwrap()
    }

    #[test]
    fn replace_repoints_name_and_preserves_old_handle() {
        let (_d, store) = store();
        let sid = "s1";
        let old_handle = seed(&store, sid, "main.rs", "fn main() {\n    foo();\n}\n", ContentVisibility::Private);

        let res = ContentPatchTool
            .run_replace(&store, sid, &Some("main.rs".into()), &Some("foo();".into()), &Some("bar();".into()), false, None, false)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["strategy"], "exact");

        let now = read(&store, sid, "main.rs");
        assert!(now.contains("bar();") && !now.contains("foo();"));

        // Immutability: the old handle is still readable, content intact.
        assert!(String::from_utf8(store.read(&old_handle).unwrap()).unwrap().contains("foo();"));

        // Name re-points to a new handle.
        let new_handle = store.resolve_name(sid, "main.rs").unwrap();
        assert_ne!(new_handle, old_handle);

        // Visibility preserved.
        let vis = store.load_manifest(sid).unwrap().visibility.get(&new_handle).copied();
        assert_eq!(vis, Some(ContentVisibility::Private));
    }

    #[test]
    fn replace_missing_name_is_error() {
        let (_d, store) = store();
        let res = ContentPatchTool
            .run_replace(&store, "s1", &Some("nope.rs".into()), &Some("a".into()), &Some("b".into()), false, None, false)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["ok"], false);
    }

    #[test]
    fn replace_no_match_carries_escalation_hint() {
        let (_d, store) = store();
        let sid = "s1";
        seed(&store, sid, "f", "alpha\n", ContentVisibility::Private);
        let res = ContentPatchTool
            .run_replace(&store, sid, &Some("f".into()), &Some("zzz".into()), &Some("q".into()), false, None, false)
            .unwrap();
        assert!(res.contains("the whole entry"), "missing escalation hint: {res}");
        assert_eq!(read(&store, sid, "f"), "alpha\n"); // untouched
    }

    #[test]
    fn v4a_multi_entry_atomic_commit() {
        let (_d, store) = store();
        let sid = "s1";
        seed(&store, sid, "a.txt", "one\n", ContentVisibility::Private);
        seed(&store, sid, "b.txt", "two\n", ContentVisibility::Private);
        let patch = "*** Begin Patch\n*** Update File: a.txt\n-one\n+ONE\n*** Update File: b.txt\n-two\n+TWO\n*** End Patch";
        let res = ContentPatchTool.run_v4a(&store, sid, &Some(patch.into()), None, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["files"].as_array().unwrap().len(), 2);
        assert!(read(&store, sid, "a.txt").contains("ONE"));
        assert!(read(&store, sid, "b.txt").contains("TWO"));
    }

    #[test]
    fn v4a_aborts_without_partial_writes() {
        let (_d, store) = store();
        let sid = "s1";
        seed(&store, sid, "a.txt", "one\n", ContentVisibility::Private);
        seed(&store, sid, "b.txt", "two\n", ContentVisibility::Private);
        // Second entry's hunk cannot match → the whole patch must abort.
        let patch = "*** Begin Patch\n*** Update File: a.txt\n-one\n+ONE\n*** Update File: b.txt\n-NOPE\n+X\n*** End Patch";
        let res = ContentPatchTool.run_v4a(&store, sid, &Some(patch.into()), None, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["ok"], false);
        // No partial write: a.txt is untouched.
        assert_eq!(read(&store, sid, "a.txt"), "one\n");
    }

    #[test]
    fn v4a_add_creates_new_entry() {
        let (_d, store) = store();
        let sid = "s1";
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+hello\n+world\n*** End Patch";
        let res = ContentPatchTool.run_v4a(&store, sid, &Some(patch.into()), None, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(read(&store, sid, "new.txt"), "hello\nworld");
    }

    #[test]
    fn v4a_rejects_delete() {
        let (_d, store) = store();
        let patch = "*** Begin Patch\n*** Delete File: x\n*** End Patch";
        let res = ContentPatchTool.run_v4a(&store, "s1", &Some(patch.into()), None, false).unwrap();
        assert!(res.to_lowercase().contains("not yet supported"));
    }

    #[test]
    fn v4a_rejects_duplicate_name_ops() {
        let (_d, store) = store();
        let sid = "s1";
        seed(&store, sid, "a.txt", "one\ntwo\n", ContentVisibility::Private);
        // Two Update sections for the same name would apply against stale state.
        let patch = "*** Begin Patch\n*** Update File: a.txt\n-one\n+ONE\n*** Update File: a.txt\n-two\n+TWO\n*** End Patch";
        let res = ContentPatchTool.run_v4a(&store, sid, &Some(patch.into()), None, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["ok"], false);
        assert!(res.contains("multiple operations"));
        assert_eq!(read(&store, sid, "a.txt"), "one\ntwo\n"); // untouched
    }

    #[test]
    fn v4a_rejects_unsafe_add_name() {
        let (_d, store) = store();
        let patch = "*** Begin Patch\n*** Add File: bad name\n+x\n*** End Patch";
        let res = ContentPatchTool.run_v4a(&store, "s1", &Some(patch.into()), None, false).unwrap();
        let v: serde_json::Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["ok"], false);
        assert!(res.contains("invalid Add File name"));
    }

    #[test]
    fn guidance_selects_family_specific_format_hint() {
        use crate::runtime::guidance::{compose_guidance, GuidanceContext};
        let blocks = ContentPatchTool.guidance();
        let caps = vec![Capability::WriteAccess { scopes: vec!["*".to_string()] }];
        let tools = vec!["content_patch".to_string()];
        let base = GuidanceContext {
            capabilities: &caps,
            active_tool_names: &tools,
            model_family: None,
            role: None,
            phase: None,
        };

        // Claude → replace-first hint (it's not gpt/codex), not the v4a one.
        let claude = GuidanceContext { model_family: Some("claude-opus-4-8"), ..base.clone() };
        let out = compose_guidance(&blocks, &claude);
        assert!(out.contains("Editing existing content"));
        assert!(out.contains("prefer `mode=\"replace\"`"));
        assert!(!out.contains("most reliably"));

        // GPT/codex → v4a-for-multi-entry hint, not the replace-first one.
        let gpt = GuidanceContext { model_family: Some("gpt-4o"), ..base.clone() };
        let out = compose_guidance(&blocks, &gpt);
        assert!(out.contains("most reliably"));
        assert!(!out.contains("Edit format: prefer `mode=\"replace\"`"));

        // Local/other models (qwen, minimax, …) → replace is the default.
        for m in ["qwen2.5-coder", "minimax/minimax-m2.7", "nemotron-4"] {
            let ctx = GuidanceContext { model_family: Some(m), ..base.clone() };
            let out = compose_guidance(&blocks, &ctx);
            assert!(out.contains("prefer `mode=\"replace\"`"), "{m} should get replace hint");
            assert!(!out.contains("most reliably"), "{m} should not get v4a hint");
        }

        // Unknown model → replace is still the safe default.
        let out = compose_guidance(&blocks, &base);
        assert!(out.contains("prefer `mode=\"replace\"`"));
        assert!(!out.contains("most reliably"));
    }
}

/// A compact, display-only `-old / +new` snippet. Not a minimal LCS diff —
/// just enough for the agent to confirm what changed. Truncated for large blocks.
fn compact_diff(old: &str, new: &str) -> String {
    const MAX_LINES: usize = 40;
    let mut out = String::new();
    for (i, line) in old.split('\n').enumerate() {
        if i >= MAX_LINES {
            out.push_str("- … (truncated)\n");
            break;
        }
        if !old.is_empty() {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }
    for (i, line) in new.split('\n').enumerate() {
        if i >= MAX_LINES {
            out.push_str("+ … (truncated)\n");
            break;
        }
        if !new.is_empty() {
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
