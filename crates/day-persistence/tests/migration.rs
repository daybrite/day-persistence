// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Migration correctness on real files: a stage and its version commit together or not at
//! all, a failed stage reruns cleanly on reopen, the lightweight path adds columns with their
//! declared constraints, every other difference is a transactional rebuild (or a refusal
//! naming the column), generated triggers reconcile on open, and the maintenance checks
//! answer with the engine's own result.

use day_persistence::{
    Capabilities, DbError, DbErrorKind, MigrationPlan, Model, ModelContainer, Row, Sqlite,
    SqliteConnection, SqliteDriver, Value, schema,
};
use day_reactive::Binding;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A fresh path under the OS temp dir; the test owns it for its lifetime.
fn temp_db(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "day-persistence-migration-{}-{}.db",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    path
}

/// `PRAGMA table_info` on a raw connection: (name, type, notnull, `dflt_value`, pk).
fn table_info(
    conn: &rusqlite::Connection,
    table: &str,
) -> Vec<(String, String, bool, Option<String>, bool)> {
    conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)? != 0,
            ))
        })
        .unwrap()
        .map(|c| c.unwrap())
        .collect()
}

fn db_version(conn: &rusqlite::Connection) -> Option<i64> {
    conn.query_row(
        "SELECT version FROM _day_schema WHERE table_name = '_db'",
        [],
        |r| r.get(0),
    )
    .ok()
}

/// A traced statement that changes the schema (the `IF NOT EXISTS` forms are reads when the
/// object exists, and the bookkeeping table's is always one of those).
fn is_ddl_write(sql: &str) -> bool {
    sql.starts_with("ALTER ")
        || sql.starts_with("DROP ")
        || sql.starts_with("CREATE TRIGGER")
        || sql.starts_with("CREATE INDEX")
        || sql.starts_with("CREATE UNIQUE")
        || (sql.starts_with("CREATE TABLE") && !sql.contains("IF NOT EXISTS"))
}

fn has_table(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        [table],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        == 1
}

// --- failure injection -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    /// Fail the `_db` version row's write.
    VersionWrite,
}

struct Faulty {
    path: std::path::PathBuf,
    fault: Rc<Cell<Fault>>,
}

struct FaultyConn(Box<dyn SqliteConnection>, Rc<Cell<Fault>>);

impl SqliteDriver for Faulty {
    type Connection = FaultyConn;
    fn open(self) -> Result<FaultyConn, DbError> {
        Ok(FaultyConn(
            Box::new(Sqlite::at(&self.path).open()?),
            self.fault,
        ))
    }
    fn capabilities(&self) -> Capabilities {
        Sqlite::at(&self.path).capabilities()
    }
}

impl SqliteConnection for FaultyConn {
    fn execute(&mut self, sql: &str, p: &[Value]) -> Result<u64, DbError> {
        if self.1.get() == Fault::VersionWrite
            && sql.starts_with(
                "INSERT INTO _day_schema (table_name, fingerprint, version) VALUES ('_db'",
            )
        {
            return Err(DbError::driver("synthetic version-write failure"));
        }
        self.0.execute(sql, p)
    }
    fn query(
        &mut self,
        sql: &str,
        p: &[Value],
        row: &mut dyn FnMut(&dyn Row),
    ) -> Result<(), DbError> {
        self.0.query(sql, p, row)
    }
    fn execute_batch(&mut self, sql: &str) -> Result<(), DbError> {
        self.0.execute_batch(sql)
    }
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "migration_notes")]
struct Note {
    #[model(id)]
    id: u32,
    title: String,
}

fn marker_stage(conn: &mut dyn SqliteConnection) -> Result<(), DbError> {
    conn.execute(
        "CREATE TABLE stage_marker (id INTEGER PRIMARY KEY, note TEXT NOT NULL)",
        &[],
    )?;
    conn.execute(
        "INSERT INTO stage_marker (id, note) VALUES (1, 'stage ran')",
        &[],
    )
    .map(|_| ())
}

#[test]
fn a_stage_whose_version_write_fails_rolls_back_with_it() {
    let path = temp_db("stage-version");
    let fault = Rc::new(Cell::new(Fault::VersionWrite));
    let driver = Faulty {
        path: path.clone(),
        fault: fault.clone(),
    };
    let plan = MigrationPlan::new().custom(0, 1, marker_stage);
    let err = ModelContainer::open_with(driver, schema![Note], plan)
        .err()
        .expect("the open fails with the version write");
    assert_eq!(err.kind, DbErrorKind::Driver, "{err}");
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        assert!(
            !has_table(&raw, "stage_marker"),
            "the stage's effects rolled back with its version"
        );
        assert_eq!(db_version(&raw), None, "the file is still at version 0");
    }

    // The next launch reruns the stage from the old state and lands version and effects
    // together.
    fault.set(Fault::None);
    let driver = Faulty {
        path: path.clone(),
        fault: fault.clone(),
    };
    let plan = MigrationPlan::new().custom(0, 1, marker_stage);
    let c = ModelContainer::open_with(driver, schema![Note], plan).expect("clean rerun");
    drop(c);
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert!(has_table(&raw, "stage_marker"));
    assert_eq!(db_version(&raw), Some(1));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_failed_stage_reruns_cleanly_on_reopen_and_never_twice() {
    let path = temp_db("stage-rerun");
    // First launch: the stage does its DDL and then fails. Nothing of it may remain.
    let plan = MigrationPlan::new().custom(0, 1, |conn| {
        marker_stage(conn)?;
        Err(DbError::driver("synthetic stage failure"))
    });
    assert!(ModelContainer::open_with(Sqlite::at(&path), schema![Note], plan).is_err());
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        assert!(!has_table(&raw, "stage_marker"));
        assert_eq!(db_version(&raw), None);
    }
    // Second launch: the same stage, intact this time, applies once.
    let plan = MigrationPlan::new().custom(0, 1, marker_stage);
    let c = ModelContainer::open_with(Sqlite::at(&path), schema![Note], plan).expect("rerun");
    c.insert(Note {
        id: 1,
        title: "after migration".into(),
    });
    c.save().unwrap();
    drop(c);
    // Third launch: the stage is recorded as applied, so its non-idempotent DDL is not run
    // again (it would fail on the existing table).
    let plan = MigrationPlan::new().custom(0, 1, marker_stage);
    let c = ModelContainer::open_with(Sqlite::at(&path), schema![Note], plan).expect("third open");
    assert_eq!(
        c.get::<Note>(1u32).unwrap().title().peek(),
        "after migration"
    );
    drop(c);
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(db_version(&raw), Some(1));
    let _ = std::fs::remove_file(&path);
}

// --- lightweight additions carry their constraints -------------------------------------------

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tasks")]
struct TaskV1 {
    #[model(id)]
    id: u32,
    title: String,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tasks")]
struct TaskV2 {
    #[model(id)]
    id: u32,
    title: String,
    count: i64,
    label: String,
    weight: f64,
    #[model(unique)]
    slug: String,
    note: Option<String>,
}

#[test]
fn an_added_required_column_is_not_null_with_its_rust_default_and_unique_gets_an_index() {
    let upgraded = temp_db("add-constrained");
    let fresh = temp_db("add-fresh");
    {
        let c = ModelContainer::open(Sqlite::at(&upgraded), schema![TaskV1]).expect("v1");
        c.insert(TaskV1 {
            id: 1,
            title: "carried".into(),
        });
        c.save().unwrap();
    }
    {
        let c = ModelContainer::open(Sqlite::at(&upgraded), schema![TaskV2]).expect("v2");
        let row = c.get::<TaskV2>(1u32).expect("faults");
        assert_eq!(row.title().peek(), "carried");
        assert_eq!(row.count().peek(), 0);
        assert_eq!(row.label().peek(), "");
        assert_eq!(row.note().peek(), None);
    }
    {
        let _c = ModelContainer::open(Sqlite::at(&fresh), schema![TaskV2]).expect("fresh v2");
    }
    let up = rusqlite::Connection::open(&upgraded).unwrap();
    let new = rusqlite::Connection::open(&fresh).unwrap();
    let up_info = table_info(&up, "tasks");
    let by_name = |n: &str| up_info.iter().find(|c| c.0 == n).cloned().unwrap();
    assert!(by_name("count").2, "count is NOT NULL");
    assert_eq!(by_name("count").3.as_deref(), Some("0"));
    assert_eq!(by_name("label").3.as_deref(), Some("''"));
    assert_eq!(by_name("weight").3.as_deref(), Some("0.0"));
    assert!(!by_name("note").2, "an Option stays nullable");
    // Fresh and upgraded agree on every enforced column constraint (a default is not one).
    let shape = |info: &[(String, String, bool, Option<String>, bool)]| {
        info.iter()
            .map(|c| (c.0.clone(), c.1.clone(), c.2, c.4))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&up_info), shape(&table_info(&new, "tasks")));
    // NULL is rejected either way…
    for conn in [&up, &new] {
        let err = conn
            .execute(
                "INSERT INTO tasks (id, title, count, label, weight, slug, note) \
                 VALUES (7, 't', NULL, 'l', 1.0, 's7', NULL)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("NOT NULL"), "{err}");
    }
    // …and so is a duplicate slug: inline UNIQUE on the fresh table, a unique index on the
    // upgraded one, since ADD COLUMN cannot carry the constraint.
    let has_uidx: i64 = up
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'day_uidx_tasks_slug'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_uidx, 1);
    for conn in [&up, &new] {
        conn.execute(
            "INSERT INTO tasks (id, title, count, label, weight, slug, note) \
             VALUES (8, 't', 0, 'l', 1.0, 'dup', NULL)",
            [],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO tasks (id, title, count, label, weight, slug, note) \
                 VALUES (9, 't', 0, 'l', 1.0, 'dup', NULL)",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "{err}");
    }
    drop((up, new));
    // A reopen with the same declaration finds the fingerprint and the upgraded layout as
    // equivalent: no DDL at all.
    let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    let driver =
        Sqlite::at(&upgraded).trace_sql(move |sql| sink.borrow_mut().push(sql.to_string()));
    let c = ModelContainer::open(driver, schema![TaskV2]).expect("reopen");
    assert!(
        !seen.borrow().iter().any(|s| is_ddl_write(s)),
        "{:?}",
        seen.borrow()
    );
    drop(c);
    // A later lightweight change keeps the unique index: it is a declared index of the
    // upgraded layout, not a leftover.
    {
        let _c = ModelContainer::open(Sqlite::at(&upgraded), schema![TaskV3]).expect("v3");
    }
    let up = rusqlite::Connection::open(&upgraded).unwrap();
    let has_uidx: i64 = up
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'day_uidx_tasks_slug'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_uidx, 1, "the unique index survives the next migration");
    assert_eq!(
        table_info(&up, "tasks").last().map(|c| c.0.as_str()),
        Some("extra")
    );
    drop(up);
    let _ = std::fs::remove_file(&upgraded);
    let _ = std::fs::remove_file(&fresh);
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tasks")]
struct TaskV3 {
    #[model(id)]
    id: u32,
    title: String,
    count: i64,
    label: String,
    weight: f64,
    #[model(unique)]
    slug: String,
    note: Option<String>,
    extra: Option<i64>,
}

// --- everything else is a rebuild, or a refusal ----------------------------------------------

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tasks")]
struct TaskCounted {
    #[model(id)]
    id: u32,
    title: String,
    count: i64,
}

#[test]
fn an_old_nullable_addition_is_rebuilt_to_the_declared_constraints_with_data_preserved() {
    let path = temp_db("rebuild-nullable");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![TaskV1]).expect("v1");
        for (id, title) in [(1, "first"), (2, "second")] {
            c.insert(TaskV1 {
                id,
                title: title.into(),
            });
        }
        c.save().unwrap();
    }
    {
        // What the older lightweight path did: a nullable column, backfilled by UPDATE. The
        // fingerprint stays the v1 one, so the next open compares the file to the
        // declaration and finds a nullability difference.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "ALTER TABLE tasks ADD COLUMN count INTEGER; UPDATE tasks SET count = 0; \
             CREATE INDEX app_tasks_title ON tasks(title); \
             INSERT INTO tasks (id, title, count) VALUES (3, 'third', NULL); \
             CREATE TABLE audit (id INTEGER PRIMARY KEY, note TEXT); \
             CREATE TRIGGER audit_lookup AFTER INSERT ON audit BEGIN \
               UPDATE audit SET note = (SELECT title FROM tasks WHERE id = NEW.id) \
               WHERE id = NEW.id; END; \
             CREATE TRIGGER tasks_forget AFTER DELETE ON tasks BEGIN \
               DELETE FROM audit WHERE id = OLD.id; END;",
        )
        .unwrap();
        assert!(!table_info(&raw, "tasks")[2].2, "count is nullable before");
    }
    {
        let c =
            ModelContainer::open(Sqlite::at(&path), schema![TaskCounted]).expect("rebuilt open");
        assert_eq!(c.table_count::<TaskCounted>().unwrap(), 3);
        assert_eq!(c.get::<TaskCounted>(2u32).unwrap().title().peek(), "second");
        assert_eq!(
            c.get::<TaskCounted>(3u32).unwrap().count().peek(),
            0,
            "the NULL the old path tolerated filled from the Rust default"
        );
        assert_eq!(c.integrity_check().unwrap(), [] as [std::string::String; 0]);
        assert_eq!(
            c.foreign_key_check().unwrap(),
            [] as [day_persistence::ForeignKeyViolation; 0]
        );
    }
    let raw = rusqlite::Connection::open(&path).unwrap();
    let info = table_info(&raw, "tasks");
    assert_eq!(
        info.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        ["id", "title", "count"]
    );
    assert!(info[2].2, "count is NOT NULL after the rebuild: {info:?}");
    assert!(!has_table(&raw, "tasks_new"), "the temporary table is gone");
    let err = raw
        .execute(
            "INSERT INTO tasks (id, title, count) VALUES (4, 'x', NULL)",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("NOT NULL"), "{err}");
    let app_index: i64 = raw
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'app_tasks_title'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        app_index, 1,
        "an application index over surviving columns is re-created"
    );
    // The application's trigger on the rebuilt table came back, and another table's trigger
    // that names the rebuilt table (what an adapter's capture trigger on a child does) still
    // resolves: the rename did not trip over the moment the table was absent.
    raw.execute("INSERT INTO audit (id) VALUES (2)", [])
        .unwrap();
    let note: String = raw
        .query_row("SELECT note FROM audit WHERE id = 2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        note, "second",
        "a trigger on another table reads the rebuilt one"
    );
    raw.execute("DELETE FROM tasks WHERE id = 2", []).unwrap();
    let left: i64 = raw
        .query_row("SELECT count(*) FROM audit WHERE id = 2", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0, "the rebuilt table's own trigger was re-created");
    let ddl: String = raw
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(ddl.ends_with("STRICT"), "{ddl}");
    let _ = std::fs::remove_file(&path);
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tasks")]
struct TaskTextCount {
    #[model(id)]
    id: u32,
    title: String,
    count: String,
}

#[test]
fn a_type_change_rebuilds_and_converts_the_stored_values() {
    let path = temp_db("rebuild-type");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![TaskCounted]).expect("int");
        c.insert(TaskCounted {
            id: 1,
            title: "t".into(),
            count: 42,
        });
        c.save().unwrap();
    }
    let c = ModelContainer::open(Sqlite::at(&path), schema![TaskTextCount]).expect("text");
    assert_eq!(c.get::<TaskTextCount>(1u32).unwrap().count().peek(), "42");
    drop(c);
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(table_info(&raw, "tasks")[2].1, "TEXT");
    let _ = std::fs::remove_file(&path);
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tasks")]
struct TaskStringKey {
    #[model(id)]
    id: String,
    title: String,
}

#[test]
fn a_key_type_change_is_refused_naming_table_and_column_and_leaves_the_file_alone() {
    let path = temp_db("refuse-key");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![TaskV1]).expect("v1");
        c.insert(TaskV1 {
            id: 1,
            title: "kept".into(),
        });
        c.save().unwrap();
    }
    let err = ModelContainer::open(Sqlite::at(&path), schema![TaskStringKey])
        .err()
        .expect("refused");
    assert_eq!(err.kind, DbErrorKind::Schema);
    assert!(
        err.message.contains("`tasks`") && err.message.contains("`id`"),
        "{}",
        err.message
    );
    let c = ModelContainer::open(Sqlite::at(&path), schema![TaskV1]).expect("v1 still opens");
    assert_eq!(c.get::<TaskV1>(1u32).unwrap().title().peek(), "kept");
    let _ = std::fs::remove_file(&path);
}

// --- generated triggers reconcile on open --------------------------------------------------------

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "articles", fts("title"))]
struct Article {
    #[model(id)]
    id: u32,
    title: String,
    read: bool,
}

#[test]
fn generated_triggers_are_replaced_when_their_text_changed_and_left_alone_otherwise() {
    let path = temp_db("triggers");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Article]).expect("open");
        c.insert(Article {
            id: 1,
            title: "maple season".into(),
            read: false,
        });
        c.save().unwrap();
    }
    {
        // An older build's trigger: fires on every update.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TRIGGER articles_fts_au; \
             CREATE TRIGGER articles_fts_au AFTER UPDATE ON articles BEGIN SELECT 1; END;",
        )
        .unwrap();
    }
    let trace = |path: &std::path::Path| {
        let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        let driver = Sqlite::at(path).trace_sql(move |sql| sink.borrow_mut().push(sql.to_string()));
        let c = ModelContainer::open(driver, schema![Article]).expect("open");
        drop(c);
        Rc::try_unwrap(seen).unwrap().into_inner()
    };
    let first = trace(&path);
    assert!(
        first
            .iter()
            .any(|s| s.starts_with("DROP TRIGGER") && s.contains("articles_fts_au")),
        "{first:?}"
    );
    assert!(
        first
            .iter()
            .any(|s| s.contains("CREATE TRIGGER \"articles_fts_au\" AFTER UPDATE OF")),
        "{first:?}"
    );
    assert!(
        !first
            .iter()
            .any(|s| s.contains("CREATE TRIGGER \"articles_fts_ai\"")),
        "an unchanged trigger is not rewritten: {first:?}"
    );
    let second = trace(&path);
    assert!(
        !second.iter().any(|s| is_ddl_write(s)),
        "a matching file opens with no DDL: {second:?}"
    );
    let raw = rusqlite::Connection::open(&path).unwrap();
    let sql: String = raw
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'articles_fts_au'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sql.contains("AFTER UPDATE OF \"id\", \"title\" ON"),
        "{sql}"
    );
    assert!(
        sql.contains("WHEN new.\"id\" IS NOT old.\"id\" OR new.\"title\" IS NOT old.\"title\""),
        "{sql}"
    );
    let _ = std::fs::remove_file(&path);
}

// --- maintenance answers ------------------------------------------------------------------------

#[test]
fn checkpoint_answers_with_the_engines_result() {
    let path = temp_db("checkpoint");
    let c = ModelContainer::open(Sqlite::at(&path), schema![Note]).expect("open");
    c.insert(Note {
        id: 1,
        title: "x".repeat(8192),
    });
    c.save().unwrap();
    // The engine counts after the TRUNCATE: a completed checkpoint reports an empty log.
    let result = c.checkpoint().expect("checkpoint");
    assert_eq!(
        result,
        day_persistence::Checkpoint {
            busy: false,
            log_frames: 0,
            checkpointed_frames: 0
        }
    );
    assert!(result.is_complete());
    // The row reached the main file: a raw reader at the path sees it with no WAL to replay.
    c.with_connection(|conn| {
        conn.execute(
            "INSERT INTO migration_notes (id, title) VALUES (2, 'in the log')",
            &[],
        )
    })
    .expect("raw write");
    assert!(c.checkpoint().expect("second checkpoint").is_complete());
    let wal = path.with_extension("db-wal");
    assert_eq!(
        std::fs::metadata(&wal).map_or(0, |m| m.len()),
        0,
        "the log was truncated"
    );
    drop(c);

    let memory = ModelContainer::open(Sqlite::memory(), schema![Note]).expect("memory");
    let result = memory.checkpoint().expect("no WAL to fold");
    assert_eq!(
        (result.log_frames, result.checkpointed_frames),
        (-1, -1),
        "{result:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn foreign_key_check_reports_dangling_rows_and_integrity_check_includes_them() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Note]).expect("open");
    c.with_connection(|conn| {
        conn.execute_batch(
            "CREATE TABLE parents (id INTEGER PRIMARY KEY); \
             CREATE TABLE children (id INTEGER PRIMARY KEY, parent INTEGER REFERENCES parents(id)); \
             PRAGMA foreign_keys = OFF; \
             INSERT INTO children (id, parent) VALUES (5, 99); \
             PRAGMA foreign_keys = ON;",
        )
    })
    .expect("fixture");
    let violations = c.foreign_key_check().expect("check");
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert_eq!(violations[0].table, "children");
    assert_eq!(violations[0].rowid, Some(5));
    assert_eq!(violations[0].parent, "parents");
    let findings = c.integrity_check().expect("integrity");
    assert_eq!(
        findings,
        ["foreign key: children row 5 references missing parents"]
    );
    c.with_connection(|conn| conn.execute("INSERT INTO parents (id) VALUES (99)", &[]))
        .expect("repair");
    assert_eq!(
        c.foreign_key_check().unwrap(),
        [] as [day_persistence::ForeignKeyViolation; 0]
    );
    assert_eq!(c.integrity_check().unwrap(), [] as [std::string::String; 0]);
}
