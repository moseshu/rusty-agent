#!/usr/bin/env python3
"""Check source-backed tutorial snippets and execute every documented offline example."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import textwrap
import tomllib


ROOT = Path(__file__).resolve().parents[1]


def check_docs(cases):
    packages = {
        tomllib.loads(path.read_text())["package"]["name"]: path.parent
        for path in (ROOT / "examples").glob("*/Cargo.toml")
    }
    if packages.keys() != cases.keys():
        raise ValueError("Every example crate must appear in examples/checks.json")
    for package, variants in cases.items():
        readme = (packages[package] / "README.md").read_text()
        if not variants:
            raise ValueError(f"{package} needs an offline run")
        for args in variants:
            if package != "minimal_agent" and "--check" not in args:
                raise ValueError(f"{package}: offline checks must explicitly select --check")
            command = f"cargo run -p {package}" + (" -- " + " ".join(args) if args else "")
            if command not in readme.splitlines():
                raise ValueError(f"{package}: document this exact command: {command}")

    guide = (ROOT / "Docs/Framework_Quickstart.md").read_text()
    snippets = re.findall(
        r"<!-- source: ([^#\s]+)#([^\s]+) -->\n```rust\n(.*?)\n```", guide, re.DOTALL
    )
    if not snippets or len(snippets) != guide.count("<!-- source: ") or len(snippets) != guide.count("```rust\n"):
        raise ValueError("Every quickstart Rust block must have a valid checked source marker")
    for file, label, snippet in snippets:
        source = (ROOT / file).read_text()
        start = f"// tutorial:{label}:start\n"
        end = f"// tutorial:{label}:end"
        if source.count(start) != 1 or source.count(end) != 1:
            raise ValueError(f"{file}#{label}: missing or ambiguous source markers")
        expected = textwrap.dedent(source.split(start)[1].split(end)[0].rstrip())
        if snippet != expected:
            raise ValueError(f"{file}#{label}: quickstart excerpt differs from compiled source")
    print(f"Documentation checked: {len(packages)} crates, {len(snippets)} source excerpts.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docs-only", action="store_true")
    args = parser.parse_args()
    cases = json.loads((ROOT / "examples/checks.json").read_text())
    check_docs(cases)
    if args.docs_only:
        return
    environment = os.environ.copy()
    for name in ("OPENAI_API_KEY", "OPENAI_BASE_URL", "RA_MODEL", "RA_REQUEST"):
        environment.pop(name, None)
    count = 0
    for package, variants in cases.items():
        # Build each package on its own: a multi-package build unifies features (for example
        # `sessions` enables `rusty-agent/sqlite`), so `cargo run -p` would rebuild every other
        # example. Building separately also keeps compile time outside the run timeout.
        subprocess.run(["cargo", "build", "-p", package], cwd=ROOT, check=True)
        for flags in variants:
            command = ["cargo", "run", "--quiet", "-p", package, *(["--", *flags] if flags else [])]
            print("Running " + " ".join(command), flush=True)
            subprocess.run(command, cwd=ROOT, env=environment, check=True, timeout=120)
            count += 1
    print(f"Offline examples passed: {count} runs. Live endpoints were not exercised.")


if __name__ == "__main__":
    main()
