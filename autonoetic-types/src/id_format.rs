//! Shared ID formatting helpers.
//!
//! Two families of identifier construction live here so the truncation lengths,
//! hashing, and encoding are defined once:
//!
//! - **Deterministic hashed IDs** — [`hash_and_truncate`] is the core (SHA-256 →
//!   lowercase hex → truncate). [`mint_hashed_prefixed_id`] is the common
//!   `prefix + hash(entropy)` shape; callers that pin a different suffix length
//!   or prefix (memory dedup keys, capsule IDs, …) call [`hash_and_truncate`]
//!   directly from their own thin, format-documented wrappers. These outputs are
//!   **persisted / cross-referenced**, so each wrapper must keep its exact
//!   length + prefix — only the boilerplate is shared here.
//! - **Random short IDs** — [`short_random_id`] mints an ephemeral
//!   `prefix + <n hex of a fresh UUID>` identifier for events, messages,
//!   notifications, etc. These are never matched by format, so they carry no
//!   compatibility constraint.

use sha2::{Digest, Sha256};

/// Fixed hex suffix length reserved by the MVP spec for suite/eval/promotion IDs.
pub const STABLE_ID_HEX_LEN: usize = 12;

/// SHA-256 the input and return the first `hex_len` lowercase hex characters.
///
/// The shared core of every deterministic hashed ID. `hex_len` is clamped to
/// the 64-char full digest. Callers own their prefix and length; this only
/// centralizes the hash + hex-encode + truncate so those never drift apart.
///
/// Note: hashing a single concatenated string is identical to feeding SHA-256
/// the same bytes in parts, so wrappers that previously called `update()`
/// multiple times can pass `format!("{a}{sep}{b}")` and get the same digest.
pub fn hash_and_truncate(input: &str, hex_len: usize) -> String {
    let digest_hex = hex::encode(Sha256::digest(input.as_bytes()));
    let len = hex_len.min(digest_hex.len());
    digest_hex[..len].to_string()
}

/// Mint a stable prefixed identifier by hashing caller-provided entropy.
///
/// Example: `mint_hashed_prefixed_id("prom-", "...")` -> `prom-1a2b3c4d5e6f`
pub fn mint_hashed_prefixed_id(prefix: &str, entropy: &str) -> String {
    format!("{}{}", prefix, hash_and_truncate(entropy, STABLE_ID_HEX_LEN))
}

/// Mint an ephemeral random identifier: `prefix` followed by `hex_len` hex
/// characters from a fresh v4 UUID (dash-free `simple` form). `hex_len` is
/// clamped to the 32-char UUID hex width.
///
/// Prefer [`short_random_id`] (8 hex) for the common event/message/notification
/// case; use this directly only when a wider random suffix is needed.
pub fn short_random_id_hex(prefix: &str, hex_len: usize) -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    let len = hex_len.min(hex.len());
    format!("{}{}", prefix, &hex[..len])
}

/// Mint an ephemeral random identifier: `prefix` followed by 8 hex characters
/// from a fresh v4 UUID (e.g. `short_random_id("msg-")` -> `msg-1a2b3c4d`).
///
/// Pass `""` for a bare 8-hex-char id. This replaces the previously-duplicated
/// `format!("{prefix}-{}", &Uuid::new_v4().to_string()[..8])` idiom, and
/// encapsulates the fact that the slice is only valid because UUID hex is ASCII.
pub fn short_random_id(prefix: &str) -> String {
    short_random_id_hex(prefix, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_and_truncate_len_and_determinism() {
        let a = hash_and_truncate("agent-a@rev-1@time", 12);
        let b = hash_and_truncate("agent-a@rev-1@time", 12);
        assert_eq!(a, b, "same input → same hash");
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Prefixes of a longer truncation match a shorter one (streaming hash).
        assert!(hash_and_truncate("x", 24).starts_with(&hash_and_truncate("x", 12)));
        // Over-long request clamps to the full 64-char digest.
        assert_eq!(hash_and_truncate("x", 999).len(), 64);
    }

    #[test]
    fn test_mint_hashed_prefixed_id_shape() {
        let id = mint_hashed_prefixed_id("prom-", "agent-a@rev-1@time");
        assert!(id.starts_with("prom-"));
        assert_eq!(id.len(), "prom-".len() + STABLE_ID_HEX_LEN);
        assert!(id["prom-".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn test_short_random_id_shape_and_uniqueness() {
        let a = short_random_id("msg-");
        assert!(a.starts_with("msg-"));
        assert_eq!(a.len(), "msg-".len() + 8);
        assert!(a["msg-".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Fresh randomness each call.
        assert_ne!(short_random_id("msg-"), short_random_id("msg-"));
        // Empty prefix → bare 8-hex id.
        assert_eq!(short_random_id("").len(), 8);
    }

    #[test]
    fn test_short_random_id_hex_len() {
        assert_eq!(short_random_id_hex("wf-", 12).len(), "wf-".len() + 12);
        assert_eq!(short_random_id_hex("", 999).len(), 32);
    }
}
