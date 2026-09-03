# Worklist: homogenize hand-built tool errors onto the canonical envelope (P-5.11)

Status: **TODO** — migration worklist (the "one big PR" rollout slice).
Refs: `docs/reference/tool-errors.md` (the contract), constitution **P-5.11**, PR #532
(the promotion-gate exemplar of the target pattern), `autonoetic-types/src/tool_error.rs`.

## Goal

Every native-tool failure must use the **one** canonical envelope (`ToolError`):

```jsonc
{ "ok": false, "error_type": "validation|permission|execution|conflict|resource|not_found|…",
  "error": "<stable_snake_case_code>",   // optional but preferred
  "message": "<human/LLM prose>",
  "repair_hint": "<mechanism-level remedy>" }   // optional
```

Many tools instead hand-build `Ok(serde_json::json!({ "ok": false, "error": "<prose>" }))`:
- no `error_type`,
- `error` holds **prose**, clashing with the stable-**code** semantics (`error: "auditor_pass_missing"`),
- usually no `message` / `repair_hint`.

Migrate every such site to construct via `ToolError` so the output is canonical and
agents branch on `error_type` + stable `error` code, not prose.

## Transformation pattern

**Before**
```rust
return Ok(serde_json::to_string(&serde_json::json!({
    "ok": false, "error": "Workbench not found"
}))?);
```
**After**
```rust
use autonoetic_types::tool_error::ToolError;   // add at top of file if absent
return Ok(ToolError::not_found("Workbench", Some("Create or project a workbench first."))
    .with_code("workbench_not_found")
    .to_error_response());        // returns String — note: NO `serde_json::to_string(..)?` wrapper
```

Builders (`autonoetic-types/src/tool_error.rs`):
- `ToolError::validation(msg, Some(hint))` — bad/missing args, malformed input.
- `ToolError::permission(msg)` — policy/gate denial (sets a default hint; override with `.with_repair_hint`).
- `ToolError::conflict(msg, Some(hint))` — wrong state (e.g. "cannot X a {status} workbench").
- `ToolError::resource(msg, Some(hint))` — missing file/dir/dependency.
- `ToolError::not_found(resource, Some(hint))` — formats `"{resource} not found"`.
- `ToolError::execution(msg, Some(hint))` — internal/unexpected (e.g. "Gateway store not available").
- chain `.with_code("snake_case")` (preferred) and `.with_enforced_rules(vec!["P-x.y".into()])` when a rule is enforced.

**Stable-code naming:** snake_case, terse, names the *unmet precondition* (`workbench_not_found`,
`gateway_store_unavailable`), not the fix. Reuse a code rather than minting a synonym.
**`message` keeps the original prose** (tests/agents read it). **`repair_hint` is mechanism-level**
— state the remedy, do not prescribe which agent to spawn (gateway states the rule, not the plan).

## File worklist

### Non-canonical — migrate every `"error":` site (error_type currently absent)
| File | sites | notes |
|---|---:|---|
| `runtime/tools/workbench.rs` | 31 | detailed below (worked example) |
| `runtime/tools/plan_frame.rs` | 11 | |
| `runtime/tools/validation.rs` | 9 | several `"Gateway X not available"` guards + `Invalid artifact_id …` |
| `runtime/tools/workflow.rs` | 5 | |
| `runtime/tools/agent.rs` | 3 | |
| `runtime/tools/tool_discover.rs` | 2 | |
| `runtime/tools/evaluation.rs` | 2 | |
| `runtime/tools/promotion.rs` | 1 | |
| `runtime/tools/improvement.rs` | 1 | |
| `runtime/tools/agent_inspect.rs` | 1 | |

### Mixed — review per-site (keep ones already paired with `error_type` + stable code; migrate prose ones)
| File | `"error":` lines | migrate | keep (already code) |
|---|---|---|---|
| `user_interaction.rs` | 134,174,191,235,272,435 | 174,191,235,272,435 (prose) | 134 `secret_collection_not_allowed` (add `error_type` if missing) |
| `credential.rs` | 126,302,461,552 | all (prose/`message` var / format!) | — |
| `web.rs` | 945,1343,1957 | all (`Network access denied …`) → `permission` + code `network_access_denied` | — |
| `session.rs` | 229,348,360,477,484,496 | all (prose) | — |
| `skill.rs` | 117,162,770,786,852 | all (format!/prose) | — |
| `observability.rs` | 233,421,438 | all (format!/prose) | — |
| `resolve.rs` | 318 | ensure `error_type` present | 318 `content_not_found` (already a code) |
| `tools/mod.rs` | 923 | ensure `error_type` present | 923 `invalid_artifact_id` (already a code) |

### Already canonical — leave
- `runtime/tools/agent_revision.rs` (7/7 — done in #532), and any `"error":` paired with `"error_type":` + a snake_case code.

Find remaining non-canonical sites:
```bash
grep -rn '"error":' autonoetic-gateway/src/runtime/tools/*.rs   # then check each lacks "error_type" / holds prose
```

## Worked example — `workbench.rs` (suggested type+code per site)

| line(s) | current `error` prose | `error_type` | code |
|---|---|---|---|
| 301, 986 | Gateway directory not available | execution | `gateway_dir_unavailable` |
| 307, 492, 598, 703, 793, 863, 992, 1288, 1381 | Gateway store not available | execution | `gateway_store_unavailable` |
| 313, 998 | Gateway config not available | execution | `gateway_config_unavailable` |
| 498, 604, 709, 875, 1007, 1294, 1387 | Workbench not found | not_found | `workbench_not_found` |
| 869 | Checkpoint not found | not_found | `checkpoint_not_found` |
| 895 | Checkpoint files not found on disk | resource | `checkpoint_files_missing` |
| 1021 | Workbench source directory does not exist | resource | `workbench_source_missing` |
| 610, 715, 881, 1014, 1301 | Cannot {verb} a `{status}` workbench / is in `{status}` status | conflict | `workbench_wrong_status` |
| 1394 | Cannot clean up an active workbench. Reconcile or discard first. | conflict | `workbench_wrong_status` |
| 1038 | No files in workbench to reconcile | conflict | `workbench_empty` |
| 724 | Checkpoint failed: {e} | execution | `checkpoint_failed` |

(Apply the same judgment to the other files — infra "not available" → execution; missing thing →
not_found/resource; wrong state → conflict; bad input → validation; policy denial → permission.)

## Tests to repoint

Any test asserting `result["error"] == "<prose>"` (or `.contains` on the `error` field, or
`result.is_err()`/`unwrap_err()` for a now-`Ok(ok:false)` block) must move to inspecting `message`
or the new stable `error` code. The reusable adapter from PR #532 (re-presents a structured block
as `Err(message)` so existing arms read unchanged):

```rust
fn as_outcome(result: Result<serde_json::Value, String>) -> Result<serde_json::Value, String> {
    match result {
        Ok(v) if v["ok"] == serde_json::Value::Bool(false) =>
            Err(v["message"].as_str().unwrap_or_default().to_string()),
        other => other,
    }
}
```
Grep the test suite for `["error"]` / `unwrap_err` / `is_err` near each migrated tool.

## Verify
```bash
cargo build -p autonoetic-gateway
cargo test -p autonoetic-gateway            # focus the touched tools' suites
cargo test -p autonoetic-gateway --test constitution p_5_11_uniform_error_envelope
```
Consider extending `tests/constitution/p_5_11_uniform_error_envelope.rs` to assert `error_type` is present
on every tool failure (guard against regressions).
