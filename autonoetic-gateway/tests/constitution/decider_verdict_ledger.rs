//! #1198: waking the seat and capturing the verdict — the advisory ledger.
//!
//! Phase 1 of the decider-appointment umbrella (#1191). #1197 decided *that*
//! a gate reaches a seat; this suite pins what happens next: the seat is
//! woken with the gate card, bounded, and its verdict — when it gives one —
//! lands structurally on the routing row. Every other outcome parks the gate
//! for the operator with the row's verdict left null.
//!
//! The two invariants the whole suite protects:
//!
//! 1. **A terminal advisory verdict never resolves the gate.** Phase 1 is
//!    advisory-only; `Advised` must coexist with "still parked".
//! 2. **Every non-answer is a park, never a decision.** Timeout, seat
//!    failure, P-2.21 escalation, unparsable reply, human-decided-first —
//!    the row keeps its null verdict and the gate waits for the operator.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use autonoetic_gateway::decider_appointment::{agreement_tally_for_appointment, appoint};
use autonoetic_gateway::decider_dispatch::sweep_undispatched_routings;
use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Role, StopReason, TokenUsage,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{ApprovalRequest, ApprovalRisk, ScheduledAction};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::decider_appointment::DeciderGateRouting;
use tempfile::tempdir;

use crate::gate_decider::{seed_decider_revision, write_agent_dir};

struct Fx {
    _temp: tempfile::TempDir,
    cfg: GatewayConfig,
    store: Arc<GatewayStore>,
    svc: GatewayExecutionService,
}

fn fx() -> anyhow::Result<Fx> {
    // The seat's turn runs through the real executor, whose attestation tail
    // binds the constitution digest. Init-or-tolerate-neighbor: under nextest
    // each test is its own process, but keep the shared-process pattern
    // anyway (see validation/user_interaction_resume.rs).
    if let Err(e) = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    ) {
        assert!(
            autonoetic_gateway::constitution_digest::is_constitution_initialized(),
            "initialize_constitution failed without a fallback runtime: {e:#}"
        );
    }

    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    write_agent_dir(
        &agents_dir,
        "nightwatch.default",
        &[Capability::GateDecider {
            kinds: vec!["approval".to_string()],
        }],
    );
    let mut llm_presets = std::collections::HashMap::new();
    llm_presets.insert(
        "decider".to_string(),
        autonoetic_types::config::LlmPreset {
            provider: Some("anthropic".to_string()),
            model: Some("claude-opus-4-20250514".to_string()),
            temperature: Some(0.0),
            fallback_provider: None,
            fallback_model: None,
            chat_only: None,
            context_window_tokens: None,
            max_tokens: None,
            base_url: None,
            api_key_env: None,
            thinking: None,
            tier: None,
            cost: None,
            latency: None,
            routing: None,
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    let cfg = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir: agents_dir.clone(),
        llm_presets,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    seed_decider_revision(&agents_dir, &gateway_dir, &store, "nightwatch.default")?;
    let svc = GatewayExecutionService::new(cfg.clone(), Some(store.clone()));
    Ok(Fx {
        _temp: temp,
        cfg,
        store,
        svc,
    })
}

fn seat(f: &Fx, scope: &str) -> anyhow::Result<String> {
    let a = appoint(
        &f.cfg,
        &f.store,
        autonoetic_gateway::decider_appointment::AppointmentRequest {
            decider_agent: "nightwatch.default".to_string(),
            kinds: vec!["approval".to_string()],
            scope_root_session: scope.to_string(),
            risk_ceiling: ApprovalRisk::High,
            advice_only: true,
            expires_at: None,
            max_gates: None,
            appointed_by: "operator".to_string(),
        },
    )?;
    Ok(a.appointment_id)
}

/// `sandbox_exec` with detected hosts is High — the Night Shift shape.
fn high_risk_exec(command: &str, hosts: &[&str]) -> ScheduledAction {
    ScheduledAction::SandboxExec {
        command: command.to_string(),
        dependencies: None,
        requires_approval: true,
        evidence_ref: None,
        detected_hosts: Some(hosts.iter().map(|h| h.to_string()).collect()),
        intent: None,
    }
}

fn seed_gate(
    f: &Fx,
    gate_id: &str,
    action: ScheduledAction,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let mut request = ApprovalRequest {
        request_id: gate_id.to_string(),
        agent_id: "coder.default".to_string(),
        session_id: "root-1/coder".to_string(),
        action,
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: reason.map(str::to_string),
        evidence_ref: None,
        root_session_id: Some("root-1".to_string()),
        workflow_id: None,
        task_id: None,
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        approval_level: autonoetic_types::background::ApprovalLevel::Operator,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    f.store.create_approval(&mut request)?;
    Ok(())
}

fn seed_routing(f: &Fx, gate_id: &str, appointment_id: &str) -> anyhow::Result<String> {
    let routing = DeciderGateRouting {
        routing_id: format!("rtg_{}", gate_id),
        gate_id: gate_id.to_string(),
        appointment_id: appointment_id.to_string(),
        decider_agent: "nightwatch.default".to_string(),
        decider_session: None,
        gate_kind: "approval".to_string(),
        gate_risk: "high".to_string(),
        advice_only: true,
        routed_at: chrono::Utc::now().to_rfc3339(),
        verdict: None,
        verdict_reason: None,
        verdict_at: None,
    };
    f.store.insert_decider_gate_routing(&routing)?;
    Ok(routing.routing_id)
}

/// A scripted driver: replies with the next entry for each completion, and
/// records every user message it was shown.
struct ScriptedDriver {
    replies: Mutex<std::collections::VecDeque<String>>,
    prompts: Mutex<Vec<String>>,
}

impl ScriptedDriver {
    fn scripted(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(std::collections::VecDeque::from([reply.to_string()])),
            prompts: Mutex::new(Vec::new()),
        })
    }

    /// A driver that rules on the **mechanical facts** of the card: parse the
    /// serialized action out of the prompt's json block and reject when any
    /// host was detected, regardless of what the prose claims.
    fn rules_from_action() -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(std::collections::VecDeque::new()),
            prompts: Mutex::new(Vec::new()),
        })
    }

    fn seen_prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmDriver for ScriptedDriver {
    async fn complete(&self, request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let prompt = request
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        self.prompts.lock().unwrap().push(prompt.clone());

        let reply = if let Some(next) = self.replies.lock().unwrap().pop_front() {
            next
        } else {
            // Rule from the mechanical facts in the card: the serialized
            // action is the gateway-computed truth; detected hosts mean the
            // egress is real no matter what the run's prose says.
            let action_json = prompt
                .split("```json")
                .nth(1)
                .and_then(|rest| rest.split("```").next())
                .unwrap_or("");
            let detected = serde_json::from_str::<serde_json::Value>(action_json)
                .ok()
                .and_then(|v| find_detected_hosts(&v))
                .unwrap_or(false);
            if detected {
                "The action carries detected hosts; the prose claiming a local run \
                 does not survive the mechanical facts.\nVERDICT: reject"
                    .to_string()
            } else {
                "No hosts detected in the serialized action.\nVERDICT: approve".to_string()
            }
        };

        Ok(CompletionResponse {
            text: reply,
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

fn advised(verdict: &str, reason: &str) -> String {
    format!("{reason}\nVERDICT: {verdict}")
}

/// `ScheduledAction` serializes with its variant tag, so `detected_hosts`
/// sits one level down. A seat looking for the mechanical fact must find it
/// wherever the envelope put it.
fn find_detected_hosts(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(hosts) = map.get("detected_hosts").and_then(|h| h.as_array()) {
                return Some(!hosts.is_empty());
            }
            map.values().find_map(find_detected_hosts)
        }
        _ => None,
    }
}

// ── The dispatch half: wake, capture ───────────────────────────────────────

#[tokio::test]
async fn a_terminal_verdict_is_recorded_and_the_gate_still_parks() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), Some("fetch prices"))?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let driver = ScriptedDriver::scripted(&advised(
        "approve",
        "SandboxExec to stooq.com, high risk, hosts match the run's stated goal.",
    ));
    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;

    // The seat advised...
    let routing = f
        .store
        .get_decider_gate_routing(&rid)?
        .expect("routing row exists");
    assert_eq!(routing.verdict.as_deref(), Some("approve"));
    assert_eq!(
        routing.verdict_reason.as_deref(),
        Some("SandboxExec to stooq.com, high risk, hosts match the run's stated goal.")
    );
    assert!(routing.verdict_at.is_some(), "the verdict is timestamped");

    // ...and the gate still parks: phase 1 is advisory, structurally.
    let gate = f.store.get_approval("apr-1")?.expect("gate exists");
    assert!(
        gate.decided_at.is_none() && gate.decided_by.is_none() && gate.status.is_none(),
        "an advisory verdict must never resolve the gate, got {:?}",
        gate.status
    );
    match outcome {
        autonoetic_gateway::execution::DeciderDispatchOutcome::Advised { verdict, .. } => {
            assert_eq!(verdict, "approve");
        }
        other => panic!("expected Advised, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn the_verdict_carries_the_appointment_reference_on_the_chain() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let driver = ScriptedDriver::scripted(&advised("reject", "hosts detected"));
    f.svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;

    let events = f
        .store
        .search_causal_events(None, Some("nightwatch.default"), 20)?;
    let advice = events
        .iter()
        .find(|e| e.action == "agent_decider.approval_gate" && e.status == "advised")
        .expect("the advisory verdict is attributed on the causal chain");
    assert!(advice.enforced_rules.contains(&"P-2.20".to_string()));
    let payload: serde_json::Value =
        serde_json::from_str(advice.payload.as_deref().unwrap_or("{}"))?;
    assert_eq!(payload["appointment_id"], serde_json::json!(apt));
    assert_eq!(payload["routing_id"], serde_json::json!(rid));
    assert_eq!(payload["advice_only"], serde_json::json!(true));
    assert_eq!(payload["verdict"], serde_json::json!("reject"));
    assert!(
        payload["card_sha256"].as_str().is_some(),
        "the Ri-0.15 context the seat consumed is recorded by digest"
    );
    assert!(
        payload["decider_session"].as_str().is_some(),
        "the seat's session is named — its reads are causal-logged there"
    );

    // A verdict spends the seat's gate budget.
    let a = f.store.get_decider_appointment(&apt)?.unwrap();
    assert_eq!(a.gates_decided, 1);
    Ok(())
}

#[tokio::test]
async fn dispatch_is_idempotent_and_never_rewrites_a_recorded_verdict() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let driver = ScriptedDriver::scripted(&advised("reject", "hosts detected"));
    f.svc
        .dispatch_decider_routing_with_driver(&rid, driver.clone(), Duration::from_secs(5))
        .await?;

    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;
    assert!(
        matches!(outcome, autonoetic_gateway::execution::DeciderDispatchOutcome::Parked { .. }),
        "a duplicate dispatch is a no-op, got {outcome:?}"
    );
    let routing = f.store.get_decider_gate_routing(&rid)?.unwrap();
    assert_eq!(routing.verdict.as_deref(), Some("reject"));
    Ok(())
}

// ── The graduated fallback: every non-answer is a park ─────────────────────

#[tokio::test]
async fn a_seat_that_escalates_leaves_the_verdict_null_and_the_gate_parked() -> anyhow::Result<()>
{
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let driver = ScriptedDriver::scripted(
        "The card lacks the evidence I need to rule either way.\nVERDICT: escalate",
    );
    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;
    assert!(matches!(
        outcome,
        autonoetic_gateway::execution::DeciderDispatchOutcome::Parked { .. }
    ));
    let routing = f.store.get_decider_gate_routing(&rid)?.unwrap();
    assert!(routing.is_awaiting_verdict(), "P-2.21 escalation is not a verdict");
    assert!(f.store.get_approval("apr-1")?.unwrap().decided_at.is_none());
    Ok(())
}

#[tokio::test]
async fn an_unparsable_reply_leaves_the_verdict_null() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let driver = ScriptedDriver::scripted("Looks fine to me, ship it.");
    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;
    assert!(matches!(
        outcome,
        autonoetic_gateway::execution::DeciderDispatchOutcome::Parked { .. }
    ));
    assert!(f.store.get_decider_gate_routing(&rid)?.unwrap().is_awaiting_verdict());
    Ok(())
}

#[tokio::test]
async fn a_verdict_without_a_motivation_fails_o1_and_parks() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let driver = ScriptedDriver::scripted("VERDICT: approve");
    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;
    assert!(matches!(
        outcome,
        autonoetic_gateway::execution::DeciderDispatchOutcome::Parked { .. }
    ));
    assert!(f.store.get_decider_gate_routing(&rid)?.unwrap().is_awaiting_verdict());
    Ok(())
}

#[tokio::test]
async fn the_dwell_bound_times_the_seat_out_into_a_park() -> anyhow::Result<()> {
    struct SlowDriver;
    #[async_trait::async_trait]
    impl LlmDriver for SlowDriver {
        async fn complete(&self, _request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(CompletionResponse {
                text: advised("approve", "slow but certain"),
                tool_calls: vec![],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
    }

    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, Arc::new(SlowDriver), Duration::from_millis(50))
        .await?;
    match outcome {
        autonoetic_gateway::execution::DeciderDispatchOutcome::Parked { reason } => {
            assert!(reason.contains("dwell"), "timeout is named: {reason}");
        }
        other => panic!("expected a park on dwell overrun, got {other:?}"),
    }
    assert!(f.store.get_decider_gate_routing(&rid)?.unwrap().is_awaiting_verdict());
    Ok(())
}

#[tokio::test]
async fn a_dead_seat_parks() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    // Vacate the seat, then dispatch anyway: the wake must not resurrect it.
    autonoetic_gateway::decider_appointment::revoke(&f.store, &apt, "operator", Some("shift over"))?;

    let driver = ScriptedDriver::scripted(&advised("approve", "would approve"));
    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;
    assert!(matches!(
        outcome,
        autonoetic_gateway::execution::DeciderDispatchOutcome::Parked { .. }
    ));
    assert!(f.store.get_decider_gate_routing(&rid)?.unwrap().is_awaiting_verdict());
    Ok(())
}

#[tokio::test]
async fn a_human_who_decided_first_is_never_second_guessed() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;
    f.store.record_decision(
        "apr-1",
        "rejected",
        "operator",
        &chrono::Utc::now().to_rfc3339(),
        Some("morning review"),
    )?;

    let driver = ScriptedDriver::scripted(&advised("approve", "would approve"));
    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver, Duration::from_secs(5))
        .await?;
    assert!(matches!(
        outcome,
        autonoetic_gateway::execution::DeciderDispatchOutcome::Parked { .. }
    ));
    // The seat never runs: no spend, no verdict.
    assert!(f.store.get_decider_gate_routing(&rid)?.unwrap().is_awaiting_verdict());
    let a = f.store.get_decider_appointment(&apt)?.unwrap();
    assert_eq!(a.gates_decided, 0);
    Ok(())
}

// ── Verdict quality: the action, not the narrative ─────────────────────────

/// The proposal's testable motivation-quality requirement: feed a card whose
/// prose contradicts its `ScheduledAction` and assert the verdict follows the
/// action. The scripted seat can only rule correctly if the card actually
/// carries the mechanical facts — so this also pins the card's shape.
#[tokio::test]
async fn the_verdict_follows_the_action_not_the_prose() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(
        &f,
        "apr-1",
        high_risk_exec("curl https://stooq.com/q/l/?s=aapl", &["stooq.com"]),
        Some("Purely local pytest run. No network access is required or performed."),
    )?;
    let rid = seed_routing(&f, "apr-1", &apt)?;

    let driver = ScriptedDriver::rules_from_action();
    let outcome = f
        .svc
        .dispatch_decider_routing_with_driver(&rid, driver.clone(), Duration::from_secs(5))
        .await?;

    match outcome {
        autonoetic_gateway::execution::DeciderDispatchOutcome::Advised { verdict, .. } => {
            assert_eq!(verdict, "reject", "the mechanical facts rule, not the prose");
        }
        other => panic!("expected Advised, got {other:?}"),
    }
    // The card really did carry both, and in this order of authority: the
    // serialized action the seat ruled on, and the run's self-serving prose.
    let prompts = driver.seen_prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("stooq.com"), "the action's hosts are on the card");
    assert!(
        prompts[0].contains("Purely local pytest run"),
        "the run's narrative is on the card too — the seat must judge it, not be spared it"
    );
    Ok(())
}

// ── The startup sweep ──────────────────────────────────────────────────────

#[tokio::test]
async fn the_startup_sweep_rewakes_live_unanswered_routings_only() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;

    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let live = seed_routing(&f, "apr-1", &apt)?;

    // Human already decided this one — moot, not woken.
    seed_gate(&f, "apr-2", high_risk_exec("curl https://example.com", &["example.com"]), None)?;
    let decided = seed_routing(&f, "apr-2", &apt)?;
    f.store.record_decision("apr-2", "approved", "operator", &chrono::Utc::now().to_rfc3339(), None)?;

    let woken = sweep_undispatched_routings(&f.svc).await;
    assert_eq!(woken, 1, "only the live unanswered routing is re-woken");

    // No worker is installed in this process, so notify is a no-op and the
    // row stays unanswered — the sweep's own failure mode parks.
    assert!(f.store.get_decider_gate_routing(&live)?.unwrap().is_awaiting_verdict());
    assert!(f.store.get_decider_gate_routing(&decided)?.unwrap().is_awaiting_verdict());
    Ok(())
}

// ── The agreement ledger, computed on read ─────────────────────────────────

#[tokio::test]
async fn agreement_rate_over_a_seeded_ledger_matches_the_hand_computed_value() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;

    // Five referrals: 2 agreements, 1 disagreement, 1 moot (gate cancelled),
    // 1 never answered. Expected: comparable 3, agreed 2 → rate 2/3.
    let cases: &[(&str, ScheduledAction, Option<&str>, Option<&str>, Option<&str>)] = &[
        (
            "apr-1",
            high_risk_exec("curl https://stooq.com", &["stooq.com"]),
            Some("approve"),
            Some("approved"),
            Some("fine"),
        ),
        (
            "apr-2",
            high_risk_exec("curl https://example.com", &["example.com"]),
            Some("reject"),
            Some("rejected"),
            Some("not tonight"),
        ),
        (
            "apr-3",
            high_risk_exec("curl https://api.github.com", &["api.github.com"]),
            Some("approve"),
            Some("rejected"),
            Some("operator disagreed"),
        ),
        (
            "apr-4",
            high_risk_exec("curl https://pypi.org", &["pypi.org"]),
            Some("approve"),
            Some("cancelled"),
            None,
        ),
        (
            "apr-5",
            high_risk_exec("curl https://crates.io", &["crates.io"]),
            None,
            None,
            None,
        ),
    ];
    for (gate_id, action, agent, human, human_reason) in cases {
        seed_gate(&f, gate_id, action.clone(), None)?;
        let rid = seed_routing(&f, gate_id, &apt)?;
        if let Some(v) = agent {
            f.store.record_decider_gate_verdict(
                &rid,
                v,
                "seeded motivation",
                &chrono::Utc::now().to_rfc3339(),
            )?;
        }
        if let Some(h) = human {
            f.store.record_decision(
                gate_id,
                h,
                "operator",
                &chrono::Utc::now().to_rfc3339(),
                *human_reason,
            )?;
        }
    }

    let tally = agreement_tally_for_appointment(&f.store, &apt)?;
    assert_eq!(tally.with_agent_verdict, 4);
    assert_eq!(tally.comparable, 3, "a cancelled gate is moot, not comparable");
    assert_eq!(tally.agreed, 2);
    assert_eq!(tally.disagreed, 1);
    assert_eq!(tally.unanswered, 1, "an unanswered referral is counted, not dropped");
    let rate = tally.rate().expect("comparable > 0");
    assert!(
        (rate - 2.0 / 3.0).abs() < 1e-9,
        "hand-computed rate is 2/3, got {rate}"
    );
    Ok(())
}

#[tokio::test]
async fn an_empty_ledger_has_no_rate_to_report() -> anyhow::Result<()> {
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    let tally = agreement_tally_for_appointment(&f.store, &apt)?;
    assert_eq!(tally.rate(), None, "no comparable cases, no rate — never a fake 100%");
    Ok(())
}

#[tokio::test]
async fn nothing_is_stored_the_rate_is_recomputed_from_the_rows() -> anyhow::Result<()> {
    // The ledger's own table has no aggregate column to go stale: the rate
    // exists only when computed from the routing rows and the approvals.
    let f = fx()?;
    let apt = seat(&f, "root-1")?;
    seed_gate(&f, "apr-1", high_risk_exec("curl https://stooq.com", &["stooq.com"]), None)?;
    let rid = seed_routing(&f, "apr-1", &apt)?;
    f.store.record_decider_gate_verdict(&rid, "reject", "hosts", &chrono::Utc::now().to_rfc3339())?;

    let before = agreement_tally_for_appointment(&f.store, &apt)?;
    assert_eq!((before.agreed, before.disagreed), (0, 0));

    // The human now disagrees; nothing was stored that could hide it.
    f.store.record_decision("apr-1", "approved", "operator", &chrono::Utc::now().to_rfc3339(), None)?;
    let after = agreement_tally_for_appointment(&f.store, &apt)?;
    assert_eq!((after.agreed, after.disagreed), (0, 1));
    Ok(())
}
