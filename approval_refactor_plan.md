# Refactoring Approval Flow: From Auto-Execution to Agent Retry

## Architectural Context

The current Autonoetic gateway intercepts restricted tool calls (e.g., `sandbox.exec` requiring network), suspends the agent's workflow, waits for an operator approval, and then **auto-executes** the command on the agent's behalf. It injects a synthetic JSON payload containing the execution results (`exit_code`, `stdout`, `stderr`) into the session as a wake-up signal.

**The Problem:**
This breaks the natural LLM ReAct loop. The LLM remembers failing to run a tool, goes to sleep, and wakes up with a JSON document saying the tool has already run. This strips the LLM of its agency, causing context fragmentation where it often forgets its overarching task and immediately calls `EndTurn`. Furthermore, placing execution logic inside the `scheduler/approval.rs` violates the "Dumb Gateway" principle—the gateway should be a mechanism enforcing policy, not orchestrating tool calls.

**The Solution:**
Transition to a **"Dumb Gate / Agent Retry"** model. The gateway simply unblocks the workflow with a success ping. The agent wakes up, sees its approval was granted, and uses its own agency to invoke the tool a second time, this time providing an `approval_ref`. The gateway statically validates this reference against the preserved command hash and permits the execution.

---

## 1. Gateway Runtime Changes (`autonoetic-gateway/src/scheduler/approval.rs`)

### Remove Auto-Execution
- Delete or modify `execute_approved_action()` specifically for `ScheduledAction::SandboxExec`.
- Delete `execute_approved_sandbox_exec()`. The gateway must no longer instantiate `SandboxExecTool` internally on approval.

### Simplify Wake-Up Notifications
- Update `resume_session_after_approval()` to send a simple, stateless notification for `sandbox_exec`.
  - **Old Behavior:** Injects the full JSON stdout/stderr payload.
  - **New Behavior:** `format!("Approval {} granted for your pending sandbox.exec. You must now RE-RUN your sandbox.exec command exactly as before, adding 'approval_ref': '{}' to the arguments.", request_id, request_id)`

### Simplify Task Checkpointing
- Update `build_approval_resume_message()`. It no longer needs to format the complex `execution: { stdout, stderr }` object. It just needs to provide the `status: "approved"` and `request_id`.
- The durable workflow resume state (`store_approval_result_in_checkpoint`) should simply inform the agent that it is unblocked and needs to retry.

## 2. Gateway Tool Changes (`autonoetic-gateway/src/runtime/tools.rs`)

### `SandboxExecTool` Validation
- The tool currently accepts an `approval_ref` and validates it against `ApprovalStore`. Make sure this logic remains totally intact.
- Ensure that if an agent retries with the correct `approval_ref`, the `ApprovalStore` marks it as 'consumed' (or handles idempotency correctly if retried multiple times).

## 3. Agent Prompts (`agents/specialists/coder.default/SKILL.md`)

Once the backend is updated, we must revert the agent instructions to the pure ReAct model:

```yaml
## Remote Access Approval (CRITICAL)

When `sandbox.exec` returns `approval_required: true` with `request_id`:
**STOP and WAIT**. Do not continue or retry until the user approves.

**After you receive an approval_resolved message:**
1. Retry `sandbox.exec` with the EXACT SAME command PLUS the `approval_ref`:
   {
     "command": "python3 /tmp/script.py",
     "approval_ref": "[request_id]"
   }
2. Use the output from this retried command to continue your work.
3. REMEMBER: Check if your overarching goal is complete before ending your turn.
```

## 4. Additional Brittleness & "Smart Gateway" Anti-Patterns Found

During deep review of the approval flow, several other areas were identified where the gateway enforces logic instead of mechanism, creating brittleness:

### A. Auto-Execution of `AgentInstall` and `WriteFile`
While `SandboxExec` is moving to a retry model, `scheduler/approval.rs` **still auto-executes** `agent.install` and `content.write`. 
- **The Danger**: For `agent.install`, the gateway dynamically builds a synthetic `parent_manifest` (with empty limits and capabilities) to execute the tool on behalf of the agent. This strips the action from the actual context and policy bounds of the calling agent (e.g., `specialized_builder.default`).
- **The Fix**: These must also move to the "Dumb Gate / Agent Retry" model. The gateway should merely ping the builder agent that the install is approved, and the builder should retry `agent.install` with `install_approval_ref`.

### B. Brittle Exact-String Matching on Retries
In `SandboxExecTool::execute` (and similarly in `AgentInstallTool`), the gateway validates the retry by comparing the currently submitted command exactly character-for-character against the original blocked command (`if command == &args.command`).
- **The Danger**: LLMs are not deterministic copiers. If the LLM adds a trailing space, changes a quote, or reformats the JSON slightly when generating the retry payload, the gateway rejects the valid `approval_ref`. 
- **The Fix**: The `ApprovalStore` should bind the `approval_ref` to the semantic intent, or the gateway should merge/inject the approved payload automatically when a valid `approval_ref` is presented, rather than forcing the LLM to blindly reproduce an exact replica of a 500-character payload.

### C. Hardcoded Session Notification Logic
In `should_notify_parent_session`, the gateway attempts to guess the orchestration topology by parsing strings (`session_id.contains('/')`). 
- **The Danger**: The gateway is attempting to orchestrate the "Planner" by guessing if it's the parent of an "Evaluator". The gateway should not know about "Planners" or "Evaluators".
- **The Fix**: Notifications should be strictly hierarchical and routed based on the Task graph (the `workflow_id` / `task_id`), rather than string-parsing session IDs.

## 5. Impact on Causal Tracing

- Currently, traces show a `tool.invoke` (which fails with approval required), followed by a synthetic `gateway_auto_complete` event injected by the approval handler.
- Under the new architecture, traces will show a `tool.invoke` (fail/block), followed by an `agent.wake`, followed by a *second* `tool.invoke` (success). 
- This is much more semantically correct for an agent trace, as it clearly shows the LLM's agency and makes debugging much more transparent.
