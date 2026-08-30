# RATIFY.md — Constitution Version 2026.08.30

## Summary

Text-then-law realignment across five classes of drift, all verified
against the enforced code on `main`. The version started as amendment
**#1078** (Ri-0.12 yield-cause enumeration) and was broadened before
signing so the whole accumulated drift is corrected in one ceremony:
while the lock does not exist, the text is still draft; after signing,
every fix below would require its own amendment.

Baseline: **2026.07.30**. No code changes, no new rule IDs, no status
upgrades beyond the corrections listed below, no register entry changes
(the enforcement register is already green on `main`).

## Class 1 — Ri-0.12 yield-cause enumeration (#1078, original scope)

The clause claimed `YieldReason` has 11 causes (6 terminal + 5
resumable); the code has 12 (5 terminal + 7 resumable). `Idle` (#902,
resident sessions) was added three days before the then-current version
was signed and never reached the clause, and `ManualStop` stopped being
terminal when it became the cooperative operator pause (#1026/#1051).
Also describes `ManualStop`'s producer (`root_session.pause`), the
pre-LLM checkpoint yield point, and in-place resume.

## Class 2 — Substantive clause updates (merged-PR behavioral drift)

- **P-2.4** — session grants are agent-scoped within the root session
  (`grant.agent_id` must match the requesting agent); root-wide coverage
  only via the `*` sentinel minted by plan-envelope locks (#1063).
- **P-2.6** — exec-cache skips no longer last "for the gateway
  lifetime"; entries expire after `default_grant_ttl_secs` (24h
  default, `0` disables) and then re-prompt (#1063).
- **P-2.16** — the "(pre-auth check — pending #503)" caveat dropped;
  envelope-lock pre-authorization shipped (#503).
- **P-2.24(c)** — the structural-similarity approval dedup store was
  dropped (migration `apply_drop_approval_similarity_v55`); what remains
  is an inline Jaccard advisory on WikiProposal gates only.
- **P-3.2** — `--share-net` for `sandbox_exec` follows the per-exec
  operator network grant, not the declared capability (#1030);
  capability-derived overrides remain only for script-mode/
  `artifact_exec`.
- **P-3.7** — quota enforcement is externalized to operator-declared
  driver profiles (fail-shut on missing declarations); only the wasm
  driver has a built-in limiter. Status corrected ENFORCED → PARTIAL.
- **P-5.2** — the LLM-coercion fallback no longer exists
  (`SchemaEnforcementMode` is `Disabled | Deterministic`; `mode: llm`
  rejected at parse). Phase 4.2 leak closed; status and §14 updated
  (the "two standing leaks" sentence is now one).
- **P-5.8** — named knobs `max_validation_loops` /
  `max_validation_duration_ms` never existed; actual controls are
  `response_validation.repair_enabled` (default false, manifest
  opt-in) and `max_repair_attempts_ceiling` (default 2). Enforcement
  citation moved to `runtime/response_validation.rs`.
- **P-7.5** — `Validation` errors added to the non-counting list; doc
  pointer corrected.
- **P-7.19** — third no-progress trip condition added
  (`RedundantAnnotationLoop`, `loop_guard.annotation_repeat_floor`,
  #1093); the text previously described only two.
- **P-9.6** — `rejected` is no longer a revision status
  (`Candidate | Ready | Archived`); rejection lives in the
  promotion-attempt ledger (#1159).

## Class 3 — Staled closed enumerations

- Amendment process: entrenched correction core is six clauses, not
  five — `P-8.1` (tamper-evident causal chain) is entrenched in
  `enforcement_register.rs::entrenched_clauses()` and
  `docs/concepts/philosophy.md` §3.1; the text omitted it.
- **I-11**: `refuse-turn` added to the fail-mode list.
- **Ri-0.15**: `GateKind` gloss now names all four kinds
  (`wiki_proposal` was missing).
- **Ri-0.16**: `Blocked` added to the trajectory classification list.
- **P-4.2**: nonce is per persist (whole-vault blob), not per entry.
- **§0 preamble**: section rundown now includes §12 and §15.
- **§3 preamble**: driver list now includes `wasm` (#1126);
  **P-7.22** driver-equivalents likewise.

## Class 4 — Newly enforced mechanisms, now named

- **§11 / P-11.5** — receiver-side messaging consent
  (`PolicyEngine::accepts_peer_message_from` against the receiver's
  `metadata.autonoetic.messaging.accepts_from`; bare pattern = exact
  match) gates delivery alongside the sender-side ACL (#1220). Against
  the preamble's "not here ⇒ not enforced", an enforced mechanism with
  no clause is drift; it is now named.
- **Ri-0.6** — `session.handoff` (#1091) is a second operator-action
  path for mid-session capability change (rebinding the live root
  session to a different agent, recorded as a `session.handoff` event);
  the closed (a)/(b) enumeration now includes it.
- **P-8.19 / O-1 / O-2** — decider identity formats corrected: nothing
  emits `"policy:<engine_id>"` (reserved, no emitter), and
  `"operator:<username>"` is recorded but not yet classified to `Human`
  by `decider_principal_kind` — named as a classification gap rather
  than papered over.

## Class 5 — Citation sweep (~45 broken citations)

The #953 domain-binary collapse (2026-07-29, one day before 2026.07.30
was signed) renamed ~40 cited test files with no same-name successor;
#1173 moved six cited docs; several cited code symbols moved or were
renamed. The signed text was born stale in this class. All citations
now resolve on `main`:

- flat `constitution_*.rs` test names →
  `autonoetic-gateway/tests/constitution/<name>.rs` (domain binary);
- `constitution_r_2_11_approval_timeout.rs` (deleted, #564) →
  `tests/workflow/approval_spawn_gate.rs` +
  `scheduler.rs::check_approval_timeouts`;
- moved integration tests (`trajectory_monitor`,
  `gate_messages_jsonrpc`, `ofp`, `root_budget_circuit_breaker`) →
  their domain-binary paths;
- moved/renamed code targets (`append_bwrap_isolation_flags`,
  `detect_sandbox_escape_indicators`,
  `detect_network_errors_in_output`, `build_degradation_notice_tail`,
  `check_ri_0_6_turn_snapshot`, `normalize_targets`,
  `validate_and_maybe_repair`, `causal_events` mirror);
- `gateway_store/…` citations prefixed with `scheduler/`;
- moved docs (`config-reference.md`, `philosophy.md`,
  `principal-model-…`, `data-envelopes-…`, the 2026-04-24 audit,
  `gateway-constitution-roadmap.md`) → their post-#1173 paths, as the
  mapping table in `docs/constitution/README.md` requests;
- drifted line-number citations dropped (path + symbol kept); the
  amendment process now points at the `tests/constitution/` domain
  binary instead of the abolished `constitution_<category>_<rule-id>`
  naming convention.

Note: no mechanical guard parses constitution-text citations —
`docs_link_guard` deliberately skips `versions/**` (digest-signed
bytes). This is why class 5 accumulated silently; a text-citation guard
is a worthwhile follow-up issue but cannot be a code change inside this
text-only amendment.

## Why no code change

Every correction above aligns text with already-enforced code; the
enforced categories, gates, and citations were verified against `main`
clause by clause. The exhaustive `ri_0_12_category` match in
`autonoetic-gateway/tests/constitution/rights_mid_bucket.rs` (added
with #998-style companion fix) fails to *compile* when a variant is
added, so `Idle` could not be silently neglected again.

## Signing runbook (operator, requires signing key)

This is the only step that needs `AUTONOETIC_CONSTITUTION_SIGNING_SK_B64`;
it is NOT available on CI or on machines without the vault copy.

```bash
# 1. Recompute the lock for the new version: writes
#    docs/constitution/versions/2026.08.30/gateway-constitution.lock.json
#    AND rewrites docs/constitution/CURRENT (the pointer).
python3 docs/constitution/recompute_lock.py --version 2026.08.30 \
  --signing-sk-b64 "$AUTONOETIC_CONSTITUTION_SIGNING_SK_B64"

# 2. Activate: bump the ratified version constant. The lock now exists, so
#    the digest + counts tests pass with the ACTIVE pointer moved.
#    Edit autonoetic-types/src/config.rs:
#      ACTIVE_CONSTITUTION_VERSION = "2026.08.30"
#    and update the 2026.07.30 → 2026.08.30 paths in docs/reference/config.md.

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
  running 2026.08.30.
