//! Post-session digest: LLM narrative + Tier-2 memory extraction after eligible sessions.

use crate::llm::{CompletionRequest, LlmDriver, Message};
use crate::runtime::content_store::{ContentStore, ContentVisibility};
use crate::runtime::live_digest::base_session_id;
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::LlmConfig;
use autonoetic_types::config::{GatewayConfig, LlmPreset};
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};
use serde::Deserialize;
use sha2::{Digest as Sha2Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

/// Registered name in the session content manifest for the LLM narrative.
pub const POST_SESSION_NARRATIVE_CONTENT_NAME: &str = "post_session_narrative.md";
/// Writer agent id for provenance on extracted memories.
pub const DIGEST_AGENT_ID: &str = "autonoetic.digest";

#[derive(Debug, Deserialize)]
struct DigestMemoryItem {
    #[serde(rename = "type")]
    mem_type: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DigestLlmOutput {
    narrative: String,
    #[serde(default)]
    memories: Vec<DigestMemoryItem>,
}

pub fn load_digest_skill_body(agents_dir: &Path) -> anyhow::Result<String> {
    let path = agents_dir.join("digest").join("SKILL.md");
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        anyhow::anyhow!(
            "post-session digest requires {}: {}",
            path.display(),
            e
        )
    })?;
    Ok(strip_markdown_frontmatter(&raw))
}

fn strip_markdown_frontmatter(raw: &str) -> String {
    let t = raw.trim_start();
    if !t.starts_with("---") {
        return raw.trim().to_string();
    }
    let mut lines = t.lines();
    let _ = lines.next();
    let mut out = Vec::new();
    let mut past = false;
    for line in lines {
        if !past {
            if line == "---" {
                past = true;
            }
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

fn resolve_digest_llm_config(config: &GatewayConfig) -> anyhow::Result<LlmConfig> {
    let d = &config.digest_agent;
    if let Some(preset_name) = d.llm_preset.as_ref() {
        let preset = config.llm_presets.get(preset_name).ok_or_else(|| {
            anyhow::anyhow!(
                "digest_agent.llm_preset '{}' not found in llm_presets",
                preset_name
            )
        })?;
        return Ok(llm_preset_to_config(preset));
    }
    let provider = d.provider.as_ref().ok_or_else(|| {
        anyhow::anyhow!("digest_agent requires llm_preset or both provider and model")
    })?;
    let model = d
        .model
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("digest_agent requires model when provider is set"))?;
    Ok(LlmConfig {
        provider: provider.clone(),
        model: model.clone(),
        temperature: 0.0,
        fallback_provider: None,
        fallback_model: None,
        chat_only: true,
        context_window_tokens: None,
    })
}

fn llm_preset_to_config(p: &LlmPreset) -> LlmConfig {
    LlmConfig {
        provider: p.provider.clone(),
        model: p.model.clone(),
        temperature: p.temperature.unwrap_or(0.0) as f64,
        fallback_provider: p.fallback_provider.clone(),
        fallback_model: p.fallback_model.clone(),
        chat_only: p.chat_only.unwrap_or(false),
        context_window_tokens: p.context_window_tokens,
    }
}

fn extract_json_object_slice(text: &str) -> anyhow::Result<&str> {
    let t = text.trim();
    let scan = if let Some(pos) = t.find("```") {
        let after = t[pos + 3..].trim_start();
        let after = if after.starts_with("json") {
            after[4..].trim_start()
        } else {
            after
        };
        if let Some(end_fence) = after.find("```") {
            &after[..end_fence]
        } else {
            after
        }
    } else {
        t
    };
    let start = scan
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("digest LLM output has no JSON object"))?;
    let end = scan
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("digest LLM output has no closing brace"))?;
    Ok(scan[start..=end].trim())
}

fn digest_memory_id(base: &str, session_key: &str, idx: usize, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(base.as_bytes());
    h.update(session_key.as_bytes());
    h.update(&(idx as u64).to_le_bytes());
    h.update(content.as_bytes());
    let hex = hex::encode(h.finalize());
    format!("dig-{}-{}", &hex[..24], idx)
}

fn sanitize_scope_segment(s: &str) -> String {
    let lower = s.to_lowercase();
    lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn apply_digest_output(
    gateway_dir: &Path,
    store: &GatewayStore,
    session_id: &str,
    source_agent_id: &str,
    output: &DigestLlmOutput,
) -> anyhow::Result<()> {
    let base = base_session_id(session_id);
    let cs = ContentStore::new(gateway_dir)?;
    let handle = cs.write(output.narrative.as_bytes())?;
    cs.register_name_with_visibility(
        base,
        POST_SESSION_NARRATIVE_CONTENT_NAME,
        &handle,
        ContentVisibility::Session,
    )?;

    for (idx, m) in output.memories.iter().enumerate() {
        let seg = sanitize_scope_segment(&m.mem_type);
        anyhow::ensure!(
            !seg.is_empty(),
            "digest memory item {} has empty or invalid type",
            idx
        );
        anyhow::ensure!(
            !m.content.trim().is_empty(),
            "digest memory item {} has empty content",
            idx
        );
        let scope = format!("digest.{seg}");
        let mut tags = m.tags.clone();
        tags.push("source:post_session_digest".to_string());
        tags.push(format!("session:{base}"));

        let memory_id = digest_memory_id(base, session_id, idx, &m.content);
        let mut obj = MemoryObject::new(
            memory_id,
            scope,
            source_agent_id.to_string(),
            DIGEST_AGENT_ID.to_string(),
            format!("session:{session_id}:post_digest"),
            m.content.clone(),
        );
        obj.source_type = MemorySourceType::SessionDigest;
        obj.tags = tags;
        obj.confidence = m.confidence;
        obj.visibility = MemoryVisibility::Global;
        store.memory_upsert(&obj)?;
    }
    Ok(())
}

async fn run_post_session_digest_inner(
    gateway_dir: &Path,
    store: &Arc<GatewayStore>,
    session_id: &str,
    source_agent_id: &str,
    digest_llm: &LlmConfig,
    driver: &dyn LlmDriver,
) -> anyhow::Result<()> {
    let base = base_session_id(session_id);
    let digest_path = gateway_dir
        .join("sessions")
        .join(base)
        .join("digest.md");
    let live_digest = match std::fs::read_to_string(&digest_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "post_session_digest",
                path = %digest_path.display(),
                error = %e,
                "live digest file missing; continuing with empty digest"
            );
            String::new()
        }
    };

    let traces = store.search_execution_traces(
        None,
        None,
        None,
        None,
        None,
        Some(session_id),
        64,
    )?;
    let mut trace_lines = Vec::new();
    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    for t in traces {
        let ok = t.success == 1;
        if ok {
            success_count += 1;
        } else {
            failure_count += 1;
        }
        let summary = if ok {
            t.result
                .as_deref()
                .or(t.stdout.as_deref())
                .unwrap_or("(success)")
        } else {
            t.error_summary
                .as_deref()
                .or(t.stderr.as_deref())
                .unwrap_or("(failure)")
        };
        let status = if ok { "ok" } else { "error" };
        let kind = if ok {
            "success"
        } else {
            t.error_type.as_deref().unwrap_or("?")
        };
        trace_lines.push(format!(
            "- {} | {} | {} | {} | {}",
            t.timestamp, t.tool_name, status, kind, summary
        ));
    }
    let trace_block = if trace_lines.is_empty() {
        "(no execution_traces for this session branch)".to_string()
    } else {
        format!(
            "counts: success={}, failure={}\n{}",
            success_count,
            failure_count,
            trace_lines.join("\n")
        )
    };

    let agents_dir = gateway_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("gateway_dir has no parent (expected agents/.gateway)"))?;
    let system = load_digest_skill_body(agents_dir)?;
    let user = format!(
        "Session id (full): {session_id}\nSource agent: {source_agent_id}\n\n## Live digest (markdown)\n\n{live_digest}\n\n## Execution traces (successes + failures summary)\n\n{trace_block}\n\nRespond with ONLY a single JSON object as specified in your instructions."
    );

    let mut req = CompletionRequest::simple(
        digest_llm.model.clone(),
        vec![Message::system(system), Message::user(user)],
    );
    if digest_llm.temperature > 0.0 {
        req.temperature = Some(digest_llm.temperature as f32);
    }

    let resp = driver.complete(&req).await?;
    if !resp.tool_calls.is_empty() {
        anyhow::bail!("digest LLM must return JSON text only, not tool calls");
    }
    let json_slice = extract_json_object_slice(&resp.text)?;
    let output: DigestLlmOutput = serde_json::from_str(json_slice)?;
    apply_digest_output(gateway_dir, store, session_id, source_agent_id, &output)?;
    Ok(())
}

/// Run after eligible sessions (spawn / checkpoint) when [`GatewayConfig::digest_agent`] is enabled.
pub async fn maybe_run_post_session_digest(
    config: &GatewayConfig,
    gateway_dir: &Path,
    store: Option<&Arc<GatewayStore>>,
    http_client: &reqwest::Client,
    session_id: &str,
    source_agent_id: &str,
    turn_count: u64,
    session_suspended: bool,
) {
    let Some(store) = store else {
        return;
    };
    if !config.digest_agent.enabled {
        return;
    }
    if session_suspended {
        return;
    }
    if turn_count < config.digest_agent.min_turns as u64 {
        return;
    }
    if source_agent_id == DIGEST_AGENT_ID {
        return;
    }
    let llm_cfg = match resolve_digest_llm_config(config) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                target: "post_session_digest",
                error = %e,
                "digest_agent enabled but LLM configuration is invalid; skipping digest"
            );
            return;
        }
    };
    let driver = match crate::llm::build_driver(llm_cfg.clone(), http_client.clone()) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(
                target: "post_session_digest",
                error = %e,
                "failed to build digest LLM driver; skipping digest"
            );
            return;
        }
    };
    if let Err(e) = run_post_session_digest_inner(
        gateway_dir,
        store,
        session_id,
        source_agent_id,
        &llm_cfg,
        driver.as_ref(),
    )
    .await
    {
        tracing::warn!(
            target: "post_session_digest",
            session_id = %session_id,
            error = %e,
            "post-session digest failed"
        );
    }
}

/// Test hook: run digest with a specific [`LlmDriver`] (no real provider).
pub async fn run_post_session_digest_with_driver(
    gateway_dir: &Path,
    store: &Arc<GatewayStore>,
    session_id: &str,
    source_agent_id: &str,
    digest_llm: &LlmConfig,
    driver: &dyn LlmDriver,
) -> anyhow::Result<()> {
    run_post_session_digest_inner(
        gateway_dir,
        store,
        session_id,
        source_agent_id,
        digest_llm,
        driver,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_frontmatter_basic() {
        let raw = "---\na: b\n---\n\nHello **body**";
        assert_eq!(strip_markdown_frontmatter(raw), "Hello **body**");
    }
}
