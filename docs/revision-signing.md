# Revision Signing (P-9.13)

## Overview

Every agent revision is automatically signed by the gateway at creation time using the
gateway's persistent Ed25519 identity key. The signature provides **integrity attestation**
— proof that the gateway computed the canonical content digest and vouches for the
revision's contents — and enables tamper detection for auditors and federation.

This is **not** an access control mechanism. The capability gate (`AgentRevision`) already
controls who can create revisions. The signature answers a different question: _"Is this
revision exactly what the gateway computed, or has it been tampered with?"_

## What Gets Signed

The signature covers the **revision content digest** — a SHA-256 hash over the complete
`BTreeMap<String, Vec<u8>>` that materializes on disk:

```
for each file (in sorted path order):
    hash(path_bytes || 0x00 || file_content_bytes || 0x00)
```

This includes:

| Component | Source |
|-----------|--------|
| Artifact code files | From the artifact bundle |
| `SKILL.md` | Agent-authored (`agent_revision_create`) or gateway-rendered (`create_from_intent`) |
| `runtime.lock` | Always gateway-normalized/canonicalized |

Layers are **not** included directly — they're bound transitively through `runtime.lock`,
which lists each layer's `layer_id`, `digest`, and `mount_path`.

### One Digest, One Meaning

The `content_digest` field in the revision record always equals the full revision digest
(`sha256:{hex of complete file map hash}`). This is the same value used to derive the
`revision_id` and the same value that gets signed. There is no separate "artifact-only"
digest stored — promotion checks use this same digest, ensuring the promotion gate
attests to the complete runnable content.

## Auto-Sign Flow

```
create_revision_from_files()
    │
    ├── 1. Normalize runtime.lock, inject into file_map
    ├── 2. Validate script shebang (if script agent)
    ├── 3. Compute revision_digest_hex = SHA-256(file_map)
    ├── 4. revision_id = "rev_sha256:{digest_hex}"
    ├── 5. content_digest = "sha256:{digest_hex}"  (always the full revision)
    │
    ├── 6. Load gateway identity key (auto-generates on first use)
    │       ├── Success → sign(digest_hex) → store signature + signer_id
    │       └── Failure + trust_unsigned_bundles → proceed unsigned (dev mode)
    │       └── Failure + !trust_unsigned_bundles → error
    │
    └── 7. Persist revision record with signature + signer_id
```

## Key-Agnostic Verification

The revision record stores two fields:

- **`signature`**: Base64-encoded Ed25519 signature over the raw hex digest string.
- **`signer_id`**: Identifies which key produced the signature (e.g. `gateway:a1b2c3d4e5f67890`).

A verifier:

1. Reads `signer_id` to determine which public key to use.
2. Resolves the public key from a **trust store** (the gateway's `state_attestation.ed25519.pub`
   for local `gateway:` signers, a federation key store for `peer:` signers, etc.).
3. Verifies the Ed25519 signature over `revision_digest_hex`.

This design supports future external signers (federation peers, CI pipelines, operator
consoles) without changing the verification path — only the trust store resolution
changes.

### Signer ID Format

| Prefix | Meaning | Public key source |
|--------|---------|-------------------|
| `gateway:{fingerprint}` | Auto-signed by the local gateway | `state_attestation.ed25519.pub` |
| `peer:{node_id}` | Signed by a federated gateway (future) | Federation trust store |
| `ci:{pipeline_id}` | Signed by a CI attestation pipeline (future) | CI public key registry |

## Gateway Identity Key

The signing key is the same `GatewayIdentityKey` used for turn-boundary state attestations
(P-6.23):

- **Private**: `.gateway/state_attestation.ed25519` (mode `0o600`, auto-generated on first load)
- **Public**: `.gateway/state_attestation.ed25519.pub` (32 bytes, Ed25519 verifying key)

## Configuration

### `trust_unsigned_bundles` (default: `false`)

When `false` (default), revision creation fails if the gateway identity key cannot be
loaded. This should never happen in normal operation (the key auto-generates on first
access), but protects against environments where the private key file has wrong
permissions or the filesystem is read-only.

When `true`, revision creation proceeds without a signature if the key is unavailable.
This is intended only for local development or constrained environments.

### Why This Replaced the Original Caller-Supplied Gate

The original design (before P-9.13 took its current gateway-side form) required
the **caller** (agent/CLI) to provide an Ed25519 signature in the tool
arguments. This was a deadlock:

1. The agent doesn't have the gateway's private key, so it can't produce a valid signature.
2. The gateway doesn't auto-sign, so there's no way to get a signature.
3. The only escape was `trust_unsigned_bundles: true`, which completely bypassed signing.

The new design recognizes that the signature's purpose is **gateway attestation** ("this
is what I computed"), not **caller authentication** (the capability gate already handles
that). Auto-signing inside `create_revision_from_files` — after the gateway has
canonicalized the content — is the correct place for this operation.

## SQLite Schema

Migration v24 adds two nullable columns to `agent_revisions`:

```sql
ALTER TABLE agent_revisions ADD COLUMN signature TEXT;
ALTER TABLE agent_revisions ADD COLUMN signer_id TEXT;
```

Existing revisions (created before this migration) have `NULL` for both fields.
Revisions created without a gateway key (`trust_unsigned_bundles: true`) also have
`NULL` for both fields.
