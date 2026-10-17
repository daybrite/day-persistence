# Working on day-persistence

This workspace owns persistence, its derive macro, and the SQLite browser worker. Keep the
framework dependency one-way. Changes to storage semantics need regression tests and updates
to docs/persistence.md. Keep the three crate versions synchronized.

Do not commit Cargo.lock or demo/Cargo.lock. CI tests the latest day/main, resolved once per
run and shared by the library matrix and demo. Keep update-day-deps enabled for the demo.
Local Cargo patches and their lockfiles stay local; validate CI dependencies in a clean
checkout without those patches. Use --locked after generating the run's dependency set.

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
