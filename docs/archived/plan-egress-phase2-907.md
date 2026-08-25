# Implementation plan — #907 [Phase 2] Bundle floors, envelope-aware history, taint-following routing

**Tracking issue:** [mandubian/autonoetic#907](https://github.com/mandubian/autonoetic/issues/907)
RFC: [`docs/rfc/data-envelopes-egress-localization.md`](../rfc/data-envelopes-egress-localization.md) §4.1, §4.5, §5.3, §5.5, §5.7, §9.1.
Parent [#903](https://github.com/mandubian/autonoetic/issues/903). Builds on merged Phase 1 (#911/#912/#913/#914). Coordinates with **open PR #915** (#905 observability leftovers).

## Where Phase 1 left the label plane

- Types complete in `autonoetic-types/src/egress.rs` (`Sink`, `EgressLabel`, `DataEnvelope`, `Provenance`, `EgressConfig`, `EgressSessionPolicy`, `NamedEgressLabel`).
- Labeler `runtime/egress_labeler.rs`: resolves `(source,path) → EgressLabel` at the tool-result boundary via operator+session rules (paths 1,4,5 of §4.1). Emits `egress.envelope_labeled`.
- Chokepoint `llm/egress_chokepoint.rs`: pure filtering — substitutes indications for **`Role::Tool`** messages whose label excludes the target sink, verbatim-echo assertion, fail-closed. Wraps every driver incl. fallbacks (`llm/mod.rs:779-796`).
- Compression gate `egress_labeler.rs:776` `compression_preset_eligible`: **single-band, all-or-nothing** eligibility (§5.7 rule 1 only).
- The label sidecar is a **transient in-memory `HashMap<tool_call_id, EgressLabel>`** on the processor (`tool_call_processor.rs:70`) → executor (`lifecycle.rs:149`), attached to requests via `metadata["__egress_labels"]` (`lifecycle.rs:2700`). **Not serialized in checkpoints.** Only covers tool messages.

### Confirmed Phase-2 gaps (with anchors)
- §4.1 path 2 (bundle floor): **absent** — no `metadata.autonoetic.egress` field, no floor input to resolution.
- §4.1 path 3 (argument taint): **stubbed** — `Provenance.parent_envelope_ids` hard-coded empty (`egress_labeler.rs:316-318`, event field `:468`).
- §3.4 message ids: **absent** — `Message` (`llm/mod.rs:461`) has no id; join key is `tool_call_id`; assistant/user/synthesized messages can't be labeled.
- Checkpoints (`checkpoint.rs:145-263`): no egress field; labels lost on suspend/resume.
- Routing: chokepoint filters but never *selects* a provider by taint; fallback chain filtered by tier only (`model_router.rs:131-157`).
- Cross-agent: `agent_message` payload (`tools/agent.rs:1683 save_agent_message`) and spawn-return `result_summary` (`scheduler/workflow_store.rs:~1117`) carry no label — the `LocalAgent` hole.
- Compression: no per-band split (§5.7 rule 2 net-new).

---

## Slicing (each row is one PR)

| Slice | Issue items | Blast radius | Depends on |
|---|---|---|---|
| **1. Label-resolution extensions** | bundle floor, argument taint | small (labeler, tool_call_processor, manifest) | — |
| **2. Envelope-aware history + checkpoints** | msg ids, checkpoint migration, transform preservation, LLM-response label | large (Message, checkpoint fmt, ~15 sites) | — (eases 3/4/5) |
| **3. Taint-following routing** | eligibility, fallback filter, provider_selected, pinned-preset ask, no-eligible-provider | large (lifecycle routing/failover) | overlaps #915 in lifecycle.rs |
| **4. Cross-agent propagation** | labels on spawn-return + agent_message | medium (tools/agent, workflow_store, workflow) | 2 (cleaner with msg ids, not required) |
| **5. Per-label-band compression** | §5.7 rule 2 | medium (compression, capsule) | 2 (labels on synthesized blocks) |
| **6. §5.6 acceptance integration test** | end-to-end incl. context-governor fire | test-only | 1–5 |

**Recommended order:** 1 → 2 → 3 → 5 → 4 → 6. Rationale: Slice 1 is safe and unblocks nothing else but delivers real value immediately; Slice 2 is the foundation everything richer wants (assistant-message labels, checkpoint durability); routing (3) is the headline behavior; per-band compression (5) needs the msg-id sidecar from 2; cross-agent (4) is self-contained and can slot anywhere after 2; the acceptance test (6) lands last.

**#915 coordination:** #915 touches `lifecycle.rs`, `session_tracer.rs`, `cli/gateway.rs`, `response_validation.rs`, `cli/common.rs`. Slices 1/2/4/5 barely touch those. **Slice 3 collides in `lifecycle.rs`** (routing region + audit rendering). Land #915 first, or rebase Slice 3 on it. Slice 3's `egress.provider_selected` should render in the `gateway egress audit` view #915 introduces — build on #915's tracer/CLI surface rather than duplicating.

---

## Slice 1 — Label-resolution extensions

**Goal:** finish §4.1 label resolution — bundle-declared floor (path 2) + argument taint (path 3).

### 1a. Bundle-declared floor (§4.1 path 2)
- New manifest field `metadata.autonoetic.egress.output_label`.
  - `autonoetic-types/src/agent.rs`: add `pub egress: Option<AgentEgressManifest>` on `AgentManifest` (~`:252`), with `AgentEgressManifest { output_label: Option<NamedEgressLabel> }` (reuse `egress::NamedEgressLabel`).
  - `runtime/parser.rs`: add `egress` to the `AutonoeticMetadata` DTO (`:72`) and thread it in the mapper (`:195-219`).
- Fold the floor into the labeler so it participates in resolution **and** clears inertness (the subtle part):
  - `EgressLabeler::from_config` currently sets `inert` when no rules + default unrestricted (`egress_labeler.rs:164`). A bundle-only floor must make the labeler non-inert and intersect on every resolve.
  - Add `EgressLabeler::with_manifest_floor(self, Option<EgressLabel>)` (or a per-tool floor map if we later want per-tool floors — RFC example is a single bundle-wide output floor). `resolve_label`/`resolve_exec_label` intersect the floor into the result; record it in provenance/event (new `bundle_floor` field on the `egress.envelope_labeled` payload).
  - Build site: `tool_call_processor.rs:222 build_egress_labeler` — it has `self.manifest` in scope.
- **Floor semantics:** intersection only (a floor restricts the bundle's own outputs, never widens operator policy — free from `restrict`).

**Tests (Slice 1a):**
- Floor applies from manifest with no operator rules (labeler no longer inert).
- Floor cannot widen: operator `local_only` + bundle `unrestricted` floor → stays `local_only`.
- Bundle `local_only` floor + operator `no_remote_model` → intersection `local_only`.
- `egress.envelope_labeled` records the floor as a resolution input.

### 1b. Argument taint (§4.1 path 3)
**Design question to settle first: how is an argument "tainted"?** The gateway must decide deterministically (Lawful-Executor). Two deterministic signals, used together:
1. **Handle references (primary):** the arguments JSON references a prior labeled envelope by handle — a labeled `artifact_id`/`artifact_ref`, or a prior `tool_call_id`. The processor already holds prior labels in `self.egress_labels` (keyed by `tool_call_id`); artifact labels come from the artifact-store sidecar (Phase 3 formally, but exec dependency labeling already reads artifacts). Scan args for these handles, collect their labels, intersect.
2. **Verbatim content (secondary, bounded):** a prior labeled envelope's content appears verbatim in the args (inbound mirror of the chokepoint echo assertion), bounded to recent envelopes / Aho-Corasick. Honest-limit: defeated by paraphrase/encoding — a tripwire, not a proof.

- Thread the accumulated prior-label map into `label_tool_result` (or intersect in the processor after the call, `tool_call_processor.rs:424-438`).
- `resolution.label = resolution.label.restrict_all(parent_labels)`; populate `Provenance.parent_envelope_ids` (needs the envelope-id ↔ tool_call_id map — Phase 1 mints `env_*` per outcome but the sidecar is keyed by `tool_call_id`; add an id map or record parents as tool_call_ids until Slice 2's msg ids).
- Emit the derivation lineage in `egress.envelope_labeled` (`parent_envelope_ids` already in the payload, `:468`).

**Tests (Slice 1b):**
- Tool called with an argument referencing a `local_only` artifact/prior tool result → output labeled `local_only`, `parent_envelope_ids` populated.
- Clean argument → no taint, no parents.
- Intersection of two tainted parents (e.g. `local_only` ∩ `no_remote_model` = `local_only`).
- Verbatim-content taint fires on an exact match, bounded to recent envelopes.

**PR title:** `feat(egress): bundle-declared floor + argument-taint intersection (#907 slice 1)`

---

## Slice 2 — Envelope-aware history + checkpoints + transform preservation

**Goal:** §3.4 message-level binding that survives every transform + checkpoint; enables LLM-response label (§4.5).

**Key decision — message ids.** Add `id: Option<String>` (`msg_<ulid>`) to `Message` (`llm/mod.rs:461`), `#[serde(default, skip_serializing_if=Option::is_none)]`. Concerns:
- `Message` derives `PartialEq` — adding a field changes equality; audit message-equality assertions in tests (grep). Consider `#[serde(default)]` + minting only at commit so in-flight constructed messages stay `id: None` (equal as before) until committed.
- Providers convert `Message` → provider format; the id never hits the wire (verify openai.rs/anthropic mapping ignores it).
- Mint at the commit points: `lifecycle.rs:4149-4157` (tool results + assistant), `:3963-3970` (approval variant), assistant pushes `:3240/3278/3370`, user push `:1879`.

**Sidecar re-key.** Move the label map from `tool_call_id`-keyed to `msg_id`-keyed (extends coverage to assistant/user/synthesized). Touch: `tool_call_processor.rs:70`, `lifecycle.rs:149`, `egress_labeler.rs:52/776`, `egress_chokepoint.rs:252` (withhold assistant messages too now), `compression.rs:255`, `context_governor/strategies.rs:35`. Keep a `tool_call_id → msg_id` bridge at commit so tool labels migrate cleanly.

**LLM-response label (§4.5).** When a completion returns, label the committed assistant message = intersection of labels of all envelopes included in that request. That makes scenario step 4 (the tainted summary) automatic and lets the chokepoint withhold "the summary above" on later remote turns.

**Checkpoint migration.**
- Add `egress_labels: HashMap<String, EgressLabel>` (msg-id-keyed) to `SessionCheckpoint` (`checkpoint.rs:145-263`), `#[serde(default)]` for backward-compat; restore in `restore_into` (`:266+`).
- Update production builders: `lifecycle.rs:1144 build_checkpoint`, `execution.rs:1801` (emergency stop). Update 13 test constructions (grep `loop_guard_state:`).

**Transform preservation (§3.4).** `sanitize_history_for_request` (`prompt_budget.rs:81`) and compression must carry ids: dedup/collapse already keep `tool_call_id`; extend to keep `id`. Compression's synthesized block gets an explicitly computed label + a new msg id.
- Required test: run sanitize + compress over a labeled history, assert every label survives (§3.4).

**Tests (Slice 2):**
- Checkpoint roundtrip preserves labels across suspend/resume (+ fork).
- Assistant message inherits request-envelope intersection (LLM-response label).
- sanitize + compress preserves every label.
- Chokepoint now withholds a labeled assistant message from a remote request.

**PR title:** `feat(egress): stable message ids + envelope-aware checkpoints + LLM-response label (#907 slice 2)`

---

## Slice 3 — Taint-following routing (§5.3)

**Goal:** per-completion provider eligibility from the new-envelope batch; fallback filtered identically; full auditability; pinned-preset inline ask; no-eligible-provider.

- **Batch capture:** at `lifecycle.rs:3780`, retain the `take_egress_labels()` delta (plus any new user-message label) as `pending_batch_labels` instead of only merging. `batch_intersection = restrict_all(delta)`.
- **Eligibility test:** a preset is eligible iff `batch_intersection.allows(preset.egress_class.as_sink())`.
- **Primary selection (`lifecycle.rs:2496-2640`):** filter routing candidates to eligible presets. **Caveat:** `self.llm` is built once (`:131/:379`); a taint-forced switch to a local preset can't be a model-name swap — reuse the fallback driver-rebuild (`build_driver`, `:2909`) for the chosen eligible preset, or trigger the inline-ask when a pin conflicts.
- **Fallback filter:** skip ineligible presets — either in `model_router.rs:build_fallback_chain` (131-157, add egress predicate next to the tier filter) or at consumption (`lifecycle.rs:2801/2869`, look up `cfg.llm_presets.get(fb_preset).egress_class`). A tainted turn must never fail over into an all-indications remote context.
- **Pinned-preset conflict → inline ask:** approval-shaped prompt (declassify / run on local preset X / abort), reusing the approval machinery (session grants / human_gate). Causal-log the choice. Never silently downgrade, never dead-end.
- **`egress_no_eligible_provider`:** when no eligible preset exists → refuse the turn with a path forward (configure local preset / declassify / abort).
- **`egress.provider_selected` event (§9.1):** emit per completion next to the routing log (`:2643-2650`) — eligible set, chosen preset, batch intersection, fallback skips, inline-ask outcome. Render in `gateway egress audit` (build on #915).

**Tests (Slice 3):**
- Tainted batch → only local presets eligible; next clean batch → remote eligible again (§5.6 steps 4–5).
- Fallback chain skips ineligible presets.
- `egress.provider_selected` emitted per completion with correct eligible set + batch intersection.
- Pinned remote + tainted batch → inline approval; each choice (declassify/local/abort) causal-logged.
- No eligible preset → `egress_no_eligible_provider` with the operator choice surfaced.

**PR title:** `feat(egress): taint-following routing + provider_selected + pinned-preset ask (#907 slice 3)`

---

## Slice 4 — Cross-agent propagation (§5.5)

**Goal:** close the `LocalAgent` hole — labels intersect onto spawn-return values and `agent_message` payloads.

- **`agent_message` payload (`tools/agent.rs:1683`):** intersect the sender session's accumulated `egress_labels` onto the payload before `save_agent_message`; enforce/withhold if the recipient's provider constraint excludes `LocalAgent`. The tool's `execute` doesn't see `self.egress_labels` today — thread it via the tool run-context or read from the store.
- **Spawn-return (`scheduler/workflow_store.rs:~1117 set_task_status_with_summary`, surfaced by `tools/workflow.rs:751-781 workflow_wait`):** when a child completes, compute the intersection of its accumulated labels and attach it to the `result_summary` envelope (one labeled envelope crosses back = child taint intersection). Otherwise `workflow_wait`'s result is only rule-labeled by tool name and misses child taint.

**Tests (Slice 4):**
- Tainted child → spawn-return summary carries the child's intersected label.
- `agent_message` from a tainted session → payload labeled; a remote-pinned sibling receiving it can't ship it (withheld/refused).

**PR title:** `feat(egress): label propagation across spawn-return + agent_message (#907 slice 4)`

---

## Slice 5 — Per-label-band compression (§5.7 rule 2)

**Goal:** never a single mixed summary — clean and tainted messages compress in separate bands → separate labeled blocks.

- Factor a `partition_by_label(band, labels)` helper out of `compression_preset_eligible`'s per-message lookup (`egress_labeler.rs:787-805`).
- `compression.rs`: between the recency split (`:299`) and the single summary build (`:433-451`), partition `compressible` by label, run the eligibility gate + a summarization per band on a band-eligible preset, emit **separate** `[COMPRESSED CONTEXT]` blocks, each a new msg id with an explicitly computed band-intersection label (Slice 2 sidecar). No eligible preset for a band → token-budget truncation for that band only.
- Mirror in `context_governor/capsule.rs` (`:531/:539-588/:602`). Add the missing `egress.boundary_refused` event (thread a `GatewayStore` handle — noted at `capsule.rs:578-583`).

**Tests (Slice 5):**
- Mixed history → two labeled blocks; tainted band never goes remote.
- Tainted band with no eligible preset → truncated, not remotely compressed.
- Every label survives compression (ties into §3.4 test).

**PR title:** `feat(egress): per-label-band compression (#907 slice 5)`

---

## Slice 6 — §5.6 acceptance integration test

**Goal:** the acceptance bar — the mixed email session end to end, **including a context-governor fire**.

- New `autonoetic-gateway/tests/egress_mixed_session_e2e.rs` following the `egress_compression_eligibility_integration.rs` harness (mock remote + mock local presets, canary content).
- Walk §5.6 steps 1–8: clean code turn → remote; sandbox.exec reads `~/mail/**` → `local_only`; next turn routes local, response intersects `local_only`; clean turn routes remote with indications where email+summary were; governor fires → per-band compression; digest local; audit renders every step.
- Assert the four acceptance criteria: no label lost across checkpoint/continuation/transforms; `LocalAgent` closed; "why this provider?" answerable from the chain; canary never appears in any captured remote wire body.

**PR title:** `test(egress): §5.6 mixed-session end-to-end incl. context-governor fire (#907 slice 6)`

---

## Cross-cutting notes
- **Constitution:** the label-plane clause is Phase 5 (#910). Phase-2 events carry `default_enforced_rules()` only — no `enforcement_register`/`CONSTITUTION_RULE_IDS` changes.
- **Docs:** update `config-reference.md`/`config-template.yaml` for the manifest floor (Slice 1) and any session-policy fields the routing slice honors (`mode`, `provider_constraint`).
- **Fail-closed everywhere:** unknown provider → remote; ineligible fallback → skipped; unreadable session policy already narrows to `local_only` (Phase 1c). Preserve these.

---

## Open design decisions (need a call before implementing the relevant slice)
1. **Slice 2 — message ids.** Add `id: Option<String>` to `Message` and re-key the sidecar `tool_call_id → msg_id`? Recommended — the `tool_call_id`-only scheme cannot label assistant/user/synthesized messages, so §4.5 (tainted summary) and full transform preservation are impossible without it.
2. **Slice 1b — argument-taint signal.** Handle-reference detection (deterministic) as primary + bounded verbatim-content match as a secondary tripwire; never LLM-inferred classification.
