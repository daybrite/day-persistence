#!/usr/bin/env python3
"""Reject machine-local dependencies in the root and demo lockfiles (Python 3.11+)."""

import argparse
from pathlib import Path
import subprocess
import sys
import tomllib


LOCKFILES = ("Cargo.lock", "demo/Cargo.lock")
# These are the intentional in-repository path dependencies. Read their names from
# the same snapshot as the locks so a staged rename is checked correctly.
MANIFESTS = (
    "crates/day-persistence/Cargo.toml",
    "crates/day-persistence-macros/Cargo.toml",
    "crates/day-sqlite-worker/Cargo.toml",
    "demo/Cargo.toml",
)


def problems(lock, local_packages):
    errors = []
    for package in lock.get("package", []):
        if "source" not in package and package["name"] not in local_packages:
            errors.append(f"{package['name']}: external dependency resolved to a local path")
    for patch in lock.get("patch", {}).get("unused", []):
        if "source" not in patch:
            errors.append(f"{patch['name']}: unused local-path patch")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--staged", action="store_true", help="check Git's index, not working files")
    args = parser.parse_args()
    root = Path(subprocess.check_output(
        ["git", "rev-parse", "--show-toplevel"], text=True
    ).strip())

    def read(path):
        if args.staged:
            content = subprocess.check_output(["git", "show", f":{path}"], cwd=root)
            return tomllib.loads(content.decode("utf-8"))
        return tomllib.loads((root / path).read_text(encoding="utf-8"))

    try:
        names = [read(path)["package"]["name"] for path in MANIFESTS]
        errors = []
        for path in LOCKFILES:
            allowed = set(names if path.startswith("demo/") else names[:-1])
            errors.extend(f"{path}: {error}" for error in problems(read(path), allowed))
    except (OSError, subprocess.CalledProcessError, ValueError, KeyError) as error:
        print(f"Cannot validate lockfiles: {error}", file=sys.stderr)
        return 1
    if errors:
        print("\n".join(errors), file=sys.stderr)
        print(
            "Regenerate the lockfiles without local Cargo patches, then stage the repaired locks.\n"
            "See README.md, Development. Keep CI's --locked checks enabled.",
            file=sys.stderr,
        )
        return 1
    print("Lockfiles contain no external local-path dependencies.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
