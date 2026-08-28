//! Run-scoped decider appointment (#1195, umbrella #1191).
//!
//! The `GateDecider` capability (P-2.20) describes an agent that *may* occupy
//! the decider seat. An appointment is the operator act that *seats* one for a
//! particular run: a durable, causal-chained record saying "for this scope,
//! gates of these kinds route to this agent, up to this risk, until this
//! expiry".
//!
//! Two properties are load-bearing and are enforced at creation rather than
//! left to callers:
//!
//! - **An appointment never widens capabilities.** The appointee must already
//!   hold `GateDecider` covering every kind the appointment names.
//! - **Standing does not transfer, only rights within the seat do** (§3.2).
//!   The appointee cannot appoint further deciders, extend its own expiry, or
//!   re-scope itself; those are operator acts against this record.

use serde::{Deserialize, Serialize};

use crate::background::ApprovalRisk;

/// Gate kinds an appointment can cover. Mirrors the `GateDecider` capability's
/// `kinds`, which is what makes "appointment never widens capabilities" a
/// containment check rather than a translation.
pub const GATE_KIND_APPROVAL: &str = "approval";
pub const GATE_KIND_ESCALATION: &str = "escalation";

/// Every gate kind an appointment may name.
pub const APPOINTABLE_GATE_KINDS: &[&str] = &[GATE_KIND_APPROVAL, GATE_KIND_ESCALATION];

/// A durable, operator-authorized record seating an agent in the decider seat
/// for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeciderAppointment {
    pub appointment_id: String,

    /// The appointee. Must already hold `GateDecider` covering `kinds`.
    pub decider_agent: String,

    /// The promoted revision the appointee was seated *as*, captured at
    /// appointment time.
    ///
    /// An agent id is not a stable thing to have seated: the revision carries
    /// the instructions, the capabilities and — the reason this field exists —
    /// the model. Calibration evidence (#1198) is a property of the closure
    /// that produced the verdicts, not of the name on the seat, so an
    /// appointment records which closure it seated. `None` only for records
    /// written before this field existed.
    #[serde(default)]
    pub decider_revision: Option<String>,

    /// Gate kinds routed to the appointee — a subset of the agent's capability.
    pub kinds: Vec<String>,

    /// The run this appointment decides **for**.
    pub scope_root_session: String,

    /// The decider's **own** top-level session — a different root, created by
    /// the gateway with the appointing operator as its principal (#1196).
    /// `None` until that session exists; an appointment is a record of
    /// authority, not of a running process.
    pub decider_session: Option<String>,

    /// Gates above this class park for the operator. `Critical` is not
    /// appointable at all — see [`AppointmentError::CriticalNotAppointable`].
    pub risk_ceiling: ApprovalRisk,

    /// When true the verdict is recorded but the gate still parks for the
    /// human. Phase 1 forces this true: binding mode does not exist until
    /// calibration evidence does (§4.4, advisory before binding).
    pub advice_only: bool,

    /// Wall-clock expiry. Independent of `max_gates`; whichever is reached
    /// first ends the appointment.
    pub expires_at: Option<String>,

    /// Gate-count expiry — "decide at most N gates". Independent of
    /// `expires_at`. An appointment with neither is a standing grant and is
    /// reported as one rather than quietly treated as run-scoped.
    pub max_gates: Option<u32>,

    /// Gates decided under this appointment so far, for `max_gates`.
    pub gates_decided: u32,

    /// The operator principal. Non-repudiable like every other power act, and
    /// the recorded principal of `decider_session` once it exists, so the
    /// chain reads as delegation rather than spawn.
    pub appointed_by: String,
    pub appointed_at: String,

    pub revoked_at: Option<String>,
    pub revoked_by: Option<String>,
    pub revoked_reason: Option<String>,
}

impl DeciderAppointment {
    /// True when the appointment has been revoked or has reached either expiry.
    /// `now` is caller-supplied so this stays a pure function.
    pub fn is_expired(&self, now: &str) -> bool {
        if self.revoked_at.is_some() {
            return true;
        }
        if let Some(max) = self.max_gates {
            if self.gates_decided >= max {
                return true;
            }
        }
        match &self.expires_at {
            Some(exp) => exp.as_str() <= now,
            None => false,
        }
    }

    /// True when no expiry of either kind was set — a standing grant. Surfaced
    /// so operator-facing views can say so out loud instead of rendering a
    /// blank column.
    pub fn is_standing(&self) -> bool {
        self.expires_at.is_none() && self.max_gates.is_none()
    }

    /// Whether a gate of `kind` at `risk` falls inside this appointment.
    /// Expiry is deliberately *not* checked here — callers check it against
    /// their own clock so this stays pure and testable.
    pub fn covers(&self, kind: &str, risk: ApprovalRisk) -> bool {
        self.kinds.iter().any(|k| k == kind) && risk.rank() <= self.risk_ceiling.rank()
    }
}

/// Why an appointment was refused. Each variant is a mechanical check, not a
/// judgment call — the gateway is a Lawful Executor here as everywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppointmentError {
    /// The appointee does not hold `GateDecider` at all.
    NotADecider { agent_id: String },
    /// The appointee holds `GateDecider` but not for every kind named.
    KindNotHeld { agent_id: String, kind: String },
    /// A kind that is not a gate kind at all.
    UnknownKind { kind: String },
    /// No kinds named — an appointment that routes nothing.
    NoKinds,
    /// `Critical` actions (`RevisionPromote`, `CredentialPrompt`) are not
    /// delegable by a single operator gesture. Refused at appointment time
    /// rather than merely sitting above a configurable ceiling.
    CriticalNotAppointable,
    /// Phase 1 ships advisory-only; binding appointments wait on calibration.
    BindingNotYetAvailable,
    /// The scope is empty.
    NoScope,
    /// An expiry timestamp that is not RFC3339.
    BadExpiry { value: String },
}

impl std::fmt::Display for AppointmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotADecider { agent_id } => write!(
                f,
                "Agent '{agent_id}' does not hold the GateDecider capability, so it cannot be \
                 appointed (P-2.20); an appointment never widens capabilities"
            ),
            Self::KindNotHeld { agent_id, kind } => write!(
                f,
                "Agent '{agent_id}' holds GateDecider but not for '{kind}' gates, so it cannot be \
                 appointed for them (P-2.20); an appointment never widens capabilities"
            ),
            Self::UnknownKind { kind } => write!(
                f,
                "'{kind}' is not a gate kind; expected one of {}",
                APPOINTABLE_GATE_KINDS.join(", ")
            ),
            Self::NoKinds => write!(f, "An appointment must name at least one gate kind"),
            Self::CriticalNotAppointable => write!(
                f,
                "Critical gates (agent promotion, credential registration) are not appointable: \
                 they are refused at appointment time rather than left above a ceiling, because \
                 promotion and secret delivery must not be delegable by a single operator gesture"
            ),
            Self::BindingNotYetAvailable => write!(
                f,
                "Binding appointments are not available yet: phase 1 is advisory-only, so \
                 advice_only must be true. Binding mode unlocks once the ledger carries \
                 agreement evidence (§4.4, advisory before binding)"
            ),
            Self::NoScope => write!(
                f,
                "An appointment must name the root session it decides for; a scopeless \
                 appointment is a global grant, which this record deliberately cannot express"
            ),
            Self::BadExpiry { value } => {
                write!(f, "expires_at '{value}' is not a valid RFC3339 timestamp")
            }
        }
    }
}

impl std::error::Error for AppointmentError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn appointment() -> DeciderAppointment {
        DeciderAppointment {
            appointment_id: "apt-1".into(),
            decider_agent: "nightwatch.default".into(),
            decider_revision: Some("rev_sha256:abc".into()),
            kinds: vec![GATE_KIND_APPROVAL.into()],
            scope_root_session: "root-1".into(),
            decider_session: None,
            risk_ceiling: ApprovalRisk::High,
            advice_only: true,
            expires_at: None,
            max_gates: None,
            gates_decided: 0,
            appointed_by: "operator".into(),
            appointed_at: "2026-08-28T00:00:00Z".into(),
            revoked_at: None,
            revoked_by: None,
            revoked_reason: None,
        }
    }

    #[test]
    fn covers_matches_kind_and_ceiling() {
        let a = appointment();
        assert!(a.covers(GATE_KIND_APPROVAL, ApprovalRisk::Standard));
        assert!(a.covers(GATE_KIND_APPROVAL, ApprovalRisk::High));
        // Above the ceiling parks for the operator.
        assert!(!a.covers(GATE_KIND_APPROVAL, ApprovalRisk::Critical));
        // A kind the appointment does not name.
        assert!(!a.covers(GATE_KIND_ESCALATION, ApprovalRisk::Standard));
    }

    #[test]
    fn standard_ceiling_does_not_cover_high() {
        // The Night Shift consequence: SandboxExec with detected hosts is
        // High, so a Standard-ceiling night watch decides neither demo gate.
        let a = DeciderAppointment {
            risk_ceiling: ApprovalRisk::Standard,
            ..appointment()
        };
        assert!(!a.covers(GATE_KIND_APPROVAL, ApprovalRisk::High));
    }

    #[test]
    fn expiry_is_reached_by_either_clock_or_count() {
        let by_time = DeciderAppointment {
            expires_at: Some("2026-08-28T06:00:00Z".into()),
            ..appointment()
        };
        assert!(!by_time.is_expired("2026-08-28T05:59:00Z"));
        assert!(by_time.is_expired("2026-08-28T06:00:00Z"));

        let by_count = DeciderAppointment {
            max_gates: Some(2),
            gates_decided: 2,
            ..appointment()
        };
        assert!(by_count.is_expired("2026-08-28T00:00:00Z"));

        // The two are independent: a count-bounded appointment with gates left
        // does not expire just because it carries no timestamp.
        let live = DeciderAppointment {
            max_gates: Some(2),
            gates_decided: 1,
            ..appointment()
        };
        assert!(!live.is_expired("2099-01-01T00:00:00Z"));
    }

    #[test]
    fn revocation_expires_regardless_of_clock() {
        let revoked = DeciderAppointment {
            revoked_at: Some("2026-08-28T01:00:00Z".into()),
            ..appointment()
        };
        assert!(revoked.is_expired("2026-08-28T00:00:00Z"));
    }

    #[test]
    fn an_appointment_with_no_expiry_reports_itself_standing() {
        assert!(appointment().is_standing());
        assert!(!DeciderAppointment {
            max_gates: Some(1),
            ..appointment()
        }
        .is_standing());
    }
}
