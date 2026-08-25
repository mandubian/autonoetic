# RATIFY.md — Constitution Version 2026.08.25

## Summary

Constitutional amendment **#1078** — realign the Ri-0.12 yield-cause
enumeration with the enforced code. The clause claimed the enum has 11
causes (6 terminal + 5 resumable); the code has 12 (5 terminal + 7
resumable), because `Idle` (#902, resident sessions) was added three days
before the then-current version was signed and never reached the clause,
and because `ManualStop` stopped being terminal when it became the
cooperative operator pause (#1026/#1051).

Baseline: **2026.07.30**. Single clause-level change — Ri-0.12 text
realignment only; no existing rule's substance changes, no new rule, no
status change, no register entry change.

## The paragraph, revised (3 corrections)

1. **Count**: `11 session yield causes (6 terminal + 5 resumable)` →
   `12 session yield causes (5 terminal + 7 resumable)`.
2. **`ManualStop` is now described as the cooperative operator pause**
   (only producer: `root_session.pause` / the room pause affordance; yields
   at the pre-LLM checkpoint so the in-flight tool batch completes; resumes
   in place on the operator's next message, no fork) — the old text never
   said what "manual stop" behaved like, and it read terminal.
3. **`Idle` joins the auto-resume set** listed under
   `should_auto_resume_checkpoint_yield_reason` — a parked resident session
   exists precisely to be resumed by the inbound message it is parked for;
   excluding it would strand every delivery.

Nothing else about the clause changes; the closed-list preamble, the
terminal/resumable split doctrine, the (a)–(f) termination-reason list
(a cooperative pause is not a termination, so it needs no new entry), and
the enforcement citation all stay verbatim.

## Why no code change

The code caught up with itself while the clause slept: the exhaustive
`ri_0_12_category` match in
`autonoetic-gateway/tests/constitution/rights_mid_bucket.rs` (added with
#998-style companion fix) fails to *compile* when a variant is added, so
`Idle` could not be silently neglected again, and the v2026.07.30-era
hand-written 11-length assertion was replaced by that same match. The
enforced categories (5 terminal / 7 resumable) already match the revised
text; this version is text-then-law alignment.

## Signing runbook (operator, requires signing key)

This is the only step that needs `AUTONOETIC_CONSTITUTION_SIGNING_SK_B64`;
it is NOT available on CI or on machines without the vault copy.

```bash
# 1. Recompute the lock for the new version: writes
#    docs/constitution/versions/2026.08.25/gateway-constitution.lock.json
#    AND rewrites docs/constitution/CURRENT (the pointer).
python3 docs/constitution/recompute_lock.py --version 2026.08.25 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"

# 2. Activate: bump the ratified version constant. The lock now exists, so
#    the digest + counts tests pass with the ACTIVE pointer moved.
#    Edit autonoetic-types/src/config.rs:
#      ACTIVE_CONSTITUTION_VERSION = "2026.08.25"
#    and update the 2026.07.30 → 2026.08.25 paths in docs/reference/config.md.

# 3. Verify — the two gates that pin this:
cargo test -p autonoetic-gateway constitution_lock_matches_canonical_digest_and_counts
cargo test -p autonoetic-gateway --test constitution_r_8_6_retention_policy_startup
```

Step 3's second test exercises the startup constitution-snapshot path
against the ACTIVE version; both must pass before the activation commit
lands.

## Details for the record

- The `constitution 2026.07.30` citation in
  `docs/diagrams/runtime-dynamics.html` § references remains correct
  until activation; the §-A "known discrepancy" note is reworded in this
  version's doc commit (a realignment is in flight), and dropped entirely
  by the activation commit.
- Federation ripple: peers in `Exact` compatibility mode must add the new
  digest to `known_compatible_digests` before federating with a gateway
  running 2026.08.25.
