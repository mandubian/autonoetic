# Agent Revision, Evaluation, and Federation MVP

Status: draft

This document turns the design direction in [design/gateway_primitives_evolution_federation.md](design/gateway_primitives_evolution_federation.md) into an implementation-oriented MVP spec.

Detailed delivery checklist: [plan-agent-revision-evaluation-federation-mvp.md](plan-agent-revision-evaluation-federation-mvp.md)

It is intentionally narrow.

The MVP must deliver a safe, precise base for:

- immutable agent revisions;
- session pinning to immutable revisions;
- evaluation before promotion;
- promotion and rollback by alias move, not in-place mutation;
- future federation compatibility through durable provenance fields.

It must not attempt to solve all of self-learning, training, or cross-node orchestration in one pass.

## 1. Problem Statement

Autonoetic agents are still treated as mutable runtime directories. This creates four problems:

1. an installed agent is mutable in place;
2. sessions do not pin an immutable agent revision;
3. promotion and rollback are not first-class generic operations;
4. future federation has no stable unit for exchange besides ad hoc directories and capsules.

The platform still contains role-specific evolution logic and install-specific approval policy, which conflict with the generic runtime direction.

## 2. MVP Outcome

After this MVP:

- every agent session runs against an immutable revision directory;
- the user-facing `agent_id` resolves through a mutable alias entry to a concrete revision;
- a new candidate revision can be created from an artifact without mutating the live alias;
- a revision's execution closure is explicit: revision bytes plus pinned `runtime.lock` and any pinned layer mounts;
- the first activation of an agent is `agent.revision.create` plus `agent.revision.promote`, not a separate install path;
- eval suites can run against candidate revisions;
- promotion and rollback are alias updates recorded in durable history;
- the gateway stores enough provenance to support later federation, peer import, and training workflows.

## 3. In Scope

### 3.1 Foundation prerequisites

These simplifications must land first or be folded into the same work stream:

- explicit ingress targeting, no gateway default routing;
- generic approval queue, not install-specific approval policy;
- timer plus signal wake model;
- ordered schema migrations for `gateway.db`, not ad hoc table creation;
- removal of role-specific install and promotion gates.

These are preconditions because revision and eval primitives must not inherit planner-specific or evolution-role-specific gateway behavior.

### 3.2 Single-gateway immutable revisions

The MVP supports one gateway instance as the execution authority.

It must:

- create immutable revision directories;
- resolve aliases to revisions;
- pin sessions to revisions;
- run eval suites against a revision;
- promote or rollback aliases.

### 3.3 Federation-ready provenance

The MVP must store origin and trust metadata even if live cross-node execution is deferred.

Required now:

- `origin_node_id` on revisions and evals;
- `source_kind` and `source_ref` for imports and lineage;
- `trust_domain` field on imported objects.

### 3.4 Minimal tool surface

The MVP tool surface is limited to:

- `agent.revision.create`
- `agent.revision.list`
- `agent.revision.inspect`
- `agent.revision.promote`
- `agent.revision.rollback`
- `agent.revision.diff`
- `eval.suite.publish`
- `eval.run`
- `eval.compare`
- `eval.report`

Registry-backed inspection surfaces such as alias listing or promotion history may still exist as CLI or admin HTTP operations in MVP, but they are not required native tools.

Everything else stays out of scope for now.

## 4. Out of Scope

The following are explicitly post-MVP:

- automatic fine-tuning jobs;
- RL training orchestration;
- model registry promotion;
- compatibility mode for mutable `agents/<agent_id>/` runtime directories;
- shadow traffic;
- canary rollout percentages;
- peer placement and execution leases;
- cross-node live session fragments;
- automatic knowledge import from remote peers;
- automated promotion based on eval score alone.

## 5. Core Decisions

### 5.1 `agent_id` remains the user-facing target

Example: `planner.default`

This is what callers pass to `event.ingest` and `agent.spawn` in normal use.

Validation rules:

- `agent_id` must match `^[a-z0-9._-]+$`;
- `@` is reserved for `agent_ref` parsing and is never valid inside `agent_id`;
- invalid target strings fail validation before resolution begins.

### 5.2 `agent_ref` is the immutable execution target

Format:

```text
<agent_id>@<revision_id>
```

Example (full):

```text
planner.default@rev_sha256:4b1a...
```

Example (short, for LLM consumption):

```text
planner.default@rev_abc12345
```

Parsing rules:

- `agent_ref` parses only when a target contains exactly one `@` delimiter and both sides validate as `agent_id` plus `revision_id`;
- a target containing `@` that fails full `agent_ref` parsing is invalid and must not fall back to alias lookup;
- short revision IDs (`rev_<crockford8>`) are resolved via the `short_id_index` table in the gateway store, not via `AgentRef::parse()`.

### 5.3 Revisions are content-addressed directories

The immutable revision directory is the execution root for a session.

The session must never run from a mutable authoring directory.

### 5.4 No bootstrap compatibility path

Repository-local `agents/<agent_id>/` directories may still exist for development and tests, but they are not part of the runtime resolution contract.

The gateway does not auto-import, auto-migrate, or execute those directories on demand.

The first runnable revision must be created explicitly from an immutable agent bundle artifact.

Development helpers may package an authoring directory into an `AgentBundle` artifact for tests, local workflows, or one-time migration seeding, but that packaging step is explicit and outside runtime resolution.

### 5.5 First activation is create plus promote, not `agent.install`

Seeding a new logical agent is:

1. build or upload an `agent_bundle` artifact;
2. call `agent.revision.create`;
3. call `agent.revision.promote` for alias `agent_id`.

`agent.install` is removed rather than wrapped.

### 5.6 Disclosure reduces to restricted vs non-restricted output

The MVP disclosure model is binary.

Gateway filtering no longer depends on path-taxonomy classes such as `public`, `internal`, `confidential`, and `secret`.

Instead:

- tool execution metadata may mark a result as `restricted_output = true`;
- unrestricted output may be returned normally;
- restricted output is recorded as tainted for the session and is never echoed verbatim back to the caller;
- reply filtering stays deterministic and exact-match based, but uses one redaction class rather than a hierarchy.

Migration rules:

- existing manifest-level disclosure rules are transitional input only;
- legacy `public` maps to unrestricted output;
- any legacy non-public class maps to restricted output;
- new bundles should rely on tool metadata and explicit capability policy rather than path-based disclosure defaults.

### 5.7 Revision closure includes layers

If the source artifact carries layers, those layer mounts are part of the revision's execution closure.

They must be pinned by `runtime.lock` or equivalent locked metadata, not discovered only by re-scanning the source artifact later.

### 5.8 Promotion and rollback are alias updates

No in-place file mutation is allowed as part of promotion.

Promotion is:

- validate candidate revision;
- optionally validate required eval run;
- update alias to new revision;
- write durable promotion history.

Rollback is the same operation in reverse.

## 6. Reference Formats

### 6.1 `revision_id`

Format:

```text
rev_sha256:<64 lowercase hex>
```

The digest is computed over canonical revision content:

- relative file paths sorted ascending;
- normalized path separator `/`;
- raw file bytes for agent files and `SKILL.md`;
- canonical serialized `runtime.lock`, including pinned layer mounts, after parse and validation.

For MVP, canonical lock serialization means deterministic JSON emitted from the validated in-memory `RuntimeLock` structure with fixed struct field order and no insignificant whitespace.

### 6.2 `agent_ref`

Format:

```text
<agent_id>@rev_sha256:<64 lowercase hex>
```

### 6.3 `suite_id`

Format:

```text
suite-<12 lowercase hex>
```

### 6.4 `eval_run_id`

Format:

```text
eval-<12 lowercase hex>
```

### 6.5 `promotion_id`

Format:

```text
prom-<12 lowercase hex>
```

## 7. Filesystem Layout

The gateway runtime filesystem layout becomes:

```text
.gateway/
  revisions/
    agents/
      planner.default/
        rev_sha256_4b1a.../
          SKILL.md
          runtime.lock
          ... agent files ...
      coder.default/
        rev_sha256_8f20.../
          ...
  eval/
    reports/
      eval-abc123.json
  capsules/
  sessions/
  content/
  scheduler/
```

Notes:

- SQLite remains the source of truth for metadata;
- revision directories store immutable bytes only;
- repository-local `agents/<agent_id>/` directories may still exist for authoring or tests, but they are outside runtime resolution.

## 8. Rust Data Model

Add new types under `autonoetic-types`.

### 8.1 New file: `autonoetic-types/src/agent_revision.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRef {
    pub agent_id: String,
    pub revision_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRevisionStatus {
    Candidate,
    Ready,
    Archived,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRevisionRecord {
    pub revision_id: String,
    pub agent_id: String,
    pub base_revision_id: Option<String>,
    pub artifact_id: Option<String>,
    pub content_digest: String,
    pub runtime_lock_hash: String,
    pub manifest_hash: String,
    pub created_at: String,
    pub created_by_type: String,
    pub created_by_id: String,
    pub source_kind: String,
    pub source_ref: Option<String>,
    pub origin_node_id: String,
    pub trust_domain: String,
    pub status: AgentRevisionStatus,
    pub metadata_json: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAliasRecord {
    pub alias_id: String,
    pub agent_id: String,
    pub revision_id: String,
    pub updated_at: String,
    pub updated_by_type: String,
    pub updated_by_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAgentBinding {
    pub session_id: String,
    pub root_session_id: String,
    pub requested_target: String,
    pub alias_id: Option<String>,
    pub agent_id: String,
    pub revision_id: String,
    pub runtime_lock_hash: String,
    pub home_node_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromotionKind {
    Promote,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromotionRecord {
    pub promotion_id: String,
    pub kind: PromotionKind,
    pub alias_id: String,
    pub agent_id: String,
    pub previous_revision_id: Option<String>,
    pub new_revision_id: String,
    pub source_eval_run_id: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
    pub created_by_type: String,
    pub created_by_id: String,
    pub origin_node_id: String,
}
```

Revision status semantics:

- `candidate`: default result of `agent.revision.create`; runnable by explicit `agent_ref`, but not yet active through alias movement;
- `ready`: optional promotable marker for deployments that add an explicit pre-promotion validation or review step;
- `rejected`: not promotable and not eligible for new normal session launches;
- `archived`: retained for audit and rollback history but not eligible for ordinary promotion.

MVP promotion may target revisions in `candidate` or `ready` state. The schema keeps `ready` so stricter governance can be added later without redesign.

### 8.2 New file: `autonoetic-types/src/evaluation.rs`

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunStatus {
    Queued,
    Running,
    Passed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalSuiteRecord {
    pub suite_id: String,
    pub name: String,
    pub description: String,
    pub spec_json: serde_json::Value,
    pub created_at: String,
    pub created_by_type: String,
    pub created_by_id: String,
    pub origin_node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalRunRecord {
    pub eval_run_id: String,
    pub suite_id: String,
    pub subject_agent_id: String,
    pub subject_revision_id: String,
    pub baseline_revision_id: Option<String>,
    pub status: EvalRunStatus,
    pub queued_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub summary_json: serde_json::Value,
    pub report_handle: Option<String>,
    pub origin_node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalCaseResultRecord {
    pub eval_run_id: String,
    pub case_id: String,
    pub status: String,
    pub score: Option<f64>,
    pub session_id: Option<String>,
    pub notes: Option<String>,
    pub output_json: serde_json::Value,
}
```

### 8.3 Extend `autonoetic-types/src/runtime_lock.rs`

Layers already exist as opaque immutable dependency bundles. Revisions must pin them explicitly in the runtime closure.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockedLayerMount {
    pub layer_id: String,
    pub digest: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLock {
    pub gateway: LockedGateway,
    pub sdk: LockedSdk,
    pub sandbox: LockedSandbox,
    #[serde(default)]
    pub dependencies: Vec<LockedDependencySet>,
    #[serde(default)]
    pub artifacts: Vec<LockedArtifact>,
    #[serde(default)]
    pub layers: Vec<LockedLayerMount>,
}
```

Rules:

- `layers` is the pinned layer closure required to execute the revision;
- revisions without layers keep this empty;
- `runtime_lock_hash` is computed over the normalized serialized lock structure, including `layers`, not over raw source-file formatting bytes;
- the same canonical JSON form used for hashing must be what revision materialization writes back as the pinned lock representation.

## 9. SQLite Data Model

All metadata is stored in `gateway.db`.

### 9.1 Schema versioning

This MVP introduces enough new persistence that `gateway.db` must move to ordered migrations.

Required metadata table:

#### `schema_migrations`

| Column | Type | Notes |
|---|---|---|
| `version` | INTEGER PK | monotonic schema version |
| `name` | TEXT NOT NULL | migration label |
| `applied_at` | TEXT NOT NULL | RFC3339 |

Rules:

- startup reads the highest applied version and executes any later migrations in order;
- fresh databases bootstrap by running the same ordered migration list from version `0`, not by ad hoc `CREATE TABLE IF NOT EXISTS` batches;
- each migration runs transactionally;
- the gateway must refuse startup if the database version is newer than the binary understands.

### 9.2 New tables

#### `agent_revisions`

| Column | Type | Notes |
|---|---|---|
| `revision_id` | TEXT PK | `rev_sha256:...` |
| `agent_id` | TEXT NOT NULL | logical agent name |
| `base_revision_id` | TEXT NULL | lineage |
| `artifact_id` | TEXT NULL | source artifact when created from artifact |
| `content_digest` | TEXT NOT NULL | same digest family as revision id |
| `runtime_lock_hash` | TEXT NOT NULL | reproducibility binding including pinned layer mounts |
| `manifest_hash` | TEXT NOT NULL | quick integrity field |
| `created_at` | TEXT NOT NULL | RFC3339 |
| `created_by_type` | TEXT NOT NULL | `agent`, `user`, `system`, `peer` |
| `created_by_id` | TEXT NOT NULL | actor id |
| `source_kind` | TEXT NOT NULL | `artifact`, `capsule_import`, `peer_import` |
| `source_ref` | TEXT NULL | artifact id, capsule id, or peer ref |
| `origin_node_id` | TEXT NOT NULL | future federation provenance |
| `trust_domain` | TEXT NOT NULL | `local`, `partner`, `foreign`, `untrusted` |
| `status` | TEXT NOT NULL | `candidate`, `ready`, `archived`, `rejected` |
| `metadata_json` | TEXT NOT NULL | serialized JSON |

Indexes:

- `(agent_id, created_at desc)`
- `(agent_id, status)`
- `UNIQUE(agent_id, content_digest)`

Notes:

- there is no `bootstrap_dir` `source_kind` in MVP because mutable authoring directories are never runtime sources.

#### `agent_aliases`

| Column | Type | Notes |
|---|---|---|
| `alias_id` | TEXT PK | MVP default equals `agent_id` |
| `agent_id` | TEXT NOT NULL | logical owner |
| `revision_id` | TEXT NOT NULL | target revision |
| `updated_at` | TEXT NOT NULL | RFC3339 |
| `updated_by_type` | TEXT NOT NULL | actor kind |
| `updated_by_id` | TEXT NOT NULL | actor id |
| `reason` | TEXT NULL | free text |

Constraint:

- `UNIQUE(agent_id)` in MVP, because each logical agent has exactly one mutable alias.

#### `session_agent_bindings`

| Column | Type | Notes |
|---|---|---|
| `session_id` | TEXT PK | exact session |
| `root_session_id` | TEXT NOT NULL | root grouping |
| `requested_target` | TEXT NOT NULL | original `agent_id` or explicit `agent_ref` |
| `alias_id` | TEXT NULL | resolved alias when session started from alias lookup |
| `agent_id` | TEXT NOT NULL | logical id |
| `revision_id` | TEXT NOT NULL | pinned immutable revision |
| `runtime_lock_hash` | TEXT NOT NULL | pinned runtime closure |
| `home_node_id` | TEXT NOT NULL | future distributed placement |
| `created_at` | TEXT NOT NULL | RFC3339 |

Rules:

- `alias_id` is null for sessions started from an explicit `agent_ref` that bypassed alias lookup;
- eval sessions against candidate revisions typically use null `alias_id`.

#### `promotion_history`

| Column | Type | Notes |
|---|---|---|
| `promotion_id` | TEXT PK | `prom-...` |
| `kind` | TEXT NOT NULL | `promote` or `rollback` |
| `alias_id` | TEXT NOT NULL | alias moved |
| `agent_id` | TEXT NOT NULL | logical id |
| `previous_revision_id` | TEXT NULL | old target |
| `new_revision_id` | TEXT NOT NULL | new target |
| `source_eval_run_id` | TEXT NULL | eval justification |
| `reason` | TEXT NULL | free text |
| `created_at` | TEXT NOT NULL | RFC3339 |
| `created_by_type` | TEXT NOT NULL | actor kind |
| `created_by_id` | TEXT NOT NULL | actor id |
| `origin_node_id` | TEXT NOT NULL | provenance |

#### `eval_suites`

| Column | Type | Notes |
|---|---|---|
| `suite_id` | TEXT PK | `suite-...` |
| `name` | TEXT NOT NULL | display name |
| `description` | TEXT NOT NULL | short text |
| `spec_json` | TEXT NOT NULL | serialized suite spec |
| `created_at` | TEXT NOT NULL | RFC3339 |
| `created_by_type` | TEXT NOT NULL | actor kind |
| `created_by_id` | TEXT NOT NULL | actor id |
| `origin_node_id` | TEXT NOT NULL | provenance |

#### `eval_runs`

| Column | Type | Notes |
|---|---|---|
| `eval_run_id` | TEXT PK | `eval-...` |
| `suite_id` | TEXT NOT NULL | FK by convention |
| `subject_agent_id` | TEXT NOT NULL | logical id |
| `subject_revision_id` | TEXT NOT NULL | candidate or target |
| `baseline_revision_id` | TEXT NULL | optional comparison baseline |
| `status` | TEXT NOT NULL | `queued`, `running`, `passed`, `failed`, `cancelled` |
| `queued_at` | TEXT NOT NULL | RFC3339 |
| `started_at` | TEXT NULL | RFC3339 |
| `completed_at` | TEXT NULL | RFC3339 |
| `summary_json` | TEXT NOT NULL | rollup fields |
| `report_handle` | TEXT NULL | content handle for full report |
| `origin_node_id` | TEXT NOT NULL | provenance |

Indexes:

- `(subject_agent_id, queued_at desc)`
- `(subject_revision_id)`

#### `eval_case_results`

| Column | Type | Notes |
|---|---|---|
| `eval_run_id` | TEXT NOT NULL | parent run |
| `case_id` | TEXT NOT NULL | stable within suite |
| `status` | TEXT NOT NULL | `passed`, `failed`, `error` |
| `score` | REAL NULL | optional numeric score |
| `session_id` | TEXT NULL | spawned session if applicable |
| `notes` | TEXT NULL | short explanation |
| `output_json` | TEXT NOT NULL | serialized output |

Primary key:

- `(eval_run_id, case_id)`

## 10. Public Tool Contracts

All new tools are native tools.

### 10.1 `agent.revision.create`

Purpose: create a candidate immutable revision from an artifact.

Input:

```json
{
  "agent_id": "planner.default",
  "artifact_id": "art_...",
  "base_ref": "planner.default@rev_sha256:...",
  "change_summary": "Tighten delegation rules and retry guidance",
  "metadata": {}
}
```

Output:

```json
{
  "ok": true,
  "agent_ref": "planner.default@rev_sha256:...",
  "revision_id": "rev_sha256:...",
  "agent_id": "planner.default",
  "status": "candidate"
}
```

Rules:

- `artifact_id` must resolve to `ArtifactKind::AgentBundle` and contain `SKILL.md`;
- manifest `agent.id` must equal requested `agent_id`;
- extract and reuse shared manifest validation logic before removing `agent.install`;
- any artifact layers must be copied into the locked runtime closure for the new revision;
- revision materialization must write the normalized `runtime.lock` form that was hashed for identity;
- do not mutate alias state;
- store lineage to `base_ref` if provided;
- if no alias exists yet, creation is still allowed, but activation requires a later promote step.

### 10.2 `agent.revision.list`

Input:

```json
{ "agent_id": "planner.default" }
```

Output: ordered newest first.

### 10.3 `agent.revision.inspect`

Input:

```json
{ "agent_ref": "planner.default@rev_sha256:..." }
```

Output includes:

- manifest summary;
- lineage;
- runtime lock hash;
- creation metadata;
- current aliases pointing to this revision;
- latest eval runs.

### 10.4 `agent.revision.promote`

Input:

```json
{
  "alias_id": "planner.default",
  "agent_ref": "planner.default@rev_sha256:...",
  "reason": "passed suite-basic-planner",
  "required_eval_run_id": "eval-abc123"
}
```

Rules:

- alias target agent id must match `agent_ref.agent_id`;
- if `required_eval_run_id` is provided, the run must exist and be `passed` for the same revision;
- write one `promotion_history` record;
- update alias atomically.

### 10.5 `agent.revision.rollback`

Input:

```json
{
  "alias_id": "planner.default",
  "target_revision_id": "rev_sha256:...",
  "reason": "candidate produced worse summaries"
}
```

Rules:

- `target_revision_id` must belong to the same logical agent;
- if omitted, rollback targets the immediately previous revision from promotion history.

### 10.6 `eval.suite.publish`

Input:

```json
{
  "name": "suite-basic-planner",
  "description": "Basic routing and delegation checks",
  "spec": {
    "cases": [
      {
        "case_id": "simple_chat",
        "message": "Summarize this task in one sentence",
        "assertions": {
          "reply_max_chars": 200,
          "reply_contains_all": ["task"]
        }
      }
    ]
  }
}
```

Minimal assertion language for MVP:

- `reply_contains_all`
- `reply_contains_none`
- `reply_max_chars`
- `artifacts_min`
- `artifacts_max`

### 10.7 `eval.run`

Input:

```json
{
  "agent_ref": "planner.default@rev_sha256:...",
  "suite_id": "suite-abc123",
  "baseline_ref": "planner.default@rev_sha256:..."
}
```

Output:

```json
{
  "ok": true,
  "eval_run_id": "eval-abc123",
  "status": "queued"
}
```

MVP behavior:

- creates durable eval run records;
- scheduler executes cases asynchronously;
- each case spawns the subject revision in an isolated eval session;
- `baseline_ref`, when supplied, is stored as report metadata only and does not change case assertions or pass/fail semantics in MVP;
- final report is written to content store and referenced by `report_handle`.

### 10.8 `eval.report`

Input:

```json
{ "eval_run_id": "eval-abc123" }
```

Output includes summary fields and `report_handle`.

### 10.9 `agent.revision.diff`

Input:

```json
{
  "from_ref": "planner.default@rev_sha256:...",
  "to_ref": "planner.default@rev_sha256:..."
}
```

Output includes:

- `changed` boolean;
- summary counts (`added`, `removed`, `modified`);
- `added` and `removed` file lists;
- `modified` entries with per-file digest and size changes.

Rules:

- both refs resolve through the same registry-backed resolver as other revision tools;
- diff operates on immutable materialized revision directories only;
- output ordering must be deterministic.

### 10.10 `eval.compare`

Input:

```json
{
  "suite_id": "suite-abc123",
  "baseline_ref": "planner.default@rev_sha256:...",
  "candidate_ref": "planner.default@rev_sha256:...",
  "queue_if_missing": true
}
```

Output:

- `status = "completed"` when both baseline and candidate runs are available, with regression/improvement summary and changed case statuses;
- `status = "queued"` when one or both runs are missing and were queued for execution.

Rules:

- baseline and candidate refs must resolve to the same logical agent in MVP;
- if completed runs already exist for each revision and suite, reuse them;
- when missing runs are queued, return queued run ids and require a later `eval.compare` call to obtain the completed report.

## 11. Capability Model Changes

Add the following capability variants to the capability model:

```rust
AgentRevision { patterns: Vec<String> }
Evaluation { patterns: Vec<String> }
ApprovalQueue { scopes: Vec<String> }
SchedulerSignal { patterns: Vec<String> }
```

Notes:

- `patterns` follow the same pattern semantics as the rest of the capability model;
- `AgentRevision.patterns` match logical agent ids, alias ids, or full `agent_ref` prefixes;
- `Evaluation.patterns` match suite names for publish operations, and match suite ids or subject agent ids for run/report/compare operations;
- `ApprovalQueue.scopes` are exact scope names, not free-form globs;
- `SchedulerSignal.patterns` match named signal channels only;
- `ApprovalQueue` and `SchedulerSignal` are prerequisites from the simplification work;
- model training and peer federation capabilities are deferred.

## 12. Target Runtime Components

This MVP is defined by target responsibilities, not by preserving today's file split.

### 12.1 Control plane

| Component | Responsibility |
|---|---|
| Ingress validator | Require explicit `target` and validate `agent_id` or `agent_ref` before execution starts |
| Approval queue | Govern promotions and other policy-gated actions through generic approvals |
| Signal scheduler | Support only timer and named signal wakes |
| Revision registry | Persist revision metadata, lineage, status, runtime lock hash, and provenance |
| Alias registry | Persist one mutable alias per logical agent in MVP |
| Eval registry | Persist suites, runs, case results, and report handles |

### 12.2 Execution plane

| Component | Responsibility |
|---|---|
| Revision materializer | Build immutable revision directories from `AgentBundle` artifacts and validated locks |
| Agent resolver | Resolve alias or explicit `agent_ref` to a loaded immutable revision |
| Session binder | Pin requested target, resolved revision, and runtime closure before any turn executes |
| Execution closure resolver | Hydrate pinned layer mounts from `runtime.lock` and never discover dependencies implicitly at run time |
| Eval runner | Execute eval cases through the same execution permits and sandbox controls as ordinary sessions |

### 12.3 Exchange plane

| Component | Responsibility |
|---|---|
| Agent bundle artifact | Serve as the sole source payload for revision creation |
| Layer store | Provide opaque immutable dependency bundles with no language-specific semantics |
| Capsule export/import | Later phase exports revision plus pinned closure and provenance |
| Provenance envelope | Carry `origin_node_id`, `trust_domain`, `source_kind`, and `source_ref` across exchangeable records |

### 12.4 Delegation boundary

This MVP deliberately separates agent-authored nondeterminism from gateway-owned execution guarantees.

Agents may:

- author candidate `AgentBundle` inputs;
- propose revision metadata and change summaries;
- publish eval suites and request eval runs;
- request promote or rollback and interpret the resulting evidence.

The gateway must:

- validate and materialize immutable revisions;
- compute canonical revision and runtime-lock identity;
- resolve aliases and bind sessions to pinned revisions;
- enforce capability checks, approval policy, disclosure filtering, and runtime permits;
- record durable eval, promotion, and provenance state.

Future portable export follows the same rule: an agent may request export, but the gateway must assemble and attest the export unit.

## 13. Resolver Contract

### 13.1 Loaded agent payload

```rust
pub struct LoadedAgent {
    pub agent_id: String,
    pub alias_id: Option<String>,
    pub agent_ref: String,
    pub revision_id: String,
    pub runtime_lock_hash: String,
    pub dir: PathBuf,
    pub manifest: AgentManifest,
    pub instructions: String,
}
```

### 13.2 Resolver operations

```rust
pub fn resolve_sync(&self, target: &str) -> anyhow::Result<LoadedAgent>;
pub async fn resolve(&self, target: &str) -> anyhow::Result<LoadedAgent>;
pub fn list_aliases(&self) -> anyhow::Result<Vec<AgentAliasRecord>>;
```

Resolution rules:

1. if `target` contains `@`, it must parse as a valid `agent_ref` or fail validation;
2. if `target` parses as a full `agent_ref` (64-char hex), load that exact revision;
3. if `target` contains `@` with a short revision ID (`rev_<crockford>`), resolve via `short_id_index` table;
4. else treat `target` as alias id;
5. if alias exists, load alias target revision;
6. else return not found.

Additional rules:

- there is no mutable-directory fallback;
- explicit `agent_ref` resolution may return candidate revisions that are not pointed to by any alias;
- list operations expose alias state, not authoring directories.

### 13.3 Revision creation contract

Revision creation requires an immutable `AgentBundle` artifact plus a validated runtime closure.

Creation writes immutable revision bytes and metadata only. It does not move aliases or activate the revision.

### 13.4 Seeding and migration contract

Because there is no bootstrap compatibility path, MVP delivery must include an explicit seeding path for tests, CLI workflows, and existing deployments.

Required support:

- an admin or test helper that packages an authoring directory into an `AgentBundle` artifact and then performs create plus promote;
- CLI surfaces that replace `agent.install` with explicit revision creation and promotion;
- an operator runbook for first-time seeding of existing logical agents before ingress depends on alias resolution.

## 14. Session Pinning Rules

At session start:

1. resolve target alias or explicit `agent_ref` to `LoadedAgent`;
2. compute a deterministic `root_session_id`;
3. insert `session_agent_bindings` if absent, storing `requested_target` and optional `alias_id`;
4. if present and session is already bound, use the persisted binding instead of re-resolving alias;
5. pass the immutable revision directory and pinned runtime closure into the executor.

This ensures later alias promotion does not change an already running session.

Required behavior on resume paths:

- approval continuation resumes against the pinned revision;
- checkpoint resume resumes against the pinned revision;
- workflow child retries resume against the pinned revision.

Eval sessions against explicit candidate revisions bind `requested_target` to the full `agent_ref` and keep `alias_id = null`.

## 15. Eval Execution Model

MVP evals run through the same execution pipeline as ordinary sessions.

Per case:

1. create session id `eval/<eval_run_id>/<case_id>`;
2. call `spawn_agent_once()` with explicit `agent_ref`;
3. capture reply, artifacts, and any error;
4. evaluate assertions;
5. persist `eval_case_results`;
6. aggregate into `eval_runs.summary_json`;
7. write full report to content store and save `report_handle`.

### 15.1 Concurrency and permits

Eval execution must consume the same execution permits and resource controls as normal runtime work.

Rules:

- eval cases must not bypass the global spawn or sandbox concurrency limits;
- default per-run case concurrency is `1` in MVP;
- the per-run limit applies within one `eval_run_id`; multiple eval runs may still interleave through the same global permit pool;
- eval workers must not reserve dedicated execution capacity away from ordinary interactive sessions;
- later higher concurrency must remain bounded by the same global semaphores used for regular sessions.

Do not route eval suites through the workflow domain in MVP. Eval is its own queue and report domain.

## 16. Promotion Rules

Promotion is allowed only when all of the following hold:

1. target revision exists and is in `candidate` or `ready` state;
2. `alias_id` belongs to the same logical agent;
3. caller has `AgentRevision` capability for that alias;
4. if `required_eval_run_id` is provided, that eval run passed and matches target revision;
5. any policy-level approval requirement has been satisfied via generic approval queue.

Alias update must be atomic at the SQLite level.

After promotion:

- already running sessions continue on their pinned revision;
- sessions launched from explicit `agent_ref` stay pinned to that exact revision;
- new sessions resolve to the new alias target.

## 17. Delivery Phases

Detailed implementation checklist: [plan-agent-revision-evaluation-federation-mvp.md](plan-agent-revision-evaluation-federation-mvp.md)

### Phase 0: Gateway Contract Simplification

Outcome:

- explicit ingress targeting;
- timer plus signal scheduler model;
- generic approval queue;
- binary restricted-output disclosure model;
- removal of install-specific and role-specific gateway behavior.

### Phase 1: Revision Registry and Resolver

Outcome:

- immutable revision storage and alias registry;
- ordered schema-backed revision metadata;
- explicit runtime closure with pinned layer mounts;
- resolver and session binding flow based only on alias or `agent_ref`.

### Phase 2: Promotion and Rollback

Outcome:

- alias moves become the only activation mechanism;
- promotion history is durable and auditable;
- running sessions remain pinned while new sessions see the alias change.

### Phase 3: Eval Suite MVP

Outcome:

- durable suite and run metadata;
- isolated eval execution through ordinary runtime limits;
- promotion can require a passed eval run.

### Phase 4: Federation-Ready Provenance

Outcome:

- every revision, promotion, and eval carries durable provenance;
- export and import concepts preserve revision identity plus runtime closure.

Phase-4 contract details:

- Provenance is mandatory on durable records; it is never inferred from directory layout or local defaults at read time.
- Future capsule manifests reserve explicit fields for:
  - `agent_ref` (full immutable identity),
  - pinned runtime closure identity (`runtime_lock_hash`),
  - `included_layers` (reserved for hermetic export planning).
- Capsule planning keys on immutable revision identity and pinned closure, not mutable directory names.
- Imported foreign revisions are created in `candidate` lifecycle status by default.
- Imported revisions and eval artifacts must carry explicit `trust_domain` (`partner`, `foreign`, `untrusted`) plus original `source_kind`/`source_ref`.
- Import never auto-promotes aliases; activation is always an explicit later alias move.
- Serialization and deserialization of provenance-bearing records must round-trip without lossy field drops.
- Export planning and import parsing preserve provenance fields end-to-end.
- Foreign objects remain distinguishable from local objects through `origin_node_id` + `trust_domain` + source metadata.

## 18. Testing Plan

Minimum test coverage required:

1. explicit agent bundle seed creates first revision and alias;
2. explicit `agent_ref` resolution bypasses alias lookup;
3. session pinning survives approval suspend and resume;
4. promotion affects only new sessions;
5. rollback restores previous alias target;
6. eval run records case results and report handle;
7. promotion with mismatched eval run id fails validation;
8. layered agent bundles produce pinned `runtime.lock` layer mounts;
9. changing pinned layer mounts changes revision identity even when agent files do not;
10. malformed targets containing `@` fail validation without alias fallback;
11. fresh and upgraded databases both apply ordered schema migrations correctly;
12. imported or foreign revision metadata survives round trip serialization.

## 19. Future Extensions After MVP

These extend the same data model and do not require redesign if MVP is implemented as specified:

- `eval.shadow`, `eval.canary`;
- `trajectory.*` and `dataset.*` built on eval and revision lineage;
- `training.*` and `model.revision.*` using the same promotion model;
- a portable autonomous agent export format that packages revision bytes, canonical runtime closure, included layers, and provenance for remote execution, distinct from `AgentBundle`;
- peer registry, replication jobs, and execution leases;
- cross-node sessions composed from per-node fragments.

## 20. Implementation Summary

The minimum coherent slice is:

1. simplify the gateway so it stops owning role semantics;
2. make revisions immutable and alias-driven;
3. make the execution closure explicit through `runtime.lock` and pinned layer mounts;
4. add eval before promotion;
5. record provenance now so federation later is additive, not disruptive.

That is the MVP.
