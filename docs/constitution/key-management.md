# Constitution Signing Key Management

This document describes the recommended way to manage the constitution
signing key so lock recomputation stays deterministic across machines.

## Goal

For Ed25519 signatures, this property must hold:

- same private key + same canonical payload bytes = same signature bytes.

To preserve that property across machines:

1. generate signing key material once,
2. store the private seed securely,
3. distribute only the public key by default,
4. keep payload canonicalization stable.

## Canonical Inputs

Lock recomputation is done by:

- `docs/constitution/recompute_lock.py`

The script already matches gateway canonicalization:

- sorted JSON keys,
- compact separators,
- UTF-8 (`ensure_ascii=False`).

## Recommended Operating Model

Use **one signing authority** (preferred) and many verifiers:

- one trusted machine or release CI signs lock updates,
- other machines verify signatures and digest/count consistency.

This reduces key exposure and makes release signatures reproducible.

## Generate Key Once

Generate once when intentionally establishing or rotating signer material:

```bash
python3 docs/constitution/recompute_lock.py --version 2026.05.30 --generate-key
```

Record outputs immediately:

- `generated signing seed (base64, 32 bytes)` -> private secret
- `trusted_signers public key (base64)` -> public value to commit

## Store and Exchange

### Private seed (`AUTONOETIC_CONSTITUTION_SIGNING_SK_B64`)

Store in a secrets manager (for example: Vault, 1Password, Bitwarden,
SOPS-encrypted file, cloud secret manager). Do not commit to git.

Only share via secure secret channels. Never paste in issue comments,
chat logs, shell history snippets, or committed files.

### Public key

Public key is safe to distribute and should be committed in:

- `autonoetic-types/src/config.rs`
- `config/config-template.yaml`
- `docs/config-reference.md`

under signer id `autonoetic:constitution:v1` (or your chosen signer id).

## Recompute Same Signature on Another Machine

1. Check out identical constitution files (no local edits).
2. Set the same signing seed:

```bash
export AUTONOETIC_CONSTITUTION_SIGNING_SK_B64="<base64_seed>"
```

3. Run:

```bash
python3 docs/constitution/recompute_lock.py --version 2026.05.30
```

Do not pass `--generate-key` unless you are rotating.

## Rotation Procedure

When rotating signer material:

1. Run recompute with `--generate-key`.
2. Update trusted signer public key in:
   - `autonoetic-types/src/config.rs`
   - `config/config-template.yaml`
   - `docs/config-reference.md`
3. Commit lock + public key updates together.
4. Distribute new private seed to authorized signing environments only.

## CI Guidance

### Verify-only CI (recommended default)

CI does not need private key. It should:

- recompute digest/count expectations,
- verify lock signature against configured `constitution.trusted_signers`,
- fail on mismatch.

### Signing CI (only when desired)

If CI must sign, inject secret as:

- `AUTONOETIC_CONSTITUTION_SIGNING_SK_B64`

from protected CI secrets/environment with reviewer controls.

## Line Endings and Determinism

Ensure constitution files use stable line endings (`LF`) across machines.
If needed, enforce via `.gitattributes` to avoid accidental payload drift.
