use serde::Serialize;
use std::fmt;

/// Typed error for context overflow situations.
#[derive(Debug, Clone, Serialize)]
pub struct ContextOverflowError {
    pub diagnostic: ContextOverflowDiagnostic,
}

impl fmt::Display for ContextOverflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "context overflow after {} actions: {} tokens vs limit {}",
            self.diagnostic.actions_attempted.len(),
            self.diagnostic.budget_snapshot.estimated_input,
            self.diagnostic.budget_snapshot.window.unwrap_or(0),
        )
    }
}

impl std::error::Error for ContextOverflowError {}

/// Structured diagnostic emitted when the governor fails to reduce context.
#[derive(Debug, Clone, Serialize)]
pub struct ContextOverflowDiagnostic {
    pub budget_snapshot: BudgetSnapshot,
    pub actions_attempted: Vec<GovernorAction>,
    pub recovery_action: RecoveryAction,
}

/// Snapshot of budget state at the time of overflow.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetSnapshot {
    pub estimated_input: usize,
    pub margin: usize,
    pub window: Option<usize>,
    pub threshold_pct: f64,
}

/// What the governor did (or didn't) do to recover.
#[derive(Debug, Clone, Serialize)]
pub enum RecoveryAction {
    Compressed,
    Trimmed,
    Failed,
}

/// Record of a single strategy firing in the pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct GovernorAction {
    pub strategy: String,
    pub tokens_after: usize,
}
