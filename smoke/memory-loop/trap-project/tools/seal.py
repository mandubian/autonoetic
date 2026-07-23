#!/usr/bin/env python3
"""Seal the data/ directory into build/manifest.json.

This is the code-generation / sealing build step: it hashes every CSV under
data/ and writes the manifest that main.py requires at runtime. Run it
whenever files under data/ change (or just run `make`, whose default target
chains seal -> report).
"""

import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DATA = ROOT / "data"
BUILD = ROOT / "build"


def main() -> None:
    files = []
    for csv in sorted(DATA.glob("*.csv")):
        digest = hashlib.sha256(csv.read_bytes()).hexdigest()
        files.append({"path": str(csv.relative_to(ROOT)), "sha256": digest})
    if not files:
        raise SystemExit("seal: no CSV files found under data/")
    BUILD.mkdir(exist_ok=True)
    manifest = BUILD / "manifest.json"
    manifest.write_text(json.dumps({"files": files}, indent=2) + "\n", encoding="utf-8")
    print(f"sealed {len(files)} data files into {manifest.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
