//! Security sentinel CLI commands.
//!
//! `autonoetic security status`         — snapshot of finding counts and sentinel health.
//! `autonoetic security findings`       — list findings with filters.
//! `autonoetic security triage`         — mark a finding or bulk-mark a class of findings.
//! `autonoetic security patterns`       — list red-team attack-pattern proposals.
//! `autonoetic security pattern-accept` — operator accepts a proposed pattern.
//! `autonoetic security pattern-reject` — operator rejects a proposed pattern.
//!
//! Since #1119 (tranche 2) every subcommand speaks JSON-RPC to the running
//! gateway ([`crate::cli::rpc::GatewayRpc`]) via the `security.*` methods —
//! the CLI never opens gateway.db directly.

use anyhow::Result;
use autonoetic_types::security::TriageState;
use std::path::Path;

use crate::cli::rpc::GatewayRpc;

fn open_rpc(config_path: &Path) -> Result<GatewayRpc> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    GatewayRpc::from_config(&config)
}

// ── status ────────────────────────────────────────────────────────────────────

pub fn handle_security_status(config_path: &Path, json: bool) -> Result<()> {
    let rpc = open_rpc(config_path)?;
    let out = rpc.call("security.status", serde_json::json!({}))?;

    let by_severity: Vec<(String, i64)> = out["pending_by_severity"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    (
                        e["severity"].as_str().unwrap_or("?").to_string(),
                        e["count"].as_i64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let by_triage: Vec<(String, i64)> = out["by_triage_state"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    (
                        e["triage_state"].as_str().unwrap_or("?").to_string(),
                        e["count"].as_i64().unwrap_or(0),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let last_sweep = out["last_sweep_at"].as_str().map(String::from);

    if json {
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
    let rpc = open_rpc(config_path)?;
    let rows = rpc.call(
        "security.findings",
        serde_json::json!({
            "severity": severity,
            "finding_type": finding_type,
            "triage": triage,
            "limit": limit,
        }),
    )?;
    let rows = rows.as_array().cloned().unwrap_or_default();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
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
            truncate(r["finding_id"].as_str().unwrap_or("?"), 22),
            r["severity"].as_str().unwrap_or("?"),
            r["triage_state"].as_str().unwrap_or("?"),
            r["finding_type"].as_str().unwrap_or("?"),
            r["confidence"].as_f64().unwrap_or(0.0),
            r["created_at"].as_str().unwrap_or("?"),
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

    if triage_state != TriageState::Pending && reason.is_none() {
        anyhow::bail!("--reason is required when setting a non-pending triage state");
    }

    let rpc = open_rpc(config_path)?;
    rpc.call(
        "security.triage",
        serde_json::json!({
            "finding_id": finding_id,
            "state": state,
            "reason": reason,
        }),
    )?;
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

    if triage_state == TriageState::Pending {
        anyhow::bail!(
            "Cannot bulk-triage to 'pending'. Valid states: true_positive, false_positive, benign, deferred"
        );
    }

    let rpc = open_rpc(config_path)?;

    if dry_run {
        // Dry run lists what *would* be marked — same filter the bulk RPC uses
        // server-side (pending + severity/type filters).
        let rows = rpc.call(
            "security.findings",
            serde_json::json!({
                "severity": severity,
                "finding_type": finding_type,
                "triage": "pending",
                "limit": 10_000,
            }),
        )?;
        let rows = rows.as_array().cloned().unwrap_or_default();
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
        for r in &rows {
            println!(
                "  [dry-run] {} ({})",
                r["finding_id"].as_str().unwrap_or("?"),
                r["finding_type"].as_str().unwrap_or("?")
            );
        }
        return Ok(());
    }

    let result = rpc.call(
        "security.triage_bulk",
        serde_json::json!({
            "state": state,
            "reason": reason,
            "severity": severity,
            "finding_type": finding_type,
        }),
    )?;
    let matched = result["matched"].as_u64().unwrap_or(0);
    if matched == 0 {
        println!("No pending findings match the given filters — nothing to triage.");
        return Ok(());
    }
    // The bulk ran server-side in one RPC; report what it did.
    println!(
        "{} pending finding(s) matched filters '{}' — marking '{}' with reason: {}",
        matched,
        format_bulk_filter(severity, finding_type),
        state,
        reason
    );

    let ok = result["triaged"].as_u64().unwrap_or(0) as usize;
    for f in result["failures"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        eprintln!(
            "  Failed to triage {}: {}",
            f["finding_id"].as_str().unwrap_or("?"),
            f["error"].as_str().unwrap_or("?")
        );
    }
    let errors = result["failures"].as_array().map(|a| a.len()).unwrap_or(0);

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

fn format_bulk_filter(severity: Option<&str>, finding_type: Option<&str>) -> String {
    match (severity, finding_type) {
        (Some(s), Some(t)) => format!("severity={s}, type={t}"),
        (Some(s), None) => format!("severity={s}"),
        (None, Some(t)) => format!("type={t}"),
        (None, None) => "all severities and types".to_string(),
    }
}

// ── attack-pattern proposals ───────────────────────────────────────────────────

pub fn handle_security_patterns(
    config_path: &Path,
    status: Option<&str>,
    limit: u32,
    json: bool,
) -> Result<()> {
    let rpc = open_rpc(config_path)?;
    let patterns = rpc.call(
        "security.patterns",
        serde_json::json!({ "status": status, "limit": limit }),
    )?;
    let patterns = patterns.as_array().cloned().unwrap_or_default();

    if json {
        println!("{}", serde_json::to_string_pretty(&patterns)?);
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
            truncate(p["pattern_id"].as_str().unwrap_or("?"), 22),
            truncate(p["category"].as_str().unwrap_or("?"), 30),
            p["status"].as_str().unwrap_or("?"),
            truncate(p["proposed_by_agent_id"].as_str().unwrap_or("?"), 14),
            p["created_at"].as_str().unwrap_or("?"),
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
    let rpc = open_rpc(config_path)?;
    rpc.call(
        "security.pattern_review",
        serde_json::json!({
            "pattern_id": pattern_id,
            "decision": "accepted",
            "check_type": check_type,
            "notes": notes,
        }),
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
    let rpc = open_rpc(config_path)?;
    rpc.call(
        "security.pattern_review",
        serde_json::json!({
            "pattern_id": pattern_id,
            "decision": "rejected",
            "notes": notes,
        }),
    )?;
    println!(
        "Pattern {} → rejected{}",
        pattern_id,
        notes.map(|n| format!(" — {}", n)).unwrap_or_default()
    );
    Ok(())
}
