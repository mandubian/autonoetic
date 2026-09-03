//! Canonical constitution digest utilities (P-10.9).
//!
//! Digest input is intentionally explicit and deterministic:
//! - full constitution text
//! - right-id -> enforcement citation table
//! - rule-id -> enforcement citation table
//!
//! Runtime wiring:
//! - paths come from `GatewayConfig.constitution`
//! - `initialize_constitution()` must run before read APIs
//! - startup refuses boot if lock integrity checks fail.

use autonoetic_types::config::GatewayConfig;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConstitutionCanonicalization {
    pub algorithm: String,
    pub payload: String,
    pub rules_prefix: String,
    pub rights_prefix: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConstitutionLockSignature {
    pub algorithm: String,
    pub signer_id: String,
    pub signature_b64: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConstitutionLock {
    pub format_version: u32,
    pub constitution_id: String,
    pub constitution_version: String,
    pub constitution_source: String,
    pub constitution_digest: String,
    pub rule_enforcement_count: usize,
    pub right_enforcement_count: usize,
    pub canonicalization: ConstitutionCanonicalization,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<ConstitutionLockSignature>,
}

#[derive(Debug)]
struct ConstitutionRuntime {
    // Read only by `ensure_same_runtime_config`, which is reached only from
    // the `#[cfg(not(test))]` arm of `initialize_constitution` -- so both the
    // fields and the check read as dead in the lib-test build while being
    // live in production.
    #[cfg_attr(test, allow(dead_code))]
    source_path: PathBuf,
    #[cfg_attr(test, allow(dead_code))]
    lock_path: PathBuf,
    gateway_dir: PathBuf,
    configured_source_label: String,
    text: Arc<str>,
    digest: Arc<str>,
    rights_enforcement: BTreeMap<String, String>,
    rules_enforcement: BTreeMap<String, String>,
    require_signature: bool,
    trusted_signers: HashMap<String, String>,
    lock: Arc<ConstitutionLock>,
}

impl ConstitutionRuntime {
    fn load(config: &GatewayConfig) -> anyhow::Result<Self> {
        let configured_source_label = normalize_config_path_label(&config.constitution.source_path);
        let (source_path, lock_path) = resolve_constitution_artifact_paths(config)?;

        let text = std::fs::read_to_string(&source_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read constitution source '{}': {}",
                source_path.display(),
                e
            )
        })?;
        let lock_json = std::fs::read_to_string(&lock_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read constitution lock '{}': {}",
                lock_path.display(),
                e
            )
        })?;
        let lock: ConstitutionLock = serde_json::from_str(&lock_json).map_err(|e| {
            anyhow::anyhow!(
                "constitution lock '{}' must be valid JSON: {}",
                lock_path.display(),
                e
            )
        })?;

        let rights_enforcement = extract_enforcement_table(&text, "Ri-");
        let rules_enforcement = extract_enforcement_table(&text, "P-");
        let payload = canonical_digest_payload(&text, &rights_enforcement, &rules_enforcement);
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        let digest = hex::encode(hasher.finalize());
        let gateway_dir = crate::execution::gateway_root_dir(&config);
        let text = Arc::<str>::from(text);
        let digest = Arc::<str>::from(digest);
        let lock = Arc::new(lock);

        Ok(Self {
            source_path,
            lock_path,
            gateway_dir,
            configured_source_label,
            text,
            digest,
            rights_enforcement,
            rules_enforcement,
            require_signature: config.constitution.require_signature,
            trusted_signers: config.constitution.trusted_signers.clone(),
            lock,
        })
    }
}

static RUNTIME: RwLock<Option<Arc<ConstitutionRuntime>>> = RwLock::new(None);

pub fn initialize_constitution(config: &GatewayConfig) -> anyhow::Result<()> {
    let loaded = Arc::new(ConstitutionRuntime::load(config)?);
    let mut guard = RUNTIME.write().expect("poisoned constitution runtime lock");
    match guard.as_ref() {
        None => {
            *guard = Some(loaded);
            Ok(())
        }
        Some(existing) => {
            // Production keeps the strict single-process drift guard. In the
            // gateway crate's own unit tests, however, each test constructs a
            // router/store with its own tempdir `gateway_dir`, so the guard
            // (which compares the constitution artifacts + signature policy)
            // would reject a differently-pathed second router in the shared
            // lib-test process. Allow re-init to replace the global under
            // test — the unit suite runs single-threaded, so no two tests
            // race on it. Integration tests run as separate processes (fresh
            // global each), so they exercise the production path below.
            #[cfg(test)]
            {
                let _ = existing;
                *guard = Some(loaded);
                Ok(())
            }
            #[cfg(not(test))]
            {
                ensure_same_runtime_config(existing.as_ref(), loaded.as_ref())
            }
        }
    }
}

/// Clears process-local constitution state so a different workspace can initialize in the same process.
/// Only compiled when the `test-utils` feature is enabled (e.g. from `autonoetic` unit tests).
#[cfg(feature = "test-utils")]
pub fn reset_constitution_runtime_for_tests() {
    *RUNTIME.write().expect("poisoned constitution runtime lock") = None;
}

/// Whether the constitution runtime has been initialized in this process.
///
/// Test helper for the "init-or-tolerate-neighbor" pattern: tests that need
/// the runtime but may share a process with other tests that initialized it
/// (possibly with a different config) should attempt their own
/// `initialize_constitution` and, on error, assert this rather than
/// swallowing the failure (`let _ = ...` hides genuine load errors behind a
/// later, less actionable digest panic).
pub fn is_constitution_initialized() -> bool {
    RUNTIME
        .read()
        .expect("poisoned constitution runtime lock")
        .is_some()
}

#[cfg_attr(test, allow(dead_code))]
fn ensure_same_runtime_config(
    existing: &ConstitutionRuntime,
    loaded: &ConstitutionRuntime,
) -> anyhow::Result<()> {
    // `gateway_dir` is deliberately NOT compared (#1090): it only locates the
    // `gateway:` signer public key at verify time. Constitution identity is
    // the artifacts + signature policy below, so two gateway dirs sharing the
    // same constitution must not panic a shared process (cargo test runs many
    // tests in one process, each with its own tempdir gateway_dir). A
    // `gateway:` signer resolved against a different dir still fails loudly at
    // key-fingerprint verification.
    anyhow::ensure!(
        existing.source_path == loaded.source_path
            && existing.lock_path == loaded.lock_path
            && existing.require_signature == loaded.require_signature
            && existing.trusted_signers == loaded.trusted_signers,
        "constitution runtime already initialized and cannot switch constitutional config in the same process (existing source='{}', lock='{}')",
        existing.source_path.display(),
        existing.lock_path.display(),
    );
    Ok(())
}

fn runtime_arc() -> Arc<ConstitutionRuntime> {
    RUNTIME
        .read()
        .expect("poisoned constitution runtime lock")
        .as_ref()
        .cloned()
        .expect(
            "constitution runtime not initialized; call initialize_constitution(config) before accessing digest/profile APIs",
        )
}

pub fn constitution_text() -> Arc<str> {
    runtime_arc().text.clone()
}

pub fn constitution_lock() -> Arc<ConstitutionLock> {
    runtime_arc().lock.clone()
}

pub fn constitution_version() -> Arc<str> {
    Arc::from(runtime_arc().lock.constitution_version.as_str())
}

pub fn constitution_format_version() -> u32 {
    runtime_arc().lock.format_version
}

pub fn constitution_digest() -> Arc<str> {
    runtime_arc().digest.clone()
}

/// Best-effort `(version, digest)` pair for session-level pinning (#821).
///
/// Unlike the strict getters above, this never panics: it returns `None`
/// when the constitution runtime has not been initialized (common in unit
/// tests that construct a router/executor without calling
/// `initialize_constitution`) or if the lock has been poisoned. Callers that
/// capture a per-session constitution pin should use this instead of the
/// panicking getters, since a session must be able to start even when no
/// constitution config is wired up.
pub fn try_constitution_pin() -> Option<(String, String)> {
    let guard = RUNTIME.read().ok()?;
    let rt = guard.as_ref()?;
    Some((rt.lock.constitution_version.clone(), rt.digest.to_string()))
}

pub fn canonical_right_enforcement_table() -> BTreeMap<String, String> {
    runtime_arc().rights_enforcement.clone()
}

pub fn canonical_rule_enforcement_table() -> BTreeMap<String, String> {
    runtime_arc().rules_enforcement.clone()
}

/// Build the client-facing constitution view (`constitution.get`): lock
/// metadata + one gloss per `P-*`/`Ri-*` clause, with enforcement citations
/// where the gateway mechanically enforces a clause. `include_text` attaches
/// the full markdown. Source of truth is the loaded constitution text, so the
/// gloss can never drift from what was signed.
pub fn constitution_profile(
    include_text: bool,
) -> autonoetic_types::constitution::ConstitutionGetResult {
    use autonoetic_types::constitution::{ConstitutionClause, ConstitutionGetResult};
    let rt = runtime_arc();
    let lock = rt.lock.as_ref();
    let glossary = extract_rule_glossary(&rt.text);
    let clauses = glossary
        .into_iter()
        .map(|(id, gloss)| {
            // Which *enforcement table* holds the citation is legitimately
            // keyed by prefix — the signed lock carries two tables, and an
            // `Ri-` id is a key in one of them. Which *power the clause
            // binds* is not: that is declared per clause in the enforcement
            // register (#1284), and deriving it from the same prefix is what
            // made this surface report the agent as responsible for
            // causal-chain integrity and egress confinement.
            let enforcement = if id.starts_with("Ri-") {
                rt.rights_enforcement.get(&id).cloned()
            } else {
                rt.rules_enforcement.get(&id).cloned()
            };
            let binds = crate::enforcement_register::binds(&id)
                .map(|b| b.label().to_string())
                .unwrap_or_else(|| {
                    autonoetic_types::constitution::BINDS_UNDECLARED.to_string()
                });
            ConstitutionClause {
                id,
                binds,
                gloss,
                enforcement,
            }
        })
        .collect();
    ConstitutionGetResult {
        version: lock.constitution_version.clone(),
        digest: rt.digest.to_string(),
        format_version: lock.format_version,
        signer_id: lock.signature.as_ref().map(|s| s.signer_id.clone()),
        signed: lock.signature.is_some(),
        rule_enforcement_count: lock.rule_enforcement_count,
        right_enforcement_count: lock.right_enforcement_count,
        clauses,
        text: include_text.then(|| rt.text.to_string()),
    }
}

pub fn canonical_constitution_profile() -> autonoetic_ofp::wire::ConstitutionProfile {
    autonoetic_ofp::wire::ConstitutionProfile {
        rules_enforcement: canonical_rule_enforcement_table(),
        rights_enforcement: canonical_right_enforcement_table(),
    }
}

pub fn verify_constitution_lock_integrity() -> anyhow::Result<()> {
    let rt = runtime_arc();
    let lock = rt.lock.as_ref();
    anyhow::ensure!(
        lock.constitution_source == rt.configured_source_label,
        "constitution lock source mismatch (lock='{}', configured='{}')",
        lock.constitution_source,
        rt.configured_source_label
    );
    anyhow::ensure!(
        lock.constitution_digest.as_str() == rt.digest.as_ref(),
        "constitution lock digest mismatch (lock={}, computed={}). \
         The markdown and lock file are out of sync: if you edited `constitution.md`, run \
         `python3 docs/constitution/recompute_lock.py --version {}` with the project signing key \
         (see AGENTS.md / docs/constitution/signing.md). \
         Also ensure `constitution.source_path` and `constitution.lock_path` resolve under the same directory \
         so the gateway does not pair a markdown file from one tree with a lock from another.",
        lock.constitution_digest,
        rt.digest.as_ref(),
        lock.constitution_version
    );
    anyhow::ensure!(
        lock.rule_enforcement_count == rt.rules_enforcement.len(),
        "constitution lock rule count mismatch (lock={}, computed={})",
        lock.rule_enforcement_count,
        rt.rules_enforcement.len()
    );
    anyhow::ensure!(
        lock.right_enforcement_count == rt.rights_enforcement.len(),
        "constitution lock right count mismatch (lock={}, computed={})",
        lock.right_enforcement_count,
        rt.rights_enforcement.len()
    );
    anyhow::ensure!(
        lock.canonicalization.algorithm == "sha256",
        "constitution lock canonicalization.algorithm must be 'sha256'"
    );
    anyhow::ensure!(
        lock.canonicalization.payload == "json({constitution_text,rights_enforcement,rules_enforcement})",
        "constitution lock canonicalization.payload must match the canonical digest payload declaration"
    );
    anyhow::ensure!(
        lock.canonicalization.rules_prefix == "P-",
        "constitution lock canonicalization.rules_prefix must be 'P-'"
    );
    anyhow::ensure!(
        lock.canonicalization.rights_prefix == "Ri-",
        "constitution lock canonicalization.rights_prefix must be 'Ri-'"
    );
    if let Some(signature) = lock.signature.as_ref() {
        anyhow::ensure!(
            signature.algorithm.eq_ignore_ascii_case("ed25519"),
            "constitution lock signature.algorithm must be 'ed25519'"
        );
        let payload = constitution_lock_signature_payload(lock)?;
        let public_key = resolve_constitution_signer_public_key(rt.as_ref(), &signature.signer_id)?;
        let verified = crate::runtime::crypto::verify_attestation_signature(
            &public_key,
            &payload,
            &signature.signature_b64,
        )?;
        if !verified {
            let mut hasher = Sha256::new();
            hasher.update(&payload);
            let payload_sha256 = hex::encode(hasher.finalize());
            anyhow::bail!(
                "constitution lock signature verification failed for signer '{}' \
                 (canonical signed payload sha256={}, {} bytes). \
                 Rebuild the gateway from current sources (`cargo build`); older binaries hashed the payload with the wrong JSON key order. \
                 If you re-signed the lock, set `constitution.trusted_signers['{}']` to the base64 public key printed by `docs/constitution/recompute_lock.py` for the same private key that produced `signature_b64`.",
                signature.signer_id,
                payload_sha256,
                payload.len(),
                signature.signer_id
            );
        }
    } else {
        anyhow::ensure!(
            !rt.require_signature,
            "constitution lock is unsigned but signature is required by config (constitution.require_signature=true)"
        );
    }
    Ok(())
}

/// Recursive key sort to match `docs/constitution/recompute_lock.py` `compact_json_bytes`
/// (`json.dumps(..., sort_keys=True, separators=(",", ":"))`).
fn sort_json_keys_for_constitution_signature(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sort_json_keys_for_constitution_signature(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(sort_json_keys_for_constitution_signature)
                .collect(),
        ),
        other => other,
    }
}

pub fn constitution_lock_signature_payload(lock: &ConstitutionLock) -> anyhow::Result<Vec<u8>> {
    let payload = json!({
        "format_version": lock.format_version,
        "constitution_id": lock.constitution_id,
        "constitution_version": lock.constitution_version,
        "constitution_source": lock.constitution_source,
        "constitution_digest": lock.constitution_digest,
        "rule_enforcement_count": lock.rule_enforcement_count,
        "right_enforcement_count": lock.right_enforcement_count,
        "canonicalization": lock.canonicalization,
    });
    let sorted = sort_json_keys_for_constitution_signature(payload);
    serde_json::to_vec(&sorted)
        .map_err(|e| anyhow::anyhow!("failed to serialize constitution signature payload: {}", e))
}

fn resolve_constitution_signer_public_key(
    rt: &ConstitutionRuntime,
    signer_id: &str,
) -> anyhow::Result<[u8; 32]> {
    if let Some(fingerprint) = signer_id.strip_prefix("gateway:") {
        let public_path = rt
            .gateway_dir
            .join(crate::runtime::crypto::GatewayIdentityKey::PUBLIC_FILENAME);
        let bytes = std::fs::read(&public_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to read gateway signer public key '{}': {}",
                public_path.display(),
                e
            )
        })?;
        anyhow::ensure!(
            bytes.len() == 32,
            "gateway signer public key '{}' has wrong length ({} bytes, expected 32)",
            public_path.display(),
            bytes.len()
        );
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        let actual = hex::encode(&out[..8]);
        anyhow::ensure!(
            fingerprint == actual,
            "gateway signer fingerprint mismatch (signer_id='{}', key fingerprint='{}')",
            signer_id,
            actual
        );
        return Ok(out);
    }

    let encoded = rt
        .trusted_signers
        .get(signer_id)
        .ok_or_else(|| anyhow::anyhow!("constitution signer '{}' is not trusted", signer_id))?;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let decoded = STANDARD.decode(encoded).map_err(|e| {
        anyhow::anyhow!(
            "invalid base64 public key for signer '{}': {}",
            signer_id,
            e
        )
    })?;
    anyhow::ensure!(
        decoded.len() == 32,
        "trusted signer '{}' key has wrong length ({} bytes, expected 32)",
        signer_id,
        decoded.len()
    );
    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded);
    Ok(out)
}

fn canonical_digest_payload(
    constitution_text: &str,
    rights_enforcement: &BTreeMap<String, String>,
    rules_enforcement: &BTreeMap<String, String>,
) -> String {
    let payload = json!({
        "constitution_text": constitution_text,
        "rights_enforcement": rights_enforcement,
        "rules_enforcement": rules_enforcement,
    });
    serde_json::to_string(&payload).unwrap_or_default()
}

fn extract_enforcement_table(text: &str, id_prefix: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        // Data rows in markdown tables always start with '|'.
        if !line.trim_start().starts_with('|') {
            continue;
        }
        let cells: Vec<String> = line
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .map(str::to_string)
            .collect();
        // Target tables have 5 columns: ID | Rule/Right | Why/Source | Enforcement | Status.
        if cells.len() < 4 {
            continue;
        }
        let id = &cells[0];
        if !id.starts_with(id_prefix) {
            continue;
        }
        // Skip header/separator rows and keep only concrete IDs.
        if id == "ID" || id.starts_with("---") {
            continue;
        }
        let enforcement = cells[3].trim().to_string();
        if enforcement.is_empty() {
            continue;
        }
        out.insert(id.clone(), enforcement);
    }
    out
}

/// Derive a one-line glossary `clause_id -> short statement` straight from the
/// constitution text — the single source of truth — so no hand-maintained map
/// can drift from it. The gloss is the **first sentence** of the clause's
/// statement column (cells[1]); it covers every `P-*` rule and `Ri-*` right
/// row in the document.
pub fn extract_rule_glossary(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if !line.trim_start().starts_with('|') {
            continue;
        }
        let cells: Vec<String> = line
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .map(str::to_string)
            .collect();
        if cells.len() < 4 {
            continue;
        }
        let id = &cells[0];
        let is_clause = (id.starts_with("P-") || id.starts_with("Ri-"))
            && id.chars().nth(if id.starts_with("Ri-") { 3 } else { 2 }).is_some_and(|c| c.is_ascii_digit());
        if !is_clause || id == "ID" || id.starts_with("---") {
            continue;
        }
        let gloss = first_sentence(&cells[1]);
        if !gloss.is_empty() {
            out.insert(id.clone(), gloss);
        }
    }
    out
}

/// The first sentence of a clause statement: text up to the first sentence
/// terminator (`. `, `; `, or `: ` followed by a space) that is **not inside a
/// backtick code span** — so `` `error_type: fatal` `` doesn't split mid-span —
/// and **not inside an open parenthetical** — so a clause like "opt-in (flag;
/// defaults false); rest" keeps its parenthetical whole instead of cutting to
/// "opt-in (flag". Unbalanced parens degrade to "no terminator found": the
/// whole string is returned rather than a run-on split at the first outside
/// terminator. Returns the whole string if no terminator is found. Markdown
/// emphasis markers are left intact (they render fine in the consuming
/// surfaces).
fn first_sentence(statement: &str) -> String {
    let s = statement.trim();
    let mut in_code = false;
    let mut paren_depth: usize = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '`' => in_code = !in_code,
            _ if in_code => {}
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '.' | ';' | ':'
                if paren_depth == 0
                    && chars.peek().is_some_and(|(_, n)| *n == ' ') =>
            {
                return s[..=i].trim_end_matches([';', ':']).to_string();
            }
            _ => {}
        }
    }
    s.to_string()
}

/// Resolve constitution markdown and lock paths from the **same** search root.
///
/// Per-path resolution would pick the first filesystem location where each file exists,
/// which can pair a `constitution.md` from one tree with a `gateway-constitution.lock.json`
/// from another (digest mismatch at startup). We only accept a root when both paths exist
/// there as regular files.
fn resolve_constitution_artifact_paths(
    config: &GatewayConfig,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let source_configured = &config.constitution.source_path;
    let lock_configured = &config.constitution.lock_path;
    let source_abs = source_configured.is_absolute();
    let lock_abs = lock_configured.is_absolute();
    if source_abs || lock_abs {
        anyhow::ensure!(
            source_abs && lock_abs,
            "constitution source_path and lock_path must both be absolute or both be relative (source='{}', lock='{}')",
            source_configured.display(),
            lock_configured.display(),
        );
        return Ok((
            source_configured.to_path_buf(),
            lock_configured.to_path_buf(),
        ));
    }

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")));

    let mut bases: Vec<PathBuf> = Vec::new();
    // The runtime dir first: `bootstrap_constitution_snapshot` writes the local
    // snapshot there and records `ACTIVE.json` paths relative to it.
    bases.push(crate::execution::gateway_root_dir(config));
    bases.push(config.agents_dir.clone());
    bases.push(
        config
            .agents_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
    );
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }
    bases.push(workspace_root.to_path_buf());

    let mut tried: Vec<String> = Vec::new();
    for base in &bases {
        let source_path = base.join(source_configured);
        let lock_path = base.join(lock_configured);
        tried.push(format!(
            "  base='{}' -> source='{}', lock='{}'",
            base.display(),
            source_path.display(),
            lock_path.display()
        ));
        if source_path.is_file() && lock_path.is_file() {
            return Ok((source_path, lock_path));
        }
    }

    anyhow::bail!(
        "constitution source_path and lock_path must exist together under the same search root \
         (runtime_dir, agents_dir, agents_dir parent, current working directory, or workspace). \
         Configured source='{}', lock='{}'. Tried:\n{}",
        source_configured.display(),
        lock_configured.display(),
        tried.join("\n")
    );
}

fn normalize_config_path_label(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sentence_splits_on_terminators() {
        // Period, semicolon, colon (each followed by a space) terminate.
        assert_eq!(first_sentence("One thing. Two thing."), "One thing.");
        assert_eq!(first_sentence("Lead clause; detail follows."), "Lead clause");
        assert_eq!(
            first_sentence("Sessions end only for reasons: (a) exit, (b) budget."),
            "Sessions end only for reasons"
        );
        // No terminator → whole string (trimmed).
        assert_eq!(first_sentence("  just one clause  "), "just one clause");
        // A terminator not followed by a space does not split (e.g. decimals).
        assert_eq!(first_sentence("Cap is 3.5 units total."), "Cap is 3.5 units total.");
    }

    #[test]
    fn first_sentence_ignores_terminators_inside_code_spans() {
        // The ": " inside `error_type: fatal` must NOT split; the real split is
        // the later "; ".
        assert_eq!(
            first_sentence("`error_type: fatal` triggers session abort; recoverable types do not."),
            "`error_type: fatal` triggers session abort"
        );
        // A code span with no outside terminator returns the whole statement.
        assert_eq!(
            first_sentence("`a: b` and `c: d` only"),
            "`a: b` and `c: d` only"
        );
    }

    #[test]
    fn first_sentence_ignores_terminators_inside_parentheticals() {
        // The 2026.08.30 P-5.8 shape: the "; " inside the manifest parenthetical
        // must not cut the gloss to an unmatched "…(manifest `a: b`". The split
        // happens at the "; " that closes the sentence after the parenthetical.
        assert_eq!(
            first_sentence(
                "Strictly opt-in (manifest `a: b`; `c` defaults to false); repair is clamped."
            ),
            "Strictly opt-in (manifest `a: b`; `c` defaults to false)"
        );
        // A period inside a parenthetical is not a sentence end either.
        assert_eq!(
            first_sentence("Bounded loop (cap 2. never more); exhaustion errors."),
            "Bounded loop (cap 2. never more)"
        );
        // Parens inside code spans do not open a parenthetical.
        assert_eq!(
            first_sentence("`use (x; y)` optional; after that, done."),
            "`use (x; y)` optional"
        );
        // Unbalanced parens degrade to "no terminator found" — whole string,
        // never a run-on cut at the first outside terminator.
        assert_eq!(
            first_sentence("Unclosed (parenthesis; text continues here"),
            "Unclosed (parenthesis; text continues here"
        );
    }

    #[test]
    fn extract_rule_glossary_covers_rules_and_rights_only() {
        let text = "\
| ID | Rule | Source | Enforcement | Status |\n\
| P-1.1 | Every tool call matches a declared capability; no overrides. | x | y | ENFORCED |\n\
| Ri-0.3 | Every rejection names the rule ID. Always. | x | y | ENFORCED |\n\
| I-6 | Not a row id we gloss | x | y | ENFORCED |\n\
| R+9 | retired marker, ignored | x | y | ENFORCED |\n";
        let g = extract_rule_glossary(text);
        assert_eq!(g.get("P-1.1").map(String::as_str), Some("Every tool call matches a declared capability"));
        assert_eq!(g.get("Ri-0.3").map(String::as_str), Some("Every rejection names the rule ID."));
        // Only P-* / Ri-* clause rows are glossed; invariants and retired markers are not.
        assert!(!g.contains_key("I-6"));
        assert!(!g.contains_key("R+9"));
    }

    fn init_default_constitution() {
        initialize_constitution(&GatewayConfig::default())
            .expect("default constitution config should initialize");
    }

    #[test]
    fn constitution_digest_is_stable_hex_sha256() {
        init_default_constitution();
        let d = constitution_digest();
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(d, constitution_digest());
    }

    #[test]
    fn extracts_right_enforcement_rows() {
        init_default_constitution();
        let rights = canonical_right_enforcement_table();
        assert!(rights.contains_key("Ri-0.10"));
        assert!(rights
            .get("Ri-0.10")
            .expect("Ri-0.10 must exist")
            .contains("constitution_read"));
    }

    #[test]
    fn extracts_rule_enforcement_rows() {
        init_default_constitution();
        let rules = canonical_rule_enforcement_table();
        assert!(rules.contains_key("P-1.1"));
        assert!(rules
            .get("P-1.1")
            .expect("P-1.1 must exist")
            .contains("tool_call_processor"));
    }

    #[test]
    fn constitution_lock_matches_canonical_digest_and_counts() {
        init_default_constitution();
        verify_constitution_lock_integrity().expect("constitution lock integrity should hold");
    }

    #[test]
    fn constitution_profile_exposes_clauses_and_metadata() {
        init_default_constitution();
        let lock = constitution_lock();

        // Lightweight by default: no full text, metadata mirrors the lock.
        let p = constitution_profile(false);
        assert!(p.text.is_none(), "include_text=false must omit the markdown");
        assert_eq!(p.version, lock.constitution_version);
        assert_eq!(p.digest.as_str(), constitution_digest().as_ref());
        assert!(p.signed && p.signer_id.as_deref() == Some("autonoetic:constitution:v1"));
        assert_eq!(p.rule_enforcement_count, lock.rule_enforcement_count);
        assert_eq!(p.right_enforcement_count, lock.right_enforcement_count);
        assert!(p.clauses.len() > 100, "expected the full clause set");

        // Bind direction comes from the register's declared field (#1284);
        // the enforcement citation is still keyed by prefix, because which
        // *table of the signed lock* holds a citation genuinely is a prefix
        // question. The two are now independent, and this asserts both.
        let clause = |id: &str| {
            p.clauses
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("{id} missing"))
        };

        // Declared directly.
        assert_eq!(clause("Ri-0.2").binds, "enforcer");
        // Declared through its parent principle: P-15 binds the enforcer, so
        // P-15.1 does. Under the old derivation this read "agent" — a party
        // that I-14 forbids from touching an egress label at all.
        assert_eq!(clause("P-15.1").binds, "enforcer");

        // Not in the register: reports `undeclared` rather than a guess, and
        // still carries its enforcement citation. This is the deliberate
        // trade of #1284 part 1 — 109 of 207 clauses lose a prefix-derived
        // label, including 9 `Ri-*` whose label happened to be right, in
        // exchange for the guarantee that no reported direction is inferred.
        // Narrowing the fallback to `Ri-*` "because rights really do bind the
        // enforcer" would keep a derivation that silently mislabels the first
        // right which does not (the `Ri-0.15` seat-standing shape). Closing
        // the gap properly is #1284 part 2.
        let p11 = clause("P-1.1");
        assert_eq!(p11.binds, autonoetic_types::constitution::BINDS_UNDECLARED);
        assert!(p11.enforcement.as_deref().unwrap_or("").contains("tool_call_processor"));
        assert_eq!(
            clause("Ri-0.10").binds,
            autonoetic_types::constitution::BINDS_UNDECLARED
        );

        // The retired party names appear nowhere.
        for c in &p.clauses {
            assert!(
                c.binds != "agent" && c.binds != "gateway",
                "{} still reports a prefix-derived party name: {}",
                c.id,
                c.binds
            );
        }

        // include_text attaches the full markdown.
        let full = constitution_profile(true);
        assert!(full.text.as_deref().unwrap_or("").contains("| P-1.1 |"));
    }

    #[test]
    fn constitution_artifacts_resolve_from_same_root_when_parent_has_only_source() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        let rel_buf = autonoetic_types::config::default_constitution_source_path();
        let rel = rel_buf
            .parent()
            .expect("default constitution source path has a version-dir parent");
        let parent_docs = tmp.path().join(rel);
        std::fs::create_dir_all(&parent_docs).unwrap();
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("CARGO_MANIFEST_DIR parent");
        let canonical_md = workspace_root.join(rel).join("constitution.md");
        let canonical_lock = workspace_root
            .join(rel)
            .join("gateway-constitution.lock.json");
        std::fs::copy(&canonical_md, parent_docs.join("constitution.md")).unwrap();

        let mut cfg = GatewayConfig::default();
        cfg.agents_dir = agents_dir;
        cfg.runtime_dir = cfg.agents_dir.join(".gateway");
        let (source_path, lock_path) =
            resolve_constitution_artifact_paths(&cfg).expect("paired resolution");
        assert_eq!(
            source_path, canonical_md,
            "must not use constitution.md from parent without a lock beside it"
        );
        assert_eq!(lock_path, canonical_lock);
    }

    #[test]
    fn constitution_mixed_absolute_relative_paths_rejected() {
        let mut cfg = GatewayConfig::default();
        cfg.constitution.source_path = PathBuf::from("/nonexistent/absolute/constitution.md");
        cfg.constitution.lock_path =
            PathBuf::from("docs/constitution/versions/2026.05.05/gateway-constitution.lock.json");
        let err = initialize_constitution(&cfg).expect_err("mixed abs/rel should be rejected");
        assert!(
            err.to_string()
                .contains("both be absolute or both be relative"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn constitution_lock_has_version_metadata() {
        init_default_constitution();
        let lock = constitution_lock();
        assert!(lock.format_version >= 1);
        assert!(!lock.constitution_id.trim().is_empty());
        assert!(!lock.constitution_version.trim().is_empty());
        assert!(
            lock.signature.is_some(),
            "default constitution lock should be signed"
        );
        assert_eq!(
            lock.signature
                .as_ref()
                .expect("signature should exist")
                .signer_id,
            "autonoetic:constitution:v1"
        );
        assert_eq!(
            lock.constitution_source,
            autonoetic_types::config::default_constitution_source_path()
                .display()
                .to_string()
        );
    }

    #[test]
    fn current_file_matches_active_constitution_version() {
        // `docs/constitution/CURRENT` is the human-facing pointer to the active
        // version; `recompute_lock.py` rewrites it on every signing run. It must
        // match `ACTIVE_CONSTITUTION_VERSION` so the constant and the pointer
        // cannot silently drift.
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("CARGO_MANIFEST_DIR parent");
        let current =
            std::fs::read_to_string(workspace_root.join("docs/constitution/CURRENT"))
                .expect("docs/constitution/CURRENT should exist in the repo");
        assert_eq!(
            current.trim(),
            autonoetic_types::config::ACTIVE_CONSTITUTION_VERSION,
            "docs/constitution/CURRENT is out of sync with ACTIVE_CONSTITUTION_VERSION; \
             re-run `python3 docs/constitution/recompute_lock.py --version {}`",
            autonoetic_types::config::ACTIVE_CONSTITUTION_VERSION,
        );
    }
}
