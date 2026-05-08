//! Frozen-baseline copies of the deterministic Phase-1 checks.
//!
//! ## Purpose
//!
//! The dual-sweep orchestrator ([`super::dual_sweep::DualSweepRunner`])
//! recursive-trust mitigation runs the same SQL queries and regexes twice:
//! once via the canonical [`super::checks`] module (the "current" sentinel)
//! and once via this `baseline` module. When a regression is introduced into
//! `super::checks` — a regex narrowed too far, a SQL filter that drops a
//! finding, a threshold weakened — the canonical pass misses the anchor.
//! The baseline pass continues to flag it, and `compare_phase1` records a
//! `baseline_only` disagreement which surfaces to the operator.
//!
//! This is the "frozen anchor" claim from the design doc made architectural
//! rather than just a configuration label.
//!
//! ## What's here
//!
//! Frozen copies of the five Phase-1 deterministic checks:
//!
//! - [`credential`]
//! - [`capability_accretion`]
//! - [`approval_bypass`]
//! - [`sandbox_escape`]
//! - [`supply_chain`]
//!
//! Phase-2 (LLM-judgment) checks are intentionally NOT mirrored here — the
//! baseline is `phase1_only` by contract.
//!
//! ## Editing rules
//!
//! - Default expectation: this directory is *never* edited. Improvements to
//!   detection logic go to `super::checks` so the canonical sentinel benefits;
//!   the baseline keeps a stable reference for disagreement detection.
//! - Updating the baseline is a deliberate operator action, e.g. retiring a
//!   detection class that has been definitively superseded. Such commits
//!   should:
//!   1. Carry a `[baseline-update]` prefix in the commit message.
//!   2. Land as a *separate PR* from any concurrent change to
//!      `super::checks`. PRs that touch both `checks/` and `baseline/` for
//!      the same pattern defeat the disagreement-detection contract.
//!   3. Bump [`BASELINE_VERSION`] following the convention documented on
//!      that constant.
//!   4. Document the rationale in the PR description.
//!
//! Today these rules are enforced solely by code review. An automated CI
//! guard rejecting concurrent edits without `[baseline-update]` is tracked
//! as issue #165.
//!
//! ## Why duplicate the code instead of generating it?
//!
//! Build-time generation would couple the baseline to the canonical source —
//! a regression in the source generator (or a careless `cargo expand`-style
//! reflection) would propagate into the baseline. Hand-written, hand-frozen
//! copies are the strongest separation we have without a separate crate or
//! versioned binary, both of which would add far more workspace machinery.

/// Version pin for the frozen baseline.
///
/// Bump on each `[baseline-update]` PR. The version is referenced from every
/// baseline file's header and surfaced as the `BASELINE_VERSION` constant
/// for tooling (CLI, docs, future CI guards) so the pin is unambiguous —
/// PR numbers can collide across forks/renames, dates do not uniquely
/// identify the snapshot, and commit SHAs are not known until merge time.
///
/// **Versioning convention:** semver-style `MAJOR.MINOR.PATCH`.
///
/// - `MAJOR` bumps when a check is added or removed (the *shape* of what
///   the baseline detects changes).
/// - `MINOR` bumps when a check's detection logic is updated (a regex
///   tightened, a threshold adjusted) without changing what categories
///   are detected.
/// - `PATCH` bumps for purely mechanical changes (rename, doc tweak,
///   refactor that does not alter SQL or regex literals).
///
/// Initial freeze for issue #153: `1.0.0`.
pub const BASELINE_VERSION: &str = "1.0.0";

pub mod approval_bypass;
pub mod capability_accretion;
pub mod credential;
pub mod sandbox_escape;
pub mod supply_chain;
