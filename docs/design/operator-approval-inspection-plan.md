# Operator Approval Inspection — Design Plan

**Status:** Draft RFC. Not yet implemented. Splits cleanly into two
phases that can ship independently.

**Refs:**
- Tracking issue: **#186**.
- Constitution: `P-2.1`, `P-2.18`, `P-2.19`, `P-2.24`, `Ri-0.1`,
  `Ri-0.5`.
- `docs/design/human-gate-unification-plan.md` (gate enrichment
  threads, ask-agent clarification child sessions).
- `docs/archived/sealed-network-evaluation-plan.md` (the auditor's
  Shape-2 review covers the same code at install-time; this RFC
  covers the same code at approval-time).

---

## 1. Problem statement

When `sandbox_exec`, `artifact_exec`, or `agent_install` triggers an
operator approval today, the approval card shows:

- The command string being run (often a one-liner).
- Detected remote-access patterns from `RemoteAccessAnalyzer` (URLs,
  imports, function calls).
- Host list.
- Optional reason string.

It does **not** show:

- The actual source code of the executable files in the artifact.
- The auditor's findings (when the artifact was audited via §3.5.1).
- A complexity classification — is this a one-line `curl`, or a
  500-line script that imports `subprocess`, `base64`, and calls
  `eval()` three times?

For trivial cases the summary is enough. For non-trivial cases the
operator is making an authority call (P-2.1, P-2.24) with **less
information than the auditor agent had** (which read the full code
during static review). That asymmetry is backwards: the operator has
higher authority but sees less.

This is also a consent problem: P-2.24 says high-risk approvals need
dwell time and typed confirmation phrases. But "I confirm I want this
to run" is meaningfully different from "I confirm I have read what
this will do." The current flow lets the operator confirm the former
without doing the latter.

## 2. Scope (split into two phases)

### Phase 1 — Code visibility in approval cards (small win, ship soon)

Pure UX + plumbing. The gateway already has the artifact's files via
the artifact store. Add to the approval payload for
`sandbox_exec` / `artifact_exec` / `agent_install`:

- `code_excerpts: Vec<CodeExcerpt>` where
  `CodeExcerpt { file_name, content, language, size_bytes,
  truncated_from_bytes }` — the executable files from the artifact.
  Caps:
  - Per-file cap (configurable, default 32 KiB) — larger files include
    head + tail with truncation marker.
  - Total cap (configurable, default 128 KiB) — additional files listed
    by name only.
- `risk_summary: RiskSummary` derived from the existing
  `RemoteAccessAnalyzer` plus the auditor's `promotion_record` if
  present:
  - host count, distinct protocols, language mix
  - dangerous-pattern flags (eval / exec / base64 blobs / subprocess)
  - auditor verdict (`pass | fail | unable_to_evaluate`) if a record
    exists for this artifact
  - link to the auditor's findings array
- `audit_trail_link: Option<String>` — opaque handle the operator can
  use to view the full auditor findings via `gateway approvals
  audit-findings <id>` (a new read-only command).

Surfaces:

- **`gateway approvals show <id>`** appends a `Code:` section with
  syntax-highlighted output (per-language fences for readability).
- **Interactive approvals TUI** — code shown in the detail panel,
  scrollable. New keybinding `c` toggles a full-screen code-only view
  for longer files.
- **Chat approval cards** — code section collapsed by default; `c`
  expands inline.
- **JSON-RPC `gate.get_messages`** carries the same `code_excerpts`
  payload so SDKs can surface it however they want.

**Phase 1 is non-breaking, no new gate kinds, no new constitutional
rule.** Just richer payload on existing approval flow.

### Phase 2 — Escalation-on-complexity (RFC-shaped, ships later)

Above a complexity threshold, the gateway routes the request through
`GateKind::Escalation` instead of `GateKind::Approval`. Escalations
have:

1. **Default-open code view** in every surface. Operator must see the
   code before any approve/reject control is enabled.
2. **Mandatory dwell time on the code section** — operator can't
   approve in < N seconds. Same mechanism as P-2.24 dwell-time on
   high-risk approvals; tighter parameters for escalations.
3. **Mandatory typed confirmation phrase** carrying the *code-seen*
   semantics, e.g. `"I have read the code at <artifact_ref>"`. The
   `confirm_phrase` field on the approval action (P-2.24) is extended
   for escalations to include the artifact identity, so a copy/paste
   attack can't reuse a phrase from a different artifact.
4. **Persisted code-seen marker**: the decision record carries
   `code_seen: true`, `code_dwell_ms: <int>`, and a digest of the
   code excerpts shown to the operator. Forensics: "did the operator
   actually look at this artifact?" → answer is in the record.

#### What triggers escalation

A configurable scoring function `classify_review_difficulty(artifact,
analyzer_findings, auditor_record)` returns one of:

- `Simple` — single file, < 50 lines, single language, ≤ 1 remote
  host, no dangerous patterns. → normal approval.
- `Moderate` — multi-host or multi-protocol but still summarisable. →
  normal approval, code section available but collapsed.
- `Complex` — any of:
  - More than 5 detected remote-access hosts
  - Mixed languages or subprocess invocations
  - Encoded/obfuscated patterns (base64 literals > 1 KiB, `eval(`,
    `exec(`, dynamic imports)
  - `subprocess` + `base64` + network in the same file
  - Auditor flagged any `unable_to_evaluate` finding for this artifact
  - Capability declarations exceed what static analysis can verify
  → **escalation**.

The thresholds are config-driven so operators can tighten/loosen them
per environment.

#### Constitutional alignment

- **P-2.1** (remote access requires approval) — escalation is a
  *stronger form* of approval. Same rule applies.
- **P-2.24** (operator approval hardening: dwell time + typed
  confirmation for destructive classes) — escalation re-uses this
  mechanism with tighter parameters and extends the confirm-phrase
  shape.
- **P-2.18** (unified `GateService`) — escalation already exists as
  `GateKind::Escalation`. Phase 2 extends it from "agent-initiated
  escalation" to "gateway-triggered escalation on complexity."
- **P-2.19** (gate enrichment messages on causal chain) — operator
  comments + ask-agent dialogue during review thread on the gate.
- **Ri-0.1** (agent inspects own state). Phase 2 gives the operator
  the analogous right: inspect what they're about to approve.

#### Composes with existing primitives

- **Ask-agent (#172)**: operator reading complex code can ask the
  agent "why does this artifact import `subprocess`?" The clarification
  child session answers; reply lands in the gate enrichment thread.
- **Auditor Shape-2 review (§3.5.1)**: escalation surfaces the
  auditor's findings inline. If auditor said `pass` but the gateway
  classifies as `complex`, the operator can see both signals.
- **Sealed-network sandbox (RFC 5.2)**: orthogonal. Sealed-network is
  the runtime defence; escalation is the pre-runtime informed-consent
  boundary.

#### Acceptance criteria (Phase 2)

- New `Escalation` variant of approval action (or a flag on
  `Approval`) carrying `complexity_classification`, `code_digest`,
  `auditor_record_ref`, `required_dwell_ms`, `confirm_phrase_shape`.
- `classify_review_difficulty` function + config-driven thresholds.
- `gateway approvals approve <id>` requires `--code-seen` plus the
  artifact-bound confirm phrase for escalations; refuses without.
- TUI escalation surface: full-screen code view, dwell countdown, no
  approve button until dwell completes.
- Causal event `approval.code_review_completed` with the dwell duration,
  code digest, and operator identity.
- Constitution test: an artifact classified `Complex` cannot be
  approved without the typed phrase and minimum dwell.

## 3. Open questions

1. **Source-code privacy.** Some artifacts may legitimately contain
   secrets (env-loading, hard-coded test fixtures). The code visible
   in the approval card runs through the same `log_redaction`
   pipeline that gate-message content uses (P-2.19) — but redaction
   may strip *exactly* the thing the operator needs to see (the
   hard-coded API URL pattern, say). Resolution: redaction policy
   per-approval is a config decision; operators can opt out of
   redaction for code excerpts via an explicit setting and audit
   record.
2. **Auditor-redundancy concern.** If auditor Shape-2 review already
   checks the SKILL.md and capability scopes, what does the
   operator's escalation check add? Answer: defence-in-depth. The
   auditor is an agent that could itself be compromised. The
   operator's informed approval is the human-authoritative final
   check. The auditor's findings *inform* the operator's review but
   don't replace it.
3. **Dwell-time UX.** A 30-second dwell on a 500-line script is
   nowhere near enough for a real review. The dwell-time is *minimum
   eyeball time*, not "long enough to actually read this." Phrase
   wording must signal that the operator's responsibility is to read,
   not just wait.
4. **Multi-file artifacts.** When the artifact has 10 source files,
   what does "code seen" mean? Initial answer: the `code_digest` in
   the decision record covers the concatenation of all excerpts shown
   to the operator (capped by the per-file and total caps). If the
   artifact is too large to fully display, that itself triggers
   `Complex` classification and the operator must explicitly
   acknowledge the truncation.
5. **Replay attacks on confirm phrase.** A phrase like
   `"I have read the code at art_a1b2"` is fixed per artifact; an
   attacker who already saw an approval flow could pre-type the
   phrase. Mitigation: the confirm phrase includes a session-bound
   nonce shown to the operator only inside the gate detail view, e.g.
   `"I have read the code at art_a1b2 (nonce: XYZ123)"`. Each gate
   gets a fresh nonce; the gate row stores it; approval validates the
   nonce matches.

## 4. Migration / rollout

Phase 1 can ship as a single PR — no design risk, no new gate
mechanics. Approval cards get richer; existing approval flows are
unchanged.

Phase 2 is internally ordered:

1. Add the `classify_review_difficulty` function + tests against a
   corpus of sample artifacts. Default thresholds picked
   conservatively (most artifacts → Moderate, not Complex).
2. Add the `Escalation` variant of approval action (or the flag on
   `Approval`).
3. Update `gateway approvals approve` to require `--code-seen` for
   escalations.
4. Update the TUI to surface escalation as a distinct flow.
5. Constitutional test pinning the dwell + phrase requirements.

Phase 2 is gated on Phase 1 being stable.

## 5. What this is NOT

- Not a replacement for the auditor's Shape-2 review — that's
  pre-install. This is approval-time, which can be any time after
  install (e.g., when the agent first asks for network).
- Not a replacement for the sealed-network sandbox — that's runtime.
  Escalation-on-complexity is informed-consent before runtime.
- Not a code-quality tool — the gateway is not making correctness
  judgements about the code, only flagging when complexity exceeds
  what an automatic summary can convey.
