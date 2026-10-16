# Persistence Lab

Read ../AGENTS.md. This app exercises the public API of the crate in the same checkout.

- src/model.rs: typed records, database commands, SQL trace and headless regression tests.
- src/lib.rs: one window's state, the four native navigation destinations, and the SQL log
  console split under each page.
- resource/locales/en/app.ftl: generated symbolic accessors for all app-owned UI text.
- dayscript/demo.yaml: the same typed/SQL/import/reopen round trip on every platform.

Keep demonstrations explicit, reversible in the disposable database, and small enough to read.
Never swallow storage errors or treat a failed query as an empty table. SQL and JSON supplied
by users are data, not UI translations. Avoid platform skips in the shared dayscript.

Day documentation: https://daybrite.dev/docs/for-agents and https://daybrite.dev/llms.txt.
