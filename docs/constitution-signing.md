# Constitution Lock Signing (v1)

This document specifies the v1 signature model for constitution lock files.

## Scope

The signature applies to:

- release lock files in `docs/constitution/versions/<version>/gateway-constitution.lock.json`,
- bootstrapped runtime lock files in
  `<agents_dir>/.gateway/constitution/versions/<version>/gateway-constitution.lock.json`.

It does **not** sign `constitution.md` directly. The lock signs the digest and
canonicalization metadata that already bind `constitution.md` content.

## Lock Schema (v1)

`gateway-constitution.lock.json` includes:

- `format_version`
- `constitution_id`
- `constitution_version`
- `constitution_source`
- `constitution_digest`
- `rule_enforcement_count`
- `right_enforcement_count`
- `canonicalization`
- optional `signature`

`signature` object:

- `algorithm`: currently `ed25519`
- `signer_id`: signer identifier
- `signature_b64`: base64 Ed25519 signature bytes

## Signed Payload (exact fields)

The signature covers this JSON object only (no `signature` field inside):

- `format_version`
- `constitution_id`
- `constitution_version`
- `constitution_source`
- `constitution_digest`
- `rule_enforcement_count`
- `right_enforcement_count`
- `canonicalization`

The gateway verifies signatures by rebuilding this object and serializing it
to compact UTF-8 JSON bytes (no pretty-print whitespace), then verifying the
Ed25519 signature over those bytes.

## Verification Rules

Startup integrity checks always verify:

- `constitution_source` matches configured `constitution.source_path`,
- `constitution_digest` matches recomputed digest,
- rule/right counts match recomputed counts,
- canonicalization fields are expected constants:
  - `algorithm = "sha256"`
  - `payload = "json({constitution_text,rights_enforcement,rules_enforcement})"`
  - `rules_prefix = "R-"`
  - `rights_prefix = "Ri-"`.

Signature checks:

- If `constitution.require_signature = true` (default):
  - missing `signature` fails startup (fail-shut),
  - invalid signature fails startup.
- If `constitution.require_signature = false`:
  - unsigned locks are allowed,
  - signed locks are still verified when present.

## Signer Resolution

`signer_id` resolution is mechanical:

1. `gateway:<fingerprint>`
   - public key source:
     `<agents_dir>/.gateway/state_attestation.ed25519.pub`
   - fingerprint check:
     `<fingerprint>` must equal hex(first 8 bytes of public key)
2. any other signer ID
   - resolved via `constitution.trusted_signers[signer_id]`
   - value must be base64 of exactly 32 Ed25519 public key bytes

If signer resolution fails, startup fails.

## Repo vs Runtime `constitution_source`

The canonical repo lock uses a repo-relative source path, for example:

- `docs/constitution/versions/2026.05.30/constitution.md`

During bootstrap (`agent bootstrap` and gateway startup), the runtime lock is
materialized under `.gateway` and rewritten to:

- `.gateway/constitution/versions/<version>/constitution.md`

So repo and runtime locks intentionally differ in `constitution_source`, but
each remains self-consistent and verifiable in its own context.

## Bootstrap Re-signing

When the gateway bootstraps constitution artifacts to `.gateway`:

- it copies `constitution.md`,
- rewrites lock `constitution_source` to `.gateway/...`,
- re-signs that rewritten lock with the local gateway identity key,
- writes `ACTIVE.json` with `lock_signer_id`.

This guarantees the runtime-local lock is signed by the exact gateway identity
running that node.
