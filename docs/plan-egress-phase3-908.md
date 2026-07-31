# Implementation plan — #908 [Phase 3] Memory, digest, curator labeling

**Tracking issue:** [mandubian/autonoetic#908](https://github.com/mandubian/autonoetic/issues/908)
RFC: [`docs/rfc/data-envelopes-egress-localization.md`](rfc/data-envelopes-egress-localization.md) §6, §9.1.
Parent [#903](https://github.com/mandubian/autonoetic/issues/903). Depends on merged Phase 2 (#907 / #940 / #941).

## Where Phase 2 left the label plane

- Session taint is durable (`session_egress_taint`, agent_messages labeled).
- Tool-result / response / compression / routing labels exist in-session.
- **No** `egress_label` on `MemoryObject` (`autonoetic-types/src/memory.rs`), `memories` table, or `execution_traces`.
- `knowledge_store` (`runtime/tools/knowledge.rs`) ignores `_run_context` / session taint.
- `build_memory_context_snippet` (`runtime/context.rs`) injects content into prompts with no provider-class filter.
- `post_session_digest` (`runtime/post_session_digest.rs`) always uses configured digest preset; stores Global memories with no label.
- `SCHEMA_VERSION_LATEST = 75` — next migration is **v76**.

### Locked RFC decisions (do not re-litigate)

- Visibility ⊥ egress label (both enforced).
- Legacy unlabeled → **configured default**; product default **`unrestricted`**; fail-closed config option treats legacy as `no_remote_model` until swept.
- `local_only` includes `MemoryPersist`.
- `promote_to_skill` from `local_only` evidence → **mechanical refuse** unless declassified.
- Every sweep/manual reclassification emits `egress.relabel`.

---

## Slicing (each row is one PR)

| Slice | Scope | Blast radius | Depends |
|---|---|---|---|
| **0. Plan doc** | `docs/plan-egress-phase3-908.md` | docs only | — |
| **1. Memory label vertical** | `MemoryObject.egress_label`, v76 `memories.egress_label_json`, store-time intersection on `knowledge_store`, request-time filter on recall/search/`build_memory_context_snippet` | types + migrate + knowledge + context | 0 |
| **2. Execution traces** | label on `ExecutionTraceRecord` + column; write from tool-result label; filter `execution_search` | migrate + tool_call_processor + execution tool | 1 (shared filter helper) |
| **3. Digest** | tainted session → local digest preset (or indications); digest memories inherit intersection; `digest_query` filters | post_session_digest | 1 |
| **4. Other re-entry surfaces** | `observability_read`, `session_peek`, `wiki_get` filter/indicate by target sink | tools | 1–2 |
| **5. Curator graduation gate** | mechanical refuse `promote_to_skill` when evidence taint is `local_only` (unless declassified) | curator_journal / response_validation | 1 |
| **6. Relabel sweep + migration modes** | `gateway memory relabel` (traces too), config knob, `egress.relabel` events, both legacy modes tested | CLI + store | 1–2 |
| **7. Acceptance e2e** | no `local_only` stored content reaches remote via recall/search/digest/curator | tests/egress | 1–6 |

**Recommended order:** 0 → 1 → 2 → 3 → 5 → 4 → 6 → 7.

Rationale: Slice 1 closes the highest-risk hole (prompt priming + knowledge). Slice 2 closes the “full stdout” hole. Digest (3) and curator (5) are the cross-session distillers. Remaining read surfaces (4) and operator sweep (6) can land after the core write/read path exists. Acceptance last.

---

## Shared design (all slices)

**Filter helper** (`runtime/egress_stored.rs`):

- `resolve_stored_label(stored, cfg)` — `None` / missing column → `cfg.legacy_unlabeled` (default `unrestricted`; fail-closed = `no_remote_model`).
- `filter_or_indicate_for_sink(content, label, sink, verbosity)` — drop/indicate when label excludes sink.
- Target sink from the completion’s effective `EgressClass` (same as chokepoint), threaded into snippet/tool paths — **not** LLM judgment.

**Store-time intersection:** `session_accumulated_taint` (or `NativeToolRunContext.egress_taint`) at `knowledge_store` and at `create_execution_trace` (tool-result label when present).

**Fail closed on missing taint context for store:** if store path cannot see session labels, read `session_egress_taint` from the store when the in-memory map is unavailable — error if store read fails rather than silently widening.

---

## Slice 1 — Memory label vertical

- `MemoryObject.egress_label` with serde default unrestricted.
- Migration **v76**: `memories.egress_label_json` + `execution_traces.egress_label_json` (both columns in one migration so Slice 2 does not need a second bump).
- Config: `egress.legacy_unlabeled: unrestricted | no_remote_model` on `EgressConfig`.
- `knowledge_store` intersects session taint onto the memory before upsert.
- `knowledge_recall` / `knowledge_search` / `build_memory_context_snippet` filter by target sink.

**PR title:** `feat(egress): MemoryObject egress_label + store/recall filter (#908 slice 1)`

---

## Slice 2 — Execution traces

- `ExecutionTraceRecord.egress_label`; write from tool-result label in `tool_call_processor`.
- `execution_search` filters/indicates by target sink.

**PR title:** `feat(egress): label execution_traces + filter execution_search (#908 slice 2)`

---

## Slice 3 — `post_session_digest`

- Session taint excludes `RemoteModel` → digest on local preset (or refuse if none configured).
- Digest memories inherit intersection; `digest_query` uses stored-content filter.

**PR title:** `feat(egress): taint-aware post_session_digest (#908 slice 3)`

---

## Slice 4 — Remaining re-entry surfaces

- `observability_read`, `session_peek`, `wiki_get`: shared filter on provider-bound payloads.
- Wiki: optional frontmatter `egress_label`; absent → legacy default.

**PR title:** `feat(egress): filter observability/session_peek/wiki by egress label (#908 slice 4)`

---

## Slice 5 — Curator graduation gate

- Mechanical refuse `promote_to_skill` unless evidence label allows `RemoteModel` or an explicit declassification grant exists.

**PR title:** `feat(egress): refuse local_only promote_to_skill without declassification (#908 slice 5)`

---

## Slice 6 — Relabel sweep + migration modes

- CLI: `autonoetic gateway memory relabel` (memories + execution_traces).
- Emit `egress.relabel`; test both legacy modes.

**PR title:** `feat(egress): gateway memory relabel + legacy unlabeled modes (#908 slice 6)`

---

## Slice 7 — Acceptance

- Egress domain binary: tainted store → remote paths never see canary; local does; relabel audit visible.

**PR title:** `test(egress): phase 3 stored-content acceptance (#908 slice 7)`

---

## Cross-cutting notes

- No constitution / enforcement-register changes in Phase 3 (clause remains Phase 5 #910); events keep `default_enforced_rules()`.
- Fail-closed everywhere: unknown/missing store taint → error on write; unlabeled legacy uses configured default only.
