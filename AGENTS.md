# Working on day-persistence

This workspace owns persistence, its derive macro, and the SQLite browser worker. Keep the
framework dependency one-way. Changes to storage semantics need regression tests and updates
to docs/persistence.md. Keep the three crate versions synchronized.

Local Cargo patches rewrite both lockfiles. Never stage those local-path resolutions: run
`python3 scripts/check-lockfiles.py --staged` before committing, and validate dependency
changes with `--locked` in a clean checkout without local patches. Enable `.githooks` as
described in README.md. Keep both lockfiles Git-backed for external dependencies.

Run cargo fmt --all, cargo test --workspace --features day-persistence/datetime, and cargo
clippy --workspace --all-targets --features day-persistence/datetime -- -D warnings. The demo
is a separate workspace: also format, test and lint from demo/. Exercise dayscript/demo.yaml
on available native and web targets. Compile-check HarmonyOS locally; use CI for its emulator.

Use https://daybrite.dev/llms.txt and https://daybrite.dev/docs/for-agents for Day documentation.
App-owned UI strings belong in demo/resource/locales/en/app.ftl and must use generated res::str
accessors. Reference bundled resources through generated res constants. Add localized
accessibility labels to controls. Keep synthetic test values distinct from shipped UI text.

Do not edit vendored SQLite/musl sources or generated bindings as routine cleanup. Preserve
upstream licenses and document deliberate engine upgrades.
