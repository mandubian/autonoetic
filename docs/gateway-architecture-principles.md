# Gateway Architecture Principles

## Core Design Tenet: Dumb Gateway, Smart Agent

The autonoetic gateway is designed as a **narrow rule enforcer** and **generic, neutral runtime executor**—not a business logic workflow engine. This distinction is critical for maintaining agent autonomy and platform safety.

### ✅ What the Gateway SHOULD Do

**Generic robustness improvements and mechanical safety guardrails**:
- **Enforce Hard Invariants**: Mechanically refuse operations that violate safety rules (e.g., rejecting high-risk deployments missing promotion records, or blocking agent installation without dependency resolution).
- **Rule Zero Enforcement**: Apply all rules equally. No agent, planner, or "trust me" flag can override a gateway safety invariant.
- **Analyze and Explain**: Scan code for patterns (imports, capabilities) and surface findings as structured data (`warnings[]`, `BundleHealthReport`) so the calling agent can act on them.
- **Tool-name canonicalization**: Map shorthand names to canonical forms.
- **Error typing and resilience**: Distinguish between recoverable and fatal errors.

These establish the safe floor of runtime robustness and capabilities without prescribing *how* agents navigate workflows to satisfy those boundaries.

### ❌ What the Gateway Should NOT Do

**Domain-specific business logic or workflow routing**:
- "Auto-spawn a builder agent when dependencies are missing" (Routing)
- "Decide if an agent's problem is worth fixing" (Workflow Decision)
- "Prevent research→builder transitions unless research returned data" (Business Logic)

These routing and workflow decisions hardcode assumptions about agent deployment. The gateway tells the agent what is wrong (e.g., missing dependencies). The agent's planner decides what to do about it.

### Where Business Logic Belongs

**In agent SKILL.md instructions** (not platform code):
- Guardrails 8 & 9 in planner.default tell the agent: "If research has no actionable data, stop and return failure instead of delegating"
- The `specialized_builder` or `planner` handles the gateway's structured error refusing deployment, decides it needs to generate a `builder.default` dependency resolution task, and executes that task before retrying.
- The agent *chooses* to follow these rules through LLM instruction-following, creating a **gate → explain → plan → execute → re-check** loop.

## Rationale

1. **Mechanical Safety against LLM Mistakes**: LLM decisions are advisory. Safety-critical invariants must be mechanically enforced by the gateway's deterministic guardrails.
2. **Agent autonomy**: Agents should make routing/delegation/auto-fix decisions, not the platform.
3. **Extensibility**: New agent types don't require platform code changes.
4. **Separation of concerns**: The Gateway analyzes, gates, and explains. Agents plan and execute via generated workflows.

## Historical Context

Session-1 failure showed that an agent could deploy a broken artifact (`import requests` with no `requirements.txt` installed) because the pipeline relied entirely on LLM judgement. The fix was **not** to have the gateway automatically invoke a builder logic (which violates the narrow rule enforcer principle), but to:
1. Hard-gate the promotion mechanically so missing dependencies trigger a refusal.
2. Send the structured explanation back and let the planner agent deploy the builder resolution step.
