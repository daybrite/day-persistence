// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Lazy blob columns: a `Blob` field is never faulted with its row, reads on demand through
//! a byte-bounded memo, writes as pending bytes the flush carries, and survives every path
//! that rewrites the rest of its row.

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use day_model::{Op, Uuid};
use day_persistence::{Blob, BlobField, DbError, Model, ModelContainer, Sqlite, Value, schema};
use day_reactive::Binding;

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "photos")]
struct Photo {
    #[model(id)]
    id: u64,
    name: String,
    // Declared in the middle on purpose: the derive still places it last in the table.
    data: Blob,
    stars: u32,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "documents")]
struct Document {
    #[model(id)]
    id: Uuid,
    title: String,
    body: Blob,
    preview: Blob,
}

/// The eager shape of `photos`, for the migration test: the same column as `Vec<u8>`.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "photos")]
struct EagerPhoto {
    #[model(id)]
    id: u64,
    name: String,
    data: Vec<u8>,
    stars: u32,
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

/// The app's own statements since the trace was last cleared (engine frames start `--`).
fn statements(trace: &Trace) -> Vec<String> {
    trace
        .borrow()
        .iter()
        .filter(|s| !s.starts_with("--"))
        .cloned()
        .collect()
}

fn temp_db(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "day-persistence-blob-{}-{name}.db",
        std::process::id()
    ));
    for ext in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
    path
}

fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn photo(id: u64, data: Option<Vec<u8>>) -> Photo {
    Photo {
        id,
        name: format!("photo {id}"),
        data: data.map(Blob::new).unwrap_or_default(),
        stars: 0,
    }
}

fn raw_blob_len(c: &ModelContainer, id: u64) -> i64 {
    let mut n = -1;
    c.with_connection(|conn| {
        conn.query(
            "SELECT length(\"data\") FROM \"photos\" WHERE \"id\" = ?",
            &[Value::Int(id as i64)],
            &mut |row| n = row.get(0).as_int().unwrap_or(-2),
        )
    })
    .expect("raw length");
    n
}

// --- declaration and DDL ----------------------------------------------------------------------

#[test]
fn the_derive_marks_the_column_lazy_nullable_and_last() {
    let cols = Photo::COLUMNS;
    let names: Vec<&str> = cols.iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        ["id", "name", "stars", "data"],
        "the blob moved last"
    );
    let data = cols.iter().find(|c| c.name == "data").unwrap();
    assert!(data.lazy);
    assert!(
        !data.not_null,
        "a blob column is nullable: NULL reads as empty"
    );
    assert!(cols.iter().filter(|c| c.name != "data").all(|c| !c.lazy));
    assert_eq!(Photo::LAZY_FIELDS, ["data"]);
    assert_eq!(Document::LAZY_FIELDS, ["body", "preview"]);
    assert_eq!(Photo::default().data, Blob::stored());
    assert_eq!(Blob::new(vec![1]), Blob::new(vec![1]));
    assert_ne!(Blob::new(vec![1]), Blob::new(vec![2]));
    assert!(
        Blob::new(vec![]) != Blob::stored(),
        "pending and stored differ"
    );
}

#[test]
fn the_ddl_declares_a_nullable_blob_column_last() {
    let (driver, trace) = traced(Sqlite::memory());
    let _c = ModelContainer::open(driver, schema![Photo]).expect("open");
    let create = statements(&trace)
        .into_iter()
        .find(|s| s.starts_with("CREATE TABLE \"photos\""))
        .expect("create table");
    assert!(
        create.ends_with("\"stars\" INTEGER NOT NULL, \"data\" BLOB) STRICT"),
        "{create}"
    );
}

// --- the write side ---------------------------------------------------------------------------

#[test]
fn an_insert_carries_the_bytes_and_settles_to_stored() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.set_autosave(false);
    let data = bytes(10_000, 1);
    c.insert(photo(1, Some(data.clone())));
    let elem = c.get::<Photo>(1u64).unwrap();
    assert!(elem.data().peek().is_pending());
    // A pending value answers reads before the flush, with no SQL.
    trace.borrow_mut().clear();
    assert_eq!(*elem.data().bytes().unwrap(), data);
    assert_eq!(elem.data().len().unwrap(), 10_000);
    assert!(statements(&trace).is_empty(), "{:?}", statements(&trace));

    c.save().expect("flush");
    let upsert = statements(&trace)
        .into_iter()
        .find(|s| s.starts_with("INSERT INTO \"photos\""))
        .expect("upsert");
    assert!(
        upsert.contains("\"data\" = COALESCE(excluded.\"data\", \"data\")"),
        "{upsert}"
    );
    assert!(upsert.contains("x'"), "the bytes were bound: {upsert}");
    assert!(
        !elem.data().peek().is_pending(),
        "the flush settled the field to stored"
    );
    assert_eq!(c.blob_cache_len(), 1, "and parked the bytes in the memo");
    assert_eq!(c.blob_cache_bytes(), 10_000);
    assert_eq!(raw_blob_len(&c, 1), 10_000);

    // Settling went through the database's own author: nothing is dirty again.
    trace.borrow_mut().clear();
    c.save().expect("nothing to flush");
    assert!(statements(&trace).is_empty(), "{:?}", statements(&trace));
}

#[test]
fn updating_other_columns_never_touches_the_stored_bytes() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.set_autosave(false);
    c.insert(photo(1, Some(bytes(500, 2))));
    c.save().expect("seed");
    let elem = c.get::<Photo>(1u64).unwrap();

    // A field write: the UPDATE names only that column.
    trace.borrow_mut().clear();
    elem.stars().write(5);
    c.save().expect("update");
    let update = statements(&trace)
        .into_iter()
        .find(|s| s.starts_with("UPDATE \"photos\""))
        .expect("update");
    assert_eq!(
        update,
        "UPDATE \"photos\" SET \"stars\" = 5 WHERE \"id\" = 1"
    );
    assert_eq!(raw_blob_len(&c, 1), 500);

    // A wholesale rewrite of the store (the import pattern): the upsert carries NULL for the
    // stored blob and COALESCE keeps the file's bytes.
    trace.borrow_mut().clear();
    c.cache::<Photo>().update("import", |k| {
        *k = day_model::Keyed::new(vec![Photo {
            id: 1,
            name: "renamed".into(),
            data: Blob::stored(),
            stars: 5,
        }]);
    });
    c.save().expect("rewrite");
    let upsert = statements(&trace)
        .into_iter()
        .find(|s| s.starts_with("INSERT INTO \"photos\""))
        .expect("upsert");
    assert!(upsert.contains("NULL"), "{upsert}");
    assert_eq!(raw_blob_len(&c, 1), 500, "COALESCE kept the bytes");
    assert_eq!(elem.data().bytes().unwrap().len(), 500);
}

#[test]
fn write_bytes_is_pending_until_the_flush_and_then_an_update() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.set_autosave(false);
    c.insert(photo(1, Some(bytes(100, 3))));
    c.save().expect("seed");
    let elem = c.get::<Photo>(1u64).unwrap();
    assert_eq!(elem.data().bytes().unwrap().len(), 100);

    let fresh = bytes(2_000, 4);
    elem.data().write_bytes(fresh.clone());
    trace.borrow_mut().clear();
    assert_eq!(
        *elem.data().bytes().unwrap(),
        fresh,
        "the pending value answers"
    );
    assert_eq!(elem.data().read_range(10..20).unwrap(), fresh[10..20]);
    assert_eq!(elem.data().len().unwrap(), 2_000);
    assert!(
        statements(&trace).is_empty(),
        "no SQL before the flush (with_connection would flush, so the file is not probed here): {:?}",
        statements(&trace)
    );

    c.save().expect("flush");
    let update = statements(&trace)
        .into_iter()
        .find(|s| s.starts_with("UPDATE \"photos\""))
        .expect("update");
    assert!(
        update.starts_with("UPDATE \"photos\" SET \"data\" = x'"),
        "{update}"
    );
    assert_eq!(raw_blob_len(&c, 1), 2_000);
    assert!(!elem.data().peek().is_pending());
    assert_eq!(
        *elem.data().bytes().unwrap(),
        fresh,
        "the memo holds the new value"
    );
}

#[test]
fn two_blob_fields_settle_independently() {
    let (driver, _trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Document]).expect("open");
    c.set_autosave(false);
    let id = Uuid::now_v7();
    c.insert(Document {
        id,
        title: "t".into(),
        body: Blob::new(bytes(300, 5)),
        preview: Blob::stored(),
    });
    c.save().expect("seed");
    let d = c.get::<Document>(id).unwrap();
    assert_eq!(d.body().len().unwrap(), 300);
    assert_eq!(
        d.preview().len().unwrap(),
        0,
        "never written: NULL reads as empty"
    );
    assert!(d.preview().is_empty().unwrap());
    d.preview().write_bytes(bytes(40, 6));
    c.save().expect("flush");
    assert_eq!(
        d.body().len().unwrap(),
        300,
        "body untouched by preview's update"
    );
    assert_eq!(d.preview().len().unwrap(), 40);
    assert_eq!(c.blob_cache_len(), 2);
}

// --- the read side ----------------------------------------------------------------------------

#[test]
fn a_fault_selects_null_for_the_blob_and_reads_it_on_demand_once() {
    let path = temp_db("fault");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Photo]).expect("open");
        c.set_autosave(false);
        for id in 1..=3 {
            c.insert(photo(id, Some(bytes(1_000 * id as usize, id as u8))));
        }
        c.save().expect("seed");
    }
    let (driver, trace) = traced(Sqlite::at(&path));
    let c = ModelContainer::open(driver, schema![Photo]).expect("reopen");
    trace.borrow_mut().clear();
    let elem = c.get::<Photo>(2u64).unwrap();
    let fault = statements(&trace)
        .into_iter()
        .find(|s| s.starts_with("SELECT"))
        .expect("fault select");
    assert_eq!(
        fault,
        "SELECT \"id\", \"name\", \"stars\", NULL FROM \"photos\" WHERE \"id\" IN (2)"
    );
    assert_eq!(elem.name().peek(), "photo 2");
    assert!(!elem.data().peek().is_pending());

    trace.borrow_mut().clear();
    let first = elem.data().bytes().unwrap();
    assert_eq!(first.len(), 2_000);
    assert_eq!(
        statements(&trace),
        ["SELECT \"data\" FROM \"photos\" WHERE \"id\" = 2"]
    );
    trace.borrow_mut().clear();
    let second = elem.data().bytes().unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "the memo served it"
    );
    assert_eq!(statements(&trace), [] as [std::string::String; 0]);
    assert_eq!(c.blob_cache_bytes(), 2_000);

    // A collected snapshot carries stored blobs, never bytes.
    let all = c.query::<Photo>().live().try_collect().unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|p| p.data == Blob::stored()));
}

#[test]
fn len_and_ranges_read_only_what_they_ask_for() {
    let path = temp_db("ranges");
    let data = bytes(100_000, 7);
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Photo]).expect("open");
        c.insert(photo(1, Some(data.clone())));
        c.save().expect("seed");
    }
    let (driver, trace) = traced(Sqlite::at(&path));
    let c = ModelContainer::open(driver, schema![Photo]).expect("reopen");
    let elem = c.get::<Photo>(1u64).unwrap();
    trace.borrow_mut().clear();
    assert_eq!(elem.data().len().unwrap(), 100_000);
    assert_eq!(
        statements(&trace),
        ["SELECT length(\"data\") FROM \"photos\" WHERE \"id\" = 1"]
    );
    trace.borrow_mut().clear();
    assert_eq!(elem.data().read_range(10..20).unwrap(), data[10..20]);
    assert_eq!(
        statements(&trace),
        ["SELECT substr(\"data\", 11, 10) FROM \"photos\" WHERE \"id\" = 1"]
    );
    // Clamping: past the end, straddling the end, empty, reversed.
    assert_eq!(
        elem.data().read_range(99_990..200_000).unwrap(),
        data[99_990..]
    );
    assert_eq!(
        elem.data().read_range(200_000..200_010).unwrap(),
        [] as [u8; 0]
    );
    assert_eq!(elem.data().read_range(5..5).unwrap(), [] as [u8; 0]);
    // A reversed range is a caller mistake the accessor answers with nothing, not a panic.
    #[allow(clippy::reversed_empty_ranges)]
    let reversed = 20..10;
    assert_eq!(elem.data().read_range(reversed).unwrap(), [] as [u8; 0]);
    assert_eq!(c.blob_cache_len(), 0, "ranges and lengths are not memoized");

    // The reader walks the value in chunks and lands on the same bytes.
    let mut out = Vec::new();
    elem.data().reader().read_to_end(&mut out).unwrap();
    assert_eq!(out, data);
    let mut head = [0u8; 16];
    let n = elem.data().reader().read(&mut head).unwrap();
    assert_eq!(n, 16);
    assert_eq!(head, data[..16]);

    // Once the whole value is memoized, ranges come from the memo without SQL.
    let _ = elem.data().bytes().unwrap();
    trace.borrow_mut().clear();
    assert_eq!(elem.data().read_range(0..3).unwrap(), data[..3]);
    assert_eq!(elem.data().len().unwrap(), 100_000);
    assert_eq!(statements(&trace), [] as [std::string::String; 0]);
}

#[test]
fn null_and_empty_values_read_as_empty() {
    let (driver, _trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.insert(photo(1, None));
    c.insert(photo(2, Some(Vec::new())));
    c.save().expect("seed");
    for id in [1u64, 2] {
        let e = c.get::<Photo>(id).unwrap();
        assert!(e.data().bytes().unwrap().is_empty());
        assert_eq!(e.data().len().unwrap(), 0);
        assert!(e.data().is_empty().unwrap());
        assert_eq!(e.data().read_range(0..10).unwrap(), [] as [u8; 0]);
        let mut out = Vec::new();
        assert_eq!(e.data().reader().read_to_end(&mut out).unwrap(), 0);
    }
    let nulls = c
        .query::<Photo>()
        .filter(Photo::data().is_null())
        .live_count()
        .get();
    assert_eq!(
        nulls, 1,
        "never written is NULL; written-empty is a zero-length blob"
    );
}

#[test]
fn a_large_value_round_trips() {
    let (driver, _trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    let big = bytes(8 << 20, 9);
    c.insert(photo(1, Some(big.clone())));
    c.save().expect("seed");
    c.set_blob_cache_limit(1 << 20);
    let e = c.get::<Photo>(1u64).unwrap();
    assert_eq!(*e.data().bytes().unwrap(), big);
    assert_eq!(
        c.blob_cache_len(),
        0,
        "larger than the limit: served, never kept"
    );
    assert_eq!(
        e.data().read_range((8 << 20) - 4..8 << 20).unwrap(),
        big[(8 << 20) - 4..]
    );
}

// --- the memo ---------------------------------------------------------------------------------

#[test]
fn the_memo_is_bounded_in_bytes_oldest_out() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.set_autosave(false);
    for id in 1..=4 {
        c.insert(photo(id, Some(bytes(1_000, id as u8))));
    }
    c.save().expect("seed");
    c.set_blob_cache_limit(2_500);
    assert!(
        c.blob_cache_bytes() <= 2_500,
        "lowering the limit trims now"
    );
    let read = |id: u64| c.get::<Photo>(id).unwrap().data().bytes().unwrap();
    let _ = read(1);
    let _ = read(2);
    assert_eq!(c.blob_cache_len(), 2);
    let _ = read(3);
    assert_eq!(
        c.blob_cache_len(),
        2,
        "1 000 + 1 000 + 1 000 > 2 500: the oldest left"
    );
    trace.borrow_mut().clear();
    let _ = read(1);
    assert_eq!(statements(&trace).len(), 1, "1 was evicted, so it re-read");
    trace.borrow_mut().clear();
    let _ = read(3);
    assert!(statements(&trace).is_empty(), "3 was still memoized");
    c.set_blob_cache_limit(0);
    assert_eq!(c.blob_cache_len(), 0);
    let _ = read(2);
    assert_eq!(c.blob_cache_len(), 0, "a zero limit keeps nothing");
}

#[test]
fn evicting_a_row_or_deleting_it_drops_its_memo() {
    let (driver, _trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.set_autosave(false);
    for id in 1..=3 {
        c.insert(photo(id, Some(bytes(100, id as u8))));
    }
    c.save().expect("seed");
    assert_eq!(c.blob_cache_len(), 3, "the flush parked all three");
    c.set_cache_limit(1);
    assert!(
        c.blob_cache_len() <= 1,
        "rows left the cache, their bytes left the memo: {}",
        c.blob_cache_len()
    );
    let _ = c.get::<Photo>(2u64).unwrap().data().bytes().unwrap();
    let with_two = c.blob_cache_len();
    assert!(with_two >= 1);
    c.delete::<Photo>(2u64).expect("delete");
    c.save().expect("flush");
    assert_eq!(
        c.blob_cache_len(),
        with_two - 1,
        "the deleted row's bytes left the memo with the row; the other resident row's stay"
    );
    assert!(c.try_get::<Photo>(2u64).unwrap().is_none());
}

// --- other writers ----------------------------------------------------------------------------

#[test]
fn an_external_rewrite_of_the_bytes_is_seen_after_check_external() {
    let path = temp_db("external");
    let c = ModelContainer::open(Sqlite::at(&path), schema![Photo]).expect("open");
    c.insert(photo(1, Some(bytes(100, 1))));
    c.save().expect("seed");
    let e = c.get::<Photo>(1u64).unwrap();
    assert_eq!(e.data().bytes().unwrap().len(), 100);

    {
        let other = rusqlite::Connection::open(&path).expect("second connection");
        other
            .execute(
                "UPDATE \"photos\" SET \"data\" = ? WHERE \"id\" = 1",
                [rusqlite::types::Value::Blob(bytes(7, 2))],
            )
            .expect("external write");
    }
    assert!(c.check_external().expect("check"), "the data_version moved");
    assert_eq!(
        e.data().bytes().unwrap().len(),
        7,
        "the memo was dropped for the refreshed table and the new bytes read"
    );
    // The row itself did not announce a change: the blob column is NULL in both reads.
    // rescan drops the memo too.
    c.rescan().expect("rescan");
    assert_eq!(c.blob_cache_len(), 0);
}

#[test]
fn a_raw_insert_of_a_row_with_bytes_is_faulted_without_them() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.with_connection(|conn| {
        conn.execute(
            "INSERT INTO \"photos\" (\"id\", \"name\", \"stars\", \"data\") VALUES (9, 'raw', 0, ?)",
            &[Value::Blob(bytes(333, 3))],
        )
        .map(|_| ())
    })
    .expect("raw insert");
    c.rescan().expect("rescan");
    trace.borrow_mut().clear();
    let e = c.get::<Photo>(9u64).unwrap();
    assert!(statements(&trace)[0].contains("NULL FROM"));
    assert_eq!(e.data().len().unwrap(), 333);
}

// --- undo, worker, migration ------------------------------------------------------------------

#[test]
fn undo_reverts_an_unflushed_write_and_leaves_a_flushed_one() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.set_autosave(false);
    c.insert(photo(1, Some(bytes(10, 1))));
    c.save().expect("seed");
    let stack = c.undo(10);
    let e = c.get::<Photo>(1u64).unwrap();

    // Not yet flushed: undo drops the pending bytes, so nothing is written.
    e.data().write_bytes(bytes(20, 2));
    assert!(stack.undo());
    trace.borrow_mut().clear();
    c.save().expect("flush");
    assert!(
        !statements(&trace).iter().any(|s| s.starts_with("UPDATE")),
        "{:?}",
        statements(&trace)
    );
    assert_eq!(raw_blob_len(&c, 1), 10);

    // Flushed: the prior the history holds is "stored", which carries no bytes to restore,
    // so the undo is a no-op for the file. Blob writes are not undoable once flushed.
    e.data().write_bytes(bytes(30, 3));
    c.save().expect("flush");
    assert_eq!(raw_blob_len(&c, 1), 30);
    stack.undo();
    c.save().expect("flush");
    assert_eq!(raw_blob_len(&c, 1), 30);
    assert_eq!(e.data().bytes().unwrap().len(), 30);
}

#[test]
fn a_worker_writes_and_reads_blobs_on_its_queue() {
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    fn block_on<F: Future>(f: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let w = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&w);
        let mut f = std::pin::pin!(f);
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park_timeout(std::time::Duration::from_millis(5));
        }
    }
    let path = temp_db("worker");
    let worker = block_on(day_persistence::DatabaseWorker::open(move || {
        ModelContainer::open(Sqlite::at(&path), schema![Photo])
    }))
    .expect("worker");
    block_on(worker.write(|db| {
        db.insert(photo(1, Some(bytes(64, 1))));
        Ok(())
    }))
    .expect("write");
    let (len, head) = block_on(worker.read(|db| {
        let e = db
            .try_get::<Photo>(1u64)?
            .ok_or_else(|| DbError::driver("missing"))?;
        Ok((e.data().len()?, e.data().read_range(0..4)?))
    }))
    .expect("read");
    assert_eq!(len, 64);
    assert_eq!(head, bytes(64, 1)[..4]);
    block_on(worker.close()).expect("close");
}

#[test]
fn an_eager_blob_column_migrates_to_lazy_with_its_bytes() {
    let path = temp_db("migrate");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![EagerPhoto]).expect("open v1");
        c.insert(EagerPhoto {
            id: 1,
            name: "eager".into(),
            data: bytes(2_048, 8),
            stars: 3,
        });
        c.save().expect("seed");
    }
    // The column was NOT NULL and is now nullable: the comparison closes that through the
    // table rebuild, and the bytes come along.
    let (driver, trace) = traced(Sqlite::at(&path));
    let c = ModelContainer::open(driver, schema![Photo]).expect("open v2");
    assert!(
        statements(&trace)
            .iter()
            .any(|s| s.starts_with("CREATE TABLE") && s.contains("\"data\" BLOB)")),
        "rebuilt with the nullable column last: {:?}",
        statements(&trace)
    );
    let e = c.get::<Photo>(1u64).unwrap();
    assert_eq!(e.name().peek(), "eager");
    assert_eq!(e.stars().peek(), 3);
    assert_eq!(*e.data().bytes().unwrap(), bytes(2_048, 8));
}

#[test]
fn restructure_inserts_carry_bytes_too() {
    // The plain store door, not `container.insert`: the bytes still travel in the struct.
    let (driver, _trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Photo]).expect("open");
    c.set_autosave(false);
    let store = c.cache::<Photo>();
    store.restructure("add", Op::Insert, 5, |k| {
        k.push(photo(5, Some(bytes(55, 5))));
    });
    c.save().expect("flush");
    assert_eq!(raw_blob_len(&c, 5), 55);
    assert!(!store.elem(5).data().peek().is_pending());
}
