//! Interactive provider and model discovery for `autonoetic run`.
//!
//! Detects available LLM providers (API keys in environment, local servers
//! running), fetches their model catalogs, and presents an interactive menu
//! so the user can pick a provider + model without editing YAML.

use std::io::{self, BufRead, Write};
use std::time::Duration;

// ── Provider registry ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProviderEntry {
    pub name: &'static str,
    pub display: &'static str,
    pub kind: ProviderKind,
}

#[derive(Debug, Clone)]
pub enum ProviderKind {
    /// Remote provider requiring an API key env var.
    Remote {
        api_key_env: &'static str,
        models_url: &'static str,
    },
    /// Local server probed at a base URL (no key).
    Local {
        models_url: &'static str,
    },
}

/// All known providers, in display order.
fn all_providers() -> Vec<ProviderEntry> {
    vec![
        ProviderEntry {
            name: "anthropic",
            display: "Anthropic (Claude)",
            kind: ProviderKind::Remote {
                api_key_env: "ANTHROPIC_API_KEY",
                models_url: "https://api.anthropic.com/v1/models",
            },
        },
        ProviderEntry {
            name: "openrouter",
            display: "OpenRouter (multi-provider gateway)",
            kind: ProviderKind::Remote {
                api_key_env: "OPENROUTER_API_KEY",
                models_url: "https://openrouter.ai/api/v1/models",
            },
        },
        ProviderEntry {
            name: "openai",
            display: "OpenAI",
            kind: ProviderKind::Remote {
                api_key_env: "OPENAI_API_KEY",
                models_url: "https://api.openai.com/v1/models",
            },
        },
        ProviderEntry {
            name: "deepseek",
            display: "DeepSeek",
            kind: ProviderKind::Remote {
                api_key_env: "DEEPSEEK_API_KEY",
                models_url: "https://api.deepseek.com/v1/models",
            },
        },
        ProviderEntry {
            name: "mistral",
            display: "Mistral AI",
            kind: ProviderKind::Remote {
                api_key_env: "MISTRAL_API_KEY",
                models_url: "https://api.mistral.ai/v1/models",
            },
        },
        ProviderEntry {
            name: "groq",
            display: "Groq",
            kind: ProviderKind::Remote {
                api_key_env: "GROQ_API_KEY",
                models_url: "https://api.groq.com/openai/v1/models",
            },
        },
        ProviderEntry {
            name: "together",
            display: "Together AI",
            kind: ProviderKind::Remote {
                api_key_env: "TOGETHER_API_KEY",
                models_url: "https://api.together.xyz/v1/models",
            },
        },
        ProviderEntry {
            name: "xai",
            display: "xAI (Grok)",
            kind: ProviderKind::Remote {
                api_key_env: "XAI_API_KEY",
                models_url: "https://api.x.ai/v1/models",
            },
        },
        ProviderEntry {
            name: "gemini",
            display: "Google Gemini",
            kind: ProviderKind::Remote {
                api_key_env: "GEMINI_API_KEY",
                models_url: "https://generativelanguage.googleapis.com/v1beta/models",
            },
        },
        ProviderEntry {
            name: "cohere",
            display: "Cohere",
            kind: ProviderKind::Remote {
                api_key_env: "COHERE_API_KEY",
                models_url: "https://api.cohere.com/v2/models",
            },
        },
        // ── Local providers ──
        ProviderEntry {
            name: "ollama",
            display: "Ollama (local)",
            kind: ProviderKind::Local {
                models_url: "http://localhost:11434/api/tags",
            },
        },
        ProviderEntry {
            name: "lmstudio",
            display: "LM Studio (local)",
            kind: ProviderKind::Local {
                models_url: "http://localhost:1234/v1/models",
            },
        },
        ProviderEntry {
            name: "vllm",
            display: "vLLM (local)",
            kind: ProviderKind::Local {
                models_url: "http://localhost:8000/v1/models",
            },
        },
        ProviderEntry {
            name: "llamacpp",
            display: "llama.cpp server (local)",
            kind: ProviderKind::Local {
                models_url: "http://localhost:8080/v1/models",
            },
        },
    ]
}

// ── Detection ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DetectedProvider {
    pub entry: ProviderEntry,
    pub source: DetectionSource,
}

#[derive(Debug, Clone)]
pub enum DetectionSource {
    ApiKeyEnv(String),
    LocalProbe,
}

impl std::fmt::Display for DetectedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            DetectionSource::ApiKeyEnv(var) => {
                write!(f, "{} ({})", self.entry.display, var)
            }
            DetectionSource::LocalProbe => {
                write!(f, "{}", self.entry.display)
            }
        }
    }
}

/// Detect which providers are available right now.
///
/// Remote providers are detected by checking env vars.
/// Local providers are probed with a short HTTP timeout.
pub async fn detect_available_providers(client: &reqwest::Client) -> Vec<DetectedProvider> {
    let mut detected = Vec::new();

    for entry in all_providers() {
        match &entry.kind {
            ProviderKind::Remote { api_key_env, .. } => {
                if std::env::var(api_key_env).is_ok() {
                    detected.push(DetectedProvider {
                        entry: entry.clone(),
                        source: DetectionSource::ApiKeyEnv((*api_key_env).to_string()),
                    });
                }
            }
            ProviderKind::Local { models_url } => {
                if probe_local(client, models_url).await {
                    detected.push(DetectedProvider {
                        entry: entry.clone(),
                        source: DetectionSource::LocalProbe,
                    });
                }
            }
        }
    }

    detected
}

async fn probe_local(client: &reqwest::Client, url: &str) -> bool {
    client
        .get(url)
        .timeout(Duration::from_millis(800))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

// ── Model fetching ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
}

impl std::fmt::Display for ModelInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref name) = self.display_name {
            if name != &self.id {
                return write!(f, "{} ({})", self.id, name);
            }
        }
        write!(f, "{}", self.id)
    }
}

/// Fetch available models from a provider's API.
pub async fn fetch_models(
    client: &reqwest::Client,
    provider: &DetectedProvider,
) -> anyhow::Result<Vec<ModelInfo>> {
    let models_url = match &provider.entry.kind {
        ProviderKind::Remote { models_url, .. } => *models_url,
        ProviderKind::Local { models_url } => *models_url,
    };

    if provider.entry.name == "ollama" {
        return fetch_ollama_models(client, models_url).await;
    }

    if provider.entry.name == "gemini" {
        return fetch_gemini_models(client, models_url).await;
    }

    if provider.entry.name == "anthropic" {
        return fetch_anthropic_models(client, models_url).await;
    }

    if provider.entry.name == "cohere" {
        return fetch_cohere_models(client, models_url).await;
    }

    fetch_openai_compatible_models(client, models_url, &provider.entry).await
}

/// OpenAI-compatible `/v1/models` response (OpenAI, OpenRouter, Groq, etc.)
async fn fetch_openai_compatible_models(
    client: &reqwest::Client,
    url: &str,
    entry: &ProviderEntry,
) -> anyhow::Result<Vec<ModelInfo>> {
    let mut req = client.get(url).timeout(Duration::from_secs(15));

    if let ProviderKind::Remote { api_key_env, .. } = &entry.kind {
        if let Ok(key) = std::env::var(api_key_env) {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
    }

    let resp: serde_json::Value = req.send().await?.json().await?;
    let models = parse_openai_models_response(&resp, entry.name);
    Ok(models)
}

fn parse_openai_models_response(resp: &serde_json::Value, provider_name: &str) -> Vec<ModelInfo> {
    let data = resp.get("data").and_then(|d| d.as_array());
    let Some(arr) = data else { return Vec::new() };

    let mut models: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|obj| {
            let id = obj.get("id")?.as_str()?.to_string();
            if should_skip_model(&id, provider_name) {
                return None;
            }
            let display_name = obj.get("name").and_then(|n| n.as_str()).map(String::from);
            Some(ModelInfo { id, display_name })
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// Ollama uses `/api/tags` with a different response shape.
async fn fetch_ollama_models(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<ModelInfo>> {
    let resp: serde_json::Value = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await?
        .json()
        .await?;

    let models_arr = resp.get("models").and_then(|m| m.as_array());
    let Some(arr) = models_arr else {
        return Ok(Vec::new());
    };

    let mut models: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|obj| {
            let name = obj.get("name")?.as_str()?.to_string();
            Some(ModelInfo {
                display_name: None,
                id: name,
            })
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Gemini uses a different API structure.
async fn fetch_gemini_models(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<ModelInfo>> {
    let key = std::env::var("GEMINI_API_KEY")?;
    let full_url = format!("{url}?key={key}");
    let resp: serde_json::Value = client
        .get(&full_url)
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .json()
        .await?;

    let models_arr = resp.get("models").and_then(|m| m.as_array());
    let Some(arr) = models_arr else {
        return Ok(Vec::new());
    };

    let mut models: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|obj| {
            let name = obj.get("name")?.as_str()?;
            let id = name.strip_prefix("models/").unwrap_or(name).to_string();
            if !id.contains("gemini") {
                return None;
            }
            let display = obj.get("displayName").and_then(|d| d.as_str()).map(String::from);
            Some(ModelInfo {
                id,
                display_name: display,
            })
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Anthropic uses x-api-key header and a different response structure.
async fn fetch_anthropic_models(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<ModelInfo>> {
    let key = std::env::var("ANTHROPIC_API_KEY")?;
    let resp: serde_json::Value = client
        .get(url)
        .header("x-api-key", &key)
        .header("anthropic-version", "2023-06-01")
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .json()
        .await?;

    let models_arr = resp.get("data").and_then(|m| m.as_array());
    let Some(arr) = models_arr else {
        return Ok(Vec::new());
    };

    let mut models: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|obj| {
            let id = obj.get("id")?.as_str()?.to_string();
            let display = obj.get("display_name").and_then(|d| d.as_str()).map(String::from);
            Some(ModelInfo {
                id,
                display_name: display,
            })
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Cohere uses a different response structure.
async fn fetch_cohere_models(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<ModelInfo>> {
    let key = std::env::var("COHERE_API_KEY")?;
    let resp: serde_json::Value = client
        .get(url)
        .header("Authorization", format!("Bearer {key}"))
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .json()
        .await?;

    let models_arr = resp.get("models").and_then(|m| m.as_array());
    let Some(arr) = models_arr else {
        return Ok(Vec::new());
    };

    let mut models: Vec<ModelInfo> = arr
        .iter()
        .filter_map(|obj| {
            let name = obj.get("name")?.as_str()?.to_string();
            Some(ModelInfo {
                display_name: None,
                id: name,
            })
        })
        .collect();

    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

fn should_skip_model(id: &str, provider: &str) -> bool {
    match provider {
        "openai" => {
            // Skip embedding, tts, whisper, dall-e, moderation, babbage, davinci models
            id.starts_with("text-embedding")
                || id.starts_with("tts-")
                || id.starts_with("whisper-")
                || id.starts_with("dall-e")
                || id.starts_with("text-moderation")
                || id.starts_with("babbage")
                || id.starts_with("davinci")
                || id.contains("realtime")
                || id.contains("audio")
                || id.contains("transcription")
                || id.contains("search")
        }
        _ => false,
    }
}

// ── Interactive selection ──────────────────────────────────────────────────

fn read_number(prompt: &str, max: usize) -> anyhow::Result<usize> {
    let stdin = io::stdin();
    let mut stdout = io::stderr();
    loop {
        write!(stdout, "{prompt}")?;
        stdout.flush()?;
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let trimmed = line.trim();
        if let Ok(n) = trimmed.parse::<usize>() {
            if n >= 1 && n <= max {
                return Ok(n);
            }
        }
        writeln!(stdout, "  Please enter a number between 1 and {max}.")?;
    }
}

fn read_line_with_prompt(prompt: &str) -> anyhow::Result<String> {
    let stdin = io::stdin();
    let mut stdout = io::stderr();
    write!(stdout, "{prompt}")?;
    stdout.flush()?;
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Present detected providers, let user pick, fetch models, let user pick.
///
/// Returns `(provider_name, model_id)` ready for config generation.
pub async fn interactive_select(
    client: &reqwest::Client,
) -> anyhow::Result<(String, String)> {
    let mut stderr = io::stderr();

    writeln!(stderr, "\n  Detecting available LLM providers...")?;
    let detected = detect_available_providers(client).await;

    // Build the menu: detected providers first, then option to see all
    if detected.is_empty() {
        writeln!(
            stderr,
            "\n  No LLM providers detected automatically."
        )?;
        writeln!(stderr, "  For remote providers, set the API key env var (e.g. OPENROUTER_API_KEY).")?;
        writeln!(stderr, "  For local providers, make sure the server is running.\n")?;
    } else {
        writeln!(stderr, "\n  Detected providers:\n")?;
        for (i, dp) in detected.iter().enumerate() {
            writeln!(stderr, "    {}) {dp}", i + 1)?;
        }
        writeln!(stderr)?;
    }

    // Always offer "all providers" and "manual entry" options
    let detected_count = detected.len();
    let all_opt = detected_count + 1;
    let manual_opt = detected_count + 2;

    writeln!(stderr, "    {all_opt}) Show all supported providers")?;
    writeln!(stderr, "    {manual_opt}) Manual entry (provider + model)")?;
    writeln!(stderr)?;

    let choice = read_number("  Select provider: ", manual_opt)?;

    if choice == manual_opt {
        return manual_entry();
    }

    let provider_entry = if choice == all_opt {
        // Show all providers
        let all = all_providers();
        writeln!(stderr, "\n  All supported providers:\n")?;
        for (i, p) in all.iter().enumerate() {
            let available = match &p.kind {
                ProviderKind::Remote { api_key_env, .. } => {
                    if std::env::var(api_key_env).is_ok() {
                        " [key found]"
                    } else {
                        ""
                    }
                }
                ProviderKind::Local { .. } => {
                    if detected.iter().any(|d| d.entry.name == p.name) {
                        " [running]"
                    } else {
                        ""
                    }
                }
            };
            writeln!(stderr, "    {}) {}{available}", i + 1, p.display)?;
        }
        writeln!(stderr)?;

        let idx = read_number("  Select provider: ", all.len())?;
        let chosen = &all[idx - 1];

        // If it's a remote provider without a key, prompt for it
        if let ProviderKind::Remote { api_key_env, .. } = &chosen.kind {
            if std::env::var(api_key_env).is_err() {
                writeln!(
                    stderr,
                    "\n  {api_key_env} is not set. Please enter your API key."
                )?;
                let key = read_line_with_prompt(&format!("  {api_key_env}= "))?;
                if key.is_empty() {
                    anyhow::bail!("API key cannot be empty");
                }
                std::env::set_var(api_key_env, &key);
                writeln!(stderr, "  Key set for this session.")?;
            }
        }

        DetectedProvider {
            entry: chosen.clone(),
            source: match &chosen.kind {
                ProviderKind::Remote { api_key_env, .. } => {
                    DetectionSource::ApiKeyEnv((*api_key_env).to_string())
                }
                ProviderKind::Local { .. } => DetectionSource::LocalProbe,
            },
        }
    } else {
        detected[choice - 1].clone()
    };

    // Fetch models
    writeln!(
        stderr,
        "\n  Fetching models from {}...",
        provider_entry.entry.display
    )?;

    let models = match fetch_models(client, &provider_entry).await {
        Ok(m) if !m.is_empty() => m,
        Ok(_) => {
            writeln!(stderr, "  No models returned. You can enter a model ID manually.")?;
            let model = read_line_with_prompt("  Model ID: ")?;
            if model.is_empty() {
                anyhow::bail!("Model ID cannot be empty");
            }
            return Ok((provider_entry.entry.name.to_string(), model));
        }
        Err(e) => {
            writeln!(
                stderr,
                "  Could not fetch models ({e}). You can enter a model ID manually."
            )?;
            let model = read_line_with_prompt("  Model ID: ")?;
            if model.is_empty() {
                anyhow::bail!("Model ID cannot be empty");
            }
            return Ok((provider_entry.entry.name.to_string(), model));
        }
    };

    // Display with pagination for large catalogs
    display_model_menu(&models)?;

    let idx = read_number(
        "  Select model (or 0 to type manually): ",
        models.len() + 1, // +1 to allow "0" range hack below
    );

    // The read_number requires 1..=max, so handle manual entry via a separate prompt
    // if the list is huge and user may want to type directly
    let model_id = match idx {
        Ok(n) if n >= 1 && n <= models.len() => models[n - 1].id.clone(),
        _ => {
            let manual = read_line_with_prompt("  Model ID: ")?;
            if manual.is_empty() {
                anyhow::bail!("Model ID cannot be empty");
            }
            manual
        }
    };

    let provider_name = if provider_entry.entry.name == "llamacpp" {
        // llama.cpp server is OpenAI-compatible; map to a generic name
        // so the config works with the existing provider resolution
        "openai".to_string()
    } else {
        provider_entry.entry.name.to_string()
    };

    Ok((provider_name, model_id))
}

fn display_model_menu(models: &[ModelInfo]) -> anyhow::Result<()> {
    let mut stderr = io::stderr();
    let total = models.len();

    if total <= 30 {
        writeln!(stderr, "\n  Available models:\n")?;
        for (i, m) in models.iter().enumerate() {
            writeln!(stderr, "    {:>3}) {m}", i + 1)?;
        }
    } else {
        // Paginate: show first 25 and offer search
        writeln!(stderr, "\n  {total} models available. Showing first 25:\n")?;
        for (i, m) in models.iter().take(25).enumerate() {
            writeln!(stderr, "    {:>3}) {m}", i + 1)?;
        }
        writeln!(stderr, "    ...")?;
        writeln!(stderr, "\n  Tip: type a search term to filter, or a number to select.\n")?;

        let input = read_line_with_prompt("  Filter or select: ")?;

        if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= total {
                // They picked a number from the full list
                writeln!(stderr, "  Selected: {}", models[n - 1])?;
                return Ok(());
            }
        }

        // Filter
        let filtered: Vec<(usize, &ModelInfo)> = models
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.id.to_lowercase().contains(&input.to_lowercase())
                    || m.display_name
                        .as_ref()
                        .map(|d| d.to_lowercase().contains(&input.to_lowercase()))
                        .unwrap_or(false)
            })
            .collect();

        if filtered.is_empty() {
            writeln!(stderr, "  No models match '{input}'.")?;
        } else {
            writeln!(stderr, "\n  Matching models:\n")?;
            for (_, (orig_idx, m)) in filtered.iter().enumerate() {
                writeln!(stderr, "    {:>3}) {m}", orig_idx + 1)?;
            }
        }
    }
    writeln!(stderr)?;
    Ok(())
}

fn manual_entry() -> anyhow::Result<(String, String)> {
    let mut stderr = io::stderr();
    writeln!(
        stderr,
        "\n  Known providers: anthropic, openai, openrouter, ollama, lmstudio, \
         deepseek, mistral, groq, together, gemini, cohere, xai, vllm, llamacpp\n"
    )?;
    let provider = read_line_with_prompt("  Provider: ")?;
    if provider.is_empty() {
        anyhow::bail!("Provider cannot be empty");
    }
    let model = read_line_with_prompt("  Model ID: ")?;
    if model.is_empty() {
        anyhow::bail!("Model ID cannot be empty");
    }

    let effective_provider = if provider == "llamacpp" || provider == "llama.cpp" {
        "openai".to_string()
    } else {
        provider
    };

    Ok((effective_provider, model))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_models_filters_non_chat() {
        let resp = serde_json::json!({
            "data": [
                {"id": "gpt-4o"},
                {"id": "gpt-4o-mini"},
                {"id": "text-embedding-ada-002"},
                {"id": "tts-1"},
                {"id": "dall-e-3"},
                {"id": "o1-preview"},
            ]
        });
        let models = parse_openai_models_response(&resp, "openai");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-4o"));
        assert!(ids.contains(&"o1-preview"));
        assert!(!ids.contains(&"text-embedding-ada-002"));
        assert!(!ids.contains(&"tts-1"));
        assert!(!ids.contains(&"dall-e-3"));
    }

    #[test]
    fn parse_openrouter_models_preserves_namespaced_ids() {
        let resp = serde_json::json!({
            "data": [
                {"id": "anthropic/claude-sonnet-4", "name": "Claude Sonnet 4"},
                {"id": "openai/gpt-4o"},
            ]
        });
        let models = parse_openai_models_response(&resp, "openrouter");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn should_skip_filters_openai_non_chat() {
        assert!(should_skip_model("text-embedding-3-large", "openai"));
        assert!(should_skip_model("tts-1-hd", "openai"));
        assert!(!should_skip_model("gpt-4o", "openai"));
        assert!(!should_skip_model("some-model", "openrouter"));
    }

    #[test]
    fn all_providers_returns_known_set() {
        let providers = all_providers();
        let names: Vec<&str> = providers.iter().map(|p| p.name).collect();
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"openrouter"));
        assert!(names.contains(&"ollama"));
        assert!(names.contains(&"lmstudio"));
        assert!(names.contains(&"llamacpp"));
    }
}
