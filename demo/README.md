# Persistence Lab

A small cross-platform workbench for day-persistence. It opens a disposable in-memory database
initially, so experimenting does not touch saved notes.

- **Database:** switch to a fresh memory database or a named database in the app's storage;
  reopen, check integrity, checkpoint the write-ahead log, or reclaim unused space.
- **Records:** insert/update/delete typed notes using the generated model bindings; inspect
  the resulting rows as JSON. Refresh also observes changes made through raw SQL.
- **SQL console:** run a statement and inspect up to 500 result rows, or execute a SQL script.
  Inspect schema with `SELECT name, sql FROM sqlite_schema` and create your own scratch tables.
- **Import / Export:** copy typed notes as JSON, edit or paste a document, and replace the note
  table. Duplicate/zero IDs and malformed JSON are rejected before mutation. Export before
  replacing data you want to keep. File exchange uses copy/paste; there is no file picker.

Every page shows the **SQL log** under its controls, in a split you can resize. Each statement
the engine runs is added as it runs, so typed edits, queries, and maintenance all show the SQL
they produce. The log keeps the latest 200 statements across reopened databases. Durable
browser databases log to the browser's developer console instead. The toolbar's SQL log button
shows and hides it.

The console can alter schema and data, including the app's `notes` table. Destructive SQL is
intentional in a database workbench; use a disposable database for such experiments. Returning
to a fresh memory database recovers from a damaged experimental schema. Logs may contain data.
Native named databases use app data storage; web databases use OPFS and require Day's isolated
web host. Closing/reopening the same name demonstrates durability.

```sh
cargo test --lib
day launch -t macos-appkit --script dayscript/demo.yaml
# Use the same script with ios-uikit, android-mdc, linux-gtk, linux-qt,
# windows-winui, windows-xaml, harmony-arkui, or web-dom (and macOS GTK/Qt).
```

The dayscript has no platform skips. It checks typed edits, direct SQL, validated JSON import,
export/delete/import restoration, maintenance, the live SQL log and its toggle, and reopening
a named fixture database. It uses only `lab-script.sqlite` for durable tests and removes its
notes on completion. CI runs it on the declared targets; local HarmonyOS emulator testing is
intentionally excluded.

See the [persistence guide](../docs/persistence.md), [Day API tour](https://daybrite.dev/docs/api-tour)
and [dayscript reference](https://daybrite.dev/docs/dayscript).
