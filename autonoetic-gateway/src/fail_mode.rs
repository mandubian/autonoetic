//! Unified fail-mode table (R++10).
//!
//! Every constitutional invariant has a declared failure action in one
//! place.  The five fail modes are:
//!
//! | Mode | Meaning |
//! |---|---|
//! | `RefuseBoot` | Gateway refuses to start. |
//! | `RefuseSessionStart` | Gateway refuses to create / resume / continue a session. Applies at session creation and at any mid-session enforcement point where the invariant cannot be verified (e.g. cost-budget catalog unavailable). |
//! | `Degrade` | Session enters degraded mode (R-7.18). |
//! | `EmergencyStop` | Session is killed immediately. |
//! | `LogOnly` | No enforcement action; event is logged for audit. |

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailMode {
    RefuseBoot,
    RefuseSessionStart,
    Degrade,
    EmergencyStop,
    LogOnly,
}

impl fmt::Display for FailMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailMode::RefuseBoot => write!(f, "refuse-boot"),
            FailMode::RefuseSessionStart => write!(f, "refuse-session-start"),
            FailMode::Degrade => write!(f, "degrade"),
            FailMode::EmergencyStop => write!(f, "emergency-stop"),
            FailMode::LogOnly => write!(f, "log-only"),
        }
    }
}

struct FailModeEntry {
    rule_id: &'static str,
    fail_mode: FailMode,
}

const FAIL_MODE_TABLE: &[FailModeEntry] = &[
    // §0 Rights
    FailModeEntry {
        rule_id: "Ri-0.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "Ri-0.2",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "Ri-0.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "Ri-0.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "Ri-0.5",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "Ri-0.6",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "Ri-0.7",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "Ri-0.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "Ri-0.9",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "Ri-0.10",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "Ri-0.11",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "Ri-0.12",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "Ri-0.13",
        fail_mode: FailMode::EmergencyStop,
    },
    // §1 Capability & Rights
    FailModeEntry {
        rule_id: "R-1.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-1.11",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §2 Approval Gates
    FailModeEntry {
        rule_id: "R-2.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-2.2",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-2.3",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-2.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-2.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-2.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-2.7",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-2.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-2.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-2.10",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-2.11",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-2.12",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-2.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-2.14",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-2.15",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-2.16",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-2.17",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §3 Sandbox Isolation
    FailModeEntry {
        rule_id: "R-3.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-3.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-3.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-3.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-3.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-3.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-3.7",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "R-3.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-3.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §4 Credential & Secret Protection
    FailModeEntry {
        rule_id: "R-4.1",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-4.2",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-4.3",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-4.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-4.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-4.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-4.7",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-4.8",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "R-4.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-4.10",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-4.11",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-4.12",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-4.13",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-4.14",
        fail_mode: FailMode::EmergencyStop,
    },
    // §5 I/O Schema Validation
    FailModeEntry {
        rule_id: "R-5.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-5.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-5.11",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-5.12",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-5.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §6 Session, Workflow & Budget
    FailModeEntry {
        rule_id: "R-6.1",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-6.2",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-6.3",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-6.4",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-6.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-6.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-6.7",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-6.8",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "R-6.9",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "R-6.10",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-6.11",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-6.12",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-6.13",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-6.14",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-6.15",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-6.16",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-6.17",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-6.18",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-6.19",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-6.20",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-6.21",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-6.22",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-6.23",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §7 Abuse / Hard-Stop / Circuit Breakers
    FailModeEntry {
        rule_id: "R-7.1",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.2",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.3",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-7.5",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.6",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.7",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.11",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.12",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.14",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.15",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.16",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R-7.17",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-7.18",
        fail_mode: FailMode::Degrade,
    },
    // §8 Audit & Traceability
    FailModeEntry {
        rule_id: "R-8.1",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-8.2",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.3",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.4",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-8.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.7",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.8",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.9",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-8.10",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.11",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-8.12",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-8.13",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.14",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-8.15",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.16",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-8.17",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-8.18",
        fail_mode: FailMode::LogOnly,
    },
    // §9 Agent Install & Provenance
    FailModeEntry {
        rule_id: "R-9.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-9.2",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-9.3",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-9.4",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-9.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-9.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-9.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-9.8",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-9.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-9.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-9.11",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-9.12",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-9.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-9.14",
        fail_mode: FailMode::LogOnly,
    },
    // §10 Federation / Remote
    FailModeEntry {
        rule_id: "R-10.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-10.2",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-10.3",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R-10.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-10.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-10.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R-10.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-10.8",
        fail_mode: FailMode::RefuseBoot,
    },
    // §11 Inter-Agent Messaging
    FailModeEntry {
        rule_id: "R-11.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-11.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-11.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-11.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-11.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-11.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-11.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R-11.8",
        fail_mode: FailMode::EmergencyStop,
    },
    // R+ additions
    FailModeEntry {
        rule_id: "R+1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+4",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R+5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+6",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R+7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+8",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R+9",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R+10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+11",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+12",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R+13",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R+14",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+15",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "R+16",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+17",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R+18",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // R++ additions
    FailModeEntry {
        rule_id: "R++4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R++7",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "R++8",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "R++9",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "R++10",
        fail_mode: FailMode::RefuseBoot,
    },
    // R+++ additions
    FailModeEntry {
        rule_id: "R+++1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+++2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "R+++3",
        fail_mode: FailMode::LogOnly,
    },
];

pub fn lookup_fail_mode(rule_id: &str) -> Option<FailMode> {
    FAIL_MODE_TABLE
        .iter()
        .find(|entry| entry.rule_id == rule_id)
        .map(|entry| entry.fail_mode)
}

pub fn all_entries() -> Vec<(&'static str, FailMode)> {
    FAIL_MODE_TABLE
        .iter()
        .map(|e| (e.rule_id, e.fail_mode))
        .collect()
}

pub fn entries_by_fail_mode(mode: FailMode) -> Vec<&'static str> {
    FAIL_MODE_TABLE
        .iter()
        .filter(|e| e.fail_mode == mode)
        .map(|e| e.rule_id)
        .collect()
}
