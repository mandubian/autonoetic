//! Unified fail-mode table (I-11).
//!
//! Every constitutional invariant has a declared failure action in one
//! place.  The five fail modes are:
//!
//! | Mode | Meaning |
//! |---|---|
//! | `RefuseBoot` | Gateway refuses to start. |
//! | `RefuseSessionStart` | Gateway refuses to create / resume / continue a session. Applies at session creation and at any mid-session enforcement point where the invariant cannot be verified (e.g. cost-budget catalog unavailable). |
//! | `RefuseTurn` | The in-flight turn is refused: the completion is aborted or the boundary send/exec is denied, mid-turn, without killing the session (§15 egress rules). |
//! | `Degrade` | Session enters degraded mode (P-7.18). |
//! | `EmergencyStop` | Session is killed immediately. |
//! | `LogOnly` | No enforcement action; event is logged for audit. |

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailMode {
    RefuseBoot,
    RefuseSessionStart,
    RefuseTurn,
    Degrade,
    EmergencyStop,
    LogOnly,
}

impl fmt::Display for FailMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailMode::RefuseBoot => write!(f, "refuse-boot"),
            FailMode::RefuseSessionStart => write!(f, "refuse-session-start"),
            FailMode::RefuseTurn => write!(f, "refuse-turn"),
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
    FailModeEntry {
        rule_id: "Ri-0.14",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "Ri-0.15",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "Ri-0.16",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "Ri-0.17",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "Ri-0.18",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §1 Capability & Rights
    FailModeEntry {
        rule_id: "P-1.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-1.11",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §2 Approval Gates
    FailModeEntry {
        rule_id: "P-2.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.2",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-2.3",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-2.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-2.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-2.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-2.7",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-2.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.10",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-2.11",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-2.12",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-2.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.14",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.15",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-2.16",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.17",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.18",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-2.19",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-2.20",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.21",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.29",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-2.23",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-2.24",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §3 Sandbox Isolation
    FailModeEntry {
        rule_id: "P-3.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-3.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-3.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-3.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-3.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-3.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-3.7",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "P-3.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-3.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-3.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §4 Credential & Secret Protection
    FailModeEntry {
        rule_id: "P-4.1",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-4.2",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-4.3",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-4.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-4.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-4.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-4.7",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-4.8",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "P-4.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-4.10",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-4.11",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-4.12",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-4.13",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-4.14",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-4.15",
        fail_mode: FailMode::RefuseBoot,
    },
    // §5 I/O Schema Validation
    FailModeEntry {
        rule_id: "P-5.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-5.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-5.11",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-5.12",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-5.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §6 Session, Workflow & Budget
    FailModeEntry {
        rule_id: "P-6.1",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-6.2",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-6.3",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-6.4",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-6.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-6.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-6.7",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-6.8",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "P-6.9",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "P-6.10",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-6.11",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-6.12",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-6.13",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-6.14",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-6.15",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-6.16",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-6.17",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-6.18",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-6.19",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-6.20",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-6.21",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-6.22",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-6.23",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §7 Abuse / Hard-Stop / Circuit Breakers
    FailModeEntry {
        rule_id: "P-7.1",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.2",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.3",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.4",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-7.5",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.6",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.7",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.8",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.11",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.12",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.14",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.15",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.16",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "P-7.17",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.18",
        fail_mode: FailMode::Degrade,
    },
    FailModeEntry {
        rule_id: "P-7.21",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-7.22",
        fail_mode: FailMode::Degrade,
    },
    // §8 Audit & Traceability
    FailModeEntry {
        rule_id: "P-8.1",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-8.2",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.3",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.4",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-8.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.7",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.8",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.9",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-8.10",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.11",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-8.12",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-8.13",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.14",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-8.15",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.16",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-8.17",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.18",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-8.19",
        fail_mode: FailMode::LogOnly,
    },
    // §9 Agent Install & Provenance
    FailModeEntry {
        rule_id: "P-9.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-9.2",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-9.3",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-9.4",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-9.5",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-9.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-9.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-9.8",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-9.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-9.10",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-9.11",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-9.12",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-9.13",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-9.14",
        fail_mode: FailMode::LogOnly,
    },
    // §10 Federation / Remote
    FailModeEntry {
        rule_id: "P-10.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-10.2",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-10.3",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-10.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-10.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-10.6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "P-10.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-10.8",
        fail_mode: FailMode::RefuseBoot,
    },
    FailModeEntry {
        rule_id: "P-10.9",
        fail_mode: FailMode::RefuseSessionStart,
    },
    // §11 Inter-Agent Messaging
    FailModeEntry {
        rule_id: "P-11.1",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-11.2",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-11.3",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-11.4",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-11.5",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-11.6",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-11.7",
        fail_mode: FailMode::RefuseSessionStart,
    },
    FailModeEntry {
        rule_id: "P-11.8",
        fail_mode: FailMode::EmergencyStop,
    },
    // §13 Cross-cutting invariants. I-11 is this table: its own failure
    // action is refuse-boot, because a gateway that cannot say how an
    // invariant fails must not start.
    FailModeEntry {
        rule_id: "I-6",
        fail_mode: FailMode::LogOnly,
    },
    FailModeEntry {
        rule_id: "I-10",
        fail_mode: FailMode::EmergencyStop,
    },
    FailModeEntry {
        rule_id: "I-11",
        fail_mode: FailMode::RefuseBoot,
    },
    // §15 Data Egress Localization (#910 / constitution 2026.07.30). All three
    // rules fail *mid-turn* by construction: the chokepoint aborts the
    // completion on an outbound-assertion violation, and boundary gates refuse
    // the send/exec before bytes leave — hence the refuse-turn mode.
    FailModeEntry {
        rule_id: "P-15.1",
        fail_mode: FailMode::RefuseTurn,
    },
    FailModeEntry {
        rule_id: "P-15.2",
        fail_mode: FailMode::RefuseTurn,
    },
    FailModeEntry {
        rule_id: "P-15.3",
        fail_mode: FailMode::RefuseTurn,
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
