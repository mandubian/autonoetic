//! Probe OpenAI-compatible and llama.cpp servers for runtime context window size.
//!
//! Local servers expose `meta.n_ctx` on `/v1/models` (llama.cpp / LM Studio) or
//! `default_generation_settings.n_ctx` on `/props`. Remote OpenAI-compatible
//! catalogs may expose `context_length` per model.

use autonoetic_types::config::{GatewayConfig, LlmPreset};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Cache keyed by `(normalized_server_root, model_id)`.
#[derive(Debug, Default)]
pub struct LocalModelContextCache {
    inner: std::sync::RwLock<HashMap<(String, String), u32>>,
}

impl LocalModelContextCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, base_url: &str, model_id: &str) -> Option<u32> {
        let root = normalize_server_root(base_url)?;
        let key = (root, model_id.to_string());
        self.inner.read().ok()?.get(&key).copied()
    }

    pub fn insert(&self, base_url: &str, model_id: &str, tokens: u32) {
        let Some(root) = normalize_server_root(base_url) else {
            return;
        };
        if let Ok(mut guard) = self.inner.write() {
            guard.insert((root, model_id.to_string()), tokens);
        }
    }

    /// Probe presets that lack `context_window_tokens` and have a probeable base URL.
    pub async fn warm_from_config(&self, client: &Client, config: &GatewayConfig) {
        for (name, preset) in &config.llm_presets {
            if preset.routing.is_some() || preset.context_window_tokens.is_some() {
                continue;
            }
            let Some(model) = preset.model.as_deref().filter(|m| !m.is_empty()) else {
                continue;
            };
            let Some(base_url) = preset.base_url.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            if !is_probeable_preset(preset) {
                continue;
            }
            match fetch_context_window_tokens(client, base_url, model).await {
                Some(tokens) => {
                    self.insert(base_url, model, tokens);
                    tracing::info!(
                        target: "autonoetic::local_model_context",
                        preset = %name,
                        model = %model,
                        base_url = %base_url,
                        context_window_tokens = tokens,
                        "Probed context window from model server"
                    );
                }
                None => {
                    tracing::debug!(
                        target: "autonoetic::local_model_context",
                        preset = %name,
                        model = %model,
                        base_url = %base_url,
                        "Could not probe context window from model server"
                    );
                }
            }
        }
    }
}

pub fn is_probeable_preset(preset: &LlmPreset) -> bool {
    preset
        .base_url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty())
}

/// Fetch runtime context window from a chat/completions or server base URL.
pub async fn fetch_context_window_tokens(
    client: &Client,
    base_url: &str,
    model_id: &str,
) -> Option<u32> {
    let root = normalize_server_root(base_url)?;

    if let Some(tokens) = fetch_from_models_endpoint(client, &root, model_id).await {
        return Some(tokens);
    }

    fetch_from_props_endpoint(client, &root).await
}

fn normalize_server_root(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let without_chat = trimmed
        .strip_suffix("/chat/completions")
        .or_else(|| trimmed.strip_suffix("/completions"))
        .unwrap_or(trimmed);
    let without_v1 = without_chat.strip_suffix("/v1").unwrap_or(without_chat);
    Some(without_v1.trim_end_matches('/').to_string())
}

fn models_api_url(server_root: &str) -> String {
    format!("{}/v1/models", server_root.trim_end_matches('/'))
}

fn props_api_url(server_root: &str) -> String {
    format!("{}/props", server_root.trim_end_matches('/'))
}

async fn fetch_from_models_endpoint(
    client: &Client,
    server_root: &str,
    model_id: &str,
) -> Option<u32> {
    let url = models_api_url(server_root);
    let resp: Value = client
        .get(&url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    parse_context_window_from_models_response(&resp, model_id)
}

async fn fetch_from_props_endpoint(client: &Client, server_root: &str) -> Option<u32> {
    let url = props_api_url(server_root);
    let resp: Value = client
        .get(&url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    parse_context_window_from_props_response(&resp)
}

pub fn parse_context_window_from_models_response(resp: &Value, model_id: &str) -> Option<u32> {
    let arrays = [
        resp.get("data").and_then(|v| v.as_array()),
        resp.get("models").and_then(|v| v.as_array()),
    ];
    for arr in arrays.into_iter().flatten() {
        for obj in arr {
            if !model_entry_matches(obj, model_id) {
                continue;
            }
            if let Some(tokens) = parse_context_window_from_model_object(obj) {
                return Some(tokens);
            }
        }
    }
    None
}

pub fn parse_context_window_from_props_response(resp: &Value) -> Option<u32> {
    parse_u32_field(
        resp.pointer("/default_generation_settings/n_ctx")
            .or_else(|| resp.get("n_ctx")),
    )
}

fn model_entry_matches(obj: &Value, model_id: &str) -> bool {
    let needle = model_id.trim();
    if needle.is_empty() {
        return false;
    }
    for key in ["id", "name", "model"] {
        if obj
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| s == needle)
        {
            return true;
        }
    }
    false
}

fn parse_context_window_from_model_object(obj: &Value) -> Option<u32> {
    if let Some(meta) = obj.get("meta") {
        if let Some(n_ctx) = parse_u32_field(meta.get("n_ctx")) {
            return Some(n_ctx);
        }
    }
    if let Some(context_length) = parse_u32_field(obj.get("context_length")) {
        return Some(context_length);
    }
    None
}

fn parse_u32_field(value: Option<&Value>) -> Option<u32> {
    let v = value?;
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).ok().filter(|&n| n > 0);
    }
    if let Some(s) = v.as_str() {
        return s.trim().parse::<u32>().ok().filter(|&n| n > 0);
    }
    None
}

/// Insert or replace `context_window_tokens` in a YAML config string (default preset block).
pub fn patch_context_window_tokens_in_yaml(content: &str, tokens: u32) -> String {
    let line = format!("context_window_tokens: {tokens}");
    if let Ok(re) = regex::Regex::new(r"(?m)^(\s*)context_window_tokens:\s*\d+\s*$") {
        if re.is_match(content) {
            return re
                .replace_all(content, |caps: &regex::Captures<'_>| {
                    format!("{}{line}", &caps[1])
                })
                .into_owned();
        }
    }
    if let Ok(re) = regex::Regex::new(r#"(?m)(^\s*base_url:\s*"[^"]*"\s*\n)"#) {
        if re.is_match(content) {
            return re
                .replace_all(content, |caps: &regex::Captures<'_>| {
                    format!("{}    {line}\n", &caps[1])
                })
                .to_string();
        }
    }
    if let Ok(re) = regex::Regex::new(r#"(?m)(^\s*model:\s*"[^"]*"\s*\n)"#) {
        if re.is_match(content) {
            return re
                .replace_all(content, |caps: &regex::Captures<'_>| {
                    format!("{}    {line}\n", &caps[1])
                })
                .to_string();
        }
    }
    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_server_root_strips_chat_completions_and_v1() {
        assert_eq!(
            normalize_server_root("http://192.168.1.20:8080/v1/chat/completions").as_deref(),
            Some("http://192.168.1.20:8080")
        );
    }

    #[test]
    fn parse_llamacpp_models_response() {
        let resp = json!({
            "data": [{
                "id": "Qwen3.6-27B-UD-Q5_K_XL.gguf",
                "meta": { "n_ctx": 114688, "n_ctx_train": 262144 }
            }]
        });
        assert_eq!(
            parse_context_window_from_models_response(&resp, "Qwen3.6-27B-UD-Q5_K_XL.gguf"),
            Some(114688)
        );
    }

    #[test]
    fn parse_openrouter_style_context_length() {
        let resp = json!({
            "data": [{
                "id": "anthropic/claude-sonnet-4",
                "context_length": 200000
            }]
        });
        assert_eq!(
            parse_context_window_from_models_response(&resp, "anthropic/claude-sonnet-4"),
            Some(200000)
        );
    }

    #[test]
    fn parse_props_response() {
        let resp = json!({
            "default_generation_settings": { "n_ctx": 114688 }
        });
        assert_eq!(parse_context_window_from_props_response(&resp), Some(114688));
    }

    #[test]
    fn patch_context_window_tokens_inserts_after_base_url() {
        let yaml = r#"llm_presets:
  default:
    provider: "llamacpp"
    model: "qwen"
    base_url: "http://localhost:8080/v1/chat/completions"
"#;
        let patched = patch_context_window_tokens_in_yaml(yaml, 114688);
        assert!(patched.contains("context_window_tokens: 114688"));
    }

    #[test]
    fn patch_context_window_tokens_replaces_existing() {
        let yaml = "    context_window_tokens: 32768\n";
        let patched = patch_context_window_tokens_in_yaml(yaml, 114688);
        assert_eq!(patched.trim(), "context_window_tokens: 114688");
    }
}
