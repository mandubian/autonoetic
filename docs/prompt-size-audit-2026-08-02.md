# Prompt Size & Caching Audit — 2026-08-02

Measured baseline of autonoetic's default prompts and prompt-cache effectiveness,
built for comparison with the Pi coding agent (and Claude Code / OpenCode / Cline
as scale references).

## 1. Methodology

- **Session**: `session-fc1a0286` (root: `planner.default`, two `coder.default`
  children), 2026-08-02 12:09–12:38 UTC, model `deepseek-v4-flash` via **OpenRouter**
  (262,144-token context window). Session status `failed` (coder LLM request
  failures), but the planner's LLM exchange data is complete.
- **Sources**:
  - `prompt_budget` causal events (30) — gateway's pre-call token estimate
    (system / tool definitions / conversation / total / utilization).
  - `completion` causal events (30) — provider-reported `input_tokens` /
    `output_tokens`.
  - Gateway run log — `cached_tokens` per exchange
    (OpenRouter `prompt_tokens_details.cached_tokens`, a subset of `input_tokens`).
- **Definitions**: `fresh = input − cached` (tokens actually re-billed);
  hit rate = `cached / input`.
- **Caveats**: single session, single model, planner agent only. Estimates use
  the `chars_per_token` heuristic (default 3.0); provider-reported counts are
  ground truth.

## 2. Default prompt size (measured)

### planner.default (root orchestrator)

| Component | Est. tokens (gateway) | Actual (provider) |
|---|---|---|
| System prompt | 27,133 (turn 1, grows to 27,389) | ≈19.4k (inferred) |
| Tool definitions | 16,146 (35 tools) | ≈11.8k (inferred) |
| Conversation (turn 1) | 25 | 25 |
| **Turn-1 total** | **43,304** | **31,510** |
| Steady-state per turn (avg of 30 calls) | 43,304–53,567 | 35,602 |
| Utilization of 262k window | 16.5% (est) | 12.0% (actual) |

System-prompt composition (bytes on disk): foundation doctrine
(`foundation_*.md`, 6 files, 20.7 KB ≈ 5.2k tok) + planner SKILL.md (**55.3 KB,
the single largest contributor** ≈ 13.8k tok) + tool guidance blocks + output
contract + state-attestation preamble + memory context (volatile tail).
Full measured breakdown with actual tokens: §7.

### coder.default (specialist, spawned children)

| Component | Est. tokens (gateway) |
|---|---|
| System prompt | 19,400 |
| Tool definitions | 10,972 (**26 tools** — capability-filtered) |
| Total | 31,519 |

No completions recorded: both coder children died with `request_failed`
(LLM network errors) — the same failure seen in the session digest.

## 3. Cache effectiveness (measured, 30 planner completions)

| Metric | Total | Per turn (avg) |
|---|---|---|
| Input tokens | 1,068,081 | 35,602 |
| **Cached (hit)** | **575,104** | **19,170** |
| Fresh (re-billed) | 492,977 | 16,432 |
| Hit rate | **53.8%** | — |
| Output tokens | 23,686 | 789 |
| Reasoning tokens (of output) | 13,033 (55%) | 434 |

Key observations:

1. **Cache covers the system prompt and nothing else.** The cached amount is
   flat across the session (18,816–19,584) and matches the system prompt's
   actual token count (est 27,133 at 3.0 c/t ⇒ ≈19.4k at measured 4.1 c/t).
   Fresh input grows exactly with conversation (12.4k → 17.9k). The **16k-token
   tool array is re-billed every call** even though it is byte-identical across
   turns — the provider's cached prefix stops at the end of the system prompt.
   (Root cause: §8.1 — DeepSeek prefix-unit caching; not an autonoetic bug.)
2. **Warm start.** Even the first recorded call had 19,072 cached tokens
   (60.5% of input): the planner system prompt is deterministic per SKILL
   revision, so cross-session prefixes (earlier planner sessions on the same
   revision) were served from cache.
3. **Stable-prefix design is correct** — no Pi-class cache-busting bug:
   the per-turn volatile tail (memory context, degradation notice, re-signed
   state attestation) is appended **after** the `system_cache_prefix_bytes`
   breakpoint (lifecycle.rs:2763–2795), so the cached region never changes.
   The attestation timestamp lives in the uncached tail.
4. **No explicit cache markers for this model.** The gateway's OpenRouter
   `cache_control` gating covers only `anthropic/*` and `gemini/*` models
   (openai.rs `model_supports_openrouter_cache_control`); DeepSeek gets no
   breakpoints and relies entirely on OpenRouter automatic prefix caching,
   whose retention is provider-controlled (short — a pause between turns
   risks eviction). Explicit `cache_control` with `prompt_cache_retention:
   "24h"` is only emitted on the OpenCode Go driver.

## 4. Estimate accuracy (chars_per_token)

The gateway's default 3.0 chars/token overestimates this prompt mix:
measured density is **≈4.1 chars/token** (est 43,304 vs actual 31,510 on
turn 1; conversation est grew 3× the actual growth). Utilization is
consequently overstated ~35% (16.5% est vs 12.0% actual). Overestimation is
conservative for caps (governor fails early) but misleads observability.
The config knob exists: `prompt_budget.chars_per_token` (4.0 would align).

## 5. Comparison vs Pi coding agent

| | **Autonoetic planner (measured)** | **Pi (default)** | Claude Code | OpenCode | Cline |
|---|---|---|---|---|---|
| System prompt | 27.1k est / ~19.4k actual | **< 1,000** | ~10k | ~10k+ | ~7k |
| Tool definitions | 16.1k est (35 tools) | included in <1k (**4 tools**) | many | many | many |
| Cold turn-1 prompt | 31,510 | ~1–2k | ~12k+ | ~12k+ | ~8k+ |
| Static scaffolding | ~43k est | <1k | ~12k+ | ~12k+ | ~8k+ |
| Cache hit rate (measured) | 53.8% | not measured (tooling exists) | n/a | n/a | n/a |
| Cache instrumentation | `cached_tokens` in run log + causal chain only | cacheRead/cacheWrite in session stats, TUI cache-waste detection (`cache-stats.ts`), `PI_CACHE_RETENTION` (Anthropic 1h / OpenAI 24h) | — | — | — |
| Cache design risk | none — volatile tail after breakpoint | had second-level-timestamp-in-prefix bug (#1873, fixed to date-only) | — | — | — |

Sources: `pi.dev`, mariozechner.at/posts/2025-11-30-pi-coding-agent ("system
prompt and tool definitions together come in below 1000 tokens"),
dreaming.press + towardsai (2026-06) for competitor estimates,
earendil-works/pi repo (system-prompt.ts, cache-stats.ts, issue #1873).

**The headline gap**: autonoetic's static scaffolding (~43k est / ~31k actual
on turn 1) is **~40× Pi's entire system prompt+tools**. Autonoetic's caching
is doing its job — the ~19k-token system prompt is essentially 100%
cache-covered every turn, avoiding ~46% of the input bill — but the un-cached
remainder (tools + conversation, ~16.4k fresh/turn avg) is still ~16× Pi's
entire cold prompt. Pi's philosophy is the inverse: a tiny prefix makes
caching almost irrelevant, since even a full cache miss costs ~1k tokens.

## 6. Findings & recommendations

1. **Tool array is never cache-covered (est 16.1k/35 tools, re-billed every
   turn).** **Resolved — §8.1**: DeepSeek prefix-unit caching + the per-turn
   attestation block define the cache edge at the end of the system prompt;
   tool schemas cannot extend it on this provider. Where breakpoints are
   supported (Anthropic, Claude/Gemini on OpenRouter, OpenCode Go) the
   gateway already marks system prefix + last tool — that path reaches
   ~88% hit rate / −60% input cost (§8.1 math).
2. **SKILL.md is the biggest prompt contributor** (planner 55.3 KB; unit_test_runner
   14.3 KB; coder 32.2 KB). **Root-caused — §8.3**: the `<!-- extended -->`
   deferred-load split (PR #218) is re-inlined at runtime
   (`inline_extended`, context.rs:321–340) because agents never fetched the
   extended half (session-3b4485d4 audit). Re-enabling requires solving the
   "agents don't fetch" problem, not just re-wiring the parser.
3. **Tool tiering works** (coder already ships 26 tools/10.9k est vs planner
   35/16.1k): for task-specific agents the `demote_tools`/tier filter is the
   cheapest prompt-size lever available today. Planner only invoked 11 of its
   35 tools (25 calls) in this session.
4. **Tune `prompt_budget.chars_per_token` to 4.0** (measured 4.27 prose /
   3.90 JSON — §8.2) so utilization/cap accounting matches provider reports
   (currently overstated ~35%).
5. **Surface `cached_tokens` in the session report** (as Pi shows
   cacheRead/cacheWrite in its TUI): today it is only in the run log and
   causal chain, invisible to operators reading `session_report.md`.
6. **For stop-and-go orchestrator workloads**, prefer models with explicit
   cache breakpoints + long retention (Anthropic-style; autonoetic already
   emits `cache_control` + `prompt_cache_retention: "24h"` on the OpenCode Go
   driver, and `ephemeral` breakpoints for Anthropic/Claude/Gemini via
   OpenRouter) — DeepSeek auto-cache eviction during multi-minute waits
   (this session: 4–10 min idle gaps between work bursts) is a hit-rate risk.
   The one >5-min gap observed (9m15s) coincided with the session's lowest
   cached value (18,816) — consistent with OpenRouter's 5-minute TTL (§8.1).

## 7. System prompt content analysis (measured, 81,397 chars)

Extracted from the persisted session history (content-store
`sha256:4d013e98…`, system message). Tokens at the measured 4.27 chars/token
density (est@3.0 shown for comparison with gateway logs).

| Component | System lines | Chars | Est tok @3.0 | Actual tok @4.27 | % of system |
|---|---|---|---|---|---|
| Foundation Core | 1–72 | 8,749 | 2,916 | 2,049 | 10.7% |
| Foundation Workflow | 73–132 | 4,668 | 1,556 | 1,093 | 5.7% |
| Foundation Artifact | 133–150 | 1,262 | 421 | 296 | 1.6% |
| SDK Reference | 151–217 | 3,814 | 1,271 | 893 | 4.7% |
| **Planner SKILL.md** | **252–921** | **52,557** | **17,519** | **12,308** | **64.6%** |
| Tool Guidance | 218–251 | 6,271 | 2,090 | 1,469 | 7.7% |
| Output Contract | 922–945 | 2,843 | 948 | 666 | 3.5% |
| Attestation (preamble+block) | 946–984 | 1,233 | 411 | 289 | 1.5% |
| **Total** | | **81,397** | **27,132** | **19,063** | 100% |

(The gateway's turn-1 estimate was 27,133 — exact at 3.0 c/t. Foundation
total: 18.0%.)

Key content findings:

1. **The SKILL.md body is 65% of the system prompt.** And it is the **full
   741-line SKILL**, not the ~285-line core: `<!-- extended -->` at
   SKILL.md:315 is decorative at runtime (`inline_extended`, context.rs:321).
   The section boundaries in the system prompt confirm the extended half is
   present (`## Artifact Execution vs Script-Agent Promotion`, `## Evaluation
   Federation`, `## Discovery`, `## Terminal signals`, … — all post-marker).
2. **Foundation doctrine is 18%** (6 files, ~3.4k actual tokens). Notably
   smaller than the SKILL despite containing the constitutional rules,
   egress law, artifact/script doctrine and the SDK reference.
3. **The attestation block is the effective cache edge**: the observed
   cached floor (18,816) matches the system prompt minus the per-turn
   volatile tail (re-signed attestation JSON ≈278 tokens + memory context)
   to within ~0.2% (static remainder 18,784). Turn 1's larger cached value
   (19,072) matches its empty `payload: {}` (§8.1).
4. **Conversation grows little**: ~3.6k actual tokens added over the whole
   session (est said 10.2k — estimate includes `reasoning_content` of
   messages, an output artifact, and uses 3.0 c/t; §8.4).
5. **Tool definitions (48,438 chars, 35 tools) are 39% of the cold turn-1
   input** at 3.90 chars/token — the densest component (JSON schemas tokenize
   tighter than prose).
6. Content quality note: the system prompt carries ~10KB of doctrine + 55KB
   of SKILL for a session whose actual work used 11 distinct tools — the
   instruction:task ratio is extreme versus Pi's <1k-token total.

## 8. Open questions — investigated

### 8.1 Why the cache stops at the system prompt (resolved)

Two mechanisms, both provider-side, both now evidenced:

**DeepSeek prefix-unit caching.** DeepSeek's context cache
(api-docs.deepseek.com/guides/kv_cache) does not cache a continuous prefix:
it persists discrete *prefix units* (at end-of-input, end-of-output, detected
common prefixes, and fixed token intervals) and a request only hits units it
**fully matches**. The end-of-input unit moves every turn (conversation
grows), so the region after the stable system prompt — tools + conversation —
never forms a fully-matching unit. Only the stable common-prefix unit hits.
This is why the 12.4k-token tool array is re-billed every call despite being
byte-identical: on DeepSeek it structurally cannot be covered by implicit
caching. (Confirmed: gateway-side tool serialization is deterministic —
registry `Vec` order, constant 35 tools / 16,146 est across all 30 calls;
guidance stable; OpenRouter sticky routing `session_id` is emitted,
openai.rs:450–456.)

**The attestation block defines the unit boundary.** The cached floor
(18,816) matches the static system prompt excluding the per-turn volatile
tail — the re-signed attestation JSON (1,188 chars ≈278 tokens) and memory
context — to within ~0.2% (static remainder 18,784 tokens). The
attestation's per-turn mutation (turn_counter, pending approvals, signature)
is the effective cache edge, by design (lifecycle.rs:2763–2795 puts it in the
volatile tail — correct for correctness, but it also caps DeepSeek's implicit
cache at the system prompt).

**TTL eviction observed.** OpenRouter's cache expires after 5 minutes idle
(provider sticky routing docs). The session's only >5-min gap (9m15s at
12:22:43→12:31:58) coincides with the session-low cached value (18,816).

**What it would take to do better** (hypothetical, explicit-breakpoint
path): cached prefix system+tools = 19,063+12,413 = 31,476 tokens →
hit rate 88.4%, fresh avg 16,432 → 4,126. On DeepSeek pricing (cache read
0.1× input) that is a **−60.4% input-cost cut** (18,349 → 7,274
input-equivalents/turn). The gateway's existing `cache_control` machinery
(system prefix + last-tool marking for Anthropic, Claude/Gemini via
OpenRouter, and OpenCode Go) already delivers this on providers that honor
breakpoints; DeepSeek does not offer breakpoints.

### 8.2 chars_per_token accuracy (confirmed, quantified)

The persisted system message is 81,397 chars and the gateway estimated
27,133 tokens at the default 3.0 c/t — exactly consistent. Provider-reported
turn-1 tokens: system 19,063 (cached) + tools 12,413 + conversation 25 =
31,501 ≈ 31,510 reported. Measured densities: **prose 4.27 c/t, tool-JSON
3.90 c/t**. The 3.0 default therefore overstates counts ~35% (utilization
16.5% est vs 12.0% actual). Setting `prompt_budget.chars_per_token: 4.0`
aligns estimates with the provider; the error is conservative for caps but
misleads observability and the context governor's decisions.

### 8.3 SKILL.md dominance (root-caused)

`<!-- extended -->` (SKILL.md:315) was the deferred-load optimization from
PR #218 (Phase 4), **reverted to inlining** after an audit of
session-3b4485d4 found agents never issued `resolve("extended_instructions")`
and silently lost critical guidance (context.rs:321–340, with the full
rationale; the split parser and `LoadedAgent.extended_instructions` remain
for a future re-wire). The planner SKILL body in the system prompt (52,557
chars) is core+extended concatenated. Re-enabling deferred loading requires
making retrieval happen mechanically (e.g., gateway auto-injects the extended
half when the agent first calls a tool whose contract lives there, or a
hybrid: keep promotion/eval gates in core, defer reference tables), not just
un-reverting the parser.

### 8.4 Conversation estimate overcounts (explained)

`estimate_message_tokens` (prompt_budget.rs) counts `reasoning_content` —
an **output** artifact — into conversation tokens, plus uses 3.0 c/t. Measured
conversation growth over the session ≈3.6k actual vs 10.2k estimated (~3×).
Same fix as 8.2 plus excluding reasoning from input-side estimates.

### 8.5 Tool usage vs availability (planner)

Of 35 tool definitions (12.4k tokens), the planner invoked only **11
distinct tools** (25 calls): credential_setup ×5, workflow_wait ×3,
digest_annotate ×3, agent_spawn ×3, workflow_state ×2, user_ask ×2,
credential_check ×2, agent_list ×2, content_write ×1, content_patch ×1,
agent_discover ×1. The unused ~24 definitions (artifact_exec, sandbox_exec,
knowledge_*, eval/revision/promotion, web_search, …) are inert weight in
every request — the tier filter (`demote_tools`) is the existing, cheap lever.

## 9. Appendix — full exchange series (planner, deepseek-v4-flash, OpenRouter)

30 completions, turns 1–11. Turn 2–3 include the largest tool-call bursts
(5–7 calls/turn); the 4–10 min gaps are human-approval waits.

| Turn | input | cached | fresh | output | reasoning |
|---|---|---|---|---|---|
| turn-000001 | 31510 | 19072 | 12438 | 193 | 84 |
| turn-000001 | 31751 | 19072 | 12679 | 82 | 25 |
| turn-000002 | 31965 | 18816 | 13149 | 1533 | 1412 |
| turn-000002 | 34767 | 18816 | 15951 | 405 | 286 |
| turn-000002 | 36389 | 19456 | 16933 | 1410 | 1019 |
| turn-000002 | 34825 | 19456 | 15369 | 208 | 34 |
| turn-000002 | 35157 | 19456 | 15701 | 500 | 257 |
| turn-000002 | 35694 | 19456 | 16238 | 3217 | 2839 |
| turn-000003 | 36014 | 18816 | 17198 | 323 | 164 |
| turn-000003 | 36437 | 19200 | 17237 | 351 | 69 |
| turn-000004 | 35998 | 19200 | 16798 | 170 | 23 |
| turn-000004 | 36268 | 19200 | 17068 | 711 | 536 |
| turn-000004 | 33917 | 19456 | 14461 | 1478 | 411 |
| turn-000004 | 35535 | 19456 | 16079 | 524 | 128 |
| turn-000005 | 36185 | 19200 | 16985 | 247 | 131 |
| turn-000005 | 36690 | 19200 | 17490 | 261 | 126 |
| turn-000006 | 37168 | 19200 | 17968 | 387 | 275 |
| turn-000006 | 36739 | 19200 | 17539 | 1483 | 417 |
| turn-000006 | 35524 | 19456 | 16068 | 215 | 24 |
| turn-000007 | 35781 | 19200 | 16581 | 643 | 493 |
| turn-000007 | 36692 | 19200 | 17492 | 219 | 86 |
| turn-000008 | 36426 | 19200 | 17226 | 187 | 58 |
| turn-000008 | 36319 | 19200 | 17119 | 2061 | 995 |
| turn-000008 | 35725 | 19456 | 16269 | 801 | 230 |
| turn-000009 | 36701 | 18816 | 17885 | 988 | 155 |
| turn-000009 | 36656 | 18816 | 17840 | 233 | 26 |
| turn-000010 | 36708 | 18816 | 17892 | 171 | 46 |
| turn-000010 | 36654 | 18816 | 17838 | 819 | 293 |
| turn-000010 | 35631 | 19584 | 16047 | 601 | 45 |
| turn-000011 | 36255 | 18816 | 17439 | 3265 | 2346 |
| **Total** | **1,068,081** | **575,104** | **492,977** | **23,686** | **13,033** |
