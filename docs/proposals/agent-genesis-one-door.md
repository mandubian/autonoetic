# Agent Genesis — One Door

> **Status:** Partial — the security core **shipped** (Parts A, B, C, D, E.1,
> F.1, merged via #802/#805; plus designer lineage #803/#804), and the
> P-9.15/P-9.16/I-13 amendment (#800) is **enacted and signed** in the
> 2026.07.19 constitution. **Remaining:** F.3/F.4 birth quality (#799,
> staged). F.5 probation is deliberately routed to the citizenship
> RFC's E.3, not here. Tracking issue:
> [#801](https://github.com/mandubian/autonoetic/issues/801)
> (workstreams #793–#800). Companion to
> [`citizenship-as-a-runtime-service.md`](citizenship-as-a-runtime-service.md)
> (#774): that RFC makes existing agents behave like citizens; this one makes
> sure **new** agents — whether built by the system or imported from outside —
> are *born* through one lawful door. Constitutional clauses proposed here
> (P-9.15, P-9.16, I-13) are now law in
> [`docs/constitution/versions/2026.07.19/`](../constitution/versions/2026.07.19/constitution.md).

---

## Motivation

Autonoetic's purpose includes **building the agents it lacks** — and having
rules to build them well. A design audit of both genesis paths (2026-07-14,
against `main`) found the first path principled and the second effectively
ungoverned:

**Path 1 — the revision pipeline** (`agent_revision_create_from_intent` →
evidence → `agent_revision_promote`) is governed in detail by constitution
§9: three-stage activation (P-9.1), immutable content-addressed revisions
(P-9.3), risk-graduated evidence gates (Full jury for
CodeExecution/AgentSpawn; audit-only for mid-risk; fail-closed otherwise),
and P-2.25's rule that a new agent's entire capability set is the maximal
delta requiring operator approval.

**Path 2 — `skill_install`** reaches the *exact same end state* (a promoted,
active agent) through `bootstrap_single_agent`'s direct store calls
(`insert_agent_revision_transactional` + `atomic_promote`), **bypassing every
one of those gates**. A remote SKILL.md declaring `NetworkAccess` +
`CodeExecution` with `trust_mode: generous` activates instantly — no
evaluator, no auditor, no capability-delta approval, no smoke test. The
constitution never mentions `skill_install`, `SkillInstall`, or `trust_mode`
at all.

Three aggravating findings:

1. **The documented protection is a lie in the "a right without a test"
   sense.** `trust_mode: strict` (the default for third-party skills) is
   documented as "the new agent cannot take any privileged action without an
   approval gate." Mechanically, `Capability::ApprovalQueue` only unlocks the
   Workflow tool tier and gates `admin_proposal_*` — declared
   `NetworkAccess`/`CodeExecution` remain fully live. Only `audit` mode
   protects, and only because it discards capabilities.
2. **Third-party frontmatter mints wildcard power.** `Bash(*)` in a remote
   skill's `allowed-tools:` infers straight into
   `CodeExecution{patterns:["*"]}`; `WebFetch` →
   `NetworkAccess{hosts:["*"]}` — from text, with no code behind it to
   review (a `skill_install` bundle contains *only* the SKILL.md).
3. **No import provenance exists.** No source URL, content digest, or fetch
   time is recorded anywhere durable; the revision reads
   `created_by: "cli", source_kind: "bootstrap"` — indistinguishable from a
   local reference bundle. Per the philosophy's append-only argument (§4.7),
   a distinction that is conceivable now and unrecorded can never be
   back-filled.

The audit also surfaced genesis-side gaps that are not security defects but
are "rules to build them well" debt: the live pipeline *teaches a falsehood*
(stale guidance contradicting the June fail-closed promotion fix), newborns
get no eval suite and no probation, and runtime-generated SKILL.md bodies
receive no mechanical quality checks.

### What is already right (and must not be regressed)

- **Citizenship is runtime-composed, not manifest-copied.** A newborn cannot
  be rights-less: foundation doctrine, the signed attestation, `self_describe`,
  `anomaly_flag`, the `anomalies` witness contract, denial affordances, and
  injected recall are all services the gateway renders at wake. Nothing in
  this RFC moves civic content into manifests.
- **Capability comes from the gate, not the parent.** There is deliberately
  no creator-capability attenuation (see Part F.2): hereditary capability
  bounds would freeze the society to its founders' powers. The control is
  evidence + operator-approved delta — this RFC *states* that principle as
  law rather than leaving it an accident.

### Invariants

1. **One door.** Every path that activates an agent (moves an alias to a
   revision) passes the same promotion gates. An installation surface that
   bypasses P-9.7/P-9.9/P-2.25 is a defect, whatever its convenience.
   (Bootstrap of the repo's own reference bundles at gateway startup is the
   sole, explicitly-scoped exception: it installs the operator's own code
   from the local tree, before any session exists.)
2. **Congruence.** A documented protection must be mechanically real
   ("a rule without a test is a wish; a right without a test is a lie").
   Where semantics and docs disagree, either implement the semantics or
   fix the docs — never leave the claim standing.
3. **Provenance from the moment conceivable** (philosophy §4.7). Imported
   vs. locally-built is recorded at install time, durably, or never.
4. **Capability is never inferred into wildcards from untrusted text.**
   Inference from external frontmatter may propose narrow capabilities; it
   may not propose `*`.
5. **Fail closed on structural impossibility.** An install that cannot
   produce a functional agent (script entrypoint that was never fetched) is
   rejected up front, not silently promoted.

---

## Part A — One door: `skill_install` installs a Candidate, never promotes *(fix-now)*

`skill_install` keeps its fetch/parse/trust-mode/write behavior but stops at
**Candidate**: it creates the revision (via the bootstrap machinery or a
Candidate-only variant) and does **not** promote. Activation then flows
through the standard door — `agent_revision_promote` with its
risk-graduated evidence gates and the P-2.25 capability-delta operator
approval (a new agent's full set = maximal delta).

- Tool response changes honestly: `activated: false`,
  `status: "candidate"`, plus a `next` hint naming the promotion path and
  what evidence the declared capabilities will require.
- Consequence accepted: importing a skill is no longer instant-on. That is
  the point. A zero/low-capability import faces a proportionally light gate
  (P-2.25 approval only); high-risk imports face the same jury as
  high-risk built agents.
- Gateway-startup bootstrap of `agents/**` reference bundles keeps its
  auto-promote (invariant 1's scoped exception); the code path gains an
  explicit parameter so the exception is visible in the signature, not
  implicit in a shared helper.

## Part B — `trust_mode` congruence *(fix-now: docs + honest semantics)*

With Part A in place, the promotion gate is the real protection and
`trust_mode` shrinks to what it truly is: a policy on **which capability set
the Candidate carries into the gate**.

- `generous` — declared capabilities pass through to the gate unchanged.
- `strict` (default) — declared capabilities pass through, **minus** any
  high-risk capability that was *inferred* rather than explicitly declared
  under `metadata.autonoetic.capabilities` (see Part C); keeps
  `ApprovalQueue` for what it actually does (admin-proposal filing).
- `audit` — unchanged: `ReadAccess(self.*)` + `ApprovalQueue`.

All documentation (docs/AGENTS.md, the tool description) is rewritten to
describe these real semantics; the false "approval gate for all actions"
claim is removed everywhere. If a genuine per-action approval-interceptor
capability is ever wanted, it is a separate RFC — not a doc claim.

## Part C — Clamp wildcard inference from `allowed-tools`

`infer_capabilities` may not mint `*`:

- `Bash(...)` → `SandboxFunctions` for the named prefixes only; **no**
  `CodeExecution{patterns:["*"]}`. If the skill genuinely needs shell
  execution, it must declare `CodeExecution` explicitly in
  `metadata.autonoetic.capabilities` — an intentional, visible act that the
  gate will then weigh.
- `WebFetch`/`WebSearch`/`Fetch` → `NetworkAccess` with an **empty hosts
  list plus a structured install-time warning** naming the hosts field, or —
  where the skill body names concrete endpoints — those hosts only. Never
  `hosts:["*"]`.

Rationale: inference is a convenience for *trusted-ish* content; wildcard
power must always be an explicit declaration someone can be held to
(Ri-0.11 attribution has nothing to attribute when the grant came from a
mapping table).

## Part D — Durable import provenance *(fix-now)*

At `skill_install` time, record on the revision:

- `source_kind: "skill_install"` (today: the generic `"bootstrap"`),
- `source_ref: Some("<url>#sha256=<digest-of-fetched-bytes>")`,
- and emit a causal event (`category: "agent_install"`,
  `action: "skill_imported"`, target = agent id, payload = url, digest,
  trust_mode, inferred-vs-declared capability summary).

This makes "which agents in this gateway came from outside, from where,
and what exactly arrived" a queryable fact forever — and gives the future
P-9.16 (Part G) something real to point at.

## Part E — Structural honesty for imports

- **E.1 Reject script-mode imports** *(fix-now)*: `skill_install` fetches
  exactly one file; an `execution_mode: script` manifest names an entrypoint
  that will not exist. Reject at install with a clear error ("skill_install
  cannot fetch companion files; package the skill as an artifact and use the
  revision pipeline, or import a reasoning-mode skill"). Today this installs
  and promotes a broken agent silently.
- **E.2 Runtime closure**: imported skills get a hardcoded empty
  `runtime.lock`. For reasoning-mode skills this is acceptable (no code);
  document it. If/when multi-file import exists, imports with dependencies
  route through `packager.default` like built agents — out of scope here.

## Part F — Genesis-side "build them well" debt

- **F.1 Fix the false doctrine** *(fix-now)*: the
  `agent_revision_create_from_intent` tool description and
  `specialized_builder.default/SKILL.md` still teach "omit `artifact_ref` —
  capability enforcement is the security gate," a path the June fail-closed
  fix (`4e0b88c9`) now rejects for any non-empty capability set (the builder
  manifest contradicts itself internally). Rewrite both to match the real
  gate matrix. Every factory run currently gets steered into a refusal.
- **F.2 State the no-attenuation principle as law** (→ Part G, I-13):
  *creation is not delegation* — a newborn's capabilities are granted by the
  gate (evidence + approved delta), never inherited from nor bounded by its
  creator's. This is the correct design (hereditary bounds would freeze the
  society to its founders); it must be a stated invariant so nobody
  "fixes" it into lineage later.
- **F.3 Eval suite at birth**: factory doctrine (agent-factory /
  specialized_builder SKILL.md) gains a required step — publish a minimal
  eval suite for the newborn alongside the revision, so P-9.7 has something
  to bite on and future drift detection has a baseline. Advisory first;
  mechanical presence-check at promotion later if adoption lags.
- **F.4 Runtime quality checks on generated SKILL.md**: at
  `create_from_intent`, mechanically (a) warn when a reasoning-mode agent
  declares no `io.returns` (the witness contract and output contract both
  need it), (b) scan the body for the doctrine fingerprints
  (`skill_doctrine_guard`'s list) — the CI guard never sees runtime-born
  agents. Warnings in the BundleHealthReport (P-9.12), not rejections.
- **F.5 Probation / graduated trust**: deliberately **not** designed here —
  it is the citizenship RFC's E.3 (civic evals as promotion evidence) plus a
  possible observation window on newly promoted aliases, building on the
  existing Tier-1 post-promotion review. Recorded so the two RFCs don't
  drift apart: E.3 is the newborn-screening mechanism; this RFC only makes
  sure every newborn passes through the door where screening can happen.

## Part G — Constitutional clauses (enacted 2026.07.19)

Enacted in `docs/constitution/versions/2026.07.19/constitution.md`, signed in
the same batch as the Ri-0.18 / O-7 / O-6-SLA citizenship amendments:

- **P-9.15 (Single door):** every surface that activates an agent passes the
  same promotion gates (P-9.7, P-9.9, P-2.25); gateway-startup bootstrap of
  local reference bundles is the sole exception. — status at enactment:
  ENFORCED (by Part A + a pinning test).
- **P-9.16 (Import provenance):** an agent installed from an external source
  durably records source URL, content digest, and install time on its
  revision; the install emits a causal event. — ENFORCED (Part D).
- **I-13 (Creation is not delegation):** a newborn agent's capabilities are
  granted through the promotion gate and are neither inherited from nor
  bounded by the creating agent's capabilities. — declared invariant
  (documents Part F.2; no new mechanism).

## Sequencing

1. **Fix-now PR** — Parts A, B (docs + strict-clamp), D, E.1, F.1, with
   tests. This closes both security-weight findings and stops the pipeline
   teaching falsehoods.
2. Part C (inference clamp) — small follow-up; severity is already defused
   by Part A (a wildcard on a Candidate still faces the gate).
3. Part G amendment draft — written with the fix-now PR so the law and the
   mechanism land in the same signing batch.
4. F.3/F.4 — factory doctrine + BundleHealth warnings.
5. F.5 — via citizenship RFC E.3.

## Open questions

1. **Should `generous` mode survive at all?** With one door, `generous` is
   merely "carry the declared caps to the gate," which is what `strict`
   minus the inference-clamp also does. Keeping three modes may be more
   ceremony than protection; collapsing to `declared` / `audit` is tempting
   but breaks the documented API — decide at Part C time.
2. **Multi-file import.** The honest fix for script skills is a real bundle
   fetch (artifact-first import), which converges with the revision
   pipeline — at which point `skill_install` becomes sugar over
   `create_from_intent`. That convergence is probably the end state; this
   RFC only refuses to pretend it already exists.
3. **Bootstrap exception surface.** Should startup bootstrap of reference
   bundles eventually verify signatures (P-9.13 applies to
   `agent_revision_create`, not bootstrap)? Out of scope, but the exception
   in P-9.15 should not silently widen.
