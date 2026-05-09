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
        )
    });

    if has_workflow_caps || !is_script_mode {
        parts.push(FOUNDATION_WORKFLOW.trim());
    }

    if has_artifact_caps {
        parts.push(FOUNDATION_ARTIFACT.trim());
    }

    if is_script_mode {
        parts.push(FOUNDATION_SCRIPT.trim());
    }

    if has_digest_cap {
        parts.push(FOUNDATION_DIGEST.trim());
    }

    if has_code_execution {
        parts.push(FOUNDATION_SDK.trim());
    }

    parts.join("\n\n---\n\n")
}

const TOOL_BRIDGING_APPENDIX: &str = r#"---

Tool Compatibility Notes (auto-generated from AgentSkills import)

This skill was imported from the Agent Skills (agentskills.io) format.
The following tool mappings apply:

| Skill references | Autonoetic equivalent |
|---|---|
| `Bash(command)` | `sandbox_exec({"command": "command"})` |
| `Read(path)` | `content_read(name_or_handle)` — files must be loaded via content store |
| `Write(path, content)` | `content_write(name, content)` |
| `WebSearch(query)` | `web_search({"query": "query"})` |
| `WebFetch(url)` | `web_fetch({"url": "url"})` |

File paths referenced by the skill are available in the agent directory.
Use content_read/content_write or sandbox paths relative to the agent working directory."#;

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
    compose_system_instructions_with_user_context(agent_instructions, manifest, output_policy, None)
}

/// Full system prompt composition with optional user context injection.
pub(crate) fn compose_system_instructions_with_user_context(
    agent_instructions: &str,
    manifest: &AgentManifest,
    output_policy: Option<&autonoetic_types::agent::OutputPolicy>,
    user_context_snippet: Option<&str>,
) -> String {
    let foundation = compose_foundation(manifest);

    let tool_bridging = manifest
        .agentskills_import
        .as_ref()
        .filter(|m| m.needs_tool_bridging)
        .map(|_| tool_bridging_appendix());

    let base = {
        let trimmed = agent_instructions.trim();
        let mut parts = vec![foundation.as_str()];
        if let Some(ref bridging) = tool_bridging {
            parts.push(bridging);
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
                lines.push(format!("- **io.returns** (your reply must conform): `{compact}`"));
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

        let budget_meters = self.snapshot_budget_meters();
        let gateway_node_id =
            std::env::var("AUTONOETIC_NODE_ID").unwrap_or_else(|_| "gateway".to_string());

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
                budget_meters,
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

        let store = self.gateway_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Ri-0.5 violation: degraded session '{}' has no gateway store for evidence lookup",
                session_id
            )
        })?;

        let degraded_event = store
            .search_causal_events(Some(session_id), None, 128)?
            .into_iter()
            .find(|event| event.category == "session" && event.action == "session.degraded")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Ri-0.5 violation: degraded session '{}' missing session.degraded causal event",
                    session_id
                )
            })?;

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
            output.contains("content_read"),
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
            },
            capabilities: vec![],
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
            agentskills_import: None,
            compression: None,
        }
    }
}

#[cfg(test)]
mod workflow_status_chat_tests {
    use super::workflow_status_user_message_for_chat;

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
}
