//! Cognitive Capsule Manifest — portable agent export.
//!
//! A Cognitive Capsule is a self-contained, portable snapshot of an agent
//! at a specific point in time. It captures everything needed to reproduce
//! that agent's behavior on a different machine, a different gateway, or in
//! a different environment.
//!
//! This module defines the **schema** for capsules (the `capsule.json`
//! manifest and its supporting types). Export/import pipelines live in
//! `autonoetic-gateway::capsule`.
//!
//! Design doc: `docs/guide/cognitive-capsule.md`.

use serde::{Deserialize, Serialize};

/// Current capsule manifest format version (semver string).
///
/// Capsules emitted by this crate are stamped with this value. Importers
/// compare it against the major component to decide compatibility: minor
/// and patch bumps are forward-compatible (older gateways accept newer
/// minor/patch capsules) but a major bump requires explicit handling.
pub const CAPSULE_FORMAT_VERSION: &str = "1.0.0";

/// Capsule export mode — what content is included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleMode {
    /// Agent revision + `runtime.lock` + artifact/layer **references**.
    /// Receiving gateway fetches dependencies from marketplace/network.
    Thin,
    /// Agent revision + `runtime.lock` + embedded artifact **content** + layers.
    /// Offline/air-gapped replay; no network needed on import.
    Hermetic,
    /// Hermetic + session checkpoint + context capsule state.
    /// Resume an agent session exactly where it left off.
    Replay,
    /// Thin (references only) + scheduled job definitions. Re-creates
    /// cold-path (cron) agents that run without human interaction. To
    /// embed dependency content alongside scheduled jobs, export
    /// `Hermetic` first and recreate jobs by hand on the receiver.
    Headless,
}

impl CapsuleMode {
    /// Returns true when the mode embeds dependency content (not just
    /// references). `Headless` is treated as a thin variant — bundling
    /// dependency content alongside scheduled jobs is intentionally
    /// out of scope for this mode.
    pub fn is_hermetic(self) -> bool {
        matches!(self, CapsuleMode::Hermetic | CapsuleMode::Replay)
    }

    /// Returns true when the mode requires a session checkpoint to be bundled.
    pub fn needs_checkpoint(self) -> bool {
        matches!(self, CapsuleMode::Replay)
    }
}

/// A reference to an included artifact (legacy / thin-mode digest reference).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncludedArtifact {
    pub artifact_id: String,
    pub sha256: String,
}

/// Gateway runtime embedded in a hermetic capsule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsuleGatewayRuntime {
    pub artifact: String,
    pub version: String,
    pub sha256: String,
}

/// Cryptographic signature attached to a capsule for integrity / authenticity.
///
/// The signature is computed over the canonical JSON serialization of the
/// `CapsuleManifest` **with the `signature` field cleared** (so signing is
/// idempotent and verification can be deterministic).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsuleSignature {
    /// Algorithm identifier — currently only `"ed25519"`.
    pub algorithm: String,
    /// Signer identity (e.g. `gateway:<fingerprint>`, `user:<id>`, `ci:<pipeline>`).
    pub signer_id: String,
    /// Base64-encoded signature bytes.
    pub signature: String,
}

/// Memory snapshot embedded in a capsule (opt-in).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsuleMemorySnapshot {
    /// Number of memory entries included. Fixed-width `u64` so the
    /// capsule manifest stays portable across 32-bit and 64-bit
    /// gateways.
    pub entry_count: u64,
    /// Knowledge-store scopes that were exported (e.g. `["memory","user_profile"]`).
    pub scopes: Vec<String>,
    /// Path inside the capsule archive that holds the memory dump file.
    pub content_handle: String,
    /// Whether the redaction pipeline was applied before export.
    pub redacted: bool,
    /// Memory entries owned by the agent but withheld because their egress
    /// label excluded the capsule's declared destination sink (RFC §7).
    #[serde(default)]
    pub withheld_count: u64,
}

/// Provenance record — where this capsule came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsuleProvenance {
    /// Node ID of the gateway that exported the capsule.
    pub origin_node_id: String,
    /// Gateway version at export time (e.g. crate version).
    pub gateway_version: String,
    /// Trust domain claim: `"local"`, `"partner"`, or `"foreign"`.
    pub trust_domain: String,
    /// Declared egress destination sink for memory filtering at export (RFC §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_sink: Option<String>,
    /// Memory entries withheld during export because their label excluded
    /// [`destination_sink`](Self::destination_sink).
    #[serde(default)]
    pub memory_withheld_count: u64,
    /// If this capsule was produced by re-exporting an imported capsule,
    /// the original capsule's ID. Enables provenance chains across hops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_capsule_id: Option<String>,
}

/// Platform descriptor used to check layer compatibility on import.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsulePlatform {
    /// e.g. `"linux"`, `"macos"`, `"windows"`.
    pub os: String,
    /// e.g. `"x86_64"`, `"aarch64"`.
    pub arch: String,
}

/// A build-layer reference (either referenced or embedded).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsuleLayerRef {
    pub layer_id: String,
    pub name: String,
    pub digest: String,
    pub size_bytes: u64,
    /// Path inside the capsule archive that holds the embedded layer archive
    /// when the mode is hermetic. `None` for thin-mode references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded_handle: Option<String>,
    /// Build platform for compatibility checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<CapsulePlatform>,
}

/// The `capsule.json` manifest.
///
/// This is the canonical schema for a capsule's metadata. Pipelines write
/// it as pretty JSON to the capsule archive root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsuleManifest {
    // --- Identity ---
    /// Unique capsule ID (e.g. ULID or content-addressed hash).
    pub capsule_id: String,
    /// Semver format version of the manifest schema itself.
    #[serde(default = "default_format_version")]
    pub format_version: String,
    pub mode: CapsuleMode,
    /// RFC3339 timestamp at export.
    pub created_at: String,

    // --- Agent identity ---
    pub agent_id: String,
    /// Pinned immutable agent revision ID (content-addressed).
    #[serde(default)]
    pub revision_id: String,
    /// Short ID for human reference (first few hex chars of the revision).
    #[serde(default)]
    pub revision_short_id: String,
    /// Content digest of the revision body (SKILL.md + files).
    #[serde(default)]
    pub content_digest: String,

    // --- Execution closure ---
    pub entrypoint: String,
    pub runtime_lock: String,

    // --- Included content ---
    #[serde(default)]
    pub included_artifacts: Vec<IncludedArtifact>,
    #[serde(default)]
    pub included_layers: Vec<CapsuleLayerRef>,
    #[serde(default)]
    pub included_skills: Vec<String>,

    // --- Optional sections ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_runtime: Option<CapsuleGatewayRuntime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_snapshot: Option<CapsuleMemorySnapshot>,
    /// Path inside the capsule archive holding the session checkpoint
    /// (Replay mode only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_handle: Option<String>,

    // --- Security ---
    /// Names of fields/sections that were redacted before export.
    #[serde(default)]
    pub redactions: Vec<String>,
    /// Optional Ed25519 signature over the canonical manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<CapsuleSignature>,
    /// Provenance metadata — where this capsule was produced.
    #[serde(default = "default_provenance")]
    pub provenance: CapsuleProvenance,

    // --- Dependency declaration (importer hints) ---
    /// Agent IDs that should exist on the receiving gateway for this
    /// capsule to function (e.g. delegate targets).
    #[serde(default)]
    pub requires_agents: Vec<String>,
    /// Skill names this capsule depends on but does not bundle.
    #[serde(default)]
    pub requires_skills: Vec<String>,

    // --- Mode-specific extras ---
    /// Scheduled job definitions bundled into Headless-mode capsules. On
    /// import the receiving gateway recreates these jobs via the
    /// scheduler (subject to local config caps).
    #[serde(default)]
    pub scheduled_jobs: Vec<CapsuleScheduledJob>,
    /// Build platform of the originating gateway. Importers compare
    /// against the local platform when `trust_domain` is not `"local"`
    /// to refuse incompatible layer bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<CapsulePlatform>,
}

/// A scheduled job definition bundled in a Headless-mode capsule.
///
/// Mirrors the gateway's `autonoetic_types::scheduled_job::ScheduledJob`
/// shape so the importer can `INSERT` directly. Job IDs are remapped on
/// import to avoid collisions with the receiver's existing jobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapsuleScheduledJob {
    pub job_id: String,
    pub owner_agent_id: String,
    pub root_session_id: String,
    pub target_agent_id: String,
    pub target_revision_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_json: Option<String>,
    pub cron_expr: String,
    pub timezone: String,
    pub created_at: String,
}

fn default_format_version() -> String {
    CAPSULE_FORMAT_VERSION.to_string()
}

fn default_provenance() -> CapsuleProvenance {
    CapsuleProvenance {
        origin_node_id: String::new(),
        gateway_version: String::new(),
        trust_domain: "local".to_string(),
        destination_sink: None,
        memory_withheld_count: 0,
        parent_capsule_id: None,
    }
}

impl CapsuleManifest {
    /// Parse the major version component of [`Self::format_version`].
    ///
    /// Used by importers to gate forward-compatibility. Returns `None`
    /// when the string is malformed.
    pub fn format_major_version(&self) -> Option<u64> {
        self.format_version
            .split('.')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
    }

    /// Returns the in-archive path the importer should look at for the
    /// embedded layer of the given reference, when present.
    pub fn layer_embedded_path<'a>(&self, ref_: &'a CapsuleLayerRef) -> Option<&'a str> {
        ref_.embedded_handle.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest(mode: CapsuleMode) -> CapsuleManifest {
        CapsuleManifest {
            capsule_id: "cap_01HZ".to_string(),
            format_version: CAPSULE_FORMAT_VERSION.to_string(),
            mode,
            created_at: "2026-05-28T00:00:00Z".to_string(),
            agent_id: "demo.agent".to_string(),
            revision_id: "rev_abcdef1234".to_string(),
            revision_short_id: "abcdef12".to_string(),
            content_digest:
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            entrypoint: "SKILL.md".to_string(),
            runtime_lock: "runtime.lock".to_string(),
            included_artifacts: vec![IncludedArtifact {
                artifact_id: "art_001".to_string(),
                sha256:
                    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                        .to_string(),
            }],
            included_layers: vec![CapsuleLayerRef {
                layer_id: "layer_001".to_string(),
                name: "python-3.12".to_string(),
                digest: "sha256:deadbeef".to_string(),
                size_bytes: 1024,
                embedded_handle: if mode.is_hermetic() {
                    Some("layers/layer_001/contents.tar.zst".to_string())
                } else {
                    None
                },
                platform: Some(CapsulePlatform {
                    os: "linux".to_string(),
                    arch: "x86_64".to_string(),
                }),
            }],
            included_skills: vec!["coder".to_string()],
            gateway_runtime: None,
            memory_snapshot: Some(CapsuleMemorySnapshot {
                entry_count: 3,
                scopes: vec!["memory".to_string()],
                content_handle: "memory/memory_snapshot.json".to_string(),
                redacted: true,
                withheld_count: 0,
            }),
            checkpoint_handle: if mode.needs_checkpoint() {
                Some("checkpoint/checkpoint.json".to_string())
            } else {
                None
            },
            redactions: vec!["env.OPENAI_API_KEY".to_string()],
            signature: Some(CapsuleSignature {
                algorithm: "ed25519".to_string(),
                signer_id: "gateway:abc123".to_string(),
                signature: "base64sig==".to_string(),
            }),
            provenance: CapsuleProvenance {
                origin_node_id: "node-A".to_string(),
                gateway_version: "0.4.0".to_string(),
                trust_domain: "local".to_string(),
                destination_sink: None,
                memory_withheld_count: 0,
                parent_capsule_id: None,
            },
            requires_agents: vec!["lead".to_string()],
            requires_skills: vec!["researcher".to_string()],
            scheduled_jobs: vec![],
            platform: Some(CapsulePlatform {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
            }),
        }
    }

    fn assert_roundtrip(manifest: &CapsuleManifest) {
        let json = serde_json::to_string_pretty(manifest).expect("serialize");
        let parsed: CapsuleManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&parsed, manifest, "roundtrip equality failed");
    }

    #[test]
    fn roundtrip_thin_mode() {
        assert_roundtrip(&sample_manifest(CapsuleMode::Thin));
    }

    #[test]
    fn roundtrip_hermetic_mode() {
        assert_roundtrip(&sample_manifest(CapsuleMode::Hermetic));
    }

    #[test]
    fn roundtrip_replay_mode() {
        let m = sample_manifest(CapsuleMode::Replay);
        assert!(m.checkpoint_handle.is_some());
        assert_roundtrip(&m);
    }

    #[test]
    fn roundtrip_headless_mode() {
        assert_roundtrip(&sample_manifest(CapsuleMode::Headless));
    }

    #[test]
    fn capsule_mode_is_hermetic_classifies_modes() {
        assert!(!CapsuleMode::Thin.is_hermetic());
        assert!(!CapsuleMode::Headless.is_hermetic());
        assert!(CapsuleMode::Hermetic.is_hermetic());
        assert!(CapsuleMode::Replay.is_hermetic());
    }

    #[test]
    fn capsule_mode_needs_checkpoint_only_for_replay() {
        assert!(!CapsuleMode::Thin.needs_checkpoint());
        assert!(!CapsuleMode::Hermetic.needs_checkpoint());
        assert!(!CapsuleMode::Headless.needs_checkpoint());
        assert!(CapsuleMode::Replay.needs_checkpoint());
    }

    #[test]
    fn capsule_mode_serializes_as_snake_case() {
        let v = serde_json::to_value(&CapsuleMode::Headless).unwrap();
        assert_eq!(v, serde_json::Value::String("headless".to_string()));
        let v: CapsuleMode = serde_json::from_str("\"thin\"").unwrap();
        assert!(matches!(v, CapsuleMode::Thin));
    }

    #[test]
    fn format_major_version_parses_supported_string() {
        let mut m = sample_manifest(CapsuleMode::Thin);
        m.format_version = "1.0.0".to_string();
        assert_eq!(m.format_major_version(), Some(1));
        m.format_version = "2.3.7".to_string();
        assert_eq!(m.format_major_version(), Some(2));
        m.format_version = "garbage".to_string();
        assert_eq!(m.format_major_version(), None);
    }

    #[test]
    fn format_version_defaults_when_absent_in_json() {
        // A minimal manifest with no `format_version` field should
        // deserialize using the default (current version).
        let json = r#"{
            "capsule_id": "cap_x",
            "mode": "thin",
            "created_at": "2026-05-28T00:00:00Z",
            "agent_id": "demo",
            "entrypoint": "SKILL.md",
            "runtime_lock": "runtime.lock"
        }"#;
        let parsed: CapsuleManifest = serde_json::from_str(json).expect("deserialize");
        assert_eq!(parsed.format_version, CAPSULE_FORMAT_VERSION);
        assert_eq!(parsed.provenance.trust_domain, "local");
    }

    #[test]
    fn signature_omitted_when_none() {
        let mut m = sample_manifest(CapsuleMode::Thin);
        m.signature = None;
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            !json.contains("\"signature\""),
            "None signature should be skipped, got: {json}"
        );
    }

    #[test]
    fn layer_embedded_path_returns_handle() {
        let m = sample_manifest(CapsuleMode::Hermetic);
        let layer = &m.included_layers[0];
        assert_eq!(
            m.layer_embedded_path(layer),
            Some("layers/layer_001/contents.tar.zst")
        );
    }
}
