//! Shared ID formatting helpers for stable prefixed identifiers.

use sha2::{Digest, Sha256};

/// Fixed hex suffix length reserved by the MVP spec for suite/eval/promotion IDs.
pub const STABLE_ID_HEX_LEN: usize = 12;

/// Mint a stable prefixed identifier by hashing caller-provided entropy.
///
/// Example: `mint_hashed_prefixed_id("prom-", "...")` -> `prom-1a2b3c4d5e6f`
pub fn mint_hashed_prefixed_id(prefix: &str, entropy: &str) -> String {
    let digest_hex = hex::encode(Sha256::digest(entropy.as_bytes()));
    format!("{}{}", prefix, &digest_hex[..STABLE_ID_HEX_LEN])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mint_hashed_prefixed_id_shape() {
        let id = mint_hashed_prefixed_id("prom-", "agent-a@rev-1@time");
        assert!(id.starts_with("prom-"));
        assert_eq!(id.len(), "prom-".len() + STABLE_ID_HEX_LEN);
        assert!(id["prom-".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
