//! Cognitive Capsule export/import pipelines.
//!
//! A Cognitive Capsule is a portable, signed, revision-pinned snapshot of
//! an agent — see `docs/guide/cognitive-capsule.md`. The
//! schema lives in `autonoetic-types/src/capsule.rs`; this module holds
//! the gateway-side pipelines that produce and consume those archives.
//!
//! Surface:
//!
//! - [`export::export`] — package a revision into a `tar.zst` archive
//! - [`import::import`] — extract an archive and create a new revision
//! - [`verify::manifest_digest`] / [`verify::verify_signature`] — canonical
//!   digest + Ed25519 signature checks
//! - [`archive::pack`] / [`archive::unpack`] — low-level archive helpers
//!
//! Phase 2 shipped the thin-mode happy path. Phase 4 adds Replay-mode
//! checkpoint bundle/restore, Headless-mode scheduled-job bundle/recreate,
//! real memory-snapshot enumeration with a conflict policy, and a
//! platform compatibility refusal that fires in non-`local` trust domains.

pub mod archive;
pub mod export;
pub mod import;
pub mod verify;

pub use export::{
    export, infer_capsule_destination_sink, resolve_capsule_destination_sink, ExportContext,
    ExportOutcome, ExportRequest,
};
pub use import::{import, ImportContext, ImportOutcome, ImportRequest, MemoryConflictPolicy};

/// Canonical relative paths inside the capsule archive.
pub mod paths {
    pub const CAPSULE_JSON: &str = "capsule.json";
    pub const SKILL_REL: &str = "SKILL.md";
    pub const RUNTIME_LOCK_REL: &str = "runtime.lock";
    pub const MEMORY_SNAPSHOT_PATH: &str = "memory/memory_snapshot.json";
    pub const CHECKPOINT_PATH: &str = "checkpoint/checkpoint.json";
}
