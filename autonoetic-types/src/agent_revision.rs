//! Agent revision types — immutable, content-addressed agent snapshots.
//!
//! An agent revision is the execution unit for agent sessions. It captures
//! an immutable snapshot of SKILL.md, agent files, capabilities, runtime
//! metadata, runtime.lock, and any referenced skills/artifacts/layers.
//!
//! Sessions pin a concrete revision at start time; later alias promotion
//! does not affect already-running sessions.

use serde::{Deserialize, Serialize};

/// Parsed target used by ingress/resolver entrypoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedAgentTarget {
    /// Plain alias/agent id (no explicit revision selector).
    AliasId(String),
    /// Explicit revision selector (`agent_id@...`).
    ExplicitRef {
        agent_id: String,
        revision_selector: String,
    },
}

/// Parse an incoming target string into alias or explicit-ref form.
///
/// Returns `None` when the target is syntactically invalid (empty values or
/// multiple `@` separators).
pub fn parse_agent_target(target: &str) -> Option<ParsedAgentTarget> {
    if target.is_empty() {
        return None;
    }
    if let Some((agent_id, revision_selector)) = target.split_once('@') {
        if agent_id.is_empty() || revision_selector.is_empty() || revision_selector.contains('@') {
            return None;
        }
        return Some(ParsedAgentTarget::ExplicitRef {
            agent_id: agent_id.to_string(),
            revision_selector: revision_selector.to_string(),
        });
    }
    Some(ParsedAgentTarget::AliasId(target.to_string()))
}

/// A fully qualified immutable agent reference.
///
/// Format: `<agent_id>@rev_sha256:<64 hex chars>`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRef {
    pub agent_id: String,
    pub revision_id: String,
}

impl AgentRef {
    pub fn new(agent_id: String, revision_id: String) -> Self {
        Self {
            agent_id,
            revision_id,
        }
    }

    /// Parse an agent_ref string in the format `agent_id@revision_id`.
    ///
    /// Validation rules:
    /// - Exactly one `@` separator
    /// - agent_id must match `[a-z0-9][a-z0-9._-]*` (no `@`)
    /// - revision_id must start with `rev_sha256:` followed by exactly 64 lowercase hex chars
    pub fn parse(s: &str) -> Option<Self> {
        let at_pos = s.find('@')?;
        if at_pos == 0 || at_pos != s.rfind('@').unwrap_or(0) {
            return None;
        }
        let agent_id = &s[..at_pos];
        let revision_id = &s[at_pos + 1..];

        if agent_id.is_empty() || revision_id.is_empty() {
            return None;
        }

        if !Self::is_valid_agent_id(agent_id) {
            return None;
        }

        if !Self::is_valid_revision_id(revision_id) {
            return None;
        }

        Some(Self {
            agent_id: agent_id.to_string(),
            revision_id: revision_id.to_string(),
        })
    }

    fn is_valid_agent_id(s: &str) -> bool {
        if s.is_empty() {
            return false;
        }
        let first = s.chars().next().unwrap();
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return false;
        }
        s.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-'
        })
    }

    fn is_valid_revision_id(s: &str) -> bool {
        let prefix = "rev_sha256:";
        if !s.starts_with(prefix) {
            return false;
        }
        let hex = &s[prefix.len()..];
        if hex.len() != 64 {
            return false;
        }
        hex.chars()
            .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_lowercase() || c.is_ascii_digit()))
    }

    /// Format as `agent_id@revision_id`.
    pub fn to_string(&self) -> String {
        format!("{}@{}", self.agent_id, self.revision_id)
    }
}

/// Lifecycle status of an agent revision.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRevisionStatus {
    /// Candidate revision not yet promoted to any alias.
    Candidate,
    /// Revision is active (pointed to by at least one alias).
    Ready,
    /// Revision has been superseded and is no longer active.
    Archived,
    /// Revision was rejected (e.g., failed evaluation).
    Rejected,
}

/// Durable record of an immutable agent revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRevisionRecord {
    /// Content-addressed revision ID (e.g., `rev_sha256:abcd1234...`).
    pub revision_id: String,
    /// Logical agent name this revision belongs to.
    pub agent_id: String,
    /// Base revision this was derived from (lineage).
    pub base_revision_id: Option<String>,
    /// Source artifact ID when created from an artifact bundle.
    pub artifact_id: Option<String>,
    /// Content digest (same digest family as revision_id).
    pub content_digest: String,
    /// Hash of the pinned runtime.lock (reproducibility binding).
    pub runtime_lock_hash: String,
    /// Hash of SKILL.md for quick integrity checks.
    pub manifest_hash: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Actor kind that created this revision. Canonical values come from
    /// [`PrincipalKind::tag()`](crate::principal::PrincipalKind::tag):
    /// `"human"`, `"autonoetic_agent"`, `"script"`, `"foreign_agent"`.
    /// Historical rows may contain legacy values (`"user"`, `"test"`,
    /// `"agent"`, `"bootstrap"`, `"system"`, `"cli"`, `"tool"`).
    pub created_by_type: String,
    /// Actor ID that created this revision.
    pub created_by_id: String,
    /// Source kind: `artifact`, `capsule_import`, `peer_import`.
    pub source_kind: String,
    /// Source reference (artifact id, capsule id, or peer ref).
    pub source_ref: Option<String>,
    /// Origin node for federation provenance.
    pub origin_node_id: String,
    /// Trust domain: `local`, `partner`, `foreign`, `untrusted`.
    pub trust_domain: String,
    /// Current lifecycle status.
    pub status: AgentRevisionStatus,
    /// Arbitrary metadata (JSON).
    pub metadata_json: serde_json::Value,
    /// Collision-safe short ID for LLM-friendly references (e.g., `abc12345`).
    #[serde(default)]
    pub short_id: String,
    /// Hostnames detected in artifact source at install time (gateway-owned contract).
    /// `None` on pre-migration revisions — unconstrained for drift signals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_network_hosts: Option<Vec<String>>,
    /// Ed25519 signature over the canonical revision content digest (base64).
    /// Produced by the gateway at revision creation time (P-9.13 auto-sign).
    /// Verified against the signer's public key for integrity attestation.
    #[serde(default)]
    pub signature: Option<String>,
    /// Identity of the signer. Format: `gateway:{fingerprint}` for gateway-auto-signed
    /// revisions, extensible to `peer:{node_id}`, `ci:{pipeline}`, etc.
    /// A verifier resolves this to a trusted public key from a trust store.
    #[serde(default)]
    pub signer_id: Option<String>,
}

/// Mutable alias binding from a stable name to a revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAliasRecord {
    /// Alias ID (MVP: equals `agent_id`).
    pub alias_id: String,
    /// Logical agent owner.
    pub agent_id: String,
    /// Target revision ID.
    pub revision_id: String,
    /// RFC3339 update timestamp.
    pub updated_at: String,
    /// Actor kind that updated this alias. See [`PrincipalKind::tag()`](crate::principal::PrincipalKind::tag)
    /// for canonical values. Historical rows may contain legacy values.
    pub updated_by_type: String,
    /// Actor ID that updated this alias.
    pub updated_by_id: String,
    /// Optional free-text reason for the update.
    pub reason: Option<String>,
    /// RFC3339 timestamp when the agent was suspended (None = active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_at: Option<String>,
    /// Reason for suspension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_reason: Option<String>,
    /// Actor that suspended the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended_by: Option<String>,
}

impl AgentAliasRecord {
    /// Create a basic alias record with no suspension.
    pub fn new(
        alias_id: String,
        agent_id: String,
        revision_id: String,
        updated_at: String,
        updated_by_type: String,
        updated_by_id: String,
        reason: Option<String>,
    ) -> Self {
        Self {
            alias_id,
            agent_id,
            revision_id,
            updated_at,
            updated_by_type,
            updated_by_id,
            reason,
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        }
    }
}

/// Session-to-agent-revision binding, pinned at session start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAgentBinding {
    /// Exact session ID.
    pub session_id: String,
    /// Root session ID for grouping related sessions.
    pub root_session_id: String,
    /// Resolved alias ID (None if resolved directly from agent_ref without alias).
    pub alias_id: Option<String>,
    /// Logical agent ID.
    pub agent_id: String,
    /// Pinned immutable revision ID.
    pub revision_id: String,
    /// Pinned runtime closure hash.
    pub runtime_lock_hash: String,
    /// Home node for future distributed placement.
    pub home_node_id: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// The original target the caller requested (agent_id or agent_ref string).
    pub requested_target: String,
}

/// Type of promotion action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromotionKind {
    /// Forward promotion to a newer revision.
    Promote,
    /// Rollback to a previous revision.
    Rollback,
}

/// Durable record of an alias promotion or rollback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromotionRecord {
    /// Unique promotion ID (e.g., `prom-abc123`).
    pub promotion_id: String,
    /// Whether this is a promote or rollback.
    pub kind: PromotionKind,
    /// Alias that was moved.
    pub alias_id: String,
    /// Logical agent ID.
    pub agent_id: String,
    /// Previous revision target (None for first promotion).
    pub previous_revision_id: Option<String>,
    /// New revision target.
    pub new_revision_id: String,
    /// Eval run that justified this promotion (if any).
    pub source_eval_run_id: Option<String>,
    /// Free-text reason.
    pub reason: Option<String>,
    /// RFC3339 creation timestamp.
    pub created_at: String,
    /// Actor kind. See [`PrincipalKind::tag()`](crate::principal::PrincipalKind::tag) for canonical values.
    /// Historical rows may contain legacy values.
    pub created_by_type: String,
    /// Actor ID.
    pub created_by_id: String,
    /// Origin node for provenance.
    pub origin_node_id: String,
    /// JSON-encoded pre-authorization metadata when the capability gate was
    /// bypassed. Examples:
    /// `{"method":"envelope","envelope_id":42,"rule":"P-2.27"}`
    /// `{"method":"escalation","escalation_id":"fed-xxx"}`
    /// `{"method":"approval","approval_id":"appr-yyy"}`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_authorization: Option<String>,
}

/// Crockford Base32 alphabet for human-friendly short IDs.
/// Excludes I, L, O, U to avoid ambiguity; 0O and 1Il confusion.
const CROCKFORD: &[char] = &[
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'j',
    'k', 'm', 'n', 'p', 'q', 'r', 's', 't', 'v', 'w', 'x', 'y', 'z',
];

/// Generate a short, human-friendly ID from a revision ID or content digest.
///
/// Uses the first N bytes of the hex digest to produce a Crockford Base32
/// encoded short ID. Default length is 8 characters (40 bits of entropy from
/// the first 5 bytes of the digest), which gives ~1 in 10^12 collision
/// probability for up to ~1M revisions.
///
/// The short ID is deterministic and reproducible — the same revision ID
/// always produces the same short ID.
pub fn short_id(digest: &str, len: Option<usize>) -> String {
    let len = len.unwrap_or(8);
    // Strip common prefixes (rev_sha256:, sha256:, etc.)
    let hex = digest
        .strip_prefix("rev_sha256:")
        .or_else(|| digest.strip_prefix("sha256:"))
        .or_else(|| digest.strip_prefix("sha384:"))
        .or_else(|| digest.strip_prefix("sha512:"))
        .unwrap_or(digest);

    // Each base32 char = 5 bits. Each hex char = 4 bits.
    // We need ceil(len * 5 / 4) hex chars.
    let hex_chars_needed = (len * 5 + 3) / 4;
    let hex_bytes = &hex[..std::cmp::min(hex_chars_needed, hex.len())];

    // Convert hex to a big number, then extract 5-bit chunks
    let mut bits = Vec::new();
    for ch in hex_bytes.chars() {
        let val = ch.to_digit(16).unwrap_or(0);
        for i in (0..4).rev() {
            bits.push(((val >> i) & 1) as u8);
        }
    }

    // Take exactly len * 5 bits (pad with zeros if needed)
    let needed = len * 5;
    while bits.len() < needed {
        bits.push(0);
    }
    let bits = &bits[..needed];

    let mut result = String::with_capacity(len);
    for chunk in bits.chunks(5) {
        let mut idx = 0u8;
        for &bit in chunk {
            idx = (idx << 1) | bit;
        }
        result.push(CROCKFORD[idx as usize]);
    }

    result
}

/// Generate a short ID with collision detection.
///
/// Given a set of existing revision IDs, generates a short ID for `digest`
/// that is unique within the set. If the default length produces a collision,
/// the length is incremented until unique (re-checking at each length).
pub fn short_id_unique<'a>(
    digest: &str,
    existing: impl IntoIterator<Item = &'a str>,
    min_len: Option<usize>,
) -> String {
    let existing_vec: Vec<&str> = existing.into_iter().collect();
    let mut len = min_len.unwrap_or(8);
    loop {
        let candidate = short_id(digest, Some(len));
        let collision = existing_vec
            .iter()
            .any(|d| short_id(d, Some(len)) == candidate);
        if !collision {
            return candidate;
        }
        len += 1;
        if len > 16 {
            // Safety valve: if we've gone past 16 chars, just use the full hex prefix
            return short_id(digest, Some(len));
        }
    }
}

/// Format an agent reference with a short revision ID for LLM consumption.
///
/// Returns e.g. `"planner.default@rev_abc12345"` — a compact, human-friendly
/// reference that can be resolved by `AgentRepository::resolve_agent()` via
/// the short ID index. Note: this format is NOT parseable by `AgentRef::parse()`
/// (which requires full hex), but IS resolvable through the repository layer.
pub fn format_short_ref(agent_id: &str, revision_id: &str) -> String {
    let short = short_id(revision_id, None);
    format!("{}@rev_{}", agent_id, short)
}

#[cfg(test)]
mod short_id_tests {
    use super::*;

    #[test]
    fn test_short_id_deterministic() {
        let digest = "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234";
        let id1 = short_id(digest, None);
        let id2 = short_id(digest, None);
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 8);
    }

    #[test]
    fn test_short_id_strips_prefix() {
        let with_prefix = "rev_sha256:abcd1234";
        let without_prefix = "abcd1234";
        // Both should produce the same short ID (same hex content)
        assert_eq!(
            short_id(with_prefix, Some(4)),
            short_id(without_prefix, Some(4))
        );
    }

    #[test]
    fn test_short_id_custom_length() {
        let digest = "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234";
        assert_eq!(short_id(digest, Some(4)).len(), 4);
        assert_eq!(short_id(digest, Some(12)).len(), 12);
    }

    #[test]
    fn test_short_id_no_ambiguous_chars() {
        let digest = "rev_sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let id = short_id(digest, None);
        for ch in id.chars() {
            assert!(
                !matches!(ch, 'i' | 'l' | 'o' | 'u' | 'I' | 'L' | 'O' | 'U'),
                "Short ID contains ambiguous char '{}': {}",
                ch,
                id
            );
        }
    }

    #[test]
    fn test_short_id_unique_avoids_collision() {
        let digest = "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234";
        let existing =
            vec!["rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"];
        // With the same digest in existing, length 8 would collide
        let unique = short_id_unique(digest, existing, Some(8));
        assert_ne!(unique, short_id(digest, Some(8)));
    }

    #[test]
    fn test_format_short_ref() {
        let ref_str = format_short_ref(
            "planner.default",
            "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234",
        );
        assert!(ref_str.starts_with("planner.default@rev_"));
        assert_eq!(ref_str.len(), "planner.default@rev_".len() + 8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::PrincipalKind;

    #[test]
    fn test_agent_ref_parse_valid() {
        let r = AgentRef::parse("planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234")
            .expect("should parse");
        assert_eq!(r.agent_id, "planner.default");
        assert_eq!(
            r.revision_id,
            "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        );
        assert_eq!(
            r.to_string(),
            "planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        );
    }

    #[test]
    fn test_agent_ref_parse_rejects_no_at() {
        assert!(AgentRef::parse("planner.default").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_multiple_at() {
        assert!(AgentRef::parse("planner@default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_empty_agent_id() {
        assert!(AgentRef::parse(
            "@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        )
        .is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_empty_revision() {
        assert!(AgentRef::parse("planner.default@").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_invalid_revision_format() {
        assert!(AgentRef::parse("planner.default@not-a-revision").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_wrong_hex_length() {
        assert!(AgentRef::parse("planner.default@rev_sha256:abcd").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_uppercase_hex() {
        assert!(AgentRef::parse("planner.default@rev_sha256:ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234").is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_invalid_agent_id_start() {
        assert!(AgentRef::parse(
            "-planner@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
        )
        .is_none());
    }

    #[test]
    fn test_agent_ref_parse_rejects_at_in_agent_id() {
        assert!(AgentRef::parse("planner@builder@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234").is_none());
    }

    #[test]
    fn test_agent_alias_record_serialization() {
        let record = AgentAliasRecord {
            alias_id: "planner.default".to_string(),
            agent_id: "planner.default".to_string(),
            revision_id:
                "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
                    .to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            updated_by_type: PrincipalKind::Human.tag().to_string(),
            updated_by_id: "admin".to_string(),
            reason: Some("initial promotion".to_string()),
            suspended_at: None,
            suspended_reason: None,
            suspended_by: None,
        };
        let json = serde_json::to_string(&record).expect("should serialize");
        let parsed: AgentAliasRecord = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(parsed.alias_id, "planner.default");
        assert_eq!(parsed.reason, Some("initial promotion".to_string()));
    }

    #[test]
    fn test_session_agent_binding_with_explicit_ref() {
        let binding = SessionAgentBinding {
            session_id: "sess-123".to_string(),
            root_session_id: "root-123".to_string(),
            alias_id: None,
            agent_id: "planner.default".to_string(),
            revision_id: "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234".to_string(),
            runtime_lock_hash: "sha256:lock123".to_string(),
            home_node_id: "gateway-1".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            requested_target: "planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234".to_string(),
        };
        assert!(binding.alias_id.is_none());
        assert_eq!(binding.requested_target, "planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234");
    }

    #[test]
    fn test_promotion_kind_roundtrip() {
        let promote = PromotionKind::Promote;
        let json = serde_json::to_string(&promote).expect("serialize");
        assert_eq!(json, "\"promote\"");
        let rollback = PromotionKind::Rollback;
        let json = serde_json::to_string(&rollback).expect("serialize");
        assert_eq!(json, "\"rollback\"");
    }

    #[test]
    fn test_parse_agent_target_alias() {
        assert_eq!(
            parse_agent_target("planner.default"),
            Some(ParsedAgentTarget::AliasId("planner.default".to_string()))
        );
    }

    #[test]
    fn test_parse_agent_target_explicit_ref() {
        assert_eq!(
            parse_agent_target("planner.default@rev_abcd1234"),
            Some(ParsedAgentTarget::ExplicitRef {
                agent_id: "planner.default".to_string(),
                revision_selector: "rev_abcd1234".to_string(),
            })
        );
    }

    #[test]
    fn test_parse_agent_target_rejects_invalid_shapes() {
        assert!(parse_agent_target("").is_none());
        assert!(parse_agent_target("@rev_abcd1234").is_none());
        assert!(parse_agent_target("planner.default@").is_none());
        assert!(parse_agent_target("a@b@c").is_none());
    }
}
