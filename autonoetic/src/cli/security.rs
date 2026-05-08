//! Security sentinel CLI commands.
//!
//! `autonoetic security status`         — snapshot of finding counts and sentinel health.
//! `autonoetic security findings`       — list findings with filters.
//! `autonoetic security triage`         — mark a finding or bulk-mark a class of findings.
//! `autonoetic security patterns`       — list red-team attack-pattern proposals.
//! `autonoetic security pattern-accept` — operator accepts a proposed pattern.
//! `autonoetic security pattern-reject` — operator rejects a proposed pattern.

use anyhow::Result;
use autonoetic_types::security::{AttackPatternStatus, TriageState};
use std::path::Path;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

fn open_store(config_path: &Path) -> Result<GatewayStore> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    Ok(GatewayStore::open(&gateway_dir)?)
}

// ── status ────────────────────────────────────────────────────────────────────

pub fn handle_security_status(config_path: &Path, json: bool) -> Result<()> {
    let store = open_store(config_path)?;

    let by_severity = store.count_pending_security_findings_by_severity()?;
    let by_triage = store.count_security_findings_by_triage_state()?;

    // Most-recent sentinel sweep of any kind: take the max last_run_at across
    // both full-sweep and incremental jobs owned by security_sentinel.
    let last_sweep = store
        .list_scheduled_jobs_for_owner("security_sentinel", None, None)?
        .into_iter()
        .filter_map(|j| j.last_run_at)
        .max();

    if json {
        let out = serde_json::json!({
            "pending_by_severity": by_severity.iter().map(|(s, c)| serde_json::json!({"severity": s, "count": c})).collect::<Vec<_>>(),
            "by_triage_state": by_triage.iter().map(|(s, c)| serde_json::json!({"triage_state": s, "count": c})).collect::<Vec<_>>(),
            "last_sweep_at": last_sweep,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("Security Sentinel Status");
    println!("{}", "─".repeat(50));

    println!("\nPending findings by severity:");
    if by_severity.is_empty() {
        println!("  (none)");
    } else {
        for (sev, count) in &by_severity {
            println!("  {:<12} {}", sev, count);
        }
    }

    println!("\nAll findings by triage state:");
    if by_triage.is_empty() {
        println!("  (none)");
    } else {
        let total: i64 = by_triage.iter().map(|(_, c)| c).sum();
        for (state, count) in &by_triage {
            println!("  {:<16} {}", state, count);
        }
        println!("  {:<16} {}", "TOTAL", total);
    }

    match &last_sweep {
        Some(ts) => println!("\nLast sentinel sweep:   {}", ts),
        None => println!("\nLast sentinel sweep:   (never — no sweep has completed yet)"),
    }

    Ok(())
}

// ── findings ──────────────────────────────────────────────────────────────────

pub fn handle_security_findings(
    config_path: &Path,
    severity: Option<&str>,
    finding_type: Option<&str>,
    triage: Option<&str>,
    limit: u32,
    json: bool,
) -> Result<()> {
    let store = open_store(config_path)?;
    let rows = store.list_security_findings_filtered(severity, finding_type, triage, limit)?;

    if json {
        let out: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "finding_id": r.finding_id,
                    "severity": r.severity,
                    "confidence": r.confidence,
                    "finding_type": r.finding_type,
                    "reproducibility": r.reproducibility,
                    "sentinel_revision_id": r.sentinel_revision_id,
                    "baseline_agreed": r.baseline_agreed,
                    "triage_state": r.triage_state,
                    "triage_reason": r.triage_reason,
                    "proposed_remediation": r.proposed_remediation,
                    "created_at": r.created_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No findings match the given filters.");
        return Ok(());
    }

    println!(
        "{:<22} {:<10} {:<12} {:<26} {:<14} CREATED AT",
        "FINDING ID", "SEVERITY", "TRIAGE", "TYPE", "CONFIDENCE"
    );
    println!("{}", "─".repeat(100));
    for r in &rows {
        println!(
            "{:<22} {:<10} {:<12} {:<26} {:<14.2} {}",
            truncate(&r.finding_id, 22),
            r.severity,
            r.triage_state,
            r.finding_type,
            r.confidence,
            r.created_at,
        );
    }
    println!("\n{} finding(s) shown.", rows.len());
    Ok(())
}

// ── triage ────────────────────────────────────────────────────────────────────

pub fn handle_security_triage(
    config_path: &Path,
    finding_id: &str,
    state: &str,
    reason: Option<&str>,
) -> Result<()> {
    let triage_state = parse_triage_state(state)?;

    if triage_state != autonoetic_types::security::TriageState::Pending && reason.is_none() {
        anyhow::bail!("--reason is required when setting a non-pending triage state");
    }

    let store = open_store(config_path)?;
    store.update_security_finding_triage(finding_id, triage_state, reason)?;
    println!(
        "Finding {} → {} {}",
        finding_id,
        state,
        reason.map(|r| format!("({})", r)).unwrap_or_default()
    );
    Ok(())
}

pub fn handle_security_triage_bulk(
    config_path: &Path,
    state: &str,
    reason: &str,
    severity: Option<&str>,
    finding_type: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let triage_state = parse_triage_state(state)?;

    if triage_state == autonoetic_types::security::TriageState::Pending {
        anyhow::bail!(
            "Cannot bulk-triage to 'pending'. Valid states: true_positive, false_positive, benign, deferred"
        );
    }

    let store = open_store(config_path)?;

    // Fetch all pending findings matching the filter (large limit for bulk).
    let rows = store.list_security_findings_filtered(
        severity,
        finding_type,
        Some("pending"),
        10_000,
    )?;

    if rows.is_empty() {
        println!("No pending findings match the given filters — nothing to triage.");
        return Ok(());
    }

    println!(
        "{} finding(s) will be marked '{}' with reason: {}",
        rows.len(),
        state,
        reason
    );

    if dry_run {
        for r in &rows {
            println!("  [dry-run] {} ({})", r.finding_id, r.finding_type);
        }
        return Ok(());
    }

    let mut ok = 0usize;
    let mut errors = 0usize;
    for r in &rows {
        match store.update_security_finding_triage(&r.finding_id, triage_state.clone(), Some(reason)) {
            Ok(()) => ok += 1,
            Err(e) => {
                eprintln!("  Failed to triage {}: {}", r.finding_id, e);
                errors += 1;
            }
        }
    }

    println!("{} triaged, {} errors.", ok, errors);
    if errors > 0 {
        anyhow::bail!("{} triage update(s) failed", errors);
    }
    Ok(())
}

fn parse_triage_state(s: &str) -> Result<TriageState> {
    match s {
        "pending" => Ok(TriageState::Pending),
        "true_positive" => Ok(TriageState::TruePositive),
        "false_positive" => Ok(TriageState::FalsePositive),
        "benign" => Ok(TriageState::Benign),
        "deferred" => Ok(TriageState::Deferred),
        other => anyhow::bail!(
            "Unknown triage state '{}'. Valid states: pending, true_positive, false_positive, benign, deferred",
            other
        ),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

// ── attack-pattern proposals ───────────────────────────────────────────────────

pub fn handle_security_patterns(
    config_path: &Path,
    status: Option<&str>,
    limit: u32,
    json: bool,
) -> Result<()> {
    let store = open_store(config_path)?;
    let patterns = store.list_attack_patterns(status, limit)?;

    if json {
        let out: Vec<_> = patterns
            .iter()
            .map(|p| {
                serde_json::json!({
                    "pattern_id": p.pattern_id,
                    "category": p.category,
                    "status": p.status.to_string(),
                    "proposed_by_agent_id": p.proposed_by_agent_id,
                    "description": p.description,
                    "accepted_check_type": p.accepted_check_type,
                    "operator_notes": p.operator_notes,
                    "created_at": p.created_at,
                    "reviewed_at": p.reviewed_at,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if patterns.is_empty() {
        println!("No attack pattern proposals match the given filters.");
        return Ok(());
    }

    println!(
        "{:<22} {:<30} {:<10} {:<14} CREATED AT",
        "PATTERN ID", "CATEGORY", "STATUS", "PROPOSER"
    );
    println!("{}", "─".repeat(100));
    for p in &patterns {
        println!(
            "{:<22} {:<30} {:<10} {:<14} {}",
            truncate(&p.pattern_id, 22),
            truncate(&p.category, 30),
            p.status.to_string(),
            truncate(&p.proposed_by_agent_id, 14),
            p.created_at,
        );
    }
    println!("\n{} proposal(s) shown.", patterns.len());
    Ok(())
}

pub fn handle_security_pattern_accept(
    config_path: &Path,
    pattern_id: &str,
    check_type: &str,
    notes: Option<&str>,
) -> Result<()> {
    if check_type != "phase1" && check_type != "phase2" {
        anyhow::bail!("--type must be 'phase1' (deterministic) or 'phase2' (llm-judgment)");
    }
    let store = open_store(config_path)?;
    store.review_attack_pattern(
        pattern_id,
        AttackPatternStatus::Accepted,
        Some(check_type),
        notes,
    )?;
    println!(
        "Pattern {} → accepted ({}){}",
        pattern_id,
        check_type,
        notes.map(|n| format!(" — {}", n)).unwrap_or_default()
    );
    println!("Next step: implement the sentinel check and add the synthetic test case to the sentinel eval suite.");
    Ok(())
}

pub fn handle_security_pattern_reject(
    config_path: &Path,
    pattern_id: &str,
    notes: Option<&str>,
) -> Result<()> {
    let store = open_store(config_path)?;
    store.review_attack_pattern(
        pattern_id,
        AttackPatternStatus::Rejected,
        None,
        notes,
    )?;
    println!(
        "Pattern {} → rejected{}",
        pattern_id,
        notes.map(|n| format!(" — {}", n)).unwrap_or_default()
    );
    Ok(())
}
