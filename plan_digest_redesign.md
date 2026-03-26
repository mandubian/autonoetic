# Live Digest Redesign: The "Group Chat" Model

## Problem Statement
When agents run in parallel, their turns interleave. If the file is strictly chronological and flat, reading it feels like parsing `tail -f /var/log/messages` with interleaved PIDs. It is currently too noisy with system metadata, inline JSON blobs, and repeated headers, making it hard for both humans to read and LLMs to parse without wasting tokens.

## The Solution
To make a single, real-time, append-only file work, we treat it like a multi-user chat room. If an agent takes consecutive actions, they are bundled under **one header**. When another agent's log comes in (a context switch), we emit a visual break and a new header.

### Core Rules

#### 1. Only write headers on Context Switches
The system tracks the `last_logged_agent_id` in memory.
- If `current_agent_id != last_logged_agent_id`, append a horizontal rule (`---`) and a bold identity header.
- If it's the same agent, just append the Turn. This stops the file from repeating `### planner.default — ` on every single turn.

#### 2. Kill the Noise (Metadata goes to DB)
Remove all system metadata that clutters the narrative:
- `tokens in/out: 11288/236` ➔ Drop it.
- `stop: ToolUse` ➔ Drop it (it's implied by the tool call).
- `2026-03-25T22:48:08.199114368+00:00` ➔ Truncate to just `[22:48:08]`.
- `*Turn wrap-up: 1 tool call(s) in this block.*` ➔ Drop it.

#### 3. Bullet-Point Actions & Truncate Inline JSON
Inline JSON strings break readability. Map the JSON into brief representations where possible, and use markdown lists.

#### 4. Explicit State Markers
Use distinct emojis or tags for lifecycle events so a human can quickly skim to find where tasks started/stopped.
- ✅ / 🟢 Success / Outcome
- ❌ / 🔴 Error
- 🛠️ Tool Call
- 🧠 Reasoning/Decision
- ⏸️ / ↻ Hibernating / Resumed
- 📄 Result

---

## Example Output

```markdown
# Live Digest: demo-session-2

### 👑 planner.default

**Turn 29** [22:48:14]
* 🛠️ **Tool:** `agent.spawn(agent_id="hello-world")`
* ❌ **Error:** Script execution failed (Invalid JSON input)

**Turn 30** [22:48:21]
* 🧠 **Reasoning:** 'hello-world' agent exists but is outdated. Overwriting.
* 🛠️ **Tool:** `agent.spawn(agent_id="specialized_builder.default")`
* ⏸️ **State:** Hibernating (waiting for child workflow to complete)

---
### 🤖 specialized_builder.default

**Turn 124** [22:48:22]
* 🛠️ **Tool:** `artifact.inspect(artifact_id="art_bd9603c2")`
* 📄 **Result:** 2 files found

**Turn 125** [22:48:35]
* 🛠️ **Tool:** `agent.install(agent_id="hello-world", artifact_id="art_bd9603c2")`
* ❌ **Error:** child agent 'hello-world' already exists

**Session Summary** [22:48:40]
* ✅ **Outcome:** `jsonrpc_spawn_complete`
* 📉 **Stats:** 67 turns | 6 tools | 2 errors

---
### 🕵️ auditor.default 

**Turn 119** [22:53:55]
* ✅ **Outcome:** `jsonrpc_spawn_complete` (119 turns, 0 errors)

---
### 👑 planner.default

**Turn 46** [22:54:03]
* ↻ **State:** Resumed from hibernation
* 📄 **Result:** `workflow.wait` ➔ All tasks completed successfully.
* 🛠️ **Tool:** `agent.spawn(agent_id="hello-world-v4")`

---
### 🤖 specialized_builder.default (Session #2)

**Turn 1** [22:54:09]
* 🛠️ **Tool:** `artifact.inspect(artifact_id="art_f8f4c2b2")`
* 📄 **Result:** 2 files found
```

---

## Why this design is better

### For LLM Agents
1. **Low token cost:** Stripping nanoseconds, giant inline JSON blobs, and repeated headers saves a massive amount of prompt tokens when this digest gets fed back into an agent (via a tool like `digest.query`).
2. **Context Barriers:** The `---` dividers map strongly to "context switching" in LLM attention mechanisms, minimizing confusion between parallel agents.
3. **Structured Format:** Markdown lists (`* 🛠️ **Tool:** ...`) are vastly more predictable for the LLM to parse than raw text paragraphs.

### For Humans
1. **Skimmable:** You can scroll rapidly and look for `❌` to find errors, or `---` to see when the workload shifted.
2. **Chunked Context:** Grouping consecutive turns by the same agent under one header makes the chronological interleaving feel natural rather than overwhelming.

---

## Implementation Outline

The gateway component that writes the `digest.md` needs a simple in-memory tracker to manage the context switches:

```rust
struct DigestWriter {
    last_agent_id: Option<String>,
}

impl DigestWriter {
    fn append_turn(&mut self, turn: TurnEvent) {
        if self.last_agent_id.as_deref() != Some(&turn.agent_id) {
            // Context switch: append divider and new header
            write!(file, "\n---\n### {} {}\n\n", get_agent_emoji(&turn.agent_id), turn.agent_id);
            self.last_agent_id = Some(turn.agent_id.clone());
        }
        
        // Write turn details cleanly
        write!(file, "**Turn {}** [{}]\n", turn.turn_number, format_time_hhmmss(turn.timestamp));
        // ... append tools, errors, state changes as bullet points
    }
}
```
