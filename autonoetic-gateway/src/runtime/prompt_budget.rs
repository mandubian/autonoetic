//! Prompt Budget Transparency.
//!
//! Provides observability into what consumes the context window budget
//! before each LLM call, and tool tiering for dynamic tool filtering.

use crate::llm::{Message, Role, ToolCall, ToolDefinition};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};

/// Options controlling how `sanitize_history_for_request` transforms the
/// stored conversation history before it is sent to the LLM.
///
/// Storage (checkpoints, exports, timeline) keeps the original messages;
/// sanitization only affects the wire-format copy used for the
/// `CompletionRequest`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistorySanitizeOptions {
    /// When true, strip `reasoning_content` and `reasoning_details` from
    /// assistant messages. The model does not need to re-read its own
    /// chain-of-thought on subsequent turns.
    pub strip_reasoning: bool,
    /// Maximum characters to allow in a tool-result message content. Values
    /// <= 0 disable truncation. When truncation is enabled, results are
    /// reduced **structurally**: if the content is valid JSON, large string
    /// *values* are head+tail truncated in-place so the JSON structure and
    /// all small metadata fields (e.g. `ok`, `offset`, `total_bytes`,
    /// `next_offset`, `truncated`, `error_type`) remain intact and parseable.
    /// Non-JSON results fall back to a whole-string head+tail truncation.
    pub max_tool_result_chars: usize,
    /// When true, duplicate `Role::Tool` messages are collapsed: the first
    /// occurrence keeps its content, and later duplicates are replaced with a
    /// short marker. The matching `tool_call_id` is preserved so the
    /// assistant/tool pairing required by providers remains valid.
    pub dedup_tool_results: bool,
    /// When true, `Role::Tool` messages that carry the *same normalized error*
    /// (issue #705) — even non-consecutively and with different volatile ids —
    /// are collapsed to a short marker, keeping only the most recent occurrence
    /// in full. Unlike `dedup_tool_results` (byte-identical, consecutive), this
    /// uses the error fingerprint so a death-spiral's repeated failure context
    /// is not re-sent in full on every round.
    pub collapse_repeated_errors: bool,
}

impl Default for HistorySanitizeOptions {
    fn default() -> Self {
        Self {
            // Default to false: reasoning_content / reasoning_details must be
            // replayed for many thinking/reasoning models (DeepSeek, OpenRouter
            // reasoning models, etc.). Operators whose model does not require
            // replay can opt in to stripping.
            strip_reasoning: false,
            // Default: 4000 chars. Large enough for a meaningful chunk of a
            // file (the resolve tool pages in `limit`-sized byte chunks) plus
            // short stdout/stderr from sandbox.exec. JSON-aware truncation
            // (see `truncate_tool_result_json`) preserves structure + metadata
            // even when the budget is tight, so pagination fields like
            // `next_offset` and `total_bytes` survive.
            max_tool_result_chars: 4000,
            // Most repeated tool reads are uninformative after the first
            // occurrence; collapsing them saves tokens without losing storage.
            dedup_tool_results: true,
            // A recurring error (death spiral) re-sends the same failure context
            // every round; collapse all but the latest occurrence (#705).
            collapse_repeated_errors: true,
        }
    }
}

/// Create a wire-format copy of `history` with tokens reduced but information
/// preserved in storage.
///
/// - Assistant `reasoning_content` / `reasoning_details` are stripped.
/// - Tool-result `content` over `max_tool_result_chars` is truncated. JSON
///   results have their large string *values* shortened in-place (preserving
///   structure + metadata like `next_offset`, `total_bytes`); non-JSON results
///   get a whole-string head+tail ellipsis so status/summary remains visible.
/// - Duplicate tool-result messages are collapsed to a short marker after the
///   first occurrence.
/// - System, user, and assistant messages are otherwise untouched.
pub fn sanitize_history_for_request(
    history: &[Message],
    opts: &HistorySanitizeOptions,
) -> Vec<Message> {
    if !opts.strip_reasoning
        && opts.max_tool_result_chars == 0
        && !opts.dedup_tool_results
        && !opts.collapse_repeated_errors
    {
        return history.to_vec();
    }

    let mut sanitized: Vec<Message> = history
        .iter()
        .map(|msg| {
            let mut m = msg.clone();

            if opts.strip_reasoning {
                m.reasoning_content = None;
                m.reasoning_details = None;
            }

            if opts.max_tool_result_chars > 0 && m.role == Role::Tool && !m.content.is_empty() {
                // Safety-net truncation only — tool results are already
                // JSON-aware truncated at push time (see
                // `handle_tool_batch`). This catches any legacy/untruncated
                // results with a fast string operation.
                m.content = truncate_middle(&m.content, opts.max_tool_result_chars);
            }

            m
        })
        .collect();

    if opts.collapse_repeated_errors {
        // Fingerprint from the ORIGINAL content (truncation above can split the
        // JSON so it no longer parses). `sanitized` is 1:1 with `history` by
        // index, so decisions map directly.
        collapse_repeated_error_results(&mut sanitized, history);
    }

    if opts.dedup_tool_results {
        dedup_duplicate_tool_results(&mut sanitized);
    }

    sanitized
}

/// Collapse `Role::Tool` messages carrying the same normalized error fingerprint
/// (issue #705). The most recent occurrence of each recurring error is kept in
/// full; earlier ones are replaced with a short marker (keeping `tool_call_id`
/// so provider assistant/tool pairing stays valid). `originals` supplies the
/// pre-truncation content used to compute fingerprints.
fn collapse_repeated_error_results(sanitized: &mut [Message], originals: &[Message]) {
    use std::collections::HashMap;

    // fingerprint -> (occurrence count, index of last occurrence)
    let mut stats: HashMap<u64, (u32, usize)> = HashMap::new();

    // Compute fingerprints once per original tool message, keyed by index, so
    // the collapse pass below does not re-parse the same JSON.
    let mut fingerprints: Vec<Option<u64>> = Vec::with_capacity(originals.len());
    for (i, msg) in originals.iter().enumerate() {
        if msg.role != Role::Tool || msg.content.is_empty() {
            fingerprints.push(None);
            continue;
        }
        let fp = crate::runtime::error_fingerprint::fingerprint_result(&msg.content);
        if let Some(h) = fp {
            let entry = stats.entry(h).or_insert((0, i));
            entry.0 += 1;
            entry.1 = i;
        }
        fingerprints.push(fp);
    }

    for (i, msg) in sanitized.iter_mut().enumerate() {
        if msg.role != Role::Tool || msg.content.is_empty() {
            continue;
        }
        let Some(fp) = fingerprints.get(i).copied().flatten() else {
            continue;
        };
        if let Some(&(count, last_idx)) = stats.get(&fp) {
            // Collapse only recurring errors, and never the latest occurrence.
            if count >= 2 && i != last_idx {
                msg.content =
                    "[repeated error — same root cause as a later tool result; collapsed to save context]"
                        .to_string();
            }
        }
    }
}

/// Replace duplicate `Role::Tool` message contents with a short marker. The
/// first occurrence in the history (or after a different tool result) is
/// preserved; later duplicates keep their `tool_call_id` so provider
/// assistant/tool pairing remains valid.
fn dedup_duplicate_tool_results(messages: &mut [Message]) {
    let mut last_tool_content: Option<String> = None;
    for msg in messages.iter_mut() {
        if msg.role == Role::Tool && !msg.content.is_empty() {
            if last_tool_content.as_deref() == Some(msg.content.as_str()) {
                msg.content = "[duplicate result — see above]".to_string();
            } else {
                last_tool_content = Some(msg.content.clone());
            }
        }
    }
}

/// Truncate a tool-result string to fit within `max_chars` chars.
///
/// If the content parses as JSON, large string *values* are truncated
/// in-place so the JSON structure and all small metadata fields remain
/// intact and parseable by the agent. This is critical for pagination: the
/// agent can still read `next_offset`, `total_bytes`, `truncated`, etc. even
/// when the `content` / `stdout` / `result` field is shortened.
///
/// If the JSON-aware pass still leaves the result over budget (many medium
/// fields), or the content is not JSON, fall back to whole-string
/// `truncate_middle`.
///
/// **Call this once at push time** (when the tool result enters history), not
/// on every turn. Re-parsing every tool result as JSON on each LLM round is
/// O(M²) in the number of tool results.
pub(crate) fn truncate_tool_result(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(s) {
        let was_truncated = {
            let total_string_chars: usize = collect_string_values(&value)
                .iter()
                .map(|s| s.chars().count())
                .sum();
            total_string_chars > max_chars
        };
        truncate_json_strings_in_place(&mut value, max_chars);
        if was_truncated {
            mark_json_truncated(&mut value);
        }
        let serialized = serde_json::to_string(&value).unwrap_or_else(|_| s.to_string());
        if serialized.chars().count() <= max_chars {
            return serialized;
        }
        // Many medium fields pushed the total over even after per-field
        // truncation — fall through to whole-string truncation.
        return truncate_middle(&serialized, max_chars);
    }
    truncate_middle(s, max_chars)
}

/// After truncating string values inside a JSON tool result, patch the
/// metadata fields that agents use for paging decisions. Without this,
/// `resolve` returns `{"truncated": false, "next_offset": null}` even
/// though the `content` field was silently shortened — leaving the agent
/// with no valid paging handle and causing it to re-read the same content.
fn mark_json_truncated(value: &mut serde_json::Value) {
    if let Some(obj) = value.as_object_mut() {
        if obj.contains_key("truncated") {
            obj.insert("truncated".to_string(), serde_json::Value::Bool(true));
        }
        if obj.contains_key("next_offset") {
            // Signal that content was cut — the agent should re-resolve
            // with an explicit limit/offset to get the full content.
            if obj.get("next_offset").map_or(false, |v| v.is_null()) {
                obj.insert("next_offset".to_string(), serde_json::Value::Number(
                    serde_json::Number::from(-1),
                ));
            }
        }
    }
}

/// Walk a JSON tree and truncate any string value longer than its fair share
/// of `max_chars`. Non-string leaves and structural overhead (keys, braces,
/// commas) are preserved untouched — only oversized string *values* are
/// shortened with head+tail + marker.
///
/// The budget is split evenly across all string values, after reserving
/// overhead for the JSON structure itself.
fn truncate_json_strings_in_place(value: &mut serde_json::Value, max_chars: usize) {
    let total_string_chars: usize = collect_string_values(value)
        .iter()
        .map(|s| s.chars().count())
        .sum();
    if total_string_chars <= max_chars {
        return;
    }

    // Reserve space for JSON structural overhead (keys, braces, commas,
    // quotes, colons). A rough estimate: 40% of the budget goes to structure,
    // capped so we never starve the string budget below ~25%.
    let struct_overhead = (max_chars * 2 / 5).min(max_chars * 3 / 4);
    let string_budget = max_chars.saturating_sub(struct_overhead);
    let string_count = count_string_values(value).max(1);
    let per_field_budget = string_budget / string_count;

    truncate_json_strings_iterative(value, per_field_budget);
}

/// Top-level JSON keys whose string values carry agent-routing directives and
/// must survive tool-result truncation verbatim. When a result has many large
/// fields (e.g. a long `undeclared_patterns` list), the per-field budget
/// shrinks and `repair_hint`/`available_actions` used to get middle-truncated
/// — destroying the one instruction telling the agent how to route the fix
/// (manifest vs code). These fields are small by design, so exempting them
/// costs little budget.
const TRUNCATION_EXEMPT_KEYS: &[&str] = &[
    "error_type",
    "error_class",
    "fix_target",
    "repair_hint",
    "repair_class",
    "available_actions",
    "enforced_rules",
];

fn truncate_json_strings_iterative(value: &mut serde_json::Value, per_field_budget: usize) {
    // `exempt` propagates down subtrees rooted at an exempt top-level key
    // (e.g. everything under `available_actions`).
    let mut stack: Vec<(&mut serde_json::Value, bool)> = vec![(value, false)];
    while let Some((v, exempt)) = stack.pop() {
        match v {
            serde_json::Value::String(s) => {
                if !exempt && s.chars().count() > per_field_budget {
                    *s = truncate_middle(s, per_field_budget);
                }
            }
            serde_json::Value::Object(map) => {
                stack.extend(map.iter_mut().map(|(k, v)| {
                    let child_exempt = exempt || TRUNCATION_EXEMPT_KEYS.contains(&k.as_str());
                    (v, child_exempt)
                }));
            }
            serde_json::Value::Array(arr) => {
                stack.extend(arr.iter_mut().map(|v| (v, exempt)));
            }
            _ => {}
        }
    }
}

fn collect_string_values(value: &serde_json::Value) -> Vec<&str> {
    let mut out = Vec::new();
    let mut stack: Vec<&serde_json::Value> = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            serde_json::Value::String(s) => out.push(s.as_str()),
            serde_json::Value::Object(map) => {
                stack.extend(map.values());
            }
            serde_json::Value::Array(arr) => {
                stack.extend(arr.iter());
            }
            _ => {}
        }
    }
    out
}

fn count_string_values(value: &serde_json::Value) -> usize {
    let mut count = 0;
    let mut stack: Vec<&serde_json::Value> = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            serde_json::Value::String(_) => count += 1,
            serde_json::Value::Object(map) => {
                stack.extend(map.values());
            }
            serde_json::Value::Array(arr) => {
                stack.extend(arr.iter());
            }
            _ => {}
        }
    }
    count
}

fn truncate_middle(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }

    // Keep head and tail; each gets roughly half the budget.
    let keep_each = max_chars.saturating_sub(30) / 2;
    if keep_each == 0 {
        return s.chars().take(max_chars).collect();
    }

    let head: String = s.chars().take(keep_each).collect();
    let tail: String = s.chars().skip(len.saturating_sub(keep_each)).collect();
    format!("{}\n[... {} chars truncated ...]\n{}", head, len - (keep_each * 2), tail)
}

/// Default chars-per-token ratio used when no override is configured.
///
/// 3.0 is intentionally conservative: real-world tokenizers (Qwen3, Llama3,
/// GPT-4o) typically achieve 2.2–3.5 chars per token, with code/JSON content
/// on the lower end. The previous default of 4.0 matched English prose under
/// the GPT-2 BPE and materially underestimated the prompt size of code-heavy
/// or mixed-format content, which let the context governor stay silent until
/// the LLM call returned a 400.
///
/// Operators running a model whose tokenizer is known to produce more tokens
/// per character (or vice versa) can override this with
/// `prompt_budget.chars_per_token` in the gateway config.
pub const DEFAULT_CHARS_PER_TOKEN: f64 = 3.0;

/// Hard sanity bound on the configurable ratio. A value below 0.5 chars/token
/// would imply more than 2 tokens per character (unrealistic for any
/// commercial tokenizer), and a value above 16 would correspond to ~0.06
/// tokens/char (single-token sentences). Both are rejected at the setter.
const MIN_CHARS_PER_TOKEN: f64 = 0.5;
const MAX_CHARS_PER_TOKEN: f64 = 16.0;

/// Effective chars-per-token ratio, stored as centi-units (multiply by 100)
/// so the value can live in an `AtomicU32` instead of requiring
/// platform-specific `AtomicU64`/`AtomicF64` plumbing. Default = 3.00.
static CHARS_PER_TOKEN_CENTIS: AtomicU32 = AtomicU32::new(300);

/// Return the current effective chars-per-token ratio. Reads from a process-
/// wide atomic; safe to call from any thread.
pub fn chars_per_token() -> f64 {
    (CHARS_PER_TOKEN_CENTIS.load(Ordering::Relaxed) as f64) / 100.0
}

/// Override the chars-per-token ratio at runtime. Returns the clamped value
/// that was actually stored (i.e. the input after bounding to
/// `[MIN_CHARS_PER_TOKEN, MAX_CHARS_PER_TOKEN]`). Callers should pass the
/// returned value to `tracing` so operators can see when clamping occurred.
///
/// Setting the value to a non-finite or non-positive number resets the
/// atomic to the default — this is the safe fallback for malformed config.
pub fn set_chars_per_token(value: f64) -> f64 {
    let stored = if value.is_finite() && value > 0.0 {
        let clamped = value.clamp(MIN_CHARS_PER_TOKEN, MAX_CHARS_PER_TOKEN);
        CHARS_PER_TOKEN_CENTIS.store(
            (clamped * 100.0).round() as u32,
            Ordering::Relaxed,
        );
        clamped
    } else {
        // Malformed config: revert to default and report what we did.
        CHARS_PER_TOKEN_CENTIS.store(
            (DEFAULT_CHARS_PER_TOKEN * 100.0).round() as u32,
            Ordering::Relaxed,
        );
        DEFAULT_CHARS_PER_TOKEN
    };
    stored
}

/// Estimated overhead tokens per tool definition (name + description + schema structure).
const TOOL_OVERHEAD_TOKENS: usize = 30;
const TOOL_CALL_OVERHEAD_TOKENS: usize = 15;

/// Conservative fallback context window (in tokens) used when the model's
/// real context window cannot be determined. This is intentionally small to
/// avoid sending prompts that exceed a small model's actual limit. However,
/// if the system prompt + tool definitions alone exceed this, the context
/// governor will fail on every turn — the caller should emit a warning and
/// point the user at `context_window_tokens` configuration.
pub const FALLBACK_CONTEXT_WINDOW: usize = 32_768;

/// Tool tier for progressive disclosure.
pub type ToolTier = autonoetic_types::agent::ToolTier;

/// Breakdown of the prompt budget before an LLM call.
#[derive(Debug, Clone, Serialize)]
pub struct PromptBudgetBreakdown {
    /// Tokens in the system prompt (foundation + agent instructions).
    pub system_prompt_tokens: usize,
    /// Tokens in the conversation history (all messages except system).
    pub conversation_tokens: usize,
    /// Number of tool definitions included.
    pub tool_count: usize,
    /// Estimated tokens for all tool definitions.
    pub tool_definition_tokens: usize,
    /// Total estimated prompt tokens.
    pub total_tokens: usize,
    /// Context window size for the target model (if known).
    pub context_window: Option<usize>,
    /// Utilization percentage of the context window (0-100).
    pub utilization_pct: Option<f64>,
}

impl PromptBudgetBreakdown {
    /// Compute a budget breakdown from the assembled request components.
    pub fn compute(
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        context_window: Option<usize>,
    ) -> Self {
        let system_prompt_tokens = estimate_tokens(system_prompt);

        let conversation_tokens: usize = messages
            .iter()
            .filter(|m| m.role != crate::llm::Role::System)
            .map(|m| estimate_message_tokens(m))
            .sum();

        let tool_count = tools.len();
        let tool_definition_tokens: usize = tools.iter().map(|t| estimate_tool_definition(t)).sum();

        let total_tokens = system_prompt_tokens + conversation_tokens + tool_definition_tokens;

        let utilization_pct = context_window.map(|cw| {
            if cw == 0 {
                0.0
            } else {
                (total_tokens as f64 / cw as f64) * 100.0
            }
        });

        Self {
            system_prompt_tokens,
            conversation_tokens,
            tool_count,
            tool_definition_tokens,
            total_tokens,
            context_window,
            utilization_pct,
        }
    }
}

/// Estimate tokens from a text string using the current chars-per-token
/// ratio (see [`chars_per_token`] / [`set_chars_per_token`]).
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        let ratio = chars_per_token();
        (text.chars().count() as f64 / ratio).ceil() as usize
    }
}

/// Estimate tokens for a single message, including `content`, `tool_calls`
/// (name + JSON arguments), and `reasoning_content`. Prior versions only
/// counted `content`, which made the governor systematically underestimate
/// assistant turns that wrote large files via `content_write` (empty content,
/// thousands of tokens in `tool_calls[].arguments`). That blind spot let
/// oversized prompts through to providers, causing context-overflow 500s
/// and parse failures in llama.cpp's tool-call renderer.
pub fn estimate_message_tokens(msg: &Message) -> usize {
    let mut tokens = estimate_tokens(&msg.content);
    for tc in &msg.tool_calls {
        tokens += estimate_tokens(&tc.name);
        tokens += estimate_tokens(&tc.arguments);
        tokens += TOOL_CALL_OVERHEAD_TOKENS;
    }
    if let Some(ref reasoning) = msg.reasoning_content {
        tokens += estimate_tokens(reasoning);
    }
    tokens
}

/// Estimate tokens for a single tool definition.
pub fn estimate_tool_definition(tool: &ToolDefinition) -> usize {
    let name_tokens = estimate_tokens(&tool.name);
    let desc_tokens = estimate_tokens(&tool.description);
    let schema_tokens =
        estimate_tokens(&serde_json::to_string(&tool.input_schema).unwrap_or_default());
    name_tokens + desc_tokens + schema_tokens + TOOL_OVERHEAD_TOKENS
}

/// Get the tier for a tool using the declarative registry.
pub fn tool_tier(tool_name: &str) -> ToolTier {
    crate::runtime::tool_tier_registry::tool_tier(tool_name)
}

/// Filter tool definitions by tier.
pub fn filter_tools_by_tier(
    tools: Vec<ToolDefinition>,
    allowed_tiers: &[ToolTier],
) -> Vec<ToolDefinition> {
    if allowed_tiers.is_empty() {
        return tools;
    }
    tools
        .into_iter()
        .filter(|t| allowed_tiers.contains(&tool_tier(&t.name)))
        .collect()
}

/// Whether `tool_name` matches a `tool_discover` pattern (exact name or `prefix*`).
pub fn tool_matches_discovered_pattern(tool_name: &str, pattern: &str) -> bool {
    let p = pattern.trim();
    if p.is_empty() {
        return false;
    }
    if let Some(prefix) = p.strip_suffix('*') {
        tool_name.starts_with(prefix)
    } else {
        tool_name == p
    }
}

/// Cap tool count for the LLM request, dropping lowest-priority tiers first.
///
/// Tools explicitly matched by `discovered_patterns` (from `tool_discover`) are
/// never dropped — the agent asked for them by name.
pub fn cap_tool_definitions_preserving_discovered(
    tools: Vec<ToolDefinition>,
    max_tools: usize,
    discovered_patterns: &HashSet<String>,
) -> Vec<ToolDefinition> {
    if max_tools == 0 || tools.len() <= max_tools {
        return tools;
    }

    let (mut pinned, mut rest): (Vec<ToolDefinition>, Vec<ToolDefinition>) = tools
        .into_iter()
        .partition(|def| {
            discovered_patterns
                .iter()
                .any(|pattern| tool_matches_discovered_pattern(&def.name, pattern))
        });

    let budget = max_tools.saturating_sub(pinned.len());
    if rest.len() > budget {
        let tier_order = |tier: &ToolTier| match tier {
            ToolTier::Core => 0,
            ToolTier::Workflow => 1,
            ToolTier::Specialized => 2,
        };
        rest.sort_by(|a, b| {
            tier_order(&tool_tier(&a.name)).cmp(&tier_order(&tool_tier(&b.name)))
        });
        rest.truncate(budget);
    }

    rest.append(&mut pinned);
    rest
}

/// Result of applying a prompt budget enforcement strategy.
#[derive(Debug)]
pub struct EnforcementResult {
    pub tools: Vec<ToolDefinition>,
    pub history: Vec<Message>,
    pub was_trimmed: bool,
}

/// Strategy for enforcing prompt budget limits when the total exceeds the effective limit.
pub trait BudgetEnforcementStrategy: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn enforce(
        &self,
        tools: Vec<ToolDefinition>,
        history: Vec<Message>,
        breakdown: &PromptBudgetBreakdown,
        effective_limit: usize,
        budget_config: &autonoetic_types::config::PromptBudgetConfig,
    ) -> anyhow::Result<EnforcementResult>;
}

fn check_section_caps(
    breakdown: &PromptBudgetBreakdown,
    budget_config: &autonoetic_types::config::PromptBudgetConfig,
    can_fix_system: bool,
    can_fix_tools: bool,
) -> anyhow::Result<()> {
    if budget_config.system_prompt_max_tokens > 0
        && breakdown.system_prompt_tokens > budget_config.system_prompt_max_tokens
    {
        if !can_fix_system {
            anyhow::bail!(
                "System prompt exceeds configured limit: {} tokens (limit: {})",
                breakdown.system_prompt_tokens,
                budget_config.system_prompt_max_tokens,
            );
        }
    }
    if budget_config.tool_definitions_max_tokens > 0
        && breakdown.tool_definition_tokens > budget_config.tool_definitions_max_tokens
    {
        if !can_fix_tools {
            anyhow::bail!(
                "Tool definitions exceed configured limit: {} tokens (limit: {})",
                breakdown.tool_definition_tokens,
                budget_config.tool_definitions_max_tokens,
            );
        }
    }
    Ok(())
}

/// Trim history strategy: removes oldest message groups to fit within budget.
#[derive(Debug, Clone, Copy)]
pub struct TrimHistoryStrategy;

impl BudgetEnforcementStrategy for TrimHistoryStrategy {
    fn name(&self) -> &'static str {
        "trim_history"
    }

    fn enforce(
        &self,
        _tools: Vec<ToolDefinition>,
        history: Vec<Message>,
        breakdown: &PromptBudgetBreakdown,
        effective_limit: usize,
        budget_config: &autonoetic_types::config::PromptBudgetConfig,
    ) -> anyhow::Result<EnforcementResult> {
        check_section_caps(breakdown, budget_config, false, false)?;
        let non_system: Vec<_> = history
            .iter()
            .filter(|m| m.role != crate::llm::Role::System)
            .cloned()
            .collect();
        let system: Vec<_> = history
            .iter()
            .filter(|m| m.role == crate::llm::Role::System)
            .cloned()
            .collect();

        let budget_for_conv = effective_limit
            .saturating_sub(breakdown.system_prompt_tokens)
            .saturating_sub(breakdown.tool_definition_tokens);

        let mut groups: Vec<(Vec<Message>, usize)> = Vec::new();
        let mut current_group: Vec<Message> = Vec::new();
        let mut current_group_tokens: usize = 0;
        let mut pending_tool_call_ids: HashSet<String> = HashSet::new();

        for msg in non_system {
            let msg_tokens = estimate_message_tokens(&msg);

            if msg.role == crate::llm::Role::Tool {
                current_group.push(msg);
                current_group_tokens += msg_tokens;
                if let Some(id) = pending_tool_call_ids.iter().next().cloned() {
                    pending_tool_call_ids.remove(&id);
                }
                if pending_tool_call_ids.is_empty() && !current_group.is_empty() {
                    groups.push((std::mem::take(&mut current_group), current_group_tokens));
                    current_group_tokens = 0;
                }
            } else if msg.role == crate::llm::Role::Assistant && !msg.tool_calls.is_empty() {
                pending_tool_call_ids = msg.tool_calls.iter().map(|tc| tc.id.clone()).collect();
                current_group.push(msg);
                current_group_tokens += msg_tokens;
                if pending_tool_call_ids.is_empty() {
                    groups.push((std::mem::take(&mut current_group), current_group_tokens));
                    current_group_tokens = 0;
                }
            } else {
                if !current_group.is_empty() {
                    groups.push((std::mem::take(&mut current_group), current_group_tokens));
                    current_group_tokens = 0;
                }
                groups.push((vec![msg], msg_tokens));
            }
        }
        if !current_group.is_empty() {
            groups.push((current_group, current_group_tokens));
        }

        let total_tokens: usize = groups.iter().map(|(_, t)| *t).sum();
        let mut current_total = total_tokens;

        let min_groups = 2.min(groups.len());

        while current_total > budget_for_conv && groups.len() > min_groups {
            let (_, group_tokens) = groups.remove(0);
            current_total = current_total.saturating_sub(group_tokens);
        }

        if current_total > budget_for_conv {
            if budget_for_conv == 0 {
                anyhow::bail!(
                    "Cannot trim history to fit within prompt budget: {} tokens remaining \
                     (budget: 0), hit message floor. The conversation budget is 0 because the \
                     system prompt ({} tokens) + tool definitions ({} tokens) already consume the \
                     entire effective limit ({} tokens).{} Set 'context_window_tokens' in the \
                     llm_preset configuration (or AUTONOETIC_LLM_CONTEXT_WINDOW env var) to the \
                     model's actual context window size.",
                    current_total,
                    breakdown.system_prompt_tokens,
                    breakdown.tool_definition_tokens,
                    effective_limit,
                    if breakdown.context_window.is_none() {
                        format!(
                            " The context window is UNKNOWN — using a conservative fallback of \
                             {} tokens that is too small for this model.",
                            FALLBACK_CONTEXT_WINDOW,
                        )
                    } else {
                        String::new()
                    },
                );
            } else {
                anyhow::bail!(
                    "Cannot trim history to fit within prompt budget: {} tokens remaining \
                     (budget: {}), hit message floor. Consider increasing the context window \
                     or reducing system prompt/tool definition size.",
                    current_total,
                    budget_for_conv,
                );
            }
        }

        let mut new_history = system;
        for (group, _) in groups {
            new_history.extend(group);
        }

        tracing::warn!(
            target: "autonoetic::prompt_budget",
            trimmed_messages = history.len() - new_history.len(),
            "Trimmed conversation history to fit within prompt budget"
        );

        Ok(EnforcementResult {
            tools: _tools,
            history: new_history,
            was_trimmed: true,
        })
    }
}

/// Demote tools strategy: removes specialized-tier tools to reduce token usage.
#[derive(Debug, Clone, Copy)]
pub struct DemoteToolsStrategy;

impl BudgetEnforcementStrategy for DemoteToolsStrategy {
    fn name(&self) -> &'static str {
        "demote_tools"
    }

    fn enforce(
        &self,
        tools: Vec<ToolDefinition>,
        history: Vec<Message>,
        breakdown: &PromptBudgetBreakdown,
        effective_limit: usize,
        budget_config: &autonoetic_types::config::PromptBudgetConfig,
    ) -> anyhow::Result<EnforcementResult> {
        check_section_caps(breakdown, budget_config, false, true)?;

        let filtered = filter_tools_by_tier(tools, &[ToolTier::Core, ToolTier::Workflow]);
        let removed_count = breakdown.tool_count - filtered.len();

        let filtered_tool_tokens: usize = filtered.iter().map(estimate_tool_definition).sum();

        if budget_config.tool_definitions_max_tokens > 0
            && filtered_tool_tokens > budget_config.tool_definitions_max_tokens
        {
            anyhow::bail!(
                "Tool definitions still exceed limit after demotion: {} tokens (limit: {}). \
                 Core and workflow tools alone exceed the configured cap.",
                filtered_tool_tokens,
                budget_config.tool_definitions_max_tokens,
            );
        }

        let filtered_total =
            breakdown.total_tokens - breakdown.tool_definition_tokens + filtered_tool_tokens;
        if filtered_total > effective_limit {
            anyhow::bail!(
                "Prompt budget still exceeded after tool demotion: {} tokens (limit: {}). \
                 Removed {} specialized tools but core/workflow tools are still too large.",
                filtered_total,
                effective_limit,
                removed_count,
            );
        }

        tracing::warn!(
            target: "autonoetic::prompt_budget",
            removed_tools = removed_count,
            tool_tokens_before = breakdown.tool_definition_tokens,
            tool_tokens_after = filtered_tool_tokens,
            "Demoted specialized tools to fit within prompt budget"
        );

        Ok(EnforcementResult {
            tools: filtered,
            history,
            was_trimmed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_approximate() {
        let text = "hello world this is a test";
        let tokens = estimate_tokens(text);
        assert!(tokens > 0);
    }

    #[test]
    fn test_default_chars_per_token_is_three() {
        // The default ratio was lowered from 4.0 → 3.0 because real-world
        // tokenizers (Qwen3, Llama3, GPT-4o) typically achieve 2.2–3.5
        // chars/token on code/JSON content, and the 4.0 heuristic
        // systematically underestimated such prompts, letting the context
        // governor stay silent until the LLM call returned a 400.
        assert_eq!(DEFAULT_CHARS_PER_TOKEN, 3.0);
        // We don't assert on chars_per_token() directly here because other
        // tests in this module may have already set it; the next test
        // resets it.
    }

    #[test]
    fn test_set_chars_per_token_round_trips_and_clamps() {
        // Round-trip a value inside the legal range.
        let stored = set_chars_per_token(2.5);
        assert_eq!(stored, 2.5);
        assert!((chars_per_token() - 2.5).abs() < 1e-9);

        // Values below MIN get clamped to MIN.
        let stored = set_chars_per_token(0.1);
        assert_eq!(stored, 0.5);
        assert!((chars_per_token() - 0.5).abs() < 1e-9);

        // Values above MAX get clamped to MAX.
        let stored = set_chars_per_token(100.0);
        assert_eq!(stored, 16.0);
        assert!((chars_per_token() - 16.0).abs() < 1e-9);

        // Malformed input (NaN, 0, negative, infinity) resets to default.
        for bad in [f64::NAN, 0.0, -1.0, f64::INFINITY, f64::NEG_INFINITY] {
            let stored = set_chars_per_token(bad);
            assert_eq!(stored, DEFAULT_CHARS_PER_TOKEN);
            assert!((chars_per_token() - DEFAULT_CHARS_PER_TOKEN).abs() < 1e-9);
        }
    }

    #[test]
    fn test_estimate_tokens_obeys_setter() {
        // Lock the ratio to 1.0 so the math is exact regardless of default.
        set_chars_per_token(1.0);
        // 4 chars → 4 tokens.
        assert_eq!(estimate_tokens("abcd"), 4);
        set_chars_per_token(2.0);
        // ceil(4/2) = 2 tokens.
        assert_eq!(estimate_tokens("abcd"), 2);
        set_chars_per_token(4.0);
        // ceil(4/4) = 1 token.
        assert_eq!(estimate_tokens("abcd"), 1);
        // Restore the default so downstream tests that rely on it are
        // unaffected. Use a safe round-trip rather than reaching into the
        // atomic directly.
        set_chars_per_token(DEFAULT_CHARS_PER_TOKEN);
    }

    #[test]
    fn test_estimate_message_tokens_counts_tool_calls() {
        set_chars_per_token(1.0);

        // Plain message — same as content-only.
        let plain = Message::assistant("hello");
        assert_eq!(estimate_message_tokens(&plain), 5);

        // Assistant with empty content but large tool_call arguments
        // (the exact pattern that caused the governor blind spot:
        // content_write with a big file body).
        let big_args = "x".repeat(3000);
        let with_tool_call = Message {
            role: crate::llm::Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "content_write".to_string(),
                arguments: format!(r#"{{"content":"{}"}}"#, big_args),
            }],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        };
        let tool_call_tokens = estimate_message_tokens(&with_tool_call);
        // Must be much larger than the empty-content estimate (0).
        assert!(
            tool_call_tokens > 1000,
            "tool_call arguments must be counted, got {}",
            tool_call_tokens
        );

        set_chars_per_token(DEFAULT_CHARS_PER_TOKEN);
    }

    #[test]
    fn test_estimate_message_tokens_counts_reasoning_content() {
        set_chars_per_token(1.0);

        let with_reasoning = Message {
            role: crate::llm::Role::Assistant,
            content: "ok".to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: Some("thinking ".repeat(100)),
            reasoning_details: None,
        };
        let tokens = estimate_message_tokens(&with_reasoning);
        // "ok" = 2 + "thinking " * 100 = 900 → total must exceed content-only.
        assert!(
            tokens > 100,
            "reasoning_content must be counted, got {}",
            tokens
        );

        set_chars_per_token(DEFAULT_CHARS_PER_TOKEN);
    }

    #[test]
    fn test_tool_tier_core() {
        assert_eq!(tool_tier("content_write"), ToolTier::Core);
        assert_eq!(tool_tier("resolve"), ToolTier::Core);
        assert_eq!(tool_tier("knowledge_store"), ToolTier::Core);
        assert_eq!(tool_tier("sandbox_exec"), ToolTier::Core);
    }

    #[test]
    fn test_tool_tier_workflow() {
        assert_eq!(tool_tier("agent_spawn"), ToolTier::Workflow);
        assert_eq!(tool_tier("workflow_wait"), ToolTier::Workflow);
        assert_eq!(tool_tier("approval_status"), ToolTier::Workflow);
    }

    /// `agent_message` must be Workflow, like its sibling `agent_spawn`.
    ///
    /// With no rule for it in `config/tools.yaml` it fell through to
    /// `default_tier: specialized`, which silently removed it from the
    /// advertised tool list of every child session and every un-escalated root
    /// session — including agents whose manifest declares `AgentMessage` and
    /// whose SKILL.md instructs them to use it. Regression guard for that.
    #[test]
    fn agent_message_is_workflow_tier_so_declaring_agents_can_see_it() {
        assert_eq!(tool_tier("agent_message"), ToolTier::Workflow);
    }

    #[test]
    fn test_tool_tier_specialized() {
        assert_eq!(tool_tier("web_search"), ToolTier::Specialized);
        assert_eq!(tool_tier("web_fetch"), ToolTier::Specialized);
        assert_eq!(tool_tier("promotion_record"), ToolTier::Specialized);
    }

    #[test]
    fn test_filter_tools_by_tier() {
        let tools = vec![
            ToolDefinition {
                name: "content_write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent_spawn".to_string(),
                description: "Spawn agent".to_string(),
                input_schema: serde_json::json!({}),
            },
        ];

        let core_only = filter_tools_by_tier(tools.clone(), &[ToolTier::Core]);
        assert_eq!(core_only.len(), 1);
        assert_eq!(core_only[0].name, "content_write");

        let core_and_workflow =
            filter_tools_by_tier(tools.clone(), &[ToolTier::Core, ToolTier::Workflow]);
        assert_eq!(core_and_workflow.len(), 2);

        let all = filter_tools_by_tier(tools, &[]);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_budget_breakdown_compute() {
        let system = "You are a helpful assistant.";
        let messages = vec![Message::user("Hello"), Message::assistant("Hi there")];
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];

        let breakdown = PromptBudgetBreakdown::compute(system, &messages, &tools, Some(128_000));

        assert!(breakdown.system_prompt_tokens > 0);
        assert!(breakdown.conversation_tokens > 0);
        assert_eq!(breakdown.tool_count, 1);
        assert!(breakdown.tool_definition_tokens > 0);
        assert!(breakdown.total_tokens > 0);
        assert_eq!(breakdown.context_window, Some(128_000));
        assert!(breakdown.utilization_pct.is_some());
        assert!(breakdown.utilization_pct.unwrap() > 0.0);
        assert!(breakdown.utilization_pct.unwrap() < 100.0);
    }

    #[test]
    fn cap_tool_definitions_preserves_discovered_specialized_tools() {
        let tools: Vec<ToolDefinition> = (0..45)
            .map(|i| ToolDefinition {
                name: format!("core_tool_{i}"),
                description: "core".to_string(),
                input_schema: serde_json::json!({}),
            })
            .chain([ToolDefinition {
                name: "federation_escalate".to_string(),
                description: "Escalate".to_string(),
                input_schema: serde_json::json!({}),
            }])
            .collect();

        let discovered = HashSet::from(["federation_escalate".to_string()]);
        let capped = cap_tool_definitions_preserving_discovered(tools, 40, &discovered);

        assert_eq!(capped.len(), 40);
        assert!(
            capped.iter().any(|t| t.name == "federation_escalate"),
            "discovered tool must survive truncation"
        );
    }

    #[test]
    fn planner_tool_surface_counts_by_tier() {
        use crate::runtime::tool_dispatch::determine_tool_tier_filter;
        use crate::runtime::tools::default_registry;
        use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
        use autonoetic_types::capability::Capability;

        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "planner.default".to_string(),
                name: "Planner".to_string(),
                description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
            capabilities: vec![
                Capability::SandboxFunctions {
                    allowed: vec![
                        "knowledge.".to_string(),
                        "agent.".to_string(),
                        "credential.".to_string(),
                    ],
                },
                Capability::CredentialAccess {
                    services: vec!["*".to_string()],
                },
                Capability::AgentSpawn {
                    max_children: 10,
                    max_spawn_depth: 0,
                },
                Capability::SchedulerAccess {
                    patterns: vec!["*".to_string()],
                },
                Capability::WriteAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
                Capability::AgentMessage {
                    patterns: vec!["*".to_string()],
                },
            ],
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
        };

        let registry = default_registry();
        let all_available = registry.available_definitions(&manifest);
        let core_workflow_filter =
            determine_tool_tier_filter(&manifest, Some("session-root"), false, autonoetic_types::agent::SessionState::Normal, false);
        let core_workflow = registry.available_definitions_filtered(&manifest, Some(&core_workflow_filter));
        let all_tiers_filter =
            determine_tool_tier_filter(&manifest, Some("session-root"), false, autonoetic_types::agent::SessionState::Normal, true);
        let all_tiers = registry.available_definitions_filtered(&manifest, Some(&all_tiers_filter));

        eprintln!(
            "planner tool counts: all_available={} core+workflow={} all_tiers_escalated={}",
            all_available.len(),
            core_workflow.len(),
            all_tiers.len()
        );
        let specialized_only: Vec<&str> = all_tiers
            .iter()
            .filter(|d| !core_workflow.iter().any(|c| c.name == d.name))
            .map(|d| d.name.as_str())
            .collect();
        eprintln!(
            "planner specialized-only (need tier escalation): {:?}",
            specialized_only
        );

        // Documented baseline in config-template.yaml — keep in sync when tiers change.
        assert!(
            core_workflow.len() >= 35 && core_workflow.len() <= 45,
            "expected ~40 core+workflow tools for planner, got {}",
            core_workflow.len()
        );
        assert!(
            all_tiers.len() >= 45 && all_tiers.len() <= 55,
            "escalated planner surface should remain modestly above max_tool_definitions cap (got {})",
            all_tiers.len()
        );
    }

    #[test]
    fn sanitize_history_strips_reasoning_from_assistant_messages() {
        let history = vec![
            Message {
                role: Role::System,
                content: "system".to_string(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Assistant,
                content: "hello".to_string(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: Some("deep thought".to_string()),
                reasoning_details: Some(serde_json::json!([{"text": "step 1"}])),
            },
        ];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: true,
                max_tool_result_chars: 2000,
                dedup_tool_results: false,
                collapse_repeated_errors: false,
            },
        );

        assert_eq!(sanitized[0].role, Role::System);
        assert!(sanitized[0].reasoning_content.is_none());
        assert_eq!(sanitized[1].role, Role::Assistant);
        assert_eq!(sanitized[1].content, "hello");
        assert!(sanitized[1].reasoning_content.is_none());
        assert!(sanitized[1].reasoning_details.is_none());

        // Original history is untouched.
        assert_eq!(history[1].reasoning_content.as_deref(), Some("deep thought"));
    }

    #[test]
    fn sanitize_history_default_preserves_reasoning() {
        // Default options must keep reasoning content because many thinking/
        // reasoning models require it to be replayed across tool-call turns.
        let history = vec![Message {
            role: Role::Assistant,
            content: "hello".to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: Some("deep thought".to_string()),
            reasoning_details: Some(serde_json::json!([{"text": "step 1"}])),
        }];

        let sanitized = sanitize_history_for_request(&history, &HistorySanitizeOptions::default());

        assert_eq!(sanitized[0].reasoning_content.as_deref(), Some("deep thought"));
        assert!(sanitized[0].reasoning_details.is_some());
    }

    #[test]
    fn sanitize_history_keeps_reasoning_when_disabled() {
        let history = vec![Message {
            role: Role::Assistant,
            content: "hello".to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: Some("deep thought".to_string()),
            reasoning_details: None,
        }];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: false,
                collapse_repeated_errors: false,
            },
        );

        assert_eq!(sanitized[0].reasoning_content.as_deref(), Some("deep thought"));
    }

    #[test]
    fn sanitize_history_truncates_long_tool_results() {
        let long_content = "x".repeat(5000);
        let history = vec![Message {
            role: Role::Tool,
            content: long_content.clone(),
            tool_calls: vec![],
            tool_call_id: Some("tc_1".to_string()),
            reasoning_content: None,
            reasoning_details: None,
        }];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 100,
                dedup_tool_results: false,
                collapse_repeated_errors: false,
            },
        );

        assert!(sanitized[0].content.len() < long_content.len());
        assert!(sanitized[0].content.contains("[..."));
        assert!(sanitized[0].content.contains("chars truncated ...]"));
        assert!(sanitized[0].content.starts_with('x'));
        assert!(sanitized[0].content.ends_with('x'));
    }

    #[test]
    fn sanitize_history_does_not_shorten_small_tool_results() {
        let history = vec![Message {
            role: Role::Tool,
            content: "small result".to_string(),
            tool_calls: vec![],
            tool_call_id: Some("tc_1".to_string()),
            reasoning_content: None,
            reasoning_details: None,
        }];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 100,
                dedup_tool_results: false,
                collapse_repeated_errors: false,
            },
        );

        assert_eq!(sanitized[0].content, "small result");
    }

    #[test]
    fn sanitize_history_dedups_consecutive_duplicate_tool_results() {
        let history = vec![
            Message {
                role: Role::Assistant,
                content: "call 1".to_string(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: "same result".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_1".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Assistant,
                content: "call 2".to_string(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: "same result".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_2".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: "same result".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_3".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: true,
                collapse_repeated_errors: false,
            },
        );

        // First occurrence is preserved.
        assert_eq!(sanitized[1].content, "same result");
        assert_eq!(sanitized[1].tool_call_id.as_deref(), Some("tc_1"));

        // Second and third consecutive duplicates are replaced by a marker.
        assert_eq!(sanitized[3].content, "[duplicate result — see above]");
        assert_eq!(sanitized[3].tool_call_id.as_deref(), Some("tc_2"));
        assert_eq!(sanitized[4].content, "[duplicate result — see above]");
        assert_eq!(sanitized[4].tool_call_id.as_deref(), Some("tc_3"));

        // Original history is untouched.
        assert_eq!(history[3].content, "same result");
    }

    #[test]
    fn sanitize_history_dedup_resets_on_non_duplicate() {
        let history = vec![
            Message {
                role: Role::Tool,
                content: "result A".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_1".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: "result B".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_2".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: "result A".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_3".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: true,
                collapse_repeated_errors: false,
            },
        );

        // The third message is identical to the first but not consecutive, so
        // it is not collapsed.
        assert_eq!(sanitized[0].content, "result A");
        assert_eq!(sanitized[1].content, "result B");
        assert_eq!(sanitized[2].content, "result A");
    }

    #[test]
    fn sanitize_history_dedup_disabled_keeps_duplicates() {
        let history = vec![
            Message {
                role: Role::Tool,
                content: "same result".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_1".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: "same result".to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_2".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: false,
                collapse_repeated_errors: false,
            },
        );

        assert_eq!(sanitized[0].content, "same result");
        assert_eq!(sanitized[1].content, "same result");
    }

    #[test]
    fn sanitize_history_dedup_composes_with_truncation() {
        let long = "x".repeat(5000);
        let history = vec![
            Message {
                role: Role::Tool,
                content: long.clone(),
                tool_calls: vec![],
                tool_call_id: Some("tc_1".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: long.clone(),
                tool_calls: vec![],
                tool_call_id: Some("tc_2".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 100,
                dedup_tool_results: true,
                collapse_repeated_errors: false,
            },
        );

        // Both are truncated identically, so the second collapses.
        assert!(sanitized[0].content.contains("[..."));
        assert_eq!(sanitized[1].content, "[duplicate result — see above]");
    }

    fn tool_err(id: &str, wf: &str) -> Message {
        Message {
            role: Role::Tool,
            content: format!(
                r#"{{"ok":false,"error":"workflow {wf} was reactivated and cannot accept child-session spawns"}}"#
            ),
            tool_calls: vec![],
            tool_call_id: Some(id.to_string()),
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    /// #705: the same normalized error (different volatile workflow ids),
    /// appearing non-consecutively, collapses to a marker on all but the most
    /// recent occurrence; the latest is kept in full.
    #[test]
    fn collapse_repeated_errors_keeps_only_latest() {
        let history = vec![
            tool_err("tc_1", "wf-aaa111"),
            Message::assistant("try another approach"),
            tool_err("tc_2", "wf-bbb222"),
            Message::assistant("try yet another"),
            tool_err("tc_3", "wf-ccc333"),
        ];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: false,
                collapse_repeated_errors: true,
            },
        );

        let marker = "[repeated error";
        assert!(sanitized[0].content.starts_with(marker), "first collapsed");
        assert_eq!(sanitized[0].tool_call_id.as_deref(), Some("tc_1"));
        assert!(sanitized[2].content.starts_with(marker), "middle collapsed");
        // Latest occurrence kept in full.
        assert!(sanitized[4].content.contains("reactivated"));
        assert!(!sanitized[4].content.starts_with(marker));
        // Storage untouched.
        assert!(history[0].content.contains("wf-aaa111"));
    }

    /// A one-off error is not collapsed (needs >= 2 occurrences).
    #[test]
    fn collapse_repeated_errors_leaves_single_error() {
        let history = vec![tool_err("tc_1", "wf-solo")];
        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: false,
                collapse_repeated_errors: true,
            },
        );
        assert!(sanitized[0].content.contains("reactivated"));
    }

    /// Distinct errors are not collapsed into each other.
    #[test]
    fn collapse_repeated_errors_ignores_distinct_errors() {
        let history = vec![
            Message {
                role: Role::Tool,
                content: r#"{"ok":false,"error":"disk full"}"#.to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_1".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: r#"{"ok":false,"error":"permission denied"}"#.to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_2".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
        ];
        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: false,
                collapse_repeated_errors: true,
            },
        );
        assert!(sanitized[0].content.contains("disk full"));
        assert!(sanitized[1].content.contains("permission denied"));
    }

    /// Successful (non-error) repeated results are left to `dedup_tool_results`,
    /// not touched by the error-collapse pass.
    #[test]
    fn collapse_repeated_errors_ignores_successful_results() {
        let history = vec![
            Message {
                role: Role::Tool,
                content: r#"{"ok":true,"stdout":"same"}"#.to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_1".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::Tool,
                content: r#"{"ok":true,"stdout":"same"}"#.to_string(),
                tool_calls: vec![],
                tool_call_id: Some("tc_2".to_string()),
                reasoning_content: None,
                reasoning_details: None,
            },
        ];
        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 0,
                dedup_tool_results: false,
                collapse_repeated_errors: true,
            },
        );
        assert!(sanitized[0].content.contains("stdout"));
        assert!(sanitized[1].content.contains("stdout"));
    }

    #[test]
    fn truncate_middle_boundary_cases() {
        assert_eq!(truncate_middle("hello", 10), "hello");
        assert_eq!(truncate_middle("hello world", 11), "hello world");
        assert!(
            truncate_middle("a".repeat(500).as_str(), 100).contains("[..."),
            "expected middle-truncation marker for large content"
        );
        // When max_chars is too small for head + ellipsis + tail, fall back
        // to a simple head truncation.
        assert_eq!(truncate_middle("hello world", 5).len(), 5);
    }

    #[test]
    fn truncate_tool_result_preserves_json_structure_and_metadata() {
        let big = "x".repeat(5000);
        let content = format!(
            r#"{{"ok":true,"kind":"content","ref":"test.txt","content":"{big}","offset":0,"limit":5000,"next_offset":5000,"total_bytes":50000,"truncated":true}}"#
        );

        let result = truncate_tool_result(&content, 400);

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("result must be valid JSON");

        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["kind"], "content");
        assert_eq!(parsed["offset"], 0);
        assert_eq!(parsed["next_offset"], 5000);
        assert_eq!(parsed["total_bytes"], 50000);
        assert_eq!(parsed["truncated"], true);

        let content_field = parsed["content"].as_str().unwrap();
        assert!(
            content_field.contains("[..."),
            "content field should contain truncation marker"
        );
        assert!(
            content_field.chars().count() < big.chars().count(),
            "content field should be shorter than the original"
        );
    }

    #[test]
    fn truncate_tool_result_short_json_untouched() {
        let content = r#"{"ok":true,"result":"small"}"#;
        let result = truncate_tool_result(content, 4000);
        assert_eq!(result, content);
    }

    #[test]
    fn truncate_tool_result_non_json_falls_back_to_middle() {
        let big = "x".repeat(5000);
        let result = truncate_tool_result(&big, 100);
        assert!(result.contains("[..."));
        assert!(
            result.chars().count() < big.chars().count(),
            "result should be significantly shorter than the original"
        );
    }

    #[test]
    fn truncate_tool_result_preserves_error_fields() {
        let big_err = "E".repeat(3000);
        let content = format!(
            r#"{{"ok":false,"error_type":"validation","error":"{big_err}","repair_hint":"pass file=<name>"}}"#
        );

        let result = truncate_tool_result(&content, 300);

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("error result must be valid JSON");

        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["error_type"], "validation");
        assert_eq!(parsed["repair_hint"], "pass file=<name>");

        let error_field = parsed["error"].as_str().unwrap();
        assert!(error_field.contains("[..."));
    }

    #[test]
    fn truncate_tool_result_exempts_routing_directive_fields() {
        // Many large pattern entries shrink the per-field budget; the
        // routing directives (error_type/repair_hint/fix_target/
        // available_actions) must survive verbatim while the bulky
        // per-pattern strings get truncated.
        let long_reason = "R".repeat(2000);
        let long_hint = "This is a manifest declaration gap, not a code bug — report to caller. ".repeat(10);
        let patterns: Vec<serde_json::Value> = (0..20)
            .map(|i| {
                serde_json::json!({
                    "category": "function_call",
                    "pattern": format!("requests.get( #{i}"),
                    "line_number": i,
                    "reason": long_reason,
                })
            })
            .collect();
        let content = serde_json::json!({
            "ok": false,
            "error_type": "undeclared_remote_pattern",
            "error_class": "manifest_declaration_gap",
            "fix_target": "manifest",
            "repair_hint": long_hint,
            "available_actions": [
                {"action": "report_to_caller", "reason": "manifest_declaration_gap", "detail": long_hint},
                {"action": "delegate", "delegate": "agent-factory.default", "reason": "manifest_declaration_gap", "detail": "builder only"}
            ],
            "undeclared_patterns": patterns,
        })
        .to_string();

        let result = truncate_tool_result(&content, 12000);

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("result must stay valid JSON");
        assert_eq!(parsed["error_type"], "undeclared_remote_pattern");
        assert_eq!(parsed["error_class"], "manifest_declaration_gap");
        assert_eq!(parsed["fix_target"], "manifest");
        assert_eq!(parsed["repair_hint"], long_hint);
        assert_eq!(parsed["available_actions"][0]["detail"], long_hint);
        assert_eq!(
            parsed["available_actions"][1]["delegate"],
            "agent-factory.default"
        );
        let pattern_reason = parsed["undeclared_patterns"][0]["reason"]
            .as_str()
            .unwrap();
        assert!(
            pattern_reason.len() < long_reason.len(),
            "non-exempt pattern strings should still be truncated"
        );
    }

    #[test]
    fn sanitize_history_uses_fast_truncate_middle_for_tool_results() {
        // sanitize_history_for_request uses truncate_middle (fast string op)
        // as a safety net. JSON-aware truncation happens at push time.
        let big_content = "y".repeat(8000);
        let tool_result = serde_json::json!({
            "ok": true,
            "kind": "content",
            "ref": "big.txt",
            "content": big_content,
            "next_offset": 8000,
        })
        .to_string();

        let history = vec![Message {
            role: Role::Tool,
            content: tool_result,
            tool_calls: vec![],
            tool_call_id: Some("tc_1".to_string()),
            reasoning_content: None,
            reasoning_details: None,
        }];

        let sanitized = sanitize_history_for_request(
            &history,
            &HistorySanitizeOptions {
                strip_reasoning: false,
                max_tool_result_chars: 500,
                dedup_tool_results: false,
                collapse_repeated_errors: false,
            },
        );

        assert!(
            sanitized[0].content.contains("[..."),
            "should contain truncation marker"
        );
        assert!(
            sanitized[0].content.chars().count() <= 600,
            "should be under budget + marker overhead"
        );
    }

    #[test]
    fn default_max_tool_result_chars_is_4000() {
        assert_eq!(HistorySanitizeOptions::default().max_tool_result_chars, 4000);
    }
}
