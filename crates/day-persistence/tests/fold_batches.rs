// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! The fold's insert batching: one table's new rows leave as multi-row upserts chunked by
//! the parameter ceiling, join memberships of a new row skip the existence probe, and the
//! shadows the triggers keep stay true across the batch.

use std::cell::RefCell;
use std::rc::Rc;

use day_persistence::{Many, Model, ModelContainer, Recorder, Sqlite, rank, schema};
use day_reactive::Binding;

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "notes", fts("title"))]
struct Note {
    #[model(id)]
    id: u32,
    title: String,
    done: bool,
    #[model(relation(target = Tag, join = "note_tags"))]
    tags: Many<Tag>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tags")]
struct Tag {
    #[model(id)]
    id: u32,
    name: String,
    #[model(relation(target = Note, join = "note_tags"))]
    notes: Many<Note>,
}

type Trace = Rc<RefCell<Vec<String>>>;

fn traced(driver: Sqlite) -> (Sqlite, Trace) {
    let trace: Trace = Rc::default();
    let sink = trace.clone();
    (
        driver.trace_sql(move |sql| sink.borrow_mut().push(sql.to_string())),
        trace,
    )
}

fn app_statements(trace: &Trace) -> Vec<String> {
    trace
        .borrow()
        .iter()
        .filter(|s| !s.starts_with("--"))
        .cloned()
        .collect()
}

fn note(id: u32, title: &str) -> Note {
    Note {
        id,
        title: title.into(),
        done: false,
        ..Default::default()
    }
}

#[test]
fn new_rows_of_one_table_leave_as_one_multi_row_upsert() {
    let (driver, log) = Recorder::new();
    let driver = driver.with_table("notes", Vec::new());
    let c = ModelContainer::open(driver, schema![Note, Tag]).expect("open");
    let sql = c
        .record_sql(|| {
            c.insert(note(1, "a"));
            c.insert(note(2, "b"));
            c.insert(note(3, "c"));
        })
        .expect("flush");
    assert_eq!(
        sql,
        [
            "INSERT INTO \"notes\" (\"id\", \"title\", \"done\") VALUES (?, ?, ?), (?, ?, ?), (?, ?, ?) \
             ON CONFLICT(\"id\") DO UPDATE SET \"title\" = excluded.\"title\", \"done\" = excluded.\"done\""
        ]
    );
    let params = log
        .entries()
        .into_iter()
        .find(|(sql, _)| sql.starts_with("INSERT INTO \"notes\""))
        .map(|(_, params)| params.len());
    assert_eq!(
        params,
        Some(9),
        "three rows' values, flattened in row order"
    );
}

#[test]
fn a_single_new_row_keeps_the_single_row_shape() {
    let (driver, _log) = Recorder::new();
    let driver = driver.with_table("notes", Vec::new());
    let c = ModelContainer::open(driver, schema![Note, Tag]).expect("open");
    let sql = c.record_sql(|| c.insert(note(1, "a"))).expect("flush");
    assert_eq!(
        sql,
        [
            "INSERT INTO \"notes\" (\"id\", \"title\", \"done\") VALUES (?, ?, ?) \
             ON CONFLICT(\"id\") DO UPDATE SET \"title\" = excluded.\"title\", \"done\" = excluded.\"done\""
        ]
    );
}

#[test]
fn batches_chunk_at_the_parameter_ceiling_and_every_row_lands() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Note, Tag]).expect("open");
    c.set_autosave(false);
    for id in 1..=2_500u32 {
        c.insert(note(id, &format!("note {id}")));
    }
    trace.borrow_mut().clear();
    c.save().expect("flush");
    let inserts: Vec<String> = app_statements(&trace)
        .into_iter()
        .filter(|s| s.starts_with("INSERT INTO \"notes\""))
        .collect();
    // Three columns and a 900-parameter ceiling: 300 rows per statement, 2 500 rows → 9.
    assert_eq!(inserts.len(), 9, "{}", inserts.len());
    let tuples: usize = inserts.iter().map(|s| s.matches("), (").count() + 1).sum();
    assert_eq!(tuples, 2_500);
    assert_eq!(c.table_count::<Note>().expect("count"), 2_500);
    // Rows that were never faulted read back whole.
    c.set_cache_limit(16);
    assert_eq!(c.get::<Note>(2_345u32).unwrap().title().peek(), "note 2345");
}

#[test]
fn the_fts_shadow_indexes_every_row_of_a_batch() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Note, Tag]).expect("open");
    c.set_autosave(false);
    for id in 1..=50u32 {
        c.insert(note(id, if id % 5 == 0 { "kyoto trip" } else { "errands" }));
    }
    c.save().expect("flush");
    let hits = c
        .query::<Note>()
        .filter(Note::fts().matches("kyoto"))
        .sort(rank())
        .live()
        .ids();
    assert_eq!(
        hits.len(),
        10,
        "the AFTER INSERT trigger fired once per row of the batch"
    );
}

#[test]
fn a_batch_of_existing_keys_is_an_upsert_that_updates() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Note, Tag]).expect("open");
    c.set_autosave(false);
    c.insert(note(1, "old one"));
    c.insert(note(2, "old two"));
    c.save().expect("seed");
    // Re-inserting through the front door replaces the resident rows and upserts the file.
    c.insert(note(1, "new one"));
    c.insert(note(2, "new two"));
    c.save().expect("upsert");
    assert_eq!(c.table_count::<Note>().expect("count"), 2);
    c.set_cache_limit(1);
    c.set_cache_limit(8192);
    assert_eq!(c.get::<Note>(1u32).unwrap().title().peek(), "new one");
    assert_eq!(c.get::<Note>(2u32).unwrap().title().peek(), "new two");
}

#[test]
fn inserts_updates_and_deletes_keep_their_order_across_tables() {
    let (driver, _log) = Recorder::new();
    let driver = driver.with_table(
        "notes",
        vec![vec![
            day_persistence::Value::Int(1),
            day_persistence::Value::Text("kept".into()),
            day_persistence::Value::Int(0),
        ]],
    );
    let c = ModelContainer::open(driver, schema![Note, Tag]).expect("open");
    let sql = c
        .record_sql(|| {
            c.insert(Tag {
                id: 1,
                name: "t".into(),
                ..Default::default()
            });
            c.insert(note(2, "new"));
            c.insert(note(3, "newer"));
            c.get::<Note>(1u32).unwrap().done().write(true);
            c.insert(Tag {
                id: 2,
                name: "u".into(),
                ..Default::default()
            });
        })
        .expect("flush");
    // Each table's inserts gather at the first insert's place; the update keeps its own.
    assert_eq!(sql.len(), 3, "{sql:?}");
    assert!(
        sql[0].starts_with("INSERT INTO \"tags\"") && sql[0].contains("), ("),
        "{sql:?}"
    );
    assert!(
        sql[1].starts_with("INSERT INTO \"notes\"") && sql[1].contains("), ("),
        "{sql:?}"
    );
    assert_eq!(sql[2], "UPDATE \"notes\" SET \"done\" = ? WHERE \"id\" = ?");
}

#[test]
fn memberships_of_a_new_row_skip_the_probe_and_batch() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Note, Tag]).expect("open");
    c.set_autosave(false);
    for id in 1..=3u32 {
        c.insert(Tag {
            id,
            name: format!("tag {id}"),
            ..Default::default()
        });
    }
    c.save().expect("tags");
    trace.borrow_mut().clear();
    c.insert(note(1, "fresh"));
    let n = c.get::<Note>(1u32).unwrap();
    assert!(n.tags().add(1u32));
    assert!(n.tags().add(2u32));
    assert!(n.tags().add(3u32));
    assert!(
        !n.tags().add(2u32),
        "already a member, answered from the dirty set"
    );
    assert_eq!(
        app_statements(&trace),
        Vec::<String>::new(),
        "a brand-new row cannot have memberships in the file: no SELECT 1 probes"
    );
    c.save().expect("flush");
    let sql = app_statements(&trace);
    let joins: Vec<&String> = sql
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"note_tags\""))
        .collect();
    assert_eq!(
        joins.len(),
        1,
        "one multi-row insert for three memberships: {sql:?}"
    );
    assert_eq!(joins[0].matches("), (").count(), 2);
    assert_eq!(n.tags().count(), 3);
    assert_eq!(c.get::<Tag>(2u32).unwrap().notes().count(), 1);

    // A pair the join cache holds answers from memory: no statement at all.
    trace.borrow_mut().clear();
    let t = c.get::<Tag>(3u32).unwrap();
    assert!(!t.notes().add(1u32), "already a member");
    assert!(
        app_statements(&trace).is_empty(),
        "{:?}",
        app_statements(&trace)
    );
}

#[test]
fn a_pair_only_the_file_holds_is_still_probed_once() {
    let path = std::env::temp_dir().join(format!(
        "day-persistence-fold-{}-probe.db",
        std::process::id()
    ));
    for ext in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Note, Tag]).expect("open");
        c.insert(Tag {
            id: 1,
            name: "t".into(),
            ..Default::default()
        });
        c.insert(note(1, "n"));
        c.get::<Note>(1u32).unwrap().tags().add(1u32);
        c.save().expect("seed");
    }
    let (driver, trace) = traced(Sqlite::at(&path));
    let c = ModelContainer::open(driver, schema![Note, Tag]).expect("reopen");
    let n = c.get::<Note>(1u32).unwrap();
    trace.borrow_mut().clear();
    // Neither row is new and the pair is not resident: only the file can answer.
    assert!(!n.tags().add(1u32), "the file holds this pair");
    let sql = app_statements(&trace);
    assert_eq!(
        sql.iter()
            .filter(|s| s.starts_with("SELECT 1 FROM \"note_tags\""))
            .count(),
        1,
        "{sql:?}"
    );
    c.save().expect("nothing to write");
    assert!(
        !app_statements(&trace)
            .iter()
            .any(|s| s.starts_with("INSERT INTO \"note_tags\"")),
        "no duplicate membership written"
    );
}
