# day-persistence

Typed SQLite persistence for [Day](https://daybrite.dev), maintained and versioned separately
from the UI framework. The dependency goes one way: persistence uses Day's public model,
reactivity and piece APIs; Day does not depend on persistence.

```toml
[dependencies]
day = { git = "https://github.com/daybrite/day.git", features = ["model"] }
day-persistence = { git = "https://github.com/daybrite/day-persistence.git", version = "0.1" }
```

The 0.1 series starts with the persistence API extracted from Day 0.5.1. Choose compatible
Day revisions across your app and this extension; mixing two copies of `day-model` creates
different Rust types. Release tags will version the three crates together. A Git dependency
tracks its Cargo.lock revision; pin a published tag or revision when reproducibility matters.

- [Persistence guide](docs/persistence.md): models, queries, relations, migrations, workers,
  maintenance, engine choices and browser storage.
- [Persistence Lab](demo/README.md): a native, cross-platform SQL console and typed-record lab.
- [History import](docs/history.md): authorship, commit mapping and extraction procedure.
- [Extraction validation](docs/validation.md): test results and remaining CI coverage.
- `crates/day-persistence`: public API and native SQLite driver.
- `crates/day-persistence-macros`: the `Model` derive, kept in sync with the public API.
- `crates/day-sqlite-worker`: the browser engine and its JavaScript/OPFS integration.

## Development

```sh
cargo test --workspace --features day-persistence/datetime
cargo clippy --workspace --all-targets --features day-persistence/datetime -- -D warnings
cargo check -p day-persistence --no-default-features --lib
cargo fmt --all -- --check
cd demo
cargo test --lib
cargo clippy --all-targets -- -D warnings
day launch -t macos-appkit --script dayscript/demo.yaml
```

CI runs host tests on Linux, macOS and Windows (the worker's Unix C shim is excluded on
Windows), checks the trait-only and system SQLite configurations, tests SQLCipher on Linux,
and runs the same demo dayscript through Day's shared platform workflow. HarmonyOS acceptance
runs in CI; do not start its emulator on the ARM development host.

To work against a sibling Day checkout, add ignored Cargo patches in `.cargo/config.toml`
for each Day Git dependency (including `day` and `day-build` from the demo). Cargo searches
this parent configuration for `demo/`, too. For example:

```toml
[patch."https://github.com/daybrite/day.git"]
day = { path = "../day/crates/day" }
day-build = { path = "../day/crates/day-build" }
day-model = { path = "../day/crates/day-model" }
# Repeat for the other Day dependencies in [workspace.dependencies].
```

Cargo resolves these configuration paths relative to the parent of `.cargo/`; adjust the
paths for your layout (a sibling `day/` uses `../day/crates/...`).
Do not commit machine-specific paths. The checked-in demo deliberately depends on this
repository's crate by relative path, so its CI tests the exact checkout.

Local patches also rewrite `Cargo.lock` and `demo/Cargo.lock`: external Day packages lose
their Git sources and unused local patches can appear in the locks. These lockfiles work
with the local configuration but fail CI's `--locked` checks. Before staging them, regenerate
them in a clean checkout without local Cargo patches (including parent/global configuration),
and run the locked test and lint commands there. If only local experimentation changed a
lockfile, restore its last Git-backed version instead of updating dependencies unnecessarily.

Enable the staged-lockfile guard once per clone (requires Python 3.11 or newer):

```sh
git config --local core.hooksPath .githooks
```

If you already use a hooks directory, call `python3 scripts/check-lockfiles.py --staged`
from its pre-commit hook instead of replacing it. The guard reads both lockfiles from Git's
index, permits this repository's own path dependencies, and rejects external local-path
dependencies and unused local patches. It never rewrites or stages files. Run
`python3 scripts/check-lockfiles.py` to check the working files; CI runs this check too, since
hooks are opt-in and can be bypassed. This guard complements Cargo's full `--locked` validation.

## Initial publication

Only imported history exists in Git until the extraction is committed. That historical tip
has no standalone workspace manifest, so consumers cannot resolve it as a Git package yet.
The initial workspace/demo commit must be made and published before consumer Git dependency
updates can build without local substitutions. No new-work commit is part of the extraction.
For pre-publication local builds, use the included wrapper:

```sh
python3 scripts/with-local-persistence.py ../Day-Showcase -- cargo test --workspace
python3 scripts/with-local-persistence.py ../Day-Sketch -- day launch -t macos-appkit
```

It substitutes this checkout in the consumer manifests for the command, then restores their
Git declarations, including on command failure. Do not edit those manifests concurrently.
Cargo's local patches alone cannot repair the incomplete historical Git tree. Local builds
also refresh consumer locks; regenerate those locks against the published repository before
committing them.
