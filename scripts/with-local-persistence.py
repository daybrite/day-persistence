#!/usr/bin/env python3
"""Run a consumer command against this checkout, restoring its Git declarations afterwards.

Usage: with-local-persistence.py ../Day-Showcase -- cargo test --workspace
Useful before the standalone workspace's first commit exists, when Cargo's Git patches alone
cannot resolve the imported historical tree. Do not edit consumer manifests concurrently.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def main():
    if len(sys.argv) < 4 or sys.argv[2] != "--":
        sys.exit(__doc__)
    app = Path(sys.argv[1]).resolve()
    if not (app / "Cargo.toml").is_file():
        sys.exit(f"No Cargo.toml in {app}")
    crates = Path(__file__).resolve().parents[1] / "crates"
    dependency = re.compile(
        r'(?m)^(day-(?:persistence(?:-macros)?|sqlite-worker))\s*=\s*\{([^}]+)\}'
    )
    source = re.compile(r'\bgit\s*=\s*"https://github.com/daybrite/day-persistence(?:\.git)?"')
    changes = []
    result = 1
    try:
        for directory, children, files in os.walk(app):
            children[:] = [d for d in children if d not in {
                ".git", "target", "build", ".cache", "node_modules", "vendor"
            }]
            if "Cargo.toml" not in files:
                continue
            path = Path(directory) / "Cargo.toml"
            original = path.read_text()

            def local(match):
                name, body = match.groups()
                if not source.search(body):
                    return match[0]
                body = source.sub(lambda _: "path = " + json.dumps(str(crates / name)), body)
                body = re.sub(r'\b(?:rev|branch|tag)\s*=\s*"[^"]*"\s*,?', "", body)
                return name + " = {" + body + "}"

            updated = dependency.sub(local, original)
            if updated != original:
                changes.append((path, original, updated))
                path.write_text(updated)
        if not changes:
            sys.exit("No standalone persistence Git dependency found; nothing was changed.")
        print(f"Using {crates} in {len(changes)} manifest(s); restoring after the command.", flush=True)
        result = subprocess.run(sys.argv[3:], cwd=app).returncode
    finally:
        for path, original, updated in changes:
            if path.read_text() == updated:
                path.write_text(original)
            else:
                # Preserve concurrent edits, and leave the exact original available for recovery.
                backup = path.with_name("Cargo.toml.persistence-original")
                backup.write_text(original)
                print(f"Concurrent change: left {path} untouched; original saved to {backup}", file=sys.stderr)
                result = 1
    return result


if __name__ == "__main__":
    sys.exit(main())
