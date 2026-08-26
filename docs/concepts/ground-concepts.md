# Ground Concepts

The small set of structural ideas every Autonoetic feature is built from.
Where [`philosophy.md`](philosophy.md) explains *why* the system is built this
way, this doc names the recurring *mechanisms* — the concepts that show up
again and again in the code — and maps each to where it is realized and which
features rest on it.

Each entry: the concept, its mechanical form, and the features that would
collapse without it. Feature status is tracked separately in
[`../reports/2026-08-26-capability-inventory.md`](../reports/2026-08-26-capability-inventory.md).

---

## 1. Immutability and content-addressing

Nothing is ever overwritten. Artifacts, revisions, sessions, and the causal
chain are content-addressed (SHA-256) or append-only; identity *is* content.

**Mechanical form:** the artifact store (files never change once created);
revision identity = canonical `content_digest` (identical content reuses the
revision); the hash-chained causal chain (P-8.1); versioned, digest-pinned
constitution (amendment mints a new version, old ones never mutate).

**Rests on it:** promotion evidence binding (evidence attaches to a digest;
digest changes → evidence is cleared and re-run), approval/exec-cache
fingerprints (an approval cannot silently transfer to mutated code),
provenance, replay, federation digest handshake.

**Why it's ground:** history that wasn't attributed can never be
re-attributed — so attribution happens at write time, forever (philosophy
§4.7).

## 2. Separation of powers

Reasoning (agents), enforcement (gateway), and decision (deciders) are held
by different parties; none can absorb the others' function.

**Mechanical form:** agents propose, the gateway executes; safety-critical
invariants are mechanically enforced, never delegated to LLM judgment.

**Rests on it:** the capability system, the approval gates, the sandbox
boundary. Full treatment: [`separation-of-powers.md`](separation-of-powers.md).

## 3. Law outside the prompt (the Lawful Executor)

Rules live in a signed constitution and in code, not in instructions the
model can forget, out-reason, or be injected past. Rejections name their rule
(Ri-0.3) as structured rule/right IDs, never prose.

**Mechanical form:** `policy.rs` checks before privileged operations;
structured `ToolError` carrying `enforced_rules` (`Vec<String>` of `P-x.y` /
`Ri-x.y` IDs — populated for policy-gated capability denials, with coverage
pinned by `constitution/rights_late_bucket.rs`, not a closed enum on every
error) plus a mechanically-derived `available_actions` list; the enforcement
register mapping every clause to code and tests.

**Rests on it:** every "say" claim in the launch pitch — least privilege,
credential isolation, egress gating.

## 4. Bind-direction discipline

Every clause binds exactly one party: rules (`P-*`) bind the agent, rights
(`Ri-*`) bind the gateway, obligations (`O-*`) bind the decider, `U-*` binds
the community toward the served party.

**Mechanical form:** the constitution's clause taxonomy; the register's
bind-direction summary; denials that carry lawful next moves (the gateway's
debt made visible at the point of refusal).

**Rests on it:** the social-contract framing — the enforcer is a bound party,
which is what separates a compliance regime from a constitution (philosophy
§2).

## 5. Declared capability, deny by default

An actor's powers are declared in its manifest, scoped by patterns, and
everything undeclared is refused — "cannot," not "asked not to."

**Mechanical form:** the `Capability` enum; `is_available()` gating per tool;
fail-closed sandbox and network-grant tests; capability *inference* that
never mints a wildcard (wildcard power is always an explicit, attributable act).

**Rests on it:** the coder-that-can't-touch-the-network story, one-door
activation, skill install trust modes.

## 6. Non-repudiable attribution

Every act carries an actor, durably (Ri-0.11). Lineage is derived from spawn
structure by the gateway, never asserted by the agent.

**Mechanical form:** causal-chain entries with actor identity; revision
`created_by` (installer) / `requested_by` (designer) split; approval cards
that name their principal.

**Rests on it:** audit, promotion gates, the future franchise (standing
computed from the ledger, never self-asserted).

## 7. Typed contracts at every boundary

Boundaries exchange schemas, not text: `io.accepts`/`io.returns` on agents,
structured error envelopes, typed wake-up reasons, typed stop conditions.

**Mechanical form:** spawn-time payload validation with per-field repair
hints; `io.returns` enforced on every script execution; closed enums for
denial reasons and loop-guard trips.

**Rests on it:** multi-agent delegation (the planner can translate intent
into a stranger's schema), script-mode reliability, resumability.

## 8. One door

Every path that activates power passes the same gate. There is no
install-specific shortcut, no side entrance.

**Mechanical form:** single activation path (artifact → revision → promote)
for built, imported, and evolved agents alike (P-9.15); one dedup chain for
approvals regardless of entry tool.

**Rests on it:** agents-building-agents being safe by construction; import
provenance.

## 9. Verified self-model (functional autonoesis)

The agent is not asked to remember what it is; the gateway hands it a signed
self-model every turn — budget, capabilities, pending gates, the constitution
digest it runs under — taught as more authoritative than its own memory.

**Mechanical form:** the P-6.23 turn-boundary attestation; `self_describe`;
rights surfaced in the foundation prompt.

**Rests on it:** legitimate responsibility (an actor that truly knows its
standing can be held to it); the project's name. Philosophy §1.

## 10. Correctability over perfection

The enforcer is fallible; legitimacy comes from errors being reportable,
attributable, and correctable — and the correction machinery is entrenched.

**Mechanical form:** the entrenched correction core (read your chain, named
rejection, propose amendment, non-repudiation, hash-chain integrity);
`anomaly_flag` (capability-free reporting, owed adjudication); discretion
leaks named and counted; contract health as a monitored gap between declared
rule and official action.

**Rests on it:** the honest pitch — auditable, detectable, accountable rather
than "unbreakable." Philosophy §3.1.

## 11. Graduated response

Between healthy and killed there are steps, and the subject is told at each
one.

**Mechanical form:** warnings → degraded mode → escalation → emergency stop
(P-7.18, Ri-0.5); per-tool failure budgets before session death; timeouts
before abandonment.

**Rests on it:** unattended runs that degrade gracefully instead of dying
silently; Ostrom's graduated-sanctions finding (philosophy §6).

## 12. Advisory before binding

Every new judgment layer starts observational, accumulates a calibration
record against outcomes, and gains authority only on that evidence.

**Mechanical form:** the Sentinel observes and never blocks (Ri-0.16);
agent-deciders staged through the same capability hardening as humans
(P-2.20); eval suites that must be run explicitly.

**Rests on it:** the democratic trajectory being a parameter change rather
than a revolution (philosophy §3.2, §4.4).

## 13. Office before occupant

Seats (decider, auditor, steward) are defined with duties attached,
independent of whether a human or an agent occupies them.

**Mechanical form:** `GateDecider` as a capability authorizing a principal,
not a person; evolution offices (steward, curator, crystallizer) as agents
bound by the same law.

**Rests on it:** enfranchisement without re-architecture; mixed human/AI
panels.

## 14. Data locality as label lattice

Where content may flow is declared metadata the gateway alone manipulates —
labels combine by meet (never widen), taint is monotonic, and declassification
is a gated, audited act. Never model-inferred.

**Mechanical form:** the egress label plane (`EgressLabel`, allowed sink
sets); the LLM chokepoint; per-agent output floors.

**Rests on it:** "an agent may read my emails; their content never reaches a
remote model." Philosophy §3.3's fourth facet.

## 15. Durability across sleep and restart

Actors hibernate and wake; nothing essential lives in a process.

**Mechanical form:** checkpoints, HMAC-signed continuations, typed wake-ups
(Ri-0.14 — parents yield instead of polling), sessions that survive gateway
restarts, work that resumes at approval boundaries.

**Rests on it:** the overnight build; the 3-hour unattended run; the whole
"walk away" pitch.

## 16. Exit and voice

Members respond to decline by leaving or speaking; both are provided, because
voice without credible exit degrades into ritual.

**Mechanical form:** voice = amendment proposals (Ri-0.8) and escalation
channels; exit = session termination (Ri-0.7) today, capsule emigration
(Ri-0.17, partial) tomorrow; for the served party, refusal.

**Rests on it:** federation as constitutional pluralism — communities under
different laws, interoperating where verifiably compatible; Hirschman's
argument in philosophy §6.

---

## Reading the map

Sixteen concepts, but they compound: immutability (1) makes attribution (6)
meaningful; attribution makes evidence-computed standing possible; typed
contracts (7) make delegation safe; one door (8) makes growth safe; verified
self-models (9) make responsibility legitimate; correctability (10) makes all
of it survivable. A feature that violates one of these is a bug against the
concept, not a trade-off — that is what makes them *ground*.
