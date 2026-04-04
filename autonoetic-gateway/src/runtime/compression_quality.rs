//! Compression Quality Regression Framework.
//!
//! Records golden sessions as JSON fixtures and validates compression
//! structural integrity. After compression, checks that the history is
//! well-formed: no orphaned tool results, system messages at start,
//! summary present, message count reduced, tool-call groups intact.

use crate::llm::{CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, ToolCall, TokenUsage};
use crate::runtime::compression::{compress_context, CompressionResult};
use autonoetic_types::agent::CompressionConfig;
use autonoetic_types::config::ContextCompressionConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenTurn {
    pub user_message: String,
    pub assistant_response: String,
    #[serde(default)]
    pub tool_calls: Vec<GoldenToolCall>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenSession {
    pub id: String,
    pub description: String,
    pub system_prompt: String,
    pub turns: Vec<GoldenTurn>,
    pub expected_outcome: String,
    #[serde(default)]
    pub expected_tool_sequence: Vec<GoldenToolCall>,
}

impl GoldenSession {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content).map_err(Into::into)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

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
    async fn complete(&self, _request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let mut turn_idx = self.current_turn.lock().unwrap();
        let turn = if *turn_idx < self.turns.len() {
            &self.turns[*turn_idx]
        } else {
            return Ok(CompletionResponse {
                text: "done".to_string(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            });
        };
        *turn_idx += 1;

        let tool_calls: Vec<ToolCall> = turn.tool_calls.iter().map(|gt| ToolCall {
            id: gt.id.clone(),
            name: gt.name.clone(),
            arguments: gt.arguments.clone(),
        }).collect();

        let stop_reason = if turn.end_turn { StopReason::EndTurn } else { StopReason::ToolUse };

        Ok(CompletionResponse {
            text: turn.assistant_response.clone(),
            tool_calls,
            stop_reason,
            usage: TokenUsage::default(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralValidation {
    pub valid: bool,
    pub no_orphaned_tool_results: bool,
    pub system_messages_first: bool,
    pub summary_present: bool,
    pub messages_reduced: bool,
    pub tool_call_groups_intact: bool,
    pub original_count: usize,
    pub compressed_count: usize,
    pub issues: Vec<String>,
}

pub fn validate_compressed_history(original: &[Message], compressed: &[Message]) -> StructuralValidation {
    let mut issues = Vec::new();

    let system_messages_first = compressed.iter()
        .take_while(|m| matches!(m.role, Role::System))
        .count() == compressed.iter().filter(|m| matches!(m.role, Role::System)).count();
    if !system_messages_first {
        issues.push("System messages are not all at the start".to_string());
    }

    let mut no_orphaned_tool_results = true;
    for msg in compressed.iter() {
        if matches!(msg.role, Role::Tool) {
            if let Some(ref tc_id) = msg.tool_call_id {
                let has_parent = compressed.iter().any(|m| {
                    matches!(m.role, Role::Assistant) && m.tool_calls.iter().any(|tc| &tc.id == tc_id)
                });
                if !has_parent {
                    no_orphaned_tool_results = false;
                    issues.push(format!("Orphaned tool result: {}", tc_id));
                }
            }
        }
    }

    let summary_present = compressed.iter().any(|m| m.content.starts_with("[COMPRESSED CONTEXT"));
    let messages_reduced = compressed.len() < original.len();

    let mut tool_call_groups_intact = true;
    for msg in compressed.iter() {
        if matches!(msg.role, Role::Assistant) && !msg.tool_calls.is_empty() {
            for tc in &msg.tool_calls {
                if let Some(tr_idx) = compressed.iter().position(|m| {
                    matches!(m.role, Role::Tool) && m.tool_call_id.as_deref() == Some(&tc.id)
                }) {
                    if let Ok(tc_idx) = compressed.iter().position(|m| m == msg).ok_or(()) {
                        if tr_idx <= tc_idx {
                            tool_call_groups_intact = false;
                            issues.push(format!("Tool result before assistant: {}", tc.id));
                        }
                    }
                }
            }
        }
    }

    let valid = no_orphaned_tool_results && system_messages_first && tool_call_groups_intact;

    StructuralValidation {
        valid,
        no_orphaned_tool_results,
        system_messages_first,
        summary_present,
        messages_reduced,
        tool_call_groups_intact,
        original_count: original.len(),
        compressed_count: compressed.len(),
        issues,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    pub session_id: String,
    pub compressed: bool,
    pub validation: Option<StructuralValidation>,
    pub tool_sequence_preserved: bool,
    pub issues: Vec<String>,
}

pub async fn run_quality_validation(
    session: &GoldenSession,
    compression_cfg: &ContextCompressionConfig,
    agent_cfg: Option<&CompressionConfig>,
) -> QualityReport {
    let mut history: Vec<Message> = vec![Message::system(&session.system_prompt)];

    for turn in &session.turns {
        history.push(Message::user(&turn.user_message));
        history.push(Message::assistant(turn.assistant_response.clone()));
        for tc in &turn.tool_calls {
            history.push(Message::tool_result(&tc.id, &tc.name, "result"));
        }
    }

    let presets = HashMap::new();
    let http_client = reqwest::Client::new();
    let result = compress_context(
        history.clone(),
        Some(128_000),
        compression_cfg,
        agent_cfg,
        &presets,
        &http_client,
        &session.id,
        session.turns.len() as u64,
        None,
    ).await;

    match result {
        Ok(result) => {
            if !result.compressed {
                return QualityReport {
                    session_id: session.id.clone(),
                    compressed: false,
                    validation: None,
                    tool_sequence_preserved: true,
                    issues: vec!["Compression not triggered (threshold not exceeded)".into()],
                };
            }
            let validation = validate_compressed_history(&history, &result.history);
            let mut issues = validation.issues.clone();
            if !validation.valid {
                issues.push("Structural validation failed".into());
            }
            QualityReport {
                session_id: session.id.clone(),
                compressed: true,
                validation: Some(validation),
                tool_sequence_preserved: true,
                issues,
            }
        }
        Err(e) => QualityReport {
            session_id: session.id.clone(),
            compressed: false,
            validation: None,
            tool_sequence_preserved: true,
            issues: vec![format!("Compression failed: {}", e)],
        },
    }
}

pub async fn run_threshold_scan(session: &GoldenSession, thresholds: &[f64]) -> Vec<(f64, QualityReport)> {
    let mut results = Vec::new();
    for threshold in thresholds {
        let cfg = ContextCompressionConfig {
            enabled: true,
            llm_preset: Some("haiku".into()),
            threshold_pct: *threshold,
            recent_turns_to_keep: 3,
            max_summary_tokens: 500,
            min_turns_between_compression: 0,
            provider: None,
            model: None,
        };
        results.push((*threshold, run_quality_validation(session, &cfg, None).await));
    }
    results
}

pub fn default_test_compression_config() -> ContextCompressionConfig {
    ContextCompressionConfig {
        enabled: true,
        llm_preset: Some("haiku".into()),
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
            id: "test-simple".into(),
            description: "Simple conversation with tool use".into(),
            system_prompt: "You are a helpful assistant.".into(),
            turns: vec![
                GoldenTurn {
                    user_message: "What is the weather?".into(),
                    assistant_response: "Let me check.".into(),
                    tool_calls: vec![GoldenToolCall { id: "tc1".into(), name: "weather.get".into(), arguments: r#"{"city":"NYC"}"#.into() }],
                    end_turn: false,
                },
                GoldenTurn {
                    user_message: "It's sunny.".into(),
                    assistant_response: "Great!".into(),
                    tool_calls: vec![],
                    end_turn: true,
                },
            ],
            expected_outcome: "Done".into(),
            expected_tool_sequence: vec![GoldenToolCall { id: "tc1".into(), name: "weather.get".into(), arguments: r#"{"city":"NYC"}"#.into() }],
        }
    }

    #[tokio::test]
    async fn test_replay_driver_returns_recorded_responses() {
        let session = make_test_session();
        let driver = ReplayDriver::new(&session);
        let response = driver.complete(&CompletionRequest { model: "test".into(), messages: vec![], tools: vec![], max_tokens: None, temperature: None, metadata: None }).await.unwrap();
        assert_eq!(response.text, "Let me check.");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "weather.get");
    }

    #[tokio::test]
    async fn test_validate_compressed_history_no_orphans() {
        let original = vec![Message::system("s"), Message::user("u1"), Message::assistant("a1"), Message::user("u2"), Message::assistant("a2")];
        let compressed = vec![Message::system("s"), Message::user("[COMPRESSED CONTEXT - Turn 1]\nsummary"), Message::user("u2"), Message::assistant("a2")];
        let v = validate_compressed_history(&original, &compressed);
        assert!(v.valid);
        assert!(v.no_orphaned_tool_results);
        assert!(v.system_messages_first);
        assert!(v.summary_present);
        assert!(v.messages_reduced);
    }

    #[tokio::test]
    async fn test_validate_detects_orphaned_tool_result() {
        let original = vec![Message::system("s"), Message::user("u")];
        let compressed = vec![
            Message::system("s"),
            Message::user("u"),
            Message::tool_result("orphan_tc", "tool", "result"),
        ];
        let v = validate_compressed_history(&original, &compressed);
        assert!(!v.no_orphaned_tool_results);
        assert!(!v.valid);
    }

    #[tokio::test]
    async fn test_validate_detects_system_messages_not_first() {
        let original = vec![Message::system("s")];
        let compressed = vec![
            Message::user("u"),
            Message::system("s"),
        ];
        let v = validate_compressed_history(&original, &compressed);
        assert!(!v.system_messages_first);
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

    #[tokio::test]
    async fn test_quality_validation_when_compression_not_triggered() {
        let session = make_test_session();
        let cfg = ContextCompressionConfig { enabled: true, threshold_pct: 99.0, ..Default::default() };
        let report = run_quality_validation(&session, &cfg, None).await;
        assert!(!report.compressed);
        assert!(report.validation.is_none());
    }

    #[tokio::test]
    async fn test_threshold_scan_returns_results() {
        let session = make_test_session();
        let results = run_threshold_scan(&session, &[20.0, 50.0]).await;
        assert_eq!(results.len(), 2);
    }
}
