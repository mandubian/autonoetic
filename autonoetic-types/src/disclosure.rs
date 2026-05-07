//! Disclosure Policy Types
//!
//! Defines how sensitive information should be handled based on who is viewing.
//! Two layers:
//! - **DisclosureClass** — controls what the LLM may repeat in assistant replies (user-facing).
//! - **ViewerClass** — controls how observability data is redacted based on the viewer's role
//!   (agent, operator, admin). See issue #11.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Viewer class (per-actor redaction — issue #11)
// ---------------------------------------------------------------------------

/// Who is viewing observability/approval data. Determines the redaction level applied.
///
/// - **Agent**: sees own traces + published reports. Secrets, credential headers/body,
///   and other agents' internal reasoning are redacted. Execution traces from other
///   sessions show only metadata (tool name, success, duration) — no stdout/stderr.
/// - **Operator**: sees all traces within scope. Secrets are masked but credential
///   names, host patterns, and command structure are visible. stdout/stderr shown.
/// - **Admin**: sees everything unredacted including raw evidence, cross-session
///   correlation, and credential headers/body values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewerClass {
    /// Agent viewing via observability/approval tools.
    Agent,
    /// Human operator via CLI / chat TUI.
    #[default]
    Operator,
    /// Admin with full access.
    Admin,
}

// ---------------------------------------------------------------------------
// Disclosure class (LLM reply filtering)
// ---------------------------------------------------------------------------

/// The disclosure classification of a piece of information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureClass {
    /// Information that can be safely disclosed to the user verbatim.
    Public,
    /// Information that must be redacted from the assistant reply.
    Restricted,
    /// Legacy variant mapped to Restricted.
    #[serde(alias = "internal")]
    Internal,
    /// Legacy variant mapped to Restricted.
    #[serde(alias = "confidential")]
    Confidential,
    /// Legacy variant mapped to Restricted.
    #[serde(alias = "secret")]
    Secret,
}

impl Default for DisclosureClass {
    fn default() -> Self {
        Self::Public
    }
}

impl DisclosureClass {
    /// Returns true if this class represents restricted content.
    pub fn is_restricted(&self) -> bool {
        !matches!(self, Self::Public)
    }
}

/// A disclosure rule mapping a source (tool/path) to a classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisclosureRule {
    /// The source pattern (e.g., `memory.read`, `state/secrets/*`, `sandbox.exec`)
    pub source: String,
    /// The path or argument pattern if applicable (e.g. `state/secrets/*`). If not provided,
    /// applies to all calls to `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_pattern: Option<String>,
    /// The class to assign to information matching this rule.
    pub class: DisclosureClass,
}

/// The disclosure policy configuration for an agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisclosurePolicy {
    /// Ordered list of disclosure rules. The first matching rule applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<DisclosureRule>,
    /// The default classification if no rules match. Defaults to `Public`.
    #[serde(default)]
    pub default_class: DisclosureClass,
}
