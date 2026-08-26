> **Archived — shipped.** The behaviour this proposed is live and described in [`room.md`](../internals/session/room.md). Kept as the design record; not source of truth.

# RFC: The Session Room — Channel-Agnostic, Multi-Actor Live Collaboration

**Status:** Draft — 2026-06-03. Supersedes the earlier
`chat-tui-unified-timeline-plan.md` draft (now folded in here).
**Tracking:** #363 — phases #364 (P1) · #365 (P2) · #366 (P3) · #367 (P4) ·
#368 (P5) · #369 (P6).

**Origin:** Operator feedback that the chat TUI is brittle, shows too little
live, and that *resume* renders far more than *live*. Investigation reframed the
problem; operator direction then widened the scope:

> "Rewrite the whole TUI from scratch, keep `chat.rs` aside — don't pay to
> refactor it. Make the unified timeline serve other channels too (Discord,
> WhatsApp-like). Think of this as a **live interaction between a special actor
> (the human) and many autonoetic actors, internal tools (workbench/plan),
> external tools (IDE), and external AI agents (Claude/Codex/opencode)**. Be
> ambitious. Confirm the timeline beats pumping causal events. End with real
> architecture docs *and* user docs."

---

## 1. The two decisions that anchor everything

### 1.1 Timeline over raw causal chain — confirmed

| Layer | Shape | Purpose | In the room |
|---|---|---|---|
| `causal_events` | Hash-linked, complete, forensic; policy checks + internal mechanics | **Audit truth** | Drill-down depth only — never streamed |
| `live_digest_events` | Turn-structured, actor-attributed, readable, append-only (P-8.7) | **Live narrative** | The **spine** of the timeline |
| `operator_activity` | Thin "needs attention" projection | Alerts/signals | Attention overlay on the spine |

The digest is *already a projection built for live presentation*. The causal
chain is the substrate you drill *into*, not the thing you pump *out*. Pumping
causal raw = firehose; the human can't read mechanics at machine pace. **The
timeline is the canonical merged projection** — digest spine + attention signals
+ external-actor events + plan/workbench/interaction state-changes — with causal
+ `execution_traces` reachable beneath any line.

### 1.2 Greenfield renderer, merge moves server-side

`chat.rs` (~8700 LOC) stays untouched and runnable. We build a **new** renderer
beside it. Crucially, the *merge* that makes `chat.rs` brittle (seven imperative
per-client folds) moves into the **gateway**: one canonical timeline, one reader,
many thin renderers. The brittleness is fixed once, for every channel, not
re-solved per client.

---

## 2. Vision: the Session Room

A session is not a log an operator tails. It is a **room of citizens**
collaborating in real time, faithful to autonoetic's actors-as-citizens
paradigm (AI / human / script are first-class with shared rights and rules).

```
                       ┌──────────────────────────────────────────┐
                       │      GATEWAY: canonical SessionTimeline    │
                       │  (merged, ordered, actor-attributed,       │
                       │   channel-neutral; one cursor, one reader) │
                       └───────────────▲───────────────┬───────────┘
        emit events (who/what/refs)     │ read(after_cursor) │ deliver
   ┌───────────────┬───────────────┬────┴─────┐        ┌─────▼─────┬──────────┐
   │ autonoetic    │ internal tools│ external  │        │  TUI      │ Discord  │
   │ agents        │ workbench/plan│ tools(IDE)│        │ renderer  │ bridge   │
   │ (planner,     │               │ + foreign │        ├───────────┼──────────┤
   │  specialists) │               │ AI agents │        │ WhatsApp  │ IDE      │
   └───────────────┴───────────────┴───────────┘        │ bridge    │ ext.     │
              ▲ act (approve / answer / edit / inject)   └───────────┴──────────┘
              └────────────  human (special actor)  ◄── renders + inputs ───────┘
```

Two halves:

- **Stream** — the ordered narrative of what happened (the timeline).
- **Surfaces** — current-state objects the room shares: the **plan**, the
  **workbench**, the **active-actor roster**, **pending gates**. Surfaces are
  *projections of current state*; the stream is *the history of changes to them*.

The TUI shows stream (center) + surfaces (panes). Discord pins surfaces as
messages and streams events into a thread. WhatsApp renders surfaces on demand
(`/plan`, `/agents`) and streams only attention-level events. **Same timeline,
different formatting and affordances** — exactly the channel-agnostic thesis the
operator-activity-feed plan set out, now realized end-to-end.

---

## 3. Architecture

### 3.1 Gateway-owned canonical timeline (the keystone)

A `SessionTimeline` projection, merged **once** server-side:

- **Source of record (DECIDED):** extend `live_digest_events` **in place** into
  the canonical stream — no parallel `session_timeline` table. Breaking the
  current schema/shape is acceptable (autonoetic has no backward-compat yet;
  delete + migrate, don't shim). It already carries `event_id, root_session_id,
  source_session_id, turn_id, source_agent_id, event_type, payload, created_at`;
  add `principal` + `role` (§3.2), `altitude`, and `refs`, then let every
  producer write into it: the digest writer, `operator_activity` (as
  attention-tagged events), interaction/approval/plan/workbench changes, and
  external/foreign-actor events. One table, one writer path, one reader.
- **Reader:** `list_session_timeline(root_session_id, after_cursor, limit,
  min_altitude, actor_filter?)` — cursor-paginated on `(created_at, event_id)`,
  mirroring the `list_operator_activity` reader already shipped. *(Today only a
  writer exists, `create_live_digest_event` at observability.rs:503 — the reader
  is the first concrete build.)*
- **Push (optional, later):** SSE/subscribe for bridges that prefer push over
  poll — this is the real consumer that justifies the deferred #357
  subscribe/webhook work.

### 3.2 The actor model: two axes — *who* (principal) × *what seat* (role)

The room has **one kind of inhabitant: a participant.** There is no privileged
"system" class that speaks differently from an agent. The divergence Sentinel
intervening with "this looks like a loop — redirect?" is *the same kind of event*
as a specialist asking "which API should I target?" — both are participants
addressing the room. This is the operator's core ask: **no difference between
actors.**

To make that real, a participant is described on **two independent axes** — which
is exactly #359's principal model (`docs/design/principal-model-and-symmetric-
obligations.md`, types in #360):

```rust
struct Participant {
    principal: Principal,   // WHO it is — identity + kind
    role: SessionRole,      // WHICH SEAT it occupies in this session
}

enum PrincipalKind {        // #359 / #360 — the citizen's nature
    Human,
    AutonoeticAgent,        // planner, specialists, sentinel, curator, auditor…
    Script,
    ForeignAgent { provider },   // claude-code, codex, opencode — bounded local providers (#343); attribution, not authority
}
struct Principal { kind: PrincipalKind, id: String /* = causal-chain actor_id */ }

enum SessionRole {          // the seat, occupant-agnostic
    Operator,               // the deciding seat — see below
    Planner,
    Specialist { kind },    // coder, researcher, debugger…
    Sentinel,               // divergence / security monitor — a participant, not chrome
    Curator, Auditor,
    Tool { surface },       // workbench, plan, sandbox
    ExternalSurface { surface }, // IDE, editor
    Runtime,                // the executor's own voice (lifecycle, mechanical rulings)
}
struct TimelineRefs { causal_event_id, execution_trace_id, artifact_id,
                      interaction_id, approval_request_id, plan_id, workbench_id }
                      // P5 adds provider_id, transcript_ref (#343 provenance)
```

Two consequences that matter:

1. **Roles are occupant-agnostic.** `Operator` is a *seat*, not a synonym for
   "human." A human occupies it today; an **AI principal can occupy it tomorrow**
   (the operator's own point). The Sentinel is an `AutonoeticAgent` principal in
   the `Sentinel` seat — a citizen that observes and *intervenes*, surfacing its
   divergence asks through the same conversational-ask primitive (§3.5) as any
   specialist, never as a special alarm widget.
2. **Obligations attach to the seat's decisions, not the occupant's kind.** This
   is the crux of #359: whoever sits in a deciding seat — human or AI — owes a
   motivation for what they decide (§3.5). Symmetric rights *and* duties, by
   construction, because the model literally cannot tell "human decider" apart
   from "AI decider" except as a `PrincipalKind` tag for display.

The only thing that is *not* a participant is the **executor function** itself —
capability validation and execution (Separation of Powers). It stays invisible
plumbing; when the runtime needs to *say* something (a policy denial, a lifecycle
note), it speaks through the `Runtime` seat as a participant, so the room stays
uniform. `actor_id` is already bound into the causal-chain entry hash
(`causal_chain.rs`), so this is attribution we surface, not a new identity system.

### 3.2.1 Altitude (DECIDED): one shared scale, per-role refinement

Altitude (`Detail` < `Normal` < `Attention` < `Error`) is the gateway-owned
importance axis the reader filters on and the renderer dials (§4). One **default
scale common to all** producers, derived from `event_type` — so importance is
consistent across the room regardless of who emitted the event. Critical roles
may **refine upward**:

```
altitude(event) = max( base_altitude(event_type), role_floor(event.role) )
```

`role_floor` lets a critical seat guarantee its events surface — e.g.
`Sentinel ⇒ Attention` so a divergence intervention never sits below the
operator's default floor, even if its underlying event_type is mild. The rule is
a pure `max` (mechanically checkable, gateway-owned); roles can only *raise*,
never *suppress*, preserving "low-noise is a default, never a gag." `role_floor`
defaults are config-tunable (per the don't-pin-tunables-in-constitution rule).

### 3.3 Channels as thin renderers + channel bindings

Each channel binds to a root session and renders the timeline. This revives the
deferred #357 **`operator_channel_bindings`** table — now with real consumers:
it records `(channel, external_id) → root_session_id` so a Discord thread or
WhatsApp chat survives reconnects and routes the human's replies back as
`Human` actor events. Renderers are formatters + input affordances; they hold
**no merge logic and no importance rules**.

### 3.4 Bidirectional: every seat *acts*

The room is two-way for *all* participants. Whoever holds the `Operator` seat —
human today, AI tomorrow — acts (approve, answer, edit the plan, inject a
message, redirect, emergency-stop), and those are participant events on the
timeline like any other. Agents and foreign agents act by proposing intents the
executor validates. The Sentinel acts by intervening. The action surface is
identical across channels (a Discord ✅ reaction and a TUI quick-pick resolve the
same gate). *How* those inputs feel is the subject of §3.5 — the most important
UX call in this RFC.

### 3.5 Conversational gates & symmetric accountability (#359)

**The reframe:** approvals, clarifications, divergence checks, and plan approvals
are not four modal card subsystems that *interrupt* the operator. They are one
primitive — **an addressed ask between citizens**, resolved as a ping-pong turn
in the same conversation. The operator should catch *instantly* what is expected
and answer without leaving the flow.

A gate event carries:
- **A clear ask** — one line, "what do you want from me," not a form to parse.
- **Pre-digested choices** — the asking actor (agent/gateway) proposes the
  likely resolutions, having done the enumeration work: e.g. for a network gate,
  `[Approve once] [Approve for session] [github.com only] [Deny] [Ask why]`.
  One keystroke / one tap / one reaction resolves it. (Separation of Powers:
  the agent *proposes* the menu; the human *decides*.)
- **An open channel** — the human can add a point, ask a counter-question, or
  redirect *without resolving*. A counter-question routes back to the asker and
  the thread continues; the gate stays open. The flow is never broken.

It is **non-modal**: a gate is a thread in the stream, not a screen takeover.
The rest of the room keeps moving where the gate doesn't strictly block that one
task. Multiple gates can be in flight as parallel threads.

**Symmetric accountability — the tension, and its resolution.** #359 (principal
model & symmetric obligations, `docs/design/principal-model-and-symmetric-
obligations.md`) closes a real asymmetry: an agent owes a reason for every
rejection (`Ri-0.3`), yet a human can reject a gate silently. Citizens with
equal rights carry equal duties — so the human's decisions must be **motivated,
sooner or later**. That pulls directly against "make approvals frictionless."

The resolution is **accountability without tedium**, graduated by stakes:

1. **Motivation rides on the choice, not a form.** A pre-digested option *is* a
   rationale (`github.com only — fixtures are external` bundles decision +
   reason in one tap). Free-text the human naturally types becomes the
   motivation — captured from conversation, not extracted by interrogation.
2. **"Sooner or later" = deferred-allowed.** Low-stakes / reversible decisions
   resolve instantly; the motivation may be empty or filled async (a gentle,
   non-blocking "want to note why?" later, never a wall). High-stakes /
   irreversible / high-authority decisions require motivation *now* — the only
   case where the gate blocks on a reason.
3. **Mechanically checkable (Lawful Executor).** The gateway records the decision
   with `decider_kind` (this is exactly #361 / P1.b) and a motivation slot; the
   §O obligations (#359 Part B) check *that a motivation exists by the required
   time*, not its quality. The gateway enforces the duty; it never judges the
   reason.

**The explicit graduated-motivation policy.** "Graduated by stakes" is a table
the gateway evaluates from facts it already holds — not a judgment call. Three
obligation tiers:

| Tier | Behavior |
|---|---|
| **BLOCKING** (motivate-now) | Decision does not commit until a non-empty motivation is attached — or a *labeled* pre-digested choice that carries one. |
| **DEFERRED** (motivate-soon) | Commits immediately; a reason is owed by a bounded deadline (config). Auto-discharged if the decider typed free-text. Unmet debt becomes a recorded **obligation-gap** event — surfaced, not punitive. |
| **OPTIONAL** | No obligation (answering a clarification an agent asked; advisory acks). |

The classifier is a pure function of existing gateway facts:

```
tier(decision, gate) =
  BLOCKING  if decision.polarity ∈ {Reject, Deny, Abort}          // ← exact mirror of Ri-0.3
         or decision.is_override                                   // bypasses a default/safe choice — cf. force_reason
         or gate.required_authority != Operator                    // Admin / Agent(id) — existing ApprovalLevel
         or action_class(gate.action) == ExternalOrIrreversible    // WebFetch/WebCall/CredentialRequest/AgentInstall, non-workspace WriteFile
  DEFERRED  else if polarity == Approve and required_authority == Operator
                 and action_class == Local                         // SandboxExec(no net), workspace WriteFile
  OPTIONAL  else
```

The spine — **a rejection is always BLOCKING** — is the exact symmetric mirror of
`Ri-0.3`. The other blocking triggers reuse precedents that already exist
(`force_reason` for overrides, `ApprovalLevel::Admin` for authority,
external/irreversible for stakes). The common case — approving a local,
reversible, expected action — is DEFERRED or OPTIONAL: one tap, no reason. Even a
BLOCKING reject is one keystroke when the asker offered a labeled option
(`Deny — out of scope`). The full §O / O-1 operationalization (and the
constitution-vs-config split) lives in `docs/design/principal-model-and-
symmetric-obligations.md` §B.4.

So the Session Room is where #359's symmetric obligation becomes *lived*: both
the agent's intent and the human's decision appear in the timeline as
accountable, motivated turns between equals — and because motivation is captured
*as conversation* with pre-digested shortcuts, the symmetric duty costs the
human almost nothing in the common case. Divergence validation folds in the same
way: the trajectory monitor poses "this looks like a loop — keep going / redirect
/ stop?" as an in-flow ask with pre-digested choices, not an alarm.

### 3.6 Who asks, who answers, who computes the choices

**Who computes the pre-digested choices — depends on the gate kind:**

- **Approval / safety gates → the gateway.** It already knows the resolution
  space (approve once / for session / host-only / deny, scoped to the
  `ScheduledAction`), so it supplies the menu deterministically. No agent
  involvement needed.
- **Clarifications → the asking agent.** Only the agent knows the choice space
  for "which API should I target?", so it supplies the hints with its ask
  (prose-first; structured options best-effort, per our output philosophy). If it
  supplies none, the gate is a free-text question — still a valid ask.

**Who may address the operator — children speak directly (libertarian default).**
Internal reasoners and sub-agents are children of the planner today. Rather than
route every ask up through the planner, **any child may address the room
directly** — it matches "no difference between actors" and avoids a planner
switchboard bottleneck. Considered cons, and why none blocks the default:

1. *Cacophony / N-to-1 flooding* (many children asking at once) — handled by the
   altitude + squashing machinery (§4) and by addressing (below); not a reason to
   centralize.
2. *Redundant / context-blind asks* (a child asks what the planner already knows)
   — mitigated by **address-to-role with escalation**: an ask is addressed to the
   *nearest competent decider* (parent agent by default, escalating to `Operator`
   only when the parent defers or it is a genuinely human decision). Direct-to-
   operator stays *possible* (the child judges when the operator is the right
   decider) without making it the default path that floods the human.
3. *Manipulation via a foreign/untrusted child* (a foreign agent crafts a
   misleading ask) — defused by **mandatory attribution**: every ask renders its
   true `principal` + `role` + parent chain, and foreign principals carry the
   §5 trust posture. The operator always sees *who* is really asking.

So: build **direct child asks** first (simplest, most uniform); add an optional
planner-mediated interface later *only if* cacophony proves real in practice. The
guardrails (attribution, address-to-role escalation, squash) cost nothing and
preserve the libertarian default.

---

## 4. The new TUI renderer (greenfield)

A new module/crate (e.g. `autonoetic-room-tui`), `chat.rs` kept aside until the
new one is at parity, then retired. Design:

- **Pure render of `SessionTimeline`.** State = `(timeline: Vec<TimelineEvent>,
  surfaces: Surfaces, view: ViewState)`. Rendering is a pure function; input
  produces gateway actions. No per-source dedup sets — the gateway already
  merged and deduped.
- **One stream, live == resume.** Live appends past the cursor; resume replays
  from the cursor. The "resume shows more" divergence is gone by construction.
  Because **a session can last days**, resume does *not* load everything: it
  shows the recent window (last-N) and **scrolls into the past on demand via the
  cursor** (lazy backfill as you page up). `session_history` (verbatim model
  conversation) becomes an explicit "show raw transcript" toggle, not the view.
- **Altitude dial.** Render all altitudes; a keybind cycles the live floor
  (`Detail ↔ Normal ↔ Attention`) like a log level. The hidden:shown ratio
  becomes the operator's choice, not a compile-time constant.
- **Progressive squashing.** Runs of low-altitude / same-kind / same-actor
  events coalesce into one dim collapsible line (`▸ 14 routine ops — ⏎ expand`).
  Always visible-that-it-happened; never floods. (The `rate_limited` notice is
  the primitive.)
- **Drill-down beneath any line.** `⏎` expands an event's `refs`: full causal
  event(s), untruncated `execution_trace` stdout/stderr, and a content preview
  of the generated artifact. Depth on demand; never streamed.
- **Conversational gates inline (§3.5).** Gates render as addressed asks in the
  stream with pre-digested choices on the hot keys — resolve in one keystroke,
  or type to elaborate/counter without leaving the input. The pending-gate pane
  is just a "threads awaiting you" index into those inline asks, not a separate
  modal flow. A motivation the operator types is attached to the decision event.
- **Runtime voice is hidable.** The `Runtime` seat's mechanical notices (policy,
  lifecycle) sit at `Detail` altitude by default — **hidden at the normal floor,
  one dial-down away when the operator wants them**. Visible if needed, never
  ambient noise.
- **Surfaces as panes.** Plan, workbench, active-actor roster, pending gates —
  live state objects, not transcript lines.

---

## 5. Federation frontier (the ambitious part)

> **Authoritative mechanism: #343** (External CLI agent delegation,
> `docs/proposals/external-cli-agent-delegation.md`; parent track #325). P5
> does **not** invent a federation system — it is the **room's rendering +
> attribution layer over #343**. Where this section and #343 disagree on
> mechanism, **#343 governs**; the Session Room only *shows* what #343 runs.

The actor model lets external work appear in the room **without special cases** —
but external agents stay exactly as bounded as #343 specifies.

- **External AI agents as bounded local providers (per #343).** Claude Code /
  Codex / opencode are **local execution providers the gateway launches** against
  a bounded workbench (`cwd = workbench/source`), under a configured provider
  profile — **not** child Autonoetic agents in the capability sense (#343 §3, §6).
  They are *not* joined as MCP/OFP peers in the MVP; `autonoetic-mcp` /
  `autonoetic-ofp` are a *possible later transport*, not the model. The gateway
  captures their transcript, exit status, and diff, then reconciles/validates —
  per #343's flow.
- **`ForeignAgent` is attribution + trust, NOT a capability grant.** The
  `PrincipalKind::ForeignAgent { provider }` tag exists so timeline events read
  *"who produced this diff"* (`provider` = #343's `provider_id`). It confers **no
  authority and no agent-like powers** — it does not make the provider a
  delegatable citizen. This is the room's reading of #343's core rule:
  > External-agent output is an **artifact mutation proposal, not an authority
  > decision.**
- **Trust posture (DECIDED, consistent with #343 §6): nothing external is
  introduced inside without strong-authority approval.** A `ForeignAgent` carries
  **no privilege by default** — it edits only inside the workbench and proposes
  diffs. Any effect that would *leave* the workbench (promotion, install,
  artifact mutation, secrets, network, capability/PlanFrame change) is denied to
  it and gated at **elevated authority** (`ApprovalLevel::Admin`+), which is
  BLOCKING in the motivation policy (§3.5). Foreign output is sandboxed and
  quarantined until reconciliation completes and a strong-authority decision
  admits it.
- **Timeline provenance mirrors #343.** A `ForeignAgent` timeline event carries
  #343's provenance — `provider_id`, `mode` (interactive/non-interactive),
  `plan_id`, `step_id`, `checkpoint_before`, `changed_files`, `transcript_ref` —
  so the room can drill from a foreign-work line into its diff/transcript.
  (Requires extending `TimelineRefs` with `provider_id` + `transcript_ref`;
  `plan_id`/`workbench_id` already exist.)
- **The IDE as both renderer and actor.** An IDE extension renders the timeline
  *and* emits `ExternalSurface` events (file edits ↔ workbench), so editor
  activity appears in the room and the room's plan/workbench reflects into the
  editor — within the same workbench boundary.
- **Workbench & plan as participating surfaces.** Edits to the shared plan or
  workbench — by human, agent, or foreign provider — are timeline events against a
  shared surface, so "who changed the plan and why" is always legible.

This is autonoetic-idiomatic: Separation of Powers holds (the provider proposes,
the gateway reconciles/validates/admits), expressed as one timeline of attributed
events feeding many channels. **P5 ships only after #343's delegation mechanism
lands** — it renders #343, it does not precede it.

---

## 6. Digest as the spine — what to enrich (gateway-side)

The room is only as explanatory as the digest. To make the narrative *flow*
rather than list mechanical bullets (operator: "make the narrative really
consistent"):

1. **Capture reasoning** — record short annotations from LLM thinking so "why"
   is in the stream (uses `record_annotation`, live_digest.rs:379).
2. **Turn intent line** — emit a one-liner at turn start so each turn reads
   *intent → actions → result*, not a flat dump.
3. **Routine annotation** — tune planner/specialist prompts so reasoning/
   decision/observation annotations are habitual (prose-first, structure
   best-effort — consistent with our output philosophy).
4. **Failure context** — on error, link the preceding action chain, not just the
   final message.

Kept as a parallel gateway track; respects P-8.7 (real-time, append-only).

---

## 7. Phases & sequencing

1. **P1 — Canonical timeline + reader (gateway).** Extend `live_digest_events`
   to canonical (actor + refs); ingest operator_activity/interaction/approval/
   plan/workbench events; ship `list_session_timeline`. *Self-contained, testable
   without any UI; fixes merge brittleness for all future channels.*
2. **P2 — New TUI renderer (greenfield).** Pure render of the timeline + surfaces;
   live == resume; altitude dial; squashing; drill-down; **conversational gates
   (§3.5)**. `chat.rs` untouched.
   - **P2 depends on #361 (decider-kind on gate decisions)** for the
     accountability half: the renderer attaches a motivation, the gateway records
     `decider_kind` + motivation. Build the gate UX and the #361 recording
     together so symmetric accountability is real, not cosmetic.
3. **P3 — Channel SDK + bindings.** `operator_channel_bindings` (#357) + a thin
   renderer trait; first external bridge (Discord) as proof. Gate affordances map
   per channel (reaction = quick-pick, reply = elaboration).
4. **P4 — Digest enrichment** (parallel to P2/P3): reasoning capture, turn intent,
   failure context.
5. **P5 — Render external delegation (#343)**: surface #343's bounded local CLI
   providers (Claude/Codex/opencode) as `ForeignAgent`-attributed timeline events
   with #343 provenance; IDE renderer+actor binding. **Ships after #343 lands** —
   P5 renders that mechanism, it does not invent one. See §5.
6. **P6 — Documentation**: architecture doc + user guide (see §9).

Risk is low because P1 is server-side and independent, and the TUI is greenfield
(no fragile refactor). Each phase ships value alone.

---

## 8. Boundaries & constitution

- **Gateway owns visibility; channels own presentation.** Importance/severity/
  emission/merge live in the gateway. Channels choose altitude, squashing,
  drill-down, formatting. No channel re-implements importance rules.
- **No causal firehose** in any channel's stream — drill-down only.
- **Separation of Powers** holds for foreign agents: they propose, the gateway
  executes. Federation does not grant privilege.
- **P-8.7** (live digest real-time) preserved; the canonical timeline is its
  generalization, not its replacement.
- **Symmetric accountability (#359).** Human and agent are equal citizens: both
  decisions are recorded with `decider_kind` and a motivation (sooner or later).
  The gateway enforces *that* a motivation exists by the required time (Lawful
  Executor, mechanically checkable) and never judges its quality. Frictionless
  is a UX duty (§3.5), not a licence to drop the obligation.
- **`session_history`** is verbatim model conversation, available on demand, not
  the operator view.

---

## 9. Final deliverables (operator asked for both)

- **Architecture documentation** (`docs/ARCHITECTURE.md` + a dedicated
  `docs/guide/session-room.md`): the canonical timeline, actor model, channel
  renderers, surfaces, federation, and how it relates to causal chain / digest /
  operator_activity.
- **User documentation** (`docs/` user guide): how to use the room — reading the
  timeline, altitude dial, squashing, drill-down, resolving gates, editing the
  plan/workbench, and (later) inviting foreign agents / binding channels.

---

## 10. Open questions

- ~~**Canonical store:**~~ **RESOLVED** — extend `live_digest_events` in place;
  breaking the schema is fine (no backward-compat). See §3.1.
- ~~**Altitude scale:**~~ **RESOLVED** — one shared default scale, per-role
  upward refinement (`max(base, role_floor)`); Sentinel-class seats raise their
  floor. See §3.2.1.
- ~~**Gates as events vs surfaces:**~~ **RESOLVED** — gate ask + decision are
  timeline events (conversational, §3.5); the pending-gate pane is a surface
  *projection* over unresolved ones ("threads awaiting you"), not a separate
  store. Dedup the asks against the digest (which already records approvals) so a
  gate appears once. See §3.5 / §4.
- ~~**Graduated motivation policy:**~~ **RESOLVED** — explicit three-tier
  classifier (BLOCKING / DEFERRED / OPTIONAL); rejection always blocks (mirror of
  Ri-0.3), plus override / elevated-authority / external-irreversible. See §3.5
  and `principal-model-and-symmetric-obligations.md` §B.4.
- ~~**Pre-digested choices — who computes them?**~~ **RESOLVED** — depends on
  gate kind: the **gateway** for approval/safety gates (it knows the resolution
  space), the **asking agent** for clarifications (only it knows the choices;
  none ⇒ free-text). See §3.6.
- ~~**Foreign-agent trust:**~~ **RESOLVED** — no privilege by default; any effect
  a `ForeignAgent` would introduce is gated at elevated authority
  (`Admin`+, BLOCKING), sandboxed/quarantined until admitted. Nothing external is
  introduced inside without strong-authority approval. See §5.
- ~~**Seating internal reasoners:**~~ **RESOLVED (direction)** — children speak
  **directly** to the room (libertarian default); guardrails = mandatory
  attribution + address-to-role-with-escalation + squash; planner-mediated
  interface added later only if cacophony proves real. See §3.6. *(Still open:
  whether a reasoner's ask may BLOCK or only advise — folded into intervention
  posture, decide during P2.)*
- ~~**Operator-as-AI:**~~ **RESOLVED (intent)** — not near-term; if it happens, an
  AI in the `Operator` seat **motivates everything**, recorded and introspectable
  (effectively all tiers become motivate-now for an AI decider; cost ~0). Authority
  config per #359 Part D.
- ~~**Runtime voice:**~~ **RESOLVED** — hidable: `Runtime` notices default to
  `Detail` altitude (hidden at the normal floor, visible one dial-down). See §4.
- ~~**Resume depth:**~~ **RESOLVED** — last-N window + cursor backfill on page-up;
  day-long sessions are not fully loaded but the past is reachable. See §4.

*(No open questions block P1; the remaining behavioral nuance — reasoner
BLOCK-vs-advise — is a P2-time decision.)*
