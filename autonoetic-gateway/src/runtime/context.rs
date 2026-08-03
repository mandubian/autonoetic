//! Context assembly: system prompt composition, foundation layers, user context, tool bridging.

use crate::runtime::lifecycle::AgentExecutor;
use autonoetic_types::agent::AgentManifest;

// ---------------------------------------------------------------------------
// Foundation Instructions
// ---------------------------------------------------------------------------

const FOUNDATION_CORE: &str = include_str!("foundation_core.md");
const FOUNDATION_WORKFLOW: &str = include_str!("foundation_workflow.md");
const FOUNDATION_ARTIFACT: &str = include_str!("foundation_artifact.md");
const FOUNDATION_SCRIPT: &str = include_str!("foundation_script.md");
const FOUNDATION_DIGEST: &str = include_str!("foundation_digest.md");
const FOUNDATION_SDK: &str = include_str!("foundation_sdk.md");

/// Compose foundation instructions based on agent capabilities and execution mode.
///
/// Always includes core instructions. Adds workflow, artifact, script, digest,
/// and SDK layers based on what the agent can actually do.
pub(crate) fn compose_foundation(manifest: &AgentManifest) -> String {
    let mut parts = Vec::new();
    parts.push(FOUNDATION_CORE.trim());

    let has_workflow_caps = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            autonoetic_types::capability::Capability::AgentSpawn { .. }
        )
    });
    let has_artifact_caps = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            autonoetic_types::capability::Capability::WriteAccess { .. }
        )
    });
    let is_script_mode = manifest.execution_mode == autonoetic_types::agent::ExecutionMode::Script;
    let has_digest_cap = manifest.capabilities.iter().any(|c| {
        if let autonoetic_types::capability::Capability::WriteAccess { scopes } = c {
            scopes.iter().any(|s| s.starts_with("digest") || s == "*")
        } else {
            false
        }
    });
    let has_code_execution = manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            autonoetic_types::capability::Capability::CodeExecution { .. }
                | autonoetic_types::capability::Capability::ArtifactExecution
        )
    });

    // SDK reference: executable-code roles, delegators (AgentSpawn), and
    // federation roles that statically review script code (architect, static_evaluator).
    let role = role_from_manifest(manifest);
    let needs_sdk_reference = has_code_execution
        || has_workflow_caps
        || matches!(role, Some("architect") | Some("static_evaluator"));

    if has_workflow_caps || !is_script_mode {
        parts.push(FOUNDATION_WORKFLOW.trim());
    }

    if has_artifact_caps {
        parts.push(FOUNDATION_ARTIFACT.trim());
        // The write-vs-patch doctrine that used to live here (foundation_editing.md,
        // #462) is now a content_patch-contributed guidance block (#464).
    }

    if is_script_mode {
        parts.push(FOUNDATION_SCRIPT.trim());
    }

    if has_digest_cap {
        parts.push(FOUNDATION_DIGEST.trim());
    }

    if needs_sdk_reference {
        parts.push(FOUNDATION_SDK.trim());
    }

    parts.join("\n\n---\n\n")
}

/// Best-effort role for guidance gating: the agent id segment before the first
/// `.` (e.g. `coder.default` → `coder`), or the whole id if there is none.
/// Returns `None` for an empty id or one with an empty leading segment
/// (e.g. `.coder`), so role-gated guidance never matches on `""`.
pub(crate) fn role_from_manifest(manifest: &AgentManifest) -> Option<&str> {
    manifest
        .agent
        .id
        .split('.')
        .next()
        .filter(|seg| !seg.is_empty())
}

const TOOL_BRIDGING_APPENDIX: &str = r#"---

Tool Compatibility Notes (auto-generated from AgentSkills import)

This skill was imported from the Agent Skills (agentskills.io) format.
The following tool mappings apply:

| Skill references | Autonoetic equivalent |
|---|---|
| `Bash(command)` | `sandbox_exec({"command": "command"})` |
| `Read(path)` | `resolve(ref, include="content")` — files must be loaded via content store |
| `Write(path, content)` | `content_write(name, content)` |
| `WebSearch(query)` | `web_search({"query": "query"})` |
| `WebFetch(url)` | `web_fetch({"url": "url"})` |

File paths referenced by the skill are available in the agent directory.
Use resolve/content_write or sandbox paths relative to the agent working directory."#;

fn tool_bridging_appendix() -> String {
    TOOL_BRIDGING_APPENDIX.to_string()
}

/// Build the system prompt given agent instructions and (optionally) raw agent
/// output policy metadata from the SKILL.md frontmatter.
///
/// When output constraints are declared, an
/// "Your Output Contract" section is appended so the agent knows upfront what
/// constraints the gateway will validate before returning its output to the caller.
pub(crate) fn compose_system_instructions_with_metadata(
    agent_instructions: &str,
    manifest: &AgentManifest,
    output_policy: Option<&autonoetic_types::agent::OutputPolicy>,
) -> String {
    compose_system_instructions_full(agent_instructions, manifest, output_policy, None, None, None)
}

/// Candidate pool size for task-matched recall: wider than `max_memories` so
/// the relevance scorer has something to rank before truncating down.
const MEMORY_CANDIDATE_POOL: usize = 50;

/// Jaccard token-overlap relevance score between the incoming task text and a
/// candidate memory's content. Mirrors
/// `runtime::tools::agent_revision::description_token_overlap` — kept as a
/// local copy rather than a cross-module dependency since the two call sites
/// score conceptually different things (description drift vs. task/memory
/// relevance) and shouldn't be coupled by a shared signature change.
pub(crate) fn score_task_relevance(task_text: &str, memory_content: &str) -> f64 {
    use std::collections::BTreeSet;
    let tokenize = |s: &str| -> BTreeSet<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 1)
            .map(|t| t.to_string())
            .collect()
    };
    let ta = tokenize(task_text);
    let tb = tokenize(memory_content);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Scopes treated as "error lessons" for prioritization/labeling purposes.
fn is_error_lesson_scope(scope: &str) -> bool {
    matches!(scope, "digest.error_pattern" | "digest.lesson")
}

/// Build a "Prior knowledge" block from Tier-2 global memories relevant to this agent.
///
/// When `task_text` is `Some` and non-empty, candidates are scored against it
/// (Jaccard token overlap) and re-ranked so task-relevant memories — error
/// lessons prioritized on ties — surface first; remaining slots are filled
/// with the most recent unscored candidates so recency value is preserved
/// when nothing matches. When `task_text` is `None`/empty, behavior is pure
/// recency, unchanged from before task-matched recall.
///
/// Returns `None` if no memories are found or the store is unavailable.
pub fn build_memory_context_snippet(
    store: &crate::scheduler::gateway_store::GatewayStore,
    agent_id: &str,
    max_memories: usize,
    task_text: Option<&str>,
    query_sink: Option<autonoetic_types::egress::Sink>,
    egress_cfg: Option<&autonoetic_types::egress::EgressConfig>,
) -> Option<String> {
    use crate::runtime::egress_stored::{
        filter_or_indicate_for_sink, query_sink_or_remote, resolve_stored_label,
        FilteredStoredContent,
    };
    use autonoetic_types::egress::IndicationVerbosity;

    let sink = query_sink_or_remote(query_sink);
    let default_cfg = autonoetic_types::egress::EgressConfig::default();
    let cfg = egress_cfg.unwrap_or(&default_cfg);
    let agent_tag = format!("agent:{agent_id}");

    // `search_memories_by_tags` ORs its tags, so the two-tag queries below
    // can return other agents' rows and crowd this agent's older-but-relevant
    // memories out of the candidate pool — re-require the conjunction here.
    let has_both = |m: &autonoetic_types::memory::MemoryObject, source_tag: &str| {
        m.tags.iter().any(|t| t == agent_tag.as_str())
            && m.tags.iter().any(|t| t == source_tag)
    };

    let agent_digests: Vec<_> = store.search_memories_by_tags(
        &[agent_tag.as_str(), "source:post_session_digest"],
        MEMORY_CANDIDATE_POOL,
    ).ok().unwrap_or_default()
        .into_iter()
        .filter(|m| has_both(m, "source:post_session_digest"))
        .collect();

    let agent_signals: Vec<_> = store.search_memories_by_tags(
        &[agent_tag.as_str(), "source:quality_signal"],
        MEMORY_CANDIDATE_POOL,
    ).ok().unwrap_or_default()
        .into_iter()
        .filter(|m| has_both(m, "source:quality_signal"))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut memories: Vec<_> = agent_digests
        .into_iter()
        .chain(agent_signals)
        .filter(|m| seen.insert(m.memory_id.clone()))
        .collect();

    if memories.is_empty() {
        memories = store
            .search_memories_by_tags(&["source:post_session_digest"], MEMORY_CANDIDATE_POOL)
            .ok()
            .unwrap_or_default();
    }

    if memories.is_empty() {
        return None;
    }

    let task_text = task_text.filter(|t| !t.trim().is_empty());
    let selected: Vec<_> = match task_text {
        Some(task) => {
            let mut scored: Vec<(f64, bool, _)> = memories
                .into_iter()
                .map(|m| {
                    let score = score_task_relevance(task, &m.content);
                    let error_lesson = is_error_lesson_scope(&m.scope);
                    (score, error_lesson, m)
                })
                .collect();
            // Score DESC, error-lesson-first tiebreak, updated_at DESC.
            scored.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| b.1.cmp(&a.1))
                    .then_with(|| b.2.updated_at.cmp(&a.2.updated_at))
            });

            let (matched, unscored): (Vec<_>, Vec<_>) =
                scored.into_iter().partition(|(score, _, _)| *score > 0.0);
            matched
                .into_iter()
                .chain(unscored)
                .take(max_memories)
                .map(|(_, _, m)| m)
                .collect()
        }
        None => memories.into_iter().take(max_memories).collect(),
    };

    if selected.is_empty() {
        return None;
    }

    let mut parts = vec!["---\n\nPrior Knowledge (from past sessions)\n".to_string()];
    for mem in &selected {
        let label = resolve_stored_label(mem.egress_label.as_ref(), cfg);
        let content = match filter_or_indicate_for_sink(
            &mem.content,
            &label,
            sink,
            Some("memory.priming"),
            IndicationVerbosity::Descriptive,
        ) {
            FilteredStoredContent::Allowed(c) => c,
            FilteredStoredContent::Withheld { indication } => indication,
        };
        let truncated: String = content.chars().take(500).collect();
        let session_ref = mem
            .tags
            .iter()
            .find_map(|t| t.strip_prefix("session:"))
            .map(|sid| sid.chars().take(8).collect::<String>());
        let prefix = if is_error_lesson_scope(&mem.scope) {
            "(error lesson) "
        } else {
            ""
        };
        match session_ref {
            Some(sid) => parts.push(format!("- {prefix}{truncated} [from session {sid}]")),
            None => parts.push(format!("- {prefix}{truncated}")),
        }
    }
    Some(parts.join("\n"))
}

/// Backward-compatible wrapper: composes system instructions with an optional
/// user context snippet but no persona.
pub(crate) fn compose_system_instructions_with_user_context(
    agent_instructions: &str,
    manifest: &AgentManifest,
    output_policy: Option<&autonoetic_types::agent::OutputPolicy>,
    user_context_snippet: Option<&str>,
) -> String {
    compose_system_instructions_full(agent_instructions, manifest, output_policy, user_context_snippet, None, None)
}

/// Concatenate core + extended SKILL.md sections for the system prompt.
///
/// The `<!-- extended -->` marker in `SKILL.md` was originally a "deferred
/// load via resolve" optimization (Phase 4 / PR #218), but an audit of
/// session-3b4485d4 found that agents with the marker never actually issued
/// `resolve("extended_instructions")` — they just operated on the core
/// section and silently lost critical guidance (e.g. promotion-gate handling,
/// "if evaluator finds issues" recipe).
///
/// Since #1015 the split is back, with the load mechanical instead of
/// agent-driven: the gateway composes the system prompt from the core half
/// until the agent's FIRST tool call, then injects the extended half as a
/// `gateway_note` on the first tool result and inlines it from then on (see
/// `AgentExecutor.extended_loaded`). This function is the "loaded" branch of
/// that gate.
pub(crate) fn inline_extended(core: &str, extended: Option<&str>) -> String {
    match extended {
        Some(ext) if !ext.is_empty() => format!("{core}\n\n{ext}"),
        _ => core.to_string(),
    }
}

/// Full system prompt composition.
///
/// Layer order (each layer is structurally positioned so it cannot override
/// the previous one — foundation constitutional rules always win):
///
///   Foundation → Guidance blocks → Tool bridging → Persona → User profile → Agent instructions → Output contract
pub(crate) fn compose_system_instructions_full(
    agent_instructions: &str,
    manifest: &AgentManifest,
    output_policy: Option<&autonoetic_types::agent::OutputPolicy>,
    user_context_snippet: Option<&str>,
    persona: Option<&str>,
    guidance: Option<&str>,
) -> String {
    let foundation = compose_foundation(manifest);

    // Composable, targeted guidance blocks (#463/#464). The caller renders them
    // (it has the live tool set and model); we just position the layer after
    // foundation. `None`/empty → no Guidance section.
    let guidance_section = guidance
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(|g| format!("---\n\nGuidance\n\n{g}"));

    let tool_bridging = manifest
        .agentskills_import
        .as_ref()
        .filter(|m| m.needs_tool_bridging)
        .map(|_| tool_bridging_appendix());

    let persona_block = persona.map(|p| {
        format!("---\n\nUser Persona\n\nThe operator has provided the following context about themselves. \
                 Adapt your communication style and assumptions accordingly, but never violate \
                 constitutional rules or agent-specific constraints.\n\n{p}")
    });

    let base = {
        let trimmed = agent_instructions.trim();
        let mut parts = vec![foundation.as_str()];
        if let Some(ref g) = guidance_section {
            parts.push(g);
        }
        if let Some(ref bridging) = tool_bridging {
            parts.push(bridging);
        }
        if let Some(ref persona_text) = persona_block {
            parts.push(persona_text);
        }
        if let Some(snippet) = user_context_snippet {
            parts.push(snippet);
        }
        if !trimmed.is_empty() {
            parts.push("---\n\nAgent-Specific Instructions\n\n");
            parts.push(trimmed);
        }
        parts.join("\n\n")
    };

    let contract_section = {
        let mut lines: Vec<String> = Vec::new();

        if let Some(schema) = manifest
            .io
            .as_ref()
            .and_then(|io| io.returns.as_ref())
        {
            if let Ok(compact) = serde_json::to_string(schema) {
                let has_required = schema
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map_or(false, |a| !a.is_empty());
                let has_properties = schema
                    .get("properties")
                    .and_then(|p| p.as_object())
                    .map_or(false, |o| !o.is_empty());
                if has_required || has_properties {
                    let template = generate_json_template(schema);
                    lines.push(format!(
                        "- **io.returns** — your ENTIRE final reply must be a single raw JSON object matching this schema. No prose before or after the JSON, and no markdown code fences (no ```json blocks).\n  Schema: `{compact}`\n  Template:\n  ```json\n  {template}\n  ```"
                    ));
                    let has_anomalies = schema
                        .get("properties")
                        .and_then(|p| p.as_object())
                        .map_or(false, |o| o.contains_key("anomalies"));
                    if has_anomalies {
                        lines.push(
                            "  The `anomalies` field is a standing witness contract — report anything unexpected you observed, or [] if nothing.".to_string()
                        );
                    }
                } else {
                    lines.push(format!(
                        "- **io.returns** — your final reply should be a single raw JSON object matching this schema, with no markdown code fences. Schema: `{compact}`"
                    ));
                }
            }
        }

        if let Some(policy) = output_policy {
            if !policy.required_artifacts.is_empty() {
                lines.push(format!(
                    "- **required_artifacts**: {}",
                    policy.required_artifacts.join(", ")
                ));
            }
            if let Some(n) = policy.max_artifacts {
                lines.push(format!("- **max_artifacts**: {n}"));
            }
            if let Some(n) = policy.max_total_size_mb {
                lines.push(format!("- **max_total_size_mb**: {n}"));
            }
            if let Some(n) = policy.max_reply_length_chars {
                lines.push(format!("- **max_reply_length_chars**: {n}"));
            }
            if let Some(n) = policy.min_artifact_builds {
                lines.push(format!(
                    "- **min_artifact_builds**: {n} (durable `artifact.build` trace required)"
                ));
            }
            if !policy.prohibited_text_patterns.is_empty() {
                lines.push(format!(
                    "- **prohibited_text_patterns**: {}",
                    policy.prohibited_text_patterns.join(", ")
                ));
            }
            lines.push(format!(
                "- **validation_max_loops**: {}",
                policy.validation_max_loops
            ));
        }

        if lines.is_empty() {
            None
        } else {
            Some(format!(
                "---\n\nYour Output Contract\n\nThe gateway will validate your final output against these constraints before returning it to the caller. Violating constraints triggers a repair prompt; repairs are bounded by the declared policy.\n\n{}",
                lines.join("\n")
            ))
        }
    };

    match contract_section {
        Some(section) => format!("{base}\n\n{section}"),
        None => base,
    }
}

/// Build a bounded user context snippet for system prompt injection.
/// Returns None if the scope is task_only or profile has no data.
pub(crate) fn render_user_context_snippet(
    profile: &autonoetic_types::agent::UserProfileRecord,
    scope: &autonoetic_types::agent::BindingScope,
) -> Option<String> {
    use autonoetic_types::agent::BindingScope;

    match scope {
        BindingScope::TaskOnly => None,
        BindingScope::Full | BindingScope::Restricted => {
            let json_str = profile.profile_json.as_ref()?;
            let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;

            let filtered = if *scope == BindingScope::Restricted {
                let mut restricted = serde_json::Map::new();
                if let Some(obj) = parsed.as_object() {
                    for key in &["preferences", "constraints"] {
                        if let Some(val) = obj.get(*key) {
                            restricted.insert((*key).to_string(), val.clone());
                        }
                    }
                }
                serde_json::Value::Object(restricted)
            } else {
                parsed
            };

            if filtered.is_null()
                || (filtered.is_object() && filtered.as_object().unwrap().is_empty())
            {
                return None;
            }

            let compact = serde_json::to_string(&filtered).ok()?;
            // Bound to ~2000 chars (~500 tokens)
            let bounded = if compact.len() > 2000 {
                format!("{}...", safe_prefix_by_bytes(&compact, 2000))
            } else {
                compact
            };

            Some(format!(
                "---\n\nUser Profile Context\n\nYou have access to this user's profile data (scope: {}). Use it to personalize your behavior.\n\n```json\n{}\n```",
                scope, bounded
            ))
        }
    }
}

pub(crate) fn safe_prefix_by_bytes(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// User-visible workflow status for chat, turn completion, and JSON-RPC `assistant_reply`.
/// When the model produced no assistant text, include a truncated "last intent" snippet from
/// the compact summary so the user sees what completed.
pub(crate) fn workflow_status_user_message_for_chat(summary: &str, planner_text_empty: bool) -> String {
    let head = summary
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Workflow updated.");
    let mut out = format!("**Workflow status:** {}", head);
    if !planner_text_empty {
        return out;
    }
    if let Some(pos) = summary.find("last intent") {
        let tail = &summary[pos..];
        if let Some(colon) = tail.find(':') {
            let body = tail[colon + 1..].trim();
            if !body.is_empty() {
                const MAX: usize = 1200;
                let snippet = if body.len() > MAX {
                    format!("{}…", safe_prefix_by_bytes(body, MAX))
                } else {
                    body.to_string()
                };
                out.push_str("\n\n");
                out.push_str(&snippet);
            }
        }
    }
    out
}

impl AgentExecutor {
    /// Build user context snippet for system prompt injection.
    pub(crate) fn build_user_context_snippet(&self) -> Option<String> {
        let user_id = self.user_id.as_ref()?;
        let store = self.gateway_store.as_ref()?;
        let agent_id = &self.manifest.agent.id;

        let binding = store.get_user_binding(user_id, agent_id).ok()??;
        let profile = store.get_user_profile(user_id).ok()??;

        render_user_context_snippet(&profile, &binding.scope)
    }

    /// Build memory context snippet from Tier-2 global memories for session continuity.
    pub(crate) fn build_memory_context_snippet(&self) -> Option<String> {
        let store = self.gateway_store.as_ref()?;
        let config = self.config.as_ref()?;
        let agent_id = &self.manifest.agent.id;
        let limit = config.profile.memory_priming_limit();
        // No config means no memory priming at all (early return above).
        // With config present, `task_matched_recall` (default true) gates
        // relevance ranking; an explicit `false` preserves pure recency.
        let task_matched = config.auto_learning.task_matched_recall;
        let task_text = task_matched
            .then_some(self.initial_user_message.as_str())
            .filter(|t| !t.trim().is_empty());
        // Fail closed to RemoteModel unless the session is already local-tainted
        // (in which case LocalModel priming is allowed).
        let query_sink = {
            let t = crate::runtime::egress_labeler::session_accumulated_taint(&self.egress_labels);
            if t.allows(autonoetic_types::egress::Sink::RemoteModel) {
                None
            } else {
                Some(autonoetic_types::egress::Sink::LocalModel)
            }
        };
        build_memory_context_snippet(
            store,
            agent_id,
            limit,
            task_text,
            query_sink,
            Some(&config.egress),
        )
    }

    /// Compose, sign, and render the R++1 state-attestation tail for the
    /// current turn. Returns:
    ///   - `Ok(Some(tail))` whenever the gateway has a directory to keep
    ///     the identity key in (the production path);
    ///   - `Ok(None)` when `gateway_dir` is unset (some unit-test paths
    ///     run an executor without persistent state — there is no key to
    ///     sign with and no operational state to attest to);
    ///   - `Err(_)` fail-shut whenever the key file is malformed or the
    ///     filesystem refuses to honour the strict permissions. The
    ///     surrounding turn must abort rather than proceed without a
    ///     trustworthy attestation.
    pub(crate) fn build_state_attestation_tail(&self) -> anyhow::Result<Option<String>> {
        let Some(gateway_dir) = self.gateway_dir.as_ref() else {
            return Ok(None);
        };
        let key = crate::runtime::crypto::GatewayIdentityKey::load_or_generate(gateway_dir)?;

        let pending_approval_ids = self
            .session_id
            .as_ref()
            .and_then(|sid| self.config.as_ref().map(|cfg| (cfg.as_ref(), sid.as_str())))
            .map(|(cfg, sid)| {
                crate::scheduler::approval::pending_approval_requests_for_session(
                    cfg,
                    self.gateway_store.as_deref(),
                    sid,
                )
                .map(|reqs| reqs.into_iter().map(|r| r.request_id).collect::<Vec<_>>())
            })
            .transpose()?
            .unwrap_or_default();

        let pending_user_interaction_ids = self
            .session_id
            .as_ref()
            .and_then(|_| self.gateway_store.as_deref())
            .map(|store| {
                let sid = self.session_id.as_deref().unwrap();
                store
                    .get_pending_interactions_for_session(sid)
                    .map(|interactions| {
                        interactions
                            .into_iter()
                            .map(|i| i.interaction_id)
                            .collect::<Vec<_>>()
                    })
            })
            .transpose()?
            .unwrap_or_default();

        let pending_escalation_ids = self
            .session_id
            .as_ref()
            .and_then(|sid| self.config.as_ref().map(|cfg| (cfg.as_ref(), sid.as_str())))
            .map(|(cfg, sid)| {
                crate::scheduler::approval::pending_approval_requests_for_session(
                    cfg,
                    self.gateway_store.as_deref(),
                    sid,
                )
                .map(|reqs| {
                    reqs.into_iter()
                        .filter(|r| matches!(r.action, autonoetic_types::background::ScheduledAction::SessionEscalate { .. }))
                        .map(|r| r.request_id)
                        .collect::<Vec<_>>()
                })
            })
            .transpose()?
            .unwrap_or_default();

        // #772 A.2: surface this agent's own still-pending constitutional
        // proposals and anomaly flags in its signed per-turn block —
        // "voice with amnesia is no voice". Mirrors the pending_escalation_ids
        // gathering above. The store queries filter to non-terminal statuses
        // in SQL (before the LIMIT) so terminal decisions can't displace
        // still-pending items from the bounded window, and errors propagate:
        // a signed "authoritative" attestation must not silently omit civic
        // items because a query failed.
        let (pending_proposal_ids, pending_flag_ids) = match self.gateway_store.as_deref() {
            Some(store) => {
                let proposals = store.list_pending_constitutional_proposals(
                    Some(&self.manifest.agent.id),
                    64,
                )?;
                let flags =
                    store.list_pending_anomaly_flags(Some(&self.manifest.agent.id), 64)?;
                (
                    proposals.into_iter().map(|p| p.proposal_id).collect(),
                    flags.into_iter().map(|f| f.flag_id).collect(),
                )
            }
            None => (Vec::new(), Vec::new()),
        };

        // #771 D.2: surface open amendment invitations addressed to this
        // agent in the same signed civic line. Carried as one-line
        // summaries (rule + denial count) rather than bare ids — the agent
        // did not file these, so the friction evidence IS the message.
        // Same error-propagation contract as proposals/flags above: a
        // signed attestation must not silently omit civic items.
        let pending_invitations = match self.gateway_store.as_deref() {
            Some(store) => store
                .list_amendment_invitations(Some("open"), Some(&self.manifest.agent.id), 64)?
                .into_iter()
                .map(|inv| crate::runtime::state_attestation::InvitationSummary {
                    invitation_id: inv.invitation_id,
                    rule_id: inv.rule_id,
                    denial_count: inv.denial_count,
                })
                .collect(),
            None => Vec::new(),
        };

        let budget_meters = self.snapshot_budget_meters();

        // RFC #778 Part D: compute burn-rate forecast from the budget meters
        // and turn counter. Pre-committed formula, no gateway judgment:
        // tokens_per_turn = used_tokens / turn_counter (turn > 0)
        // projected_turns_remaining = remaining_tokens / tokens_per_turn
        let burn_rate = compute_burn_rate(&budget_meters, self.turn_counter);

        let gateway_node_id = crate::execution::gateway_actor_id();

        // Bind the active constitution (version + digest) into the signed
        // per-turn block so non-retroactivity (Ri-0.10) is a verified fact,
        // not an on-demand lookup. The constitution is initialized at gateway
        // startup before any turn runs, so these accessors are always live
        // here. Fetched in the caller to keep `state_attestation` free of
        // upward dependencies.
        let constitution_version = crate::constitution_digest::constitution_version();
        let constitution_digest = crate::constitution_digest::constitution_digest();

        // The concrete model this spawn is actually running (preset-resolved,
        // session-override-aware). Authoritative per-turn value — mirrors what
        // tracing and guidance gating use. Provider config/endpoints are
        // intentionally excluded; only the model id is attested.
        let model_id = self.resolved_model_id();

        let attestation = crate::runtime::state_attestation::compose_and_sign(
            crate::runtime::state_attestation::AttestationInputs {
                agent_id: &self.manifest.agent.id,
                session_id: self.session_id.as_deref(),
                root_session_id: self.root_session_id_opt(),
                turn_counter: self.turn_counter,
                manifest: &self.manifest,
                gateway_node_id: &gateway_node_id,
                pending_approval_ids,
                pending_user_interaction_ids,
                pending_escalation_ids,
                pending_proposal_ids,
                pending_flag_ids,
                pending_invitations,
                budget_meters,
                burn_rate,
                constitution_version: &constitution_version,
                constitution_digest: &constitution_digest,
                model_id: &model_id,
            },
            &key,
        )?;

        Ok(Some(crate::runtime::state_attestation::render_tail(
            &attestation,
        )?))
    }

    /// Build Ri-0.5 degraded-mode notice text injected into the system prompt
    /// before the next turn executes.
    ///
    /// Constitutional requirement:
    /// - agent is told it is degraded,
    /// - rule IDs are explicit,
    /// - trigger evidence is explicit.
    pub(crate) fn build_degradation_notice_tail(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        if self.session_state != autonoetic_types::agent::SessionState::Degraded {
            return Ok(None);
        }

        let store = match self.gateway_store.as_ref() {
            Some(s) => s,
            None => {
                return Ok(Some(format!(
                    "[Ri-0.5 DEGRADED NOTICE] Session '{}' is in degraded mode. \
                     Trigger evidence unavailable (no gateway store).",
                    session_id
                )));
            }
        };

        let root_sid = crate::runtime::content_store::root_session_id(session_id);
        let degraded_event = store
            .search_causal_events(Some(session_id), None, 128)?
            .into_iter()
            .find(|event| event.category == "session" && event.action == "session.degraded")
            .or_else(|| {
                if root_sid != session_id {
                    store
                        .search_causal_events(Some(root_sid), None, 128)
                        .ok()?
                        .into_iter()
                        .find(|e| e.category == "session" && e.action == "session.degraded")
                } else {
                    None
                }
            });

        let Some(degraded_event) = degraded_event else {
            return Ok(Some(format!(
                "[Ri-0.5 DEGRADED NOTICE] Session '{}' is in degraded mode. \
                 session.degraded causal event not found.",
                session_id
            )));
        };

        anyhow::ensure!(
            !degraded_event.enforced_rules.is_empty(),
            "Ri-0.5 violation: session.degraded event '{}' has no enforced rule IDs",
            degraded_event.event_id
        );

        let evidence = degraded_event
            .payload
            .clone()
            .or_else(|| {
                degraded_event
                    .reason
                    .clone()
                    .map(|reason| serde_json::json!({ "reason": reason }).to_string())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Ri-0.5 violation: session.degraded event '{}' has no evidence payload",
                    degraded_event.event_id
                )
            })?;

        let rules = degraded_event.enforced_rules.join(", ");
        Ok(Some(format!(
            "---\n\nDegradation Notice (Ri-0.5)\n\n\
             This session is in degraded mode before this turn executes.\n\
             Rule IDs: {}\n\
             Evidence Event: {}\n\
             Evidence: {}\n",
            rules, degraded_event.event_id, evidence
        )))
    }
}

#[cfg(test)]
mod inline_extended_tests {
    use super::inline_extended;

    #[test]
    fn no_extended_returns_core_unchanged() {
        assert_eq!(inline_extended("core only", None), "core only");
    }

    #[test]
    fn empty_extended_returns_core_unchanged() {
        assert_eq!(inline_extended("core only", Some("")), "core only");
    }

    #[test]
    fn concatenates_with_blank_line_separator() {
        assert_eq!(
            inline_extended("core part", Some("extended part")),
            "core part\n\nextended part"
        );
    }
}

#[cfg(test)]
mod agentskills_bridging_tests {
    use super::*;
    use autonoetic_types::agent::AgentSkillsImportMetadata;

    #[test]
    fn tool_bridging_injected_for_agentskills_import() {
        let mut manifest = default_test_manifest();
        manifest.agentskills_import = Some(AgentSkillsImportMetadata {
            license: Some("MIT".to_string()),
            compatibility: Some("claude-code".to_string()),
            allowed_tools: vec!["Bash(*)".to_string(), "Read".to_string()],
            needs_tool_bridging: true,
            capabilities_inferred: true,
        });

        let output = compose_system_instructions_with_metadata(
            "Do git things with Bash(git log).",
            &manifest,
            None,
        );

        assert!(
            output.contains("Tool Compatibility Notes"),
            "should include tool bridging appendix"
        );
        assert!(
            output.contains("Bash(command)"),
            "should contain Bash mapping"
        );
        assert!(
            output.contains("resolve"),
            "should contain content.read mapping"
        );
        assert!(
            output.contains("Do git things with Bash(git log)."),
            "should still contain agent instructions"
        );
    }

    #[test]
    fn no_tool_bridging_without_agentskills_import() {
        let manifest = default_test_manifest();
        let output = compose_system_instructions_with_metadata("Do things.", &manifest, None);
        assert!(
            !output.contains("Tool Compatibility Notes"),
            "should not include tool bridging for native agents"
        );
    }

    #[test]
    fn output_contract_states_no_markdown_fences() {
        // The io.returns renderer owns the "single raw JSON, no fences"
        // instruction now (#466 dedup) — agents no longer restate it.
        let mut manifest = default_test_manifest();
        manifest.io = Some(autonoetic_types::agent::AgentIO {
            accepts: None,
            returns: Some(serde_json::json!({
                "type": "object",
                "required": ["status"],
                "properties": { "status": { "type": "string" } }
            })),
            returns_enforcement: None,
            output_policy: None,
        });
        let output = compose_system_instructions_with_metadata("Do things.", &manifest, None);
        assert!(output.contains("Your Output Contract"));
        assert!(
            output.contains("no markdown code fences"),
            "io.returns contract must instruct no fences: {output}"
        );
    }

    #[test]
    fn output_contract_states_anomalies_witness_line_when_present() {
        // RFC C.2 (#770): when the (gateway-augmented) io.returns schema
        // carries an `anomalies` property, the Output Contract renders the
        // standing-witness doctrine line once, in addition to the schema.
        let mut manifest = default_test_manifest();
        manifest.io = Some(autonoetic_types::agent::AgentIO {
            accepts: None,
            returns: Some(serde_json::json!({
                "type": "object",
                "required": ["status", "anomalies"],
                "properties": {
                    "status": { "type": "string" },
                    "anomalies": { "type": "array" }
                }
            })),
            returns_enforcement: None,
            output_policy: None,
        });
        let output = compose_system_instructions_with_metadata("Do things.", &manifest, None);
        assert!(output.contains("Your Output Contract"));
        assert!(output.contains("anomalies"));
        assert!(
            output.contains("standing witness contract"),
            "expected the anomalies witness-contract doctrine line: {output}"
        );
    }

    #[test]
    fn output_contract_omits_anomalies_line_when_absent() {
        let mut manifest = default_test_manifest();
        manifest.io = Some(autonoetic_types::agent::AgentIO {
            accepts: None,
            returns: Some(serde_json::json!({
                "type": "object",
                "required": ["status"],
                "properties": { "status": { "type": "string" } }
            })),
            returns_enforcement: None,
            output_policy: None,
        });
        let output = compose_system_instructions_with_metadata("Do things.", &manifest, None);
        assert!(
            !output.contains("standing witness contract"),
            "no anomalies property declared; witness line must not appear: {output}"
        );
    }

    #[test]
    fn planner_system_prompt_includes_sentinel_self_correction_guidance() {
        // D.7b: planner role must receive the Sentinel self-correction builtin
        // guidance block so it treats sentinel_notice as an advisory signal
        // rather than bouncing to the operator.
        use crate::runtime::guidance::{builtin_blocks, compose_guidance, GuidanceContext};
        use autonoetic_types::capability::Capability;

        let caps = vec![Capability::AgentSpawn {
            max_children: 5,
            max_spawn_depth: 0,
        }];
        let ctx = GuidanceContext {
            capabilities: &caps,
            active_tool_names: &[],
            model_family: None,
            role: Some("planner"),
        };
        let guidance = compose_guidance(&builtin_blocks(), &ctx);
        assert!(
            guidance.contains("Sentinel notices are advisory"),
            "planner guidance must include Sentinel self-correction doctrine: {guidance}"
        );
        assert!(
            guidance.contains("self-correct, don't ask"),
            "planner guidance must direct self-correction rather than operator escalation: {guidance}"
        );
    }

    #[test]
    fn non_planner_system_prompt_excludes_sentinel_self_correction_guidance() {
        // D.7b: the planner-specific Sentinel guidance must not leak to other roles.
        use crate::runtime::guidance::{builtin_blocks, compose_guidance, GuidanceContext};

        let ctx = GuidanceContext {
            capabilities: &[],
            active_tool_names: &[],
            model_family: None,
            role: Some("coder"),
        };
        let guidance = compose_guidance(&builtin_blocks(), &ctx);
        assert!(
            !guidance.contains("Sentinel notices are advisory"),
            "coder should not receive planner Sentinel guidance: {guidance}"
        );
    }

    fn default_test_manifest() -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: autonoetic_types::agent::RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: autonoetic_types::agent::AgentIdentity {
                id: "test".to_string(),
                name: "Test".to_string(),
                description: "Test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
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
            execution_mode: Default::default(),
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
            egress: None,
        }
    }
}

fn generate_json_template(schema: &serde_json::Value) -> String {
    let props = schema
        .get("properties")
        .and_then(|p| p.as_object());
    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let Some(props) = props else {
        return "{}".to_string();
    };

    let mut obj = serde_json::Map::new();
    for (key, prop_schema) in props {
        let placeholder = if required.contains(key.as_str()) {
            json_placeholder(prop_schema, false)
        } else {
            json_placeholder(prop_schema, true)
        };
        obj.insert(key.clone(), placeholder);
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap_or_else(|_| "{}".to_string())
}

fn json_placeholder(prop_schema: &serde_json::Value, optional: bool) -> serde_json::Value {
    let type_str = prop_schema.get("type").and_then(|t| t.as_str()).unwrap_or("string");
    match type_str {
        "string" => {
            let hint = if optional { "..." } else { "(required)" };
            serde_json::Value::String(hint.to_string())
        }
        "boolean" => serde_json::Value::Bool(false),
        "integer" | "number" => serde_json::Value::Number(0.into()),
        "array" => serde_json::Value::Array(vec![]),
        "object" => serde_json::Value::Object(serde_json::Map::new()),
        _ => serde_json::Value::String(format!("({})", type_str)),
    }
}

#[cfg(test)]
mod workflow_status_chat_tests {
    use super::*;

    #[test]
    fn workflow_chat_planner_nonempty_only_headline() {
        let s = "workflow wf-abc · 2 done [RESUMABLE]\n  last intent (v3): long details here";
        let m = workflow_status_user_message_for_chat(s, false);
        assert!(m.starts_with("**Workflow status:**"));
        assert!(m.contains("wf-abc"));
        assert!(!m.contains("long details"));
    }

    #[test]
    fn workflow_chat_planner_empty_includes_intent_snippet() {
        let s = "workflow wf-abc · 2 done [RESUMABLE]\n  last intent (v3): Done with task.";
        let m = workflow_status_user_message_for_chat(s, true);
        assert!(m.contains("wf-abc"));
        assert!(m.contains("Done with task."));
    }

    #[test]
    fn json_template_coder_schema() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["status"],
            "properties": {
                "status": { "type": "string" },
                "artifact_ref": { "type": "string" },
                "clarification_request": { "type": "object" },
                "reason": { "type": "string" },
                "dependency_files": { "type": "array", "items": { "type": "string" } }
            }
        });
        let tmpl = generate_json_template(&schema);
        eprintln!("{}", tmpl);
        assert!(tmpl.contains("\"status\": \"(required)\""));
        assert!(tmpl.contains("\"artifact_ref\": \"...\""));
        assert!(tmpl.contains("\"clarification_request\": {}"));
        assert!(tmpl.contains("\"dependency_files\": []"));
    }

    #[test]
    fn json_template_empty_schema() {
        let schema = serde_json::json!({"type": "object"});
        let tmpl = generate_json_template(&schema);
        assert_eq!(tmpl, "{}");
    }

    #[test]
    fn burn_rate_none_on_turn_zero() {
        assert!(compute_burn_rate(&[], 0).is_none());
    }

    #[test]
    fn burn_rate_none_without_token_meter() {
        use crate::runtime::state_attestation::BudgetMeter;
        let meters = vec![BudgetMeter {
            name: "llm_rounds".to_string(),
            used: 5.0,
            limit: Some(20.0),
        }];
        assert!(compute_burn_rate(&meters, 5).is_none());
    }

    #[test]
    fn burn_rate_shows_remaining_even_when_no_usage() {
        // On turn 1 with no tokens used yet, the forecast still shows
        // remaining_tokens so the agent knows its ceiling.
        use crate::runtime::state_attestation::BudgetMeter;
        let meters = vec![BudgetMeter {
            name: "llm_tokens".to_string(),
            used: 0.0,
            limit: Some(10000.0),
        }];
        let forecast = compute_burn_rate(&meters, 1).expect("forecast");
        assert_eq!(forecast.tokens_per_turn, 0.0);
        assert_eq!(forecast.remaining_tokens, Some(10000.0));
        assert!(forecast.projected_turns_remaining.is_none());
    }

    #[test]
    fn burn_rate_computes_projected_turns() {
        use crate::runtime::state_attestation::BudgetMeter;
        // 5000 tokens used over 10 turns = 500 tokens/turn.
        // Limit 10000, used 5000 → remaining 5000.
        // Projected turns = 5000 / 500 = 10.
        let meters = vec![
            BudgetMeter {
                name: "llm_rounds".to_string(),
                used: 10.0,
                limit: Some(20.0),
            },
            BudgetMeter {
                name: "llm_tokens".to_string(),
                used: 5000.0,
                limit: Some(10000.0),
            },
        ];
        let forecast = compute_burn_rate(&meters, 10).expect("forecast");
        assert!((forecast.tokens_per_turn - 500.0).abs() < 0.01);
        assert_eq!(forecast.remaining_tokens, Some(5000.0));
        assert_eq!(forecast.projected_turns_remaining, Some(10.0));
    }

    #[test]
    fn burn_rate_no_projection_without_limit() {
        use crate::runtime::state_attestation::BudgetMeter;
        let meters = vec![BudgetMeter {
            name: "llm_tokens".to_string(),
            used: 5000.0,
            limit: None,
        }];
        let forecast = compute_burn_rate(&meters, 10).expect("forecast");
        assert!((forecast.tokens_per_turn - 500.0).abs() < 0.01);
        assert!(forecast.remaining_tokens.is_none());
        assert!(forecast.projected_turns_remaining.is_none());
    }
}

/// RFC #778 Part D — compute the burn-rate forecast from budget meters and
/// the current turn counter.
///
/// Pre-committed formula (no gateway judgment):
/// - `tokens_per_turn` = `llm_tokens.used / turn_counter` (when turn > 0)
/// - `remaining_tokens` = `llm_tokens.limit - llm_tokens.used`
/// - `projected_turns_remaining` = `remaining_tokens / tokens_per_turn`
///
/// Returns `None` when there is not enough data (turn 0, no token budget, or
/// budgets disabled).
fn compute_burn_rate(
    budget_meters: &[crate::runtime::state_attestation::BudgetMeter],
    turn_counter: u64,
) -> Option<crate::runtime::state_attestation::BurnRateForecast> {
    use crate::runtime::state_attestation::BurnRateForecast;

    if turn_counter == 0 {
        return None;
    }

    let token_meter = budget_meters
        .iter()
        .find(|m| m.name == "llm_tokens")?;

    // tokens_per_turn is 0.0 when no tokens have been used yet — the forecast
    // still carries remaining_tokens so the agent can see its budget ceiling.
    let tokens_per_turn = token_meter.used / turn_counter as f64;
    let remaining_tokens = token_meter.remaining();
    let projected_turns_remaining = match remaining_tokens {
        Some(rem) if tokens_per_turn > 0.0 => Some(rem / tokens_per_turn),
        _ => None,
    };

    Some(BurnRateForecast {
        tokens_per_turn,
        remaining_tokens,
        projected_turns_remaining,
    })
}

#[cfg(test)]
mod injected_recall_tests {
    use super::*;
    use crate::scheduler::gateway_store::GatewayStore;
    use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};

    #[test]
    fn scorer_matches_related_memory_and_zero_for_unrelated() {
        let task = "fetch weather data from api";
        let related = score_task_relevance(task, "weather api requires retry on 429");
        let unrelated = score_task_relevance(task, "unrelated database migration note");
        assert!(related > 0.0, "expected positive score, got {related}");
        assert_eq!(unrelated, 0.0, "expected zero score, got {unrelated}");
    }

    fn seed_memory(
        store: &GatewayStore,
        id: &str,
        agent_id: &str,
        scope: &str,
        content: &str,
        session: &str,
        updated_at: &str,
    ) {
        let mut mem = MemoryObject::new(
            id.to_string(),
            scope.to_string(),
            agent_id.to_string(),
            agent_id.to_string(),
            format!("session:{session}:post_digest"),
            content.to_string(),
        );
        mem.source_type = MemorySourceType::SessionDigest;
        mem.tags = vec![
            "source:post_session_digest".to_string(),
            format!("session:{session}"),
            format!("agent:{agent_id}"),
        ];
        mem.visibility = MemoryVisibility::Global;
        mem.created_at = updated_at.to_string();
        mem.updated_at = updated_at.to_string();
        store.memory_upsert(&mem).unwrap();
    }

    #[test]
    fn task_matched_recall_prefers_relevant_over_recent() {
        let temp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();
        let agent_id = "coder.default";

        seed_memory(
            &store,
            "mem-relevant",
            agent_id,
            "digest.lesson",
            "weather api requires retry on 429 rate limits",
            "sess-relevant-aaaaaaaa",
            "2026-01-01T00:00:00Z",
        );
        seed_memory(
            &store,
            "mem-recent-irrelevant",
            agent_id,
            "digest.fact",
            "unrelated database migration note about schema versions",
            "sess-recent-bbbbbbbb",
            "2026-06-01T00:00:00Z",
        );

        let snippet = build_memory_context_snippet(
            &store,
            agent_id,
            1,
            Some("fetch weather data from api"),
            None,
            None,
        )
        .expect("expected snippet");

        assert!(
            snippet.contains("weather api requires retry"),
            "expected relevant memory to be selected: {snippet}"
        );
        assert!(
            !snippet.contains("database migration"),
            "irrelevant, more recent memory should not have been selected: {snippet}"
        );
    }

    #[test]
    fn snippet_includes_provenance_and_error_lesson_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();
        let agent_id = "coder.default";

        seed_memory(
            &store,
            "mem-error",
            agent_id,
            "digest.error_pattern",
            "weather api call failed with 429 without retry",
            "sess-errsession",
            "2026-01-01T00:00:00Z",
        );

        let snippet = build_memory_context_snippet(
            &store,
            agent_id,
            1,
            Some("fetch weather data from api"),
            None,
            None,
        )
        .expect("expected snippet");

        assert!(
            snippet.contains("[from session sess-err"),
            "expected provenance suffix: {snippet}"
        );
        assert!(
            snippet.contains("(error lesson)"),
            "expected error-lesson prefix: {snippet}"
        );
    }

    #[test]
    fn other_agents_digests_are_excluded_when_agent_has_own_memories() {
        let temp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();

        seed_memory(
            &store,
            "mem-own",
            "coder.default",
            "digest.lesson",
            "own lesson about api retries",
            "sess-own-1",
            "2026-01-01T00:00:00Z",
        );
        // search_memories_by_tags ORs its tags, so without the conjunction
        // re-filter this newer foreign row would enter the candidate pool.
        seed_memory(
            &store,
            "mem-foreign",
            "researcher.default",
            "digest.lesson",
            "foreign lesson about api retries",
            "sess-foreign-1",
            "2026-06-01T00:00:00Z",
        );

        let snippet =
            build_memory_context_snippet(
            &store,
            "coder.default",
            5,
            Some("api retries"),
            None,
            None,
        )
                .expect("expected snippet");
        assert!(snippet.contains("own lesson"), "own memory expected: {snippet}");
        assert!(
            !snippet.contains("foreign lesson"),
            "another agent's digest must not appear: {snippet}"
        );
    }

    #[test]
    fn no_task_text_preserves_recency_order() {
        let temp = tempfile::tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();
        let agent_id = "coder.default";

        seed_memory(
            &store,
            "mem-older",
            agent_id,
            "digest.fact",
            "older fact about unrelated topic",
            "sess-older-1",
            "2026-01-01T00:00:00Z",
        );
        seed_memory(
            &store,
            "mem-newer",
            agent_id,
            "digest.fact",
            "newer fact about unrelated topic",
            "sess-newer-2",
            "2026-06-01T00:00:00Z",
        );

        let snippet = build_memory_context_snippet(
            &store,
            agent_id,
            2,
            None,
            None,
            None,
        )
            .expect("expected snippet");

        let lines: Vec<&str> = snippet.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("newer fact") && lines[1].contains("older fact"),
            "expected recency (newest first) order preserved when task_text is None: {snippet}"
        );
    }
}
