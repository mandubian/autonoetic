//! Client-facing constitution exposure (`constitution.get`).
//!
//! The signed constitution is the contract every actor lives under, but it was
//! previously only visible as a digest (`gateway.info`) or buried in the repo.
//! This surfaces it to any channel/SDK: lightweight metadata plus a one-line
//! gloss per clause for those interested in the principles, with the full
//! markdown available on request (`include_text`).

use serde::{Deserialize, Serialize};

/// Parameters for `constitution.get`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConstitutionGetParams {
    /// Include the full constitution markdown in the response. Off by default —
    /// the metadata + per-clause gloss is the lightweight view; the full text
    /// can be large.
    #[serde(default)]
    pub include_text: bool,
}

/// One clause of the constitution, lightweight: its ID, who it binds, a
/// one-line gloss (first sentence from the source), and — when the gateway
/// mechanically enforces it — the code/test citation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstitutionClause {
    /// Clause ID, e.g. `P-7.19` (a rule) or `Ri-0.10` (a right).
    pub id: String,
    /// `agent` for a `P-*` principle/rule, `gateway` for an `Ri-*` right.
    pub binds: String,
    /// One-line statement — the first sentence of the clause in the source.
    pub gloss: String,
    /// Enforcement citation (code/test site) when the clause is mechanically
    /// enforced; `None` for declarative clauses with no enforcement row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcement: Option<String>,
}

/// Result of `constitution.get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstitutionGetResult {
    /// Constitution version, e.g. `2026.06.04`.
    pub version: String,
    /// Canonical SHA-256 digest (64 hex chars) of the signed payload.
    pub digest: String,
    /// Lock format version.
    pub format_version: u32,
    /// Signer id from the lock (e.g. `autonoetic:constitution:v1`); `None` if unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer_id: Option<String>,
    /// Whether the lock carries a signature.
    pub signed: bool,
    /// Number of `P-*` rules with an enforcement citation (matches the lock).
    pub rule_enforcement_count: usize,
    /// Number of `Ri-*` rights with an enforcement citation (matches the lock).
    pub right_enforcement_count: usize,
    /// Every `P-*`/`Ri-*` clause, one line each — the lightweight "by clause"
    /// view, sorted by ID.
    pub clauses: Vec<ConstitutionClause>,
    /// Full constitution markdown — present only when `include_text` was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}
