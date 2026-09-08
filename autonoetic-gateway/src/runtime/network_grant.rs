//! The per-exec network-namespace decision for `sandbox_exec` (#1022).
//!
//! # What this decides
//!
//! Whether one sandboxed execution gets the host network namespace
//! (bubblewrap `--share-net`). It is deliberately a small pure function so the
//! rule is auditable in one place and testable without a sandbox — the same
//! shape as [`crate::sandbox::SandboxDriverKind::guarantees_network_off`].
//!
//! # Ceiling vs grant
//!
//! The `NetworkAccess` capability is a **ceiling**: "may this agent ever reach
//! the network". An operator approval (or a declared preapproval, or a live
//! approved-exec cache entry) is a **grant**: "does *this* exec reach it".
//!
//! `sandbox_exec` seeded `share_net` from the ceiling
//! ([`crate::sandbox::BwrapIsolationOverrides::from_capabilities`]) and let the
//! gate only *widen* it. The gate could therefore never be the thing that
//! granted network — so an exec that raised no gate kept the ceiling's value.
//! For an agent holding `NetworkAccess { hosts: [...] }` in a session whose
//! egress taint still allows `Sink::Network` (the default for an untainted
//! session), that meant:
//!
//! ```text
//! zero detected signals  →  requires_network_gate = false  →  no operator prompt
//!                        →  share_net = true (inherited from the capability)
//!                        →  bwrap --share-net, full host network namespace
//! ```
//!
//! Static analysis loses by construction against dynamic access — `__import__`,
//! `exec("import socket")`, `getattr(lib, "fn")()`, a host assembled from pieces
//! or read from the environment, an unlisted third-party library. Each of those
//! is a zero-signal exec, so the path above is reachable on purpose-built code
//! and reachable by accident on ordinary code the detector table has not caught
//! up with.
//!
//! That contradicted the policy `sandbox_exec` enforces everywhere else: for
//! this tool, network reachability is operator-gated, not capability-derived.
//! (`artifact_exec` and script-mode execution are capability-derived *by
//! design* — see `docs/internals/sandbox/network-grant.md` — so they are out of scope
//! here.)
//!
//! # The rule
//!
//! `share_net` is now true **iff** an explicit per-exec grant exists and nothing
//! forces the network off. No grant ⇒ no network, whatever the capability says.
//! A missed detection becomes a runtime connection refusal (reported as
//! `network_isolated` with a repair hint) instead of silent namespace-wide
//! reachability.
//!
//! Closing this window does not make the analyzer's precision irrelevant: it
//! decides *what the operator is asked about*, which is a UX property. It does
//! mean the analyzer is no longer the thing standing between agent-supplied code
//! and the network. Per-host runtime enforcement (#1024) is the next step;
//! namespace-wide `--share-net` is still all-or-nothing once granted.

/// Inputs to [`decide_share_net`]. All fields are facts the caller has already
/// established; this function performs no I/O and consults no globals.
#[derive(Debug, Clone, Copy)]
pub struct ShareNetInputs {
    /// The **ceiling**: the manifest declares `NetworkAccess` with a non-empty
    /// host list. Recorded for observability only — it can no longer grant
    /// network on its own.
    pub capability_allows_network: bool,
    /// The **grant**: a validated `approval_ref`, an operator approval cleared
    /// this turn, a declared `approval_mode: preapproved` bypass, or an
    /// approved-exec cache hit standing in for a prior approval of the same
    /// fingerprint.
    pub approval_validated: bool,
    /// The command was recognised as safe inspection and skipped the gate; it
    /// needs no network by definition.
    pub safe_inspection_bypass: bool,
    /// The session's egress taint excludes `Sink::Network`.
    pub network_sink_excluded: bool,
    /// An active declassification grant covers `Sink::Network` for this exec's
    /// resolved hosts. Only consulted when `network_sink_excluded` is true.
    pub network_declassified: bool,
    /// The exec class runs offline unconditionally (`Evaluation` capability,
    /// promotion gate). Wins over everything else.
    pub force_network_off: bool,
}

/// Why an exec ended up with or without the host network namespace. Emitted in
/// the `sandbox_exec` trace so a network-off run is attributable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareNetReason {
    /// Offline by exec class — `Evaluation` capability or promotion gate.
    ForcedOff,
    /// Safe-inspection command; it bypassed the gate and needs no network.
    SafeInspection,
    /// Session taint excludes `Sink::Network` and no declassification grant
    /// covers this exec.
    TaintNotDeclassified,
    /// An explicit per-exec grant enabled the network namespace.
    GrantedByApproval,
    /// Capability-driven sibling paths (`artifact_exec` ordinary runs, script
    /// mode): the declared `NetworkAccess` capability *is* the grant there by
    /// design (see `docs/internals/sandbox/network-grant.md`). Never produced by
    /// [`decide_share_net`]; exists so the #1321 result disclosure speaks one
    /// reason vocabulary across exec paths.
    GrantedByCapability,
    /// A `sandbox_network: sealed`/`recording` proxy opened the namespace after
    /// the decision so the sandbox can reach the host-loopback proxy
    /// (`setup_sealed_proxy_for_exec`). Never produced by [`decide_share_net`].
    GrantedBySealedProxy,
    /// Fail-closed default: no per-exec grant (#1022). Includes the case where
    /// the capability ceiling would have permitted network but static analysis
    /// surfaced nothing for the gate to ask about.
    NoGrant,
}

impl ShareNetReason {
    /// Stable snake_case token for structured logs and events.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ForcedOff => "forced_off",
            Self::SafeInspection => "safe_inspection",
            Self::TaintNotDeclassified => "taint_not_declassified",
            Self::GrantedByApproval => "granted_by_approval",
            Self::GrantedByCapability => "granted_by_capability",
            Self::GrantedBySealedProxy => "granted_by_sealed_proxy",
            Self::NoGrant => "no_grant",
        }
    }
}

/// The outcome of the per-exec network decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareNetDecision {
    /// Whether to pass `--share-net` for this execution.
    pub share_net: bool,
    /// Why.
    pub reason: ShareNetReason,
    /// The capability ceiling permitted network but this exec had no grant, so
    /// the network stayed off. This is the #1022 window, now closed: surfaced so
    /// the resulting connection failures are attributable rather than mysterious
    /// (see `apply_network_isolation_failure_to_result`, which turns them into a
    /// `network_isolated` tool error with a repair hint).
    pub capability_ceiling_unused: bool,
}

/// Decide whether one `sandbox_exec` gets the host network namespace.
///
/// Precedence, first match wins:
///
/// | condition                                       | share_net | reason                  |
/// |-------------------------------------------------|-----------|-------------------------|
/// | `force_network_off`                             | false     | `ForcedOff`             |
/// | `safe_inspection_bypass`                        | false     | `SafeInspection`        |
/// | `network_sink_excluded && !network_declassified` | false     | `TaintNotDeclassified`  |
/// | `approval_validated`                            | **true**  | `GrantedByApproval`     |
/// | otherwise                                       | false     | `NoGrant`               |
///
/// `capability_allows_network` appears in no row: the ceiling never grants.
pub fn decide_share_net(inputs: ShareNetInputs) -> ShareNetDecision {
    let reason = if inputs.force_network_off {
        ShareNetReason::ForcedOff
    } else if inputs.safe_inspection_bypass {
        ShareNetReason::SafeInspection
    } else if inputs.network_sink_excluded && !inputs.network_declassified {
        ShareNetReason::TaintNotDeclassified
    } else if inputs.approval_validated {
        ShareNetReason::GrantedByApproval
    } else {
        ShareNetReason::NoGrant
    };
    let share_net = matches!(reason, ShareNetReason::GrantedByApproval);
    ShareNetDecision {
        share_net,
        reason,
        capability_ceiling_unused: !share_net && inputs.capability_allows_network,
    }
}

/// The `network` object embedded in `sandbox_exec` / `artifact_exec` result
/// bodies (#1321).
///
/// A net-less exec is indistinguishable from a DNS/egress outage from stdout
/// alone: `getent hosts` returning empty in a namespace that was never given
/// the network reads exactly like "DNS is broken" (observed live: a packager
/// pre-flight probe failed its task and filed an egress anomaly that was an
/// artifact of its own isolation). Disclosing the namespace state in the
/// result lets an agent branch on it mechanically:
///
/// - `share_net: false` — the command ran without the network. Any connection
///   failure it reports is a grant gap, not an outage.
/// - `reason` — the [`ShareNetReason`] token for *why*, including the
///   capability-driven sibling-path tokens.
/// - `capability_ceiling_unused` — the agent holds `NetworkAccess` but this
///   exec had no grant (the #1022 shape; only meaningful on the
///   operator-gated `sandbox_exec` path, always `false` elsewhere).
///
/// `share_net` is the **effective** state: a sealed-network proxy widens the
/// namespace after [`decide_share_net`], so callers pass the override value
/// actually handed to the sandbox, with the corresponding reason token.
pub fn network_disclosure_json(
    share_net: bool,
    reason: ShareNetReason,
    capability_ceiling_unused: bool,
) -> serde_json::Value {
    serde_json::json!({
        "share_net": share_net,
        "reason": reason.as_str(),
        "capability_ceiling_unused": capability_ceiling_unused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A network-capable agent with an explicit grant and nothing forcing the
    /// network off — the baseline "approved exec reaches the network" case.
    fn granted() -> ShareNetInputs {
        ShareNetInputs {
            capability_allows_network: true,
            approval_validated: true,
            safe_inspection_bypass: false,
            network_sink_excluded: false,
            network_declassified: false,
            force_network_off: false,
        }
    }

    #[test]
    fn approval_grants_the_network_namespace() {
        let d = decide_share_net(granted());
        assert!(d.share_net);
        assert_eq!(d.reason, ShareNetReason::GrantedByApproval);
        assert!(!d.capability_ceiling_unused);
    }

    /// The #1022 regression pin. Network-capable agent, untainted session (the
    /// default: an unrecorded session taint resolves to `unrestricted`, which
    /// allows `Sink::Network`), and static analysis found nothing — so no gate
    /// was raised and no grant exists. The exec must run WITHOUT the network
    /// namespace rather than inheriting it from the capability.
    #[test]
    fn zero_signals_on_network_capable_agent_does_not_inherit_share_net() {
        let d = decide_share_net(ShareNetInputs {
            approval_validated: false,
            ..granted()
        });
        assert!(
            !d.share_net,
            "a network-capable agent with no per-exec grant must not get --share-net (#1022)"
        );
        assert_eq!(d.reason, ShareNetReason::NoGrant);
        assert!(
            d.capability_ceiling_unused,
            "the unused ceiling must be reported so the runtime failure is attributable"
        );
    }

    /// The same fail-closed default for an agent with no capability at all —
    /// here the ceiling was never in play, so nothing is reported as unused.
    #[test]
    fn no_capability_and_no_grant_is_network_off_without_ceiling_report() {
        let d = decide_share_net(ShareNetInputs {
            capability_allows_network: false,
            approval_validated: false,
            ..granted()
        });
        assert!(!d.share_net);
        assert_eq!(d.reason, ShareNetReason::NoGrant);
        assert!(!d.capability_ceiling_unused);
    }

    /// `force_network_off` (Evaluation capability / promotion gate) beats an
    /// explicit grant: promotion verdicts are decided offline.
    #[test]
    fn force_network_off_beats_an_explicit_grant() {
        let d = decide_share_net(ShareNetInputs {
            force_network_off: true,
            ..granted()
        });
        assert!(!d.share_net);
        assert_eq!(d.reason, ShareNetReason::ForcedOff);
        assert!(d.capability_ceiling_unused);
    }

    /// Safe inspection sets `approval_validated` as a gate bypass; it must not
    /// be read back as a network grant.
    #[test]
    fn safe_inspection_bypass_is_not_a_network_grant() {
        let d = decide_share_net(ShareNetInputs {
            safe_inspection_bypass: true,
            ..granted()
        });
        assert!(!d.share_net);
        assert_eq!(d.reason, ShareNetReason::SafeInspection);
    }

    /// Under a taint that excludes `Sink::Network`, host approval alone does not
    /// widen — declassification is required (RFC §8 / #909 follow-up).
    #[test]
    fn taint_excluding_network_requires_declassification_not_just_approval() {
        let blocked = decide_share_net(ShareNetInputs {
            network_sink_excluded: true,
            network_declassified: false,
            ..granted()
        });
        assert!(!blocked.share_net);
        assert_eq!(blocked.reason, ShareNetReason::TaintNotDeclassified);

        let declassified = decide_share_net(ShareNetInputs {
            network_sink_excluded: true,
            network_declassified: true,
            ..granted()
        });
        assert!(declassified.share_net);
        assert_eq!(declassified.reason, ShareNetReason::GrantedByApproval);
    }

    /// Declassification without a grant is still no network: it lifts the taint
    /// restriction, it does not stand in for operator approval.
    #[test]
    fn declassification_without_a_grant_is_still_network_off() {
        let d = decide_share_net(ShareNetInputs {
            approval_validated: false,
            network_sink_excluded: true,
            network_declassified: true,
            ..granted()
        });
        assert!(!d.share_net);
        assert_eq!(d.reason, ShareNetReason::NoGrant);
    }

    /// #1321: the result disclosure for the observed failure shape — a net-less
    /// exec whose empty probe output looked like a DNS outage must be
    /// machine-recognisable as a grant gap instead.
    #[test]
    fn disclosure_names_the_grant_gap() {
        let d = decide_share_net(ShareNetInputs {
            approval_validated: false,
            ..granted()
        });
        let disclosure = network_disclosure_json(
            d.share_net,
            d.reason,
            d.capability_ceiling_unused,
        );
        assert_eq!(
            disclosure,
            serde_json::json!({
                "share_net": false,
                "reason": "no_grant",
                "capability_ceiling_unused": true,
            })
        );
    }

    /// A granted exec discloses the positive state too, so "the network worked
    /// because it was granted" is distinguishable from "no field, who knows".
    #[test]
    fn disclosure_names_the_grant() {
        let d = decide_share_net(granted());
        let disclosure = network_disclosure_json(
            d.share_net,
            d.reason,
            d.capability_ceiling_unused,
        );
        assert_eq!(
            disclosure,
            serde_json::json!({
                "share_net": true,
                "reason": "granted_by_approval",
                "capability_ceiling_unused": false,
            })
        );
    }

    /// The sibling-path tokens stay stable snake_case wire strings — the same
    /// closed-vocabulary contract as the decision reasons.
    #[test]
    fn sibling_path_reason_tokens_are_stable() {
        assert_eq!(ShareNetReason::GrantedByCapability.as_str(), "granted_by_capability");
        assert_eq!(
            ShareNetReason::GrantedBySealedProxy.as_str(),
            "granted_by_sealed_proxy"
        );
        // Never produced by the decision itself: the capability is not a grant
        // on the operator-gated path, and the proxy acts after the decision.
        for bits in 0u8..64 {
            let inputs = ShareNetInputs {
                capability_allows_network: bits & 1 != 0,
                approval_validated: bits & 2 != 0,
                safe_inspection_bypass: bits & 4 != 0,
                network_sink_excluded: bits & 8 != 0,
                network_declassified: bits & 16 != 0,
                force_network_off: bits & 32 != 0,
            };
            let d = decide_share_net(inputs);
            assert_ne!(d.reason, ShareNetReason::GrantedByCapability);
            assert_ne!(d.reason, ShareNetReason::GrantedBySealedProxy);
        }
    }

    /// The invariant, exhaustively: over every input combination, `share_net` is
    /// true iff a grant exists and nothing forces the network off. In particular
    /// no combination of capability + taint + declassification can produce
    /// network without `approval_validated`.
    #[test]
    fn share_net_implies_a_grant_across_the_whole_input_space() {
        for bits in 0u8..64 {
            let inputs = ShareNetInputs {
                capability_allows_network: bits & 1 != 0,
                approval_validated: bits & 2 != 0,
                safe_inspection_bypass: bits & 4 != 0,
                network_sink_excluded: bits & 8 != 0,
                network_declassified: bits & 16 != 0,
                force_network_off: bits & 32 != 0,
            };
            let d = decide_share_net(inputs);
            if d.share_net {
                assert!(
                    inputs.approval_validated
                        && !inputs.force_network_off
                        && !inputs.safe_inspection_bypass
                        && (!inputs.network_sink_excluded || inputs.network_declassified),
                    "share_net granted without a valid grant: {inputs:?}"
                );
            }
            assert_eq!(
                d.capability_ceiling_unused,
                !d.share_net && inputs.capability_allows_network
            );
        }
    }
}
