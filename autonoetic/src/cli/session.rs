//! `autonoetic session ...` subcommand. Self-Improvement loop P0 (#245).
//!
//! - `session rate <id> --thumbs-up|--thumbs-down [--note ...]` — attach
//!   an operator rating to the SessionOutcome row.
//! - `session show <id>` — print the SessionOutcome row as JSON.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use autonoetic_gateway::runtime::session_export::{
    export_session, render_export, ExportFormat, ExportOptions,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::session_outcome::OperatorThumb;

use crate::cli::common::SessionCommands;

pub async fn handle_session(config_path: &Path, command: &SessionCommands) -> anyhow::Result<()> {
    let loaded_config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = loaded_config.agents_dir.join(".gateway");
    let store = Arc::new(
        GatewayStore::open(&gateway_dir)
            .context("Failed to open GatewayStore — has the gateway run at this path?")?,
    );

    match command {
        SessionCommands::Rate {
            session_id,
            thumbs_up,
            thumbs_down,
            note,
        } => handle_rate(&store, session_id, *thumbs_up, *thumbs_down, note.as_deref()
        ),
        SessionCommands::Show { session_id } => handle_show(&store, session_id),
        SessionCommands::Export {
            session_id,
            output,
            format,
            with_checkpoints,
            min_altitude,
            output_dir,
        } => handle_export(
            &store,
            &loaded_config,
            session_id,
            output.as_deref(),
            format,
            *with_checkpoints,
            min_altitude.as_deref(),
            output_dir.as_deref(),
        ),
        SessionCommands::EgressPolicy { command } => handle_egress_policy(&store, command),
    }
}

/// `autonoetic session egress-policy …` — the session-scoped half of the egress
/// source rules (RFC data-envelopes §5.4).
fn handle_egress_policy(
    store: &Arc<GatewayStore>,
    command: &crate::cli::common::EgressPolicyCommands,
) -> anyhow::Result<()> {
    use crate::cli::common::EgressPolicyCommands;
    use autonoetic_gateway::runtime::content_store::root_session_id;

    match command {
        EgressPolicyCommands::Show { session_id } => {
            let root = root_session_id(session_id);
            match store.get_egress_session_policy(root)? {
                Some(stored) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "root_session_id": stored.root_session_id,
                            "policy": stored.policy,
                            "set_by": stored.set_by,
                            "created_at": stored.created_at,
                            "updated_at": stored.updated_at,
                        }))?
                    );
                }
                None => {
                    println!(
                        "No session egress policy for root session '{}'. \
                         Operator-global `egress.rules` still apply.",
                        root
                    );
                }
            }
            Ok(())
        }
        EgressPolicyCommands::Set {
            session_id,
            rules,
            default_label,
            provider_constraint,
            set_by,
        } => {
            anyhow::ensure!(
                !rules.is_empty() || default_label.is_some() || provider_constraint.is_some(),
                "nothing to declare — pass at least one --rule, a --default-label, or --provider-constraint"
            );
            let provider_constraint = match provider_constraint.as_deref() {
                Some("local_only") => {
                    Some(autonoetic_types::egress::ProviderConstraint::LocalOnly)
                }
                Some(other) => anyhow::bail!(
                    "unknown provider constraint '{other}'; expected `local_only`"
                ),
                None => None,
            };
            let policy = autonoetic_types::egress::EgressSessionPolicy {
                rules: rules
                    .iter()
                    .map(|spec| parse_rule_spec(spec))
                    .collect::<anyhow::Result<Vec<_>>>()?,
                default_label: match default_label {
                    Some(l) => Some(parse_named_label(l)?),
                    None => None,
                },
                provider_constraint,
            };
            let root = root_session_id(session_id).to_string();
            let stored = store.set_egress_session_policy(&root, &policy, set_by)?;
            println!(
                "Declared egress policy for root session '{}' ({} rule(s)).",
                root,
                stored.policy.rules.len()
            );
            println!("{}", serde_json::to_string_pretty(&stored.policy)?);
            Ok(())
        }
        EgressPolicyCommands::Clear { session_id, .. } => {
            let root = root_session_id(session_id).to_string();
            let cleared = store.delete_egress_session_policy(&root)?;
            println!(
                "{} for root session '{}'.",
                if cleared {
                    "Cleared egress policy"
                } else {
                    "No egress policy to clear"
                },
                root
            );
            Ok(())
        }
    }
}

/// Parse `SOURCE[:PATH]=LABEL` into an [`autonoetic_types::egress::EgressRule`].
///
/// The label is split off from the right so a path may itself contain `=`; the
/// source/path split then takes the *first* `:`, since tool names never contain
/// one and paths may.
pub(crate) fn parse_rule_spec(spec: &str) -> anyhow::Result<autonoetic_types::egress::EgressRule> {
    let (lhs, label) = spec.rsplit_once('=').ok_or_else(|| {
        anyhow::anyhow!("rule '{spec}' is missing '=LABEL' (e.g. 'email.*=local_only')")
    })?;
    let (source, path) = match lhs.split_once(':') {
        Some((s, p)) => (s.trim(), Some(p.trim().to_string())),
        None => (lhs.trim(), None),
    };
    anyhow::ensure!(!source.is_empty(), "rule '{spec}' has an empty source");
    Ok(autonoetic_types::egress::EgressRule {
        source: source.to_string(),
        path: path.filter(|p| !p.is_empty()),
        label: parse_named_label(label.trim())?.to_label(),
    })
}

pub(crate) fn parse_named_label(
    raw: &str,
) -> anyhow::Result<autonoetic_types::egress::NamedEgressLabel> {
    use autonoetic_types::egress::NamedEgressLabel;
    match raw.trim().to_ascii_lowercase().as_str() {
        "unrestricted" => Ok(NamedEgressLabel::Unrestricted),
        "local_only" | "local-only" => Ok(NamedEgressLabel::LocalOnly),
        "no_remote_model" | "no-remote-model" => Ok(NamedEgressLabel::NoRemoteModel),
        other => anyhow::bail!(
            "unknown egress label '{other}' — expected unrestricted, local_only, or no_remote_model"
        ),
    }
}

fn handle_rate(
    store: &Arc<GatewayStore>,
    session_id: &str,
    thumbs_up: bool,
    thumbs_down: bool,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let thumb = match (thumbs_up, thumbs_down) {
        (true, false) => OperatorThumb::Up,
        (false, true) => OperatorThumb::Down,
        (false, false) => {
            anyhow::bail!("must specify --thumbs-up or --thumbs-down")
        }
        (true, true) => unreachable!("clap conflicts_with prevents both"),
    };

    // Soft cap on note length so operators don't accidentally paste
    // huge transcripts into the rating column. The schema column is
    // unbounded — this is a CLI-layer guard.
    if let Some(n) = note {
        if n.len() > 500 {
            anyhow::bail!(
                "--note is {} chars; cap is 500. Use a separate notes file if more detail is needed.",
                n.len()
            );
        }
    }

    store
        .set_session_outcome_operator_rating(session_id, thumb, note)
        .with_context(|| format!("failed to record operator rating for {}", session_id))?;

    println!(
        "Recorded {} rating for session `{}`",
        thumb.as_str(),
        session_id
    );
    Ok(())
}

fn handle_show(store: &Arc<GatewayStore>, session_id: &str) -> anyhow::Result<()> {
    let outcome = store
        .get_session_outcome(session_id)
        .with_context(|| format!("failed to query session_outcomes for {}", session_id))?;
    match outcome {
        Some(o) => {
            println!("{}", serde_json::to_string_pretty(&o)?);
            Ok(())
        }
        None => {
            anyhow::bail!(
                "no SessionOutcome row found for session `{}`. \
                 Rows are created automatically when a session closes; \
                 historical sessions from before P0 will not have one yet.",
                session_id
            );
        }
    }
}

fn handle_export(
    store: &Arc<GatewayStore>,
    config: &autonoetic_types::config::GatewayConfig,
    session_id: &str,
    output: Option<&Path>,
    format: &str,
    with_checkpoints: bool,
    min_altitude: Option<&str>,
    output_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let format = format
        .parse::<ExportFormat>()
        .with_context(|| format!("invalid export format '{}'", format))?;

    let min_altitude = match min_altitude {
        Some(s) => Some(
            autonoetic_types::session_timeline::Altitude::parse_str(s)
                .ok_or_else(|| anyhow::anyhow!("invalid min-altitude '{}'", s))?,
        ),
        None => None,
    };

    let opts = ExportOptions {
        format,
        min_altitude,
        // Archive mode always collects checkpoints so we can emit both the
        // with-checkpoints and without-checkpoints artifacts.
        with_checkpoints: with_checkpoints || output_dir.is_some(),
        ..ExportOptions::default()
    };

    let export = export_session(store, config, session_id, &opts)
        .with_context(|| format!("failed to export session {}", session_id))?;

    if let Some(base_dir) = output_dir {
        let archive_dir = build_archive_dir(config, base_dir, session_id)?;

        // Clean any previous export for this session so stale pages are removed.
        if archive_dir.exists() {
            std::fs::remove_dir_all(&archive_dir)
                .with_context(|| format!("failed to clean previous archive dir {}", archive_dir.display()))?;
        }
        std::fs::create_dir_all(&archive_dir)
            .with_context(|| format!("failed to create archive dir {}", archive_dir.display()))?;

        // Wiki-style archive: interlinked Markdown files split by topic/turn.
        let wiki_dir = archive_dir.join("wiki");
        let wiki_files = autonoetic_gateway::runtime::session_export::render_wiki(&export, &wiki_dir)
            .with_context(|| format!("failed to render wiki archive for session {}", session_id))?;

        // Also emit a single JSON dump for programmatic use.
        let json_path = archive_dir.join(format!("{}.json", session_id));
        let json_rendered = render_export(&export, &ExportOptions {
            format: ExportFormat::Json,
            with_checkpoints: true,
            ..opts.clone()
        })
        .with_context(|| format!("failed to render json export for session {}", session_id))?;
        std::fs::write(&json_path, json_rendered)
            .with_context(|| format!("failed to write json export to {}", json_path.display()))?;

        // Manifest describing the archive.
        let manifest_path = archive_dir.join("MANIFEST.json");
        let manifest = serde_json::json!({
            "session_id": session_id,
            "exported_at": export.export_generated_at,
            "archive_format_version": "2",
            "gateway_dir": config.agents_dir.join(".gateway").display().to_string(),
            "constitution_lock": read_constitution_lock(config),
            "files": [
                serde_json::json!({ "kind": "json", "path": json_path.file_name().and_then(|n| n.to_str()).unwrap_or("") }),
            ],
            "wiki": {
                "dir": "wiki",
                "pages": wiki_files.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            },
        });
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
            .with_context(|| format!("failed to write manifest to {}", manifest_path.display()))?;

        println!(
            "Archived session `{}` to {} (wiki: {} pages + json)",
            session_id,
            archive_dir.display(),
            wiki_files.len()
        );
        return Ok(());
    }

    let rendered = render_export(&export, &opts)
        .with_context(|| format!("failed to render export for session {}", session_id))?;

    let output_path = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_export_path(session_id, format));

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }
    std::fs::write(&output_path, rendered)
        .with_context(|| format!("failed to write export to {}", output_path.display()))?;

    println!(
        "Exported session `{}` to {} ({}, {} bytes)",
        session_id,
        output_path.display(),
        export.export_options.format,
        std::fs::metadata(&output_path)
            .map(|m| m.len().to_string())
            .unwrap_or_else(|_| "?".to_string())
    );
    Ok(())
}

fn build_archive_dir(
    config: &autonoetic_types::config::GatewayConfig,
    base_dir: &Path,
    session_id: &str,
) -> anyhow::Result<PathBuf> {
    let lock = read_constitution_lock(config);
    let lock_subdir = match (&lock.version, &lock.digest) {
        (Some(v), Some(d)) => format!("{}-{}", v, &d[..d.len().min(8)]),
        (Some(v), None) => v.clone(),
        (None, Some(d)) => format!("lock-{}", &d[..d.len().min(16)]),
        (None, None) => "unknown-lock".to_string(),
    };
    Ok(base_dir.join(lock_subdir).join(session_id))
}

#[derive(Debug, Clone, serde::Serialize)]
struct ConstitutionLockInfo {
    version: Option<String>,
    digest: Option<String>,
    signer_id: Option<String>,
    lock_path: Option<String>,
}

fn read_constitution_lock(
    config: &autonoetic_types::config::GatewayConfig,
) -> ConstitutionLockInfo {
    let active_path = config.agents_dir.join(".gateway").join("constitution").join("ACTIVE.json");
    let text = match std::fs::read_to_string(&active_path) {
        Ok(t) => t,
        Err(_) => {
            return ConstitutionLockInfo {
                version: None,
                digest: None,
                signer_id: None,
                lock_path: None,
            };
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return ConstitutionLockInfo {
                version: None,
                digest: None,
                signer_id: None,
                lock_path: None,
            };
        }
    };
    ConstitutionLockInfo {
        version: value
            .get("constitution_version")
            .and_then(|v| v.as_str())
            .map(String::from),
        digest: value
            .get("constitution_digest")
            .and_then(|v| v.as_str())
            .map(String::from),
        signer_id: value
            .get("lock_signer_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        lock_path: value
            .get("lock_path")
            .and_then(|v| v.as_str())
            .map(String::from),
    }
}

fn default_export_path(session_id: &str, format: ExportFormat) -> std::path::PathBuf {
    let ext = match format {
        ExportFormat::Json => "json",
        ExportFormat::Room | ExportFormat::RoomRaw => "room.md",
    };
    let safe_id: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .collect();
    std::path::PathBuf::from(format!("{}.{}", safe_id, ext))
}

#[cfg(test)]
mod egress_policy_tests {
    use autonoetic_types::egress::{EgressLabel, NamedEgressLabel};

    use super::{parse_named_label, parse_rule_spec};

    #[test]
    fn parses_source_only_rule() {
        let r = parse_rule_spec("email.*=local_only").unwrap();
        assert_eq!(r.source, "email.*");
        assert_eq!(r.path, None);
        assert_eq!(r.label, EgressLabel::local_only());
    }

    #[test]
    fn parses_source_and_path_rule() {
        let r = parse_rule_spec("sandbox.exec:~/mail/**=local_only").unwrap();
        assert_eq!(r.source, "sandbox.exec");
        assert_eq!(r.path.as_deref(), Some("~/mail/**"));
    }

    /// A path may contain `=`; the label is split from the right, so it still
    /// lands where it should.
    #[test]
    fn label_is_split_from_the_right() {
        let r = parse_rule_spec("fs.read:/data/a=b/**=no_remote_model").unwrap();
        assert_eq!(r.path.as_deref(), Some("/data/a=b/**"));
        assert_eq!(r.label, EgressLabel::no_remote_model());
    }

    #[test]
    fn rejects_missing_label_and_empty_source() {
        assert!(parse_rule_spec("email.*").is_err());
        assert!(parse_rule_spec("=local_only").is_err());
    }

    #[test]
    fn rejects_unknown_label() {
        assert!(parse_rule_spec("email.*=super_secret").is_err());
        assert_eq!(
            parse_named_label("LOCAL-ONLY").unwrap(),
            NamedEgressLabel::LocalOnly
        );
    }
}
