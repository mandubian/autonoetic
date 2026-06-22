use crate::llm::ToolDefinition;
use crate::policy::PolicyEngine;
use crate::runtime::active_execution_registry::NativeToolRunContext;
use crate::runtime::network_policy::{self, DeclarationRequirement};
use crate::runtime::tools::web_redirect::{
    hosts_same_redirect_scope, is_redirect_status, resolve_redirect_location,
    MAX_WEB_REDIRECT_HOPS,
};
use crate::runtime::tools::{block_on_http, extract_host, NativeTool, NativeToolRegistry};
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::background::{ApprovalRequest, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::tagged;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration as StdDuration, Instant};

pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(WebSearchTool));
    registry.register(Box::new(WebFetchTool));
    registry.register(Box::new(WebCallTool));
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    engine_url: Option<String>,
    #[serde(default)]
    duckduckgo_engine_url: Option<String>,
    #[serde(default)]
    google_engine_url: Option<String>,
    #[serde(default)]
    google_engine_id: Option<String>,
    #[serde(default)]
    google_api_key_env: Option<String>,
    #[serde(default)]
    google_engine_id_env: Option<String>,
    #[serde(default)]
    cache_ttl_secs: Option<u64>,
    /// Approval request ID from a previous approval-required response.
    #[serde(default)]
    approval_ref: Option<String>,
}

fn default_web_search_engine_url() -> String {
    "https://duckduckgo.com/".to_string()
}

fn default_google_search_engine_url() -> String {
    "https://www.googleapis.com/customsearch/v1".to_string()
}

const GOOGLE_API_KEY_ENV_DEFAULT: &str = "AUTONOETIC_GOOGLE_SEARCH_API_KEY";
const GOOGLE_API_KEY_ENV_LEGACY: &str = "GOOGLE_SEARCH_API_KEY";
const GOOGLE_ENGINE_ID_ENV_DEFAULT: &str = "AUTONOETIC_GOOGLE_SEARCH_ENGINE_ID";
const GOOGLE_ENGINE_ID_ENV_LEGACY: &str = "GOOGLE_SEARCH_ENGINE_ID";
const GOOGLE_ENGINE_ID_ENV_LEGACY_ALT: &str = "GOOGLE_SEARCH_CX";
const WEB_SEARCH_CACHE_TTL_DEFAULT_SECS: u64 = 120;
const WEB_SEARCH_CACHE_TTL_MAX_SECS: u64 = 3_600;

#[derive(Debug, Clone)]
struct WebSearchCacheEntry {
    expires_at: Instant,
    payload: serde_json::Value,
}

static WEB_SEARCH_CACHE: LazyLock<Mutex<HashMap<String, WebSearchCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSearchProvider {
    Auto,
    DuckDuckGo,
    Google,
}

impl WebSearchProvider {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::DuckDuckGo => "duckduckgo",
            Self::Google => "google",
        }
    }
}

fn parse_web_search_provider(raw: Option<&str>) -> anyhow::Result<WebSearchProvider> {
    let normalized = raw
        .map(|value| value.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "auto".to_string());
    match normalized.as_str() {
        "auto" => Ok(WebSearchProvider::Auto),
        "duckduckgo" | "ddg" => Ok(WebSearchProvider::DuckDuckGo),
        "google" => Ok(WebSearchProvider::Google),
        other => Err(anyhow::Error::from(tagged::Tagged::validation(
            anyhow::anyhow!(
                "Unsupported web.search provider '{}'. Use 'auto', 'duckduckgo', or 'google'.",
                other
            ),
        ))),
    }
}

fn network_policy_violation_to_anyhow(
    violation: network_policy::NetworkPolicyViolation,
) -> anyhow::Error {
    let err = anyhow::anyhow!(violation.message);
    match violation.error_type {
        "missing_remote_access_declaration"
        | "undeclared_remote_target"
        | "remote_preapproval_requires_network_capability" => {
            anyhow::Error::from(tagged::Tagged::validation(err))
        }
        _ => anyhow::Error::from(tagged::Tagged::permission(err)),
    }
}

fn enforce_remote_target_for_web(
    manifest: &AgentManifest,
    agent_dir: &Path,
    host: &str,
    request_url: &str,
) -> anyhow::Result<()> {
    network_policy::enforce_remote_target_policy(
        manifest,
        agent_dir,
        host,
        Some(request_url),
        DeclarationRequirement::Required,
    )
    .map(|_| ())
    .map_err(network_policy_violation_to_anyhow)
}

fn validate_approval_ref_context(
    approval: &ApprovalRequest,
    manifest: &AgentManifest,
    session_id: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        approval.agent_id == manifest.agent.id,
        "approval_ref belongs to agent '{}' but current agent is '{}'",
        approval.agent_id,
        manifest.agent.id
    );
    let sid = session_id.ok_or_else(|| {
        anyhow::anyhow!("approval_ref requires a session context but no session_id was provided")
    })?;
    let current_root = crate::runtime::content_store::root_session_id(sid);
    let approved_root = approval
        .root_session_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::runtime::content_store::root_session_id(&approval.session_id));
    anyhow::ensure!(
        approved_root == current_root,
        "approval_ref belongs to root session '{}' but current root session is '{}'",
        approved_root,
        current_root
    );
    Ok(())
}

fn session_grants_allow_host(
    store: &crate::scheduler::gateway_store::GatewayStore,
    session_id: Option<&str>,
    host: &str,
) -> bool {
    let Some(sid) = session_id else {
        return false;
    };
    let root_sid = crate::runtime::content_store::root_session_id(sid);
    store.session_grants_cover_targets(root_sid, &[host.to_string()])
}

fn resolve_duckduckgo_engine_url(args: &WebSearchArgs) -> String {
    args.duckduckgo_engine_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            args.engine_url
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .unwrap_or_else(default_web_search_engine_url)
}

fn resolve_google_engine_url(args: &WebSearchArgs) -> String {
    args.google_engine_url
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            args.engine_url
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
        })
        .unwrap_or_else(default_google_search_engine_url)
}

fn resolve_web_search_cache_ttl_secs(args: &WebSearchArgs) -> u64 {
    args.cache_ttl_secs
        .unwrap_or(WEB_SEARCH_CACHE_TTL_DEFAULT_SECS)
        .min(WEB_SEARCH_CACHE_TTL_MAX_SECS)
}

fn web_search_cache_key(
    args: &WebSearchArgs,
    provider: WebSearchProvider,
    requested_max_results: usize,
    timeout_secs: u64,
) -> String {
    let query = args.query.trim();
    let ddg_engine_url = resolve_duckduckgo_engine_url(args);
    let google_engine_url = resolve_google_engine_url(args);
    let google_engine_id = args
        .google_engine_id
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let google_api_key_env = args
        .google_api_key_env
        .as_deref()
        .unwrap_or(GOOGLE_API_KEY_ENV_DEFAULT);
    let google_engine_id_env = args
        .google_engine_id_env
        .as_deref()
        .unwrap_or(GOOGLE_ENGINE_ID_ENV_DEFAULT);
    format!(
        "provider={}|query={}|max_results={}|timeout_secs={}|ddg_engine_url={}|google_engine_url={}|google_engine_id={}|google_api_key_env={}|google_engine_id_env={}",
        provider.as_str(),
        query,
        requested_max_results,
        timeout_secs,
        ddg_engine_url,
        google_engine_url,
        google_engine_id,
        google_api_key_env,
        google_engine_id_env
    )
}

fn web_search_cache_get(key: &str) -> Option<serde_json::Value> {
    let now = Instant::now();
    let mut cache = WEB_SEARCH_CACHE.lock().ok()?;
    cache.retain(|_, entry| entry.expires_at > now);
    cache.get(key).map(|entry| entry.payload.clone())
}

fn web_search_cache_put(key: String, payload: serde_json::Value, ttl_secs: u64) {
    if ttl_secs == 0 {
        return;
    }
    if let Ok(mut cache) = WEB_SEARCH_CACHE.lock() {
        let now = Instant::now();
        cache.retain(|_, entry| entry.expires_at > now);
        cache.insert(
            key,
            WebSearchCacheEntry {
                expires_at: now + StdDuration::from_secs(ttl_secs),
                payload,
            },
        );
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn resolve_google_api_key(args: &WebSearchArgs) -> anyhow::Result<String> {
    let key_env = args
        .google_api_key_env
        .as_deref()
        .unwrap_or(GOOGLE_API_KEY_ENV_DEFAULT);
    let key = non_empty_env(key_env).or_else(|| {
        if args.google_api_key_env.is_none() {
            non_empty_env(GOOGLE_API_KEY_ENV_LEGACY)
        } else {
            None
        }
    });
    key.ok_or_else(|| {
        anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
            "Google web.search requires API key env '{}'",
            key_env
        )))
    })
}

fn resolve_google_engine_id(args: &WebSearchArgs) -> anyhow::Result<String> {
    if let Some(explicit) = args
        .google_engine_id
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        return Ok(explicit.to_string());
    }
    let engine_id_env = args
        .google_engine_id_env
        .as_deref()
        .unwrap_or(GOOGLE_ENGINE_ID_ENV_DEFAULT);
    let engine_id = non_empty_env(engine_id_env).or_else(|| {
        if args.google_engine_id_env.is_none() {
            non_empty_env(GOOGLE_ENGINE_ID_ENV_LEGACY)
                .or_else(|| non_empty_env(GOOGLE_ENGINE_ID_ENV_LEGACY_ALT))
        } else {
            None
        }
    });
    engine_id.ok_or_else(|| {
        anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
            "Google web.search requires engine id via argument 'google_engine_id' or env '{}'",
            engine_id_env
        )))
    })
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_duckduckgo_results(
    payload: &serde_json::Value,
    max_results: usize,
) -> Vec<serde_json::Value> {
    fn maybe_push(
        out: &mut Vec<serde_json::Value>,
        seen_urls: &mut HashSet<String>,
        text: &str,
        url: &str,
        max_results: usize,
    ) {
        if out.len() >= max_results {
            return;
        }
        if text.trim().is_empty() || url.trim().is_empty() {
            return;
        }
        if !seen_urls.insert(url.to_string()) {
            return;
        }
        out.push(serde_json::json!({
            "title": normalize_text(text),
            "url": url,
            "snippet": normalize_text(text),
        }));
    }

    fn walk(
        node: &serde_json::Value,
        out: &mut Vec<serde_json::Value>,
        seen_urls: &mut HashSet<String>,
        max_results: usize,
    ) {
        if out.len() >= max_results {
            return;
        }

        if let Some(obj) = node.as_object() {
            if let (Some(text), Some(url)) = (
                obj.get("Text").and_then(|v| v.as_str()),
                obj.get("FirstURL").and_then(|v| v.as_str()),
            ) {
                maybe_push(out, seen_urls, text, url, max_results);
            }
            if let Some(topics) = obj.get("Topics").and_then(|v| v.as_array()) {
                for topic in topics {
                    walk(topic, out, seen_urls, max_results);
                    if out.len() >= max_results {
                        return;
                    }
                }
            }
            return;
        }

        if let Some(arr) = node.as_array() {
            for item in arr {
                walk(item, out, seen_urls, max_results);
                if out.len() >= max_results {
                    return;
                }
            }
        }
    }

    let mut out = Vec::new();
    let mut seen_urls = HashSet::new();

    if let Some(results) = payload.get("Results").and_then(|v| v.as_array()) {
        for result in results {
            walk(result, &mut out, &mut seen_urls, max_results);
            if out.len() >= max_results {
                return out;
            }
        }
    }
    if let Some(related) = payload.get("RelatedTopics").and_then(|v| v.as_array()) {
        for topic in related {
            walk(topic, &mut out, &mut seen_urls, max_results);
            if out.len() >= max_results {
                return out;
            }
        }
    }
    out
}

fn collect_google_results(
    payload: &serde_json::Value,
    max_results: usize,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut seen_urls = HashSet::new();
    if let Some(items) = payload.get("items").and_then(|v| v.as_array()) {
        for item in items {
            if out.len() >= max_results {
                break;
            }
            let title = item
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let url = item
                .get("link")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let snippet = item
                .get("snippet")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if title.trim().is_empty() || url.trim().is_empty() {
                continue;
            }
            if !seen_urls.insert(url.to_string()) {
                continue;
            }
            out.push(serde_json::json!({
                "title": normalize_text(title),
                "url": url,
                "snippet": normalize_text(snippet),
            }));
        }
    }
    out
}

#[derive(Debug)]
struct WebSearchResponse {
    provider: WebSearchProvider,
    engine_url: String,
    status_code: u16,
    results: Vec<serde_json::Value>,
    abstract_text: Option<String>,
    total_results: Option<u64>,
}

fn execute_duckduckgo_search(
    manifest: &AgentManifest,
    _policy: &PolicyEngine,
    agent_dir: &Path,
    query: &str,
    engine_url: String,
    max_results: usize,
    timeout_secs: u64,
) -> anyhow::Result<WebSearchResponse> {
    let engine_host = extract_host(&engine_url)?;
    enforce_remote_target_for_web(manifest, agent_dir, &engine_host, &engine_url)?;

    let request_engine_url = engine_url.clone();
    let request_query = query.to_string();
    let (status_code, payload) = block_on_http(async move {
        let mut request_url = reqwest::Url::parse(&request_engine_url).map_err(|e| {
            anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
                "Invalid search engine URL '{}': {}",
                request_engine_url,
                e
            )))
        })?;
        {
            let mut pairs = request_url.query_pairs_mut();
            pairs.append_pair("q", request_query.as_str());
            pairs.append_pair("format", "json");
            pairs.append_pair("no_html", "1");
            pairs.append_pair("skip_disambig", "1");
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow::anyhow!("web.search client build failed: {}", e))?;
        let response = client
            .get(request_url)
            .timeout(StdDuration::from_secs(timeout_secs))
            .send()
            .await
            .map_err(|e| {
                anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                    "web.search request failed: {}",
                    e
                )))
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::Error::from(tagged::Tagged::resource(
                anyhow::anyhow!("web.search request failed with status {}", status),
            )));
        }
        let payload = response.json::<serde_json::Value>().await.map_err(|e| {
            anyhow::Error::from(tagged::Tagged::execution(anyhow::anyhow!(
                "web.search could not decode JSON response: {}",
                e
            )))
        })?;
        Ok((status.as_u16(), payload))
    })?;

    let results = collect_duckduckgo_results(&payload, max_results);
    let abstract_text = payload
        .get("AbstractText")
        .and_then(|v| v.as_str())
        .map(normalize_text)
        .filter(|text| !text.is_empty());

    Ok(WebSearchResponse {
        provider: WebSearchProvider::DuckDuckGo,
        engine_url,
        status_code,
        results,
        abstract_text,
        total_results: None,
    })
}

fn execute_google_search(
    manifest: &AgentManifest,
    _policy: &PolicyEngine,
    agent_dir: &Path,
    query: &str,
    engine_url: String,
    api_key: String,
    engine_id: String,
    max_results: usize,
    timeout_secs: u64,
) -> anyhow::Result<WebSearchResponse> {
    let engine_host = extract_host(&engine_url)?;
    enforce_remote_target_for_web(manifest, agent_dir, &engine_host, &engine_url)?;

    let request_engine_url = engine_url.clone();
    let request_query = query.to_string();
    let (status_code, payload) = block_on_http(async move {
        let mut request_url = reqwest::Url::parse(&request_engine_url).map_err(|e| {
            anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
                "Invalid search engine URL '{}': {}",
                request_engine_url,
                e
            )))
        })?;
        {
            let mut pairs = request_url.query_pairs_mut();
            pairs.append_pair("q", request_query.as_str());
            pairs.append_pair("key", api_key.as_str());
            pairs.append_pair("cx", engine_id.as_str());
            pairs.append_pair("num", &max_results.to_string());
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| anyhow::anyhow!("web.search client build failed: {}", e))?;
        let response = client
            .get(request_url)
            .timeout(StdDuration::from_secs(timeout_secs))
            .send()
            .await
            .map_err(|e| {
                anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                    "web.search request failed: {}",
                    e
                )))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::Error::from(tagged::Tagged::resource(
                anyhow::anyhow!("web.search request failed with status {}", status),
            )));
        }
        let payload = response.json::<serde_json::Value>().await.map_err(|e| {
            anyhow::Error::from(tagged::Tagged::execution(anyhow::anyhow!(
                "web.search could not decode JSON response: {}",
                e
            )))
        })?;
        Ok((status.as_u16(), payload))
    })?;

    if let Some(error_payload) = payload.get("error") {
        let message = error_payload
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown google search error");
        return Err(anyhow::Error::from(tagged::Tagged::execution(
            anyhow::anyhow!("web.search google provider returned error: {}", message),
        )));
    }

    let results = collect_google_results(&payload, max_results);
    let total_results = payload
        .pointer("/searchInformation/totalResults")
        .and_then(|v| v.as_str())
        .and_then(|value| value.parse::<u64>().ok());

    Ok(WebSearchResponse {
        provider: WebSearchProvider::Google,
        engine_url,
        status_code,
        results,
        abstract_text: None,
        total_results,
    })
}

fn web_search_response_to_payload(query: &str, response: WebSearchResponse) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "ok": true,
        "provider": response.provider.as_str(),
        "query": query,
        "engine_url": response.engine_url,
        "status_code": response.status_code,
        "result_count": response.results.len(),
        "results": response.results
    });
    if let Some(abstract_text) = response.abstract_text {
        payload["abstract"] = serde_json::json!(abstract_text);
    }
    if let Some(total_results) = response.total_results {
        payload["total_results"] = serde_json::json!(total_results);
    }
    payload
}

pub struct WebSearchTool;

impl NativeTool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::NetworkAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description:
                "Search the web via provider-backed JSON APIs (duckduckgo, google, or auto fallback)."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "provider": { "type": "string", "enum": ["auto", "duckduckgo", "google"] },
                    "max_results": { "type": "integer", "minimum": 1, "maximum": 20 },
                    "timeout_secs": { "type": "integer", "minimum": 5, "maximum": 120 },
                    "engine_url": { "type": "string" },
                    "duckduckgo_engine_url": { "type": "string" },
                    "google_engine_url": { "type": "string" },
                    "google_engine_id": { "type": "string" },
                    "google_api_key_env": { "type": "string" },
                    "google_engine_id_env": { "type": "string" },
                    "cache_ttl_secs": { "type": "integer", "minimum": 0, "maximum": 3600 },
                    "approval_ref": { "type": "string" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: WebSearchArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.query.trim().is_empty(), "query must not be empty");
        let query = args.query.trim().to_string();
        let requested_provider = parse_web_search_provider(args.provider.as_deref())?;
        let timeout_secs = args.timeout_secs.unwrap_or(20).clamp(5, 120);
        let requested_max_results = args.max_results.unwrap_or(5).clamp(1, 20);
        let cache_ttl_secs = resolve_web_search_cache_ttl_secs(&args);
        let cache_key = web_search_cache_key(
            &args,
            requested_provider,
            requested_max_results,
            timeout_secs,
        );

        if cache_ttl_secs > 0 {
            if let Some(mut cached_payload) = web_search_cache_get(&cache_key) {
                cached_payload["cache_hit"] = serde_json::json!(true);
                cached_payload["cache_ttl_secs"] = serde_json::json!(cache_ttl_secs);
                return serde_json::to_string(&cached_payload).map_err(Into::into);
            }
        }

        let approved_host_override: Option<String> = if let (Some(approval_ref), Some(store)) =
            (args.approval_ref.as_deref(), _gateway_store.as_ref())
        {
            let Some(approval) = store.get_approval(approval_ref)? else {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("approval '{}'", approval_ref),
                    Some(
                        "The approval may not exist, may have expired, or may not yet be decided."
                            .to_string(),
                    ),
                )
                .to_error_response());
            };
            validate_approval_ref_context(&approval, manifest, _session_id)?;
            if approval.status != Some(autonoetic_types::background::ApprovalStatus::Approved) {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("approval '{}'", approval_ref),
                    Some(
                        "The approval may not exist, may have expired, or may not yet be decided."
                            .to_string(),
                    ),
                )
                .to_error_response());
            }
            match approval.action {
                ScheduledAction::WebSearch {
                    query: approved_query,
                    provider,
                    max_results,
                    timeout_secs,
                    engine_url,
                    duckduckgo_engine_url,
                    google_engine_url,
                    google_engine_id,
                    google_api_key_env,
                    google_engine_id_env,
                    cache_ttl_secs,
                    detected_hosts,
                    ..
                } => {
                    let args_query = args.query.trim();
                    if approved_query == args_query
                        && provider == args.provider
                        && max_results == args.max_results
                        && timeout_secs == args.timeout_secs
                        && engine_url == args.engine_url
                        && duckduckgo_engine_url == args.duckduckgo_engine_url
                        && google_engine_url == args.google_engine_url
                        && google_engine_id == args.google_engine_id
                        && google_api_key_env == args.google_api_key_env
                        && google_engine_id_env == args.google_engine_id_env
                        && cache_ttl_secs == args.cache_ttl_secs
                    {
                        let host = detected_hosts
                            .as_ref()
                            .and_then(|hosts| hosts.first().cloned());
                        Some(
                            host.or_else(|| {
                                let url = engine_url
                                    .as_ref()
                                    .or(duckduckgo_engine_url.as_ref())
                                    .or(google_engine_url.as_ref())?;
                                extract_host(url).ok()
                            })
                            .ok_or_else(|| {
                                anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
                                    "approval_ref does not specify an engine host"
                                )))
                            })?,
                        )
                    } else {
                        return Ok(autonoetic_types::tool_error::ToolError::validation(
                            "approval_ref does not match this web.search payload",
                            Some(
                                "Ensure all parameters match the original request that created the approval."
                                    .to_string(),
                            ),
                        )
                        .to_error_response());
                    }
                }
                _ => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        format!("approval_ref '{}' is not for web.search", approval_ref),
                        Some(
                            "Use the approval_ref from a web.search approval response.".to_string(),
                        ),
                    )
                    .to_error_response());
                }
            }
        } else {
            None
        };

        let host_allowed = |host: &str| -> bool {
            policy.can_connect_net(host).is_allowed()
                || _gateway_store
                    .as_ref()
                    .is_some_and(|s| session_grants_allow_host(s.as_ref(), _session_id, host))
                || approved_host_override.as_deref() == Some(host)
        };

        let mut maybe_suspend_for_engine = |provider: WebSearchProvider,
                                            engine_url: &str,
                                            max_results: usize|
         -> anyhow::Result<Option<String>> {
            let engine_host = extract_host(engine_url)?;
            enforce_remote_target_for_web(manifest, agent_dir, &engine_host, engine_url)?;

            if host_allowed(&engine_host) {
                return Ok(None);
            }

            let Some(store) = _gateway_store.as_ref() else {
                return Err(anyhow::Error::from(tagged::Tagged::permission(
                    anyhow::anyhow!(
                        "Permission Denied: NetworkAccess does not allow host '{}'",
                        engine_host
                    ),
                )));
            };
            let Some(cfg) = _config else {
                return Err(anyhow::Error::from(tagged::Tagged::permission(
                    anyhow::anyhow!(
                        "Permission Denied: NetworkAccess does not allow host '{}'",
                        engine_host
                    ),
                )));
            };

            let action = ScheduledAction::WebSearch {
                query: query.clone(),
                provider: Some(provider.as_str().to_string()),
                max_results: Some(max_results),
                timeout_secs: Some(timeout_secs),
                engine_url: Some(engine_url.to_string()),
                duckduckgo_engine_url: None,
                google_engine_url: None,
                google_engine_id: args.google_engine_id.clone(),
                google_api_key_env: args.google_api_key_env.clone(),
                google_engine_id_env: args.google_engine_id_env.clone(),
                cache_ttl_secs: args.cache_ttl_secs,
                detected_hosts: Some(vec![engine_host.clone()]),
                payload: Some(serde_json::json!({
                    "host": engine_host.clone(),
                    "retry_field": "approval_ref"
                })),
            };
            let reason = format!("web.search to {} requires approval", engine_host);

            let gate = crate::runtime::human_gate::GateService::new(store.clone());
            let gate_result = gate.check(
                crate::runtime::human_gate::GateRequest {
                    kind: crate::runtime::human_gate::GateKind::Approval {
                        action: action.clone(),
                        targets: vec![engine_host.clone()],
                        match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
                    },
                    manifest,
                    session_id: _session_id,
                    run_context: _run_context,
                    config: Some(cfg),
                    reason: reason.clone(),
                    summary: format!("web.search {}", engine_host),
                    approval_ref: None,
                    pre_validated: false,
                    cache_backfill: None,
                    turn_id: None,
                },
            )?;
            match gate_result {
                crate::runtime::human_gate::GateResult::Cleared { .. } => Ok(None),
                crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
                    Ok(Some(serde_json::json!({
                        "ok": false,
                        "approval_required": true,
                        "approval_already_pending": true,
                        "request_id": gate_id,
                        "suspended": true,
                        "reason": reason,
                        "repair_hint": "Wait for the existing approval to be resolved.",
                        "approval": {
                            "kind": "web_search",
                            "summary": format!("web.search {}", engine_host),
                            "retry_field": "approval_ref"
                        }
                    }).to_string()))
                }
                crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
                    Ok(Some(serde_json::json!({
                        "ok": false,
                        "error_type": "permission",
                        "message": format!(
                            "Execution suspended pending operator approval ({}). Retry web.search with approval_ref after approval.",
                            gate_id
                        ),
                        "repair_hint": "Wait for approval and retry this exact request using approval_ref.",
                        "error": "network_access_denied",
                        "approval_required": true,
                        "request_id": gate_id,
                        "suspended": true,
                        "reason": reason,
                        "approval": {
                            "kind": "web_search",
                            "summary": format!("web.search {}", engine_host),
                            "reason": format!("web.search to {} requires approval", engine_host),
                            "retry_field": "approval_ref"
                        }
                    }).to_string()))
                }
                other => {
                    tracing::warn!(
                        target: "web",
                        gate_result = ?other,
                        "Unexpected gate result for web.search gate"
                    );
                    Ok(None)
                }
            }
        };

        let mut attempted_providers = Vec::new();
        let mut fallback_reason: Option<String> = None;

        let response = match requested_provider {
            WebSearchProvider::DuckDuckGo => {
                attempted_providers.push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                let engine_url = resolve_duckduckgo_engine_url(&args);
                if let Some(suspended) = maybe_suspend_for_engine(
                    WebSearchProvider::DuckDuckGo,
                    &engine_url,
                    requested_max_results.clamp(1, 20),
                )? {
                    return Ok(suspended);
                }
                execute_duckduckgo_search(
                    manifest,
                    policy,
                    agent_dir,
                    &query,
                    engine_url,
                    requested_max_results.clamp(1, 20),
                    timeout_secs,
                )?
            }
            WebSearchProvider::Google => {
                attempted_providers.push(WebSearchProvider::Google.as_str().to_string());
                let engine_url = resolve_google_engine_url(&args);
                if let Some(suspended) = maybe_suspend_for_engine(
                    WebSearchProvider::Google,
                    &engine_url,
                    requested_max_results.clamp(1, 10),
                )? {
                    return Ok(suspended);
                }
                let api_key = resolve_google_api_key(&args)?;
                let engine_id = resolve_google_engine_id(&args)?;
                execute_google_search(
                    manifest,
                    policy,
                    agent_dir,
                    &query,
                    engine_url,
                    api_key,
                    engine_id,
                    requested_max_results.clamp(1, 10),
                    timeout_secs,
                )?
            }
            WebSearchProvider::Auto => {
                let ddg_engine_url = resolve_duckduckgo_engine_url(&args);
                let google_engine_url = resolve_google_engine_url(&args);
                let ddg_max_results = requested_max_results.clamp(1, 20);
                let google_max_results = requested_max_results.clamp(1, 10);

                let google_credentials = resolve_google_api_key(&args).and_then(|api_key| {
                    resolve_google_engine_id(&args).map(|engine_id| (api_key, engine_id))
                });

                match google_credentials {
                    Ok((api_key, engine_id)) => {
                        attempted_providers.push(WebSearchProvider::Google.as_str().to_string());
                        // If google is blocked (remote_access or NetworkAccess), fall back to
                        // duckduckgo without prompting.
                        let google_engine_host = extract_host(&google_engine_url)?;
                        let google_declared = enforce_remote_target_for_web(
                            manifest,
                            agent_dir,
                            &google_engine_host,
                            &google_engine_url,
                        )
                        .is_ok();
                        if !google_declared {
                            fallback_reason = Some(format!(
                                "google provider blocked by remote_access for host {}",
                                google_engine_host
                            ));
                            attempted_providers
                                .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                            if let Some(suspended) = maybe_suspend_for_engine(
                                WebSearchProvider::DuckDuckGo,
                                &ddg_engine_url,
                                ddg_max_results,
                            )? {
                                return Ok(suspended);
                            }
                            execute_duckduckgo_search(
                                manifest,
                                policy,
                                agent_dir,
                                &query,
                                ddg_engine_url,
                                ddg_max_results,
                                timeout_secs,
                            )?
                        } else if !host_allowed(&google_engine_host) {
                            fallback_reason = Some(format!(
                                "google provider blocked by NetworkAccess for host {}",
                                google_engine_host
                            ));
                            attempted_providers
                                .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                            if let Some(suspended) = maybe_suspend_for_engine(
                                WebSearchProvider::DuckDuckGo,
                                &ddg_engine_url,
                                ddg_max_results,
                            )? {
                                return Ok(suspended);
                            }
                            execute_duckduckgo_search(
                                manifest,
                                policy,
                                agent_dir,
                                &query,
                                ddg_engine_url,
                                ddg_max_results,
                                timeout_secs,
                            )?
                        } else {
                            match execute_google_search(
                                manifest,
                                policy,
                                agent_dir,
                                &query,
                                google_engine_url,
                                api_key,
                                engine_id,
                                google_max_results,
                                timeout_secs,
                            ) {
                                Ok(google_response) if !google_response.results.is_empty() => {
                                    google_response
                                }
                                Ok(_) => {
                                    fallback_reason =
                                        Some("google returned no results".to_string());
                                    attempted_providers
                                        .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                                    if let Some(suspended) = maybe_suspend_for_engine(
                                        WebSearchProvider::DuckDuckGo,
                                        &ddg_engine_url,
                                        ddg_max_results,
                                    )? {
                                        return Ok(suspended);
                                    }
                                    execute_duckduckgo_search(
                                        manifest,
                                        policy,
                                        agent_dir,
                                        &query,
                                        ddg_engine_url,
                                        ddg_max_results,
                                        timeout_secs,
                                    )?
                                }
                                Err(google_err) => {
                                    let google_error_text = google_err.to_string();
                                    fallback_reason = Some(format!(
                                        "google provider failed: {google_error_text}"
                                    ));
                                    attempted_providers
                                        .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                                    if let Some(suspended) = maybe_suspend_for_engine(
                                        WebSearchProvider::DuckDuckGo,
                                        &ddg_engine_url,
                                        ddg_max_results,
                                    )? {
                                        return Ok(suspended);
                                    }
                                    match execute_duckduckgo_search(
                                        manifest,
                                        policy,
                                        agent_dir,
                                        &query,
                                        ddg_engine_url,
                                        ddg_max_results,
                                        timeout_secs,
                                    ) {
                                        Ok(ddg_response) => ddg_response,
                                        Err(ddg_err) => {
                                            return Err(anyhow::Error::from(tagged::Tagged::resource(
                                            anyhow::anyhow!(
                                                "web.search auto provider failed: google error: {}; duckduckgo error: {}",
                                                google_error_text,
                                                ddg_err
                                            ),
                                        )));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        fallback_reason =
                            Some("google credentials unavailable; used duckduckgo".to_string());
                        attempted_providers
                            .push(WebSearchProvider::DuckDuckGo.as_str().to_string());
                        if let Some(suspended) = maybe_suspend_for_engine(
                            WebSearchProvider::DuckDuckGo,
                            &ddg_engine_url,
                            ddg_max_results,
                        )? {
                            return Ok(suspended);
                        }
                        execute_duckduckgo_search(
                            manifest,
                            policy,
                            agent_dir,
                            &query,
                            ddg_engine_url,
                            ddg_max_results,
                            timeout_secs,
                        )?
                    }
                }
            }
        };

        let mut payload = web_search_response_to_payload(&query, response);
        payload["requested_provider"] = serde_json::json!(requested_provider.as_str());
        payload["attempted_providers"] = serde_json::json!(attempted_providers);
        if let Some(reason) = fallback_reason {
            payload["fallback_reason"] = serde_json::json!(reason);
        }
        payload["cache_hit"] = serde_json::json!(false);
        payload["cache_ttl_secs"] = serde_json::json!(cache_ttl_secs);

        if cache_ttl_secs > 0 {
            web_search_cache_put(cache_key, payload.clone(), cache_ttl_secs);
        }

        serde_json::to_string(&payload).map_err(Into::into)
    }
}

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_chars: Option<usize>,
    /// Approval request ID from a previous approval-required response.
    #[serde(default)]
    approval_ref: Option<String>,
}

enum WebFetchHostGate {
    Allowed,
    ApprovalPayload(String),
}

enum WebFetchHttpOutcome {
    Success {
        status_code: u16,
        content_type: Option<String>,
        body: String,
        final_url: String,
        redirect_hops: u32,
    },
    NeedsApproval(String),
}

enum WebFetchHop {
    Redirect(String),
    Success {
        status_code: u16,
        content_type: Option<String>,
        body: String,
    },
}

fn gate_web_fetch_host(
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &Path,
    session_id: Option<&str>,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    run_context: Option<&NativeToolRunContext>,
    args: &WebFetchArgs,
    host: &str,
    request_url: &str,
    approval_validated: bool,
    reason: &str,
) -> anyhow::Result<WebFetchHostGate> {
    enforce_remote_target_for_web(manifest, agent_dir, host, request_url)?;

    let host_allowed = policy.can_connect_net(host).is_allowed()
        || gateway_store
            .as_ref()
            .is_some_and(|s| session_grants_allow_host(s.as_ref(), session_id, host))
        || approval_validated;

    if host_allowed {
        return Ok(WebFetchHostGate::Allowed);
    }

    let Some(store) = gateway_store else {
        return Err(anyhow::Error::from(tagged::Tagged::permission(
            anyhow::anyhow!(
                "Permission Denied: NetworkAccess does not allow host '{}'",
                host
            ),
        )));
    };
    let Some(cfg) = config else {
        return Err(anyhow::Error::from(tagged::Tagged::permission(
            anyhow::anyhow!(
                "Permission Denied: NetworkAccess does not allow host '{}'",
                host
            ),
        )));
    };

    let action = ScheduledAction::WebFetch {
        url: request_url.to_string(),
        timeout_secs: args.timeout_secs,
        max_chars: args.max_chars,
        detected_hosts: Some(vec![host.to_string()]),
        payload: Some(serde_json::json!({
            "host": host,
            "retry_field": "approval_ref"
        })),
    };

    let gate = crate::runtime::human_gate::GateService::new(store);
    let gate_result = gate.check(crate::runtime::human_gate::GateRequest {
        kind: crate::runtime::human_gate::GateKind::Approval {
            action,
            targets: vec![host.to_string()],
            match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
        },
        manifest,
        session_id,
        run_context,
        config: Some(cfg),
        reason: reason.to_string(),
        summary: format!("web.fetch {}", host),
        approval_ref: None,
        pre_validated: false,
        cache_backfill: None,
        turn_id: None,
    })?;
    match gate_result {
        crate::runtime::human_gate::GateResult::Cleared { .. } => Ok(WebFetchHostGate::Allowed),
        crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
            Ok(WebFetchHostGate::ApprovalPayload(
                serde_json::json!({
                    "ok": false,
                    "approval_required": true,
                    "approval_already_pending": true,
                    "request_id": gate_id,
                    "suspended": true,
                    "reason": reason,
                    "repair_hint": "Wait for the existing approval to be resolved.",
                    "approval": {
                        "kind": "web_fetch",
                        "summary": format!("web.fetch {}", host),
                        "retry_field": "approval_ref"
                    }
                })
                .to_string(),
            ))
        }
        crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
            Ok(WebFetchHostGate::ApprovalPayload(
                serde_json::json!({
                    "ok": false,
                    "error_type": "permission",
                    "message": format!(
                        "Execution suspended pending operator approval ({}). Retry web.fetch with approval_ref after approval.",
                        gate_id
                    ),
                    "repair_hint": "Wait for approval and retry this exact request using approval_ref.",
                    "error": "network_access_denied",
                    "approval_required": true,
                    "request_id": gate_id,
                    "suspended": true,
                    "reason": reason,
                    "approval": {
                        "kind": "web_fetch",
                        "summary": format!("web.fetch {}", host),
                        "reason": reason,
                        "retry_field": "approval_ref"
                    }
                })
                .to_string(),
            ))
        }
        other => {
            tracing::warn!(
                target: "web",
                gate_result = ?other,
                "Unexpected gate result for web.fetch gate"
            );
            Ok(WebFetchHostGate::Allowed)
        }
    }
}

fn execute_web_fetch_http(
    manifest: &AgentManifest,
    policy: &PolicyEngine,
    agent_dir: &Path,
    session_id: Option<&str>,
    config: Option<&autonoetic_types::config::GatewayConfig>,
    gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    run_context: Option<&NativeToolRunContext>,
    args: &WebFetchArgs,
    approval_validated: bool,
    timeout_secs: u64,
) -> anyhow::Result<WebFetchHttpOutcome> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| anyhow::anyhow!("web.fetch client build failed: {}", e))?;

    let mut current_url = args.url.clone();
    let mut redirect_hops = 0u32;

    loop {
        let host = extract_host(&current_url)?;
        let gate_reason = format!("web.fetch to {} requires approval", host);
        match gate_web_fetch_host(
            manifest,
            policy,
            agent_dir,
            session_id,
            config,
            gateway_store.clone(),
            run_context,
            args,
            &host,
            &current_url,
            approval_validated && current_url == args.url,
            &gate_reason,
        )? {
            WebFetchHostGate::ApprovalPayload(payload) => {
                return Ok(WebFetchHttpOutcome::NeedsApproval(payload));
            }
            WebFetchHostGate::Allowed => {}
        }

        let fetch_url = current_url.clone();
        let hop = block_on_http({
            let client = client.clone();
            async move {
                let response = client
                    .get(&fetch_url)
                    .timeout(StdDuration::from_secs(timeout_secs))
                    .send()
                    .await
                    .map_err(|e| {
                        anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                            "web.fetch request failed: {}",
                            e
                        )))
                    })?;

                let status = response.status();
                if is_redirect_status(status) {
                    let location = response
                        .headers()
                        .get(reqwest::header::LOCATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string())
                        .ok_or_else(|| {
                            anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                                "web.fetch redirect response missing Location header (status {})",
                                status
                            )))
                        })?;
                    return Ok(WebFetchHop::Redirect(location));
                }
                if !status.is_success() {
                    return Err(anyhow::Error::from(tagged::Tagged::resource(
                        anyhow::anyhow!("web.fetch request failed with status {}", status),
                    )));
                }
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.to_string());
                let body = response.text().await.map_err(|e| {
                    anyhow::Error::from(tagged::Tagged::execution(anyhow::anyhow!(
                        "web.fetch could not decode text response: {}",
                        e
                    )))
                })?;
                Ok(WebFetchHop::Success {
                    status_code: status.as_u16(),
                    content_type,
                    body,
                })
            }
        })?;

        match hop {
            WebFetchHop::Redirect(location) => {
                redirect_hops += 1;
                if redirect_hops > MAX_WEB_REDIRECT_HOPS {
                    return Err(anyhow::Error::from(tagged::Tagged::resource(
                        anyhow::anyhow!(
                            "web.fetch exceeded maximum of {} redirects",
                            MAX_WEB_REDIRECT_HOPS
                        ),
                    )));
                }
                let next_url = resolve_redirect_location(&current_url, &location)
                    .map_err(|e| anyhow::Error::from(tagged::Tagged::resource(e)))?;
                if next_url == current_url {
                    return Err(anyhow::Error::from(tagged::Tagged::resource(
                        anyhow::anyhow!(
                            "web.fetch redirect loop detected: {} redirects to itself",
                            current_url
                        ),
                    )));
                }
                let next_host = extract_host(&next_url)?;
                if !hosts_same_redirect_scope(&host, &next_host) {
                    let reason = format!(
                        "web.fetch redirect from {} to {} requires approval (cross-registrable-domain redirect)",
                        host, next_host
                    );
                    match gate_web_fetch_host(
                        manifest,
                        policy,
                        agent_dir,
                        session_id,
                        config,
                        gateway_store.clone(),
                        run_context,
                        args,
                        &next_host,
                        &next_url,
                        false,
                        &reason,
                    )? {
                        WebFetchHostGate::Allowed => {
                            // Cross-domain redirect target is already allowed — follow it.
                        }
                        WebFetchHostGate::ApprovalPayload(payload) => {
                            let mut parsed: serde_json::Value =
                                serde_json::from_str(&payload).unwrap_or(serde_json::json!({}));
                            if let Some(obj) = parsed.as_object_mut() {
                                obj.insert("redirect_cross_domain".into(), serde_json::json!(true));
                                obj.insert("redirect_url".into(), serde_json::json!(next_url));
                            }
                            return Ok(WebFetchHttpOutcome::NeedsApproval(parsed.to_string()));
                        }
                    }
                }
                current_url = next_url;
            }
            WebFetchHop::Success {
                status_code,
                content_type,
                body,
            } => {
                return Ok(WebFetchHttpOutcome::Success {
                    status_code,
                    content_type,
                    body,
                    final_url: current_url,
                    redirect_hops,
                });
            }
        }
    }
}

pub struct WebFetchTool;

impl NativeTool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::NetworkAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Fetch a web page by URL and return its textual payload.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "timeout_secs": { "type": "integer", "minimum": 5, "maximum": 120 },
                    "max_chars": { "type": "integer", "minimum": 512, "maximum": 200000 },
                    "approval_ref": { "type": "string" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: WebFetchArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.url.trim().is_empty(), "url must not be empty");
        let host = extract_host(&args.url)?;
        enforce_remote_target_for_web(manifest, agent_dir, &host, &args.url)?;
        let approval_validated = if let (Some(approval_ref), Some(store)) =
            (args.approval_ref.as_deref(), _gateway_store.as_ref())
        {
            let Some(approval) = store.get_approval(approval_ref)? else {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("approval '{}'", approval_ref),
                    Some(
                        "The approval may not exist, may have expired, or may not yet be decided."
                            .to_string(),
                    ),
                )
                .to_error_response());
            };
            validate_approval_ref_context(&approval, manifest, _session_id)?;
            if approval.status != Some(autonoetic_types::background::ApprovalStatus::Approved) {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("approval '{}'", approval_ref),
                    Some(
                        "The approval may not exist, may have expired, or may not yet be decided."
                            .to_string(),
                    ),
                )
                .to_error_response());
            }
            match approval.action {
                ScheduledAction::WebFetch {
                    url,
                    timeout_secs,
                    max_chars,
                    ..
                } => {
                    if url == args.url
                        && timeout_secs == args.timeout_secs
                        && max_chars == args.max_chars
                    {
                        true
                    } else {
                        return Ok(autonoetic_types::tool_error::ToolError::validation(
                            "approval_ref does not match this web.fetch payload",
                            Some(
                                "Ensure all parameters match the original request that created the approval."
                                    .to_string(),
                            ),
                        )
                        .to_error_response());
                    }
                }
                _ => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        format!("approval_ref '{}' is not for web.fetch", approval_ref),
                        Some(
                            "Use the approval_ref from a web.fetch approval response.".to_string(),
                        ),
                    )
                    .to_error_response());
                }
            }
        } else {
            false
        };

        let timeout_secs = args.timeout_secs.unwrap_or(20).clamp(5, 120);
        let max_chars = args.max_chars.unwrap_or(20_000).clamp(512, 200_000);

        match execute_web_fetch_http(
            manifest,
            policy,
            agent_dir,
            _session_id,
            _config,
            _gateway_store,
            _run_context,
            &args,
            approval_validated,
            timeout_secs,
        )? {
            WebFetchHttpOutcome::NeedsApproval(payload) => Ok(payload),
            WebFetchHttpOutcome::Success {
                status_code,
                content_type,
                body,
                final_url,
                redirect_hops,
            } => {
                let total_chars = body.chars().count();
                let truncated = total_chars > max_chars;
                let content = if truncated {
                    body.chars().take(max_chars).collect::<String>()
                } else {
                    body
                };

                serde_json::to_string(&serde_json::json!({
                    "ok": true,
                    "url": args.url,
                    "final_url": final_url,
                    "redirect_hops": redirect_hops,
                    "status_code": status_code,
                    "content_type": content_type,
                    "truncated": truncated,
                    "total_chars": total_chars,
                    "content": content
                }))
                .map_err(Into::into)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// web.call — arbitrary HTTP method with optional headers and JSON body
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebCallArgs {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    #[serde(default)]
    body: Option<serde_json::Value>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_chars: Option<usize>,
    /// Approval request ID from a previous approval-required response.
    #[serde(default)]
    approval_ref: Option<String>,
}

pub struct WebCallTool;

impl NativeTool for WebCallTool {
    fn name(&self) -> &'static str {
        "web_call"
    }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest
            .capabilities
            .iter()
            .any(|cap| matches!(cap, Capability::NetworkAccess { .. }))
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Make an HTTP request (GET, POST, PUT, PATCH, DELETE) with optional \
                headers and JSON body. Use this for service registration, REST API calls, and \
                webhook endpoints. The response body is returned as-is (JSON-parsed when \
                possible). Note: secrets returned in responses are visible in the LLM context — \
                use credential.setup for flows that must keep secrets server-side."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Full URL for the request"
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                        "description": "HTTP method (default: GET)"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional HTTP headers as key-value pairs (e.g. {\"Authorization\": \"Bearer sk_...\"})",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "object",
                        "description": "Optional JSON body for POST/PUT/PATCH requests"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 5,
                        "maximum": 120
                    },
                    "max_chars": {
                        "type": "integer",
                        "minimum": 512,
                        "maximum": 200000
                    },
                    "approval_ref": {
                        "type": "string",
                        "description": "Approval request ID from a previous approval-required response."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(
        &self,
        manifest: &AgentManifest,
        policy: &PolicyEngine,
        agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        let args: WebCallArgs = serde_json::from_str(arguments_json)
            .map_err(|e| anyhow::anyhow!("Invalid JSON arguments for '{}': {}", self.name(), e))?;

        anyhow::ensure!(!args.url.trim().is_empty(), "url must not be empty");
        let host = extract_host(&args.url)?;
        enforce_remote_target_for_web(manifest, agent_dir, &host, &args.url)?;
        let approval_validated = if let (Some(approval_ref), Some(store)) =
            (args.approval_ref.as_deref(), _gateway_store.as_ref())
        {
            let Some(approval) = store.get_approval(approval_ref)? else {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("approval '{}'", approval_ref),
                    Some(
                        "The approval may not exist, may have expired, or may not yet be decided."
                            .to_string(),
                    ),
                )
                .to_error_response());
            };
            validate_approval_ref_context(&approval, manifest, _session_id)?;
            if approval.status != Some(autonoetic_types::background::ApprovalStatus::Approved) {
                return Ok(autonoetic_types::tool_error::ToolError::not_found(
                    format!("approval '{}'", approval_ref),
                    Some(
                        "The approval may not exist, may have expired, or may not yet be decided."
                            .to_string(),
                    ),
                )
                .to_error_response());
            }
            match approval.action {
                ScheduledAction::WebCall {
                    url,
                    method,
                    headers,
                    body,
                    timeout_secs,
                    max_chars,
                    ..
                } => {
                    if url == args.url
                        && method == args.method
                        && headers == args.headers
                        && body == args.body
                        && timeout_secs == args.timeout_secs
                        && max_chars == args.max_chars
                    {
                        true
                    } else {
                        return Ok(autonoetic_types::tool_error::ToolError::validation(
                            "approval_ref does not match this web.call payload",
                            Some(
                                "Ensure all parameters match the original request that created the approval."
                                    .to_string(),
                            ),
                        )
                        .to_error_response());
                    }
                }
                _ => {
                    return Ok(autonoetic_types::tool_error::ToolError::validation(
                        format!("approval_ref '{}' is not for web.call", approval_ref),
                        Some("Use the approval_ref from a web.call approval response.".to_string()),
                    )
                    .to_error_response());
                }
            }
        } else {
            false
        };

        let host_allowed = policy.can_connect_net(&host).is_allowed()
            || _gateway_store
                .as_ref()
                .is_some_and(|s| session_grants_allow_host(s.as_ref(), _session_id, &host))
            || approval_validated;

        if !host_allowed {
            let Some(store) = _gateway_store else {
                return Err(anyhow::Error::from(tagged::Tagged::permission(
                    anyhow::anyhow!(
                        "Permission Denied: NetworkAccess does not allow host '{}'",
                        host
                    ),
                )));
            };
            let Some(cfg) = _config else {
                return Err(anyhow::Error::from(tagged::Tagged::permission(
                    anyhow::anyhow!(
                        "Permission Denied: NetworkAccess does not allow host '{}'",
                        host
                    ),
                )));
            };

            let action = ScheduledAction::WebCall {
                url: args.url.clone(),
                method: args.method.clone(),
                headers: args.headers.clone(),
                body: args.body.clone(),
                timeout_secs: args.timeout_secs,
                max_chars: args.max_chars,
                detected_hosts: Some(vec![host.clone()]),
                payload: Some(serde_json::json!({
                    "host": host.clone(),
                    "retry_field": "approval_ref"
                })),
            };
            let reason = format!("web.call to {} requires approval", host);

            let gate = crate::runtime::human_gate::GateService::new(store);
            let gate_result = gate.check(
                crate::runtime::human_gate::GateRequest {
                    kind: crate::runtime::human_gate::GateKind::Approval {
                        action: action.clone(),
                        targets: vec![host.clone()],
                        match_strategy: crate::runtime::human_gate::MatchStrategy::ExactPayload,
                    },
                    manifest,
                    session_id: _session_id,
                    run_context: _run_context,
                    config: Some(cfg),
                    reason: reason.clone(),
                    summary: format!("web.call {}", host),
                    approval_ref: None,
                    pre_validated: false,
                    cache_backfill: None,
                    turn_id: None,
                },
            )?;
            match gate_result {
                crate::runtime::human_gate::GateResult::Cleared { .. } => {}
                crate::runtime::human_gate::GateResult::AlreadyPending { gate_id, .. } => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "approval_required": true,
                        "approval_already_pending": true,
                        "request_id": gate_id,
                        "suspended": true,
                        "reason": reason,
                        "repair_hint": "Wait for the existing approval to be resolved.",
                        "approval": {
                            "kind": "web_call",
                            "summary": format!("web.call {}", host),
                            "retry_field": "approval_ref"
                        }
                    }).to_string());
                }
                crate::runtime::human_gate::GateResult::Suspended { gate_id, .. } => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error_type": "permission",
                        "message": format!(
                            "Execution suspended pending operator approval ({}). Retry web.call with approval_ref after approval.",
                            gate_id
                        ),
                        "repair_hint": "Wait for approval and retry this exact request using approval_ref.",
                        "error": "network_access_denied",
                        "approval_required": true,
                        "request_id": gate_id,
                        "suspended": true,
                        "reason": reason,
                        "approval": {
                            "kind": "web_call",
                            "summary": format!("web.call {}", host),
                            "reason": format!("web.call to {} requires approval", host),
                            "retry_field": "approval_ref"
                        }
                    }).to_string());
                }
                other => {
                    tracing::warn!(
                        target: "web",
                        gate_result = ?other,
                        "Unexpected gate result for web.call gate"
                    );
                }
            }
        }

        let method = args
            .method
            .as_deref()
            .unwrap_or("GET")
            .trim()
            .to_uppercase();
        let timeout_secs = args.timeout_secs.unwrap_or(20).clamp(5, 120);
        let max_chars = args.max_chars.unwrap_or(20_000).clamp(512, 200_000);

        let fetch_url = args.url.clone();
        let method_bytes = method.clone();
        let headers = args.headers.clone().unwrap_or_default();
        let body = args.body.clone();

        let (status_code, content_type, response_text) = block_on_http(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| anyhow::anyhow!("web.call client build failed: {}", e))?;

            let http_method =
                reqwest::Method::from_bytes(method_bytes.as_bytes()).map_err(|e| {
                    anyhow::Error::from(tagged::Tagged::validation(anyhow::anyhow!(
                        "Invalid HTTP method '{}': {}",
                        method_bytes,
                        e
                    )))
                })?;

            let mut req = client
                .request(http_method, &fetch_url)
                .timeout(StdDuration::from_secs(timeout_secs));

            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }

            if let Some(ref b) = body {
                req = req.json(b);
            }

            let response = req.send().await.map_err(|e| {
                anyhow::Error::from(tagged::Tagged::resource(anyhow::anyhow!(
                    "web.call request failed: {}",
                    e
                )))
            })?;

            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_string());
            let text = response.text().await.map_err(|e| {
                anyhow::Error::from(tagged::Tagged::execution(anyhow::anyhow!(
                    "web.call could not decode response: {}",
                    e
                )))
            })?;
            Ok((status, content_type, text))
        })?;

        let total_chars = response_text.chars().count();
        let truncated = total_chars > max_chars;
        let content = if truncated {
            response_text.chars().take(max_chars).collect::<String>()
        } else {
            response_text
        };

        // Try to parse response as JSON for cleaner output; fall back to plain string.
        let body_value: serde_json::Value =
            serde_json::from_str(&content).unwrap_or(serde_json::Value::String(content));

        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "url": args.url,
            "method": method,
            "status_code": status_code,
            "content_type": content_type,
            "truncated": truncated,
            "body": body_value,
        }))
        .map_err(Into::into)
    }
}
