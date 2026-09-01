#!/usr/bin/env python3
"""Generate wrapper agent files from base skill + schema diff.

Inputs:
  --base-skill <path>
  --base-agent-id <id>
  --wrapper-id <id>
  --target-spec-json <json-string>
  --schema-diff-json <json-string>
  [--base-manifest-json <json-string>]
  [--base-schema-json <json-string>]    base accepts/returns schemas (enables
                                        round-trip validation; omitted =
                                        skipped, verdict under-claims)
  [--base-revision-digest <digest>]
  [--wrapper-mode reasoning|script]     default: reasoning
  [--base-script-path <path>]           script mode: base entry file to copy
  [--fail-soft]                         opt out of fail-loud mapping hooks
  [--output-dir <path>]

Output (stdout JSON):
  {
    "wrapper_id": ...,
    "wrapper_mode": "reasoning" | "script",
    "requires_input_mapping": bool,
    "requires_output_mapping": bool,
    "verdict": "ok" | "partial" | "clarification_needed",
    "validation_failures": ["..."],
    "notes": ["..."],
    "files": [...]
  }

Round-trip validation (#1234): every generated mapping hook is executed
against a synthetic payload built from the declaring schema, and its output is
validated against the other side's schema. The verdict is mechanically derived
from those results — the adapter agent must surface it as its own status
instead of claiming `ok` from LLM judgment. Verdicts:

  ok                  every emitted mapper was proven on a synthetic payload
  partial             mapper emitted but unproven (validation failed, or was
                      skipped because a schema was unavailable); failing paths
                      are named in notes/validation_failures
  clarification_needed no trustworthy mapper exists (schema missing on one
                      side, nothing derivable, or every inferred rename was
                      type-invalid) — no mapper is emitted for that direction

Generated mapping hooks are fail-loud: a payload that violates the declared
contract (unparseable, non-object, missing a required mapped field) makes the
hook exit non-zero so the turn fails closed instead of letting the LLM
improvise on untransformed data. Optional fields may be absent.

Script mode (#1251): `--wrapper-mode script` emits a deterministic wrapper —
an entry shim that runs an in-bundle copy of the base agent's script entry,
with verbatim stdin->stdout mapping hooks at the payload boundary. The copy is
pinned by `adapter.base_revision_digest`, so the existing drift machinery
(roster `stale_base`, promotion-time drift events) applies unchanged, and the
wrapper's declared capabilities (inherited from the base manifest) govern what
the copied entry may do — no privilege is smuggled through composition.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

VERDICT_OK = "ok"
VERDICT_PARTIAL = "partial"
VERDICT_CLARIFICATION = "clarification_needed"
_VERDICT_ORDER = {VERDICT_OK: 0, VERDICT_PARTIAL: 1, VERDICT_CLARIFICATION: 2}

MAX_SYNTH_DEPTH = 6


def worst_verdict(verdicts: List[Optional[str]]) -> str:
    worst = VERDICT_OK
    for verdict in verdicts:
        if verdict is None:
            continue
        if _VERDICT_ORDER.get(verdict, _VERDICT_ORDER[VERDICT_CLARIFICATION]) > _VERDICT_ORDER[worst]:
            worst = verdict
    return worst


# ---------------------------------------------------------------------------
# Minimal JSON-Schema subset handling (stdlib only — the adapter sandbox does
# not ship third-party validators).
# ---------------------------------------------------------------------------

def required_fields(schema: Optional[Dict[str, Any]]) -> List[str]:
    if not isinstance(schema, dict):
        return []
    req = schema.get("required")
    if isinstance(req, list):
        return [str(x) for x in req]
    return []


def primary_type(schema: Optional[Dict[str, Any]]) -> Optional[str]:
    if not isinstance(schema, dict):
        return None
    t = schema.get("type")
    if isinstance(t, str):
        return t
    if isinstance(t, list):
        non_null = [x for x in t if x != "null"]
        if non_null:
            return str(non_null[0])
    return None


def property_type(schema: Optional[Dict[str, Any]], field: str) -> Optional[str]:
    if not isinstance(schema, dict):
        return None
    props = schema.get("properties")
    if not isinstance(props, dict):
        return None
    return primary_type(props.get(field))


def schema_properties(schema: Optional[Dict[str, Any]]) -> Dict[str, Any]:
    if isinstance(schema, dict) and isinstance(schema.get("properties"), dict):
        return schema["properties"]
    return {}


def synthetic_instance(schema: Optional[Dict[str, Any]], prefer: str = "", depth: int = 0) -> Any:
    """Build a minimal, deterministic, type-respecting instance of `schema`.

    Required fields only; strings take the field name so mapped values are
    traceable in failure output. Unknown or union types degrade to `None`
    (the validator skips uncheckable fields).
    """
    if not isinstance(schema, dict) or depth > MAX_SYNTH_DEPTH:
        return None
    if isinstance(schema.get("const"), (str, int, float, bool)):
        return schema["const"]
    enum = schema.get("enum")
    if isinstance(enum, list) and enum:
        return enum[0]
    t = primary_type(schema)
    if t == "object":
        props = schema_properties(schema)
        return {
            name: synthetic_instance(props.get(name), prefer=name, depth=depth + 1)
            for name in required_fields(schema)
        }
    if t == "array":
        items = schema.get("items")
        if isinstance(items, dict):
            element = synthetic_instance(items, prefer=prefer, depth=depth + 1)
            return [] if element is None else [element]
        return []
    if t == "string":
        return prefer or "synthetic"
    if t == "integer" or t == "number":
        return 0
    if t == "boolean":
        return False
    return None


def validate_subset(
    instance: Any,
    schema: Optional[Dict[str, Any]],
    path: str,
    failures: List[str],
    depth: int = 0,
) -> None:
    """Validate `instance` against the JSON-Schema subset the adapter reasons
    over (required fields, primitive types, enums, one array/object level).
    Keywords outside the subset are ignored — this proves the *mapping*, not
    the whole contract."""
    if not isinstance(schema, dict) or depth > MAX_SYNTH_DEPTH:
        return
    enum = schema.get("enum")
    if isinstance(enum, list) and enum and instance not in enum:
        failures.append(f"{path}: value {instance!r} not in enum {enum!r}")
        return
    t = primary_type(schema)
    if t is None:
        return
    if t == "object":
        if not isinstance(instance, dict):
            failures.append(f"{path}: expected object, got {type(instance).__name__}")
            return
        props = schema_properties(schema)
        for name in required_fields(schema):
            if name not in instance:
                failures.append(f"{path}: missing required field '{name}'")
            else:
                validate_subset(
                    instance[name], props.get(name), f"{path}.{name}", failures, depth + 1
                )
        return
    if t == "array":
        if not isinstance(instance, list):
            failures.append(f"{path}: expected array, got {type(instance).__name__}")
            return
        items = schema.get("items")
        if isinstance(items, dict):
            for index, element in enumerate(instance):
                validate_subset(element, items, f"{path}[{index}]", failures, depth + 1)
        return
    if t == "string":
        if not isinstance(instance, str):
            failures.append(f"{path}: expected string, got {type(instance).__name__}")
    elif t == "integer":
        if not isinstance(instance, int) or isinstance(instance, bool):
            failures.append(f"{path}: expected integer, got {type(instance).__name__}")
    elif t == "number":
        if isinstance(instance, bool) or not isinstance(instance, (int, float)):
            failures.append(f"{path}: expected number, got {type(instance).__name__}")
    elif t == "boolean":
        if not isinstance(instance, bool):
            failures.append(f"{path}: expected boolean, got {type(instance).__name__}")
    elif t == "null":
        if instance is not None:
            failures.append(f"{path}: expected null, got {type(instance).__name__}")


# ---------------------------------------------------------------------------
# Generated hook scripts. Two contracts:
#   envelope — LLM completion request/response (reasoning-mode middleware)
#   verbatim — raw payload on stdin, mapped payload on stdout (script mode)
# Fail-loud is the default (#1234): structural mismatch exits non-zero so the
# turn fails closed instead of feeding untransformed data to the next stage.
# ---------------------------------------------------------------------------

LOUD_DOC = (
    "Fail-loud: any structural mismatch (unparseable input, non-object payload,\n"
    "missing required mapped field) exits non-zero — the turn fails closed\n"
    "instead of passing untransformed data through. Optional fields may be absent."
)
SOFT_DOC = (
    "Fail-soft: on any structural mismatch the input passes through unchanged."
)

_PRE_VERBATIM_LOUD = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (pre_map).

Contract: verbatim stdin -> stdout at the script-mode payload boundary.
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r
REQUIRED_SOURCES = %(required)r


def fail(message):
    sys.stderr.write("pre_map: " + message + "\\n")
    sys.exit(1)


def main():
    try:
        payload = json.loads(sys.stdin.read())
    except ValueError as error:
        fail("input is not valid JSON: " + str(error))
    if not isinstance(payload, dict):
        fail("input payload is not a JSON object")
    missing = [name for name in REQUIRED_SOURCES if name not in payload]
    if missing:
        fail("payload is missing required field(s): " + ", ".join(missing))
    for src, dst in MAPPINGS:
        if src in payload and dst not in payload:
            payload[dst] = payload.pop(src)
    json.dump(payload, sys.stdout)


main()
'''

_PRE_ENVELOPE_LOUD = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (pre_map).

Contract: LLM completion request envelope (reasoning-mode middleware); the
last message's JSON content is mapped in place.
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r
REQUIRED_SOURCES = %(required)r


def fail(message):
    sys.stderr.write("pre_map: " + message + "\\n")
    sys.exit(1)


def main():
    try:
        envelope = json.loads(sys.stdin.read())
    except ValueError as error:
        fail("request envelope is not valid JSON: " + str(error))
    messages = envelope.get("messages") if isinstance(envelope, dict) else None
    if not isinstance(messages, list) or not messages:
        fail("request envelope carries no messages")
    last = messages[-1]
    if not isinstance(last, dict):
        fail("last message is not an object")
    content = last.get("content")
    try:
        payload = json.loads(content) if isinstance(content, str) else content
    except ValueError as error:
        fail("last message content is not valid JSON: " + str(error))
    if not isinstance(payload, dict):
        fail("last message content is not a JSON object")
    missing = [name for name in REQUIRED_SOURCES if name not in payload]
    if missing:
        fail("payload is missing required field(s): " + ", ".join(missing))
    for src, dst in MAPPINGS:
        if src in payload and dst not in payload:
            payload[dst] = payload.pop(src)
    last["content"] = json.dumps(payload)
    json.dump(envelope, sys.stdout)


main()
'''

_POST_VERBATIM_LOUD = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (post_map).

Contract: verbatim stdin -> stdout at the script-mode payload boundary.
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r
REQUIRED_SOURCES = %(required)r


def fail(message):
    sys.stderr.write("post_map: " + message + "\\n")
    sys.exit(1)


def main():
    try:
        payload = json.loads(sys.stdin.read())
    except ValueError as error:
        fail("input is not valid JSON: " + str(error))
    if not isinstance(payload, dict):
        fail("input payload is not a JSON object")
    missing = [name for name in REQUIRED_SOURCES if name not in payload]
    if missing:
        fail("payload is missing required field(s): " + ", ".join(missing))
    for src, dst in MAPPINGS:
        if src in payload and dst not in payload:
            payload[dst] = payload.pop(src)
    json.dump(payload, sys.stdout)


main()
'''

_POST_ENVELOPE_LOUD = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (post_map).

Contract: LLM completion response envelope (reasoning-mode middleware); the
JSON `text` field is mapped in place.
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r
REQUIRED_SOURCES = %(required)r


def fail(message):
    sys.stderr.write("post_map: " + message + "\\n")
    sys.exit(1)


def main():
    try:
        envelope = json.loads(sys.stdin.read())
    except ValueError as error:
        fail("response envelope is not valid JSON: " + str(error))
    if not isinstance(envelope, dict):
        fail("response envelope is not a JSON object")
    text = envelope.get("text")
    try:
        payload = json.loads(text) if isinstance(text, str) else text
    except ValueError as error:
        fail("response text is not valid JSON: " + str(error))
    if not isinstance(payload, dict):
        fail("response text is not a JSON object")
    missing = [name for name in REQUIRED_SOURCES if name not in payload]
    if missing:
        fail("payload is missing required field(s): " + ", ".join(missing))
    for src, dst in MAPPINGS:
        if src in payload and dst not in payload:
            payload[dst] = payload.pop(src)
    envelope["text"] = json.dumps(payload)
    json.dump(envelope, sys.stdout)


main()
'''

# Fail-soft fallbacks (--fail-soft): legacy envelope behavior plus the
# verbatim-contract equivalent. A soft hook never changes the payload it
# cannot map.
_PRE_VERBATIM_SOFT = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (pre_map).

Contract: verbatim stdin -> stdout at the script-mode payload boundary.
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r

payload = json.load(sys.stdin)
try:
    if isinstance(payload, dict):
        for src, dst in MAPPINGS:
            if src in payload and dst not in payload:
                payload[dst] = payload.pop(src)
except Exception:
    pass
print(json.dumps(payload))
'''

_PRE_ENVELOPE_SOFT = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (pre_map).

Contract: LLM completion request envelope (reasoning-mode middleware).
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r

request = json.load(sys.stdin)
try:
    if request.get("messages"):
        content_obj = json.loads(request["messages"][-1]["content"])
        if isinstance(content_obj, dict):
            for src, dst in MAPPINGS:
                if src in content_obj and dst not in content_obj:
                    content_obj[dst] = content_obj.pop(src)
        request["messages"][-1]["content"] = json.dumps(content_obj)
except Exception:
    pass
print(json.dumps(request))
'''

_POST_VERBATIM_SOFT = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (post_map).

Contract: verbatim stdin -> stdout at the script-mode payload boundary.
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r

payload = json.load(sys.stdin)
try:
    if isinstance(payload, dict):
        for src, dst in MAPPINGS:
            if src in payload and dst not in payload:
                payload[dst] = payload.pop(src)
except Exception:
    pass
print(json.dumps(payload))
'''

_POST_ENVELOPE_SOFT = '''#!/usr/bin/env python3
"""Generated by agent-adapter.default (post_map).

Contract: LLM completion response envelope (reasoning-mode middleware).
%(loudness_doc)s

Generated notes:
%(notes_block)s
"""
import json
import sys

MAPPINGS = %(mappings)r

response = json.load(sys.stdin)
try:
    text_obj = json.loads(response.get("text", ""))
    if isinstance(text_obj, dict):
        for src, dst in MAPPINGS:
            if src in text_obj and dst not in text_obj:
                text_obj[dst] = text_obj.pop(src)
    response["text"] = json.dumps(text_obj)
except Exception:
    pass
print(json.dumps(response))
'''

_HOOK_TEMPLATES = {
    ("pre", "verbatim", True): _PRE_VERBATIM_LOUD,
    ("pre", "envelope", True): _PRE_ENVELOPE_LOUD,
    ("post", "verbatim", True): _POST_VERBATIM_LOUD,
    ("post", "envelope", True): _POST_ENVELOPE_LOUD,
    ("pre", "verbatim", False): _PRE_VERBATIM_SOFT,
    ("pre", "envelope", False): _PRE_ENVELOPE_SOFT,
    ("post", "verbatim", False): _POST_VERBATIM_SOFT,
    ("post", "envelope", False): _POST_ENVELOPE_SOFT,
}


def render_hook(
    role: str,
    contract: str,
    pairs: List[Tuple[str, str]],
    required_sources: List[str],
    fail_loud: bool,
    notes: List[str],
) -> str:
    template = _HOOK_TEMPLATES[(role, contract, fail_loud)]
    notes_block = "\n".join(f"# - {note}" for note in notes[:8]) or "# - (none)"
    return template % {
        "loudness_doc": LOUD_DOC if fail_loud else SOFT_DOC,
        "notes_block": notes_block,
        "mappings": pairs,
        "required": list(required_sources),
    }


ENTRY_SHIM = '''#!/usr/bin/env python3
"""Generated entry shim (agent-adapter.default, script-mode wrapper #1251).

Runs the in-bundle copy of the base agent's entry script in-process so
stdin/stdout/environment and argv pass through verbatim — the wrapper never
pays for a completion. The copy lives at scripts/base_entry.py inside this
bundle; drift against the base's promoted revision is detected through the
`adapter:` provenance digest, not by re-reading the base at runtime. This
shim is marked executable by the gateway at install time.
"""
import os
import runpy
import sys

_BASE_ENTRY = os.path.join(os.path.dirname(os.path.abspath(__file__)), "base_entry.py")
sys.argv = [_BASE_ENTRY] + sys.argv[1:]
runpy.run_path(_BASE_ENTRY, run_name="__main__")
'''


# ---------------------------------------------------------------------------
# Round-trip validation: execute the *emitted* hook against a synthetic
# payload and validate its output against the other side's schema.
# ---------------------------------------------------------------------------

def envelope_pre_wrap(payload: Any) -> str:
    return json.dumps({"messages": [{"role": "user", "content": json.dumps(payload)}]})


def envelope_pre_unwrap(out: Any) -> Any:
    return json.loads(out["messages"][-1]["content"])


def envelope_post_wrap(payload: Any) -> str:
    return json.dumps({"text": json.dumps(payload)})


def envelope_post_unwrap(out: Any) -> Any:
    return json.loads(out["text"])


def round_trip(
    hook_path: Path,
    synthetic: Any,
    wrap,
    unwrap,
) -> Tuple[Optional[Any], Optional[str]]:
    stdin_text = wrap(synthetic) if wrap is not None else json.dumps(synthetic)
    try:
        proc = subprocess.run(
            [sys.executable, str(hook_path)],
            input=stdin_text,
            capture_output=True,
            text=True,
            timeout=60,
        )
    except subprocess.TimeoutExpired:
        return None, "hook did not terminate within 60s"
    if proc.returncode != 0:
        return None, f"hook exited {proc.returncode}: {proc.stderr.strip()[:400]}"
    try:
        out = json.loads(proc.stdout)
    except ValueError as error:
        return None, f"hook stdout is not valid JSON: {error}"
    if unwrap is not None:
        try:
            out = unwrap(out)
        except (ValueError, KeyError, IndexError, TypeError) as error:
            return None, f"cannot read mapped payload from hook output: {error}"
    return out, None


def process_direction(
    label: str,
    mapping_required: bool,
    mappings_raw: Any,
    source_schema: Optional[Dict[str, Any]],
    dest_schema: Optional[Dict[str, Any]],
    role: str,
    contract: str,
    fail_loud: bool,
    notes: List[str],
    failures: List[str],
) -> Tuple[Optional[str], Optional[str]]:
    """Produce (or refuse to produce) the hook for one direction and return
    `(hook_source, verdict)`. `source_schema` is the schema the caller speaks
    (target accepts / base returns); `dest_schema` the schema on the other
    side."""
    if not mapping_required:
        return None, None

    pairs: List[Tuple[str, str]] = []
    for item in mappings_raw or []:
        if not isinstance(item, dict):
            continue
        src, dst = item.get("from"), item.get("to")
        if isinstance(src, str) and isinstance(dst, str):
            pairs.append((src, dst))
    if not pairs:
        notes.append(
            f"{label}: mapping required but no deterministic mapping is derivable — "
            "no mapper generated (a passthrough fork must not masquerade as an adapter)"
        )
        return None, VERDICT_CLARIFICATION

    # Type-guard: a rename across two declared types is confidently wrong —
    # drop it rather than emit a mapper that cannot work.
    kept: List[Tuple[str, str]] = []
    for src, dst in pairs:
        src_t = property_type(source_schema, src)
        dst_t = property_type(dest_schema, dst)
        if src_t is not None and dst_t is not None and src_t != dst_t:
            notes.append(
                f"{label}: dropped rename {src}->{dst}: declared types differ "
                f"({src_t} vs {dst_t})"
            )
        else:
            kept.append((src, dst))
    if not kept:
        notes.append(
            f"{label}: every inferred rename was type-invalid — no mapper generated"
        )
        return None, VERDICT_CLARIFICATION

    if role == "pre":
        # pre maps caller->base; the caller must supply every mapped source.
        hook_pairs = kept
        synthetic_schema = source_schema
        validate_against = dest_schema
    else:
        # post maps base->caller (the reverse of the diff's target->base
        # pairs); the base output must supply every mapped field.
        hook_pairs = [(dst, src) for src, dst in kept]
        synthetic_schema = dest_schema
        validate_against = source_schema
    required_sources = [src for src, _ in hook_pairs]

    # Optional fields the flat renames cannot cover stay unmapped — name them.
    props = schema_properties(source_schema)
    source_required = set(required_fields(source_schema))
    mapped_sources = {src for src, _ in kept}
    uncovered = sorted(
        name for name in props if name not in source_required and name not in mapped_sources
    )
    if uncovered:
        notes.append(
            f"{label}: optional field(s) {uncovered} pass through unmapped"
        )

    hook_source = render_hook(role, contract, hook_pairs, required_sources, fail_loud, notes)

    if synthetic_schema is None or validate_against is None:
        notes.append(
            f"{label}: round-trip validation skipped — schema unavailable on one side "
            "(under-claim; pass --base-schema-json to prove the mapping)"
        )
        return hook_source, VERDICT_PARTIAL

    synthetic = synthetic_instance(synthetic_schema)
    if contract == "envelope":
        wrap, unwrap = (
            (envelope_pre_wrap, envelope_pre_unwrap)
            if role == "pre"
            else (envelope_post_wrap, envelope_post_unwrap)
        )
    else:
        wrap = unwrap = None

    with tempfile.TemporaryDirectory() as tmp:
        hook_path = Path(tmp) / f"{role}_map.py"
        hook_path.write_text(hook_source, encoding="utf-8")
        mapped, error = round_trip(hook_path, synthetic, wrap, unwrap)

    if error is not None:
        failures.append(f"{label} round-trip: {error}")
        notes.append(f"{label}: round-trip execution failed — {error}")
        return hook_source, VERDICT_PARTIAL

    direction_failures: List[str] = []
    validate_subset(mapped, validate_against, label, direction_failures)
    if direction_failures:
        failures.extend(direction_failures)
        notes.append(
            f"{label}: round-trip validation failed — "
            f"{len(direction_failures)} schema violation(s) (see validation_failures)"
        )
        return hook_source, VERDICT_PARTIAL

    notes.append(
        f"{label}: round-trip validation passed ({len(kept)} rename(s) proven on a synthetic payload)"
    )
    return hook_source, VERDICT_OK


# ---------------------------------------------------------------------------
# Wrapper manifest rendering.
# ---------------------------------------------------------------------------

def render_inference_block(base_manifest: Dict[str, Any]) -> str:
    """Inference settings for the wrapper, inherited from the base agent.

    A wrapper is a transformation layer around a base specialist, so it must
    reason with the *base's* model — never a model hardcoded here. An explicit
    `llm_config` on the base is copied (with temperature pinned to 0.0, since
    the wrapper only maps I/O); otherwise the base's `llm_preset` is reused so
    the gateway resolves provider/model from its own config. With neither, fall
    back to the `agentic` preset rather than naming a provider.

    Script-mode wrappers never reason — `main()` passes no inference block.  """
    overrides = '    llm_overrides:\n      temperature: 0.0\n'

    base_config = base_manifest.get("llm_config")
    if isinstance(base_config, dict) and (
        base_config.get("provider") or base_config.get("model")
    ):
        inherited = {k: v for k, v in base_config.items() if k != "temperature"}
        inherited["temperature"] = 0.0
        config_json = json.dumps(inherited, indent=2)
        config_json = "\n".join("      " + line for line in config_json.splitlines())
        return f"    llm_config:\n{config_json}\n"

    preset = base_manifest.get("llm_preset")
    if not isinstance(preset, str) or not preset.strip():
        preset = "agentic"
    return f'    llm_preset: "{preset}"\n{overrides}'


def render_execution_block(base_manifest: Dict[str, Any]) -> str:
    """Script-mode plumbing: the entry shim plus, when the base declares one,
    its input delivery mode — the shim forwards argv, so the base script sees
    the payload exactly as it would running natively."""
    lines = '    execution_mode: "script"\n'
    lines += '    script_entry: "scripts/entry.py"\n'
    mode = base_manifest.get("script_input_mode")
    if isinstance(mode, str) and mode in ("stdin", "args"):
        lines += f'    script_input_mode: "{mode}"\n'
    return lines


def render_skill(
    wrapper_id: str,
    base_agent_id: str,
    target_spec: Dict[str, Any],
    with_pre: bool,
    with_post: bool,
    base_skill: str,
    base_capabilities: Any | None,
    schema_notes: List[str],
    inference_block: str,
    execution_block: str,
    base_revision_digest: str | None,
) -> str:
    middleware_lines = ""
    if with_pre or with_post:
        middleware_lines = "    middleware:\n"
        if with_pre:
            middleware_lines += '      pre_process: "python3 scripts/pre_map.py"\n'
        if with_post:
            middleware_lines += '      post_process: "python3 scripts/post_map.py"\n'

    io_block = json.dumps(target_spec, indent=2)
    io_block = "\n".join("      " + line for line in io_block.splitlines())
    capabilities_block = ""
    if base_capabilities is not None:
        caps_json = json.dumps(base_capabilities, indent=2)
        caps_json = "\n".join("      " + line for line in caps_json.splitlines())
        capabilities_block = f"    capabilities:\n{caps_json}\n"

    generated_at = datetime.now(timezone.utc).isoformat()
    notes_block = json.dumps(schema_notes[:5])

    # Composition provenance (proposal docs/proposals/agent-adaptation-composition.md):
    # the parser used to drop this block silently — it is now a first-class
    # `AgentManifest.adapter` field. `generator` and `base_revision_digest`
    # let roster tooling attribute the wrapper and detect base-revision drift;
    # an unknown digest stays absent (under-claim, never guess).
    adapter_lines = f"    adapter:\n      base_agent_id: \"{base_agent_id}\"\n"
    if base_revision_digest:
        adapter_lines += f"      base_revision_digest: \"{base_revision_digest}\"\n"
    adapter_lines += f"      generated_at: \"{generated_at}\"\n"
    adapter_lines += f"      schema_notes: {notes_block}\n"
    adapter_lines += "      generator: \"agent-adapter.default\"\n"

    if execution_block:
        mode_paragraph = (
            "This wrapper runs in script mode: its entry shim executes a pinned\n"
            "copy of the base agent's script with generated mapping hooks at the\n"
            "payload boundary — deterministic I/O adaptation with no completion.\n"
        )
    else:
        mode_paragraph = (
            "Wrapper agents use deterministic LLM settings (temperature: 0.0) because they are\n"
            "transformation layers. The base agent provides the reasoning; the wrapper only\n"
            "maps I/O schemas via middleware scripts.\n"
        )

    return f"""---
name: "{wrapper_id}"
description: "Wrapper generated by agent-adapter.default"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "{wrapper_id}"
      name: "{wrapper_id}"
      description: "Wrapper generated by agent-adapter.default"
{capabilities_block}{execution_block}{adapter_lines}{inference_block}    io:
{io_block}
{middleware_lines}---
# {wrapper_id}

This wrapper adapts I/O around a base specialist.

## Adapter Metadata

- **Base Agent:** `{base_agent_id}`
- **Generated:** {generated_at}
- **Schema Notes:** {notes_block}

{mode_paragraph}
## Base Skill Reference (excerpt)

```text
{base_skill[:2000]}
```
"""


# ---------------------------------------------------------------------------
# Script-mode prerequisites: refuse loudly rather than emit a wrapper that
# cannot install or run.
# ---------------------------------------------------------------------------

def script_mode_prerequisite_error(
    base_script_path: Optional[str],
    target_spec: Dict[str, Any],
) -> Optional[str]:
    if not base_script_path:
        return (
            "--wrapper-mode script requires --base-script-path "
            "(the base agent's installed script_entry file)"
        )
    path = Path(base_script_path)
    if not path.is_file():
        return f"--base-script-path does not exist or is not a file: {base_script_path}"
    if path.suffix != ".py":
        return (
            f"only Python base entries can be wrapped by the generated shim "
            f"(got {path.name!r}); refuse or hand-build the wrapper"
        )
    try:
        source = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        return f"--base-script-path is not readable UTF-8 source: {error}"
    try:
        compile(source, str(path), "exec")
    except SyntaxError as error:
        return f"base entry script does not compile: {error}"

    accepts = target_spec.get("accepts") if isinstance(target_spec, dict) else None
    if not isinstance(accepts, dict) or primary_type(accepts) != "object":
        return (
            "script-mode wrappers require an object io.accepts in the target spec — "
            "script agents without an input contract are rejected at install"
        )
    return None


# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-skill", required=True)
    parser.add_argument("--base-agent-id", required=True)
    parser.add_argument("--wrapper-id", required=True)
    parser.add_argument("--target-spec-json", required=True)
    parser.add_argument("--schema-diff-json", required=True)
    parser.add_argument("--base-manifest-json")
    parser.add_argument(
        "--base-schema-json",
        help='Base I/O schemas {"accepts": ...|null, "returns": ...|null}. '
        "Without it, round-trip validation is skipped and the verdict "
        "under-claims to partial.",
    )
    parser.add_argument(
        "--base-revision-digest",
        help="Promoted revision digest of the base at generation time "
        "(e.g. rev_sha256:...). Omitted = unknown — provenance under-claims.",
    )
    parser.add_argument(
        "--wrapper-mode",
        choices=["reasoning", "script"],
        default="reasoning",
        help="reasoning: LLM wrapper with envelope middleware (default). "
        "script: deterministic wrapper around a copy of the base's script "
        "entry (#1251) — no completion is ever paid for.",
    )
    parser.add_argument(
        "--base-script-path",
        help="script mode: path to the base agent's installed script_entry "
        "file; it is copied verbatim into the wrapper bundle.",
    )
    parser.add_argument(
        "--fail-soft",
        action="store_true",
        help="Emit legacy fail-soft mapping hooks (structural mismatch passes "
        "through) instead of the default fail-loud ones. Discouraged.",
    )
    parser.add_argument("--output-dir")
    args = parser.parse_args()

    base_skill = Path(args.base_skill).read_text(encoding="utf-8")
    target_spec = json.loads(args.target_spec_json)
    schema_diff = json.loads(args.schema_diff_json)
    base_manifest = json.loads(args.base_manifest_json) if args.base_manifest_json else {}
    base_schemas = json.loads(args.base_schema_json) if args.base_schema_json else {}
    base_capabilities = base_manifest.get("capabilities")
    fail_loud = not args.fail_soft
    wrapper_mode = args.wrapper_mode

    if wrapper_mode == "script":
        error = script_mode_prerequisite_error(args.base_script_path, target_spec)
        if error is not None:
            print(
                json.dumps(
                    {
                        "wrapper_id": args.wrapper_id,
                        "wrapper_mode": wrapper_mode,
                        "verdict": VERDICT_CLARIFICATION,
                        "error": error,
                        "files": [],
                    }
                )
            )
            print(f"generate_wrapper: refusing script-mode generation: {error}", file=sys.stderr)
            return 2

    contract = "verbatim" if wrapper_mode == "script" else "envelope"

    pre_needed = bool(schema_diff.get("requires_input_mapping"))
    post_needed = bool(schema_diff.get("requires_output_mapping"))
    diff_notes = [str(n) for n in schema_diff.get("notes", [])]
    input_mappings = schema_diff.get("input_mappings") or []
    output_mappings = schema_diff.get("output_mappings") or []
    base_accepts = base_schemas.get("accepts") if isinstance(base_schemas, dict) else None
    base_returns = base_schemas.get("returns") if isinstance(base_schemas, dict) else None

    notes = list(diff_notes)
    failures: List[str] = []

    # pre hook: caller (target accepts) -> base (base accepts).
    pre_source, pre_verdict = process_direction(
        "accepts",
        pre_needed,
        input_mappings,
        target_spec.get("accepts"),
        base_accepts,
        "pre",
        contract,
        fail_loud,
        notes,
        failures,
    )
    # post hook: base (base returns) -> caller (target returns).
    post_source, post_verdict = process_direction(
        "returns",
        post_needed,
        output_mappings,
        target_spec.get("returns"),
        base_returns,
        "post",
        contract,
        fail_loud,
        notes,
        failures,
    )
    verdict = worst_verdict([pre_verdict, post_verdict])

    with_pre = pre_source is not None
    with_post = post_source is not None

    files: Dict[str, str] = {}
    execution_block = ""
    if wrapper_mode == "script":
        execution_block = render_execution_block(base_manifest)
        base_entry_source = Path(args.base_script_path).read_text(encoding="utf-8")
        files["scripts/base_entry.py"] = base_entry_source
        files["scripts/entry.py"] = ENTRY_SHIM
    files["SKILL.md"] = render_skill(
        args.wrapper_id,
        args.base_agent_id,
        target_spec,
        with_pre=with_pre,
        with_post=with_post,
        base_skill=base_skill,
        base_capabilities=base_capabilities,
        schema_notes=notes,
        inference_block=(
            render_inference_block(base_manifest) if wrapper_mode == "reasoning" else ""
        ),
        execution_block=execution_block,
        base_revision_digest=args.base_revision_digest,
    )
    files["runtime.lock"] = (
        "# Generated runtime.lock - sha256 is computed on first gateway load.\n"
        "gateway:\n"
        '  artifact: "marketplace://gateway/autonoetic-gateway"\n'
        '  version: "0.1.0"\n'
        '  sha256: ""\n'
        "sdk:\n"
        '  version: "0.1.0"\n'
        "sandbox:\n"
        '  backend: "bubblewrap"\n'
        "dependencies: []\n"
        "artifacts: []\n"
    )
    if with_pre:
        files["scripts/pre_map.py"] = pre_source
    if with_post:
        files["scripts/post_map.py"] = post_source

    if args.output_dir:
        out_dir = Path(args.output_dir)
        for rel_path, content in files.items():
            target = out_dir / rel_path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")

    print(
        json.dumps(
            {
                "wrapper_id": args.wrapper_id,
                "wrapper_mode": wrapper_mode,
                "requires_input_mapping": pre_needed,
                "requires_output_mapping": post_needed,
                "verdict": verdict,
                "validation_failures": failures,
                "notes": notes,
                "files": list(files.keys()),
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
