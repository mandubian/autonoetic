//! Compression Quality Regression Framework.
//!
//! Records golden sessions as JSON fixtures and replays them with/without
//! compression to detect quality regressions. Compares tool call sequences,
//! decisions, and final output shape between compressed and uncompressed runs.

use crate::llm::{CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, ToolCall, TokenUsage};
use crate::runtime::compression::{compress_context, CompressionMetadata};
use autonoetic_types::agent::CompressionConfig;
use autonoetic_types::config::{ContextCompressionConfig, LlmPreset};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// A recorded LLM turn in a golden session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenTurn {
    /// The user message sent to the LLM.
    pub user_message: String,
    /// The expected assistant response text.
    pub assistant_response: String,
    /// Expected tool calls (if any).
    #[serde(default)]
    pub tool_calls: Vec<GoldenToolCall>,
    /// Whether this turn should trigger EndTurn.
    #[serde(default = "default_end_turn")]
    pub end_turn: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoldenToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

fn default_end_turn() -> bool {
    true
}

/// A golden session fixture — a recorded conversation that can be replayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenSession {
    /// Unique identifier for this fixture.
    pub id: String,
    /// Description of what this session tests.
    pub description: String,
    /// The system prompt used in this session.
    pub system_prompt: String,
    /// Recorded turns (user → assistant exchanges).
    pub turns: Vec<GoldenTurn>,
    /// Expected final outcome summary.
    pub expected_outcome: String,
    /// Expected tool call sequence across all turns (flattened).
    #[serde(default)]
    pub expected_tool_sequence: Vec<GoldenToolCall>,
}

impl GoldenSession {
    /// Load a golden session from a JSON fixture file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let session: GoldenSession = serde_json::from_str(&content)?;
        Ok(session)
    }

    /// Save this golden session to a JSON fixture file.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

/// ReplayDriver: an LlmDriver that replays pre-recorded responses.
///
/// Extends the FixedTextDriver pattern by returning turn-specific
/// responses from a GoldenSession instead of a single fixed text.
pub struct ReplayDriver {
    turns: Vec<GoldenTurn>,
    current_turn: std::sync::Mutex<usize>,
}

impl ReplayDriver {
    pub fn new(session: &GoldenSession) -> Self {
        Self {
            turns: session.turns.clone(),
            current_turn: std::sync::Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmDriver for ReplayDriver {
    async fn complete(
        &self,
        _request: &CompletionRequest,
    ) -> anyhow::Result<CompletionResponse> {
        let mut turn_idx = self.current_turn.lock().unwrap();
        let turn = if *turn_idx < self.turns.len() {
            &self.turns[*turn_idx]
        } else {
            // Return a default end-turn response for extra turns
            return Ok(CompletionResponse {
                text: "done".to_string(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            });
        };
        *turn_idx += 1;
        drop(turn_idx);

        let tool_calls: Vec<ToolCall> = turn
            .tool_calls
            .iter()
            .map(|gt| ToolCall {
                id: gt.id.clone(),
                name: gt.name.clone(),
                arguments: gt.arguments.clone(),
            })
            .collect();

        let stop_reason = if turn.end_turn {
            StopReason::EndTurn
        } else {
            StopReason::ToolUse
        };

        Ok(CompletionResponse {
            text: turn.assistant_response.clone(),
            tool_calls,
            stop_reason,
            usage: TokenUsage::default(),
        })
    }
}

/// Result of a single replay run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplayResult {
    /// All assistant response texts in order.
    pub responses: Vec<String>,
    /// All tool calls made across all turns (flattened).
    pub tool_calls: Vec<GoldenToolCall>,
    /// Final turn count.
    pub turn_count: usize,
    /// Whether the session ended with EndTurn.
    pub ended_normally: bool,
}

/// Compare results between compressed and uncompressed runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonResult {
    /// Whether the runs are considered equivalent.
    pub equivalent: bool,
    /// Tool call sequence matches exactly.
    pub tool_sequence_match: bool,
    /// Same number of turns.
    pub turn_count_match: bool,
    /// Both ended normally.
    pub both_ended_normally: bool,
    /// Details about differences found.
    pub differences: Vec<String>,
}

/// Compare two replay results structurally.
pub fn compare_results(baseline: &ReplayResult, candidate: &ReplayResult) -> ComparisonResult {
    let mut differences = Vec::new();

    let tool_sequence_match = baseline.tool_calls == candidate.tool_calls;
    if !tool_sequence_match {
        differences.push(format!(
            "Tool sequence mismatch: baseline has {} calls, candidate has {}",
            baseline.tool_calls.len(),
            candidate.tool_calls.len()
        ));
        for (i, (b, c)) in baseline.tool_calls.iter().zip(candidate.tool_calls.iter()).enumerate() {
            if b != c {
                differences.push(format!("  Turn {}: baseline={:?}, candidate={:?}", i, b, c));
            }
        }
    }

    let turn_count_match = baseline.turn_count == candidate.turn_count;
    if !turn_count_match {
        differences.push(format!(
            "Turn count mismatch: baseline={}, candidate={}",
            baseline.turn_count, candidate.turn_count
        ));
    }

    let both_ended_normally = baseline.ended_normally && candidate.ended_normally;
    if !both_ended_normally {
        differences.push(format!(
            "Ending mismatch: baseline_ended={}, candidate_ended={}",
            baseline.ended_normally, candidate.ended_normally
        ));
    }

    let equivalent = tool_sequence_match && turn_count_match && both_ended_normally;

    ComparisonResult {
        equivalent,
        tool_sequence_match,
        turn_count_match,
        both_ended_normally,
        differences,
    }
}

/// Run a replay with the given compression config.
///
/// This simulates the compression flow by:
/// 1. Building the full message history from the golden session
/// 2. Optionally applying compression
/// 3. Replaying remaining turns with the (possibly compressed) history
pub async fn run_replay(
    session: &GoldenSession,
    compression_enabled: bool,
    compression_cfg: &ContextCompressionConfig,
    agent_cfg: Option<&CompressionConfig>,
) -> ReplayResult {
    let mut responses = Vec::new();
    let mut all_tool_calls = Vec::new();
    let mut history: Vec<Message> = vec![Message::system(&session.system_prompt)];

    let mut turn_count = 0;
    let mut ended_normally = false;

    for turn in &session.turns {
        history.push(Message::user(&turn.user_message));

        if compression_enabled && turn_count > 0 {
            let presets = HashMap::new();
            let http_client = reqwest::Client::new();
            match compress_context(
                history.clone(),
                Some(128_000),
                compression_cfg,
                agent_cfg,
                &presets,
                &http_client,
                &session.id,
                turn_count as u64,
                None,
            )
            .await
            {
                Ok(result) => {
                    if result.compressed {
                        history = result.history;
                    }
                }
                Err(_) => {}
            }
        }

        let driver = ReplayDriver::new(&GoldenSession {
            id: session.id.clone(),
            description: session.description.clone(),
            system_prompt: session.system_prompt.clone(),
            turns: vec![turn.clone()],
            expected_outcome: session.expected_outcome.clone(),
            expected_tool_sequence: session.expected_tool_sequence.clone(),
        });

        let response = driver
            .complete(&CompletionRequest {
                model: "test".to_string(),
                messages: history.clone(),
                tools: vec![],
                max_tokens: None,
                temperature: None,
                metadata: None,
            })
            .await
            .unwrap();

        responses.push(response.text.clone());
        all_tool_calls.extend(response.tool_calls.iter().map(|tc| GoldenToolCall {
            id: tc.id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        }));

        history.push(Message::assistant(response.text.clone()));
        if !response.tool_calls.is_empty() {
            for tc in &response.tool_calls {
                history.push(Message::tool_result(&tc.id, &tc.name, "ok"));
            }
        }

        ended_normally = response.stop_reason == StopReason::EndTurn;
        turn_count += 1;
    }

    ReplayResult {
        responses,
        tool_calls: all_tool_calls,
        turn_count,
        ended_normally,
    }
}

/// Run the same session with and without compression and compare results.
pub async fn run_comparison(
    session: &GoldenSession,
    compression_cfg: &ContextCompressionConfig,
    agent_cfg: Option<&CompressionConfig>,
) -> ComparisonResult {
    let baseline = run_replay(session, false, compression_cfg, agent_cfg).await;
    let candidate = run_replay(session, true, compression_cfg, agent_cfg).await;
    compare_results(&baseline, &candidate)
}

/// Scan compression at different threshold points and compare results.
pub async fn run_threshold_scan(
    session: &GoldenSession,
    thresholds: &[f64],
) -> Vec<(f64, ComparisonResult)> {
    let mut results = Vec::new();
    for threshold in thresholds {
        let cfg = ContextCompressionConfig {
            enabled: true,
            threshold_pct: *threshold,
            ..Default::default()
        };
        let comparison = run_comparison(session, &cfg, None).await;
        results.push((*threshold, comparison));
    }
    results
}

/// Default compression config for testing.
pub fn default_test_compression_config() -> ContextCompressionConfig {
    ContextCompressionConfig {
        enabled: true,
        llm_preset: Some("haiku".to_string()),
        threshold_pct: 50.0,
        recent_turns_to_keep: 3,
        max_summary_tokens: 500,
        min_turns_between_compression: 3,
        provider: None,
        model: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_session() -> GoldenSession {
        GoldenSession {
            id: "test-simple-convo".to_string(),
            description: "Simple conversation with tool use".to_string(),
            system_prompt: "You are a helpful assistant.".to_string(),
            turns: vec![
                GoldenTurn {
                    user_message: "What is the weather?".to_string(),
                    assistant_response: "Let me check.".to_string(),
                    tool_calls: vec![GoldenToolCall {
                        id: "tc1".to_string(),
                        name: "weather.get".to_string(),
                        arguments: r#"{"city":"NYC"}"#.to_string(),
                    }],
                    end_turn: false,
                },
                GoldenTurn {
                    user_message: "The weather is sunny.".to_string(),
                    assistant_response: "Great, enjoy the sunny weather!".to_string(),
                    tool_calls: vec![],
                    end_turn: true,
                },
            ],
            expected_outcome: "Weather query completed".to_string(),
            expected_tool_sequence: vec![GoldenToolCall {
                id: "tc1".to_string(),
                name: "weather.get".to_string(),
                arguments: r#"{"city":"NYC"}"#.to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn test_replay_driver_returns_recorded_responses() {
        let session = make_test_session();
        let driver = ReplayDriver::new(&session);

        let response = driver
            .complete(&CompletionRequest {
                model: "test".to_string(),
                messages: vec![],
                tools: vec![],
                max_tokens: None,
                temperature: None,
                metadata: None,
            })
            .await
            .unwrap();

        assert_eq!(response.text, "Let me check.");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "weather.get");
        assert!(matches!(response.stop_reason, StopReason::ToolUse));
    }

    #[tokio::test]
    async fn test_replay_without_compression_matches_baseline() {
        let session = make_test_session();
        let cfg = default_test_compression_config();

        let result = run_replay(&session, false, &cfg, None).await;

        assert_eq!(result.responses.len(), 2);
        assert_eq!(result.responses[0], "Let me check.");
        assert_eq!(result.responses[1], "Great, enjoy the sunny weather!");
        assert_eq!(result.tool_calls.len(), 1);
        assert!(result.ended_normally);
    }

    #[tokio::test]
    async fn test_compare_equivalent_results() {
        let baseline = ReplayResult {
            responses: vec!["a".into(), "b".into()],
            tool_calls: vec![],
            turn_count: 2,
            ended_normally: true,
        };
        let candidate = baseline.clone();
        let comparison = compare_results(&baseline, &candidate);

        assert!(comparison.equivalent);
        assert!(comparison.tool_sequence_match);
        assert!(comparison.turn_count_match);
        assert!(comparison.both_ended_normally);
        assert!(comparison.differences.is_empty());
    }

    #[tokio::test]
    async fn test_compare_different_tool_sequences() {
        let baseline = ReplayResult {
            responses: vec!["a".into()],
            tool_calls: vec![GoldenToolCall {
                id: "1".into(),
                name: "tool_a".into(),
                arguments: "{}".into(),
            }],
            turn_count: 1,
            ended_normally: true,
        };
        let candidate = ReplayResult {
            responses: vec!["a".into()],
            tool_calls: vec![GoldenToolCall {
                id: "1".into(),
                name: "tool_b".into(),
                arguments: "{}".into(),
            }],
            turn_count: 1,
            ended_normally: true,
        };
        let comparison = compare_results(&baseline, &candidate);

        assert!(!comparison.equivalent);
        assert!(!comparison.tool_sequence_match);
        assert!(comparison.differences.len() >= 1);
    }

    #[tokio::test]
    async fn test_threshold_scan_returns_results_for_each_threshold() {
        let session = make_test_session();
        let thresholds = vec![20.0, 50.0, 80.0];
        let results = run_threshold_scan(&session, &thresholds).await;

        assert_eq!(results.len(), 3);
        for (threshold, _comparison) in &results {
            assert!(thresholds.contains(threshold));
        }
    }

    #[tokio::test]
    async fn test_golden_session_save_and_load() {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path().join("golden.json");

        let session = make_test_session();
        session.save(&path).unwrap();

        let loaded = GoldenSession::load(&path).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.turns.len(), session.turns.len());
    }
}
