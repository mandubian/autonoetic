# Spec: Install Pipeline Hardening

**Status:** Implemented (April 2026) — superseded by [Promotion Federation](../archived/promotion-federation-plan.md) for evaluation model
**Date:** 2026-04-07 (updated 2026-04-08, federation note 2026-05-13)
**Authors:** mandubian
**Related:** [archived/promotion-federation-plan.md](../archived/promotion-federation-plan.md), [design/post-promotion-review-design.md](../proposals/post-promotion-review.md)

> **Federation update (2026-05-13):** This spec describes the pre-federation single-evaluator pipeline (`evaluator.default`). The promotion gate has since been extended with federation roles (`static_evaluator.default`, `unit_test_runner.default`, `sealed_evaluator.default`) and a FullJury gate requiring operator escalation. The mechanical hardening described here (capability delta, content digest binding, unresolved dependency check, promotion evidence validation, audit-only gating) was preserved and extended for federation. References to `evaluator.default` below reflect the pipeline as it existed when this spec was written.

---

## 1. Problem Statement

Demo-session-1 exposed a cascade of failures in the agent installation pipeline. A weather agent with `NetworkAccess` capability and external Python dependencies (`import requests` via `requirements.txt`) was installed and promoted without:

1. **Dependencies being resolved** — no `packager.default` step was spawned; `requirements.txt` was bundled raw
2. **Promotion gate evidence** — neither `evaluator.default` nor `auditor.default` called `promotion_record`
3. **Dependency detection** — `create_from_intent` didn't notice `requirements.txt` needed a packager step, and didn't detect `import requests` as an unresolved external dependency
4. **Network failure surfacing** — `sandbox_exec` silently swallowed an HTTPS connection failure (the sandbox blocked it, Python caught the exception, `exit=0` was returned)

### Root Cause

The pipeline relies entirely on **LLM judgment** to enforce safety invariants. The planner "forgot" to spawn `packager.default`. The evaluator "decided" to approve despite seeing a network error in stdout. No mechanical guardrails stopped either mistake.

### Resulting State of Installed Agent

```
weather-agent revision:
├── SKILL.md          ← metadata fields mostly null (no llm_config, no limits, etc.)
├── runtime.lock      ← sha256: "replace-me", dependencies: [], layers: []
├── requirements.txt  ← present but never installed
├── weather_agent.py  ← contains `import requests` (unresolved)
├── test_weather_agent.py
└── README.md
```

This agent would fail at runtime in any isolated sandbox.

---

## 2. Design Principles

> **Rule Zero: Rules cannot be overridden. Not by agents, not by planners, not by parameters, not by "trust me" flags.**

If a rule exists, it applies equally to all agents without exception. Any mechanism that allows bypassing a rule is itself a bug. Freedom for agents comes from well-defined capabilities within boundaries — not from the ability to move the boundaries.

> **LLM decisions are advisory. Safety-critical invariants must be mechanically enforced by the gateway.**

Agents can make mistakes — forgetting steps, approving despite errors, guessing wrong. The gateway must have deterministic guardrails that catch these mistakes before they become deployed agents.

> **The gateway is a narrow rule enforcer. It analyzes, gates, and explains — but never routes.**

The gateway enforces hard invariants that agents cannot bypass. Within its scope, it has absolute authority: it refuses operations that violate safety rules, and agents have no escape hatch. Outside its scope, it is completely hands-off: it never routes, delegates, or makes workflow decisions.

| Layer | What the gateway does | What the gateway never does |
|-------|----------------------|----------------------------|
| **Analyze** | Scan code for patterns, detect capabilities, find dependency gaps (same as existing `PatternAnalyzer`, `PythonAstAnalyzer`) | Interpret intent, decide what an agent "should" do |
| **Gate** | **Refuse** operations when hard invariants are violated — promotion without records, unresolved dependencies on high-risk agents, missing capabilities. The refusal includes a structured error explaining what must be fixed. | Auto-fix problems, auto-spawn packager, make workflow decisions, provide escape hatches |
| **Explain** | Surface findings as structured data in tool responses (`warnings[]`, `BundleHealthReport`, error messages with `required_actions`). The calling agent uses this information to plan its next steps. | Decide *which* agent to spawn to fix the problem, or *whether* the problem is worth fixing |

When the gate refuses an operation, the calling agent (typically `specialized_builder`) reports the structured error back to the planner. The planner decides how to resolve it — which agent to spawn, what steps to run. The gateway then re-checks the invariants on the next attempt. This creates a tight **gate → explain → plan → execute → re-check** loop where the gateway never trusts agent judgment on safety-critical rules.

---

## 3. Proposed Changes

### Tier 1 — Mechanical Guardrails (gateway-enforced)

These are deterministic checks inside gateway tools. They cannot be bypassed by LLM hallucination.

---

#### 3.1 Promotion Gate for High-Risk Capabilities

**File:** `autonoetic-gateway/src/runtime/tools/agent_revision.rs` — `AgentRevisionPromoteTool::execute`

**Change:** When promoting a revision whose SKILL.md declares high-risk capabilities (`NetworkAccess`, `CodeExecution`, or `AgentSpawn`), `agent_revision_promote` **refuses** unless:

- A `promotion_record` with `pass=true` and `role=evaluator` exists for the revision's `artifact_id`, **AND**
- A `promotion_record` with `pass=true` and `role=auditor` exists for the same artifact

**Pseudocode:**

```rust
// After loading the revision record:
let revision_dir = gateway_dir
    .join("revisions/agents")
    .join(&args.agent_id)
    .join(&args.revision_id);
let skill_bytes = std::fs::read(revision_dir.join("SKILL.md"))?;
let (manifest, _) = SkillParser::parse(&String::from_utf8_lossy(&skill_bytes))?;

let high_risk_caps = [
    |c: &Capability| matches!(c, Capability::NetworkAccess { .. }),
    |c: &Capability| matches!(c, Capability::CodeExecution { .. }),
    |c: &Capability| matches!(c, Capability::AgentSpawn { .. }),
];
let is_high_risk = manifest.capabilities.iter().any(|c| {
    high_risk_caps.iter().any(|check| check(c))
});

if is_high_risk {
    let artifact_id = rev.artifact_id.as_deref()
        .ok_or_else(|| anyhow!("high-risk revision has no artifact_id — cannot verify promotion gate"))?;
    let promo_store = PromotionStore::new(gateway_dir)?;
    let record = promo_store.get_promotion(artifact_id)
        .ok_or_else(|| anyhow!(
            "Promotion gate: no promotion_record found for artifact '{}'. \
             Agents with NetworkAccess/CodeExecution/AgentSpawn require both \
             evaluator and auditor pass records before promotion.",
            artifact_id
        ))?;
    anyhow::ensure!(
        record.evaluator_pass,
        "Promotion gate: evaluator did not pass for artifact '{}'. \
         Fix the evaluation findings and re-run evaluator.default.",
        artifact_id
    );
    anyhow::ensure!(
        record.auditor_pass,
        "Promotion gate: auditor did not pass for artifact '{}'. \
         Fix the audit findings and re-run auditor.default.",
        artifact_id
    );
}

// Gate 2: Unresolved dependencies
if is_high_risk {
    anyhow::ensure!(
        !rev.has_unresolved_dependencies,
        "Promotion gate: revision has unresolved dependencies ({}). \
          Run packager.default to install dependencies as layers, \
         then re-submit the revision.",
        rev.unresolved_dep_files.join(", ")
    );
}
```

**Error message returned to calling agent:**

```json
{
  "error": "Promotion gate: no promotion_record found for artifact 'art_425a482c'. Agents with NetworkAccess/CodeExecution/AgentSpawn require both evaluator and auditor pass records before promotion.",
  "required_actions": [
    "Run evaluator.default against the artifact and call promotion_record(pass=true)",
    "Run auditor.default against the artifact and call promotion_record(pass=true)"
  ]
}
```

**Impact:** This is a **breaking change** for any workflow that calls `promote` on high-risk agents without running the full evaluation pipeline. That is precisely the bug being fixed.

---

#### 3.2 Unresolved Dependency Detection in `create_from_intent`

**File:** `autonoetic-gateway/src/runtime/tools/agent_revision.rs` — `AgentRevisionCreateFromIntentTool::execute`

**Change:** After building the file map, scan for known dependency manifest files. If any are present but the artifact has no layers, emit `warnings` and set `has_unresolved_dependencies` in the response.

**Detected dependency files:**

| File | Ecosystem |
|------|-----------|
| `requirements.txt` | Python (pip) |
| `pyproject.toml` | Python (pip/poetry/hatch) |
| `package.json` | Node.js (npm/yarn) |
| `go.mod` | Go |
| `Cargo.toml` | Rust |
| `Gemfile` | Ruby |

**Pseudocode:**

```rust
const DEPENDENCY_FILES: &[(&str, &str)] = &[
    ("requirements.txt", "python/pip"),
    ("pyproject.toml", "python"),
    ("package.json", "node/npm"),
    ("go.mod", "go"),
    ("Cargo.toml", "rust/cargo"),
    ("Gemfile", "ruby/bundler"),
];

let found_dep_files: Vec<(&str, &str)> = DEPENDENCY_FILES
    .iter()
    .filter(|(f, _)| file_map.contains_key(*f))
    .copied()
    .collect();

let has_unresolved_dependencies = !found_dep_files.is_empty() && bundle.layers.is_empty();

// Include in response:
if has_unresolved_dependencies {
    obj.insert("warnings", json!([
        format!(
            "Dependency files found ({}) but no layers in artifact. \
              Run packager.default to install dependencies as layers before evaluation.",
            found_dep_files.iter().map(|(f, eco)| format!("{f} ({eco})")).join(", ")
        )
    ]));
    obj.insert("has_unresolved_dependencies", json!(true));
}
```

**Behavior:** The revision is still created (agents need it to iterate), but `has_unresolved_dependencies: true` is stored in the revision metadata. The promotion gate (3.1) **refuses** to promote any high-risk agent with unresolved dependencies — the warning alone is not sufficient, the agent must resolve dependencies (typically via `packager.default`) and re-submit before promotion is allowed.

---

#### 3.3 External Import Detection (lightweight static scan)

**File:** `autonoetic-gateway/src/runtime/install_contract.rs` — new public function

**Justification:** The gateway already performs this level of analysis. The existing `PatternAnalyzer` (in `runtime/analysis/pattern.rs`) scans for `requests.get`, `urllib.request`, `subprocess.run` to infer `NetworkAccess` and `CodeExecution` capabilities. The `PythonAstAnalyzer` (in `runtime/analysis/python_ast.rs`) goes further with full AST parsing via `python3`. Detecting `import requests` → "needs dependency layer" is the **same tier of analysis** as detecting `import requests` → "needs NetworkAccess capability".

**Change:** Add a lightweight scanner that detects external imports in Python files by comparing `import X` / `from X import` statements against a known Python stdlib set. When external imports are found and no dependency layer provides them, include in the `warnings[]` response.

**The gateway does NOT route or auto-fix based on this finding.** It reports `detected_external_imports: ["requests"]` and `has_unresolved_dependencies: true` in the tool response, and stores this in the revision metadata. The promotion gate then uses this metadata to refuse promotion until resolved. The calling agent sees the structured error and reports back to the planner, who decides how to resolve it.

**Scope:** Python only (most common in current agent ecosystem). Extensible to other languages later.

**Pseudocode:**

```rust
/// Known Python standard library modules (3.11+). Not exhaustive but covers common ones.
const PYTHON_STDLIB: &[&str] = &[
    "os", "sys", "json", "re", "math", "datetime", "collections", "itertools",
    "functools", "pathlib", "typing", "io", "abc", "enum", "dataclasses",
    "unittest", "argparse", "logging", "hashlib", "base64", "uuid", "copy",
    "time", "random", "string", "textwrap", "shutil", "tempfile", "glob",
    "subprocess", "threading", "multiprocessing", "http", "urllib",
    "csv", "sqlite3", "xml", "html", "email", "socket", "ssl",
    "struct", "array", "queue", "heapq", "bisect", "contextlib",
    "traceback", "inspect", "ast", "dis", "pdb", "profile", "timeit",
    // ... extend as needed
];

pub fn detect_external_python_imports(
    file_map: &BTreeMap<String, Vec<u8>>,
) -> Vec<String> {
    let mut external = BTreeSet::new();
    for (path, content) in file_map {
        if !path.ends_with(".py") { continue; }
        let text = String::from_utf8_lossy(content);
        for line in text.lines() {
            let trimmed = line.trim();
            let module = if trimmed.starts_with("import ") {
                trimmed.strip_prefix("import ").and_then(|s| s.split_whitespace().next())
            } else if trimmed.starts_with("from ") {
                trimmed.strip_prefix("from ").and_then(|s| s.split_whitespace().next())
            } else {
                None
            };
            if let Some(module) = module {
                let top_level = module.split('.').next().unwrap_or(module);
                if !PYTHON_STDLIB.contains(&top_level) && top_level != "__future__" {
                    let local_file = format!("{}.py", top_level);
                    if !file_map.contains_key(&local_file) {
                        external.insert(top_level.to_string());
                    }
                }
            }
        }
    }
    external.into_iter().collect()
}
```

**Integration point:** Called by `create_from_intent` after building the file map. Results appended to `warnings[]` and `detected_external_imports[]` in the response. The gateway never acts on these findings — it only reports them.

---

### Tier 2 — Pipeline Feedback (advisory improvements)

---

#### 3.4 Bundle Health Diagnostic

**File:** `autonoetic-gateway/src/runtime/install_contract.rs` — new public struct + function

**Change:** Encapsulate all Tier 1 diagnostics into a reusable `BundleHealthReport`:

```rust
pub struct BundleHealthReport {
    /// Dependency manifest files found in bundle (e.g., ["requirements.txt"])
    pub dependency_files: Vec<String>,
    /// Whether any dependency files exist but no layers are present
    pub has_unresolved_dependencies: bool,
    /// External module imports detected with no matching dependency layer
    pub detected_external_imports: Vec<String>,
    /// Whether the agent declares NetworkAccess capability
    pub declares_network_access: bool,
    /// Whether the agent declares CodeExecution capability
    pub declares_code_execution: bool,
    /// Structured warning messages
    pub warnings: Vec<String>,
}

pub fn analyze_bundle_health(
    file_map: &BTreeMap<String, Vec<u8>>,
    capabilities: &[Capability],
    has_layers: bool,
) -> BundleHealthReport { ... }
```

This function is called by `create_from_intent` and its output is included in the response for the calling agent to act on.

---

#### 3.5 Planner SKILL.md — Detect Dependency Files from Named Outputs

**File:** `agents/lead/planner.default/SKILL.md`

**Change:** Add an explicit rule to the Decision Flow that leverages the new `content.named_outputs` from the implicit artifact:

```markdown
### Post-Coder Dependency Check (CRITICAL)

After the coder task completes, read its implicit artifact (`impl_task-{id}`) 
and check `content.named_outputs` for ANY of these files:
- `requirements.txt`, `pyproject.toml`, `package.json`, `go.mod`, `Cargo.toml`, `Gemfile`

If found, you **MUST** spawn `packager.default` before `evaluator.default`.
The packager has `NetworkAccess` capability and will:
1. Install dependencies (pip install, npm install, etc.)
2. Capture installed packages as a layer
3. Update the artifact with the dependency layer

**Without this step:**
- The evaluator runs in a network-isolated sandbox
- `import requests` and similar imports silently fail at runtime
- The agent appears to work but is broken in production

**NEVER skip this step when dependency files exist.**
```

---

### Tier 3 — Enhancements (tracking)

Originally lower-priority follow-ups; **3.6–3.11 are implemented** (see Status column below).

| ID | Enhancement | Description | Complexity | Status |
|----|-------------|-------------|------------|--------|
| 3.6 | `sandbox_exec` network policy | After execution in a network-isolated sandbox, scan stdout/stderr for typical network-failure fingerprints; if found, return `ok: false`, `error_type: network_isolated`, and structured `network_*` fields so a zero exit from swallowed exceptions cannot masquerade as success. Operator approval path unchanged for commands that statically require network. | High | ✅ Done |
| 3.7 | Gateway SHA computation | Hybrid lock identity: keep compile-time source fingerprint (`build.rs` → `GATEWAY_BUILD_SHA256` + `GATEWAY_BUILD_TAG`) and add runtime-computed executable hash (`gateway.binary_sha256`) from the running gateway binary bytes. | Medium — build/runtime plumbing | ✅ Done |
| 3.8 | Promotion record digest binding | Bind promotion evidence to canonical revision `content_digest`. Allow evidence recorded before revision creation, but reject/reconcile mismatched digests to prevent evidence replay across different revision contents. | Low | ✅ Done |
| 3.9 | `create_from_intent` null field cleanup | Omit null optional fields from serialized SKILL.md metadata instead of writing `llm_config: null`, `limits: null`, etc. Reduces noise. | Low | ✅ Done |
| 3.10 | JSON-RPC ingress authentication | Require auth token on local JSON-RPC ingress (`event.ingest`, `agent_spawn`, and all methods). Gateway now validates request token against `AUTONOETIC_SHARED_SECRET` and rejects unauthenticated requests. | Medium | ✅ Done |
| 3.11 | Strict env-override gating | Fail-closed handling for security-sensitive env overrides: `AUTONOETIC_BWRAP_*` and global `AUTONOETIC_LLM_*` overrides are ignored unless explicit allow flags are set (`AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES`, `AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES`). | Medium | ✅ Done |

> **Note:** Auto-dependency resolution (having the gateway automatically invoke packager logic) was considered and **rejected** — it violates the narrow rule enforcer principle. The gateway reports unresolved dependencies; the planner decides whether and how to resolve them.

---

## 4. Open Questions

### Q1: ~~Promotion gate escape hatch~~ Resolved: No escape hatch.

**Decision:** The promotion gate has no escape hatch. If an agent declares high-risk capabilities, promotion records and resolved dependencies are required — period. There is no `gating_policy` parameter.

**Rationale:** An escape hatch defeats the purpose of a mechanical gate. The moment you add `gating_policy: "none"`, the planner (an LLM) will learn to pass it on every call to avoid friction. The gate becomes advisory, which is exactly the failure mode this spec fixes. For low-risk agents that don't declare high-risk capabilities, the gate doesn't apply — that's the escape hatch: don't declare capabilities you don't need.

For pure-transform agents (no I/O beyond `self.*`), the planner's existing matrix already routes them to skip evaluator/auditor. Since they don't declare `NetworkAccess`/`CodeExecution`/`AgentSpawn`, the promotion gate doesn't trigger. No special parameter needed.

### Q2: ~~Should unresolved dependencies block promotion?~~ Resolved: Yes.

**Decision:** Unresolved dependencies block promotion for high-risk agents. The promotion gate (3.1) checks `has_unresolved_dependencies` in the revision metadata alongside promotion records. Both conditions must pass.

**Rationale:** The gateway's job is to enforce invariants that prevent broken agents from being deployed. An agent with `requirements.txt` but no dependency layers will fail at runtime in any isolated sandbox — that's a hard invariant, not a suggestion. The "requirements.txt for dev only" case can be handled by the planner: if the dependency file isn't needed at runtime, it shouldn't be in the agent bundle, or the planner can document the exception in the evaluation step.

### Q3: ~~Import scan scope~~ Resolved: Scan entrypoint files only.

**Decision:** The import scanner checks only the file referenced in `script_entry` (if present), falling back to all `.py` files if no `script_entry` is declared.

**Rationale:** Test files commonly import test frameworks (`pytest`, `unittest` mocks, `responses`) that aren't needed at runtime. Scanning them would produce false positive warnings about "unresolved dependencies" that are actually dev dependencies. The entrypoint file is the authoritative source of runtime imports — if it `import requests`, the agent needs `requests`.

---

## 5. Implementation Status

### ✅ Already Implemented (this session)

| Change | File | Description |
|--------|------|-------------|
| Implicit artifact `content.named_outputs` | `workflow_store.rs:create_implicit_artifact` | Child session content names + refs now included in implicit artifact |
| Planner output access pattern | `planner.default/SKILL.md` | New "Reading Child Agent Outputs" section with `named_outputs` access pattern |
| Promotion gate for high-risk capabilities (3.1) | `agent_revision.rs` | Refuses promotion without evaluator/auditor pass records |
| Unresolved dependency detection (3.2) | `agent_revision.rs` / `install_contract.rs` | Warns on missing layers for dependency files; blocks promotion |
| External import detection (3.3) | `install_contract.rs` | Scans entrypoint files (or all files) for external imports |
| Bundle health diagnostic (3.4) | `install_contract.rs` | Structured `BundleHealthReport` for diagnostic feedback |
| Planner SKILL.md dependency check (3.5) | `planner.default/SKILL.md` | Decision Flow rule to spawn `packager.default` when needed |
| `sandbox_exec` network-isolation policy (3.6) | `runtime/tools/sandbox.rs` | `detect_network_errors_in_output` + failed tool result (`ok: false`, `error_type: network_isolated`) when isolated run output matches |
| Gateway lock identity (3.7) | `build.rs`, `install_contract.rs`, `runtime_lock.rs` | Source fingerprint (`sha256`, `build_tag`) + runtime executable digest (`binary_sha256`) populated by gateway |
| `force_complete` gate (A.1) | `workflow.rs` | Refuses `Succeeded` status without child session evidence |
| `capability_from_shorthand` gate (A.2) | `install_contract.rs` | Refuses bare shorthand for high-risk caps (e.g. "NetworkAccess") |
| Promotion record digest binding (3.8) | `agent_revision.rs`, `promotion_store.rs`, `promotion.rs`, `autonoetic-types/promotion.rs` | Binds/validates promotion evidence against canonical `content_digest`; reconciles or clears mismatched evidence |
| Null field cleanup (3.9) | `autonoetic-types/agent.rs` | `skip_serializing_if` on `Option` fields — SKILL.md no longer emits `llm_config: null` etc. |
| JSON-RPC auth gate (3.10) | `server/jsonrpc.rs`, `server/mod.rs`, `router.rs`, CLI/test clients | JSON-RPC requests now include `auth_token`; gateway rejects missing/mismatched token with unauthorized JSON-RPC error |
| Strict env override gates (3.11) | `sandbox.rs`, `llm/mod.rs`, docs | Security-sensitive env overrides are disabled by default and require explicit `AUTONOETIC_ALLOW_*_ENV_OVERRIDES=true` opt-in |

### 📋 To Implement

| ID | Change | Priority | Estimated Effort |
|----|--------|----------|-----------------|
| - | No open items in this spec — Tier 1, 2, and listed Tier 3 tasks are implemented | - | - |

---

## 6. Verification Plan

### ✅ Automated Tests — All Implemented

```
# Promotion gate integration tests (promotion_gate_hardening_integration.rs)
test_promote_rejects_high_risk_without_promotion_records          ✅
test_promote_succeeds_with_both_evaluator_and_auditor_pass        ✅
test_promote_rejects_when_evaluator_fails                         ✅
test_promote_rejects_when_auditor_missing                         ✅
test_promote_allows_low_risk_without_records                      ✅
test_promote_rejects_high_risk_with_unresolved_dependencies       ✅
test_full_pipeline_with_builder_and_promotion_gates               ✅
test_promote_accepts_precreate_records_when_digest_matches        ✅

# Capability shorthand tests (agent_revision.rs — capability_lenient_deser_tests)
string_shorthand_network_access_refused                           ✅
string_shorthand_code_execution_refused                           ✅
string_shorthand_read_access_allowed                              ✅
scoped_network_access_object_accepted                             ✅

# Import & bundle health tests (install_contract.rs)
test_detect_external_python_imports_finds_requests                ✅
test_detect_external_python_imports_ignores_stdlib                ✅
test_detect_external_python_imports_ignores_local_modules         ✅
test_analyze_bundle_health_warns_on_requirements_without_layers   ✅
test_analyze_bundle_health_no_warnings_when_layers_present        ✅

# sandbox_exec network fingerprint unit tests (sandbox.rs — network_error_detection_tests)
empty_output_matches_nothing                                      ✅
detects_stdlib_url_errors                                         ✅
detects_requests_traceback                                        ✅
ignores_plain_connection_word                                     ✅
marks_result_as_failed_when_network_failure_detected              ✅
leaves_result_untouched_when_no_network_failure_detected          ✅

# Gateway compile-time fingerprint (install_contract.rs)
test_gateway_build_sha256_is_not_placeholder                       ✅

# JSON-RPC auth gate tests (server/jsonrpc.rs)
test_jsonrpc_tcp_rejects_missing_auth_token_when_required          ✅

# Force-complete gate tests (workflow.rs — force_complete_gate_tests)
gate_refuses_succeeded_without_evidence                           ✅
gate_allows_failed_without_evidence                               ✅
gate_allows_succeeded_with_evidence                               ✅

# Null field cleanup test (install_contract.rs)
test_render_skill_document_omits_null_optional_fields             ✅
```

### Manual Verification

1. Re-run the demo-session-1 equivalent workflow
2. Verify `promote` fails when evaluator doesn't call `promotion_record`
3. Verify `create_from_intent` warnings show unresolved `requirements.txt`
4. Verify planner spawns `packager.default` before evaluator when `named_outputs` includes dependency files
5. Verify `create_from_intent` refuses bare `"NetworkAccess"` and returns scoped capability error
6. Verify `force_complete` refuses `succeeded` when child session has no completion evidence
7. Verify JSON-RPC ingress rejects requests without `auth_token` when gateway is running with `AUTONOETIC_SHARED_SECRET`
8. Verify `AUTONOETIC_BWRAP_*` and global `AUTONOETIC_LLM_*` env overrides are ignored unless matching `AUTONOETIC_ALLOW_*_ENV_OVERRIDES=true` is set

---

## Appendix A: Existing Escape Hatch Audit

Codebase audit conducted 2026-04-08. Findings below are ordered by severity.

### A.1 `workflow_force_complete` — Agent can mark tasks as "succeeded" without real evidence

**File:** `autonoetic-gateway/src/runtime/tools/workflow.rs:935–1183`

**What it does:** Any agent with `AgentSpawn` capability can call `workflow_force_complete` to transition a stuck task from `Running` to `Succeeded` or `Failed`. It gathers evidence (session manifest, digest, checkpoint, implicit artifact) but **proceeds regardless** — line 1128:

```rust
if !session_completed {
    evidence.push("WARNING: could not confirm child session completed — proceeding based on caller judgment".to_string());
}
// ... proceeds to mark as succeeded anyway
```

**Why this violates Rule Zero:** The calling agent decides whether a task succeeded. If the evaluator crashed mid-run, the planner can `force_complete(status: "succeeded")` and move on. The gateway's "evidence" is just a string attached to the record — it never refuses.

**Fix:** When `session_completed == false`, `force_complete` should **refuse** to set `Succeeded`. It may set `Failed` (the task is stuck, that's a legitimate failure), but `Succeeded` requires real evidence. The error message should list what evidence was missing.

### A.2 `capability_from_shorthand` — Wildcard scopes silently granted for high-risk capabilities

**File:** `autonoetic-gateway/src/runtime/tools/agent_revision.rs:200–243`

**What it does:** When an agent's SKILL.md declares a capability as a bare string (e.g., `"NetworkAccess"` instead of a JSON object), `capability_from_shorthand` converts it to a fully-permissive form: `NetworkAccess { hosts: ["*"] }`, `CodeExecution { patterns: ["*"] }`, etc.

**Why this violates Rule Zero:** For high-risk capabilities (`NetworkAccess`, `CodeExecution`, `AgentSpawn`), a bare string shorthand silently grants unrestricted access. The gateway never refuses — it fills in wildcards on the agent's behalf. An LLM that writes `"NetworkAccess"` instead of `{"type": "NetworkAccess", "hosts": ["api.weather.com"]}` gets full network access without ever being asked to scope it. The gateway is doing the agent a favor instead of enforcing a rule.

**Fix — Gate, don't fix:** `capability_from_shorthand` should **refuse** to expand bare strings for high-risk capabilities. Instead, it returns an error that tells the agent exactly what to provide:

```rust
fn capability_from_shorthand(s: &str) -> anyhow::Result<Capability> {
    match s.trim() {
        // Low-risk capabilities: shorthand is fine, these are not dangerous
        "SandboxFunctions" => Ok(Capability::SandboxFunctions {
            allowed: vec!["*".to_string()],
        }),
        "ReadAccess" => Ok(Capability::ReadAccess {
            scopes: vec!["*".to_string()],
        }),
        "WriteAccess" => Ok(Capability::WriteAccess {
            scopes: vec!["*".to_string()],
        }),
        "Evaluation" => Ok(Capability::Evaluation {
            patterns: vec!["*".to_string()],
        }),
        // High-risk capabilities: MUST be scoped explicitly
        "NetworkAccess" => Err(anyhow::anyhow!(
            "Capability 'NetworkAccess' requires explicit host scoping. \
             Use {{ \"type\": \"NetworkAccess\", \"hosts\": [\"api.example.com\"] }} \
             instead of the bare string. Wildcard hosts (\"*\") are not allowed \
             via shorthand."
        )),
        "CodeExecution" => Err(anyhow::anyhow!(
            "Capability 'CodeExecution' requires explicit command patterns. \
             Use {{ \"type\": \"CodeExecution\", \"patterns\": [\"python*\"] }} \
             instead of the bare string."
        )),
        "AgentSpawn" => Err(anyhow::anyhow!(
            "Capability 'AgentSpawn' requires explicit configuration. \
             Use a JSON object with 'max_children' and 'allowed_agents' fields."
        )),
        // ... remaining low-risk capabilities unchanged
    }
}
```

The calling agent (typically `specialized_builder`) receives the error, reports it back to the planner, and the planner tells the coder to re-write the SKILL.md with properly scoped capabilities. Standard **gate → explain → plan → re-check** loop.

Low-risk capabilities (`SandboxFunctions`, `ReadAccess`, `WriteAccess`, `Evaluation`, etc.) keep their shorthand — the wildcard default isn't dangerous for these.

### A.3 `required_eval_run_id` — Optional, not enforced for high-risk agents

**File:** `autonoetic-gateway/src/runtime/tools/agent_revision.rs:1272–1292`

**What it does:** `agent_revision_promote` accepts an optional `required_eval_run_id` parameter. If provided, it verifies the eval run passed. If omitted, no eval check is performed at all.

**Why this violates Rule Zero:** For high-risk agents, eval verification should be **mandatory**, not optional. Currently any agent with `AgentRevision` capability can promote any revision without any eval evidence.

**Fix:** This is exactly what Section 3.1 (Promotion Gate) addresses. The existing `required_eval_run_id` parameter becomes redundant for high-risk agents — the gate enforces it mechanically.

### A.4 `skip_llm` in pre-process hooks — Operator-level bypass, not agent-level

**File:** `autonoetic-gateway/src/runtime/lifecycle.rs:1330–1355`

**What it does:** Pre-process hooks (operator-configured shell scripts) can set `metadata.skip_llm: true` and provide a synthetic `assistant_reply`, bypassing the LLM call entirely.

**Why this is NOT a Rule Zero violation:** Hooks are operator-configured, not agent-controlled. An agent cannot set `skip_llm` on its own requests. The hook script runs as a separate process configured in gateway settings. This is an operator escape hatch, not an agent escape hatch — it's the operator's system to control.

**Status:** Acceptable. Documented for awareness.

---

## Appendix B: Demo-Session-1 Failure Timeline

```
Turn 20  planner spawns coder.default
Turn 22  coder writes requirements.txt (import requests)
Turn 23  coder writes weather_agent.py
         ← MISSING: packager.default should have been spawned here
Turn 28  planner spawns evaluator.default (no builder step)
Turn 35  evaluator runs sandbox_exec → works (requests pre-installed on host)
Turn 36  evaluator runs live HTTPS call → NetworkError (sandbox blocks it)
Turn 37  evaluator writes "APPROVE" report
         ← MISSING: evaluator never called promotion_record
Turn 39  planner spawns auditor.default
Turn 41  auditor completes (no promotion_record call)
         ← MISSING: auditor never called promotion_record
Turn 43  planner tries artifact_build → permission denied
Turn 44  planner spawns specialized_builder
Turn 52  specialized_builder calls agent_revision_create_from_intent
         ← MISSING: no warning about requirements.txt without layers
Turn 53  specialized_builder calls agent_revision_promote
         ← BUG: promote succeeds without any promotion gate evidence
Turn 54  Agent installed and promoted. Broken at runtime.
```

With the proposed changes:
- Turn 52 would emit warnings about `requirements.txt` and `import requests`
- Turn 53 would **fail** with: "Promotion gate: no promotion_record found for artifact 'art_425a482c'"
- The specialized_builder would report this failure to the planner
- The planner would need to iterate: spawn evaluator → evaluator calls `promotion_record` → spawn auditor → auditor calls `promotion_record` → retry promote
