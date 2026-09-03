//! Hash-chain Causal Logger — the lean witness (#1278).
//!
//! The gateway is DB-centred: `causal_events` in gateway.db is the queryable
//! store, and the `.jsonl` causal chain is its *witness* — a small,
//! append-only, hash-chained file that can be shipped offsite or put on WORM
//! media and verified without SQLite. To stay shippable it must stay small,
//! so entries carry fingerprints, not content:
//!
//! - the (redacted) payload is stored once in the content-addressed
//!   [`payload_cas_dir`] beside the witness (`payloads/<sha256>.json`),
//! - the entry commits to `payload_hash` (its SHA-256) and `payload_ref`
//!   (its CAS key — same hex, by construction),
//! - `enforced_rules` is bound into the entry hash so enforcement
//!   attribution (I-6) is tamper-evident, not just queryable,
//! - the entry hash commits to the *fingerprint set*, never to inline
//!   content — verification re-derives the hash from the fields and checks
//!   the prev-linkage across the whole chain (see [`verify_chain`]).
//!
//! Pre-existing segments are v1 (inline `payload`, no `v` field) and keep
//! verifying under their original field set via version dispatch — see
//! [`WITNESS_FORMAT_VERSION_V1`] in `autonoetic-types`.

pub mod promotion_lookup;
pub mod rotation;

pub use rotation::{
    generate_segment_filename, get_or_create_history_dir, migrate_legacy_log, parse_segment_info,
    read_all_entries_across_segments, RetentionActions, RetentionPolicy, RotationPolicy,
    RotationStrategy, SegmentIndex, SegmentMetadata,
};

use autonoetic_types::causal_chain::{
    CausalChainEntry, EntryStatus, WITNESS_FORMAT_VERSION_V1, WITNESS_FORMAT_VERSION_V2,
};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::log_redaction::RedactedPayload;

pub struct CausalLogger {
    pub log_path: PathBuf,
    last_hash: Mutex<String>,
}

impl CausalLogger {
    pub fn new(log_path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Self::new_with_policy(log_path, RotationPolicy::disabled())
    }

    pub fn new_with_policy(
        log_path: impl Into<PathBuf>,
        policy: RotationPolicy,
    ) -> anyhow::Result<Self> {
        let log_path = log_path.into();

        if policy.rotation_strategy != RotationStrategy::Disabled {
            if let Some(parent) = log_path.parent() {
                let _ = migrate_legacy_log(parent);
            }
        }

        // Tail read only: construction costs O(last line), not O(file), so
        // opening a logger per call site (SDK bridge, promotion, federation
        // emits) never re-scans the witness.
        let last_hash = load_last_hash_tail(&log_path);

        Ok(Self {
            log_path,
            last_hash: Mutex::new(last_hash),
        })
    }

    #[cfg(test)]
    pub fn test_logger(log_path: impl Into<PathBuf>) -> Self {
        let log_path = log_path.into();
        let last_hash = load_last_hash_tail(&log_path);
        Self {
            log_path,
            last_hash: Mutex::new(last_hash),
        }
    }

    /// Append a new action to the Causal Chain.
    ///
    /// The payload never enters the file: its content-addressed copy is
    /// written to [`payload_cas_dir`] and the entry carries
    /// `payload_hash`/`payload_ref` only.
    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &self,
        actor_id: &str,
        session_id: &str,
        turn_id: Option<&str>,
        event_seq: u64,
        category: &str,
        action: &str,
        status: EntryStatus,
        target: Option<&str>,
        enforced_rules: &[String],
        payload: Option<RedactedPayload>,
    ) -> anyhow::Result<()> {
        let mut last_hash_guard = self
            .last_hash
            .lock()
            .map_err(|_| anyhow::anyhow!("causal logger mutex poisoned"))?;
        let prev_hash = last_hash_guard.clone();

        let entry = self.build_entry(
            actor_id,
            session_id,
            turn_id,
            event_seq,
            category,
            action,
            status,
            target,
            enforced_rules,
            payload,
            &prev_hash,
        )?;

        append_entry(&self.log_path, &entry)?;

        *last_hash_guard = entry.entry_hash;

        Ok(())
    }

    /// Append a new action to the Causal Chain and fsync before returning.
    /// Use this for state-mutating events that gate a privileged operation
    /// (approval resolve, grant insert, promotion commit, emergency stop).
    /// Hot-path info events should use `log()` instead.
    #[allow(clippy::too_many_arguments)]
    pub fn log_durable(
        &self,
        actor_id: &str,
        session_id: &str,
        turn_id: Option<&str>,
        event_seq: u64,
        category: &str,
        action: &str,
        status: EntryStatus,
        target: Option<&str>,
        enforced_rules: &[String],
        payload: Option<RedactedPayload>,
    ) -> anyhow::Result<()> {
        let mut last_hash_guard = self
            .last_hash
            .lock()
            .map_err(|_| anyhow::anyhow!("causal logger mutex poisoned"))?;
        let prev_hash = last_hash_guard.clone();

        let entry = self.build_entry(
            actor_id,
            session_id,
            turn_id,
            event_seq,
            category,
            action,
            status,
            target,
            enforced_rules,
            payload,
            &prev_hash,
        )?;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        file.flush()?;
        file.sync_all()?;

        *last_hash_guard = entry.entry_hash;

        Ok(())
    }

    /// Build the lean (v2) entry: payload → CAS, hash over the fingerprint
    /// set. `prev_hash` is passed in by the caller holding the mutex guard.
    #[allow(clippy::too_many_arguments)]
    fn build_entry(
        &self,
        actor_id: &str,
        session_id: &str,
        turn_id: Option<&str>,
        event_seq: u64,
        category: &str,
        action: &str,
        status: EntryStatus,
        target: Option<&str>,
        enforced_rules: &[String],
        payload: Option<RedactedPayload>,
        prev_hash: &str,
    ) -> anyhow::Result<CausalChainEntry> {
        let payload = payload.map(|p| p.into_inner());
        let payload_hash = payload_hash(&payload)?;
        // The /dev/null logger is a deliberate no-op (retired gateway chain,
        // test fixtures): its parent directory is not a writable history dir,
        // so there is nowhere to put a content-addressed copy — and nothing
        // to put there, since the entry is discarded.
        let payload_ref = match &payload {
            // Content-addressed: the CAS key *is* the payload's SHA-256, so
            // ref and hash coincide by construction. Write-once — a colliding
            // file already holds these exact bytes.
            Some(value) if !is_noop_witness(&self.log_path) => {
                Some(put_payload(payload_cas_dir(&self.log_path), value)?)
            }
            _ => None,
        };

        let timestamp = chrono::Utc::now().to_rfc3339();
        let log_id = uuid::Uuid::new_v4().to_string();
        let entry_hash = compute_entry_hash_v2(
            &timestamp,
            &log_id,
            actor_id,
            session_id,
            turn_id,
            event_seq,
            category,
            action,
            target,
            &status,
            enforced_rules,
            payload_hash.as_deref(),
            payload_ref.as_deref(),
            prev_hash,
        )?;

        Ok(CausalChainEntry {
            timestamp,
            log_id,
            actor_id: actor_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.map(|v| v.to_string()),
            event_seq,
            category: category.to_string(),
            action: action.to_string(),
            target: target.map(|v| v.to_string()),
            status,
            reason: None,
            payload: None,
            payload_hash,
            payload_ref,
            enforced_rules: enforced_rules.to_vec(),
            format_version: WITNESS_FORMAT_VERSION_V2,
            prev_hash: prev_hash.to_string(),
            entry_hash,
        })
    }

    /// Get the log file path.
    pub fn path(&self) -> &std::path::Path {
        &self.log_path
    }

    /// Read all entries from the log file.
    pub fn read_entries(path: &std::path::Path) -> anyhow::Result<Vec<CausalChainEntry>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: CausalChainEntry = serde_json::from_str(trimmed)?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Read all entries across all segments with continuity validation.
    pub fn read_all_entries(
        history_dir: &std::path::Path,
    ) -> anyhow::Result<Vec<CausalChainEntry>> {
        read_all_entries_across_segments(history_dir)
    }
}

/// Directory of the content-addressed payload store that belongs to the
/// witness file at `log_path`: `<history_dir>/payloads/` — a sibling of the
/// `.jsonl` witness, so the pair ships (or seals) together.
pub fn payload_cas_dir(log_path: &Path) -> PathBuf {
    match log_path.parent() {
        Some(parent) => parent.join("payloads"),
        None => PathBuf::from("payloads"),
    }
}

/// The no-op logger writes to `/dev/null` — its entries are discarded by
/// construction, so it must skip side effects that assume a real history
/// directory (the payload CAS).
fn is_noop_witness(log_path: &Path) -> bool {
    log_path == std::path::Path::new("/dev/null")
}

/// Write `value` into the CAS under its SHA-256 and return the hex key.
///
/// Files are immutable and content-addressed, so a file already present under
/// this key must hold exactly these bytes. It is verified anyway: a leftover
/// from a crash mid-write (or disk corruption) would otherwise stay broken
/// forever while the witness entry commits to `payload_hash` — silently
/// poisoning every later `resolve_entry_payload`. A bad file is replaced with
/// the correct bytes via temp-file + rename, which is atomic within the
/// directory, so no reader ever observes a partial copy.
fn put_payload(cas_dir: PathBuf, value: &serde_json::Value) -> anyhow::Result<String> {
    let encoded = serde_json::to_string(value)?;
    let key = sha256_hex(&encoded)?;
    std::fs::create_dir_all(&cas_dir)?;
    let path = cas_dir.join(format!("{key}.json"));
    let intact = std::fs::read_to_string(&path)
        .map(|existing| existing == encoded)
        .unwrap_or(false);
    if !intact {
        let tmp = cas_dir.join(format!(
            ".{key}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&tmp, &encoded)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(key)
}

/// Resolve an entry's payload to its content.
///
/// - v1 entries carry the payload inline — return it.
/// - v2 entries point at the CAS copy: the bytes are read back and verified
///   against the committed `payload_hash`, so a tampered or truncated CAS
///   file fails loudly instead of silently "verifying".
pub fn resolve_entry_payload(log_path: &Path, entry: &CausalChainEntry) -> anyhow::Result<Option<serde_json::Value>> {
    if !entry.is_lean_witness() {
        return Ok(entry.payload.clone());
    }
    let Some(reference) = entry.payload_ref.as_deref() else {
        // A v2 entry with no payload_ref is only legitimate when there is no
        // payload at all (payload_hash also None).
        if entry.payload_hash.is_some() {
            anyhow::bail!(
                "witness entry {} commits to a payload_hash but carries no payload_ref",
                entry.log_id
            );
        }
        return Ok(None);
    };
    let encoded = std::fs::read_to_string(payload_cas_dir(log_path).join(format!("{reference}.json")))
        .map_err(|e| anyhow::anyhow!("payload {} unavailable from CAS: {}", reference, e))?;
    let actual = sha256_hex(&encoded)?;
    if Some(actual.as_str()) != entry.payload_hash.as_deref() {
        anyhow::bail!(
            "payload {} content hash mismatch: CAS bytes hash to {}, entry commits to {}",
            reference,
            actual,
            entry.payload_hash.as_deref().unwrap_or("<none>")
        );
    }
    Ok(serde_json::from_str(&encoded).ok())
}

/// v1 entry hash — the legacy field set, unchanged from the fat-witness era
/// so pre-existing segments keep verifying (the register's `hash_chain_integrity`
/// citation points here).
#[allow(clippy::too_many_arguments)]
pub fn compute_entry_hash(
    timestamp: &str,
    log_id: &str,
    actor_id: &str,
    session_id: &str,
    turn_id: Option<&str>,
    event_seq: u64,
    category: &str,
    action: &str,
    status: &EntryStatus,
    payload_hash: Option<&str>,
    prev_hash: &str,
) -> anyhow::Result<String> {
    let canonical = serde_json::json!({
        "timestamp": timestamp,
        "log_id": log_id,
        "actor_id": actor_id,
        "session_id": session_id,
        "turn_id": turn_id,
        "event_seq": event_seq,
        "category": category,
        "action": action,
        "status": status,
        "payload_hash": payload_hash,
        "prev_hash": prev_hash
    });
    let encoded = serde_json::to_string(&canonical)?;
    sha256_hex(&encoded)
}

/// v2 (lean witness) entry hash — the fingerprint set including `target`,
/// `enforced_rules`, and `payload_ref`, so the witnessed attribution and the
/// payload locator are as tamper-evident as the hash chain itself.
#[allow(clippy::too_many_arguments)]
pub fn compute_entry_hash_v2(
    timestamp: &str,
    log_id: &str,
    actor_id: &str,
    session_id: &str,
    turn_id: Option<&str>,
    event_seq: u64,
    category: &str,
    action: &str,
    target: Option<&str>,
    status: &EntryStatus,
    enforced_rules: &[String],
    payload_hash: Option<&str>,
    payload_ref: Option<&str>,
    prev_hash: &str,
) -> anyhow::Result<String> {
    let canonical = serde_json::json!({
        "timestamp": timestamp,
        "log_id": log_id,
        "actor_id": actor_id,
        "session_id": session_id,
        "turn_id": turn_id,
        "event_seq": event_seq,
        "category": category,
        "action": action,
        "target": target,
        "status": status,
        "enforced_rules": enforced_rules,
        "payload_hash": payload_hash,
        "payload_ref": payload_ref,
        "prev_hash": prev_hash,
        "v": WITNESS_FORMAT_VERSION_V2,
    });
    let encoded = serde_json::to_string(&canonical)?;
    sha256_hex(&encoded)
}

/// Re-derive an entry's hash under its own format version and compare with
/// the stored `entry_hash`. Version dispatch is the migration contract: a v1
/// entry is never validated against the v2 field set or vice versa.
pub fn verify_entry_hash(entry: &CausalChainEntry) -> anyhow::Result<bool> {
    let computed = match entry.format_version {
        WITNESS_FORMAT_VERSION_V1 => compute_entry_hash(
            &entry.timestamp,
            &entry.log_id,
            &entry.actor_id,
            &entry.session_id,
            entry.turn_id.as_deref(),
            entry.event_seq,
            &entry.category,
            &entry.action,
            &entry.status,
            entry.payload_hash.as_deref(),
            &entry.prev_hash,
        )?,
        WITNESS_FORMAT_VERSION_V2 => compute_entry_hash_v2(
            &entry.timestamp,
            &entry.log_id,
            &entry.actor_id,
            &entry.session_id,
            entry.turn_id.as_deref(),
            entry.event_seq,
            &entry.category,
            &entry.action,
            entry.target.as_deref(),
            &entry.status,
            &entry.enforced_rules,
            entry.payload_hash.as_deref(),
            entry.payload_ref.as_deref(),
            &entry.prev_hash,
        )?,
        other => anyhow::bail!("unsupported witness format version: {other}"),
    };
    // Constant-shape comparison is unnecessary for a hash the attacker
    // cannot predict offline; plain equality keeps this auditable.
    Ok(computed == entry.entry_hash)
}

/// Outcome of a full-witness verification pass ([`verify_chain`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerification {
    pub total_entries: usize,
    /// Entries whose own hash re-derived correctly before any break.
    pub verified_entries: usize,
    /// Index of the first entry that failed verification (hash mismatch,
    /// broken prev-linkage, or unsupported version), if any.
    pub broken_at: Option<usize>,
    pub reason: Option<String>,
}

impl ChainVerification {
    pub fn is_intact(&self) -> bool {
        self.broken_at.is_none()
    }
}

/// Verify the whole witness: per-entry hash recomputation (version-dispatched)
/// plus prev-hash linkage from genesis across all segments.
pub fn verify_chain(history_dir: &Path) -> anyhow::Result<ChainVerification> {
    let entries = read_all_entries_across_segments(history_dir)?;
    let mut verification = ChainVerification {
        total_entries: entries.len(),
        verified_entries: 0,
        broken_at: None,
        reason: None,
    };

    let mut expected_prev = "genesis".to_string();
    for (index, entry) in entries.iter().enumerate() {
        if entry.prev_hash != expected_prev {
            verification.broken_at = Some(index);
            verification.reason = Some(format!(
                "broken prev-hash linkage: expected {}, got {}",
                expected_prev, entry.prev_hash
            ));
            return Ok(verification);
        }
        match verify_entry_hash(entry) {
            Ok(true) => verification.verified_entries += 1,
            Ok(false) => {
                verification.broken_at = Some(index);
                verification.reason =
                    Some("entry_hash does not match re-derived hash".to_string());
                return Ok(verification);
            }
            Err(e) => {
                verification.broken_at = Some(index);
                verification.reason = Some(e.to_string());
                return Ok(verification);
            }
        }
        expected_prev = entry.entry_hash.clone();
    }

    Ok(verification)
}

/// The last entry's `event_seq`, read from the tail of the file only.
///
/// This is the counter primitive the SDK bridge needs — O(1) in the file
/// size for lean (v2) entries, so the witness is never re-read in full on a
/// hot path (#1278). Legacy v1 lines can embed large inline payloads; when
/// the final line outgrows the tail window, the window grows until the line
/// is complete, so a giant last line degrades to one bounded re-read instead
/// of silently resetting the counter to 1 (duplicate `event_seq`s).
pub fn last_entry_event_seq(log_path: &Path) -> u64 {
    let Ok(mut file) = std::fs::File::open(log_path) else {
        return 0;
    };
    let len = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => return 0,
    };
    if len == 0 {
        return 0;
    }
    let Some(last_line) = read_tail_last_line(&mut file, len) else {
        return 0;
    };
    serde_json::from_slice::<CausalChainEntry>(last_line.trim_ascii())
        .map(|entry| entry.event_seq)
        .unwrap_or(0)
}

/// The final non-empty line of a file, read backwards from the end.
///
/// Starts with a 64 KiB tail window — a v2 entry is well under a KiB — and
/// grows geometrically when the last line turns out to be longer (legacy v1
/// entries embed their payload inline), capped by the file size. A "line" is
/// complete once a `\n` precedes it inside the window (or the window reaches
/// the file start).
fn read_tail_last_line(file: &mut std::fs::File, len: u64) -> Option<Vec<u8>> {
    let mut window = 64 * 1024u64;
    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail).ok()?;

        // The candidate is the last non-empty chunk. It is *complete* only if
        // a newline precedes it inside the window (its chunk index > 0) or
        // the window reaches the file start — otherwise the window opened in
        // the middle of exactly this line, and the trailing '\n' at EOF (or
        // its absence) proves nothing about the chunk's left edge.
        let chunks: Vec<&[u8]> = tail.split(|b| *b == b'\n').collect();
        let candidate = chunks
            .iter()
            .enumerate()
            .rev()
            .find(|(_, chunk)| !chunk.trim_ascii().is_empty());
        match candidate {
            Some((idx, chunk)) if idx > 0 || start == 0 => return Some(chunk.to_vec()),
            // Incomplete last line (or an all-whitespace window): grow and
            // retry — the cap at `len` guarantees termination at start == 0.
            _ if start > 0 => {
                window = window.saturating_mul(4).min(len);
            }
            _ => return None,
        }
    }
}

/// The chain tip, read from the tail of the file only — same contract as the
/// previous full-scan loader (entry_hash of the final line; a raw sha256 for
/// pre-hash legacy lines; "genesis" when absent), at O(last line) cost.
fn load_last_hash_tail(log_path: &Path) -> String {
    const GENESIS: &str = "genesis";
    let Ok(mut file) = std::fs::File::open(log_path) else {
        return GENESIS.to_string();
    };
    let len = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => return GENESIS.to_string(),
    };
    if len == 0 {
        return GENESIS.to_string();
    }
    let Some(last_line) = read_tail_last_line(&mut file, len) else {
        return GENESIS.to_string();
    };
    let Ok(last_line) = std::str::from_utf8(last_line.trim_ascii()) else {
        return GENESIS.to_string();
    };
    if last_line.is_empty() {
        return GENESIS.to_string();
    }
    if let Ok(entry) = serde_json::from_str::<CausalChainEntry>(last_line) {
        if !entry.entry_hash.trim().is_empty() {
            return entry.entry_hash;
        }
    }
    sha256_hex(last_line).unwrap_or_else(|_| GENESIS.to_string())
}

fn payload_hash(payload: &Option<serde_json::Value>) -> anyhow::Result<Option<String>> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    let encoded = serde_json::to_string(payload)?;
    Ok(Some(sha256_hex(&encoded)?))
}

fn append_entry(log_path: &Path, entry: &CausalChainEntry) -> anyhow::Result<()> {
    // Rotation is handled externally via new_with_policy — the append is
    // always a tail write on whatever segment file this logger was opened on.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    writeln!(file, "{}", serde_json::to_string(entry)?)?;

    Ok(())
}

fn sha256_hex(input: &str) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    Ok(format!("{:x}", digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn default_rules() -> Vec<String> {
        vec!["R+++3".to_string()]
    }

    #[test]
    fn test_logger_reloads_last_hash_across_instances() {
        let temp = tempdir().expect("tempdir should create");
        let path = temp.path().join("causal_chain.jsonl");

        let logger = CausalLogger::new(&path).expect("logger should init");
        logger
            .log(
                "agent-a",
                "session-1",
                Some("turn-000001"),
                1,
                "lifecycle",
                "wake",
                EntryStatus::Success,
                None,
                &default_rules(),
                Some(RedactedPayload::from_redacted(serde_json::json!({"k":"v"}))),
            )
            .expect("first log should append");

        let content = std::fs::read_to_string(&path).expect("log should read");
        let first: CausalChainEntry =
            serde_json::from_str(content.lines().next().expect("first line should exist"))
                .expect("entry should parse");

        let logger2 = CausalLogger::new(&path).expect("second logger should init");
        logger2
            .log(
                "agent-a",
                "session-1",
                Some("turn-000001"),
                2,
                "lifecycle",
                "hibernate",
                EntryStatus::Success,
                None,
                &default_rules(),
                None,
            )
            .expect("second log should append");

        let content = std::fs::read_to_string(&path).expect("log should read");
        let second: CausalChainEntry =
            serde_json::from_str(content.lines().nth(1).expect("second line should exist"))
                .expect("entry should parse");
        assert_eq!(second.prev_hash, first.entry_hash);
    }

    #[test]
    fn lean_entries_carry_hash_and_ref_not_payload() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("causal_chain.jsonl");
        let logger = CausalLogger::new(&path).unwrap();

        let payload = serde_json::json!({"arguments": {"artifact_id": "art_1"}, "big": "blob"});
        logger
            .log(
                "agent-a",
                "session-1",
                None,
                1,
                "tool",
                "promotion_record",
                EntryStatus::Success,
                Some("art_1"),
                &default_rules(),
                Some(RedactedPayload::from_redacted(payload.clone())),
            )
            .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert!(
            value.get("payload").is_none(),
            "v2 witness must not embed the payload: {raw}"
        );
        assert_eq!(value["v"], 2);

        let entry = &CausalLogger::read_entries(&path).unwrap()[0];
        assert!(entry.is_lean_witness());
        assert_eq!(entry.payload, None);
        assert_eq!(entry.target.as_deref(), Some("art_1"));
        assert_eq!(entry.payload_ref.as_deref(), entry.payload_hash.as_deref());
        assert_eq!(entry.enforced_rules, default_rules());

        // The CAS copy holds the exact content and hashes to the commitment.
        let resolved = resolve_entry_payload(&path, entry)
            .expect("payload should resolve from CAS")
            .expect("payload should be present");
        assert_eq!(resolved, payload);
        assert!(
            verify_entry_hash(entry).unwrap(),
            "lean entry hash must re-derive from the fingerprint set"
        );
    }

    #[test]
    fn verify_chain_detects_hash_tampering_and_links_breaks() {
        let temp = tempdir().unwrap();
        let history_dir = temp.path().to_path_buf();
        let path = history_dir.join("causal_chain.jsonl");
        let logger = CausalLogger::new(&path).unwrap();
        for seq in 1..=3 {
            logger
                .log(
                    "agent-a",
                    "session-1",
                    None,
                    seq,
                    "tool",
                    "sandbox_exec",
                    EntryStatus::Success,
                    None,
                    &default_rules(),
                    Some(RedactedPayload::from_redacted(serde_json::json!({"seq": seq}))),
                )
                .unwrap();
        }

        let intact = verify_chain(&history_dir).unwrap();
        assert!(intact.is_intact(), "{:?}", intact.reason);
        assert_eq!(intact.total_entries, 3);
        assert_eq!(intact.verified_entries, 3);

        // Tamper with the first entry's action: stored hashes go stale.
        let raw = std::fs::read_to_string(&path).unwrap();
        let tampered = raw.replacen("sandbox_exec", "hijacked_exec", 1);
        std::fs::write(&path, tampered).unwrap();

        let broken = verify_chain(&history_dir).unwrap();
        assert_eq!(broken.broken_at, Some(0));
        assert_eq!(broken.verified_entries, 0);
        assert!(!broken.is_intact());
    }

    #[test]
    fn last_entry_event_seq_reads_tail_without_full_scan() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("causal_chain.jsonl");
        assert_eq!(last_entry_event_seq(&path), 0, "missing file → 0");

        let logger = CausalLogger::new(&path).unwrap();
        for seq in [1u64, 7, 42] {
            logger
                .log(
                    "agent-a",
                    "sdk-bridge",
                    None,
                    seq,
                    "memory",
                    "read",
                    EntryStatus::Success,
                    None,
                    &default_rules(),
                    None,
                )
                .unwrap();
        }
        assert_eq!(last_entry_event_seq(&path), 42);
    }

    #[test]
    fn put_payload_repairs_a_corrupted_existing_file_atomically() {
        let temp = tempdir().unwrap();
        let cas_dir = temp.path().to_path_buf();
        let value = serde_json::json!({"k": "v"});
        let encoded = serde_json::to_string(&value).unwrap();
        let key = sha256_hex(&encoded).unwrap();
        let path = cas_dir.join(format!("{key}.json"));

        // Leftover from a crash mid-write: truncated, wrong bytes.
        std::fs::write(&path, r#"{"k":"#).unwrap();

        let returned = put_payload(cas_dir.clone(), &value).unwrap();
        assert_eq!(returned, key);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            encoded,
            "corrupted CAS file must be repaired, not trusted"
        );

        // A subsequent put with the same content is a verified no-op.
        let returned = put_payload(cas_dir.clone(), &value).unwrap();
        assert_eq!(returned, key);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), encoded);
    }

    #[test]
    fn last_entry_event_seq_handles_last_line_larger_than_the_tail_window() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("causal_chain.jsonl");

        // Small v2 entry first, then a legacy v1-style line whose inline
        // payload pushes it past the 64 KiB default tail window.
        let logger = CausalLogger::new(&path).unwrap();
        logger
            .log(
                "agent-a",
                "sdk-bridge",
                None,
                1,
                "memory",
                "read",
                EntryStatus::Success,
                None,
                &default_rules(),
                None,
            )
            .unwrap();

        let padding = "x".repeat(200 * 1024);
        let fat_v1 = serde_json::json!({
            "timestamp": "2026-01-01T00:00:00+00:00",
            "log_id": "fat-1",
            "actor_id": "agent-a",
            "session_id": "sdk-bridge",
            "event_seq": 997,
            "category": "memory",
            "action": "read",
            "status": "SUCCESS",
            "payload": {"blob": padding},
            "payload_hash": null,
            "prev_hash": "genesis",
            "entry_hash": "",
        });
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{fat_v1}").unwrap();
        drop(file);

        assert_eq!(
            last_entry_event_seq(&path),
            997,
            "an oversized final line must be read completely, never reset the counter"
        );
    }

    #[test]
    fn logger_constructor_takes_only_the_tail_for_the_chain_tip() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("causal_chain.jsonl");

        let logger = CausalLogger::new(&path).unwrap();
        logger
            .log(
                "agent-a",
                "session-1",
                None,
                1,
                "tool",
                "sandbox_exec",
                EntryStatus::Success,
                None,
                &default_rules(),
                Some(RedactedPayload::from_redacted(serde_json::json!({"n": 1}))),
            )
            .unwrap();
        let tip = {
            let entries = CausalLogger::read_entries(&path).unwrap();
            entries.last().unwrap().entry_hash.clone()
        };

        // A fresh logger must resume exactly at the tip — the constructor
        // reads the last line, not the whole file.
        let logger2 = CausalLogger::new(&path).unwrap();
        logger2
            .log(
                "agent-a",
                "session-1",
                None,
                2,
                "tool",
                "sandbox_exec",
                EntryStatus::Success,
                None,
                &default_rules(),
                None,
            )
            .unwrap();
        let entries = CausalLogger::read_entries(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].prev_hash, tip);
        assert!(
            verify_chain(temp.path()).unwrap().is_intact(),
            "chain must continue across logger instances"
        );
    }

    #[test]
    fn resolve_entry_payload_rejects_tampered_cas_bytes() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("causal_chain.jsonl");
        let logger = CausalLogger::new(&path).unwrap();
        logger
            .log(
                "agent-a",
                "session-1",
                None,
                1,
                "tool",
                "promotion_record",
                EntryStatus::Success,
                None,
                &default_rules(),
                Some(RedactedPayload::from_redacted(serde_json::json!({"a": 1}))),
            )
            .unwrap();

        let entry = &CausalLogger::read_entries(&path).unwrap()[0];
        let cas_file = payload_cas_dir(&path)
            .join(format!("{}.json", entry.payload_ref.as_deref().unwrap()));
        std::fs::write(&cas_file, r#"{"a": 2}"#).unwrap();

        let err = resolve_entry_payload(&path, entry)
            .expect_err("hash mismatch between CAS bytes and commitment must fail loudly");
        assert!(err.to_string().contains("mismatch"), "{err}");
    }
}
