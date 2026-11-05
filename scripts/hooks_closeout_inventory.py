#!/usr/bin/env python3
"""List the fixed Codex core and app-server test functions that exercise hooks.

Run from this repository with the adjacent Codex checkout, or pass its path:
    python3 scripts/hooks_closeout_inventory.py ../codex

Every Rust file under `codex-rs/core` and `codex-rs/app-server` at the fixed revision is
scanned; no source list is kept by hand. A test function is listed when its file is a hooks
suite (its file name contains `hook`), its name mentions user hooks, or its own body constructs
hook payloads, configures hooks, or observes hook events (see `MARKERS`). Git hook paths and
network MITM hooks are separate mechanisms, excluded from the name heuristic. `EXTRA` names the functions registered for another reason, each with
that reason. The hooks crate's own unit tests (`codex-rs/hooks`) are outside this inventory.

The output is a JSON inventory, not an automatic claim of behavioral equivalence: each row is
dispositioned by hand in the closeout table, where marker-only matches are marked incidental.
"""
import json
import re
import subprocess
import sys
from pathlib import Path

REVISION = "815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052"
ROOTS = ("codex-rs/core", "codex-rs/app-server")

# Hook configuration (TOML / JSON declarations, feature gate, trust, managed policy, the
# fixture writers) or hook events (event names, run notifications, hook prompts, transcript).
MARKERS = re.compile(
    r"\[\[?hooks[\.\]]|\"hooks\"\s*:|hooks\.json|Feature::CodexHooks|HooksConfig"
    r"|\bwrite_\w*hooks?\w*\s*\(|\binstall_\w*hooks?\w*\s*\("
    r"|HookToolName|HookEventName|hook_event_name|hookEventName|EventMsg::Hook\w+|HookRunStatus|HookPrompt"
    r"|hook_transcript_path|preview_\w*hooks|\bread_hook_\w+|wait_for_\w*hooks?\b|codex_hooks::"
    r"|HooksList|hooks/list|HookTrustStatus|trust_discovered_hooks|bypass_hook_trust"
    r"|allow_managed_hooks_only|\brefresh_hooks\b|session\.hooks\b"
    r"|(?:pre_tool_use|post_tool_use|permission_request)_payload|with_updated_hook_input"
    r"|hook_trusted_hash_edit|hook_payloads"
)

EXTRA = {
    ("core/src/session/config_refresh_tests.rs",
     "reload_user_config_layer_resolves_paths_without_resetting_runtime_settings"):
        "the session-static settings a hook refresh keeps; its body names no hook",
    ("core/src/session/tests.rs", "danger_full_access_tool_attempts_do_not_enforce_managed_network"):
        "registered earlier as incidental scope evidence; hooks appear only in shared fixtures",
}

TEST_ATTRIBUTE = re.compile(
    r"#\[(?:tokio::test|test\b|rstest\b|test_case(?:::(?:test_case|test_matrix))?\b)"
)
FUNCTION = re.compile(r"\b(?:async\s+)?fn\s+(\w+)\s*\(")
RAW_STRING = re.compile(r'r(#*)"')
CHAR = re.compile(r"'(?:\\.[^']*|[^'\\])'")


def body(source, start):
    """The braced body of the function whose signature starts at `start`.

    Braces inside comments, strings, raw strings and character literals are skipped.
    """
    position = source.index("{", start)
    depth = 0
    open_at = position
    end = len(source)
    while position < end:
        if source.startswith("//", position):
            newline = source.find("\n", position)
            position = end if newline < 0 else newline
            continue
        if source.startswith("/*", position):
            position = source.index("*/", position) + 2
            continue
        raw = RAW_STRING.match(source, position)
        if raw and (position == 0 or not (source[position - 1].isalnum() or source[position - 1] == "_")):
            closing = '"' + raw[1]
            position = source.index(closing, raw.end()) + len(closing)
            continue
        character = source[position]
        if character == '"':
            position += 1
            while source[position] != '"':
                position += 2 if source[position] == "\\" else 1
            position += 1
            continue
        if character == "'":
            literal = CHAR.match(source, position)
            if literal:
                position = literal.end()
                continue
        if character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
            if depth == 0:
                return source[open_at:position + 1]
        position += 1
    raise ValueError("unbalanced braces")


def inventory(checkout):
    files = subprocess.check_output(
        ["git", "-C", str(checkout), "ls-tree", "-r", "--name-only", REVISION, *ROOTS], text=True
    ).split()
    rows = []
    extra = dict(EXTRA)
    for file in sorted(name for name in files if name.endswith(".rs")):
        path = file[len("codex-rs/"):]
        source = subprocess.check_output(
            ["git", "-C", str(checkout), "show", f"{REVISION}:{file}"], text=True
        )
        hooks_suite = "hook" in Path(path).name
        seen = set()
        for attribute in TEST_ATTRIBUTE.finditer(source):
            function = FUNCTION.search(source, attribute.end())
            if function is None:
                raise ValueError(f"test attribute without a function in {path}")
            name = function[1]
            if name in seen:
                continue
            seen.add(name)
            text = body(source, function.end())
            reason = None
            if hooks_suite:
                reason = "hooks suite"
            elif "hook" in name and not re.search(r"mitm|configured_hooks_path", name):
                reason = "hook behavior named by test"
            elif MARKERS.search(text):
                reason = "markers: " + ", ".join(sorted({m[0] for m in MARKERS.finditer(text)}))
            elif (path, name) in extra:
                reason = "extra: " + extra[(path, name)]
            extra.pop((path, name), None)
            if reason is None:
                continue
            rows.append({
                "path": path, "name": name,
                "line": source[:function.start()].count("\n") + 1,
                "attributes": source[attribute.start():function.start()].strip(),
                "reason": reason,
            })
    if extra:
        raise ValueError(f"extra functions missing: {sorted(extra)}")
    return rows


if __name__ == "__main__":
    checkout = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[2] / "codex"
    print(json.dumps(inventory(checkout), ensure_ascii=False, indent=2))
