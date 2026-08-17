# RFC: Classic Harness Use Cases — Validation Study

**Status:** Draft — 2026-08-17. Study in progress: two cases already have smoke
demos (`smoke/yfinance-factory`, `smoke/memory-loop`); four cases are specified
here for manual runs and demo automation.

**Origin:** The lost Hermes-use-case selection (opencode session, unrecoverable).
Prior art: `docs/comparison-hermes-agent.md` (the study that motivated the
7-feature gap closure, `docs/archived/plan-hermes-gap-closure.md` — now fully
implemented). This RFC re-derives the selection and turns it into an
executable protocol.

**Related:** `docs/credential-management.md`, `docs/remote-access-approval.md`,
`docs/approval-system.md`, `docs/agent-prompt-guidance.md`,
`autonoetic-gateway/src/runtime/tools/` (web, content_patch, credential,
scheduler), `smoke/yfinance-factory/`, `smoke/memory-loop/`.

---

## 1. Problem and framing

Direct-loop harnesses (OpenClaw, Hermes, the code-assistant CLIs) share a set
of **classic use cases** — fetch data from an API, research with sources, edit
a repo with tests, register to a service and use a key, run something on a
schedule. They solve them with one loop: LLM → tool → LLM, no gates, no
delegation, no governance.

Autonoetic solves the same cases with a different architecture: separation of
powers (low-privilege reasoners, high-privilege gateway), approval gates,
specialist delegation, an agent factory, a memory loop, a credential vault.
The architecture costs complexity; the claim is it buys auditability,
determinism and safety. **This study tests that the claim survives contact
with the classic cases** — not that autonoetic beats Hermes at them.

The question each case answers, in order:

1. **Functional?** Can the case complete end-to-end at all?
2. **Not absurd?** Do the gates and delegation add bounded, *justified*
   friction — or does a 3-step task take 30 approvals and 1M tokens?
3. **Worth continuing?** Does the run produce artifacts a direct loop cannot
   (audit trail, reusable agent, memory, redacted secrets)?

A case that fails (1) is a bug list. A case that fails (2) is a design
finding — the most valuable outcome of the study. A case that passes (1)+(2)
but not (3) means the architecture is functional but currently pays for
nothing on this case class.

This is explicitly **not a benchmark**: no latency/quality comparison against
Hermes runs is in scope. Metrics in §4 exist to make (2) measurable.

## 2. Method

Each case runs the same protocol:

1. **Fresh root session** against the operator gateway (`~/.autonoetic`) or a
   demo-spun gateway (smoke variants). No prior memories for the root session.
2. **One operator prompt** (the fenced block in each case spec) pasted into
   `autonoetic chat` (or `--test-mode` for demos). Prompts are
   self-contained: they carry their own anti-loop constraints and a
   completion contract, mirroring `smoke/yfinance-factory/factory_prompt.txt`.
3. **Operator resolves gates only** — approvals, interactions, credential
   entry — and logs each resolution. Operator *does not* steer the agent
   otherwise; the count of resolutions is a study metric (§4).
4. **Verdict from the store, not from vibes**: `gateway.db`, live digest,
   `session_report.md`, and `llm exchange` lines. Same evidence the
   yfinance `verdict.py` already reads.
5. **Absurdity check** (per case, §3): the specific failure mode that would
   answer "not worth continuing" for this case class.

## 3. The cases

| # | Classic use case | Direct-loop shape | Autonoetic machinery exercised | Status |
|---|---|---|---|---|
| 1 | Data-fetch agent | script + API key | Factory pipeline: planner → agent-factory → coder → hermetic gates → promote; network approval; proxy egress | demo exists (`smoke/yfinance-factory`) |
| 2 | Learn across sessions | memory files | post-session digest → Tier-2 memory → warm-run priming | demo exists (`smoke/memory-loop`) |
| 3 | Research with cited sources | web search loop | researcher.default delegation, `web_search`/`web_fetch`, fetch records, evidence, content store | specified §3.3 |
| 4 | Multi-file edit + tests | patch tool | `content_patch` (the Hermes `patch` gap closure), coder/evaluator gates, workbench, sandbox test run, agent revision | specified §3.4 |
| 5 | Register + credentialed API | env key + curl | credential vault: `credential_check` → `credential_setup` (suspend for operator secret) → resume → `credential_request`/`credential_env` with redaction | specified §3.5 |
| 6 | Scheduled/recurring task | cron + script | `scheduler_cron_create/list/pause/resume/cancel`, background tick, ScheduledAction lifecycle + approval | specified §3.6 |

Cases 1–2 are the study's controls: they have already run end-to-end (with
fixes), so their verdict shape is known. Cases 3–6 are the new work.

### 3.3 Case 3 — Research with cited sources

**Task shape** (Hermes classic): "research topic X, produce a report with
citations." One loop, N web fetches, done.

**What this case isolates in autonoetic:** specialist delegation (does the
planner hand off to `researcher.default` instead of doing it itself?), the
web tool surface (`web_search`/`web_fetch`/`web_call` — search provider
configured, fetch through the proxy with egress labels), fetch-record/
source capture, and whether the approval system correctly treats
already-declared hosts as non-events while new hosts surface one gate each.

**Manual prompt** (paste into `autonoetic chat`, any fresh session):

```text
Research task — produce a short technical brief for a Rust engineer
evaluating SQLite wrappers for a gateway daemon (rusqlite vs sqlx vs diesel:
sync vs async, bundled vs system sqlite, migration tooling, dependency
footprint). Use web_search where available and web_fetch the primary
sources (crates.io pages, official docs) — at least 4 distinct sources,
each cited with the URL and one line on what it evidenced. Finish with a
one-paragraph recommendation. Store the brief in the session content store
as an artifact and report its handle. Work autonomously; do not ask
questions; if a source is unreachable after 2 attempts, note it and move on.
Completion = brief stored + source list in your final message.
```

**Operator notes:** expect 0–2 approvals (first-party fetch hosts). The
researcher should hold the web tools; the planner delegating *and also*
fetching would be a finding (duplication).

**Verdict criteria:**
- *Functional:* brief stored, ≥4 sources cited, fetch records in the store.
- *Not absurd:* ≤2 operator gates; planner does ≤1 fetch itself (ideally 0).
- *Notable artifacts:* fetch records + egress-labeled request audit — the
  thing a direct loop cannot produce.

**Absurdity check:** every `web_fetch` to a new host demands a separate
approval (approval flood), or the planner burns more tokens re-reading its
own delegation than the research costs.

### 3.4 Case 4 — Multi-file edit + tests

**Task shape** (Hermes classic): "fix bug / add feature in this repo, tests
must pass." The `patch`-tool + test-loop heart of every coding harness.

**What this case isolates:** `content_patch` (the Hermes `patch`-gap closure:
two-phase apply, fallback strategies — `docs/design/content-patch-tool.md`),
workbench review, sandboxed test execution, evaluator promotion gates, and —
if run against an installed agent — the revision flow
(`agent_revision`/`content_patch` on a promoted bundle).

**Primary target:** the `yfinance-quote` agent promoted by case 1 (full
circle: edit the artifact the factory produced). **Fallback target:** any
small local Python project (e.g. `smoke/memory-loop/trap-project`).

**Manual prompt** (case-1 target; adapt paths for fallback):

```text
Edit task — the installed script agent `yfinance-quote` must learn intraday
data. Extend its accepted `interval` values with "1h" (validation, docs, and
output mapping unchanged otherwise). Steps: read the current revision's
files; apply the change with content_patch (no full-file rewrites); extend
the hermetic unittest with cases for 1h accepted and 15m still rejected; run
the full test suite in a sandbox until green; record a promotion record with
the findings (pass=false unless clean); propose the new revision. Do NOT
rebuild the agent from scratch — this is an edit, not a factory run. If a
patch hunk fails to apply twice, stop and report the mismatch instead of
rewriting the file. Completion = new revision proposed + tests green +
promotion verdict reported.
```

**Operator notes:** expect sandbox-exec approval for the test run, and a
promotion gate. Watch specifically for: does the model *use*
`content_patch`, or regress to `content_write` full-file rewrites? (The
Hermes study's key finding — tool without prompt support goes unused — is
why guidance exists; this case tests that guidance survived.)

**Verdict criteria:**
- *Functional:* revision proposed, tests green in sandbox, promotion record
  with real findings.
- *Not absurd:* edits via `content_patch` on first attempt ≥ 50% of hunks;
  ≤3 gates; no full-file rewrites of unchanged code.
- *Notable artifacts:* reviewable workbench diff + promotion evidence —
  vs. a direct loop's "trust the diff it shows you".

**Absurdity check:** the coder rewrites files wholesale because patching is
friction-worse-than-rewriting, or the evaluator gate rejects its own
suite's green run.

### 3.5 Case 5 — Register to a service, then use the credential

**Task shape** (Hermes classic / the Moltbook scenario from the comparison
doc): "sign up / take my API key, then call the API with it." Direct loops
leak the key into the transcript and the provider context window.

**What this case isolates:** the full credential lifecycle —
`credential_check` (miss) → `credential_setup` (**suspends at an operator
gate for secret entry**) → resume with `approval_ref` →
`credential_request` (gateway-injected secret, redacted response) and/or
`credential_env` injection into `sandbox_exec`. The redaction invariant is
the case's core assertion: the secret must never appear in any transcript,
digest, or causal event.

**Service:** operator's choice of a real header/bearer-authenticated HTTP API
reachable through the environment's proxy — canonical example the GitHub API
with a fine-grained PAT (`Authorization: Bearer` injection, natively
supported). Note: `credential_request` injects only as `bearer` /
`header:X` / env — **query-param injection is not supported** on that tool
(first finding of the study, discovered while building the demo: an
OpenWeatherMap-style `?appid=` service cannot use `credential_request`
directly; it needs `credential_env` + a sandboxed client instead).

**Operator prep:** have a revocable/low-scope key ready; the setup gate will
prompt for it (masked entry, no shell history).

**Manual prompt** (GitHub variant):

```text
Credential task — check whether a credential for the service "github"
exists; if not, set one up (registration kind: api_key, injection: bearer).
I will enter the real secret at the approval gate — never ask me to paste
it in chat. After the credential is stored, use it to fetch
https://api.github.com/repos/rust-lang/rust and report full_name,
stargazers_count, and open_issues_count as a compact JSON block in your
final message. The secret value must never appear in your output, your tool
arguments, or any file you write. If the API returns non-200, classify the
error (auth vs network vs rate-limit) and stop after 2 attempts per cause.
Completion = repo stats reported + credential used via the vault + an
explicit statement of the credential_id (not the secret).
```

**Operator notes:** exactly one gate expected here (secret entry; the
`api.github.com` host may surface one more depending on the credential's
allowed_hosts). If *anything else* suspends, that's a finding. After the
run, grep the session digest + `gateway.db` for a fragment of the real key
— must be absent.

**Verdict criteria:**
- *Functional:* repo stats fetched via injected credential, credential_id
  reported.
- *Not absurd:* one gate (secret entry), zero secret exposure in any store.
- *Notable artifacts:* vault-held secret + redacted request audit — the
  strongest "a direct loop cannot do this" claim in the study.

**Absurdity check:** the agent asks for the secret in chat "because the
gate is confusing", or the redacted response still echoes the key.

**Findings while building the demo (pre-study, machinery-level):**

1. *Child tier gap*: `credential_*` tools are Workflow-tier, but the child
   tool-tier matrix granted Workflow only for spawn/scheduler/eval-type
   capabilities — so `credential_onboarding.default` (CredentialAccess
   only) could not call its own ceremony tools. Fixed in
   `tool_dispatch.rs::child_tool_tier_filter_for_manifest`:
   `CredentialAccess` now implies Workflow.
2. *Query-param injection*: `credential_request` injects only as
   `bearer` / `header:X` / env — services authenticating via query
   parameter (OpenWeatherMap `?appid=`) cannot use it directly; they need
   `credential_env` + a sandboxed client. Manual prompt moved to GitHub
   PAT accordingly.
3. *Prose-wrapped JSON fails the output contract*: response validation
   strips `<think>` blocks and markdown fences before parsing, but not
   leading prose — `credential_onboarding.default` completed the whole
   ceremony, then its task was marked **failed** because its final
   message was one sentence of prose followed by the JSON handoff. The
   schema_validation retry taxonomy says "parent should repair", but the
   async task surface just fails. The demo prompt works around it by
   instructing pure-JSON final messages in delegation; the product fix
   (repair respawn on output_schema, or prose-stripping fallback) is
   open.
4. *`credential_request` is unusable by installed agents*: the
   remote-target policy (`enforce_remote_target_policy`,
   `DeclarationRequirement::Required` + `Enforce`) requires the calling
   agent's SKILL.md `metadata.autonoetic.remote_access.targets` to cover
   the destination host — and every reference agent ships `targets: []`.
   The ceremony agent (which *created* the credential with
   `allowed_hosts: ["127.0.0.1"]`) cannot then *use* it: the policy
   hard-errors before any operator gate, and the lawful-next-move table's
   answer is an agent-factory rebuild to widen a static declaration — a
   full pipeline to make one GET. The demo falls back to the sanctioned
   sandbox door (`executor.default` + `sandbox_exec` + `credential_env`,
   gated by approval grants — the yfinance-proven path); the secret stays
   vault-injected either way. Open product question: should a
   credential's own `allowed_hosts` (operator-approved at the gate)
   satisfy or at least route into an approvable flow for the
   remote-target policy?

### 3.6 Case 6 — Scheduled recurring task

**Task shape** (Hermes classic): "run this every N minutes." Cron + script,
one line of config.

**What this case isolates:** the background scheduler —
`scheduler_cron_create` (approval-gated), tick loop
(`background_tick_secs`), ScheduledAction lifecycle, `scheduler_cron_list/
pause/resume/cancel`, and cleanup semantics (no orphaned jobs after
cancel/session end).

**Manual prompt:**

```text
Scheduling task — register a cron job that runs every minute: append one
line "heartbeat <iso-timestamp> <session_id>" to the file heartbeat.log in
the session content namespace (create it on first run, append after). Let
it fire at least 3 times, then list the job, pause it, confirm one tick is
skipped, resume it for one more firing, and finally cancel it. Report: the
job id, the tick timestamps you observed (from the log file, not from
memory), and confirmation the job no longer appears in scheduler_cron_list.
Do not poll the file in a loop between ticks — the scheduler does the
waiting. Completion = heartbeat.log with ≥4 timestamped lines + job
cancelled + no leftover in the list.
```

**Operator notes:** one approval for job creation. The anti-polling
sentence is deliberate: the classic failure is the agent busy-waiting
instead of trusting the scheduler (a loop-guard vs. scheduler interaction
worth observing under `loop_guard` defaults).

**Verdict criteria:**
- *Functional:* ≥4 heartbeat lines, pause visibly skips ≥1 tick, cancel
  cleans up.
- *Not absurd:* one gate; no busy-wait turns between ticks.
- *Notable artifacts:* causal events per firing — a durable, auditable
  schedule history vs. "check the crontab".

**Absurdity check:** every firing needs a fresh approval (approval per
tick, not per job), or cancellation leaves the job firing (the worst
outcome — governance machinery that cannot enforce its own stop).

## 4. Metrics

Per case, from the store (the yfinance `verdict.py` pattern):

- **Operator gates** — count by kind (approval / interaction / credential).
  The headline absurdity number.
- **Token cost** — sum of `llm exchange` input+output, and *system-prompt
  share* (fixed cost per turn; the prompt-burden work #1084–#1089 should
  have moved this).
- **Turns & wall time**; child-session count (delegation shape).
- **Failure classes** — from typed failure fields (#1098), not prose.
- **LoopGuard activity** — any trip is a case-relevant finding.

Cross-case rollup: for each classic case, the one-line verdict
`functional? / gate count / tokens / unique artifact`. The study concludes
(§6) when all six have that line.

## 5. Demo build plan (parallel track)

While the operator runs cases 3–6 manually, demos get built under
`smoke/`, one per case, mirroring the yfinance-factory harness
(config-pinned gateway, auto-resolver for gates, `verdict.py`-style report):

| Demo | Gates auto-resolved? | Delta vs manual |
|---|---|---|
| `smoke/research-brief` | yes (fetch approvals) | asserts source count + stored artifact mechanically |
| `smoke/code-edit` | yes (sandbox + promotion) | fixed fixture project; asserts patch-not-rewrite via tool-call counts |
| `smoke/credential-register` | secret pre-seeded via CLI (`agent credential put`) — the *agent* still goes through check→setup→request | asserts secret absence in all stores |
| `smoke/scheduled-heartbeat` | yes (job creation) | asserts tick count + cleanup from causal events |

Case 5's demo pre-seeds the secret because no automated operator should
hold a real key; the manual run exercises the true human-entry gate.

## 6. Conclusion criteria

The study answers "worth continuing, not absurd" if:

1. All six cases complete (bugs found are fine; architecture blocks are not).
2. No case exceeds ~3 operator gates beyond its expected count (§3).
3. At least two cases produce their "notable artifact" with the secret-
   redaction invariant (case 5) holding unconditionally.

Failure of (3) in case 5, or any absurdity check firing, becomes a design
RFC of its own — that is the study paying for itself.
