#!/usr/bin/env python3
"""weathermerge — merge station CSV data into report.md.

Reads the sealed manifest at build/manifest.json, verifies every listed
data file against its recorded digest, then merges all rows into a single
markdown report.

The manifest is a build artifact: it does not exist in a fresh checkout.
"""

import hashlib
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
MANIFEST = ROOT / "build" / "manifest.json"
REPORT = ROOT / "report.md"


def fail(msg: str, code: int) -> "None":
    print(f"weathermerge: error: {msg}", file=sys.stderr)
    sys.exit(code)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    h.update(path.read_bytes())
    return h.hexdigest()


def main() -> None:
    if not MANIFEST.is_file():
        # Deliberately opaque: the fix is a build step, not a code change.
        fail("EMANIFEST (resource sealed) — aborting", 3)

    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    entries = manifest.get("files", [])
    if not entries:
        fail("EMANIFEST (manifest seals zero data files) — aborting", 3)

    rows = []
    stations = set()
    for entry in entries:
        data_file = ROOT / entry["path"]
        if not data_file.is_file():
            fail(f"ESEAL (sealed file missing: {entry['path']})", 4)
        if sha256(data_file) != entry["sha256"]:
            fail(f"ESEAL (digest mismatch: {entry['path']})", 4)
        for line in data_file.read_text(encoding="utf-8").splitlines()[1:]:
            if not line.strip():
                continue
            station, date, tmin, tmax, precip = line.split(",")
            stations.add(station)
            rows.append((station, date, float(tmin), float(tmax), float(precip)))

    rows.sort(key=lambda r: (r[0], r[1]))

    lines = [
        "# weathermerge report",
        "",
        f"stations: {len(stations)}  rows: {len(rows)}",
        "",
        "| station | date | tmin | tmax | precip |",
        "|---|---|---|---|---|",
    ]
    for station, date, tmin, tmax, precip in rows:
        lines.append(f"| {station} | {date} | {tmin:.1f} | {tmax:.1f} | {precip:.1f} |")
    REPORT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"OK: report.md written ({len(rows)} rows, {len(stations)} stations)")


if __name__ == "__main__":
    main()
