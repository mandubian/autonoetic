//! Export a full session into a single human-readable archive.
//!
//! Supports plain Markdown (`room` format) for agent review and JSON for
//! programmatic consumption. The artifact is intentionally unsigned; signing
//! can be layered on later if needed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use autonoetic_types::background::{ApprovalRequest, SessionApprovalGrant, UserInteraction};
use autonoetic_types::causal_chain::{CausalEventRecord, ExecutionTraceRecord};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::session_outcome::SessionOutcome;
use autonoetic_types::session_timeline::{Altitude, SessionTimelineEntry, SessionTimelineListResult};
use serde::{Deserialize, Serialize};

use crate::runtime::checkpoint::{list_checkpoints, load_checkpoint, SessionCheckpoint};
use crate::runtime::content_store::root_session_id;
use crate::scheduler::gateway_store::{EmergencyStopRecord, GatewayStore, LiveDigestEventRecord};

/// Export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Markdown narrative optimized for agent review.
    Room,
    /// Markdown with collapsible raw JSON sections, no narrative rewriting.
    RoomRaw,
    /// Single structured JSON dump.
    Json,
}

impl std::str::FromStr for ExportFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "room" => Ok(ExportFormat::Room),
            "room-raw" => Ok(ExportFormat::RoomRaw),
            "json" => Ok(ExportFormat::Json),
            _ => anyhow::bail!("unknown export format '{}'; expected room, room-raw, or json", s),
        }
    }
}

/// Options controlling what goes into the export.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// Minimum timeline altitude to include. `None` includes everything.
    pub min_altitude: Option<Altitude>,
    /// Include checkpoint files (full message history) in the appendix.
    pub with_checkpoints: bool,
    /// Cap for large collections to keep exports readable.
    pub row_limit: i64,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Room,
            min_altitude: None,
            with_checkpoints: false,
            row_limit: 10_000,
        }
    }
}

/// A single checkpoint bundled into the export.
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct ExportedCheckpoint {
    pub turn_id: String,
    pub turn_counter: u64,
    pub created_at: String,
    pub yield_reason: String,
    pub message_count: usize,
    pub checkpoint: SessionCheckpoint,
}

/// Serializable snapshot of a gateway-store emergency stop record.
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct ExportedEmergencyStop {
    pub stop_id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub root_session_id: String,
    pub workflow_id: Option<String>,
    pub requested_by_type: String,
    pub requested_by_id: String,
    pub reason: Option<String>,
    pub trigger_kind: String,
    pub mode: String,
    pub status: String,
    pub requested_at: String,
    pub completed_at: Option<String>,
    pub details_json: Option<String>,
}

impl From<&EmergencyStopRecord> for ExportedEmergencyStop {
    fn from(r: &EmergencyStopRecord) -> Self {
        Self {
            stop_id: r.stop_id.clone(),
            scope_type: r.scope_type.clone(),
            scope_id: r.scope_id.clone(),
            root_session_id: r.root_session_id.clone(),
            workflow_id: r.workflow_id.clone(),
            requested_by_type: r.requested_by_type.clone(),
            requested_by_id: r.requested_by_id.clone(),
            reason: r.reason.clone(),
            trigger_kind: r.trigger_kind.clone(),
            mode: r.mode.clone(),
            status: r.status.clone(),
            requested_at: r.requested_at.clone(),
            completed_at: r.completed_at.clone(),
            details_json: r.details_json.clone(),
        }
    }
}

/// Serializable snapshot of a session envelope record.
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct ExportedEnvelope {
    pub id: i64,
    pub root_session_id: String,
    pub capability: serde_json::Value,
    pub source: String,
    pub observed_at: Option<String>,
    pub locked_at: Option<String>,
    pub locked_by: Option<String>,
    pub plan_id: Option<String>,
    pub created_at: String,
}

impl From<&crate::scheduler::gateway_store::session_envelopes::SessionEnvelopeRecord>
    for ExportedEnvelope
{
    fn from(r: &crate::scheduler::gateway_store::session_envelopes::SessionEnvelopeRecord) -> Self {
        Self {
            id: r.id,
            root_session_id: r.root_session_id.clone(),
            capability: serde_json::to_value(&r.capability,
            )
            .unwrap_or(serde_json::Value::String("(unserializable)".to_string())),
            source: r.source.clone(),
            observed_at: r.observed_at.clone(),
            locked_at: r.locked_at.clone(),
            locked_by: r.locked_by.clone(),
            plan_id: r.plan_id.clone(),
            created_at: r.created_at.clone(),
        }
    }
}

/// Serializable snapshot of export options.
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct ExportOptionsSnapshot {
    pub format: String,
    pub min_altitude: Option<String>,
    pub with_checkpoints: bool,
    pub row_limit: i64,
}

impl From<&ExportOptions> for ExportOptionsSnapshot {
    fn from(opts: &ExportOptions) -> Self {
        Self {
            format: format!("{:?}", opts.format).to_lowercase(),
            min_altitude: opts.min_altitude.map(|a| a.as_str().to_string()),
            with_checkpoints: opts.with_checkpoints,
            row_limit: opts.row_limit,
        }
    }
}

/// Complete session export payload.
#[derive(Deserialize, Debug, Clone, Serialize)]
pub struct SessionExport {
    pub session_id: String,
    pub root_session_id: String,
    pub export_generated_at: String,
    pub export_options: ExportOptionsSnapshot,

    // Identity / lineage
    pub parent_session_id: Option<String>,
    pub fork_source_session_id: Option<String>,
    pub spawn_lineage: Vec<autonoetic_types::session_timeline::SessionSpawnLineageEntry>,

    // Summaries
    pub outcome: Option<SessionOutcome>,

    // SQLite-backed event streams
    pub timeline: SessionTimelineListResult,
    pub causal_events: Vec<CausalEventRecord>,
    pub execution_traces: Vec<ExecutionTraceRecord>,
    pub approvals: Vec<ApprovalRequest>,
    pub user_interactions: Vec<UserInteraction>,
    pub session_grants: Vec<SessionApprovalGrant>,
    pub emergency_stops: Vec<ExportedEmergencyStop>,
    pub envelopes: Vec<ExportedEnvelope>,

    // On-disk narrative artifacts
    pub digest_markdown: Option<String>,
    pub overview_markdown: Option<String>,
    pub report_json: Option<serde_json::Value>,

    // Optional full checkpoint history
    pub checkpoints: Vec<ExportedCheckpoint>,
}

/// Export the session identified by `session_id`.
///
/// `session_id` may be a root session or any child/spawn session; the export
/// always covers the entire root session tree.
pub fn export_session(
    store: &GatewayStore,
    config: &GatewayConfig,
    session_id: &str,
    opts: &ExportOptions,
) -> Result<SessionExport> {
    let root_session_id = root_session_id(session_id).to_string();
    let gateway_dir = crate::execution::gateway_root_dir(&config);
    let sessions_dir = gateway_dir.join("sessions").join(&root_session_id);

    let export_generated_at = chrono::Utc::now().to_rfc3339();

    let outcome = store
        .get_session_outcome(session_id)
        .context("failed to read session_outcome")?;

    let timeline = store
        .list_session_timeline(
            &root_session_id,
            None,
            opts.row_limit as u32,
            opts.min_altitude,
            None,
        )
        .context("failed to read session timeline")?;

    let causal_events = store
        .search_causal_events(Some(session_id), None, opts.row_limit)
        .context("failed to read causal events")?;

    let execution_traces = store
        .search_execution_traces(
            None,
            None,
            None,
            None,
            None,
            Some(session_id),
            opts.row_limit,
        )
        .context("failed to read execution traces")?;

    let approvals = store
        .list_all_approvals_for_session(session_id)
        .context("failed to read approvals")?;

    let user_interactions = store
        .list_user_interactions_for_session_trace(session_id)
        .context("failed to read user interactions")?;

    let session_grants = store
        .get_session_grants_structured(&root_session_id)
        .context("failed to read session approval grants")?;

    let emergency_stops = store
        .list_emergency_stops_for_root_session(&root_session_id)
        .context("failed to read emergency stops")?
        .iter()
        .map(ExportedEmergencyStop::from)
        .collect();

    let envelopes = store
        .get_active_envelopes(&root_session_id)
        .context("failed to read session envelopes")?
        .iter()
        .map(ExportedEnvelope::from)
        .collect();

    let spawn_lineage = store
        .list_session_spawn_lineage(&root_session_id)
        .context("failed to read spawn lineage")?;

    let fork_source_session_id = store
        .get_fork_source(session_id)
        .context("failed to read fork lineage")?;

    let parent_session_id = spawn_lineage
        .iter()
        .find(|e| e.child_session_id == session_id)
        .map(|e| e.parent_session_id.clone());

    let digest_markdown = read_optional_file(&sessions_dir.join("digest.md"));
    let overview_markdown = read_optional_file(&sessions_dir.join("session_overview.md"));
    let report_json = read_optional_json(&sessions_dir.join("session_report.json"));

    let checkpoints = if opts.with_checkpoints {
        load_checkpoints_for_session(config, session_id)
            .context("failed to load checkpoints")?
    } else {
        Vec::new()
    };

    Ok(SessionExport {
        session_id: session_id.to_string(),
        root_session_id,
        export_generated_at,
        export_options: ExportOptionsSnapshot::from(opts),
        parent_session_id,
        fork_source_session_id,
        spawn_lineage,
        outcome,
        timeline,
        causal_events,
        execution_traces,
        approvals,
        user_interactions,
        session_grants,
        emergency_stops,
        envelopes,
        digest_markdown,
        overview_markdown,
        report_json,
        checkpoints,
    })
}

fn read_optional_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn read_optional_json(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn load_checkpoints_for_session(
    config: &GatewayConfig,
    session_id: &str,
) -> Result<Vec<ExportedCheckpoint>> {
    let turn_ids = list_checkpoints(config, session_id)?;
    let mut out = Vec::with_capacity(turn_ids.len());
    for turn_id in turn_ids {
        if let Some(checkpoint) = load_checkpoint(config, session_id, &turn_id)? {
            out.push(ExportedCheckpoint {
                turn_id: turn_id.clone(),
                turn_counter: checkpoint.turn_counter,
                created_at: checkpoint.created_at.clone(),
                yield_reason: format!("{:?}", checkpoint.yield_reason),
                message_count: checkpoint.history.len(),
                checkpoint,
            });
        }
    }
    Ok(out)
}

/// Render the export to the configured output format.
pub fn render_export(export: &SessionExport, opts: &ExportOptions) -> Result<String> {
    match opts.format {
        ExportFormat::Room => render_room_markdown(export),
        ExportFormat::RoomRaw => render_room_raw_markdown(export),
        ExportFormat::Json => render_json(export),
    }
}

/// Render a wiki-style archive: a directory of interlinked Markdown files.
/// Each file is kept small enough for LLM/human consumption.
pub fn render_wiki(export: &SessionExport, wiki_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    WikiRenderer::new(export, wiki_dir).render()
}

struct WikiRenderer<'a> {
    export: &'a SessionExport,
    wiki_dir: &'a Path,
    written: Vec<(String, PathBuf)>,
}

impl<'a> WikiRenderer<'a> {
    fn new(export: &'a SessionExport, wiki_dir: &'a Path) -> Self {
        Self {
            export,
            wiki_dir,
            written: Vec::new(),
        }
    }

    fn render(mut self) -> Result<Vec<(String, PathBuf)>> {
        std::fs::create_dir_all(self.wiki_dir)
            .with_context(|| format!("failed to create wiki dir {}", self.wiki_dir.display()))?;

        // Top-level narrative pages.
        self.write_page("index.md", self.render_index()?)?;
        self.write_page("metadata.md", self.render_metadata()?)?;
        self.write_page("summary.md", self.render_summary()?)?;
        self.write_page("issues.md", self.render_issues()?)?;
        self.write_page("approvals.md", self.render_approvals()?)?;
        self.write_page("interactions.md", self.render_interactions()?)?;

        // Timeline split by turn.
        self.render_timeline()?;

        // Tool execution log split by tool name.
        self.render_tools()?;

        // Checkpoints split by turn.
        if !self.export.checkpoints.is_empty() {
            self.render_checkpoints()?;
        }

        // Raw data appendices.
        self.render_raw_data()?;

        // Original on-disk artifacts.
        if let Some(digest) = &self.export.digest_markdown {
            self.write_page("digest.md", digest.clone())?;
        }
        if let Some(overview) = &self.export.overview_markdown {
            self.write_page("overview.md", overview.clone())?;
        }

        Ok(self.written)
    }

    fn write_page(&mut self, name: &str, content: String) -> Result<()> {
        let path = self.wiki_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write wiki page {}", path.display()))?;
        self.written.push((name.to_string(), path));
        Ok(())
    }

    fn render_index(&self) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "# Session `{}`", self.export.session_id)?;
        writeln!(&mut out)?;
        writeln!(&mut out, "Wiki-style archive generated at `{}`.", self.export.export_generated_at)?;
        writeln!(&mut out)?;

        writeln!(&mut out, "## Navigation")?;
        writeln!(&mut out, "- [Metadata](metadata.md)")?;
        writeln!(&mut out, "- [Executive Summary](summary.md)")?;
        writeln!(&mut out, "- [Issues & Escalations](issues.md) ({} error-level events, {} emergency stops)",
            self.export.timeline.entries.iter().filter(|e| matches!(e.altitude, Altitude::Error)).count(),
            self.export.emergency_stops.len()
        )?;
        writeln!(&mut out, "- [Approvals](approvals.md) ({})", self.export.approvals.len())?;
        writeln!(&mut out, "- [User Interactions](interactions.md) ({})", self.export.user_interactions.len())?;
        writeln!(&mut out, "- [Timeline](timeline/index.md) ({} entries)", self.export.timeline.entries.len())?;
        writeln!(&mut out, "- [Tool Execution Log](tools/index.md) ({} traces)", self.export.execution_traces.len())?;
        if !self.export.checkpoints.is_empty() {
            writeln!(&mut out, "- [Checkpoints](checkpoints/index.md) ({})", self.export.checkpoints.len())?;
        }
        writeln!(&mut out, "- [Raw Data](raw/index.md)")?;
        if self.export.digest_markdown.is_some() {
            writeln!(&mut out, "- [Original Digest](digest.md)")?;
        }
        if self.export.overview_markdown.is_some() {
            writeln!(&mut out, "- [Original Overview](overview.md)")?;
        }
        writeln!(&mut out)?;

        // Compact summary table.
        writeln!(&mut out, "## Quick Stats")?;
        writeln!(&mut out, "| Metric | Value |")?;
        writeln!(&mut out, "|---|---|")?;
        writeln!(&mut out, "| Session | `{}` |", self.export.session_id)?;
        writeln!(&mut out, "| Root session | `{}` |", self.export.root_session_id)?;
        if let Some(outcome) = &self.export.outcome {
            writeln!(&mut out, "| Agent | `{}` |", outcome.source_agent_id)?;
            writeln!(&mut out, "| Completion | `{:?}` |", outcome.completion)?;
            writeln!(&mut out, "| Turns | {} |", outcome.turns)?;
            writeln!(&mut out, "| Tokens | {} |", outcome.tokens.total)?;
            writeln!(&mut out, "| Cost USD | ${:.6} |", outcome.cost_usd)?;
        }
        writeln!(&mut out, "| Timeline entries | {} |", self.export.timeline.entries.len())?;
        writeln!(&mut out, "| Causal events | {} |", self.export.causal_events.len())?;
        writeln!(&mut out, "| Execution traces | {} |", self.export.execution_traces.len())?;
        writeln!(&mut out, "| Approvals | {} |", self.export.approvals.len())?;
        writeln!(&mut out, "| User interactions | {} |", self.export.user_interactions.len())?;
        writeln!(&mut out)?;

        // Recent error-level events inline.
        let errors: Vec<_> = self.export.timeline.entries.iter()
            .filter(|e| matches!(e.altitude, Altitude::Error))
            .take(10)
            .collect();
        if !errors.is_empty() {
            writeln!(&mut out, "## Recent Errors")?;
            for e in &errors {
                writeln!(&mut out, "- `{}` `{}` — {}", e.occurred_at, e.event_type,
                    e.payload.as_deref().unwrap_or(""))?;
            }
            if errors.len() < self.export.timeline.entries.iter().filter(|e| matches!(e.altitude, Altitude::Error)).count() {
                writeln!(&mut out, "_See [issues.md](issues.md) for the full list._")?;
            }
            writeln!(&mut out)?;
        }

        Ok(out)
    }

    fn render_metadata(&self) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "# Metadata")?;
        writeln!(&mut out)?;
        writeln!(&mut out, "- **Session ID**: `{}`", self.export.session_id)?;
        writeln!(&mut out, "- **Root session ID**: `{}`", self.export.root_session_id)?;
        if let Some(parent) = &self.export.parent_session_id {
            writeln!(&mut out, "- **Parent session**: `{}`", parent)?;
        }
        if let Some(fork) = &self.export.fork_source_session_id {
            writeln!(&mut out, "- **Forked from**: `{}`", fork)?;
        }
        writeln!(&mut out, "- **Exported at**: {}", self.export.export_generated_at)?;
        writeln!(&mut out, "- **Export options**: `{:?}`", self.export.export_options)?;
        writeln!(&mut out)?;

        if let Some(outcome) = &self.export.outcome {
            writeln!(&mut out, "## Outcome")?;
            writeln!(&mut out, "- **Agent**: {}", outcome.source_agent_id)?;
            writeln!(&mut out, "- **Status**: {:?}", outcome.completion)?;
            writeln!(&mut out, "- **Turns**: {}", outcome.turns)?;
            writeln!(&mut out, "- **Tokens**: {}", outcome.tokens.total)?;
            writeln!(&mut out, "- **Cost USD**: ${:.6}", outcome.cost_usd)?;
            writeln!(&mut out, "- **Wall clock seconds**: {:.2}", outcome.wall_clock_secs)?;
            if let Some(goal) = &outcome.task_goal {
                writeln!(&mut out, "- **Task goal**: {}", goal)?;
            }
            if let Some(rating) = &outcome.operator_rating {
                writeln!(&mut out, "- **Operator rating**: {:?}", rating.thumb)?;
                if let Some(note) = &rating.note {
                    writeln!(&mut out, "- **Operator note**: {}", note)?;
                }
            }
            writeln!(&mut out)?;
        }

        if !self.export.spawn_lineage.is_empty() {
            writeln!(&mut out, "## Spawn Lineage")?;
            for entry in &self.export.spawn_lineage {
                writeln!(&mut out, "- `{}` spawned from `{}` at turn {} (agent `{}`)",
                    entry.child_session_id, entry.parent_session_id, entry.spawned_at_turn, entry.target_agent_id)?;
            }
            writeln!(&mut out)?;
        }

        Ok(out)
    }

    fn render_summary(&self) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "# Executive Summary")?;
        writeln!(&mut out)?;
        if let Some(digest) = &self.export.digest_markdown {
            // Keep the summary page small; link to the full digest.
            let lines: Vec<_> = digest.lines().collect();
            let preview_lines = lines.len().min(100);
            for line in &lines[..preview_lines] {
                writeln!(&mut out, "{}", line)?;
            }
            if lines.len() > 100 {
                writeln!(&mut out)?;
                writeln!(&mut out, "_... {} more lines; see [full digest](digest.md) ..._", lines.len() - 100)?;
            }
        } else {
            writeln!(&mut out, "_No digest available for this session._")?;
        }
        Ok(out)
    }

    fn render_issues(&self) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "# Issues & Escalations")?;
        writeln!(&mut out)?;

        let errors: Vec<_> = self.export.timeline.entries.iter()
            .filter(|e| matches!(e.altitude, Altitude::Error))
            .collect();

        if errors.is_empty() && self.export.emergency_stops.is_empty() {
            writeln!(&mut out, "_No error-level events or emergency stops recorded._")?;
        } else {
            writeln!(&mut out, "## Error-level timeline events ({})", errors.len())?;
            for e in &errors {
                writeln!(&mut out, "- `{}` `{}` — {}", e.occurred_at, e.event_type,
                    e.payload.as_deref().unwrap_or(""))?;
            }
            writeln!(&mut out)?;

            if !self.export.emergency_stops.is_empty() {
                writeln!(&mut out, "## Emergency stops ({})", self.export.emergency_stops.len())?;
                for stop in &self.export.emergency_stops {
                    writeln!(&mut out, "- **{}** (`{}`): {}", stop.stop_id, stop.status,
                        stop.reason.as_deref().unwrap_or("no reason"))?;
                }
                writeln!(&mut out)?;
            }
        }

        Ok(out)
    }

    fn render_approvals(&self) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "# Approvals")?;
        writeln!(&mut out)?;
        if self.export.approvals.is_empty() {
            writeln!(&mut out, "_No approval requests._")?;
        } else {
            for a in &self.export.approvals {
                writeln!(&mut out, "## `{}`", a.request_id)?;
                writeln!(&mut out, "- **Created**: {}", a.created_at)?;
                writeln!(&mut out, "- **Kind**: `{}`", a.action.kind())?;
                writeln!(&mut out, "- **Status**: {:?}", a.status)?;
                if let Some(reason) = &a.reason {
                    writeln!(&mut out, "- **Reason**: {}", reason)?;
                }
                if let Some(decided_by) = &a.decided_by {
                    writeln!(&mut out, "- **Decided by**: {}", decided_by)?;
                }
                if let Some(decision_reason) = &a.decision_reason {
                    writeln!(&mut out, "- **Decision reason**: {}", decision_reason)?;
                }
                writeln!(&mut out, "- **Payload**: `{}`", format!("{:?}", a.action).chars().take(200).collect::<String>())?;
                writeln!(&mut out)?;
            }
        }
        Ok(out)
    }

    fn render_interactions(&self) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "# User Interactions")?;
        writeln!(&mut out)?;
        if self.export.user_interactions.is_empty() {
            writeln!(&mut out, "_No user.ask interactions._")?;
        } else {
            for ui in &self.export.user_interactions {
                writeln!(&mut out, "## `{}`", ui.interaction_id)?;
                writeln!(&mut out, "- **Kind**: {:?}", ui.kind)?;
                writeln!(&mut out, "- **Status**: {:?}", ui.status)?;
                writeln!(&mut out, "- **Question**: {}", ui.question)?;
                if let Some(answer) = &ui.answer_text {
                    writeln!(&mut out, "- **Answer**: {}", answer)?;
                }
                if let Some(option) = &ui.answer_option_id {
                    writeln!(&mut out, "- **Selected option**: {}", option)?;
                }
                writeln!(&mut out)?;
            }
        }
        Ok(out)
    }

    fn render_timeline(&mut self) -> Result<()> {
        let timeline_dir = self.wiki_dir.join("timeline");
        std::fs::create_dir_all(&timeline_dir)?;

        const ENTRIES_PER_PAGE: usize = 50;

        // Group timeline entries by turn.
        let mut by_turn: BTreeMap<String, Vec<&SessionTimelineEntry>> = BTreeMap::new();
        let mut no_turn: Vec<&SessionTimelineEntry> = Vec::new();
        for e in &self.export.timeline.entries {
            if let Some(turn) = &e.turn_id {
                by_turn.entry(turn.clone()).or_default().push(e);
            } else {
                no_turn.push(e);
            }
        }

        // Index page.
        let mut index = String::new();
        writeln!(&mut index, "# Timeline")?;
        writeln!(&mut index)?;
        writeln!(&mut index, "Total entries: {}", self.export.timeline.entries.len())?;
        writeln!(&mut index)?;
        writeln!(&mut index, "## Turns")?;
        for (turn, entries) in &by_turn {
            let pages = page_count(entries.len(), ENTRIES_PER_PAGE);
            if pages == 1 {
                let filename = format!("{}.md", sanitize_filename(turn));
                writeln!(&mut index, "- [{}]({}) ({} entries)", turn, filename, entries.len())?;
            } else {
                writeln!(&mut index, "- **{}** ({} entries)", turn, entries.len())?;
                for p in 1..=pages {
                    let filename = format!("{}-p{}.md", sanitize_filename(turn), p);
                    let start = (p - 1) * ENTRIES_PER_PAGE + 1;
                    let end = (p * ENTRIES_PER_PAGE).min(entries.len());
                    writeln!(&mut index, "  - [page {}]({}) (entries {}–{})", p, filename, start, end)?;
                }
            }
        }
        if !no_turn.is_empty() {
            let pages = page_count(no_turn.len(), ENTRIES_PER_PAGE);
            if pages == 1 {
                writeln!(&mut index, "- [Other events](other.md) ({} entries)", no_turn.len())?;
            } else {
                writeln!(&mut index, "- **Other events** ({} entries)", no_turn.len())?;
                for p in 1..=pages {
                    let filename = format!("other-p{}.md", p);
                    let start = (p - 1) * ENTRIES_PER_PAGE + 1;
                    let end = (p * ENTRIES_PER_PAGE).min(no_turn.len());
                    writeln!(&mut index, "  - [page {}]({}) (entries {}–{})", p, filename, start, end)?;
                }
            }
        }
        self.write_page("timeline/index.md", index)?;

        // Per-turn pages (chunked).
        for (turn, entries) in &by_turn {
            let pages = page_count(entries.len(), ENTRIES_PER_PAGE);
            for p in 1..=pages {
                let start = (p - 1) * ENTRIES_PER_PAGE;
                let end = (p * ENTRIES_PER_PAGE).min(entries.len());
                let chunk = &entries[start..end];

                let mut out = String::new();
                writeln!(&mut out, "# Timeline: {} (page {}/{})", turn, p, pages)?;
                writeln!(&mut out)?;
                writeln!(&mut out, "[Back to timeline index](index.md)")?;
                if p > 1 {
                    let prev = format!("{}-p{}.md", sanitize_filename(turn), p - 1);
                    writeln!(&mut out, " | [Previous]({})", prev)?;
                }
                if p < pages {
                    let next = format!("{}-p{}.md", sanitize_filename(turn), p + 1);
                    writeln!(&mut out, " | [Next]({})", next)?;
                }
                writeln!(&mut out)?;
                for e in chunk {
                    writeln!(&mut out, "- `{}` `{}` `{}` — {}", e.occurred_at,
                        e.principal.id, e.event_type, e.payload.as_deref().unwrap_or(""))?;
                }

                let filename = if pages == 1 {
                    format!("timeline/{}.md", sanitize_filename(turn))
                } else {
                    format!("timeline/{}-p{}.md", sanitize_filename(turn), p)
                };
                self.write_page(&filename, out)?;
            }
        }

        // No-turn events (chunked).
        if !no_turn.is_empty() {
            let pages = page_count(no_turn.len(), ENTRIES_PER_PAGE);
            for p in 1..=pages {
                let start = (p - 1) * ENTRIES_PER_PAGE;
                let end = (p * ENTRIES_PER_PAGE).min(no_turn.len());
                let chunk = &no_turn[start..end];

                let mut out = String::new();
                writeln!(&mut out, "# Timeline: Other Events (page {}/{})", p, pages)?;
                writeln!(&mut out)?;
                writeln!(&mut out, "[Back to timeline index](index.md)")?;
                if p > 1 {
                    writeln!(&mut out, " | [Previous](other-p{}.md)", p - 1)?;
                }
                if p < pages {
                    writeln!(&mut out, " | [Next](other-p{}.md)", p + 1)?;
                }
                writeln!(&mut out)?;
                for e in chunk {
                    writeln!(&mut out, "- `{}` `{}` `{}` — {}", e.occurred_at,
                        e.principal.id, e.event_type, e.payload.as_deref().unwrap_or(""))?;
                }

                let filename = if pages == 1 {
                    "timeline/other.md".to_string()
                } else {
                    format!("timeline/other-p{}.md", p)
                };
                self.write_page(&filename, out)?;
            }
        }

        Ok(())
    }

    fn render_tools(&mut self) -> Result<()> {
        let tools_dir = self.wiki_dir.join("tools");
        std::fs::create_dir_all(&tools_dir)?;

        // Group by tool name.
        let mut by_tool: BTreeMap<String, Vec<&ExecutionTraceRecord>> = BTreeMap::new();
        for t in &self.export.execution_traces {
            by_tool.entry(t.tool_name.clone()).or_default().push(t);
        }

        // Index page.
        let mut index = String::new();
        writeln!(&mut index, "# Tool Execution Log")?;
        writeln!(&mut index)?;
        writeln!(&mut index, "Total traces: {}", self.export.execution_traces.len())?;
        writeln!(&mut index)?;
        writeln!(&mut index, "## Tools")?;
        for (tool, traces) in &by_tool {
            let filename = format!("{}.md", sanitize_filename(tool));
            writeln!(&mut index, "- [{}]({}) ({} traces)", tool, filename, traces.len())?;
        }
        self.write_page("tools/index.md", index)?;

        // Per-tool pages.
        for (tool, traces) in &by_tool {
            let mut out = String::new();
            writeln!(&mut out, "# Tool: `{}`", tool)?;
            writeln!(&mut out)?;
            writeln!(&mut out, "[Back to tool index](index.md)")?;
            writeln!(&mut out)?;
            for t in traces {
                let status = if t.success != 0 { "ok" } else { "FAIL" };
                writeln!(&mut out, "- `{}` `{}` — exit={:?} — {}", t.timestamp, status,
                    t.exit_code, t.error_summary.as_deref().unwrap_or(""))?;
            }
            self.write_page(&format!("tools/{}.md", sanitize_filename(tool)), out)?;
        }

        Ok(())
    }

    fn render_checkpoints(&mut self) -> Result<()> {
        let cp_dir = self.wiki_dir.join("checkpoints");
        std::fs::create_dir_all(&cp_dir)?;

        let mut index = String::new();
        writeln!(&mut index, "# Checkpoints")?;
        writeln!(&mut index)?;
        writeln!(&mut index, "Total checkpoints: {}", self.export.checkpoints.len())?;
        writeln!(&mut index)?;
        for cp in &self.export.checkpoints {
            let filename = format!("{}.md", sanitize_filename(&cp.turn_id));
            writeln!(&mut index, "- [{}]({}) — {} messages, yield: `{}`",
                cp.turn_id, filename, cp.message_count, cp.yield_reason)?;
        }
        self.write_page("checkpoints/index.md", index)?;

        for cp in &self.export.checkpoints {
            let mut out = String::new();
            writeln!(&mut out, "# Checkpoint: {}", cp.turn_id)?;
            writeln!(&mut out)?;
            writeln!(&mut out, "[Back to checkpoints index](index.md)")?;
            writeln!(&mut out)?;
            writeln!(&mut out, "- **Turn counter**: {}", cp.turn_counter)?;
            writeln!(&mut out, "- **Created at**: {}", cp.created_at)?;
            writeln!(&mut out, "- **Yield reason**: `{}`", cp.yield_reason)?;
            writeln!(&mut out, "- **Messages**: {}", cp.message_count)?;
            writeln!(&mut out)?;
            writeln!(&mut out, "## Messages")?;
            for (i, msg) in cp.checkpoint.history.iter().enumerate() {
                writeln!(&mut out, "### Message {}", i)?;
                writeln!(&mut out, "- **Role**: {:?}", msg.role)?;
                if !msg.content.is_empty() {
                    let preview: String = msg.content.chars().take(500).collect();
                    writeln!(&mut out, "- **Content**: {}", preview)?;
                    if msg.content.len() > 500 {
                        writeln!(&mut out, "  _... truncated ({}/{} chars) ..._", 500, msg.content.len())?;
                    }
                }
                if !msg.tool_calls.is_empty() {
                    writeln!(&mut out, "- **Tool calls**: {:?}", msg.tool_calls)?;
                }
                if let Some(tc_id) = &msg.tool_call_id {
                    writeln!(&mut out, "- **Tool call id**: {}", tc_id)?;
                }
                writeln!(&mut out)?;
            }
            self.write_page(&format!("checkpoints/{}.md", sanitize_filename(&cp.turn_id)), out)?;
        }

        Ok(())
    }

    fn render_raw_data(&mut self) -> Result<()> {
        let raw_dir = self.wiki_dir.join("raw");
        std::fs::create_dir_all(&raw_dir)?;

        let mut index = String::new();
        writeln!(&mut index, "# Raw Data")?;
        writeln!(&mut index)?;
        writeln!(&mut index, "Structured data exports for programmatic reference.")?;
        writeln!(&mut index)?;

        self.write_raw_json("raw/causal-events.json", "Causal Events", &self.export.causal_events)?;
        writeln!(&mut index, "- [Causal Events](causal-events.json)")?;

        self.write_raw_json("raw/execution-traces.json", "Execution Traces", &self.export.execution_traces)?;
        writeln!(&mut index, "- [Execution Traces](execution-traces.json)")?;

        self.write_raw_json("raw/approvals.json", "Approvals", &self.export.approvals)?;
        writeln!(&mut index, "- [Approvals](approvals.json)")?;

        self.write_raw_json("raw/user-interactions.json", "User Interactions", &self.export.user_interactions)?;
        writeln!(&mut index, "- [User Interactions](user-interactions.json)")?;

        self.write_raw_json("raw/session-grants.json", "Session Grants", &self.export.session_grants)?;
        writeln!(&mut index, "- [Session Grants](session-grants.json)")?;

        self.write_raw_json("raw/emergency-stops.json", "Emergency Stops", &self.export.emergency_stops)?;
        writeln!(&mut index, "- [Emergency Stops](emergency-stops.json)")?;

        self.write_raw_json("raw/envelopes.json", "Envelopes", &self.export.envelopes)?;
        writeln!(&mut index, "- [Envelopes](envelopes.json)")?;

        self.write_raw_json("raw/spawn-lineage.json", "Spawn Lineage", &self.export.spawn_lineage)?;
        writeln!(&mut index, "- [Spawn Lineage](spawn-lineage.json)")?;

        self.write_page("raw/index.md", index)?;
        Ok(())
    }

    fn write_raw_json<T: Serialize>(
        &mut self,
        page_path: &str,
        title: &str,
        value: &T,
    ) -> Result<()> {
        let json = serde_json::to_string_pretty(value)
            .with_context(|| format!("failed to serialize {}", title))?;
        self.write_page(page_path, json)
    }
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn page_count(total: usize, per_page: usize) -> usize {
    if total == 0 {
        return 1;
    }
    (total + per_page - 1) / per_page
}
fn render_json(export: &SessionExport) -> Result<String> {
    Ok(serde_json::to_string_pretty(export)?)
}

fn render_room_markdown(export: &SessionExport) -> Result<String> {
    let mut out = String::new();

    writeln!(&mut out, "# Session Export: `{}`", export.session_id)?;
    writeln!(&mut out)?;

    // Metadata
    writeln!(&mut out, "## Metadata")?;
    writeln!(&mut out, "- **Root session**: `{}`", export.root_session_id)?;
    if let Some(parent) = &export.parent_session_id {
        writeln!(&mut out, "- **Parent session**: `{}`", parent)?;
    }
    if let Some(fork) = &export.fork_source_session_id {
        writeln!(&mut out, "- **Forked from**: `{}`", fork)?;
    }
    writeln!(
        &mut out,
        "- **Exported at**: {}",
        export.export_generated_at
    )?;
    writeln!(
        &mut out,
        "- **Format**: {} | checkpoints: {} | row_limit: {}",
        export.export_options.format,
        export.export_options.with_checkpoints,
        export.export_options.row_limit
    )?;
    if let Some(outcome) = &export.outcome {
        writeln!(&mut out, "- **Agent**: {}", outcome.source_agent_id)?;
        writeln!(&mut out, "- **Status**: {:?}", outcome.completion)?;
        if outcome.turns > 0 {
            writeln!(&mut out, "- **Turns**: {}", outcome.turns)?;
        }
        if outcome.tokens.total > 0 {
            writeln!(
                &mut out, "- **Tokens**: {}", outcome.tokens.total
            )?;
        }
        if outcome.cost_usd > 0.0 {
            writeln!(
                &mut out, "- **Cost USD**: ${:.6}", outcome.cost_usd
            )?;
        }
        if let Some(rating) = &outcome.operator_rating {
            writeln!(
                &mut out,
                "- **Operator rating**: {:?}",
                rating.thumb
            )?;
            if let Some(note) = &rating.note {
                writeln!(&mut out, "- **Operator note**: {}", note)?;
            }
        }
    }
    writeln!(&mut out)?;

    // Executive summary from digest / overview
    if let Some(digest) = &export.digest_markdown {
        writeln!(&mut out, "## Executive Summary")?;
        let excerpt: String = digest.lines().take(30).collect::<Vec<_>>().join("\n");
        writeln!(&mut out, "{}", excerpt)?;
        writeln!(&mut out)?;
    }

    // Issues & Escalations
    writeln!(&mut out, "## Issues & Escalations")?;
    let errors: Vec<_> = export
        .timeline
        .entries
        .iter()
        .filter(|e| matches!(e.altitude, Altitude::Error))
        .collect();
    if errors.is_empty() && export.emergency_stops.is_empty() {
        writeln!(
            &mut out, "_No error-level events or emergency stops recorded._")?;
    } else {
        for e in &errors {
            writeln!(
                &mut out,
                "- `{}` `{}` — {}",
                e.occurred_at,
                e.event_type,
                e.payload.as_deref().unwrap_or("")
            )?;
        }
        for stop in &export.emergency_stops {
            writeln!(
                &mut out,
                "- **Emergency stop** `{}` ({}): {}",
                stop.stop_id,
                stop.status,
                stop.reason.as_deref().unwrap_or("no reason")
            )?;
        }
    }
    writeln!(&mut out)?;

    // Approvals
    writeln!(&mut out, "## Approvals")?;
    if export.approvals.is_empty() {
        writeln!(&mut out, "_No approval requests._")?;
    } else {
        for a in &export.approvals {
            writeln!(
                &mut out,
                "- `{}` `{}` — `{}` — status: {:?}",
                a.created_at,
                a.action.kind(),
                format!("{:?}", a.action).chars().take(120).collect::<String>(),
                a.status
            )?;
        }
    }
    writeln!(&mut out)?;

    // User interactions
    writeln!(&mut out, "## User Interactions")?;
    if export.user_interactions.is_empty() {
        writeln!(&mut out, "_No user.ask interactions._")?;
    } else {
        for ui in &export.user_interactions {
            writeln!(
                &mut out,
                "- `{}` status: {:?} — {}",
                ui.interaction_id,
                ui.status,
                ui.question.as_str()
            )?;
        }
    }
    writeln!(&mut out)?;

    // Timeline
    writeln!(&mut out, "## Timeline")?;
    if export.timeline.entries.is_empty() {
        writeln!(&mut out, "_No timeline events._")?;
    } else {
        for e in &export.timeline.entries {
            writeln!(
                &mut out,
                "- `{}` `{}` `{}` — {}",
                e.occurred_at,
                e.principal.id,
                e.event_type,
                e.payload.as_deref().unwrap_or("")
            )?;
        }
    }
    writeln!(&mut out)?;

    // Tool execution log
    writeln!(&mut out, "## Tool Execution Log")?;
    if export.execution_traces.is_empty() {
        writeln!(&mut out, "_No execution traces._")?;
    } else {
        for t in &export.execution_traces {
            let status = if t.success != 0 { "ok" } else { "FAIL" };
            writeln!(
                &mut out,
                "- `{}` `{}` `{}` — {}",
                t.timestamp, t.tool_name, status, t.error_summary.as_deref().unwrap_or("")
            )?;
        }
    }
    writeln!(&mut out)?;

    // Checkpoints
    if !export.checkpoints.is_empty() {
        writeln!(&mut out, "## Checkpoints")?;
        for cp in &export.checkpoints {
            writeln!(
                &mut out,
                "- `{}` — {} messages, yield: {}",
                cp.turn_id, cp.message_count, cp.yield_reason
            )?;
        }
        writeln!(&mut out)?;
    }

    // Raw data appendix
    writeln!(&mut out, "## Raw Data")?;
    writeln!(&mut out, "<details>")?;
    writeln!(
        &mut out, "<summary>Full export JSON</summary>")?;
    writeln!(&mut out)?;
    writeln!(&mut out, "```json")?;
    writeln!(&mut out, "{}", serde_json::to_string_pretty(export)?)?;
    writeln!(&mut out, "```")?;
    writeln!(&mut out, "</details>")?;

    Ok(out)
}

fn render_room_raw_markdown(export: &SessionExport) -> Result<String> {
    let mut out = String::new();

    writeln!(&mut out, "# Session Export: `{}`", export.session_id)?;
    writeln!(&mut out)?;
    writeln!(
        &mut out,
        "_Plain archive generated at {}. Each section below is the raw JSON/Markdown source._",
        export.export_generated_at
    )?;
    writeln!(&mut out)?;

    section_json(
        &mut out,
        "Metadata",
        &serde_json::json!({
            "session_id": export.session_id,
            "root_session_id": export.root_session_id,
            "parent_session_id": export.parent_session_id,
            "fork_source_session_id": export.fork_source_session_id,
            "export_options": export.export_options,
        }),
    )?;

    if let Some(outcome) = &export.outcome {
        section_json(&mut out, "Outcome", outcome)?;
    }

    section_json(&mut out, "Timeline", &export.timeline)?;
    section_json(&mut out, "Causal Events", &export.causal_events)?;
    section_json(&mut out, "Execution Traces", &export.execution_traces)?;
    section_json(&mut out, "Approvals", &export.approvals)?;
    section_json(&mut out, "User Interactions", &export.user_interactions)?;
    section_json(&mut out, "Session Grants", &export.session_grants)?;
    section_json(&mut out, "Emergency Stops", &export.emergency_stops)?;
    section_json(&mut out, "Envelopes", &export.envelopes)?;
    section_json(&mut out, "Spawn Lineage", &export.spawn_lineage)?;

    if let Some(md) = &export.digest_markdown {
        section_markdown(&mut out, "Digest", md)?;
    }
    if let Some(md) = &export.overview_markdown {
        section_markdown(&mut out, "Overview", md)?;
    }
    if let Some(json) = &export.report_json {
        section_json(&mut out, "Session Report", json)?;
    }
    if !export.checkpoints.is_empty() {
        section_json(&mut out, "Checkpoints", &export.checkpoints)?;
    }

    Ok(out)
}

fn section_json(out: &mut String, title: &str, value: &impl Serialize) -> Result<()> {
    writeln!(out, "## {}", title)?;
    writeln!(out, "<details>")?;
    writeln!(out, "<summary>{}</summary>", title)?;
    writeln!(out)?;
    writeln!(out, "```json")?;
    writeln!(out, "{}", serde_json::to_string_pretty(value)?)?;
    writeln!(out, "```")?;
    writeln!(out, "</details>")?;
    writeln!(out)?;
    Ok(())
}

fn section_markdown(out: &mut String, title: &str, content: &str) -> Result<()> {
    writeln!(out, "## {}", title)?;
    writeln!(out, "<details>")?;
    writeln!(out, "<summary>{}</summary>", title)?;
    writeln!(out)?;
    writeln!(out, "{}", content)?;
    writeln!(out, "</details>")?;
    writeln!(out)?;
    Ok(())
}

use std::fmt::Write;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::gateway_store::GatewayStore;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn export_empty_session_renders_markdown() {
        let dir = tempdir().unwrap();
        let gateway_dir = dir.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).unwrap();
        let store = GatewayStore::open(&gateway_dir).unwrap();
        let config = GatewayConfig {
            runtime_dir: dir.path().to_path_buf().join(".gateway"),
            agents_dir: dir.path().to_path_buf(),
            ..GatewayConfig::default()
        };

        let export = export_session(
            &store, &config, "root-empty", &ExportOptions::default()
        )
        .expect("export should succeed for empty session");

        let md = render_room_markdown(&export).expect("render should succeed");
        assert!(md.contains("# Session Export: `root-empty`"));
        assert!(md.contains("## Metadata"));
        assert!(md.contains("## Timeline"));
    }

    #[test]
    fn export_unknown_format_errors() {
        let result = ExportFormat::from_str("xml");
        assert!(result.is_err());
    }
}
