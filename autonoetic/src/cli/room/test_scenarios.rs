//! Synthetic event generators for Room TUI test mode (`/test <scenario>`).
//!
//! Each scenario produces a `Vec<SessionTimelineEntry>` that the TUI injects
//! directly into its `entries` vector — no gateway RPC needed. Events use
//! realistic payloads so the rendering, gate-detection, and interaction paths
//! exercise the same code as live sessions.
//!
//! **Gate resolution is simulated**: when the operator resolves a test-mode
//! gate, the TUI marks it `acted` locally without calling the gateway.

use autonoetic_types::principal::{Principal, PrincipalKind};
use autonoetic_types::session_timeline::{
    Altitude, SessionRole, SessionTimelineEntry, TimelineRefs,
};
use std::sync::atomic::{AtomicU64, Ordering};

static EV_SEQ: AtomicU64 = AtomicU64::new(10_000);

fn next_ev_id() -> String {
    format!("test-ev-{}", EV_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn next_gate_id(prefix: &str) -> String {
    format!("test-{}-{}", prefix, EV_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn agent_principal(id: &str) -> Principal {
    Principal {
        kind: PrincipalKind::AutonoeticAgent,
        id: id.to_string(),
    }
}

fn operator_principal() -> Principal {
    Principal {
        kind: PrincipalKind::Human,
        id: "operator".to_string(),
    }
}

fn entry(
    root: &str,
    event_type: &str,
    altitude: Altitude,
    payload: Option<String>,
    refs: TimelineRefs,
    principal: Principal,
    role: &str,
) -> SessionTimelineEntry {
    SessionTimelineEntry {
        event_id: next_ev_id(),
        root_session_id: root.to_string(),
        source_session_id: root.to_string(),
        turn_id: Some("test-turn-1".to_string()),
        principal,
        role: SessionRole::from_storage(role),
        event_type: event_type.to_string(),
        altitude,
        occurred_at: now_iso(),
        payload,
        refs,
    }
}

fn agent_entry(
    root: &str,
    event_type: &str,
    altitude: Altitude,
    payload: Option<String>,
    refs: TimelineRefs,
) -> SessionTimelineEntry {
    entry(
        root,
        event_type,
        altitude,
        payload,
        refs,
        agent_principal("planner.default"),
        "planner",
    )
}

fn tool_completed(root: &str, tool: &str, summary: &str) -> SessionTimelineEntry {
    agent_entry(
        root,
        "tool.completed",
        Altitude::Normal,
        Some(serde_json::json!({
            "tool_name": tool,
            "summary": summary,
        })
        .to_string()),
        TimelineRefs::default(),
    )
}

fn agent_message(root: &str, msg: &str) -> SessionTimelineEntry {
    agent_entry(
        root,
        "agent.message",
        Altitude::Normal,
        Some(serde_json::json!({ "message": msg }).to_string()),
        TimelineRefs::default(),
    )
}

fn operator_message(root: &str, msg: &str) -> SessionTimelineEntry {
    entry(
        root,
        "operator.message",
        Altitude::Normal,
        Some(serde_json::json!({ "message": msg }).to_string()),
        TimelineRefs::default(),
        operator_principal(),
        "operator",
    )
}

fn turn_start(root: &str) -> SessionTimelineEntry {
    agent_entry(
        root,
        "turn.start",
        Altitude::Detail,
        None,
        TimelineRefs::default(),
    )
}

fn turn_end(root: &str) -> SessionTimelineEntry {
    agent_entry(
        root,
        "turn.end",
        Altitude::Detail,
        None,
        TimelineRefs::default(),
    )
}

fn llm_round(root: &str, model: &str, in_t: u32, out_t: u32) -> SessionTimelineEntry {
    agent_entry(
        root,
        "llm.round",
        Altitude::Detail,
        Some(
            serde_json::json!({
                "model": model,
                "input_tokens": in_t,
                "output_tokens": out_t,
                // Plausible prompt-prefix cache hit (~70% of input) so the
                // stats panel / dividers exercise the cached display.
                "cached_tokens": in_t * 7 / 10,
            })
            .to_string(),
        ),
        TimelineRefs::default(),
    )
}

// ── Scenario definitions ──────────────────────────────────────────────

/// Available test scenarios. The name is the `/test <name>` key.
pub const SCENARIOS: &[(&str, &str)] = &[
    ("user-ask", "user.ask with options (clarification)"),
    ("user-decision", "user.ask decision with freeform"),
    ("user-confirmation", "user.ask yes/no confirmation"),
    ("approval", "approval.pending for sandbox exec"),
    ("approval-cycle", "approval → approved full cycle"),
    ("approval-reject", "approval → rejected"),
    ("approval-notification", "operator.message notification with approval ref"),
    ("escalation", "escalation.pending (promotion)"),
    ("divergence-watch", "divergence: watching level"),
    ("divergence-critical", "divergence: critical + user.ask"),
    ("llm-error", "LLM request failed"),
    ("llm-empty", "LLM empty response (0 tokens)"),
    ("promotion-pass", "promotion verdict: pass"),
    ("promotion-fail", "promotion verdict: fail (critical)"),
    ("emergency-stop", "emergency stop"),
    ("guard-tripped", "loop guard tripped"),
    ("plan-propose", "plan.pending with structured steps"),
    ("plan-approved", "plan.approved full cycle"),
    ("workbench", "workbench created → reconciled lifecycle"),
    ("collaborative-session", "plan + workbench + delegation full session"),
    ("full-session", "complete session lifecycle"),
    ("help", "list available scenarios"),
];

pub fn scenario_help() -> String {
    let mut lines = vec!["Test scenarios:".to_string(), String::new()];
    for (name, desc) in SCENARIOS {
        lines.push(format!("  /test {name:<22} {desc}"));
    }
    lines.push(String::new());
    lines.push("Gates in test mode are resolved locally (no gateway call).".to_string());
    lines.join("\n")
}

pub fn run(name: &str, root: &str) -> Option<Vec<SessionTimelineEntry>> {
    match name {
        "user-ask" => Some(scenario_user_ask(root)),
        "user-decision" => Some(scenario_user_decision(root)),
        "user-confirmation" => Some(scenario_user_confirmation(root)),
        "approval" => Some(scenario_approval(root)),
        "approval-cycle" => Some(scenario_approval_cycle(root)),
        "approval-reject" => Some(scenario_approval_reject(root)),
        "approval-notification" => Some(scenario_approval_notification(root)),
        "escalation" => Some(scenario_escalation(root)),
        "divergence-watch" => Some(scenario_divergence_watch(root)),
        "divergence-critical" => Some(scenario_divergence_critical(root)),
        "llm-error" => Some(scenario_llm_error(root)),
        "llm-empty" => Some(scenario_llm_empty(root)),
        "promotion-pass" => Some(scenario_promotion_pass(root)),
        "promotion-fail" => Some(scenario_promotion_fail(root)),
        "emergency-stop" => Some(scenario_emergency_stop(root)),
        "guard-tripped" => Some(scenario_guard_tripped(root)),
        "plan-propose" => Some(scenario_plan_propose(root)),
        "plan-approved" => Some(scenario_plan_approved(root)),
        "workbench" => Some(scenario_workbench(root)),
        "collaborative-session" => Some(scenario_collaborative_session(root)),
        "full-session" => Some(scenario_full_session(root)),
        _ => None,
    }
}

/// Follow-up events injected when a test gate is resolved locally.
///
/// `gate_id` starts with `test-`. `action` is the gate action taken.
/// `answer` is the option label chosen (if any) or freeform text.
/// `root` is the root session id for the synthetic events.
pub fn resolve_followup(
    gate_id: &str,
    action_approve: bool,
    answer: Option<&str>,
    root: &str,
) -> Vec<SessionTimelineEntry> {
    if gate_id.starts_with("test-int-") {
        resolve_interaction_followup(gate_id, answer, root)
    } else if gate_id.starts_with("test-apr-") {
        resolve_approval_followup(gate_id, action_approve, root)
    } else {
        vec![]
    }
}

fn resolve_interaction_followup(
    _interaction_id: &str,
    answer: Option<&str>,
    root: &str,
) -> Vec<SessionTimelineEntry> {
    let answer_text = answer.unwrap_or("(no answer captured)");
    vec![
        operator_message(root, answer_text),
        llm_round(root, "claude-sonnet-4-20250514", 800, 120),
        agent_message(
            root,
            &format!(
                "Got it, thanks! I'll proceed with: {answer_text}. Continuing the task now."
            ),
        ),
        tool_completed(root, "sandbox.exec", "Task step executed successfully"),
        agent_message(root, "Step complete. Moving to the next phase."),
        turn_end(root),
    ]
}

fn resolve_approval_followup(
    request_id: &str,
    approved: bool,
    root: &str,
) -> Vec<SessionTimelineEntry> {
    if approved {
        vec![
            agent_entry(
                root,
                "approval.approved",
                Altitude::Normal,
                Some(
                    serde_json::json!({
                        "request_id": request_id,
                        "decided_by": "operator",
                        "reason": "Approved (test mode)",
                    })
                    .to_string(),
                ),
                TimelineRefs {
                    approval_request_id: Some(request_id.to_string()),
                    ..Default::default()
                },
            ),
            tool_completed(root, "sandbox.exec", "Command executed in sandbox (test mode)"),
            agent_message(root, "Sandbox execution complete. Proceeding."),
            turn_end(root),
        ]
    } else {
        vec![
            agent_entry(
                root,
                "approval.rejected",
                Altitude::Normal,
                Some(
                    serde_json::json!({
                        "request_id": request_id,
                        "decided_by": "operator",
                        "reason": "Rejected (test mode)",
                    })
                    .to_string(),
                ),
                TimelineRefs {
                    approval_request_id: Some(request_id.to_string()),
                    ..Default::default()
                },
            ),
            agent_message(root, "Understood, I'll take an alternative approach."),
            turn_end(root),
        ]
    }
}

// ── Scenario implementations ──────────────────────────────────────────

fn scenario_user_ask(root: &str) -> Vec<SessionTimelineEntry> {
    let interaction_id = next_gate_id("int");
    vec![
        turn_start(root),
        agent_message(root, "I need to clarify the approach before proceeding."),
        llm_round(root, "claude-sonnet-4-20250514", 1200, 85),
        agent_entry(
            root,
            "user.ask.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "interaction_id": interaction_id,
                    "question": "Which database schema should I use for the new auth module?",
                    "options": [
                        {"id": "opt-a", "label": "Flat table with JSON columns"},
                        {"id": "opt-b", "label": "Normalized relational (3NF)"},
                        {"id": "opt-c", "label": "Hybrid (relational core + JSON extensions)"},
                    ],
                    "options_count": 3,
                    "allow_freeform": true,
                })
                .to_string(),
            ),
            TimelineRefs {
                interaction_id: Some(interaction_id.to_string()),
                ..Default::default()
            },
        ),
    ]
}

fn scenario_user_decision(root: &str) -> Vec<SessionTimelineEntry> {
    let interaction_id = next_gate_id("int");
    vec![
        turn_start(root),
        agent_message(root, "Both approaches have trade-offs. I need a decision."),
        llm_round(root, "claude-sonnet-4-20250514", 3400, 210),
        tool_completed(root, "architect.analyze", "Analyzed 2 architectural options"),
        agent_entry(
            root,
            "user.ask.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "interaction_id": interaction_id,
                    "question": "Should we use microservices or a monolith for this project?",
                    "options": [
                        {"id": "opt-micro", "label": "Microservices (scalable, complex)"},
                        {"id": "opt-mono", "label": "Monolith (simpler, less scalable)"},
                    ],
                    "options_count": 2,
                    "allow_freeform": true,
                })
                .to_string(),
            ),
            TimelineRefs {
                interaction_id: Some(interaction_id.to_string()),
                ..Default::default()
            },
        ),
    ]
}

fn scenario_user_confirmation(root: &str) -> Vec<SessionTimelineEntry> {
    let interaction_id = next_gate_id("int");
    vec![
        turn_start(root),
        agent_message(root, "I'm about to drop the staging database table. Please confirm."),
        agent_entry(
            root,
            "user.ask.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "interaction_id": interaction_id,
                    "question": "Confirm: DROP TABLE staging.users (irreversible)?",
                    "options": [
                        {"id": "opt-yes", "label": "Yes, drop it"},
                        {"id": "opt-no", "label": "No, keep it"},
                    ],
                    "options_count": 2,
                    "allow_freeform": false,
                })
                .to_string(),
            ),
            TimelineRefs {
                interaction_id: Some(interaction_id.to_string()),
                ..Default::default()
            },
        ),
    ]
}

fn scenario_approval(root: &str) -> Vec<SessionTimelineEntry> {
    let request_id = next_gate_id("apr");
    vec![
        turn_start(root),
        agent_message(root, "I need to run the test suite in the sandbox."),
        llm_round(root, "claude-sonnet-4-20250514", 800, 65),
        agent_entry(
            root,
            "tool.requested",
            Altitude::Detail,
            Some(
                serde_json::json!({
                    "tool_name": "sandbox.exec",
                    "args": {"command": "cargo test --all"},
                })
                .to_string(),
            ),
            TimelineRefs::default(),
        ),
        agent_entry(
            root,
            "approval.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "request_id": request_id,
                    "approval_level": "operator",
                    "action": "sandbox_exec",
                    "command": "cargo test --all",
                    "host_patterns": ["api.github.com"],
                    "risk_summary": "Runs arbitrary commands in sandbox; requests network access to github.com",
                })
                .to_string(),
            ),
            TimelineRefs {
                approval_request_id: Some(request_id.to_string()),
                ..Default::default()
            },
        ),
    ]
}

fn scenario_approval_cycle(root: &str) -> Vec<SessionTimelineEntry> {
    let request_id = next_gate_id("apr");
    let mut evts = scenario_with_approval(root, &request_id, "cargo build");
    evts.push(agent_entry(
        root,
        "approval.approved",
        Altitude::Normal,
        Some(
            serde_json::json!({
                "request_id": request_id,
                "decided_by": "operator",
                "reason": "Approved for testing",
            })
            .to_string(),
        ),
        TimelineRefs {
            approval_request_id: Some(request_id.to_string()),
            ..Default::default()
        },
    ));
    evts.push(tool_completed(
        root,
        "sandbox.exec",
        "Build succeeded in 12.3s",
    ));
    evts.push(turn_end(root));
    evts
}

fn scenario_approval_reject(root: &str) -> Vec<SessionTimelineEntry> {
    let request_id = next_gate_id("apr");
    let mut evts = scenario_with_approval(root, &request_id, "rm -rf /tmp/test");
    evts.push(agent_entry(
        root,
        "approval.rejected",
        Altitude::Normal,
        Some(
            serde_json::json!({
                "request_id": request_id,
                "decided_by": "operator",
                "reason": "Destructive command rejected",
            })
            .to_string(),
        ),
        TimelineRefs {
            approval_request_id: Some(request_id.to_string()),
            ..Default::default()
        },
    ));
    evts.push(agent_message(
        root,
        "Understood. I'll use a safer cleanup approach instead.",
    ));
    evts.push(turn_end(root));
    evts
}

fn scenario_with_approval(root: &str, request_id: &str, cmd: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, &format!("Requesting approval to execute: {cmd}")),
        llm_round(root, "claude-sonnet-4-20250514", 600, 45),
        agent_entry(
            root,
            "approval.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "request_id": request_id,
                    "approval_level": "operator",
                    "action": "sandbox_exec",
                    "command": cmd,
                    "risk_summary": "Sandbox command execution",
                })
                .to_string(),
            ),
            TimelineRefs {
                approval_request_id: Some(request_id.to_string()),
                ..Default::default()
            },
        ),
    ]
}

fn scenario_approval_notification(root: &str) -> Vec<SessionTimelineEntry> {
    let apr_id = next_gate_id("apr");
    let task_id = "task-b2f67266";
    let workflow_id = "wf-08997c6b";
    let child_id = format!(
        "session-fcfcfe87/agent-factory.default-ac286223/specialized_builder.default-{}",
        &apr_id[apr_id.len() - 8..]
    );
    let notification = serde_json::json!({
        "child_session_id": child_id,
        "child_status": "awaiting_approval",
        "approval_request_id": apr_id,
        "task_id": task_id,
        "workflow_id": workflow_id,
    });
    vec![
        turn_start(root),
        agent_message(root, "Spawning specialized builder for the refactoring task."),
        llm_round(root, "claude-sonnet-4-20250514", 800, 65),
        tool_completed(root, "agent_spawn", &format!("spawned specialized_builder for task {task_id}")),
        entry(
            root,
            "operator.message",
            Altitude::Attention,
            Some(serde_json::json!({
                "message": serde_json::to_string(&serde_json::json!({
                    "message": format!("Workflow child '{task_id}' changed state to 'awaiting_approval'."),
                    "notification": notification,
                    "type": "child_state_notification",
                })).unwrap(),
            }).to_string()),
            TimelineRefs::default(),
            operator_principal(),
            "operator",
        ),
    ]
}

fn scenario_escalation(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Evaluation complete. Recommending promotion."),
        llm_round(root, "claude-sonnet-4-20250514", 2800, 340),
        tool_completed(
            root,
            "sealed_evaluation.evaluate",
            "All checks passed, recommending promote",
        ),
        entry(
            root,
            "escalation.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "escalation_id": "test-esc-1",
                    "agent_id": "sealed_evaluator.default",
                    "revision_id": "rev-42",
                    "synthesis": "Sealed evaluation passed all gates. Artifact integrity verified. Recommending promotion to main.",
                    "escalation_type": "promotion",
                })
                .to_string(),
            ),
            TimelineRefs {
                artifact_id: Some("art-sha256-aabbccdd".to_string()),
                ..Default::default()
            },
            agent_principal("sealed_evaluator.default"),
            "specialist:sealed_evaluator",
        ),
    ]
}

fn scenario_divergence_watch(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Let me refactor the authentication module."),
        llm_round(root, "claude-sonnet-4-20250514", 1500, 180),
        tool_completed(root, "sandbox.exec", "Ran sed commands on auth.rs"),
        llm_round(root, "claude-sonnet-4-20250514", 2000, 220),
        tool_completed(root, "sandbox.exec", "Ran more sed commands on auth.rs"),
        llm_round(root, "claude-sonnet-4-20250514", 1800, 195),
        tool_completed(root, "sandbox.exec", "Yet more sed commands"),
        entry(
            root,
            "divergence.intervention",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "level": "watching",
                    "turn": 3,
                    "indicator": "repeated sed without verification",
                    "recommendation": "Run tests to verify changes",
                })
                .to_string(),
            ),
            TimelineRefs {
                enforced_rules: vec!["P-7.19".to_string()],
                ..Default::default()
            },
            agent_principal("sentinel.divergence"),
            "sentinel",
        ),
    ]
}

fn scenario_divergence_critical(root: &str) -> Vec<SessionTimelineEntry> {
    let interaction_id = next_gate_id("int");
    let mut evts = scenario_divergence_watch(root);
    evts.push(entry(
        root,
        "divergence.intervention",
        Altitude::Attention,
        Some(
            serde_json::json!({
                "level": "critical",
                "turn": 5,
                "indicator": "agent has made 8 consecutive file edits without any test verification",
            })
            .to_string(),
        ),
        TimelineRefs {
            enforced_rules: vec!["Ri-0.9".to_string()],
            ..Default::default()
        },
        agent_principal("sentinel.divergence"),
        "sentinel",
    ));
    evts.push(agent_entry(
        root,
        "user.ask.pending",
        Altitude::Attention,
        Some(
            serde_json::json!({
                "interaction_id": interaction_id,
                "question": "Critical divergence detected: 8 edits without verification. How should I proceed?",
                "options": [
                    {"id": "opt-ack", "label": "Acknowledge and continue"},
                    {"id": "opt-test", "label": "Force test run before continuing"},
                    {"id": "opt-stop", "label": "Stop and revert changes"},
                ],
                "options_count": 3,
                "allow_freeform": false,
            })
            .to_string(),
        ),
        TimelineRefs {
            interaction_id: Some(interaction_id.to_string()),
            ..Default::default()
        },
    ));
    evts
}

fn scenario_llm_error(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Let me analyze the error patterns."),
        llm_round(root, "claude-sonnet-4-20250514", 900, 120),
        tool_completed(root, "researcher.search", "Found 5 related issues"),
        agent_entry(
            root,
            "llm.request_failed",
            Altitude::Error,
            Some(
                serde_json::json!({
                    "error": "API error 502: Bad Gateway from upstream provider",
                    "model": "nvidia/nemotron-3-ultra-550b-a55b:free",
                    "provider": "openrouter",
                    "attempt": 3,
                    "max_retries": 3,
                })
                .to_string(),
            ),
            TimelineRefs::default(),
        ),
        agent_message(root, "The model failed after 3 retries. Switching to fallback."),
        llm_round(root, "claude-sonnet-4-20250514", 900, 95),
        turn_end(root),
    ]
}

fn scenario_llm_empty(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Generating the implementation plan."),
        agent_entry(
            root,
            "llm.empty_response",
            Altitude::Error,
            Some(
                serde_json::json!({
                    "model": "nvidia/nemotron-3-ultra-550b-a55b:free",
                    "stop_reason": "end_turn",
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "provider": "openrouter",
                })
                .to_string(),
            ),
            TimelineRefs::default(),
        ),
        agent_message(
            root,
            "Received empty response from model. Retrying with fallback provider.",
        ),
        llm_round(root, "claude-sonnet-4-20250514", 1200, 250),
        turn_end(root),
    ]
}

fn scenario_promotion_pass(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Running sealed evaluation on the artifact."),
        llm_round(root, "claude-sonnet-4-20250514", 3200, 450),
        tool_completed(
            root,
            "sandbox.exec",
            "cargo test — 47 passed, 0 failed",
        ),
        tool_completed(
            root,
            "sandbox.exec",
            "cargo clippy — 0 warnings",
        ),
        tool_completed(
            root,
            "sealed_evaluation.evaluate",
            "PASS: all gates satisfied",
        ),
        entry(
            root,
            "tool.completed",
            Altitude::Normal,
            Some(
                serde_json::json!({
                    "tool_name": "promotion.record",
                    "summary": "PASS — sealed evaluation complete",
                    "result": {
                        "artifact_id": "art-sha256-eeff0011",
                        "role": "sealed_evaluator",
                        "pass": true,
                        "findings": [
                            {"severity": "info", "description": "All unit tests passed (47/47)"},
                            {"severity": "info", "description": "No clippy warnings"},
                            {"severity": "warning", "description": "Test coverage at 78%, below 80% target", "evidence": "coverage report: src/auth.rs lines 45-67 uncovered"},
                        ],
                    },
                })
                .to_string(),
            ),
            TimelineRefs {
                artifact_id: Some("art-sha256-eeff0011".to_string()),
                ..Default::default()
            },
            agent_principal("sealed_evaluator.default"),
            "specialist:sealed_evaluator",
        ),
        turn_end(root),
    ]
}

fn scenario_promotion_fail(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Running sealed evaluation."),
        llm_round(root, "claude-sonnet-4-20250514", 3200, 450),
        tool_completed(
            root,
            "sandbox.exec",
            "cargo test — 44 passed, 3 failed",
        ),
        entry(
            root,
            "tool.completed",
            Altitude::Normal,
            Some(
                serde_json::json!({
                    "tool_name": "promotion.record",
                    "summary": "FAIL — 3 critical test failures",
                    "result": {
                        "artifact_id": "art-sha256-22334455",
                        "role": "sealed_evaluator",
                        "pass": false,
                        "findings": [
                            {"severity": "critical", "description": "test_auth_login_returns_401_for_invalid_credentials FAILED"},
                            {"severity": "critical", "description": "test_session_expiry_cleans_up_tokens FAILED"},
                            {"severity": "critical", "description": "test_rate_limiter_blocks_after_threshold FAILED"},
                            {"severity": "warning", "description": "Coverage at 62%"},
                        ],
                    },
                })
                .to_string(),
            ),
            TimelineRefs {
                artifact_id: Some("art-sha256-22334455".to_string()),
                ..Default::default()
            },
            agent_principal("sealed_evaluator.default"),
            "specialist:sealed_evaluator",
        ),
        agent_message(
            root,
            "Promotion blocked: 3 critical test failures detected. Will fix and re-evaluate.",
        ),
        turn_end(root),
    ]
}

fn scenario_emergency_stop(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Starting the deployment pipeline."),
        llm_round(root, "claude-sonnet-4-20250514", 1000, 80),
        tool_completed(root, "sandbox.exec", "Deploying to production..."),
        entry(
            root,
            "session.emergency_stop",
            Altitude::Error,
            Some(
                serde_json::json!({
                    "reason": "Operator triggered emergency stop: unauthorized production access",
                    "triggered_by": "operator",
                    "sessions_terminated": 2,
                })
                .to_string(),
            ),
            TimelineRefs::default(),
            operator_principal(),
            "operator",
        ),
    ]
}

fn scenario_guard_tripped(root: &str) -> Vec<SessionTimelineEntry> {
    vec![
        turn_start(root),
        agent_message(root, "Let me fix the failing tests."),
    ]
    .into_iter()
    .chain((0..5).flat_map(|i| {
        let _tool = format!("sandbox.exec (attempt {}/{})", i + 1, 5);
        vec![
            llm_round(root, "claude-sonnet-4-20250514", 800 + i * 100, 60 + i * 10),
            agent_entry(
                root,
                "tool.failed",
                Altitude::Error,
                Some(
                    serde_json::json!({
                        "tool_name": "sandbox.exec",
                        "error": format!("Command exited with code 1: cargo test -- test_auth_{i}"),
                    })
                    .to_string(),
                ),
                TimelineRefs::default(),
            ),
        ]
    }))
    .chain(std::iter::once(entry(
        root,
        "guard.tripped",
        Altitude::Error,
        Some(
            serde_json::json!({
                "reason": "max_tool_failures exceeded for sandbox.exec (5/5)",
                "rule_id": "max_tool_failures",
                "tool_name": "sandbox.exec",
                "failure_count": 5,
                "threshold": 5,
            })
            .to_string(),
        ),
        TimelineRefs {
            enforced_rules: vec!["Ri-0.9".to_string()],
            ..Default::default()
        },
        agent_principal("autonoetic"),
        "system",
    )))
    .collect()
}

fn scenario_plan_propose(root: &str) -> Vec<SessionTimelineEntry> {
    let plan_id = next_gate_id("pln");
    let plan = serde_json::json!({
        "plan_id": plan_id,
        "version": 1,
        "title": "Add rate limiter to /api/echo endpoint",
        "objective": "Throttle per-IP request rate on /api/echo to prevent abuse while preserving legitimate traffic.",
        "steps": [
            {"step_id": "s1", "title": "Design rate-limit algorithm", "owner": "agent", "agent_id": "architect.default", "depends_on": []},
            {"step_id": "s2", "title": "Implement middleware", "owner": "agent", "agent_id": "coder.default", "depends_on": ["s1"]},
            {"step_id": "s3", "title": "Add unit tests", "owner": "agent", "agent_id": "unit_test_runner.default", "depends_on": ["s2"]},
            {"step_id": "s4", "title": "Security review", "owner": "agent", "agent_id": "auditor.default", "depends_on": ["s2"]},
        ],
        "validation_policy": {
            "entries": [
                {"step_id": "s3", "validator": "unit_test_runner.default", "min_pass_rate": 1.0},
                {"step_id": "s4", "validator": "auditor.default", "blocking_findings_severity": ["critical"]},
            ]
        }
    });
    vec![
        turn_start(root),
        agent_message(
            root,
            "Before I start building, let me propose a structured plan. This is a 4-step plan with distinct validators.",
        ),
        llm_round(root, "claude-sonnet-4-20250514", 2400, 320),
        tool_completed(
            root,
            "planframe_propose",
            &format!("PlanFrame {plan_id} proposed (4 steps)"),
        ),
        agent_entry(
            root,
            "plan.pending",
            Altitude::Attention,
            Some(plan.to_string()),
            TimelineRefs {
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
        ),
    ]
}

fn scenario_plan_approved(root: &str) -> Vec<SessionTimelineEntry> {
    let plan_id = next_gate_id("pln");
    let mut evts = scenario_plan_propose(root);
    evts[4] = {
        let mut e = agent_entry(
            root,
            "plan.pending",
            Altitude::Attention,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "version": 1,
                "title": "Refactor auth module into separate service",
            }).to_string()),
            TimelineRefs {
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
        );
        e.event_id = format!("test-ev-{}-override", EV_SEQ.fetch_add(1, Ordering::Relaxed));
        e
    };
    evts.push(operator_message(root, "Plan looks good. Approving to start execution."));
    evts.push(agent_entry(
        root,
        "plan.approved",
        Altitude::Normal,
        Some(serde_json::json!({
            "plan_id": plan_id,
            "version": 1,
            "approved_by": "operator",
        }).to_string()),
        TimelineRefs {
            plan_id: Some(plan_id.clone()),
            ..Default::default()
        },
    ));
    evts.push(agent_message(
        root,
        "Plan approved. Starting step s1 — delegating to architect.",
    ));
    evts.push(tool_completed(root, "agent_spawn", "spawned architect.default for s1"));
    evts.push(llm_round(root, "claude-sonnet-4-20250514", 1200, 180));
    evts.push(tool_completed(
        root,
        "planframe_amend",
        &format!("PlanFrame {plan_id} amended: s1 marked complete"),
    ));
    evts.push(turn_end(root));
    evts
}

fn scenario_workbench(root: &str) -> Vec<SessionTimelineEntry> {
    let workbench_id = next_gate_id("wb");
    let plan_id = next_gate_id("pln");
    vec![
        turn_start(root),
        agent_message(
            root,
            "Projecting the auth module artifact into a workbench for the operator to review before promotion.",
        ),
        llm_round(root, "claude-sonnet-4-20250514", 1600, 220),
        tool_completed(
            root,
            "planframe_propose",
            &format!("PlanFrame {plan_id} proposed (1 step)"),
        ),
        agent_entry(
            root,
            "plan.pending",
            Altitude::Attention,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "version": 1,
                "title": "Project auth module for operator review",
            }).to_string()),
            TimelineRefs {
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
        ),
        operator_message(root, "Approved — project the workbench."),
        agent_entry(
            root,
            "plan.approved",
            Altitude::Normal,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "version": 1,
                "approved_by": "operator",
            }).to_string()),
            TimelineRefs {
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
        ),
        tool_completed(
            root,
            "artifact_project",
            &format!("Workbench {workbench_id} projected (3 files)"),
        ),
        entry(
            root,
            "workbench.created",
            Altitude::Detail,
            Some(serde_json::json!({ "workbench_id": workbench_id }).to_string()),
            TimelineRefs {
                workbench_id: Some(workbench_id.clone()),
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
            agent_principal("planner.collaborative"),
            "planner",
        ),
        agent_message(
            root,
            "Workbench projected. The operator can now review and edit the files before reconciliation.",
        ),
        operator_message(
            root,
            "Reviewed the changes. Looks correct — reconciling to make the revisions permanent.",
        ),
        entry(
            root,
            "workbench.reconciled",
            Altitude::Detail,
            Some(serde_json::json!({ "workbench_id": workbench_id }).to_string()),
            TimelineRefs {
                workbench_id: Some(workbench_id.clone()),
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
            operator_principal(),
            "operator",
        ),
        agent_message(root, "Reconciled. The session can now proceed to promotion."),
        turn_end(root),
    ]
}

fn scenario_collaborative_session(root: &str) -> Vec<SessionTimelineEntry> {
    let plan_id = next_gate_id("pln");
    let workbench_id = next_gate_id("wb");
    let interaction_id = next_gate_id("int");
    vec![
        turn_start(root),
        agent_message(
            root,
            "Let me design the new feature, but first I want to clarify a constraint before committing to a plan.",
        ),
        llm_round(root, "claude-sonnet-4-20250514", 1800, 210),
        tool_completed(
            root,
            "researcher.search",
            "Found 3 existing rate-limiter libraries compatible with the current stack",
        ),
        agent_entry(
            root,
            "user.ask.pending",
            Altitude::Attention,
            Some(serde_json::json!({
                "interaction_id": interaction_id,
                "question": "The research found 3 viable rate-limiter libraries. Which should I standardize on?",
                "options": [
                    {"id": "opt-redis", "label": "Redis-based (distributed, requires Redis)"},
                    {"id": "opt-mem", "label": "In-memory (per-instance, no external deps)"},
                    {"id": "opt-token", "label": "Token bucket algorithm (custom impl, lightweight)"},
                ],
                "options_count": 3,
                "allow_freeform": true,
            }).to_string()),
            TimelineRefs {
                interaction_id: Some(interaction_id.clone()),
                ..Default::default()
            },
        ),
        operator_message(root, "Use Redis — we already run a Redis cluster for sessions."),
        llm_round(root, "claude-sonnet-4-20250514", 1500, 240),
        agent_message(root, "Good. Now I have a clear plan to propose."),
        tool_completed(
            root,
            "planframe_propose",
            &format!("PlanFrame {plan_id} proposed (4 steps)"),
        ),
        agent_entry(
            root,
            "plan.pending",
            Altitude::Attention,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "version": 1,
                "title": "Add Redis-backed rate limiter to /api/echo",
                "objective": "Throttle /api/echo per-IP using Redis-backed sliding window.",
                "steps": [
                    {"step_id": "s1", "title": "Architect Redis schema", "owner": "agent", "agent_id": "architect.default"},
                    {"step_id": "s2", "title": "Implement middleware", "owner": "agent", "agent_id": "coder.default", "depends_on": ["s1"]},
                    {"step_id": "s3", "title": "Write unit tests", "owner": "agent", "agent_id": "unit_test_runner.default", "depends_on": ["s2"]},
                    {"step_id": "s4", "title": "Security review", "owner": "agent", "agent_id": "auditor.default", "depends_on": ["s2"]},
                ],
            }).to_string()),
            TimelineRefs {
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
        ),
        operator_message(root, "Plan approved."),
        agent_entry(
            root,
            "plan.approved",
            Altitude::Normal,
            Some(serde_json::json!({
                "plan_id": plan_id,
                "version": 1,
                "approved_by": "operator",
            }).to_string()),
            TimelineRefs {
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
        ),
        tool_completed(
            root,
            "agent_spawn",
            "spawned architect.default for s1",
        ),
        tool_completed(
            root,
            "agent_spawn",
            "spawned coder.default for s2",
        ),
        tool_completed(
            root,
            "planframe_amend",
            &format!("PlanFrame {plan_id} amended: s1+s2 complete"),
        ),
        tool_completed(
            root,
            "artifact_project",
            &format!("Workbench {workbench_id} projected (5 files)"),
        ),
        entry(
            root,
            "workbench.created",
            Altitude::Detail,
            Some(serde_json::json!({ "workbench_id": workbench_id }).to_string()),
            TimelineRefs {
                workbench_id: Some(workbench_id.clone()),
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
            agent_principal("planner.collaborative"),
            "planner",
        ),
        agent_message(
            root,
            "The implementation is in the workbench. Please review and reconcile when ready — I'll resume with the changes applied.",
        ),
        operator_message(
            root,
            "Reviewed. Two small fixes applied in the workbench. Reconciling now.",
        ),
        entry(
            root,
            "workbench.reconciled",
            Altitude::Detail,
            Some(serde_json::json!({ "workbench_id": workbench_id }).to_string()),
            TimelineRefs {
                workbench_id: Some(workbench_id.clone()),
                plan_id: Some(plan_id.clone()),
                ..Default::default()
            },
            operator_principal(),
            "operator",
        ),
        llm_round(root, "claude-sonnet-4-20250514", 2200, 380),
        agent_message(
            root,
            "Reconciled with operator edits. Running sealed evaluation before promotion.",
        ),
        tool_completed(
            root,
            "sealed_evaluation.evaluate",
            "PASS: all gates satisfied",
        ),
        agent_message(
            root,
            "All checks pass. The feature is ready for production.",
        ),
        turn_end(root),
    ]
}

fn scenario_full_session(root: &str) -> Vec<SessionTimelineEntry> {
    let interaction_id = next_gate_id("int");
    let request_id = next_gate_id("apr");
    vec![
        turn_start(root),
        agent_message(
            root,
            "I'll implement the authentication module. Let me start by researching the requirements.",
        ),
        llm_round(root, "claude-sonnet-4-20250514", 2000, 300),
        tool_completed(
            root,
            "researcher.search",
            "Found auth best practices and OWASP guidelines",
        ),
        agent_message(root, "I have a question about the preferred approach."),
        agent_entry(
            root,
            "user.ask.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "interaction_id": interaction_id,
                    "question": "Should I use JWT or session-based authentication?",
                    "options": [
                        {"id": "opt-jwt", "label": "JWT (stateless, scalable)"},
                        {"id": "opt-session", "label": "Session-based (stateful, simpler)"},
                    ],
                    "options_count": 2,
                    "allow_freeform": true,
                })
                .to_string(),
            ),
            TimelineRefs {
                interaction_id: Some(interaction_id.to_string()),
                ..Default::default()
            },
        ),
        operator_message(root, "Let's go with JWT — we need stateless scaling."),
        llm_round(root, "claude-sonnet-4-20250514", 1500, 250),
        agent_message(root, "Good choice. Implementing JWT auth now. I'll need sandbox access."),
        llm_round(root, "claude-sonnet-4-20250514", 1800, 180),
        agent_entry(
            root,
            "approval.pending",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "request_id": request_id,
                    "approval_level": "operator",
                    "action": "sandbox_exec",
                    "command": "cargo build && cargo test",
                    "risk_summary": "Build and test in sandbox",
                })
                .to_string(),
            ),
            TimelineRefs {
                approval_request_id: Some(request_id.to_string()),
                ..Default::default()
            },
        ),
        operator_message(root, "Approved."),
        agent_entry(
            root,
            "approval.approved",
            Altitude::Normal,
            Some(
                serde_json::json!({
                    "request_id": request_id,
                    "decided_by": "operator",
                })
                .to_string(),
            ),
            TimelineRefs {
                approval_request_id: Some(request_id.to_string()),
                ..Default::default()
            },
        ),
        tool_completed(root, "sandbox.exec", "Build succeeded in 8.2s"),
        tool_completed(
            root,
            "sandbox.exec",
            "Tests: 12 passed, 0 failed",
        ),
        entry(
            root,
            "divergence.intervention",
            Altitude::Attention,
            Some(
                serde_json::json!({
                    "level": "watching",
                    "turn": 1,
                    "indicator": "normal progress, monitoring",
                })
                .to_string(),
            ),
            TimelineRefs::default(),
            agent_principal("sentinel.divergence"),
            "sentinel",
        ),
        llm_round(root, "claude-sonnet-4-20250514", 2200, 350),
        tool_completed(
            root,
            "sealed_evaluation.evaluate",
            "PASS: all gates satisfied",
        ),
        entry(
            root,
            "tool.completed",
            Altitude::Normal,
            Some(
                serde_json::json!({
                    "tool_name": "promotion.record",
                    "summary": "PASS — sealed evaluation complete",
                    "result": {
                        "artifact_id": "art-sha256-deadbeef",
                        "role": "sealed_evaluator",
                        "pass": true,
                        "findings": [
                            {"severity": "info", "description": "All tests passed"},
                        ],
                    },
                })
                .to_string(),
            ),
            TimelineRefs {
                artifact_id: Some("art-sha256-deadbeef".to_string()),
                ..Default::default()
            },
            agent_principal("sealed_evaluator.default"),
            "specialist:sealed_evaluator",
        ),
        agent_message(root, "Authentication module complete. All tests passing, promotion recorded."),
        turn_end(root),
    ]
}
