# Agent Lifecycle: Wake, Reason, Hibernate

## Reasoning Agent Lifecycle

```
1. WAKE: Gateway receives event.ingest or agent_spawn
2. CONTEXT ASSEMBLY:
   - Load SKILL.md instructions
   - Inject foundation instructions
   - Load session context (if re-entering session)
   - Load conversation history (if forked session)
3. REASONING LOOP:
   - Build completion request (messages + tools)
   - Call LLM
   - Dispatch tool calls
   - Check stop reason:
     * end_turn → break
     * tool_use → execute, add result, continue
     * max_tokens → break
4. HIBERNATE:
   - Log session end in causal chain
   - Persist conversation history
   - Update session context
   - Return response
```

## Script Agent Fast Path

```
1. WAKE: Gateway receives event.ingest or agent_spawn
2. SCRIPT EXECUTION:
   - Resolve script path from manifest
   - Build sandbox command
   - Execute directly (no LLM)
   - Capture stdout as reply
3. HIBERNATE:
   - Log script.completed/failed
   - Return response
```

## Turn Continuation

When an agent hits an approval boundary mid-turn:
1. The turn is **suspended** with a continuation token
2. The operator resolves the approval
3. The turn **resumes** from the exact point of suspension
4. Continuation is HMAC-signed and verified on resume

## Child Agent Spawning

When spawning children:
- **Sequential/single child**: Spawn `async=true`, then **end your turn**. The gateway resumes you when the child completes.
- **Parallel fan-out**: Spawn all `async=true`, then call `workflow_wait(task_ids=[...])` **once** to join.
- **Never poll** — do not loop on `workflow_wait` or `workflow_state`.

## Extended Thinking

Agents can configure extended thinking in their SKILL.md:
```yaml
llm_config:
  thinking:
    effort: medium  # "low", "medium", "high"
```
The gateway translates this to each provider's native format.
