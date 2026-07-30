# RFC: Data Envelopes — Egress Localization for LLM Context, Memory, and Federation

**Status:** Draft — 2026-07-26 (amended after external expert review, same day;
implementation amendments 2026-07-30) —
implementation tracked in umbrella issue
[mandubian/autonoetic#903](https://github.com/mandubian/autonoetic/issues/903)
(phases: #904, #905, #906, #907, #908, #909, #910).
**Origin:** Operator requirement: an agent may read my emails, but email content must never
be sent to a remote LLM — while in the *same session* a remote LLM may still write the
script that processes those emails, and a *local* LLM may summarize and analyze them.
Credentials already enjoy this protection by architecture (vault references, never
serialized into context); this RFC generalizes that property into a first-class,
gateway-enforced concept: the **data envelope**.

**Related:** `docs/credential-management.md` (vault, `credential_env` injection),
`docs/rfc/llm-preset-inference-profiles.md` (preset registry, routing),
`docs/approval-system.md` (session approval grants, declassification precedent),
`docs/constitution/versions/2026.07.19/constitution.md` (§14 Lawful-Executor, I-6, I-11),
`autonoetic-types/src/disclosure.rs` (DisclosureClass — the *inward* complement).

**Decisions so far** (operator, 2026-07-26):

- Default label for unlabeled *locally-originated* content: `unrestricted` (one-line
  config flip to tighten). Third-party content is the exception: remote MCP results
  default `no_remote_model` (§4.5) and inbound OFP messages fail closed (§7).
- `local_only` includes `MemoryPersist` — durable *labeled* memory beats forcing a
  choice between memory and privacy.
- Pinned-preset conflict with taint: gateway **asks the operator inline**
  (approval-shaped prompt), never silently downgrades, never hard-refuses (§5.3).
- First-touch classification (`prompt_once`, §4.4): **kept in the design, deferred** —
  not scheduled in phases 1–2; revisit once real operator usage is visible.
- Compartments (§5.5): a **usage pattern**, not new machinery; **no auto-spawn**;
  data-owner shape anchored on session residency (PR #902 — open, see §5.5
  dependency note); stateful-singleton taint accumulation recorded as a decision
  criterion for #686.
- Traceability is a first-class deliverable (§9): named causal events, the filtered
  wire view as evidence, and a five-question introspection acceptance bar.

**Expert-review amendments** (2026-07-26, accepted):

- **Context compression is a first-class egress and a label-destroying merge** —
  new §5.7; per-label-band compression + compression-preset eligibility are phase
  1–2 scope, not an afterthought.
- **The `LocalAgent` hole is closed in phase 2**, not phase 4: label propagation
  onto spawn-return values and `agent_message` payloads moves to phase 2
  (§5.5, §12).
- **Envelope ↔ message binding is specified** (§3.4): message-level granularity,
  stable message ids, sidecar keyed by id, label preservation as a requirement of
  every history transform.
- **Memory is not the only cross-session re-entry path** (§6): all stored-content
  query surfaces (`execution_search`, `digest_query`, `observability_read`,
  `session_peek`, `wiki_get`) get label columns and provider-class filtering.
- Terminology: the operation is **`restrict` / `intersect`** everywhere (lattice
  meet) — never `join`, which invites inverted-lattice bugs.
- `secret` is **not an envelope label** — the vault path never creates envelopes
  (§3.2).
- Fail-mode for outbound-assertion violations: `emergency-stop` / `refuse-turn`,
  not `refuse-session-start` (§13).

**Implementation amendments** (2026-07-30, landed with phases 1–4 / #904–#909):

- **Boundary surfaces are an open set, not a list** (§7): any surface that moves
  session-derived bytes off-machine gates on taint before send and emits
  `egress.boundary_refused`. `web` and `hooks` joined `ofp`/`mcp`/`sandbox`, and
  compression-band refusals (§5.7) use the same event with
  `surface: "compression"`.
- **The `unrestricted` default covers locally-originated content only** (see
  decisions above): inbound OFP messages with a missing/unparseable label are
  ingested as `FederatedAgent`-tainted, never `unrestricted` — the
  launder-through-an-unlabeled-peer path is closed (§7).
- **Boundary declassification is host-scoped** (§8): approving a network action
  under taint materializes `session:<root>:host:<host>` grants — one per host the
  operator saw, disclosed in the approval prompt, revocable per host — never a
  silent session-wide widen. Session-wide `session:<root>` requires an explicit
  `EgressDeclassify` request.
- **`egress.declassified` payload** (§9.1): `expires_at` added; operator identity
  is not duplicated into the payload (it joins through `source_approval_id`);
  revocation is recorded on the grant row and rides the `grant_revocation`
  causal event.
- **`provider_constraint` (§5.4 rung 1) is implemented** and is the liveness pin
  for resident data-owners (§5.5).

---

## 1. Problem

Everything an agent reads — tool results, file contents, `web.fetch` bodies, sandbox
stdout, recalled memories — is flattened into `history` and shipped verbatim to
whatever provider the preset resolves to. Today the only data protected from that
path is vault credentials, and they are protected by *never entering context at all*
(reference-based injection, `runtime/tools/sandbox.rs:2162-2213`). For everything
else there is no answer to "may this content leave the machine?":

- **LLM egress is unfiltered.** `CompletionRequest` is assembled at
  `runtime/lifecycle.rs:2598` from `sanitize_history_for_request`
  (`runtime/prompt_budget.rs:81`), which performs token-budget transforms only. No
  locality check exists anywhere in `src/llm/`.
- **Context accumulates.** History is append-only across turns; a sensitive read on
  turn 3 is still present on turn 40. There is no mechanism to withhold *part* of
  the accumulated context from a given provider.
- **Failover re-ships the transcript.** The cross-provider failover loop
  (`lifecycle.rs:2748-2871`) rebuilds a driver per fallback preset
  (`lifecycle.rs:2787`) and re-sends the *same* request to a *different* provider —
  including from a local preset to a remote one.
- **Context compression is a second, hidden LLM egress.** `compress_context`
  (`runtime/compression.rs:239`) ships the whole history to a separately resolved
  preset (`resolve_compression_llm_config`, `compression.rs:45`) and returns a
  rewritten history — today with no locality check, and structurally hostile to
  per-envelope labeling (see §5.7).
- **Memory is an unlabeled side door.** `knowledge.store` defaults to
  `visibility: global` (`runtime/tools/knowledge.rs`), recall hits are appended
  verbatim to the system prompt (`runtime/context.rs:182`,
  `lifecycle.rs:1367-1370`), and `post_session_digest` ships session content to the
  digest LLM. Anything sensitive that lands in Tier-2 memory can re-enter any later
  session's provider-bound context, cross-session, with no screening. And memory is
  not the only re-entry path — `execution_search`, `digest_query`,
  `observability_read`, `session_peek`, `wiki_get` all pull stored content into
  provider-bound contexts; `execution_traces` stores *full untruncated stdout*.
- **Curators amplify it.** `memory-curator.default` distills completed sessions into
  `global` knowledge entries (`scope: evolution/patterns`) and graduates lessons into
  SKILL.md instructions. A curator that reads tainted session material today
  republishes it globally by design.
- **Other surfaces are unscreened:** OFP `AgentMessage` text crosses to peer gateways
  in cleartext (`server/router.rs:300-315`); remote MCP servers (SSE) bypass the
  approval machinery entirely (`tool_call_processor.rs:547-562`); once a sandbox has
  `share_net`, grant targets are matched at approval time only, never enforced at the
  network layer.

The existing `DisclosureClass`/`DisclosurePolicy` system
(`autonoetic-types/src/disclosure.rs`) points the other way: it filters the assistant
reply *to the user* and observability viewers. Nothing filters what goes *upstream*.

### 1.1 The scenario that must work

One session. The operator refuses to let email content reach any remote provider.

1. Remote LLM (e.g. `sonnet`) is asked to **write a script** that parses emails.
   It sees the code conversation. It never sees an email. ✅ must keep working.
2. The script runs in the sandbox and **reads the emails**. Output is local-only.
3. A **local** LLM (e.g. an `ollama` preset) **summarizes** the emails. ✅ must work.
4. The summary is derived from local-only data → it is itself local-only. Asking the
   remote LLM about "the summary above" must not leak it.
5. Memories distilled from this session must carry the taint so a *later* session on a
   remote preset doesn't recall email-derived content into a remote request.
6. The context governor may fire at any point in this session — compression must not
   be the moment the guarantee silently breaks (§5.7).
7. After the fact, the operator can answer "what left my machine, and why was each
   piece allowed?" from the audit trail alone (§9).

And it must work **without operator expertise**: nobody should have to write YAML
lattices or flip providers per turn to get it.

---

## 2. Design principles

### 2.1 Separation of Powers — unchanged

Labels are **declared metadata, manipulated only by the gateway**. Agents never set,
strip, or read labels; the middleware pre-hook (`lifecycle.rs:2652-2665`) and any
manifest-declared script operate on content, not on the label plane. This is required
by the Lawful-Executor invariant (§14, I-10): enforcement must be a deterministic
function of declared inputs — never LLM-inferred content classification.

### 2.2 Fail closed

- A provider with no explicit locality classification is **remote**.
- A routing/fallback candidate that would receive content it is not cleared for is
  **not a candidate**.
- Widening a label happens only via operator declassification (§8).

(Per decision, the *default label* for unlabeled content is `unrestricted` for now —
fail-closed applies to the mechanisms, not to the default corpus. Flipping the
default later is a one-line config change.)

### 2.3 Withhold, don't poison

Per-envelope granularity: reading one email must not make the whole session
remote-unsafe. The gateway *substitutes a non-divulging indication* for withheld
envelopes, so the remote model keeps a coherent (if incomplete) context and the
session remains usable for clean work — scenario step 1 above.

### 2.4 Monotonic taint

Derivation can only restrict, never widen. The label of any output is the
**intersection of its inputs' allowed-sink sets**. Terminology matters: labels form
a lattice in which restriction is the *meet*; the operation is named
`restrict`/`intersect` in code and prose — never `join`, which in lattice terms
means *widening* and invites inverted code. The only way to widen a label is an
explicit, operator-approved **declassification** (§8), causal-logged.

### 2.5 Label the exceptions, not the corpus

Because the default is `unrestricted`, the operator's whole job is naming what is
private — one rule per sensitive source ("emails stay local"), never a
classification program. Operator policy is a small set of **source rules** (§4.2);
everything else flows by default. This is the same mental model as firewall rules,
and the same shape as the existing `DisclosureRule` — but owned by the operator
(gateway config / session spawn), not by agent bundles, because privacy policy is
deployment-specific like `llm_presets`, while bundles are shared, content-addressed
revisions.

---

## 3. The Data Envelope

### 3.1 Type

```rust
// autonoetic-types/src/egress.rs (new)

/// A sink class: a place data can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sink {
    LocalModel,       // provider classified `local` (ollama/vllm/lmstudio/llamacpp, explicit)
    RemoteModel,      // provider classified `remote` (default for anything unclassified)
    LocalAgent,       // another agent session on this gateway
    FederatedAgent,   // a peer gateway over OFP
    Network,          // sandboxed code with share_net, web.call bodies, remote MCP args
    MemoryPersist,    // durable memory (Tier-1 state/, Tier-2 SQLite) — cross-session
    UserReply,        // the assistant reply to the operator (bridges DisclosureClass)
}

/// Set of allowed sinks. Restriction = intersection (`restrict`/`intersect`);
/// labels form a meet-lattice under restriction — never call this `join`.
pub type EgressLabel = BTreeSet<Sink>; // or a bitflags newtype

pub struct DataEnvelope {
    pub id: String,                 // env_<ulid> — referenced by causal events
    pub label: EgressLabel,
    pub provenance: Provenance,     // source tool + args digest, session, artifact id,
                                    // matched rules, parent envelope ids (§9.1)
    pub indication: Indication,     // safe substitute when withheld (§3.3)
    pub content: EnvelopeContent,   // the payload (text/bytes handle)
}
```

### 3.2 Predefined labels

| Name | Allowed sinks | Use |
|---|---|---|
| `unrestricted` | all | Default for ordinary workspace content. |
| `local_only` | `{LocalModel, LocalAgent, UserReply, MemoryPersist}` | Emails, personal files, anything the operator refuses to ship. (MemoryPersist included per decision — durable labeled memory is allowed.) |
| `no_remote_model` | all except `RemoteModel`, `FederatedAgent` | Business-confidential but federatable. |

Custom labels are just sink sets; the predefined names exist for manifests and config
ergonomics. **Labels attach to envelopes, not to providers or sessions** — providers
get a *classification* (§5.1), sessions get a *policy* (§5.4).

**Credentials are not an envelope label.** There is deliberately no `secret` row in
this table: the vault path never creates envelopes — credential values never enter
agent context at all (reference-based `credential_env` injection straight into the
sandbox process env), so there is nothing to label. Envelope labels do **not** govern
`credential_env`; that path stays governed by `allowed_hosts` and sandbox network
policy, and its residual exposure (injection into a `share_net` sandbox can
exfiltrate over the network) is documented in §11 rather than hidden behind a
vacuous `{}` label.

### 3.3 Indications — withholding without divulging

When an envelope's label excludes the target sink, the gateway replaces its content
in the outbound payload with an **indication**:

```
[withheld: 2× email.read results — policy local_only]
```

Rules:

- Generated by the gateway from **provenance metadata only** (tool name, count,
  label name) — never from content. A tool manifest may declare an
  `egress.indication` template, but it is interpolated with metadata, not content.
- Verbosity is configurable: `terse` (`[content withheld]`) for maximally private
  deployments, `descriptive` (default) for model coherence.
- Every substitution emits a causal event `egress.envelope_withheld` carrying
  envelope ids, target sink, and label — never content (I-6 attribution; see §9).
- Indications are part of the *wire transform*, exactly where
  `sanitize_history_for_request` already lives: history on disk keeps full content;
  each outbound request gets its own filtered view.

### 3.4 Envelope ↔ message binding

`Message` is `{role, content: String, tool_calls}` — there is no envelope inside it,
and this binding is the crux of the implementation, so it is specified here rather
than left to the implementer:

- **Granularity: whole messages.** Each `Message` in history maps to at most one
  envelope. Sub-message spans (a paragraph tainted inside an otherwise clean
  message) are **out of scope** — a tool result is labeled as a unit, an assistant
  message as a unit.
- **Binding: sidecar keyed by stable message id.** Every message committed to
  history gets a stable `msg_<ulid>` at commit time (the tool-result commit point
  is `lifecycle.rs:4019-4026`); the envelope map lives alongside history keyed by
  that id — **never by index**. Index-keyed bindings break on the first history
  transform.
- **Every history transform must preserve labels.** `sanitize_history_for_request`
  collapses duplicate tool results to markers and truncates values
  (`prompt_budget.rs:75-81`); compression replaces spans wholesale (§5.7).
  Transforms operate on cloned/derived messages and must propagate the source
  message ids (or compute explicit new labels for synthesized messages, as
  compression does). A transform that cannot account for a message's label is a
  bug, caught by a dedicated test: run sanitize + compress over a labeled history
  and assert every label survives.

---

## 4. Where envelopes are born (and how operators declare sources)

### 4.1 Label resolution = intersection of all applicable declarations

An envelope's label is the **intersection** (`restrict`) of every declaration that
applies — any matching rule can only restrict, so evaluation order doesn't matter
and there is no "first match wins" subtlety:

1. **Operator source rules** (§4.2) — gateway config + session-scoped additions.
2. **Bundle-declared floor** — a tool's SKILL.md may declare
   `metadata.autonoetic.egress.output_label` (e.g. the email bundle ships
   `email.read → local_only`). A floor: the bundle can restrict its own outputs,
   it can never widen what operator policy restricted.
3. **Argument taint** — a tool called with a tainted argument produces tainted
   output (intersection of argument labels).
4. **Session policy default** (§5.4) — for content nothing else labels.
5. **Configured fallback** — per-tool-class defaults; unknown → `unrestricted`
   (current decision; flip to `no_remote_model` later if desired).

Widening never happens through this resolution — only through §8 declassification.
Every resolution is recorded as an `egress.envelope_labeled` causal event with its
inputs (§9.1) — "why is this labeled?" is always answerable.

### 4.2 Operator source rules

```yaml
# gateway config.yaml — operator-global
egress:
  rules:
    - { source: "email.*", label: local_only }
    - { source: "mcp.gmail.*", label: local_only }
    - { source: "fs.read", path: "~/mail/**", label: local_only }
    - { source: "sandbox.exec", path: "~/mail/**", label: local_only }
  unclassified_source_mode: unrestricted   # | prompt_once | local_only
```

- `source` matches tool names (`email.*`, `mcp.<server>.<tool>`); `path` narrows to
  filesystem paths for path-taking tools.
- **`sandbox.exec` rules are enforced by static analysis**: the command and its
  script dependencies are scanned for labeled paths at exec time — a direct sibling
  of the existing `RemoteAccessAnalyzer`
  (`runtime/remote_access.rs:786`), same "analyze before execute" pattern. A match
  labels the exec's stdout/stderr envelope. This is what keeps scenario step 2 safe
  when the *script*, not a structured tool, does the reading.
- Session-scoped additions ride in the session policy (§5.4) and die with the root
  session.

### 4.3 Intent as authoring aid, never as enforcement input

Operators don't write the YAML by hand in the common case. At session start (or in
the session room): *"emails stay local"* → the gateway **proposes** the concrete
rule set above (from known tool catalogs, MCP server list, and path conventions) →
operator confirms with one keystroke → the confirmed rules are the declared input.
The natural language is only an authoring convenience; what gets enforced is the
explicit, operator-confirmed rule. Lawful-Executor (§14) is preserved: enforcement
remains a deterministic function of declared inputs.

### 4.4 First-touch classification (deferred)

`unclassified_source_mode: prompt_once` enables a browser-permission-style flow: the
first time a session touches a source no rule covers, the gateway asks
"allow remote / local-only?" via the existing approval machinery (flood-capped per
P-7.17's shape), and the answer can be persisted as a rule. **Status: kept in the
design, deferred** — not built in phases 1–2 (per decision; operator usage patterns
are not yet clear enough to schedule it). With the decided default
(`unclassified_source_mode: unrestricted`), the mode is simply off.

### 4.5 Other birth points

- **LLM response** — label = intersection of the labels of all envelopes included in
  that request. A remote model that only ever saw clean envelopes produces clean
  output; a local model that saw `local_only` input produces `local_only` output.
  This makes scenario step 4 (the tainted summary) automatic.
- **User/operator message** — session policy default; the operator can mark a
  message or the whole session.
- **Memory recall hit** — the label stored on the `MemoryObject` (§6).
- **Artifact read** — label in the artifact manifest sidecar
  (`artifact_store.rs` manifest already carries metadata; add `egress_label`).
- **MCP tool result** — from the server classification; remote servers' results
  default conservative (`no_remote_model`) unless a rule says otherwise — their
  content came from a third party, but it may quote anything it was given.

Labels persist: history checkpoints serialize envelopes with labels, so suspend /
resume / restart / continuation keep them. (`LoopGuard` checkpoint construction sites
must be updated per the AGENTS.md pattern when the checkpoint format changes.)

---

## 5. Context assembly: merge, withhold, route

### 5.1 Provider classification

Explicit, in `llm_presets` / provider defaults (`autonoetic-types/src/config.rs`,
`llm/provider.rs:174`):

```yaml
llm_presets:
  sonnet:  { provider: anthropic, model: ..., egress_class: remote }   # default if absent
  local:   { provider: ollama, base_url: http://127.0.0.1:11434/v1, egress_class: local }
```

`egress_class` defaults to `remote` (fail closed). `provider_defaults`
(`provider.rs:324-352`) pre-marks `ollama`/`vllm`/`lmstudio`/`llamacpp` as `local`,
overridable — a *remote* Ollama server is a real deployment shape.

### 5.2 Request-time filtering (the chokepoint)

At `lifecycle.rs:2598` / immediately before `driver.complete()`
(`lifecycle.rs:2720`), or equivalently as a policy-wrapping `LlmDriver` installed by
`build_driver` (`llm/mod.rs:720`):

1. Resolve target provider → its `egress_class` → the `Sink` it represents.
2. Walk the assembled message list; for each envelope whose label excludes that sink,
   substitute its indication (§3.3), using the §3.4 id-keyed sidecar.
3. **Outbound content assertion (defense in depth):** the gateway still holds the
   withheld content, so the wrapper can assert no withheld envelope's payload appears
   verbatim in the serialized request body. A hit is a bug or an attack — fail the
   call, emit `egress.assertion_violation`, emergency-stop per fail-mode table. This
   catches exfiltration-by-echo (sandbox stdout replaying labeled input, an agent
   copy-pasting a secret into its own message) without any content classification.
   Cost and reach are bounded — see §11; it is a tripwire, not a proof.
4. Emit `egress.request_filtered` with counts.
5. Record the **filtered wire view** as trace evidence (§9.2) — the exact body sent,
   so "what left my machine at turn N?" has a direct answer.

Because filtering is a pure function of (envelopes × provider class), it composes
with the existing sanitize/budget pass and applies identically to streaming
(`stream()` default impl) and to **every fallback driver** in the failover loop —
closing the local→remote failover leak for free. The §5.3 eligibility rules apply
to fallback candidates identically to routing candidates (phase 2).

Two audit obligations ride with the chokepoint:

- **Gateway-authored strings.** Repair prompts (`response_validation.rs:762`) are
  built from canned `ValidationViolation` strings — safe today — but any call site
  that interpolates content into `message`, and tool error strings embedding
  stderr, are potential unlabeled content channels. One audit pass over
  gateway-authored strings is part of phase 1.
- **Canary test.** A tainted session run against a mock remote provider must never
  have a canary marker appear in any captured wire body — this turns §5.2.3 from
  an assertion into a proof over the whole path (filtering + transforms +
  serialization).

### 5.3 Taint-following routing — mixed sessions without mode-switching

The mixed session hides a routing problem: once emails are in history, which
provider handles the *next* turn? Making the operator flip presets per turn is
unacceptable complexity; letting an LLM decide is a discretion leak. It resolves
deterministically:

- For each completion, compute the **intersection of the labels of the envelopes
  added since the last completion** (the new tool batch + the new user message).
- A preset is eligible for that completion iff its `egress_class` covers that
  intersection. The routing strategy (fixed preset, `routing:` preset, fallback
  chain) picks among **eligible candidates only** — and this applies to the
  **failover chain identically**: a fallback preset that is not eligible for the
  current batch is skipped, so a tainted turn never fails over into an
  all-indications remote context (which would be worse than refusing).
- Older history is always handled by §5.2 filtering (indications), regardless of
  which eligible preset runs.
- Every selection emits `egress.provider_selected` (§9.1): eligible set, chosen
  preset, batch intersection, fallback skips, inline-ask outcome — "why did this
  turn run on this provider?" is always answerable.

Consequences, with no operator intervention:

- Code-generation turns produce clean batches → remote presets eligible → remote.
- The turn whose sandbox exec read `~/mail/**` produces a `local_only` batch → only
  local presets eligible → the reasoning-over-emails completion runs locally, and
  its response intersects to `local_only` (§4.5).
- The next clean code turn routes remote again; the email output and the summary
  appear to it as indications.

If no eligible preset exists for a tainted batch (no local preset configured), the
turn refuses with `egress_no_eligible_provider` and surfaces the operator choice:
configure a local preset / declassify specific envelopes (§8) / abort.

A pinned preset (agent `llm_preset`, session override) acts as a constraint. If the
pin conflicts with the taint (pinned remote, tainted batch), the gateway **asks the
operator inline** — an approval-shaped prompt offering: declassify these envelopes /
run this turn on local preset X / abort — and causal-logs the choice (decided
2026-07-26). It never silently downgrades (a discretion leak — the Ri-0.6-analogue
from the inference-profiles RFC applies to egress downgrades too) and never
hard-refuses without a path forward (a dead end for non-experts). **Status
(implementation amendment, 2026-07-30):** pinning itself works (the pinned preset
is the primary), but the pin × taint **conflict** path is not yet implemented —
the routing plane does not detect that the primary was pinned, and preset
eligibility does not yet consult declassification grants, so a `RemoteModel`
declassify grant does not unblock a refused turn. Today the gateway reroutes to
an eligible local preset automatically (covering the "run on local" option) or
refuses with a path forward. Wiring grant-aware eligibility + the declassify
offer is the remaining §5.3 slice.

Two session modes tune the behavior (§5.4): `withhold_and_proceed` (default; above)
and `require_full_context` (a turn that *references* withheld envelopes is refused
rather than sent a filtered view — for workflows where a summary-of-an-indication is
a worse failure than a refusal).

### 5.4 Session policy — the granularity ladder

The operator (or a plan approval) declares per root session:

```yaml
egress_policy:
  default_label: unrestricted        # for unlabeled content
  mode: withhold_and_proceed         # or require_full_context
  provider_constraint: any           # or local_only — whole session pinned to local presets
  indication_verbosity: descriptive
  rules: []                          # session-scoped source rules (added to §4.2, die with session)
```

Three rungs, simplest first — the operator picks the coarsest one that fits:

1. **Whole session private** — `provider_constraint: local_only` (+ optionally
   `default_label: local_only`). One flag: "this room is private." Provider
   *selection* itself is constrained, not just content. Composes with session-room
   UX (`docs/rfc/session-room-channel-agnostic-timeline.md`): a room marked private.
2. **Named sources private** — session `rules` (§4.2): the email scenario. Code
   generation stays remote; named sources stay local; taint-following routing (§5.3)
   moves turns between providers automatically.
3. **Single envelope** — ad-hoc "this one message/artifact is private" — the inverse
   of declassification, same approval-shaped interaction.

### 5.5 Compartments — session boundaries as taint firebreaks (a pattern, not machinery)

A "compartment" is **not a new subsystem**: it is an ordinary session boundary used
deliberately. Autonoetic already isolates history per session — when a parent spawns
a child agent, the child's tool calls, raw reads, and intermediate reasoning live in
the *child's* history; the parent's history only contains the spawn message and the
child's return value. Envelopes make the privacy accounting explicit: the return
value crosses back as **one labeled envelope** (label = intersection of what the
child touched), instead of the parent's context accumulating every raw read.

There is therefore **no fork and no two flavors to maintain**: the only code the
label plane needs is (a) labels intersecting onto spawn-return values, (b) labels
onto `ecosystem.send_message` payloads, (c) `provider_constraint` per session. Items
(a) and (b) are **phase 2 scope** — until they land, `LocalAgent` in `local_only`
would be a hole: a tainted session could hand content to a remote-pinned sibling
with nothing carrying the label. The two shapes below then fall out of lifecycle
mechanics that already exist, and choosing between them is a per-task
operator/planner decision.

1. **Task-scoped child** (spawn): *"analyze my whole mail archive"* → the parent
   spawns a mail specialist. The child reads thousands of emails; taint-following
   routing keeps its completions local automatically. The parent receives one
   `local_only` report envelope. Compare with doing the same job in the main
   session: every `email.read` lands in the main history, every later remote turn
   ships a growing pile of indications, and everything the session derives
   afterwards risks intersecting the taint. The compartment keeps the parent's
   taint surface at exactly what it needs — and the raw material dies with the
   child session. Best for **one-shot bulk jobs**: no lifecycle to manage, no
   permanent taint accumulation.
2. **Data-owner agent** (resident): a long-lived "mail agent" owns the sensitive
   source; other agents query it via `ecosystem.send_message` and receive
   `local_only`-labeled answers — just session policy + source rules applied to one
   agent. **Dependency: session residency (PR #902, open at time of writing —
   `agent.resident_idle_ttl_secs`, `session_residency`).** On clean completion the
   session parks (`YieldReason::Idle` checkpoint + `session_residency` row) instead
   of dying, stays addressable, and an inbound message resumes it through the
   notification pump. Before residency this shape is not viable — a data-owner dies
   with its task, and messaging has nothing to talk to (2 messages sent, 0 consumed
   across one real gateway's lifetime). For **standing personal data sources**
   (mail, health, finance) this is expected to become the default posture: access,
   labeling, and audit centralize in one agent whose sessions are pinned
   `provider_constraint: local_only`, and its accumulating history is a feature —
   it builds up context about the source, all `local_only` by design. The pin is
   **liveness**, not just safety (implementation amendment): taint-following
   routing decides per *batch*, so a resident owner whose accumulated history is
   tainted can still route a clean inbound `agent_message` turn remote — and
   answer from an all-indications context. No leak, but the owner stops
   functioning as an owner. `provider_constraint` (implemented, §5.4 rung 1)
   restricts provider *selection* itself, clean batches included.

Two precision notes on the data-owner shape:

- **Residency is continuity, not durability.** TTL reaping eventually closes the
  parked session; the owner's long-term knowledge lives in Tier-2 memory *with
  labels*, and a post-reap message spawns a fresh session that recalls them. The
  label plane is the correctness layer; residency is a latency/continuity
  optimization. (Known #902 gap: a gate-suspended session is not addressable;
  queued messages wait for the gate to resolve.)
- **Labels alone make it a soft boundary.** Another session can still read
  `~/mail` directly — source rules label the results, so nothing reaches a remote
  sink, but access is not centralized. The **hard boundary** adds capability
  confinement: only the data-owner bundle holds `ReadAccess` over the mail paths,
  and other bundles' scopes exclude them. Messaging the owner is then the *only*
  way to touch the data — a true chokepoint in the separation-of-powers idiom, and
  the recommended posture for genuinely sensitive standing sources.

**No auto-spawn** (decided 2026-07-26): the per-turn provider problem is already
handled by taint-following routing (§5.3), and the ergonomics of automatic
compartment creation are unproven. Compartments stay a manual pattern; revisit with
real usage evidence if over-tainting hurts in practice.

**Stateful singletons (#683 / #686) — what they change for data.** Singleton dedup
(#685) gives exactly-one-session-per-agent; residency (#902) keeps it addressable;
stateful singletons (#686, deferred) would make subsequent spawns arrive as
`agent_message` calls into the *same accumulating session*. For the label plane
this cuts both ways:

- A stateful **data-owner** is the ideal shape: pinned local, everything it
  accumulates is tainted by design, and its identity is the audit chokepoint.
- A stateful singleton in a *remote-reasoning* role degrades: once any task taints
  its accumulated context, taint-following routing makes it local-eligible forever
  after. This adds a decision criterion to #686 beyond the context-contamination
  concerns already recorded there: **taint accumulation**. Data-owner roles want
  stateful + local; reasoning roles (architect, evaluators — already flagged for
  contamination) want fresh sessions per task.

**Ownership vs. versatility — access and flow are orthogonal.** This RFC does not
make singletons the sole owners of their data, and does not need to. Visibility
(`MemoryVisibility` private/session/global) controls *who may read*; egress labels
control *where data may flow afterward*, regardless of who read it. A `local_only`
memory readable by every local agent still cannot reach a remote provider,
whichever agent recalls it. So the versatile shared-tier model stays the default
substrate, and singleton ownership is a **per-source hardening posture** (capability
confinement, above) used where centralized access and audit are worth the
bottleneck — decided per source, not per system. The pragmatic middle for a
data-owner: it holds *raw* source access (private, confined) and **publishes
derived artifacts** (summaries, indexes) into shared tiers with intersected labels —
siblings consume derived data directly; only raw/new queries go through
`agent_message`. Publishing is egress-safe by construction (labels travel);
the residual trust question — which derivations are OK to share with siblings — is
LLM judgment at the local boundary, bounded by the fact that flow control still
applies to whatever was shared. One genuine new risk: `agent_message` requests are
untrusted sibling input, so a data-owner singleton is a confused-deputy surface;
its replies stay label-constrained, capping the blast radius at local
over-disclosure.

### 5.6 Worked example — the mixed session, end to end

1. Session start, operator: *"emails stay local"* → gateway proposes
   `{email.* → local_only}`, `{fs.read, ~/mail/** → local_only}`,
   `{sandbox.exec, ~/mail/** → local_only}` → operator confirms. (If the email
   bundle already declares its floor, even this is skipped.)
2. *"Write a parser script for my mailbox export."* → clean turn → remote `sonnet`.
   No email content exists in context; nothing withheld.
3. Agent runs the script via `sandbox.exec`. Static analysis matches `~/mail/**` in
   the script → the stdout envelope is `local_only`
   (`egress.envelope_labeled`: matched rule `sandbox.exec:~/mail/**`).
4. Next completion: the new batch is `local_only` → taint-following routing makes
   only the local `ollama` preset eligible (`egress.provider_selected`: eligible
   `[local]`, chosen `local`, batch intersection `local_only`) → the model
   summarizes locally. Its response intersects to `local_only`.
5. *"Now add error handling to the script."* → clean batch → remote again
   (`egress.provider_selected`: eligible `[sonnet, …]`, chosen `sonnet`). The
   request contains the full code conversation plus
   `[withheld: 1× sandbox.exec result — policy local_only]` and
   `[withheld: 1× assistant message — policy local_only]` where the emails and the
   summary were (two `egress.envelope_withheld` events). Remote never sees the
   content; the code work continues.
6. Session ends: `post_session_digest` sees tainted envelopes → runs on the local
   preset; digest memories are stored `local_only` (§6). A later remote session
   recalling them gets indications, not content.
7. At any point: if the context governor fires, compression runs **per label band**
   on eligible presets only (§5.7) — the clean band may compress remotely, the
   tainted band only locally; no single mixed summary is ever produced.
8. Afterwards: `gateway egress audit <session>` (§9.3) renders every step above —
   per-turn provider, withheld envelopes, labeling provenance, wire views — without
   anyone having taken notes.

### 5.7 Context compression is an egress too — and a label-destroying merge

`compress_context` (`runtime/compression.rs:239`) takes the whole history, ships it
to a separately resolved preset (`resolve_compression_llm_config`,
`compression.rs:45`), and returns a rewritten history. In the template config that
preset resolves to a *remote* model. Two distinct rules are required:

1. **Eligibility:** the compression preset must be eligible for the band it
   compresses. Compressing `local_only` history on a remote preset is a leak *even
   if every envelope is individually filtered* — the purpose of the call is to
   transmit that content. This is the same rule as §5.3, applied to the
   compression call site; the chokepoint wrapper alone is not sufficient, because
   an all-indications compression input would produce a useless summary.
2. **Per-label-band compression:** clean and tainted messages are compressed in
   **separate bands**, producing separate summary blocks, each labeled by the
   intersection of its band's inputs. Compressing a mixed history into one block
   would label the entire block `local_only` by intersection — the over-tainting
   cascade §11 warns about, *guaranteed* rather than incidental, permanently
   defeating the per-envelope withholding that makes mixed sessions viable.

Compressed blocks are new envelopes: label = intersection of the band, provenance
records the compression event and source message ids (§3.4's transform-preservation
requirement applies — a compressed block is a synthesized message with an
explicitly computed label; `egress.envelope_labeled` carries the band membership so
the summary's lineage is queryable). If no eligible compression preset exists for
the tainted band, the governor falls back to token-budget truncation/dropping for
that band rather than compressing it remotely — an incomplete local context beats
a remote leak. The refusal is auditable: it emits `egress.boundary_refused` with
`surface: "compression"`, the band label, the source message ids, and the chosen
fallback (implementation amendment — §5.7 named the fallback but not the event).

---

## 6. Memory, digest, curators — and all stored-content re-entry paths

Memory is where localization survives across sessions — and where it is easiest to
lose. Rules:

1. **`MemoryObject` gains `egress_label`** (`autonoetic-types/src/memory.rs`,
   alongside `MemoryVisibility`). Visibility governs *which agent* may read; the
   egress label governs *where the content may go after it's read*. Orthogonal, both
   enforced.
2. **`knowledge.store` label** = intersection of (explicit label arg if the
   *gateway* supplies one, labels of session content the stored text derives from).
   In practice: the gateway intersects the session's accumulated taint at store
   time. `visibility: global` stays allowed — but the label travels with the record.
3. **Recall filters by target provider class.** `build_memory_context_snippet`
   (`runtime/context.rs:182`) already ranks candidates; it now also drops (or
   substitutes indications for) memories whose label excludes the provider the
   snippet is being built for. Since the snippet is built per session before the
   provider is finalized, the filter must run at request time alongside §5.2, or the
   snippet builder must receive the resolved provider class.
4. **`post_session_digest`** is itself an LLM egress: if the session contains
   `local_only` envelopes, the digest job must run on a local preset, or filter with
   indications. The resulting digest memory inherits the intersection of what it
   read.
5. **Curators** (`memory-curator.default`, digest-scoped jobs): a curator's stored
   learnings inherit the intersection of everything it read — if the curator runs
   on a remote preset, `local_only` sources appear to it only as indications, which
   mechanically prevents tainted distillation. `promote_to_skill` graduations write
   into SKILL.md *instructions*; a graduation whose evidence derives from
   `local_only` material is refused unless declassified, because SKILL.md is a
   broadly distributed artifact.
6. **Memory is not the only cross-session re-entry path.** `execution_search`,
   `digest_query`, `observability_read`, `session_peek`, `wiki_get` all pull stored
   content into provider-bound contexts. `execution_traces` in particular stores
   *full untruncated stdout* — the exact output of the tainted `sandbox.exec` in
   scenario step 2. All stored-content surfaces get a label column (written with
   the same store-time intersection as memories) and are filtered/indicated by
   target provider class at query time, exactly like recall.
7. **Migration:** existing stored content is unlabeled → treated as the configured
   default, and an operator sweep tool (`gateway memory relabel`) reclassifies in
   bulk; every sweep emits an `egress.relabel` audit event (§9.1). Fail-closed
   option: treat legacy unlabeled records as `no_remote_model` until swept.

---

## 7. Other egress surfaces

LLM context is phase 1, but the label plane is designed to cover all of them.
The rule is open-ended (implementation amendment): **any surface that moves
session-derived bytes off-machine gates on session taint before send**, and
every refusal emits `egress.boundary_refused` (§9.1) with a `surface` tag —
`sandbox` / `web` / `hooks` / `mcp` / `ofp` / `compression`. The named
surfaces:

- **OFP federation** (`server/router.rs:88`, `server/ofp.rs`): `AgentMessage` gains
  label metadata. The gateway refuses to send an envelope whose label excludes
  `FederatedAgent`; withheld content is replaced with indications before
  serialization. **Inbound is fail-closed:** a missing or unparseable inbound
  label is ingested as `FederatedAgent`-tainted (never `unrestricted`) — the
  launder-through-an-unlabeled-peer path is closed; the outbound wire field
  stays optional for backward compatibility with older peers. Because peers
  enforce the same constitution (P-10.9 digest
  handshake), label semantics become part of the constitutional compatibility
  surface — a peer that doesn't enforce them fails the compatibility profile.
- **MCP** (`autonoetic-mcp/src/client.rs:87-115`): registry entries gain
  `egress_class: local | remote`. Tool *arguments* are intersected from their
  envelope labels; a call whose arguments exclude `Network` (remote server) is
  refused or the tainted arguments withheld — closing the current gap where remote
  MCP bypasses all approval machinery.
- **Sandbox network** (`runtime/tools/sandbox.rs:2122-2138`): a session carrying
  taint that excludes `Network` escalates any `share_net` exec to an operator
  approval even when the manifest declaration passes, and `NetworkCoverage::Unresolved`
  + taint = hard refuse. Full network-layer enforcement (egress proxy) is out of
  scope; documented honestly as a residual gap, as today.
- **Gateway-native web tools** (`runtime/tools/web.rs`): `web_fetch` /
  `web_search` / `web_call` gate on session taint × `Sink::Network` before any
  outbound HTTP — closing only the sandbox would leave exfiltration through
  gateway-owned HTTP.
- **Hook deliveries** (`scheduler/hooks.rs`): `http.callback` deliveries gate
  identically when the delivery is session-derived.
- **Capsules** (`capsule/export.rs`): the export already redacts memory snapshots;
  with labels it instead *includes* memories whose label permits the capsule's
  declared destination and withholds the rest. The dead-code OFP capsule transfer in
  `autonoetic-ofp/src/wire.rs` must not be wired up without label metadata.

---

## 8. Declassification

The only label-widening path. Mirrors session approval grants:

- Operator approves `(envelope-id | source-pattern | memory-id) × sink` — e.g.
  "this one summary → RemoteModel". Scoped, optionally expiring, revocable.
- **Boundary targets are host-scoped** (implementation amendment). At network
  boundaries the refusal fires before any envelope exists, so the natural target
  is the destination: approving a network action (`web_fetch` / `web_call` /
  `web_search` / `sandbox_exec`) under taint materializes
  `source_pattern: session:<root>:host:<host>` grants — one per host the operator
  saw, with the widening disclosed in the approval prompt, revocable per host via
  `gateway grants revoke --host`. A session-wide `session:<root>` grant is never
  materialized implicitly; it requires an explicit `EgressDeclassify` request
  (`gateway egress-declassify`), where the operator chose exactly that breadth.
- Recorded as `egress.declassified` causal event with `enforced_rules` and
  `expires_at`; revocation is recorded on the grant row (`revoked_at`, never
  deleted) and rides the `grant_revocation` causal event. Appears in
  the approval surface alongside existing grants.
- Never inferred, never agent-requested without operator decision, never granted by
  an LLM judgment. Reuses the approval flood cap machinery (P-7.17 shape).

---

## 9. Traceability and introspection

The label plane is only trustworthy if an operator — or an agent, within its
`ViewerClass` limits — can reconstruct *why* the gateway did what it did, after the
fact, without having taken notes. The substrate already exists (causal chain with
I-6 attribution, session tracer, observability tools, `ViewerClass` redaction; the
`curator.decision` journal of issue #30 is the precedent: one queryable event per
decision, so "why was memory X dropped" has a direct answer). This section names
what the egress work must emit. Design rule: **every trace artifact is content-free
metadata** (ids, labels, rule names, counts, indication text) **or content that
already left** — withheld content never appears in any event.

### 9.1 Events

All causal-chained; constitutional ones carry `enforced_rules` (§13).

| Event | Payload (never withheld content) | Answers |
|---|---|---|
| `egress.envelope_labeled` | envelope id, msg id, resulting label, resolution inputs (matched rules / bundle floor / argument-taint parents), parent envelope ids | "Why is this labeled?" — rule provenance + derivation lineage |
| `egress.envelope_withheld` | envelope ids, target sink, label, indication text | "Why was X withheld from this request?" |
| `egress.request_filtered` | provider, preset, counts (included/withheld) | Per-request summary |
| `egress.provider_selected` | turn, eligible presets, chosen preset, batch intersection, fallback skips, inline-ask outcome | "Why did turn 7 run on ollama?" |
| `egress.assertion_violation` | envelope id, provider, request digest | Tripwire (bug or echo attack) |
| `egress.declassified` | grant shape (target × sink), scope, expiry, source approval id (operator identity joins through it); revocation rides `grant_revocation` | "Who widened what, when, until when?" |
| `egress.boundary_refused` | surface (`sandbox` / `web` / `hooks` / `mcp` / `ofp` / `compression`), rule/label, reason; envelope ids where envelopes exist (network-boundary refusals fire before any envelope is born — §8; compression refusals carry band label + source message ids + chosen fallback — §5.7) | "Why was this send/exec refused?" |
| `egress.relabel` | record id, old label, new label, operator | Sweep/manual reclassification audit |

### 9.2 Evidence: the filtered wire view

Per provider request, the session tracer records the **post-filtering request
body** — the exact view serialized to the provider — redacted per the existing
P-4.13 storage rules (`session_tracer.rs:658-686` already redacts before append).
This is the strongest introspection artifact: "show me exactly what left my machine
at turn N" has a literal answer. It is safe by construction: the stored filtered
view contains only content that was actually sent; withheld content exists only in
history (visible locally to Operator/Admin viewers, never in events).

### 9.3 Query surfaces

- `gateway trace` renders egress events inline with turns (withheld markers,
  provider switches, refusals).
- `gateway egress audit <session>` — dedicated report: per-turn provider +
  eligible set, withheld envelopes with indications, labeling provenance per
  envelope, declassifications, assertion violations. Exportable through
  `session_report` / capsule (viewer-redacted as usual).
- **Agent-level introspection:** because events are content-free metadata,
  `ViewerClass::Agent` may see them for its own session via the observability
  tools — this is how an agent explains `egress_no_eligible_provider` or a missing
  context chunk without ever being shown withheld content.
- Retention: egress events inherit the P-8.6 retention policy like other causal
  events.

### 9.4 The five questions (acceptance bar)

The work is traceable iff an operator can answer, from the causal chain + audit
view alone:

1. **What exactly left the machine at turn N?** → filtered wire view (§9.2).
2. **Why was X withheld?** → `egress.envelope_withheld` + the envelope's label.
3. **Why is this envelope labeled `local_only`?** → `egress.envelope_labeled`:
   matched rules + parent lineage.
4. **Why did turn 7 run on this provider?** → `egress.provider_selected`:
   eligible set, batch intersection, fallback skips.
5. **Who declassified what, when, until when?** → `egress.declassified` + grant
   revocation events.

---

## 10. Interaction with existing systems

| System | Relationship |
|---|---|
| Vault / `credential_env` | Credentials are **not** envelopes (§3.2): values never enter context, so there is nothing to label. Injection stays governed by `allowed_hosts` + sandbox network policy; its `share_net` exposure is documented in §11. |
| `DisclosureClass` (disclosure.rs) | Complementary direction: disclosure = inward (to user/viewer), egress = outward (to providers/peers/network). `UserReply` sink bridges them. Keep the enums separate; document the mapping. |
| `store.apply_and_redact` + `DisclosureState` (`tool_call_processor.rs:665`) | The existing secret-value registry becomes one source of the §5.2 outbound content assertion. |
| Approval grants (`GrantTarget`) | Declassification reuses the grant shape; grants remain host-scoped, declassification is content-scoped. |
| `RemoteAccessAnalyzer` (remote_access.rs) | Sibling analyzer for §4.2 `sandbox.exec` path rules: same "scan command + dependencies before exec" pattern, different predicate (labeled paths instead of network patterns). |
| `compress_context` (compression.rs) | Second LLM egress; per-band compression + preset eligibility per §5.7. |
| Causal chain / session tracer / observability tools | The §9 substrate: events, filtered wire view evidence, `ViewerClass`-redacted query surfaces. |
| Sentinel `scan_credential_leaks` | Post-hoc detector extended to labeled-content hashes in causal payloads. |
| `sanitize_history_for_request` | The wire-transform stage the indication substitution rides on; token-budget logic unchanged; must preserve message ids per §3.4. |

---

## 11. Honest limits

- **The guarantee is about source content, not influence.** If the operator pastes an
  email into chat (unlabeled input), no label saves it — labeling the user message
  stream (§4.5) and the verbatim-echo assertion (§5.2.3) mitigate but cannot fully
  solve unlabeled ingress.
- **Paraphrase at the local boundary is trusted.** A local model summarizing
  `local_only` email produces `local_only` output mechanically — but if a *remote*
  model is fed indications and *guesses* the content, that's inference, not leakage.
  The guarantee: labeled content never appears in a disallowed request.
- **The echo assertion is a tripwire, not a proof.** It catches *verbatim* echo
  only — base64, reordering, or paraphrase defeat it. And naively it is O(n·m)
  (every withheld payload × every serialized body); bound it to recent turns or use
  multi-pattern matching (Aho-Corasick). Its role is catching bugs and naive echo,
  not proving non-leakage — the proof comes from input-side filtering (§5.2) plus
  the canary test.
- **Source rules are only as complete as their patterns.** A `sandbox.exec` that
  reads a labeled path through indirection (symlink, env var, `$(cat ...)`) can
  evade the static path match. Backstops: the echo assertion (when the gateway holds
  the content), `Network`-sink escalation (exfil needs network), and compartments
  for high-sensitivity work where `default_label: local_only` labels *everything*.
- **Credential injection has its own exposure.** `credential_env` delivers secrets
  into a sandbox that may hold `share_net`; once the namespace is open, nothing
  filters at the network layer. This predates envelopes and is unchanged by them —
  mitigated by `allowed_hosts` and approval gating, stated here so the envelope
  system is never mistaken for covering it.
- **Prompt caching interacts with filtering.** Cache-marker placement
  (`openai.rs:227-240`) assumes stable history prefixes; per-request filtering
  changes the prefix whenever a withheld envelope sits inside the cached span, and
  a session oscillating local↔remote maintains two prefix shapes — a real cost
  regression, not a leak. Marker placement should account for label bands. Corner
  case: content sent to a provider *before* a rule was added may persist in
  provider-side caches — **relabeling is not retroactive for live sessions** (the
  §6.7 sweep covers stored local content only).
- **Over-tainting is the main UX risk.** Monotonic intersections can cascade (a
  global memory recalled into a session taints derived work). Mitigations:
  per-envelope withholding (not session poisoning), taint-following routing (clean
  turns still go remote), per-band compression (§5.7 — without it, the first
  governor fire guarantees the cascade), compartments (§5.5), declassification
  (§8), and the relabel sweep (§6.7).
- **Post-grant sandbox network** has no host-level enforcement today; labels
  escalate and refuse but don't add a proxy. Stated, not solved.
- **Indications leak existence** ("2 emails withheld"). `terse` verbosity reduces
  this; it cannot remove it without breaking model coherence.
- **Trace evidence is metadata, but still sensitive.** Knowing *that* a session
  withheld 2 emails is itself information; events are content-free but not
  fact-free. They inherit `ViewerClass` redaction and P-8.6 retention like other
  causal events — no new exposure class is created.

---

## 12. Phasing

Each phase is independently shippable and testable.

1. **Provider classification + envelope type + LLM chokepoint + first
   traceability.** `egress_class` on presets (default remote),
   `DataEnvelope`/`Sink`/`EgressLabel` in `autonoetic-types`, stable message ids +
   id-keyed envelope sidecar (§3.4), indication substitution + outbound assertion
   at the `LlmDriver` chokepoint, session policy config, operator source rules
   (§4.2) with the sandbox static-analysis path matcher, **compression-preset
   eligibility** (§5.7 rule 1), gateway-authored strings audit, the **canary
   test** (tainted session vs mock remote provider — marker never appears in any
   wire body), and the first traceability deliverables: `egress.envelope_labeled`
   / `egress.envelope_withheld` / `egress.request_filtered` events, the filtered
   wire view in the session tracer (§9.2), and `gateway egress audit` v1 (§9.3).
   **The email scenario works end to end.** (Explicitly out of phase 1: the
   deferred `prompt_once` mode, §4.4.)
2. **Bundle floors + taint intersection + envelope-aware history + routing +
   cross-agent propagation + per-band compression.** `metadata.autonoetic.egress`
   floors in SKILL.md, intersect at `tool_call_processor`, envelope-aware history +
   checkpoint migration, taint-following routing (§5.3) **with fallback
   eligibility** and `egress.provider_selected` events, **labels on spawn-return
   values and `ecosystem.send_message` payloads** (closes the `LocalAgent` hole,
   §5.5), **per-label-band compression** (§5.7 rule 2), and the
   transform-preservation test (§3.4).
3. **Memory + all stored-content surfaces.** `egress_label` on `MemoryObject` **and
   `execution_traces`**, store-time intersection, request-time recall/query filter
   across `knowledge.recall`, `execution_search`, `digest_query`,
   `observability_read`, `session_peek`, `wiki_get`, digest/curator rules, relabel
   sweep with `egress.relabel` audit events.
4. **Federation/MCP/sandbox composition + declassification approvals** (OFP label
   metadata, MCP `egress_class`, `share_net` escalation, capsules, declassification
   grants, `egress.boundary_refused` events). Data-owner compartment pattern
   becomes fully usable here (depends on PR #902 landing).
5. **Constitution amendment** (§13) — mechanics proven first, per the amendment
   process (constitution.md:593-633).

---

## 13. Constitution path

After phases 1–3 land with tests:

- **New clause** (candidate: a `P-10.x` rule or a short new section), e.g.
  *"P-x.y: Content labeled with an egress label must never be included in a request
  to a sink the label excludes; withheld envelopes are replaced by non-divulging
  indications."* Plus an `I-x` cross-cutting invariant stating the label plane is
  gateway-only, non-agent-negotiable.
- Register in `enforcement_register.rs` (code + test citations), add to
  `fail_mode.rs` (`FAIL_MODE_TABLE` — **`emergency-stop` / `refuse-turn` for
  outbound-assertion violations**, which fire mid-turn; `degrade` for filtering) and
  the hardcoded `CONSTITUTION_RULE_IDS` test list.
- Causal events carry `enforced_rules: ["P-x.y"]` (the §9.1 event set is the
  attribution surface).
- Tests: fail-before/pass-after per the amendment process; new
  `constitution_egress_*.rs` integration tests (withhold at chokepoint, failover
  re-filter, taint-following routing, per-band compression, memory recall filter,
  stored-content query filters, curator taint inheritance, declassification audit,
  traceability five-questions).
- Bump `ACTIVE_CONSTITUTION_VERSION` (`autonoetic-types/src/config.rs:852`),
  recompute the lock (`docs/constitution/recompute_lock.py`), and handle the
  **federation ripple**: peers in `Exact` compatibility mode must add the new digest
  to `known_compatible_digests` (P-10.9).

---

## 14. Open questions

1. ~~Default label for plain workspace file reads~~ — **decided: `unrestricted`**,
   one-line config flip to tighten later.
2. ~~Should `local_only` include `MemoryPersist`?~~ — **decided: yes** (durable
   labeled memory beats no memory).
3. ~~First-touch classification (§4.4)~~ — **decided: deferred but kept** in the
   design. Not scheduled in phases 1–2; revisit when real operator usage shows
   whether being asked beats writing rules up front.
4. ~~Pinned preset conflicts with taint~~ — **decided: ask the operator inline**
   (approval-shaped: declassify / run on local / abort), causal-logged.
5. Indication granularity: one indication per envelope vs collapsing runs of
   withheld envelopes (token cost vs clarity).
6. Echo-assertion scope — **lean recorded** (§11): verbatim exact match, bounded to
   recent turns (Aho-Corasick if profiling demands), documented as tripwire rather
   than proof; canary test carries the proof burden. Final bounds set during
   implementation.
7. ~~Auto-spawned compartments~~ — **decided: no auto-spawn.** Compartments are a
   documented usage pattern (§5.5): task-scoped child sessions or a pinned
   data-owner agent. Revisit only if over-tainting proves painful in practice.
