use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementLevel {
    L1,
    L2,
    L3,
}

impl fmt::Display for ImprovementLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImprovementLevel::L1 => write!(f, "l1"),
            ImprovementLevel::L2 => write!(f, "l2"),
            ImprovementLevel::L3 => write!(f, "l3"),
        }
    }
}

impl FromStr for ImprovementLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim_matches('"').to_lowercase().as_str() {
            "l1" => Ok(ImprovementLevel::L1),
            "l2" => Ok(ImprovementLevel::L2),
            "l3" => Ok(ImprovementLevel::L3),
            other => Err(format!("unknown ImprovementLevel: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CycleOutcome {
    Success,
    Regression,
    Rejected,
    Cancelled,
}

impl fmt::Display for CycleOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CycleOutcome::Success => write!(f, "success"),
            CycleOutcome::Regression => write!(f, "regression"),
            CycleOutcome::Rejected => write!(f, "rejected"),
            CycleOutcome::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl FromStr for CycleOutcome {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim_matches('"').to_lowercase().as_str() {
            "success" => Ok(CycleOutcome::Success),
            "regression" => Ok(CycleOutcome::Regression),
            "rejected" => Ok(CycleOutcome::Rejected),
            "cancelled" => Ok(CycleOutcome::Cancelled),
            other => Err(format!("unknown CycleOutcome: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImprovementCycleRecord {
    pub cycle_id: String,
    pub agent_id: String,
    pub level: ImprovementLevel,
    pub outcome: CycleOutcome,
    pub regression_detected: bool,
    pub operator_decision: String,
    pub session_id: Option<String>,
    pub revision_before: Option<String>,
    pub revision_after: Option<String>,
    pub blast_radius_score: Option<f64>,
    pub created_at: String,
    pub closed_at: Option<String>,
}
