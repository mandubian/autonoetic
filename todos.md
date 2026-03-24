Tirith for command security

hermes => Memory and skills: injection / abuse checks
    tools/memory_tool.py — _scan_memory_content blocks memory entries that look like injection/exfil before they’re written (they’re injected into the system prompt later).

    agent/prompt_builder.py — Scans context files (e.g. AGENTS.md) for injection; can block or sanitize.

    tools/skills_guard.py — Heuristic checks on skill content (injection, exfil, destructive patterns, etc.).

    tools/skills_tool.py — Additional prompt-injection warnings for skill content.