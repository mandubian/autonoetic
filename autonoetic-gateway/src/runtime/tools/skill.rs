//! `skill.install` — fetch a remote SKILL.md and register it as a local agent.
//!
//! Requires the `SkillInstall` capability. The `allowed_sources` field constrains
//! which URL hosts the agent may pull from.

use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::tools::{
    block_on_memory, extract_host, tier2_memory_for_native_tool, NativeTool, NativeToolRegistry,
};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, LlmConfig};
use autonoetic_types::capability::Capability;
use autonoetic_types::memory::{MemoryObject, MemoryVisibility};
use autonoetic_types::principal::PrincipalKind;
use autonoetic_types::tool_error::ToolError;
use gray_matter::Matter;
use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SkillInstallTool));
    registry.register(Box::new(SkillNormalizeTool));
}

pub struct SkillInstallTool;

#[derive(Debug, Deserialize)]
struct SkillInstallArgs {
    /// URL to a remote SKILL.md file.
    url: String,
    /// New agent ID to register the skill as.
    agent_id: String,
    /// Trust level applied to the imported capabilities.
    /// - "generous": capabilities from the SKILL.md are used as-is.
    /// - "strict" (default): capabilities preserved but every action requires approval.
    /// - "audit": read-only + approval gate, ignores original capabilities.
    #[serde(default)]
    trust_mode: Option<String>,
}

impl NativeTool for SkillInstallTool {
    fn name(&self) -> &'static str {
        "skill_install"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Fetch a remote SKILL.md and register it as a Candidate revision of a \
                new local agent (one door — every install faces the same promotion gates as an \
                agent built in-house). Requires the SkillInstall capability. The agent directory \
                is created under agents_dir, the skill is parsed, a runtime.lock is generated, \
                and the bundle is bootstrapped as a Candidate revision — it is NOT activated. \
                Promote it with agent_revision_promote, which applies the standard risk-graduated \
                evidence gates (P-9.9) and the P-2.25 operator approval of the capability delta. \
                Rejects execution_mode: script skills up front (skill_install fetches only the \
                SKILL.md; a script entrypoint would never be fetched). trust_mode controls which \
                capabilities the Candidate carries into the gate: generous (declared/inferred \
                capabilities as-is), strict (default: drops any high-risk capability that was \
                inferred rather than explicitly declared, then adds ApprovalQueue, which enables \
                admin-proposal filing and the Workflow tool tier — it does not gate declared \
                capabilities), audit (ReadAccess(self.*) + ApprovalQueue only, declared \
                capabilities ignored). Inference from allowed-tools never mints wildcard power: \
                Bash proposes SandboxFunctions prefixes only (never CodeExecution), and \
                WebSearch/WebFetch/Fetch propose NetworkAccess with an empty hosts list — shell \
                execution and concrete network hosts require an explicit \
                metadata.autonoetic.capabilities declaration. See the response's `warnings` \
                field for what was clamped."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Full URL to a SKILL.md file, e.g. https://agentskills.io/skills/web-researcher/SKILL.md"
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "ID for the newly installed agent, e.g. \"web-researcher.default\". \
                            May only contain ASCII letters, digits, '.', '-', and '_'."
                    },
                    "trust_mode": {
                        "type": "string",
                        "enum": ["generous", "strict", "audit"],
                        "description": "Which capabilities the Candidate carries into the promotion gate. \
                            generous: declared/inferred capabilities as-is; \
                            strict (default): drops any high-risk capability that was inferred \
                            (not explicitly declared) and adds ApprovalQueue; \
                            audit: drop to read-only + approval gate, declared capabilities ignored."
                    }
                },
                "required": ["url", "agent_id"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::SkillInstall { .. }))
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        _agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: SkillInstallArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid arguments: {}", e))?;

        // ── 1. Validate agent_id ──────────────────────────────────────────────
        crate::runtime::tools::validate_agent_id(&args.agent_id)?;

        // ── 2. Transport: HTTPS only beyond loopback ──────────────────────────
        // A remote SKILL.md is a whole agent definition — fetched over
        // plaintext HTTP, a network MITM could substitute it wholesale
        // (#802 review). Plain HTTP is accepted only for loopback hosts
        // (local dev / tests). Checked before the capability gate: request
        // shape validation precedes authorization.
        if !url_scheme_is_fetch_safe(&args.url) {
            return Ok(ToolError::validation(
                format!("skill_install fetches SKILL.md over HTTPS only (got '{}') — plain HTTP is accepted only for loopback hosts", args.url),
                Some("Serve the skill over HTTPS, or use a loopback address for local development."),
            )
            .with_code("skill_install_insecure_scheme")
            .to_error_response());
        }

        // ── 2b. Policy: SkillInstall capability must permit this URL host ─────
        let url_host = extract_host(&args.url)?;
        if !policy.can_install_skill(&url_host).is_allowed() {
            return Ok(ToolError::permission(format!("SkillInstall capability does not permit fetching from host '{}'", url_host)).with_code("skill_install_host_denied").to_error_response());
        }

        // ── 3. Resolve config and paths ───────────────────────────────────────
        let config = config.ok_or_else(|| anyhow::anyhow!("Gateway config not available"))?;
        let gateway_dir =
            gateway_dir.ok_or_else(|| anyhow::anyhow!("Gateway directory not available"))?;

        // Use dot-to-dash conversion for the filesystem directory name (same as CLI).
        let dir_name = args.agent_id.replace('.', "-");
        let target_dir = config.agents_dir.join(&dir_name);

        anyhow::ensure!(
            !target_dir.exists(),
            "Agent directory '{}' already exists — choose a different agent_id or remove the existing agent first",
            target_dir.display()
        );

        // ── 4. Fetch the remote SKILL.md ──────────────────────────────────────
        let url_clone = args.url.clone();
        let (http_status, fetched_bytes) = {
            let result: anyhow::Result<(u16, Vec<u8>)> = (|| {
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .build()?;
                let resp = client.get(url_clone.as_str()).send()?;
                let status = resp.status().as_u16();
                if !resp.status().is_success() {
                    let _ = resp.bytes()?;
                    return Ok((status, Vec::new()));
                }
                let content = resp.bytes()?;
                Ok((status, content.to_vec()))
            })();
            result?
        };

        if !(200..300).contains(&(http_status as i32)) {
            return Ok(ToolError::execution(format!("HTTP {} fetching SKILL.md from {}", http_status, args.url), Some("Ensure the URL is accessible and retry.")).with_code("skill_fetch_failed").to_error_response());
        }

        // Provenance (RFC Part D): digest the exact fetched bytes, reusing the
        // content store's own SHA-256 helper rather than hashing ad hoc.
        let fetched_sha256_hex = crate::runtime::content_store::ContentStore::compute_handle(
            &fetched_bytes,
        )
        .trim_start_matches("sha256:")
        .to_string();

        // Decode the SKILL.md text from the same byte buffer the provenance
        // digest was computed over — hashing already-decoded text instead
        // could diverge from the exact fetched bytes (charset decoding /
        // invalid-byte replacement), weakening the provenance guarantee.
        let skill_content = String::from_utf8(fetched_bytes).map_err(|_| {
            anyhow::anyhow!("SKILL.md from {} is not valid UTF-8", args.url)
        })?;

        // ── 5. Parse the SKILL.md ─────────────────────────────────────────────
        let (parsed_manifest, body) = crate::runtime::parser::SkillParser::parse(&skill_content)
            .map_err(|e| {
                anyhow::anyhow!("Failed to parse remote SKILL.md from {}: {}", args.url, e)
            })?;

        // ── 5b. Reject script-mode imports before writing anything to disk ────
        // skill_install fetches exactly one file (the SKILL.md); a script-mode
        // manifest names an entrypoint that was never fetched, which would
        // silently produce a broken agent (RFC Part E.1).
        if matches!(parsed_manifest.execution_mode, ExecutionMode::Script) {
            return Ok(ToolError::validation(
                "skill_install fetches only the SKILL.md — a script-mode skill's entrypoint \
                 would never be fetched, producing a broken agent. Package the skill as an \
                 artifact and use the revision pipeline (agent_revision_create_from_intent), \
                 or import a reasoning-mode skill.",
                None::<String>,
            )
            .with_code("skill_install_script_mode_rejected")
            .to_error_response());
        }

        // ── 6. Apply trust mode ───────────────────────────────────────────────
        let trust_mode = args.trust_mode.as_deref().unwrap_or("strict");
        let (capabilities, capabilities_source) = apply_trust_mode(trust_mode, &parsed_manifest)?;

        // Install-time warnings (RFC Part C): when the applied set came from
        // allowed-tools inference rather than an explicit declaration, tell
        // the operator which power was clamped, so the gap is visible at
        // install rather than discovered later as "why can't this agent run
        // shell / reach the network".
        let inference_warnings = if capabilities_source == "inferred" {
            let allowed_tools: Vec<String> = parsed_manifest
                .agentskills_import
                .as_ref()
                .map(|m| m.allowed_tools.clone())
                .unwrap_or_default();
            capability_inference_warnings(&allowed_tools, trust_mode)
        } else {
            Vec::new()
        };

        // ── 7. Build target manifest ──────────────────────────────────────────
        let llm_config = parsed_manifest.llm_config.clone().or_else(|| {
            // Fall back to the gateway's default preset if the skill has no LLM config.
            config
                .llm_preset_mapping
                .get("default")
                .and_then(|name| config.llm_presets.get(name.as_str()))
                .map(|preset| LlmConfig {
                    provider: preset
                        .provider
                        .clone()
                        .unwrap_or_else(|| "anthropic".to_string()),
                    model: preset
                        .model
                        .clone()
                        .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
                    temperature: preset.temperature.unwrap_or(0.2),
                    fallback_provider: None,
                    fallback_model: None,
                    chat_only: preset.chat_only.unwrap_or(false),
                    context_window_tokens: None,
                    base_url: preset.base_url.clone(),
                    api_key_env: preset.api_key_env.clone(),
                    routing_preset: None,
                    thinking: preset.thinking.clone(),
                    egress_class: preset.egress_class,
                })
        });

        let target_manifest = AgentManifest {
            version: parsed_manifest.version.clone(),
            runtime: parsed_manifest.runtime.clone(),
            agent: AgentIdentity {
                id: args.agent_id.clone(),
                name: parsed_manifest.agent.name.clone(),
                description: parsed_manifest.agent.description.clone(),
                singleton: parsed_manifest.agent.singleton,
                resident_idle_ttl_secs: parsed_manifest.agent.resident_idle_ttl_secs,
            },
            capabilities,
            llm_overrides: parsed_manifest.llm_overrides.clone(),
            llm_preset: parsed_manifest.llm_preset.clone(),
            llm_config,
            limits: parsed_manifest.limits.clone(),
            background: parsed_manifest.background.clone(),
            disclosure: parsed_manifest.disclosure.clone(),
            io: parsed_manifest.io.clone(),
            middleware: parsed_manifest.middleware.clone(),
            execution_mode: parsed_manifest.execution_mode,
            script_entry: parsed_manifest.script_entry.clone(),
            script_input_mode: parsed_manifest.script_input_mode,
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: parsed_manifest.allowed_tool_tiers.clone(),
            excluded_tools: parsed_manifest.excluded_tools.clone(),
            agentskills_import: parsed_manifest.agentskills_import.clone(),
            compression: parsed_manifest.compression.clone(),
            open_web: parsed_manifest.open_web,
            sandbox_network: parsed_manifest.sandbox_network,
            egress: None,
        };

        // ── 8. Write agent directory: SKILL.md + runtime.lock ─────────────────
        std::fs::create_dir_all(&target_dir)?;

        let skill_doc =
            crate::runtime::install_contract::render_skill_document(&target_manifest, &body)?;
        std::fs::write(target_dir.join("SKILL.md"), &skill_doc)?;

        let lock_doc = crate::runtime::install_contract::render_runtime_lock_example();
        std::fs::write(target_dir.join("runtime.lock"), &lock_doc)?;

        tracing::info!(
            target: "skill_install",
            agent_id = %args.agent_id,
            url = %args.url,
            trust_mode = %trust_mode,
            "Wrote agent bundle to disk"
        );

        // ── 9. Bootstrap as a Candidate revision — one door (RFC Part A) ──────
        // skill_install never promotes; activation flows through the standard
        // agent_revision_promote gates. Provenance (RFC Part D) is carried on
        // the revision instead of the generic "cli"/"bootstrap" bootstrap uses
        // for the repo's own reference bundles.
        let source_ref = format!("{}#sha256={}", args.url, fetched_sha256_hex);
        let outcome = crate::bootstrap::bootstrap_single_agent_candidate_only(
            config,
            gateway_dir,
            &dir_name,
            PrincipalKind::AutonoeticAgent.tag(),
            &manifest.agent.id,
            "skill_install",
            Some(&source_ref),
        )?;

        let message = if outcome.created {
            format!(
                "Skill '{}' installed as a candidate revision; it is NOT active.",
                args.agent_id
            )
        } else {
            format!(
                "Skill written to disk as agent '{}' but a matching revision already existed — no new revision created",
                args.agent_id
            )
        };

        tracing::info!(
            target: "skill_install",
            agent_id = %args.agent_id,
            revision_id = %outcome.revision_id,
            created = outcome.created,
            trust_mode = %trust_mode,
            "Bootstrap complete (candidate, not promoted)"
        );

        // Causal event (RFC Part D): best-effort — the durable revision row
        // above is the source of truth, so a logging failure here must not
        // fail the install.
        if let Some(store) = gateway_store.as_ref() {
            let event = autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("skill-install-{}", uuid::Uuid::new_v4()),
                agent_id: manifest.agent.id.clone(),
                session_id: session_id.unwrap_or("").to_string(),
                turn_id: turn_id.map(|s| s.to_string()),
                event_seq: 0,
                timestamp: chrono::Utc::now().to_rfc3339(),
                category: "agent_install".to_string(),
                action: "skill_imported".to_string(),
                status: "SUCCESS".to_string(),
                enforced_rules: autonoetic_types::causal_chain::default_enforced_rules(),
                target: Some(args.agent_id.clone()),
                payload: Some(
                    serde_json::json!({
                        "url": args.url,
                        "sha256": fetched_sha256_hex,
                        "trust_mode": trust_mode,
                        "agent_id": args.agent_id,
                        "capabilities_source": capabilities_source,
                    })
                    .to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            };
            if let Err(e) = store.create_causal_event(&event) {
                tracing::warn!(
                    target: "skill_install",
                    agent_id = %args.agent_id,
                    error = %e,
                    "Failed to record skill_imported causal event"
                );
            }
        }

        Ok(serde_json::json!({
            "ok": true,
            "agent_id": args.agent_id,
            "trust_mode": trust_mode,
            "activated": false,
            "status": "candidate",
            "revision_id": outcome.revision_id,
            "message": message,
            "warnings": inference_warnings,
            "next": "Promote via agent_revision_promote — declared capabilities will face the standard gates (P-9.9 evidence for high-risk capabilities; P-2.25 operator approval of the capability delta for a new agent).",
        })
        .to_string())
    }
}

// ---------------------------------------------------------------------------
// skill_normalize — heuristically convert plain-markdown API docs to Autonoetic
// SKILL.md with YAML frontmatter (+ autonoetic.onboarding) for credential_setup(skill_url).
// ---------------------------------------------------------------------------

pub struct SkillNormalizeTool;

#[derive(Debug, Deserialize)]
struct SkillNormalizeArgs {
    /// Markdown body of the third-party skill spec (no Autonoetic frontmatter required).
    content: String,
    /// Logical service name (used in credential + default path).
    service: String,
    /// Where the markdown was loaded from, for base_url + allowed_hosts inference.
    #[serde(default)]
    source_url: Option<String>,
    /// Relative path under the agent workspace, e.g. `skills/moltbook/SKILL.md`.
    #[serde(default)]
    store_path: Option<String>,
    /// Required by JSON schema; must be non-empty after trim.
    #[serde(default)]
    intent: Option<String>,
}

#[derive(Serialize)]
struct SkillNormalizeFrontmatter {
    autonoetic: SkillNormalizeAutonoeticBody,
}

#[derive(Serialize)]
struct SkillNormalizeAutonoeticBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    credential: NormalizeCredentialOut,
    onboarding: NormalizeOnboardingOut,
}

#[derive(Serialize)]
struct NormalizeCredentialOut {
    service: String,
    inject_as: String,
    allowed_hosts: Vec<String>,
}

#[derive(Serialize)]
struct NormalizeOnboardingOut {
    steps: Vec<serde_yaml::Value>,
}

#[derive(Serialize)]
struct SkillNormalizeSessionContentOut {
    normalized_name: String,
    normalized_alias: String,
    normalized_ref: String,
}

fn service_slug(service: &str) -> String {
    service
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn inject_as_for_service(service: &str) -> String {
    autonoetic_types::runtime_lock::inject_as_for_service(service)
}

/// When `content` is a single non-empty line that is only an `http://` or `https://` URL, the
/// planner likely meant "fetch this" rather than "treat this string as markdown". We resolve
/// it here (subject to [`PolicyEngine::can_connect_net`]) so `skill_normalize` does not fail
/// with "No HTTP API endpoints could be extracted" on the URL text itself.
fn sole_http_url_content(content: &str) -> Option<String> {
    let t = content.trim();
    if t.is_empty() {
        return None;
    }
    let lines: Vec<&str> = t
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() != 1 {
        return None;
    }
    let line = lines[0];
    if !line.starts_with("http://") && !line.starts_with("https://") {
        return None;
    }
    if line.chars().any(char::is_whitespace) {
        return None;
    }
    let parsed = url::Url::parse(line).ok()?;
    if parsed.host_str()?.is_empty() {
        return None;
    }
    Some(line.to_string())
}

fn fetch_markdown_for_skill_normalize(
    policy: &PolicyEngine,
    url: &str,
) -> Result<String, String> {
    let host = extract_host(url).map_err(|e| e.to_string())?;
    if !policy.can_connect_net(&host).is_allowed() {
        return Err(format!(
            "skill_normalize received only a URL in `content`, but NetworkAccess does not allow host '{}' (rule P-1.5). \
Fetch the document with a network-capable step and pass the markdown body in `content`, or add this host to NetworkAccess.",
            host
        ));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err(format!(
            "HTTP {} while fetching skill markdown from {}",
            status, url
        ));
    }
    resp.text().map_err(|e| e.to_string())
}

fn extract_base_and_hosts(source_url: Option<&str>) -> (Option<String>, Vec<String>) {
    let Some(raw) = source_url else {
        return (None, Vec::new());
    };
    let Ok(parsed) = url::Url::parse(raw) else {
        return (None, Vec::new());
    };
    let host = parsed.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return (None, Vec::new());
    }
    let base = match parsed.port() {
        Some(p) => format!(
            "{}://{}:{}",
            parsed.scheme(),
            host,
            p
        ),
        None => format!("{}://{}", parsed.scheme(), host),
    };
    (Some(base), vec![host])
}

fn sniff_json_object_after(text: &str, search_from: usize) -> Option<serde_json::Value> {
    let slice = text.get(search_from..)?;
    let brace = slice.find('{')?;
    let rest = &slice[brace..];
    let mut depth = 0usize;
    let mut end_idx = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    end_idx = Some(i + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end_idx?;
    let snippet = rest.get(..end)?;
    serde_json::from_str(snippet).ok()
}

fn extract_api_steps_from_markdown(body: &str) -> (Vec<serde_yaml::Value>, Vec<String>) {
    let re = Regex::new(
        r#"(?i)\b(GET|POST|PUT|PATCH|DELETE)\s+(`([^`]+)`|(/[a-zA-Z0-9_./-]+))"#,
    )
    .expect("valid regex");
    let mut steps: Vec<serde_yaml::Value> = Vec::new();
    let mut fragments: Vec<String> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for cap in re.captures_iter(body) {
        let method = cap.get(1).map(|m| m.as_str().to_uppercase()).unwrap();
        let path = cap
            .get(3)
            .map(|m| m.as_str().to_string())
            .or_else(|| cap.get(4).map(|m| m.as_str().to_string()))
            .unwrap_or_default();
        let path = path.trim().to_string();
        if path.starts_with("http://") || path.starts_with("https://") {
            fragments.push(format!("skipped absolute url endpoint: {path}"));
            continue;
        }
        let path = if path.starts_with('/') {
            path
        } else {
            format!("/{}", path.trim_start_matches('/'))
        };
        let key = (method.clone(), path.clone());
        if !seen.insert(key) {
            continue;
        }
        let m_end = cap.get(0).map(|m| m.end()).unwrap_or(0);
        let json_body = sniff_json_object_after(body, m_end);

        let mut step_map: HashMap<String, serde_yaml::Value> = HashMap::new();
        step_map.insert(
            "type".to_string(),
            serde_yaml::Value::String("api_call".to_string()),
        );
        step_map.insert(
            "method".to_string(),
            serde_yaml::Value::String(method.clone()),
        );
        step_map.insert("url".to_string(), serde_yaml::Value::String(path.clone()));
        if let Some(j) = json_body {
            if let Ok(v) = serde_yaml::to_value(&j) {
                step_map.insert("body".to_string(), v);
            }
        }
        // Heuristic: registration-style responses often expose `secret`.
        if method == "POST" && path.contains("register") {
            let mut es = serde_yaml::Mapping::new();
            es.insert(
                serde_yaml::Value::String("api_secret".to_string()),
                serde_yaml::Value::String("$.secret".to_string()),
            );
            step_map.insert(
                "extract_secrets".to_string(),
                serde_yaml::Value::Mapping(es),
            );
        }
        if let Ok(v) = serde_yaml::to_value(&step_map) {
            steps.push(v);
        }
    }
    (steps, fragments)
}

/// Split an absolute http(s) URL into `(origin, host, path)` where `origin` is
/// `scheme://host[:port]` and `path` defaults to `/`. Used by the single-endpoint
/// fallback (#856) to derive a `base_url` + operation path from a bare URL.
fn split_endpoint_url(raw: &str) -> Option<(String, String, String)> {
    let parsed = url::Url::parse(raw.trim()).ok()?;
    // Only http(s): a synthesized skill must not emit a base_url with an
    // unsupported scheme (ftp:, file:, …).
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?.to_string();
    if host.is_empty() {
        return None;
    }
    let origin = match parsed.port() {
        Some(p) => format!("{}://{}:{}", parsed.scheme(), host, p),
        None => format!("{}://{}", parsed.scheme(), host),
    };
    // Preserve the query string so required parameters survive in the
    // synthesized GET step's path (a base URL may carry defaults like `?units=`).
    let mut path = if parsed.path().is_empty() {
        "/".to_string()
    } else {
        parsed.path().to_string()
    };
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    Some((origin, host, path))
}

/// Trim trailing markdown/prose punctuation a URL regex may over-capture.
fn trim_url_tail(u: &str) -> &str {
    u.trim_end_matches(|c| {
        matches!(c, '.' | ',' | ';' | ':' | ')' | ']' | '>' | '"' | '\'' | '`')
    })
}

/// When [`extract_api_steps_from_markdown`] finds no `METHOD path` operations, a
/// document may still declare a single API by its base/endpoint URL. Find that
/// URL so the caller can synthesize a one-operation (GET) skill instead of
/// rejecting the whole document (#856). Priority:
///   1. an http(s) URL under / next to a `Base URL` or `## Endpoint(s)` marker,
///   2. else the first standalone http(s) URL in the body,
///   3. else the caller-supplied `source_url`.
fn fallback_single_endpoint(body: &str, source_url: Option<&str>) -> Option<String> {
    let url_re = Regex::new(r#"https?://[^\s`<>()\[\]"']+"#).ok()?;
    let lines: Vec<&str> = body.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_ascii_lowercase();
        let is_marker = lower.contains("base url")
            || lower.contains("base-url")
            || (lower.trim_start().starts_with('#') && lower.contains("endpoint"));
        if !is_marker {
            continue;
        }
        // URL on the marker line itself, else on the next non-empty line.
        if let Some(m) = url_re.find(line) {
            return Some(trim_url_tail(m.as_str()).to_string());
        }
        if let Some(next) = lines.iter().skip(i + 1).find(|l| !l.trim().is_empty()) {
            if let Some(m) = url_re.find(next) {
                return Some(trim_url_tail(m.as_str()).to_string());
            }
        }
    }
    if let Some(m) = url_re.find(body) {
        return Some(trim_url_tail(m.as_str()).to_string());
    }
    source_url.map(|s| s.trim().to_string())
}

/// Build a single `GET` `api_call` step for a synthesized one-operation skill (#856).
fn synthesize_get_step(path: &str) -> serde_yaml::Value {
    let mut step: HashMap<String, serde_yaml::Value> = HashMap::new();
    step.insert(
        "type".to_string(),
        serde_yaml::Value::String("api_call".to_string()),
    );
    step.insert(
        "method".to_string(),
        serde_yaml::Value::String("GET".to_string()),
    );
    step.insert(
        "url".to_string(),
        serde_yaml::Value::String(path.to_string()),
    );
    serde_yaml::to_value(&step).unwrap_or(serde_yaml::Value::Null)
}

fn autonoetic_onboarding_present(content: &str) -> bool {
    if !content.trim_start().starts_with("---") {
        return false;
    }
    let matter = Matter::<gray_matter::engine::YAML>::new();
    let Ok(parsed) = matter.parse::<serde_yaml::Value>(content) else {
        return false;
    };
    let Some(data) = parsed.data else {
        return false;
    };
    data.get("autonoetic")
        .and_then(|a| a.get("onboarding"))
        .and_then(|o| o.get("steps"))
        .and_then(|s| s.as_sequence())
        .map(|seq| !seq.is_empty())
        .unwrap_or(false)
}

fn validate_rel_store_path(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!path.is_empty(), "store_path must not be empty");
    anyhow::ensure!(
        !path.contains(".."),
        "store_path must not contain '..'"
    );
    let p = Path::new(path);
    anyhow::ensure!(
        !p.is_absolute(),
        "store_path must be relative to the agent workspace"
    );
    anyhow::ensure!(
        p.components()
            .all(|c| matches!(c, Component::Normal(_))),
        "store_path must be a simple relative path"
    );
    anyhow::ensure!(
        path.starts_with("skills/"),
        "store_path must be under skills/ (WriteAccess scope)"
    );
    Ok(())
}

fn write_skill_to_gateway_dir(gateway_dir: Option<&Path>, rel: &str, content: &str) {
    if let Some(gw) = gateway_dir {
        let gw_path = gw.join(rel);
        if let Some(parent) = gw_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&gw_path, content);
    }
}

fn register_skill_session_content(
    gateway_dir: Option<&Path>,
    session_id: Option<&str>,
    manifest: &AgentManifest,
    service_slug: &str,
    content: &str,
) -> anyhow::Result<Option<SkillNormalizeSessionContentOut>> {
    let Some(gw_dir) = gateway_dir else {
        return Ok(None);
    };

    let sid = session_id.unwrap_or(&manifest.agent.id);
    let store = crate::runtime::content_store::ContentStore::new(gw_dir)?;
    let handle = store.write(content.as_bytes())?;
    let normalized_name = format!("skill.{}.md", service_slug);
    store.register_name_with_visibility(
        sid,
        &normalized_name,
        &handle,
        crate::runtime::content_store::ContentVisibility::Session,
    )?;
    let normalized_alias =
        crate::runtime::content_store::ContentStore::get_short_alias(&handle);

    Ok(Some(SkillNormalizeSessionContentOut {
        normalized_name,
        normalized_alias: normalized_alias.clone(),
        normalized_ref: format!("cnt_{}", normalized_alias),
    }))
}

fn write_skill_discovery_record(
    gateway_dir: Option<&Path>,
    gateway_store: Option<&std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    manifest: &AgentManifest,
    session_id: Option<&str>,
    turn_id: Option<&str>,
    service: &str,
    skill_path: &str,
    steps_count: usize,
    base_url: Option<&str>,
    source_url: Option<&str>,
) -> anyhow::Result<bool> {
    let Some(gw_dir) = gateway_dir else {
        return Ok(false);
    };

    let sid = session_id.unwrap_or(&manifest.agent.id);
    let source_ref = match turn_id {
        Some(tid) => format!("session:{}:turn:{}", sid, tid),
        None => format!("session:{}", sid),
    };

    let mem = tier2_memory_for_native_tool(gw_dir, gateway_store, &manifest.agent.id, session_id)?;

    let id = format!("registration:{}", service_slug(service));
    let content = serde_json::json!({
        "service": service,
        "skill_path": skill_path,
        "base_url": base_url,
        "steps_count": steps_count,
        "source_url": source_url,
    })
    .to_string();

    let mut memory = MemoryObject::new(
        id,
        "skills".to_string(),
        manifest.agent.id.clone(),
        manifest.agent.id.clone(),
        source_ref,
        content,
    );
    memory.visibility = MemoryVisibility::Global;
    memory.confidence = Some(1.0);
    memory.tags = vec![
        "source:skill_normalize".to_string(),
        format!("service:{}", service_slug(service)),
        "type:normalized_skill".to_string(),
    ];

    if let Some(sid) = session_id {
        if let Some(store) = gateway_store {
            if let Ok(Some(binding)) = store.get_session_agent_binding(sid) {
                memory.revision_id = Some(binding.revision_id.clone());
                memory.binding_session_id = Some(binding.session_id.clone());
                memory.alias_ref = binding.alias_id.clone();
            }
        }
    }

    block_on_memory(mem.save_memory(&memory))?;
    Ok(true)
}

impl NativeTool for SkillNormalizeTool {
    fn name(&self) -> &'static str {
        "skill_normalize"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Convert a plain-markdown third-party skill/API document into an Autonoetic \
                SKILL.md with YAML frontmatter (`autonoetic.onboarding` steps) so `credential_setup` \
                can load it via `skill_url`. Uses heuristics (HTTP method + path + optional JSON body); \
                ambiguous docs return `ok:false` with `partial` and `fragments` for manual completion. \
                Requires WriteAccess under `skills/`."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "intent": {
                        "type": "string",
                        "description": "Why you are normalizing this skill (1-2 sentences)."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full markdown text of the external skill/API spec. If this value is a single line containing only an http(s) URL, the gateway fetches it (requires NetworkAccess for that host, rule P-1.5) and normalizes the response body."
                    },
                    "service": {
                        "type": "string",
                        "description": "Short service identifier (e.g. moltbook)."
                    },
                    "source_url": {
                        "type": "string",
                        "description": "Optional original URL; used to set base_url and allowed_hosts."
                    },
                    "store_path": {
                        "type": "string",
                        "description": "Relative path to write, default skills/<service>/SKILL.md"
                    }
                },
                "required": ["intent", "content", "service"],
                "additionalProperties": false
            }),
        }
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest.capabilities.iter().any(|c| {
            matches!(
                c,
                Capability::WriteAccess { scopes }
                    if scopes.iter().any(|s| s == "skills/*" || s == "skills/" || s.starts_with("skills"))
            )
        })
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        agent_dir: &Path,
        gateway_dir: Option<&Path>,
        arguments_json: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: SkillNormalizeArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid arguments: {}", e))?;

        let intent = args.intent.as_deref().unwrap_or("").trim();
        anyhow::ensure!(!intent.is_empty(), "`intent` is required for skill_normalize");

        let slug = service_slug(&args.service);
        anyhow::ensure!(!slug.is_empty(), "service must yield a non-empty slug");

        let mut markdown = args.content.clone();
        let mut source_url = args.source_url.clone();
        if let Some(url) = sole_http_url_content(&markdown) {
            match fetch_markdown_for_skill_normalize(policy, &url) {
                Ok(body) => {
                    markdown = body;
                    if source_url.is_none() {
                        source_url = Some(url);
                    }
                }
                Err(msg) => {
                    return Ok(ToolError::execution(msg, None::<String>).to_error_response());
                }
            }
        }

        let rel = args
            .store_path
            .clone()
            .unwrap_or_else(|| format!("skills/{slug}/SKILL.md"));
        validate_rel_store_path(&rel)?;
        if !policy.can_write_path(&rel).is_allowed() {
            return Ok(ToolError::permission(format!("WriteAccess denied for normalized skill path '{}' (policy P-1.4)", rel)).with_code("skill_write_access_denied").to_error_response());
        }

        if autonoetic_onboarding_present(&markdown) {
            let out_path = agent_dir.join(&rel);
            std::fs::create_dir_all(out_path.parent().unwrap())?;
            std::fs::write(&out_path, &markdown)?;
            write_skill_to_gateway_dir(gateway_dir, &rel, &markdown);
            let session_content = register_skill_session_content(
                gateway_dir,
                session_id,
                manifest,
                &slug,
                &markdown,
            )?;
            let discovery_record_registered =
                match write_skill_discovery_record(
                    gateway_dir,
                    gateway_store.as_ref(),
                    manifest,
                    session_id,
                    turn_id,
                    &args.service,
                    &rel,
                    0,
                    None,
                    source_url.as_deref(),
                ) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!(
                            target: "skill_normalize",
                            service = %args.service,
                            skill_path = %rel,
                            error = %err,
                            "Failed to write normalized-skill discovery record"
                        );
                        false
                    }
                };
            return Ok(serde_json::json!({
                "ok": true,
                "skill_path": rel,
                "already_normalized": true,
                "session_content": session_content,
                "discovery_record_registered": discovery_record_registered,
                "message": "Content already contains autonoetic.onboarding steps; file written as-is. Use resolve with session_content.normalized_name, or credential_setup with skill_url=skill_path.",
            })
            .to_string());
        }

        let (mut base_url, mut allowed_hosts) = extract_base_and_hosts(source_url.as_deref());

        let (mut step_values, fragments) = extract_api_steps_from_markdown(&markdown);

        // Tolerance (#856): a doc that only declares a base/endpoint URL (no
        // `METHOD path` lines) is still a valid one-operation GET API. Synthesize
        // a single step from that URL rather than rejecting the whole document —
        // the rejection here sent the planner into an 11-minute divergent
        // re-spawn in session-0718349d. The synthesized op's host is
        // authoritative, so it overrides base_url/allowed_hosts (the API host,
        // e.g. api.open-meteo.com, is usually different from the docs host in
        // source_url).
        let mut synthesized_single_endpoint = false;
        if step_values.is_empty() {
            if let Some((origin, host, path)) =
                fallback_single_endpoint(&markdown, source_url.as_deref())
                    .as_deref()
                    .and_then(split_endpoint_url)
            {
                base_url = Some(origin);
                allowed_hosts = vec![host];
                step_values.push(synthesize_get_step(&path));
                synthesized_single_endpoint = true;
            }
        }

        if allowed_hosts.is_empty() {
            allowed_hosts.push("localhost".to_string());
        }

        if step_values.is_empty() {
            let found = if fragments.is_empty() {
                "no HTTP method + path lines and no base/endpoint URL".to_string()
            } else {
                fragments.join("; ")
            };
            return Ok(ToolError::validation(
                format!(
                    "No HTTP API endpoints could be extracted from the markdown. Expected a \
                     `## Endpoints` section with one URL template per operation (e.g. \
                     `GET https://host/path{{?param1,param2}}`) or `METHOD /path` lines, or at \
                     least a `## Base URL` line naming the API's base URL. Found: {found}."
                ),
                Some(
                    "Recovery: re-request the research with an explicit `## Endpoints` section \
                     (one `GET https://host/path` per operation), or patch the markdown to add a \
                     `## Base URL` line naming the API host, then retry skill_normalize.",
                ),
            )
            .with_code("no_api_endpoints_found")
            .to_error_response());
        }

        let steps_count = step_values.len();
        let inject_as = inject_as_for_service(&args.service);
        let fm = SkillNormalizeFrontmatter {
            autonoetic: SkillNormalizeAutonoeticBody {
                base_url: base_url.clone(),
                credential: NormalizeCredentialOut {
                    service: args.service.clone(),
                    inject_as,
                    allowed_hosts,
                },
                onboarding: NormalizeOnboardingOut {
                    steps: step_values,
                },
            },
        };
        let yaml = serde_yaml::to_string(&fm)
            .map_err(|e| anyhow::anyhow!("failed to serialize skill frontmatter: {}", e))?;
        let document = format!("---\n{yaml}---\n\n{}", markdown);

        let out_path = agent_dir.join(&rel);
        std::fs::create_dir_all(
            out_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("invalid store_path"))?,
        )?;
        std::fs::write(&out_path, &document)?;
        write_skill_to_gateway_dir(gateway_dir, &rel, &document);
        let session_content = register_skill_session_content(
            gateway_dir,
            session_id,
            manifest,
            &slug,
            &document,
        )?;

        let discovery_record_registered = match write_skill_discovery_record(
            gateway_dir,
            gateway_store.as_ref(),
            manifest,
            session_id,
            turn_id,
            &args.service,
            &rel,
            steps_count,
            base_url.as_deref(),
            source_url.as_deref(),
        ) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    target: "skill_normalize",
                    service = %args.service,
                    skill_path = %rel,
                    error = %err,
                    "Failed to write normalized-skill discovery record"
                );
                false
            }
        };

        let agent_candidate = steps_count >= 2;

        Ok(serde_json::json!({
            "ok": true,
            "skill_path": rel,
            "service": args.service,
            "steps_count": steps_count,
            "base_url": base_url,
            "fragments": fragments,
            "synthesized_single_endpoint": synthesized_single_endpoint,
            "session_content": session_content,
            "discovery_record_registered": discovery_record_registered,
            "agent_creation_candidate": agent_candidate,
            "message": if synthesized_single_endpoint {
                "Wrote Autonoetic SKILL.md with a single GET operation synthesized from the document's base/endpoint URL (no explicit METHOD path operations were declared). Use resolve with session_content.normalized_name, or credential_setup with skill_url for onboarding. If the API has more operations or query parameters, re-run skill_normalize on markdown with an explicit `## Endpoints` section."
            } else if agent_candidate {
                "Wrote Autonoetic SKILL.md with multiple API operations. Use resolve with session_content.normalized_name, or credential_setup with skill_url for onboarding. Consider spawning coder.default to build a reusable script agent for this service."
            } else {
                "Wrote Autonoetic SKILL.md; use resolve with session_content.normalized_name, or credential_setup with skill_url pointing at this path or a file:// URL as supported by your deployment."
            },
        })
        .to_string())
    }
}

/// `true` when a SKILL.md URL may be fetched: always `https://`, or
/// `http://` only for loopback hosts (local dev / tests). Any other scheme
/// (`file:`, `ftp:`, …) is rejected as well.
fn url_scheme_is_fetch_safe(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => true,
        "http" => matches!(
            parsed.host_str(),
            Some("127.0.0.1") | Some("::1") | Some("localhost")
        ),
        _ => false,
    }
}

/// Structured install-time warnings for the RFC Part C inference clamp: when
/// `allowed-tools` asked for shell or network tools but the capability set
/// was *inferred* (not declared), the operator gets told exactly what was
/// clamped rather than silently granted a narrower power than the tool name
/// suggests. `trust_mode` is threaded in because the network wording differs:
/// `strict` drops the inferred (high-risk) `NetworkAccess` entirely, whereas
/// `generous` keeps it with an empty hosts list — the warning must not imply
/// the Candidate carries network access it doesn't. (`SandboxFunctions` is not
/// high-risk, so the shell wording holds for both modes.)
fn capability_inference_warnings(allowed_tools: &[String], trust_mode: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let wants_bash = allowed_tools
        .iter()
        .any(|t| t.trim() == "Bash" || t.trim().starts_with("Bash("));
    let wants_network = allowed_tools
        .iter()
        .any(|t| matches!(t.trim(), "WebSearch" | "WebFetch" | "Fetch"));
    if wants_bash {
        warnings.push(
            "allowed-tools requested Bash: shell execution requires an explicit CodeExecution \
             declaration; granted SandboxFunctions prefixes only."
                .to_string(),
        );
    }
    if wants_network {
        warnings.push(if trust_mode == "strict" {
            "allowed-tools requested network tools, but strict trust_mode dropped the inferred \
             NetworkAccess entirely; declare NetworkAccess with concrete hosts in \
             metadata.autonoetic.capabilities to grant it."
                .to_string()
        } else {
            "allowed-tools requested network tools: NetworkAccess inferred with an empty hosts \
             list — declare concrete hosts in metadata.autonoetic.capabilities to enable network \
             access."
                .to_string()
        });
    }
    warnings
}

/// Map a trust_mode string to the capability set the Candidate carries into
/// the promotion gate, plus which source produced it (`"declared"` — explicit
/// `metadata.autonoetic.capabilities`; `"inferred"` — derived from
/// `allowed-tools`; `"defaults"` — neither present) for the causal-event
/// payload (RFC Part D) and the `strict` inference clamp (RFC Part B): with
/// one door in place (promotion gate = the real protection), `strict` no
/// longer claims to gate every action — it drops any *inferred* high-risk
/// capability (untrusted text should not be able to mint a review-worthy
/// capability nobody explicitly declared) and keeps `ApprovalQueue` for what
/// it actually does (admin-proposal filing + the Workflow tool tier).
fn apply_trust_mode(
    trust_mode: &str,
    parsed: &AgentManifest,
) -> anyhow::Result<(Vec<Capability>, &'static str)> {
    // The standard-frontmatter parser pre-infers capabilities from
    // `allowed-tools` INTO `parsed.capabilities` and records that fact on the
    // import metadata — so a non-empty capability set is NOT proof of an
    // explicit declaration. Trust the recorded bit first; the emptiness
    // branches below only cover manifests the parser did not pre-infer for.
    let parser_inferred = parsed
        .agentskills_import
        .as_ref()
        .is_some_and(|m| m.capabilities_inferred);
    if parser_inferred && matches!(trust_mode, "generous" | "strict") {
        let mut caps = parsed.capabilities.clone();
        if trust_mode == "strict" {
            caps.retain(|c| !crate::runtime::install_contract::is_high_risk_capability(c));
            caps.push(Capability::ApprovalQueue {
                patterns: vec!["*".to_string()],
            });
        }
        return Ok((caps, "inferred"));
    }
    match trust_mode {
        "generous" => {
            // Use capabilities declared in the remote SKILL.md as-is.
            // Fall back to minimal defaults if none declared.
            if parsed.capabilities.is_empty() {
                let allowed_tools: Vec<String> = parsed
                    .agentskills_import
                    .as_ref()
                    .map(|m| m.allowed_tools.clone())
                    .unwrap_or_default();
                if allowed_tools.is_empty() {
                    Ok((
                        vec![
                            Capability::ReadAccess {
                                scopes: vec!["self.*".to_string()],
                            },
                            Capability::WriteAccess {
                                scopes: vec!["self.*".to_string()],
                            },
                        ],
                        "defaults",
                    ))
                } else {
                    Ok((
                        crate::runtime::parser::infer_capabilities(&allowed_tools),
                        "inferred",
                    ))
                }
            } else {
                Ok((parsed.capabilities.clone(), "declared"))
            }
        }
        "strict" => {
            // Preserve capabilities, but drop any high-risk capability that
            // was inferred rather than explicitly declared, then add the
            // approval-queue gate.
            let (mut caps, source) = if parsed.capabilities.is_empty() {
                let allowed_tools: Vec<String> = parsed
                    .agentskills_import
                    .as_ref()
                    .map(|m| m.allowed_tools.clone())
                    .unwrap_or_default();
                if allowed_tools.is_empty() {
                    (
                        vec![Capability::ReadAccess {
                            scopes: vec!["self.*".to_string()],
                        }],
                        "defaults",
                    )
                } else {
                    (
                        crate::runtime::parser::infer_capabilities(&allowed_tools),
                        "inferred",
                    )
                }
            } else {
                (parsed.capabilities.clone(), "declared")
            };
            if source == "inferred" {
                caps.retain(|c| !crate::runtime::install_contract::is_high_risk_capability(c));
            }
            caps.push(Capability::ApprovalQueue {
                patterns: vec!["*".to_string()],
            });
            Ok((caps, source))
        }
        "audit" => {
            // Read-only + approval gate — ignores declared capabilities.
            Ok((
                vec![
                    Capability::ReadAccess {
                        scopes: vec!["self.*".to_string()],
                    },
                    Capability::ApprovalQueue {
                        patterns: vec!["*".to_string()],
                    },
                ],
                "defaults",
            ))
        }
        other => {
            return Err(autonoetic_types::tool_error::tagged::Tagged::validation(anyhow::anyhow!(
                "Unknown trust_mode '{}'; valid values: generous, strict, audit",
                other
            )).into());
        }
    }
}

#[cfg(test)]
mod skill_normalize_extractor_tests {
    use super::*;

    #[test]
    fn extractor_still_parses_method_path_endpoints() {
        let md = "## Endpoints\n\nGET `/v1/forecast`\nPOST `/v1/register`\n";
        let (steps, _frags) = extract_api_steps_from_markdown(md);
        assert_eq!(steps.len(), 2, "explicit METHOD path endpoints still extract");
    }

    #[test]
    fn extractor_empty_for_prose_only_base_url() {
        // The session-0718349d shape: a base URL in prose, no `METHOD path` lines.
        let md = "## Base URL\nhttps://api.open-meteo.com/v1/forecast\n\nDescribes latitude, longitude params.";
        let (steps, _frags) = extract_api_steps_from_markdown(md);
        assert!(steps.is_empty(), "prose base URL yields no method+path steps");
    }

    #[test]
    fn fallback_finds_url_under_base_url_heading() {
        let md = "# Open-Meteo\n\n## Base URL\nhttps://api.open-meteo.com/v1/forecast\n\n## Parameters\nlatitude, longitude";
        assert_eq!(
            fallback_single_endpoint(md, Some("https://open-meteo.com/en/docs")).as_deref(),
            Some("https://api.open-meteo.com/v1/forecast")
        );
    }

    #[test]
    fn fallback_finds_url_on_inline_base_url_line() {
        let md = "Base URL: https://api.example.com/v2\nsome prose";
        assert_eq!(
            fallback_single_endpoint(md, None).as_deref(),
            Some("https://api.example.com/v2")
        );
    }

    #[test]
    fn fallback_prefers_body_url_over_source_url() {
        // The API host from the body must win over the docs host in source_url.
        let md = "## Base URL\nhttps://api.open-meteo.com/v1/forecast";
        let got = fallback_single_endpoint(md, Some("https://open-meteo.com/en/docs")).unwrap();
        assert!(got.contains("api.open-meteo.com"), "got: {got}");
    }

    #[test]
    fn fallback_uses_source_url_when_body_has_no_url() {
        let md = "This API returns weather data. No URLs here.";
        assert_eq!(
            fallback_single_endpoint(md, Some("https://api.example.com/v1")).as_deref(),
            Some("https://api.example.com/v1")
        );
    }

    #[test]
    fn fallback_none_when_no_url_anywhere() {
        assert!(fallback_single_endpoint("just prose, no links", None).is_none());
    }

    #[test]
    fn fallback_trims_trailing_punctuation() {
        // The regex includes a trailing '.', which trim_url_tail must strip.
        let md = "Base URL: https://api.example.com/v1.";
        assert_eq!(
            fallback_single_endpoint(md, None).as_deref(),
            Some("https://api.example.com/v1")
        );
    }

    #[test]
    fn split_endpoint_url_parts() {
        let (origin, host, path) =
            split_endpoint_url("https://api.open-meteo.com/v1/forecast").unwrap();
        assert_eq!(origin, "https://api.open-meteo.com");
        assert_eq!(host, "api.open-meteo.com");
        assert_eq!(path, "/v1/forecast");
    }

    #[test]
    fn split_endpoint_url_origin_only_path_is_root() {
        let (origin, host, path) = split_endpoint_url("https://api.example.com").unwrap();
        assert_eq!(origin, "https://api.example.com");
        assert_eq!(host, "api.example.com");
        assert_eq!(path, "/");
    }

    #[test]
    fn split_endpoint_url_with_port() {
        let (origin, _host, path) = split_endpoint_url("http://localhost:8080/api").unwrap();
        assert_eq!(origin, "http://localhost:8080");
        assert_eq!(path, "/api");
    }

    #[test]
    fn split_endpoint_url_rejects_non_http_schemes() {
        assert!(split_endpoint_url("ftp://files.example.com/x").is_none());
        assert!(split_endpoint_url("file:///etc/passwd").is_none());
        assert!(split_endpoint_url("mailto:a@b.com").is_none());
    }

    #[test]
    fn split_endpoint_url_preserves_query_string() {
        let (origin, _host, path) =
            split_endpoint_url("https://api.example.com/v1/forecast?units=metric&hourly=temp")
                .unwrap();
        assert_eq!(origin, "https://api.example.com");
        assert_eq!(path, "/v1/forecast?units=metric&hourly=temp");
    }

    #[test]
    fn synthesize_get_step_shape() {
        let step = synthesize_get_step("/v1/forecast");
        assert_eq!(step.get("type").and_then(|v| v.as_str()), Some("api_call"));
        assert_eq!(step.get("method").and_then(|v| v.as_str()), Some("GET"));
        assert_eq!(step.get("url").and_then(|v| v.as_str()), Some("/v1/forecast"));
    }

    /// End-to-end of the fix logic on the exact failing shape: prose base URL →
    /// no extracted steps → fallback finds the API URL → split → single GET op.
    #[test]
    fn openmeteo_shape_synthesizes_single_get_operation() {
        let md = "# Open-Meteo Weather Forecast API\n\n## Base URL\nhttps://api.open-meteo.com/v1/forecast\n\n## Parameters\n- latitude (required)\n- longitude (required)\n";
        let (steps, _frags) = extract_api_steps_from_markdown(md);
        assert!(steps.is_empty());
        let (origin, host, path) =
            fallback_single_endpoint(md, Some("https://open-meteo.com/en/docs"))
                .as_deref()
                .and_then(split_endpoint_url)
                .expect("fallback should find the API base URL");
        assert_eq!(origin, "https://api.open-meteo.com");
        assert_eq!(host, "api.open-meteo.com");
        assert_eq!(path, "/v1/forecast");
        let step = synthesize_get_step(&path);
        assert_eq!(step.get("method").and_then(|v| v.as_str()), Some("GET"));
        assert_eq!(step.get("url").and_then(|v| v.as_str()), Some("/v1/forecast"));
    }
}
