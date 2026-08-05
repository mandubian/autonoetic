# Gateway Architecture (Rust Crate `autonoetic-gateway`)

> **Audience**: developers navigating this crate.  
> **Scope**: `autonoetic-gateway/src/` — the core runtime, not the CLI binary or SDKs.

---

## 1. Responsibilities

The gateway is the **high-privilege execution boundary** between LLM-powered agents and the outside world. It enforces separation of powers: agents propose actions, the gateway executes them under mechanical guardrails.

| Responsibility | Enforced by |
|---|---|
| **Accept & route** JSON-RPC calls from clients and agents | `server/` + `router.rs` |
| **Validate capabilities** before every privileged operation | `policy.rs` |
| **Execute agent reasoning loops** with tool dispatch, loop guard, budget tracking | `runtime/lifecycle.rs` |
| **Dispatch all 40+ native tools** (content, sandbox, revision, approval, web, etc.) | `runtime/tools/mod.rs` |
| **Sandbox execution** via bubblewrap/docker/microvm/wasm | `sandbox.rs` |
| **Persist all transactional state** (sessions, approvals, workflows, schedules) | `scheduler/gateway_store/` |
| **Approve or reject** privileged actions with 5-layer dedup, HMAC continuations, flood cap | `scheduler/approval.rs` + runtime approval path |
| **Schedule background work** (agent wakes, cron jobs, reclamation, approval timeout) | `scheduler.rs` + `scheduler/` submodules |
| **Audit trail** via immutable causal chain (JSONL + SQLite mirror) | `causal_chain.rs` |
| **Secret injection** — vault-stored credentials, never exposed to LLM | `vault.rs` |
| **Sentinel sweeps** — security auditing of gateway state | `sentinel/` |
| **Constitution enforcement** — digest verification, lock integrity | `constitution_digest.rs` |
| **Federation** — cross-gateway message routing via OFP | `server/ofp.rs` |
| **Fast scheduler sidecar** — sub-second tick for time-sensitive work | `scheduler/fast_scheduler.rs` |

---

## 2. Module Map

```
autonoetic-gateway/src/
│
├── lib.rs                         # Crate root: declares 28+ modules, re-exports key types
│
├── server/                        # Transport layer — ingress for all external communication
│   ├── mod.rs                     #   GatewayServer struct, run() — main daemon entry point
│   ├── jsonrpc.rs                 #   JSON-RPC 2.0 transport (Unix socket + shared_secret)
│   ├── http.rs                    #   HTTP server for content store access (optional, Bearer auth)
│   ├── ofp.rs                     #   OpenFang Protocol for cross-gateway federation
│   ├── router.rs                  #   MessageRouter: routes agent messages locally or via OFP
│   └── registry.rs                #   PeerRegistry: discovers and manages remote gateways
│
├── router.rs                      # JsonRpcRouter: dispatches 40+ JSON-RPC methods
│                                   #   └─ delegates to GatewayExecutionService for core operations
│
├── execution.rs                   # GatewayExecutionService — central orchestrator
│   ├── spawn_agent_once()         #   Main entry: loads agent, creates executor, runs loop
│   ├── emergency_stop_root_session()  # Circuit breaker
│   ├── degrade_session()          #   P-7.18 degraded mode
│   └── resolve_inference_profile()   # LLM preset resolution
│
├── runtime/                       # Core execution engine
│   ├── lifecycle.rs               #   AgentExecutor (4336 lines) — the main reasoning loop
│   │   ├── execute_loop()         #     Full session lifecycle
│   │   ├── execute_with_history() #     Per-turn: context → LLM → tools → guard → repeat
│   │   └── close_session()        #     Finalize session state
│   ├── tool_call_processor.rs     #   ToolCallProcessor: validates & dispatches tool calls
│   ├── tool_dispatch.rs           #   Native tool dispatch layer
│   ├── tools/                     #   32 tool modules, each registers 1-5 tools
│   │   ├── mod.rs                 #     NativeTool trait, NativeToolRegistry, ToolTierFilter
│   │   ├── execution.rs           #     sandbox_exec
│   │   ├── sandbox.rs             #     sandbox.exec (new sandbox tool)
│   │   ├── content.rs             #     content.read/write/list
│   │   ├── artifact*.rs           #     artifact.build/inspect/exec/prepare
│   │   ├── agent.rs               #     agent.spawn/message/list
│   │   ├── agent_inspect.rs       #     agent.inspect
│   │   ├── agent_revision.rs      #     agent.revision.*
│   │   ├── approval.rs            #     approval.status/withdraw/list
│   │   ├── web.rs                 #     web.search/fetch
│   │   ├── knowledge.rs           #     knowledge.*
│   │   ├── workflow.rs            #     workflow.*
│   │   ├── promotion.rs           #     promotion.record
│   │   ├── credential.rs          #     credential.*
│   │   ├── scheduler.rs           #     scheduler.cron.*
│   │   ├── federation.rs          #     ecosystem.*
│   │   ├── skill.rs               #     skill.install
│   │   ├── self_describe.rs       #     self_describe
│   │   ├── constitution.rs        #     constitution.read
│   │   ├── capsule.rs             #     capsule.export/import
│   │   ├── observability.rs       #     observability.*
│   │   ├── sentinel.rs            #     sentinel.*
│   │   ├── evaluation.rs          #     evaluation.*
│   │   ├── plan_frame.rs          #     plan_frame.*
│   │   ├── workbench.rs           #     workbench.*
│   │   ├── validation.rs          #     validation.*
│   │   ├── user_interaction.rs    #     user_interaction.*
│   │   ├── user_profile.rs        #     user.profile
│   │   ├── wiki.rs                #     wiki.*
│   │   ├── digest.rs              #     digest_query
│   │   ├── session.rs             #     session.*
│   │   ├── resolve.rs             #     resolve
│   │   ├── quality_trend.rs       #     quality_trend.*
│   │   ├── improvement.rs         #     improvement.*
│   │   ├── github_issue.rs        #     github.issue.create
│   │   ├── tool_discover.rs       #     tool.discover
│   │   ├── admin_proposal.rs      #     admin.proposal.*
│   │   ├── security_redteam.rs    #     security_redteam.*
│   │   └── content_patch.rs       #     content.patch
│   ├── checkpoint.rs              #   SessionCheckpoint: HMAC-signed snapshots at all yield points
│   ├── guard.rs                   #   LoopGuard: progress + per-tool failure budgets
│   ├── session_context.rs         #   Session state, checkpoints, resume assessment
│   ├── session_read_cache.rs      #   Per-session LRU cache for pure-read tool results
│   ├── session_resume.rs          #   Resume a session from checkpoint
│   ├── content_store.rs           #   SHA-256 content-addressed storage
│   ├── artifact.rs                #   Artifact building logic
│   ├── approved_exec_cache.rs     #   Fingerprint-based exec replay cache
│   ├── budget_tracker.rs          #   Per-session token/cost budgets
│   ├── session_budget.rs          #   Session budget registry
│   ├── root_session_budget.rs     #   Root session budget registry
│   ├── init_prompt_budget.rs -> prompt_budget.rs
│   ├── prompt_budget.rs           #   Context window management
│   ├── model_router.rs            #   LLM provider/model routing
│   ├── llm_preset_resolver.rs     #   Resolve LLM preset from manifest + config
│   ├── context.rs                 #   System prompt assembly
│   ├── parser.rs                  #   Tool call parsing from LLM output
│   ├── middleware.rs              #   Pre/post processing middleware
│   ├── mcp.rs                     #   MCP (Model Context Protocol) tool runtime
│   ├── network_policy.rs          #   Network access policy resolution
│   ├── sealed_network_proxy.rs    #   Sealed sandbox network proxy
│   ├── remote_access.rs           #   Static analysis for network access patterns
│   ├── response_validation.rs     #   Response contract enforcement
│   ├── code_excerpts.rs           #   Code excerpt extraction
│   ├── script_execute.rs          #   Script agent fast path
│   ├── reevaluation_state.rs      #   Background reevaluation state
│   ├── v4a.rs                     #   V4A agent support
│   ├── active_execution_registry  #   (inferred) Running execution tracking
│   ├── operator_activity.rs       #   Operator activity recording
│   ├── semantic_diff.rs           #   Semantic diff between revisions
│   ├── guidance.rs                #   Guidance block management
│   ├── quality_signal.rs          #   Quality signal collection
│   ├── eval_stats.rs              #   Evaluation statistics
│   ├── openrouter_catalog.rs      #   OpenRouter model catalog
│   ├── curator_journal.rs         #   Curator decision journal
│   ├── session_tracer.rs          #   Session tracing
│   └── workbench_return.rs        #   Workbench return flow
│
├── scheduler/                     # Background processing
│   ├── scheduler.rs -> ../scheduler.rs  # Legacy redirect (the real file is at src/scheduler.rs)
│   ├── approval.rs                #   Approval resolution (level matching, approve/reject)
│   ├── approval_hardening.rs      #   Approval durability hardening
│   ├── decision.rs                #   should_wake() — when to wake background agents
│   ├── runner.rs                  #   handle_due_wake() — launch background agent turns
│   ├── signal.rs                  #   Signal polling and delivery
│   ├── hooks.rs                   #   Webhook execution on lifecycle events
│   ├── store.rs                   #   Background state persistence (background_state.json)
│   ├── workflow_store.rs          #   Workflow run/task persistence
│   ├── workflow_causal.rs         #   Workflow causal event integration
│   ├── fast_scheduler.rs          #   High-frequency tick for time-sensitive work
│   ├── eval_runner.rs             #   Evaluation suite execution
│   ├── plan_frame_ops.rs          #   Plan frame queries (pending plans)
│   ├── session_envelope_ops.rs    #   Session envelope operations
│   ├── reclamation.rs             #   Garbage collection (blobs, revisions, memories)
│   ├── system_agents.rs           #   System agent cron reconciliation
│   ├── auto_learning_jobs.rs      #   Auto-learning cron scheduling
│   ├── single_flight.rs           #   Dedup for concurrent identical operations
│   ├── task_notify.rs             #   Task notification dispatch
│   ├── cron_parser.rs             #   Cron expression parser
│   ├── agent_outcome.rs           #   Agent outcome tracking
│   ├── overflow_classifier.rs     #   Context overflow classification
│   ├── gateway_store/             #   SQLite-backed data store (39 submodules)
│   │   ├── mod.rs                 #     GatewayStore struct, open(), core CRUD
│   │   ├── migrate.rs             #     Schema versioning & migrations
│   │   ├── approvals.rs           #     Approval request persistence
│   │   ├── agent_registry.rs      #     Agent revisions, aliases, bindings
│   │   ├── credentials.rs         #     Credential records
│   │   ├── memory.rs              #     Tier 2 memory
│   │   ├── messages.rs            #     Agent messages + delivery
│   │   ├── workflow.rs            #     Workflow runs, task runs
│   │   ├── plan_frames.rs         #     Plan frames
│   │   ├── session_envelopes.rs   #     Session envelopes
│   │   ├── session_timeline.rs    #     Live digest events, chat timeline
│   │   ├── scheduled_jobs.rs      #     Cron-style scheduled jobs
│   │   ├── user_interactions.rs   #     User interaction persistence
│   │   ├── operator_activity.rs   #     Operator activity log
│   │   ├── observability.rs       #     Published reports
│   │   ├── evaluations.rs         #     Evaluation suite results
│   │   ├── security_findings.rs   #     Security finding records
│   │   ├── session_outcomes.rs    #     Session outcome records
│   │   ├── notifications.rs       #     Notification records
│   │   ├── hook_deliveries.rs     #     Hook dispatch tracking
│   │   ├── recordings.rs          #     Recording mode
│   │   ├── artifacts.rs           #     Artifact refs
│   │   ├── reclamation.rs         #     Reclamation tracker
│   │   ├── channel_bindings.rs    #     Channel binding records
│   │   ├── session_inference.rs   #     Session inference overrides
│   │   ├── user_profiles.rs       #     User profiles
│   │   ├── row_decode.rs          #     SQLite row decoders
│   │   ├── util.rs                #     SQLite utility functions
│   │   ├── workbenches.rs         #     Workbench records
│   │   ├── gate_messages.rs       #     Gate message records
│   │   ├── sentinel_disagreements.rs # Sentinel disagreement records
│   │   ├── post_promotion_reviews.rs # Post-promotion review records
│   │   ├── admin_proposals.rs     #     Admin proposals
│   │   ├── constitutional_proposals.rs # Constitutional proposals
│   │   ├── attack_patterns.rs     #     Attack pattern records
│   │   ├── improvement_cycles.rs  #     Improvement cycle records
│   │   └── validation_waivers.rs  #     Validation waiver records
│   └── ...other submodules
│
├── scheduler.rs                   # The real scheduler tick function (not the dir)
│   └── start_background_scheduler() / run_scheduler_tick()
│       # Orchestrates: notifications → job processing → workflow drain → agent wakes
│       # → approval timeout → orphan reaping → reclamation → post-promotion review
│
├── policy.rs                      # PolicyEngine: capability validation + security analysis
│   ├── can_exec_shell_detailed()  #   CodeExecution + SecurityAnalyzer
│   ├── can_invoke_tool()          #   SandboxFunctions
│   ├── can_connect_net()          #   NetworkAccess
│   ├── can_spawn_agent()          #   AgentSpawn
│   └── ... 15+ capability checks
│
├── sandbox.rs                     # SandboxRunner: bwrap/docker/microvm/wasm
│   ├── run_to_output()            #   Execute and capture result
│   ├── spawn()                    #   Spawn process in sandbox
│   └── start_sdk_bridge()         #   Unix socket transport for sandbox↔gateway
│
├── vault.rs                       # Credential vault
├── causal_chain.rs                # Append-only JSONL + SQLite mirror
├── artifact_store.rs              # Content-addressed artifact storage (re-export)
├── layer_store.rs                 # Compressed dependency layer storage
├── runtime_lock.rs                # Runtime lock verification
├── config.rs                      # Configuration loading
├── host_capabilities.rs           # Host-level capability registry
├── constitution_digest.rs         # Constitution digest verification
├── constitution_glossary.rs       # Constitutional glossary
├── enforcement_register.rs        # Principle/right → rule ID mapping
├── log_redaction.rs               # Secret redaction in logs
├── exec_request.rs                # Execution request types
├── interaction_answer.rs          # Interaction answer handling
├── fail_mode.rs                   # Fail mode configuration
├── post_promotion_review.rs       # Post-promotion review logic
├── agent.rs                       # Agent loading/resolution
├── wasm_backend.rs                # In-process WASM execution (feature-gated)
├── tracing/                       # Tracing infrastructure
├── sentinel/                      # Security sentinel system
│   ├── mod.rs
│   ├── runner.rs                  #   Sweep runner
│   ├── scheduler.rs               #   Sentinel tick scheduling
│   ├── dual_sweep.rs              #   Baseline + current comparison
│   ├── promotion_gate.rs          #   Sentinel-based promotion gating
│   ├── checks/                    #   Deterministic security checks
│   │   ├── credential.rs          #     Credential exposure
│   │   ├── sandbox_escape.rs      #     Sandbox escape attempts
│   │   ├── approval_bypass.rs     #     Approval bypass detection
│   │   ├── capability_accretion.rs #    Capability accretion monitoring
│   │   ├── supply_chain.rs        #     Supply chain integrity
│   │   ├── prompt_injection.rs    #     Prompt injection attempts
│   │   └── session_cluster.rs     #     Session clustering analysis
│   └── baseline/                  #   Frozen sentinel baselines
│       ├── credential.rs, sandbox_escape.rs, approval_bypass.rs, 
│       ├── capability_accretion.rs, supply_chain.rs
│       └── mod.rs
├── capsule/                       # Agent capsule export/import
│   └── archive.rs
└── bootstrap.rs                   # Agent bootstrapping
```

---

## 3. Precise Workflow

### 3.1 Daemon Startup

```
main() → GatewayServer::run()
  │
  ├─ Set AUTONOETIC_NODE_ID / AUTONOETIC_NODE_NAME env vars
  ├─ Initialize sandbox config from gateway config
  ├─ Bootstrap constitution snapshot → gateway dir
  ├─ Bootstrap SDK snapshot → gateway dir  
  ├─ Verify constitution lock integrity (digest + signature)
  ├─ Open GatewayStore (SQLite):
  │     - Apply pending schema migrations
  │     - Set approval flood caps
  │     - Probe vault master key
  ├─ Apply data retention policy (prune old traces, events)
  ├─ Reap orphaned continuation files (crash recovery)
  ├─ Reconcile system agents (cron jobs)
  ├─ Create JsonRpcRouter + GatewayExecutionService
  ├─ Warm local model context cache
  └─ Launch 6 concurrent services via tokio::try_join!:
       ├─ OFP server (cross-gateway federation)
       ├─ JSON-RPC server (Unix socket)
       ├─ HTTP server (content store API, optional)
       ├─ Background scheduler (tick every N seconds)
       ├─ Fast scheduler (sub-second tick, optional)
       └─ Eval runner (evaluation suite execution)
```

### 3.2 Ingress: Client Message → Agent Reply

```
Client → JSON-RPC (event.ingest or agent_spawn)
  │
  ▼
JsonRpcRouter::dispatch()
  │
  ├─ Parse method + params
  ├─ Validate auth token (shared_secret or bearer)
  ├─ Resolve target agent via alias registry
  └─ Call GatewayExecutionService::spawn_agent_once()
       │
       ├─ Load agent manifest + SKILL.md from AgentRepository
       ├─ Resolve LLM inference profile (preset → provider/model)
       ├─ Create AgentExecutor with:
       │     - LlmDriver, LoopGuard, ToolTierFilter
       │     - SessionBudget, session_id, workflow context
       │     - LiveDigest + SessionReport writers
       │
       ├─ Determine execution mode:
       │     ├─ "script" → ScriptExecute::execute() (no LLM)
       │     └─ "reasoning" → AgentExecutor::execute_with_history()
       │
       └─ Return SpawnResult to caller
```

### 3.3 Reasoning Loop (the core)

```
AgentExecutor::execute_with_history(history)
  │
  ├─ Check runtime lock drift (SHA-256 of runtime.lock)
  ├─ Open live digest + session report
  │
  └─ LOOP (until end_turn, suspension, or error):
       │
       ├─ 1. CONTEXT ASSEMBLY
       │     ├─ Compose system prompt:
       │     │    - SKILL.md instructions
       │     │    - Foundation instructions
       │     │    - Extended instructions (if requested)
       │     │    - Guidance blocks from tools
       │     │    - Tool definitions (tier-filtered)
       │     ├─ Build messages array (system + history)
       │     └─ Apply context window budget (truncate if needed)
       │
       ├─ 2. LLM COMPLETION
       │     ├─ Call LlmDriver::complete()
       │     ├─ Parse response: text + tool calls
       │     └─ Record LLM round (budget tracking)
       │
       ├─ 3. TOOL DISPATCH (if LLM emitted tool calls)
       │     └─ ToolCallProcessor::process_tool_calls()
       │           │
       │           ├─ For each tool call:
       │           │     ├─ Canonicalize tool name
       │           │     ├─ Check degraded mode blocking
       │           │     ├─ Validate tool intent
       │           │     ├─ PolicyEngine check (capability gating)
       │           │     ├─ Route to MCP runtime or NativeToolRegistry
       │           │     ├─ Session read cache (pure reads only)
       │           │     ├─ Execute tool:
       │           │     │    ├─ content_* → content store
       │           │     │    ├─ sandbox_exec → SandboxRunner
       │           │     │    ├─ agent_spawn → recursive spawn_agent_once
       │           │     │    ├─ web_* → http_client (policy-gated)
       │           │     │    ├─ knowledge_* → GatewayStore
       │           │     │    └─ ... 40+ tool types
       │           │     ├─ Secret store redaction
       │           │     ├─ Disclosure state update
       │           │     ├─ Execution trace (SQLite)
       │           │     └─ Return result JSON
       │           │
       │           ├─ If any result requires APPROVAL:
       │           │     ├─ Check 5-layer dedup:
       │           │     │     1. Exec cache (fingerprint)
       │           │     │     2. Plan grants
       │           │     │     3. Session approval grants
       │           │     │     4. Existing pending/approved approvals
       │           │     │     5. Approval flood cap (>50 → rejected)
        │           │     ├─ If not auto-approved:
        │           │     │     ├─ Create ApprovalRequest (SQLite)
        │           │     │     ├─ Save signed SessionCheckpoint (YieldReason::ApprovalRequired)
        │           │     │     └─ Suspend turn
       │           │     └─ If auto-approved: execute directly
       │           │
       │           └─ Aggregate results for LLM
       │
       ├─ 4. LOOP GUARD CHECK
       │     ├─ Track progress fingerprints
       │     ├─ Check 6 trip conditions:
       │     │     1. No meaningful progress (same tool+args loop)
       │     │     2. Tool failure budget exceeded
       │     │     3. Rotating polling pattern
       │     │     4. Child failure budget
       │     │     5. Redundant roster polling
       │     │     6. LLM failure budget
       │     └─ If tripped → return error, close session
       │
       └─ 5. STOP REASON EVALUATION
             ├─ end_turn → break (return text)
             ├─ tool_use (more calls in batch) → continue
             ├─ max_tokens → break
             ├─ ApprovalRequired → return Suspended{approval_request_id}
             └─ UserInputRequired → return SuspendedUserInput{interaction_id}
```

### 3.4 Approval Flow (Operator Consent)

```
Agent calls privileged tool (e.g., sandbox_exec on new host)
  │
  ▼
ToolCallProcessor detects action requires approval
  │
  ├─ Check 5-layer dedup chain (see above)
  │
  └─ If new approval needed:
       │
       ├─ 1. GATEWAY PERSISTS
       │     ├─ Create ApprovalRequest in SQLite
       │     └─ Save signed SessionCheckpoint (includes:
       │           history, pending tool, remaining tools, loop guard)
       │
       ├─ 2. TURN SUSPENDS
       │     ├─ YieldReason::ApprovalRequired
       │     └─ Return TurnOutcome::Suspended
       │
       ├─ 3. OPERATOR DECIDES (via JSON-RPC or CLI)
       │
       ├─ 4a. APPROVED
       │     ├─ scheduler::approval::approve_request()
       │     │    ├─ Validate approval level (Admin/Operator/Agent)
       │     │    ├─ Check dwell time (P-7.4 / R++4)
       │     │    ├─ Check confirm phrase (R++4)
       │     │    ├─ Check capability accretion acknowledgement (R++2)
       │     │    ├─ Persist decision → SQLite
       │     │    └─ apply_decision() handles all post-decision side-effects:
       │     │         - workflow task status update
       │     │         - session approval grant materialization
       │     │         - notification / signal delivery
       │     │         - causal event emission
       │     ├─ Verify checkpoint HMAC
       │     ├─ Verify checkpoint/approval action-equality
       │     ├─ If sandbox_exec: record session approval grants
       │     ├─ Inject approval_ref into suspended tool call
       │     ├─ Resume reasoning loop
       │     ├─ Agent re-issues tool call with approval_ref; gateway executes it normally
       │     ├─ Inject real tool result into history
       │     ├─ Execute remaining tool calls from original batch
       │     └─ Delete checkpoint after successful resume
       │
        └─ 4b. REJECTED
              ├─ Persist rejection → SQLite
              ├─ apply_decision() notifies agent / fails workflow task
              └─ Checkpoint is deleted when the session is cleaned up
```

### 3.5 Background Scheduler Tick

```
run_scheduler_tick()  [runs every config.background_tick_secs]
  │
  ├─ 1. Process pending notifications from GatewayStore
  ├─ 2. Clean up stale notifications (>24h)
  ├─ 3. Prune expired session approval grants
  ├─ 4. R++8 Sandbox Escape Detection
  │      └─ Check session escape-attempt thresholds → degrade or e-stop
  ├─ 5. Process due scheduled jobs (cron-like)
  ├─ 6. Drain runnable and queued workflow tasks
  │      (runs before background agent wakes to prevent child starvation)
  ├─ 7. Clean up stale single-flight reservations
  ├─ 8. Background Agent Wakes
  │      └─ For each loaded agent:
  │           ├─ decision::should_wake() (approval resolved? timer due? action pending?)
  │           └─ runner::handle_due_wake() → launch agent turn
  ├─ 9. Approval timeout check
  │      └─ Fail tasks stuck in AwaitingApproval > config.approval_timeout_secs
  ├─10. Wiki proposal auto-expiry
  ├─11. Stuck running task detection
  │      └─ Auto-resolve tasks stuck in Running with session evidence
  ├─12. Orphan child reaper (R+12)
  │      └─ Cancel children of terminated parent sessions
  ├─13. Resource reclamation sweep
  │      ├─ GC content blobs (unreferenced)
  │      ├─ Prune old agent revisions
  │      ├─ Expire timed memories
  │      ├─ Clean orphaned sessions
  │      └─ Remove stale scheduled jobs
  └─14. Post-promotion review
         └─ Check causal event trends + sentinel findings
```

### 3.6 Emergency Stop Flow

```
Any authorized requester → GatewayExecutionService::emergency_stop_root_session()
  │
  ├─ Validate source permissions via PolicyEngine (P-7.1)
  ├─ Persist stop request to emergency_stops table
  ├─ Mark workflow as EmergencyStopping
  ├─ Kill sandbox child processes (SIGKILL)
  ├─ Abort running tokio tasks
  ├─ Cancel pending approvals and user interactions
  ├─ Delete session approval grants (prevent post-stop auto-approval)
  ├─ Write terminal checkpoint with YieldReason::EmergencyStop
  ├─ Finalize status to EmergencyStopped
  ├─ Emit causal event with Ri-0.9 "last word" notice
  └─ Degrade all child sessions
```

---

## 4. Data Store Domains

### SQLite (GatewayStore) — Transactional State

```
Tables organized by domain:

SESSIONS & AGENTS:
  agent_revisions          # Immutable revision records
  agent_aliases            # Mutable alias → revision pointer
  session_agent_bindings   # Per-session pinned revision
  session_outcomes         # Session outcome records
  
WORKFLOW & APPROVAL:
  workflows                # Workflow run records
  workflow_tasks           # Task run records  
  approvals               # Approval gates (pending + history)
  session_approval_grants  # Session-scoped auto-approvals
  escalation_messages      # Federation escalation records
  user_interactions        # user_ask questions/answers
  plan_frames              # Plan capability grants
  session_envelopes        # Session envelopes
  
EXECUTION & AUDIT:
  causal_events            # Queryable mirror of causal chain JSONL
  execution_traces         # Full stdout/stderr/exit_code per tool call
  active_executions        # Running execution leases
  emergency_stops          # Circuit breaker audit trail
  notifications            # Notification records
  hook_deliveries          # Hook dispatch tracking
  
MEMORY:
  memories                 # Tier 2 durable facts
  memory_tags              # Tag index for knowledge_search
  
SCHEDULING:
  scheduled_jobs           # Cron-style scheduled jobs
  
OBSERVABILITY:
  published_session_reports     # Published report catalog
  live_digest_events            # Real-time session digest
  
SECURITY:
  security_findings        # Sentinel detection records
  sentinel_disagreements   # Baseline vs current comparison
  
MISC:
  credentials              # Credential records
  recordings               # Recording mode state
  gate_messages            # Gate message records
  wiki_proposals           # Wiki proposal records
  workbenches              # Workbench records
  channel_bindings         # Channel binding records
  operator_activity        # Operator activity log
  session_timeline         # Live digest events + chat timeline
  session_inference        # Session inference overrides
  improvement_cycles       # Improvement cycle records
  agent_messages           # Agent message delivery
  validation_waivers       # Validation waiver records
  user_profiles            # User profiles
  admin_proposals          # Admin proposals
  constitutional_proposals # Constitutional proposals
  attack_patterns          # Security attack pattern records
  post_promotion_reviews   # Post-promotion review records
```

### Content Store (Filesystem) — Artifacts

```
.gateway/
├── content/sha256/ab/c123...    # Immutable content blobs
├── sessions/<session_id>/
│   ├── manifest.json            # name → handle mappings
│   └── artifacts.json           # Artifact metadata
├── layers/                      # Compressed dependency directories
│   ├── index.json               # digest → layer_id
│   └── layer_{id}/
│       ├── manifest.json        # Metadata
│       └── contents.tar.zst     # Compressed tarball
├── checkpoints/<session_id>/    # Session checkpoints (HMAC-signed)
└── history/              
    └── causal_chain.jsonl       # Append-only audit log
```

---

## 5. Key Architectural Patterns

### 5.1 Separation of Powers

```
AGENT (low privilege)              GATEWAY (high privilege)
─────────────────────────          ─────────────────────────
Proposes actions                   Validates capabilities
Reads its own content              Manages all storage
Calls tools (if permitted)         Executes all side effects
                                    Enforces constitution
                                    Owns secrets
                                    Manages sessions
                                    Handles approval gates
```

### 5.2 Policy Enforcement Points

Every side-effecting operation passes through:

```
Tool Call
  → Availability check (manifest capabilities → is_available())
  → ToolTierFilter (current workflow phase)
  → PolicyEngine::can_*() (capability gating)
  → SecurityAnalyzer (static command analysis for sandbox_exec)
  → Approval system (5-layer dedup → execute or suspend)
  → Execution trace logging
```

### 5.3 Approval 5-Layer Dedup (checked in order)

```
1. Exec Cache (fingerprint-level, cross-session)
   └─ Only when all patterns are concrete (url_literal/ip_address/host_constant)

2. Plan Grants (operator-approved plan envelope)
   └─ Materialized as session grant

3. Session Approval Grants (target-level, scope-aware)
   └─ Tables: session_approval_grants + session_approval_grant_targets
   └─ Supports ExactHost, HostSuffix, HostAndPort, UrlPrefix
   └─ Scoped RootSession or Session
   └─ Optional expires_at

4. Existing Approved/Pending Approvals (domain-level matching)
   └─ Same session, same action kind, same target host

5. Approval Flood Cap (max_pending_approvals_per_root, default 50)
   └─ Rejects with approval_flood if would exceed
```

### 5.4 LoopGuard Trip Conditions

```
6 independent trip mechanisms:

1. NoMeaningfulProgress  → same (tool, args) fingerprint repeated
2. ToolFailureBudget     → per-tool failure count (never resets)
3. RotatingPollingPattern → cycling read-only tools without progress
4. ChildFailureBudget    → child task failures exceeded
5. RedundantRosterPolling → agent_list/agent_inspect repeated identically
6. LlmFailureBudget      → consecutive LLM endpoint failures
```

### 5.5 Tool Tier System

```
Tiers for progressive disclosure:

Core          → content_*, sandbox_exec, resolve, knowledge_* (always available)
Workflow      → agent_spawn, workflow_*, approval_*, user_interaction_*
Specialized   → web_*, promotion_record, agent_revision_*, evaluation_*

Filter strategies:
  all()                         → no filtering
  core_only()                   → agent boot, degraded mode
  core_and_workflow()           → child sessions
  core_and_workflow_with_approvals() → pending approval
  degraded()                    → core + inspection only (P-7.18)
  clarification()               → read-only inspection (operator probe)
```

---

## 6. Caveats, Observations & Mutualisation Needs

### 6.1 Surface Observations

| Observation | Risk / Note |
|---|---|
| **`lifecycle.rs` is 4336 lines** | The main reasoning loop has grown very large. Contains context assembly, LLM dispatch, tool processing orchestration, budget tracking, checkpoint logic, and turn outcome handling. Would benefit from splitting into smaller modules. |
| **`tool_call_processor.rs` is 1799 lines** | Combined dispatch + approval detection + cache logic. Approval-flow branching logic is tightly coupled with tool execution. |
| **`approval.rs` (scheduler) is 2596 lines** | The approval resolution logic is dense, combining level resolution, approve/reject/cancel, grant management, and notification dispatch. |
| **`router.rs` is 4631 lines** | Single-file dispatch for 40+ JSON-RPC methods. Growing unbounded — every new JSON-RPC endpoint adds to this file. |
| **GatewayStore diverging schema** | 39 submodules with overlapping concerns. For example, `notifications.rs`, `messages.rs`, `gate_messages.rs` are closely related but separate. |
| **Two approval file sets** | Approval logic lives in `runtime/tools/approval.rs` (agent-facing tools) and `scheduler/approval.rs` (operator-facing resolution). The boundary is clean but both need awareness of the approval schema. |
| **Policy checks duplicated across tools** | Each tool checks its capability via `is_available()` + `PolicyEngine`, but the `ToolCallProcessor` also does a `can_invoke_tool` check. This is intentional defense-in-depth, but could be consolidated. |

### 6.2 Potential Mutualisation Opportunities

| Opportunity | Why | Effort |
|---|---|---|
| **Unified tool metadata schema** | `ToolMetadata` is currently minimal (just `path`) and defined in `tools/mod.rs`. A richer schema shared across all tools would improve introspection. | Low |
| **Generic MCP-bridge pattern** | Both `mcp.rs` and the native tool dispatch follow similar patterns (definition → availability → execute). A shared trait-based dispatch framework could reduce boilerplate. | Medium |
| **Approval flow state machine** | Approval lifecycle (pending → approved/rejected → history) is spread across `scheduler/approval.rs`, `runtime/checkpoint.rs`, and `runtime/tool_call_processor.rs`. A centralized state machine would reduce inconsistencies. | High |
| **Causal event publishing** | Every tool/action that emits a causal event does so ad-hoc. A centralized event bus with typed event payloads would: (1) guarantee every tool call emits a trace, (2) enable `policy.decision` hooks without per-tool cooperation, (3) simplify the hook system. | High |
| **Session lifecycle manager** | Session creation, checkpoint, suspend, resume, re-entry, and close logic is interleaved across `lifecycle.rs`, `execution.rs`, `session_resume.rs`, `session_context.rs`, and `checkpoint.rs`. A `SessionLifecycleManager` struct would clarify the state machine. | Medium |
| **Resource reclamation unification** | Reclamation logic is in `scheduler/reclamation.rs` (GC), `runtime/checkpoint.rs` (orphan reaping), `execution.rs` (session cleanup), and `gateway_store/` (retention policy). A single `ReclamationService` would reduce missed-deadlines. | Medium |
| **Tool registration vs. JSON-RPC routing** | Both `default_registry()` (tools) and `JsonRpcRouter::dispatch()` (RPC methods) define method-name → handler mappings. A shared routing table would DRY up name resolution. | Low |
| **Loop guard types** | `LoopGuardTripReason` variants and `TurnOutcome` variants partially overlap (both handle failures, suspensions). A unified `SessionInterruptReason` enum could simplify checkpoint/resume logic. | Low |
| **Secret redaction pipeline** | Secret redaction happens in `log_redaction.rs`, `tool_call_processor.rs` (secret store), and each tool individually. A centralized post-execution redaction pass would prevent accidental leaks. | Medium |
| **Template for new tools** | Each tool duplicates the `NativeTool` trait implementation boilerplate (name, definition, is_available, execute). A macro or builder pattern would make adding tools simpler and safer. | Low |
| **GatewayStore submodule cross-referencing** | `approvals.rs`, `messages.rs`, `notifications.rs`, and `gate_messages.rs` all query overlapping SQLite tables. A unified data-access layer with typed queries would prevent schema drift. | High |

### 6.3 Known Hard Problems

1. **Recursive approval loops** — Agent spawns tool that requires approval, which spawns another agent, which calls another privileged tool. The approval dedup chain must correctly scope session grants across nested sessions.
2. **Checkpoint consistency on restart** — Session checkpoints save execution state at every yield point. A restart between the SQLite approval insert and the checkpoint write could leave a dangling approval row; the startup reaper handles orphaned checkpoints and approvals.
3. **Flood cap interaction with nested sessions** — `max_pending_approvals_per_root` counts across all child sessions. An agent could inadvertently exhaust the cap for the entire root tree.
4. **Session grant scope lifting** — A session grant created with `HostSuffix` coverage might unintentionally cover a host in a child session that the parent never intended.

