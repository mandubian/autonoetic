#!/usr/bin/env python3
"""Recompute constitution lock digest and signature.

Canonicalization must match autonoetic-gateway/src/constitution_digest.rs:
- digest payload serialized as compact JSON
- sorted map keys
- UTF-8 bytes (ensure_ascii=False)
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
from typing import Dict

from nacl.signing import SigningKey


def extract_enforcement_table(markdown: str, prefix: str) -> Dict[str, str]:
    out: Dict[str, str] = {}
    for line in markdown.splitlines():
        if not line.lstrip().startswith("|"):
            continue
        cells = [cell.strip() for cell in line.split("|") if cell.strip()]
        if len(cells) < 4:
            continue
        rule_id = cells[0]
        if not rule_id.startswith(prefix):
            continue
        if rule_id == "ID" or rule_id.startswith("---"):
            continue
        enforcement = cells[3].strip()
        if enforcement:
            out[rule_id] = enforcement
    return dict(sorted(out.items()))


def compact_json_bytes(payload: dict) -> bytes:
    return json.dumps(
        payload,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")


def parse_args() -> argparse.Namespace:
    repo_default = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(
        description="Recompute constitution lock digest and Ed25519 signature."
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=repo_default,
        help="Repository root (default: inferred from script location).",
    )
    parser.add_argument(
        "--version",
        default="2026.05.30",
        help="Constitution version directory under docs/constitution/versions/.",
    )
    parser.add_argument(
        "--signer-id",
        default="autonoetic:constitution:v1",
        help="Signer ID written to lock.signature.signer_id.",
    )
    parser.add_argument(
        "--signing-sk-b64",
        default=os.environ.get("AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"),
        help="Base64 Ed25519 private key (64 bytes seed+public). If omitted, use env AUTONOETIC_CONSTITUTION_SIGNING_SK_B64.",
    )
    parser.add_argument(
        "--generate-key",
        action="store_true",
        help="Generate a new Ed25519 keypair for this signing run.",
    )
    return parser.parse_args()


def load_signing_key(args: argparse.Namespace) -> tuple[SigningKey, bool]:
    if args.signing_sk_b64:
        raw = base64.b64decode(args.signing_sk_b64)
        if len(raw) == 32:
            return SigningKey(raw), False
        if len(raw) == 64:
            return SigningKey(raw[:32]), False
        raise SystemExit(
            "Invalid signing key length: expected 32-byte seed or 64-byte keypair (base64)."
        )

    if args.generate_key:
        return SigningKey.generate(), True

    raise SystemExit(
        "Missing signing key. Set AUTONOETIC_CONSTITUTION_SIGNING_SK_B64 or pass --signing-sk-b64. "
        "If rotating signer material intentionally, pass --generate-key."
    )


def find_template_lock(versions_root: Path, current_version: str) -> Path | None:
    """Newest sibling version's lock, used to seed a brand-new version's lock.

    A freshly-prepared version dir ships `constitution.md` only (the operator
    signs the lock). The fields this script does NOT recompute — `format_version`,
    `constitution_id`, `canonicalization` — are stable across versions, so they
    are inherited from the most recent prior version that has a lock. Version dirs
    are date-named (`YYYY.MM.DD`), so lexical sort == chronological.
    """
    if not versions_root.is_dir():
        return None
    siblings = sorted(
        (d for d in versions_root.iterdir() if d.is_dir() and d.name != current_version),
        key=lambda d: d.name,
        reverse=True,
    )
    for d in siblings:
        candidate = d / "gateway-constitution.lock.json"
        if candidate.exists():
            return candidate
    return None


def main() -> None:
    args = parse_args()
    repo_root = args.repo_root.resolve()
    version_dir = repo_root / "docs" / "constitution" / "versions" / args.version
    constitution_path = version_dir / "constitution.md"
    lock_path = version_dir / "gateway-constitution.lock.json"

    if not constitution_path.exists():
        raise SystemExit(f"Constitution markdown not found: {constitution_path}")

    # A freshly-prepared version dir may ship constitution.md only. Seed the lock
    # structure from the most recent prior version (stable fields), then recompute.
    if lock_path.exists():
        lock = json.loads(lock_path.read_text())
    else:
        template = find_template_lock(version_dir.parent, args.version)
        if template is None:
            raise SystemExit(
                f"Constitution lock not found and no prior version lock to seed from: {lock_path}"
            )
        print(f"No lock at {lock_path}; seeding structure from {template}")
        lock = json.loads(template.read_text())
        lock.pop("signature", None)  # inherited signature is invalid here; this run re-signs

    text = constitution_path.read_text()
    rights = extract_enforcement_table(text, "Ri-")
    rules = extract_enforcement_table(text, "P-")

    digest_payload = {
        "constitution_text": text,
        "rights_enforcement": rights,
        "rules_enforcement": rules,
    }
    constitution_digest = hashlib.sha256(compact_json_bytes(digest_payload)).hexdigest()

    lock["constitution_version"] = args.version
    lock["constitution_source"] = f"docs/constitution/versions/{args.version}/constitution.md"
    lock["constitution_digest"] = constitution_digest
    lock["rule_enforcement_count"] = len(rules)
    lock["right_enforcement_count"] = len(rights)

    signature_payload = {
        "format_version": lock["format_version"],
        "constitution_id": lock["constitution_id"],
        "constitution_version": lock["constitution_version"],
        "constitution_source": lock["constitution_source"],
        "constitution_digest": lock["constitution_digest"],
        "rule_enforcement_count": lock["rule_enforcement_count"],
        "right_enforcement_count": lock["right_enforcement_count"],
        "canonicalization": lock["canonicalization"],
    }
    signature_payload_bytes = compact_json_bytes(signature_payload)

    signing_key, generated_key = load_signing_key(args)
    verify_key = signing_key.verify_key
    public_key_b64 = base64.b64encode(verify_key.encode()).decode()
    signature_b64 = base64.b64encode(
        signing_key.sign(signature_payload_bytes).signature
    ).decode()

    lock["signature"] = {
        "algorithm": "ed25519",
        "signer_id": args.signer_id,
        "signature_b64": signature_b64,
    }

    lock_path.write_text(json.dumps(lock, indent=2, ensure_ascii=False) + "\n")

    # Keep the human-facing `CURRENT` pointer in lockstep with the signed
    # version. `ACTIVE_CONSTITUTION_VERSION` in autonoetic-types/src/config.rs
    # is the other half of this contract; the
    # `current_file_matches_active_constitution_version` test enforces they match.
    current_path = repo_root / "docs" / "constitution" / "CURRENT"
    current_path.write_text(f"{args.version}\n")

    print(f"Updated: {lock_path}")
    print(f"Updated: {current_path}")
    print(f"constitution_digest: {constitution_digest}")
    print(f"rule_enforcement_count: {len(rules)}")
    print(f"right_enforcement_count: {len(rights)}")
    print(f"trusted_signers public key (base64): {public_key_b64}")
    print(f"signature_b64: {signature_b64}")

    if generated_key:
        seed_b64 = base64.b64encode(signing_key.encode()).decode()
        print("generated signing seed (base64, 32 bytes):", seed_b64)
        print(
            "NOTE: update trusted_signers for the signer_id above in "
            "autonoetic-types/src/config.rs, config/config-template.yaml, and docs/reference/config.md."
        )


if __name__ == "__main__":
    main()
