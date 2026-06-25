//! Phase 5 acceptance corpus for the divergence Sentinel (#613).
//!
//! Runs a blind, deterministic corpus of 20 synthetic sessions
//! (10 productive-repair + 10 genuine-divergence) through the real
//! `TrajectoryMonitor` and asserts the acceptance bar:
//!
//! - 0/10 false escalations on the repair set.
//! - 0/10 missed divergences on the divergence set.
//! - Near-zero operator notifications on the repair set.
//!
//! The fixtures are intentionally synthetic so the test is reproducible in CI
//! without relying on operator-flagged archived sessions. Real session fixtures
//! can be added later by extending `CorpusSession`.

use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::runtime::trajectory_health::TrajectoryHealth;
use autonoetic_gateway::runtime::trajectory_monitor::{ToolObservation, TrajectoryMonitor};
use autonoetic_types::config::TrajectoryConfig;
use autonoetic_types::trajectory::FeedbackEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedOutcome {
    Repair,
    Divergence,
}

/// Per-turn input to the monitor.
struct Turn {
    /// Tool observations this turn (fingerprint + failed flag).
    observations: Vec<ToolObservation>,
    /// Corrective feedback events issued to the agent this turn.
    feedback_events: Vec<FeedbackEvent>,
    /// Simulated LoopGuard state for this turn.
    guard: LoopGuard,
    /// Optional prompt-budget utilization fraction [0.0, 1.0+].
    context_utilization: Option<f32>,
}

struct CorpusSession {
    name: &'static str,
    expected: ExpectedOutcome,
    turns: Vec<Turn>,
}

struct ReplayResult {
    max_health: TrajectoryHealth,
    reached_critical: bool,
    first_critical_turn: Option<u64>,
}

fn val(rule: &'static str) -> FeedbackEvent {
    FeedbackEvent::Validation {
        rule: rule.into(),
        field_path: None,
    }
}

fn guard(current_loops: u32, tool_failures: &[(&str, u32)], child_failures: u32) -> LoopGuard {
    LoopGuard {
        max_loops_without_progress: 10,
        max_tool_failures: 8,
        max_child_failures: 5,
        current_loops,
        child_failure_count: child_failures,
        tool_failure_counts: tool_failures
            .iter()
            .map(|(t, c)| (t.to_string(), *c))
            .collect(),
        ..LoopGuard::default()
    }
}

fn fp(id: u64) -> ToolObservation {
    ToolObservation {
        fingerprint: id,
        failed: false,
    }
}

fn fp_failed(id: u64) -> ToolObservation {
    ToolObservation {
        fingerprint: id,
        failed: true,
    }
}

fn replay(session: &CorpusSession) -> ReplayResult {
    let mut monitor = TrajectoryMonitor::new(TrajectoryConfig::default());
    let mut max_health = TrajectoryHealth::Healthy;
    let mut reached_critical = false;
    let mut first_critical_turn = None;

    for (idx, turn) in session.turns.iter().enumerate() {
        let turn_counter = (idx + 1) as u64;
        monitor.record_feedback(turn_counter, &turn.feedback_events);
        let result = monitor.tick(
            turn_counter,
            &turn.observations,
            &turn.feedback_events,
            turn.context_utilization,
            &turn.guard,
        );

        if let TrajectoryHealth::Critical { .. } = &result.health {
            if first_critical_turn.is_none() {
                first_critical_turn = Some(turn_counter);
            }
            reached_critical = true;
        }

        if health_rank(&result.health) > health_rank(&max_health) {
            max_health = result.health.clone();
        }
    }

    ReplayResult {
        max_health,
        reached_critical,
        first_critical_turn,
    }
}

fn health_rank(h: &TrajectoryHealth) -> u8 {
    match h {
        TrajectoryHealth::Healthy => 0,
        TrajectoryHealth::Watching { .. } => 1,
        TrajectoryHealth::Blocked { .. } => 2,
        TrajectoryHealth::Diverging { .. } => 3,
        TrajectoryHealth::Critical { .. } => 4,
    }
}

fn corpus() -> Vec<CorpusSession> {
    vec![
        // ── Repair set (10 sessions) ─────────────────────────────────────
        CorpusSession {
            name: "feedback_incorporated_only",
            expected: ExpectedOutcome::Repair,
            turns: vec![
                Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("output_schema")],
                    guard: guard(1, &[("sandbox.exec", 1)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp_failed(2)],
                    feedback_events: vec![val("tool_schema")],
                    guard: guard(2, &[("sandbox.exec", 1), ("web.fetch", 1)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp_failed(3)],
                    feedback_events: vec![val("capability_scope")],
                    guard: guard(3, &[("web.fetch", 1)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(4)],
                    feedback_events: vec![val("format")],
                    guard: guard(1, &[], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(5)],
                    feedback_events: vec![val("structure")],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
            ],
        },
        CorpusSession {
            name: "low_loop_pressure_repair",
            expected: ExpectedOutcome::Repair,
            turns: (1..=6)
                .map(|t| Turn {
                    observations: vec![fp(t as u64)],
                    feedback_events: vec![],
                    guard: guard(t, &[], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "transient_tool_failures_then_success",
            expected: ExpectedOutcome::Repair,
            turns: vec![
                Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("sandbox_exec")],
                    guard: guard(1, &[("sandbox.exec", 1)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("retry_once")],
                    guard: guard(2, &[("sandbox.exec", 2)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("credentials")],
                    guard: guard(3, &[("sandbox.exec", 3)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(2)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(3)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
            ],
        },
        CorpusSession {
            name: "high_entropy_varied_work",
            expected: ExpectedOutcome::Repair,
            turns: (1..=8)
                .map(|t| Turn {
                    observations: vec![fp(t as u64), fp(100 + t as u64)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "digest_stall_recovers",
            expected: ExpectedOutcome::Repair,
            turns: (1..=5)
                .map(|t| Turn {
                    observations: vec![fp(1)],
                    feedback_events: vec![],
                    guard: guard(t, &[], 0),
                    context_utilization: None,
                })
                .chain((6..=8).map(|t| Turn {
                    observations: vec![fp(t as u64)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                }))
                .collect(),
        },
        CorpusSession {
            name: "context_pressure_advisory_only",
            expected: ExpectedOutcome::Repair,
            turns: (1..=5)
                .map(|_| Turn {
                    observations: vec![fp(7)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: Some(0.92),
                })
                .collect(),
        },
        CorpusSession {
            name: "repair_with_one_tool_error_burst_then_recovery",
            expected: ExpectedOutcome::Repair,
            turns: vec![
                Turn {
                    observations: vec![fp_failed(10)],
                    feedback_events: vec![val("sandbox_exec")],
                    guard: guard(1, &[("sandbox.exec", 1)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp_failed(10)],
                    feedback_events: vec![val("missing_arg")],
                    guard: guard(2, &[("sandbox.exec", 2)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp_failed(11)],
                    feedback_events: vec![val("timeout")],
                    guard: guard(3, &[("sandbox.exec", 3)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(12)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(13)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
            ],
        },
        CorpusSession {
            name: "mixed_advisory_signals_no_feedback_repeat",
            expected: ExpectedOutcome::Repair,
            turns: (1..=6)
                .map(|t| Turn {
                    observations: vec![fp(t as u64)],
                    feedback_events: vec![val(match t {
                        1 => "rule_a",
                        2 => "rule_b",
                        3 => "rule_c",
                        4 => "rule_d",
                        5 => "rule_e",
                        _ => "rule_f",
                    })],
                    guard: guard(t, &[("sandbox.exec", t)], 0),
                    context_utilization: Some(0.85),
                })
                .collect(),
        },
        CorpusSession {
            name: "feedback_acknowledged_then_progress",
            expected: ExpectedOutcome::Repair,
            turns: vec![
                Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("missing_field")],
                    guard: guard(1, &[("content.write", 1)], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(2)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(3)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(4)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
            ],
        },
        CorpusSession {
            name: "child_failure_recovers",
            expected: ExpectedOutcome::Repair,
            turns: vec![
                Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("child_failed")],
                    guard: guard(1, &[], 1),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("spawn_limit")],
                    guard: guard(2, &[], 2),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(2)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
                Turn {
                    observations: vec![fp(3)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                },
            ],
        },
        CorpusSession {
            name: "brief_loop_then_progress",
            expected: ExpectedOutcome::Repair,
            turns: (1..=4)
                .map(|t| Turn {
                    observations: vec![fp(1)],
                    feedback_events: vec![],
                    guard: guard(t, &[], 0),
                    context_utilization: None,
                })
                .chain((5..=7).map(|t| Turn {
                    observations: vec![fp(t as u64)],
                    feedback_events: vec![],
                    guard: guard(0, &[], 0),
                    context_utilization: None,
                }))
                .collect(),
        },
        // ── Divergence set (10 sessions) ─────────────────────────────────
        CorpusSession {
            name: "feedback_ignored_to_critical",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=6)
                .map(|t| Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("output_schema")],
                    guard: guard(t, &[("content.write", t.min(5))], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "loop_pressure_with_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=8)
                .map(|t| Turn {
                    observations: vec![fp(1)],
                    feedback_events: vec![val("no_progress")],
                    guard: guard(t, &[], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "failure_pressure_with_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=8)
                .map(|t| Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("sandbox_exec")],
                    guard: guard(t, &[("sandbox.exec", t)], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "repetition_entropy_plus_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=7)
                .map(|t| Turn {
                    observations: vec![fp(1), fp(1), fp(1)],
                    feedback_events: vec![val("repeated_call")],
                    guard: guard(t, &[], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "digest_stall_with_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=10)
                .map(|t| Turn {
                    observations: vec![fp(1)],
                    feedback_events: vec![val("no_new_digest")],
                    guard: guard(t, &[], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "child_failure_burst_with_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=7)
                .map(|t| Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("child_failed")],
                    guard: guard(t, &[], t),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "error_burst_with_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=10)
                .map(|t| Turn {
                    observations: vec![fp_failed(t as u64)],
                    feedback_events: vec![val("tool_error")],
                    guard: guard(t, &[("web.fetch", t)], 0),
                    context_utilization: None,
                })
                .collect(),
        },
        CorpusSession {
            name: "context_pressure_with_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=6)
                .map(|t| Turn {
                    observations: vec![fp_failed(1)],
                    feedback_events: vec![val("context_overflow")],
                    guard: guard(t, &[("sandbox.exec", t)], 0),
                    context_utilization: Some(0.97),
                })
                .collect(),
        },
        CorpusSession {
            name: "combined_signals_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=8)
                .map(|t| Turn {
                    observations: vec![fp(1), fp(1)],
                    feedback_events: vec![val("combined")],
                    guard: guard(t, &[("sandbox.exec", t)], t.min(5)),
                    context_utilization: Some(0.90),
                })
                .collect(),
        },
        CorpusSession {
            name: "escalating_ignored_feedback",
            expected: ExpectedOutcome::Divergence,
            turns: (1..=7)
                .map(|t| Turn {
                    observations: vec![fp_failed(t as u64)],
                    feedback_events: vec![val("escalating")],
                    guard: guard(t, &[("tool", t)], 0),
                    context_utilization: None,
                })
                .collect(),
        },
    ]
}

#[test]
fn divergence_sentinel_blind_corpus_meets_acceptance_bar() {
    let mut false_escalations = 0usize;
    let mut missed_divergences = 0usize;
    let mut operator_notifications_on_repair = 0usize;
    let mut detection_turns = Vec::new();

    let mut matrix = String::from("| session | expected | max level | critical? | first critical |\n|---|---|---|---|---|\n");

    for session in corpus() {
        let result = replay(&session);
        let max_level = result.max_health.level_str();
        matrix.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            session.name,
            match session.expected {
                ExpectedOutcome::Repair => "repair",
                ExpectedOutcome::Divergence => "divergence",
            },
            max_level,
            result.reached_critical,
            result
                .first_critical_turn
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ));

        match session.expected {
            ExpectedOutcome::Repair => {
                if result.reached_critical {
                    false_escalations += 1;
                    operator_notifications_on_repair += 1;
                }
            }
            ExpectedOutcome::Divergence => {
                if !result.reached_critical {
                    missed_divergences += 1;
                } else if let Some(t) = result.first_critical_turn {
                    detection_turns.push(t);
                }
            }
        }
    }

    let avg_detection_turn = if detection_turns.is_empty() {
        0.0
    } else {
        detection_turns.iter().sum::<u64>() as f64 / detection_turns.len() as f64
    };

    println!("\n{}", matrix);
    println!(
        "false_escalations={}/10, missed_divergences={}/10, operator_notifications_on_repair={}/10, avg_detection_turn={:.1}",
        false_escalations, missed_divergences, operator_notifications_on_repair, avg_detection_turn
    );

    assert_eq!(
        false_escalations, 0,
        "productive-repair sessions must never reach Critical (target 0/10 false escalations)"
    );
    assert_eq!(
        missed_divergences, 0,
        "genuine-divergence sessions must be caught (target 0/10 missed divergences)"
    );
    assert_eq!(
        operator_notifications_on_repair, 0,
        "operator notifications on repair set must be near-zero"
    );
}
