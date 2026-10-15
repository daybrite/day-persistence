# Extraction validation — 2026-10-15

Validated on an Apple Silicon macOS development host. No new-work commits or pushes were made;
only the 44 filtered historical commits were imported. The source Day history remains at
`35126a99d471b3fd03985ae4d697503e65674bb8`. Authors, committers, dates and messages were compared
for every retained commit. All 103 moved crate files matched the originals before extraction
edits; the only subsequent library changes add named-column queries to the browser driver.

## Libraries and consumers

| Check | Result |
| --- | --- |
| Persistence workspace, datetime enabled | 257 passed; one pre-existing ignored documentation example |
| SQLCipher encryption/rekey tests | 3 passed |
| Demo headless model tests | 3 passed |
| Day model/reactive/macro suites | 116 passed; five ignored documentation examples |
| Day-News workspace | 114 passed |
| Day-Showcase | 36 passed |
| Day-Sketch | 109 passed |
| Day-Tunes | 6 passed |
| Day-Skipjack | 8 passed |
| Day-Bench | 7 passed |
| Day-Bench/persist | 3 passed |
| Stanza-Redux | 37 passed |

Consumer tests used the extracted local crate with their final Git declarations restored
following validation. `scripts/with-local-persistence.py` provides this workflow until the
standalone workspace has its first published commit. Day-Bench's headless benchmark keeps its
existing local-development convention and now points to the sibling persistence repository.
All Day-* manifests were scanned; apps without a persistence dependency need no new edge.

The standalone workspace and demo also passed tests and Clippy with **no local Day patches**,
using the Git revision recorded in their portable Cargo.lock files. Formatting, workflow
syntax (`actionlint`), trait-only compilation, system SQLite compilation, demo `day lint`,
and Day's `scripts/ci/check-ready.sh` passed. SQLCipher and browser worker checks supplement
the ordinary default-feature suite.

## App acceptance

`day launch -t TARGET --script dayscript/demo.yaml --fast` passed **74/74 steps**, with zero
skips and three screenshots, on each of:

- macos-appkit
- macos-gtk
- macos-qt
- ios-uikit (one explicit booted simulator)
- android-mdc (one explicit emulator)
- web-dom (Playwright driver, including OPFS reopening and named-column queries)

HarmonyOS Rust compilation and HAP packaging/signing passed. This host required
`NODE_PATH` pointing to the installed Hvigor `node_modules` and `OHOS_BASE_SDK_HOME` pointing
to the versioned API 18 SDK. No HarmonyOS emulator was launched, per the development policy.
Linux/Windows builds and HarmonyOS emulator acceptance await CI; they were not run locally.
The checked-in workflow declares the demo's full target list and uses the same script.

## Publication boundary

The new repository's current Git tip contains imported historical crate files, not the
uncommitted standalone workspace manifest. Consumer Git dependencies cannot resolve that tip
until the extraction is committed and published. Commit/publish the standalone repository
first, then regenerate consumer lockfiles against that Git source before committing consumer
updates. Local test lockfiles are not evidence that the unpublished Git source resolves.
