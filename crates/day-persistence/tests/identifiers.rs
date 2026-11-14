// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Names SQL would choke on, in every place the container writes a name: keywords, spaces,
//! quotes of each kind, a comment marker, a semicolon, an emoji, a tab, a dot, and the
//! `excluded`/`new` pseudo-tables the fold's own statements speak of. Every identifier the
//! crate emits is quoted, so each of these is a working table or column. These tests take
//! such schemas through DDL, the fold, faulting, typed queries, the FTS5 and R*Tree shadows,
//! relations, join tables, migration, undo, external changes, raw queries, the worker and an
//! attached database.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use day_model::Op;
use day_persistence::{
    DatabaseWorker, GeoRect, Linked, Many, Model, ModelContainer, One, Schema, Sqlite,
    SqliteConnection, SqliteDriver, Value, ident, rank, schema,
};
use day_reactive::Binding;

// --- the schemas ----------------------------------------------------------------------------

/// Every name a SQL keyword: the table, the key, each column, the indexed pair.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "order", index("group", "select"))]
#[expect(
    clippy::struct_field_names,
    reason = "the field is named after an SQL keyword on purpose"
)]
struct Order {
    #[model(id, column = "primary")]
    id: u32,
    order: i64,
    #[model(index)]
    select: String,
    group: bool,
    #[model(column = "where")]
    place: Option<String>,
    #[model(column = "table", unique)]
    label: String,
}

/// The same table, one release later: a keyword column added, another dropped.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "order")]
struct OrderNext {
    #[model(id, column = "primary")]
    id: u32,
    order: i64,
    select: String,
    group: bool,
    #[model(column = "table", unique)]
    label: String,
    #[model(column = "limit")]
    limit: i64,
}

/// Names built to break a statement: quotes of every kind, a comment marker, a semicolon, an
/// emoji, a tab, non-Latin script, and the pseudo-tables the fold's SQL names (`excluded` in
/// the upsert, `new` in the shadow triggers). The `fts(…)` and `index(…)` lists spell the
/// column names the same way the fields do, escapes included.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(
    table = "it's \"my\" notes; -- 🚀",
    fts("title 🧨", "body\twith tab"),
    index("title 🧨", "日本語")
)]
struct Weird {
    #[model(id, column = "id; drop table x")]
    id: u64,
    #[model(column = "title 🧨")]
    title: String,
    #[model(column = "body\twith tab")]
    body: String,
    #[model(column = "日本語", index)]
    japanese: i64,
    #[model(column = "back`tick [bracket] 'single'")]
    tick: f64,
    #[model(column = "excluded")]
    excluded: bool,
    #[model(column = "new")]
    fresh: i64,
    #[model(column = r#"raw "literal" \n"#)]
    raw: String,
}

/// An R*Tree pair with degree signs in the column names and a space in the key's.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "geo table", spatial(lat = "lat°", lon = "lon °"))]
struct Place {
    #[model(id, column = "the key")]
    id: u64,
    #[model(column = "lat°")]
    lat: f64,
    #[model(column = "lon °")]
    lon: f64,
    name: String,
}

/// A dot in a plain table's name, which is not a schema qualifier.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "dot.ted")]
struct Dotted {
    #[model(id)]
    id: u32,
    name: String,
}

/// A to-many over keyword names on both sides, ordered by a column called `order`.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "group")]
struct Group {
    #[model(id)]
    id: u32,
    name: String,
    #[model(relation(target = Member, inverse = "group", delete = "cascade", ordered = "order"))]
    members: Many<Member>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "member; -- 👥")]
struct Member {
    #[model(id)]
    id: u32,
    name: String,
    #[model(column = "group")]
    group: One<Group>,
    #[model(column = "order")]
    order: f64,
}

/// A many-to-many whose join table carries a comment marker, over a table whose derived join
/// column (`tags 🙂_id`) is as odd as its name.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tags 🙂")]
struct Tag {
    #[model(id)]
    id: u32,
    name: String,
    #[model(relation(target = Post, join = "tag posts; -- 🙂"))]
    posts: Many<Post>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "posts")]
struct Post {
    #[model(id)]
    id: u32,
    title: String,
    #[model(relation(target = Tag, join = "tag posts; -- 🙂"))]
    tags: Many<Tag>,
}

/// A catalog someone else built, with a space in its table name and an emoji in a column,
/// read through an attached database and linked by value.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "my stations", external = "cat", fts("name 🎵"))]
struct Station {
    #[model(id, column = "rowid")]
    id: u64,
    uuid: Vec<u8>,
    #[model(column = "name 🎵")]
    name: String,
    #[model(link(target = Fav, local = "uuid", remote = "station"))]
    favs: Linked<Fav>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "favs")]
struct Fav {
    #[model(id)]
    id: u64,
    station: Vec<u8>,
    #[model(link(target = Station, local = "station", remote = "uuid"))]
    link: Linked<Station>,
}

/// A model over a table another tool made with plain, unquoted DDL.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "plain_notes")]
struct PlainNote {
    #[model(id)]
    id: u32,
    title: String,
}

// --- helpers --------------------------------------------------------------------------------

/// A fresh path under the OS temp dir; the test owns it for its lifetime.
fn temp_db(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "day-persistence-identifiers-{}-{name}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    path
}

/// A drain plus the turn-end requeries it triggers.
fn settle() {
    day_reactive::flush_sync();
    day_reactive::flush_sync();
}

fn order(id: u32, order: i64, select: &str, group: bool) -> Order {
    Order {
        id,
        order,
        select: select.into(),
        group,
        place: None,
        label: format!("label {id}"),
    }
}

fn weird(id: u64, title: &str, body: &str) -> Weird {
    Weird {
        id,
        title: title.into(),
        body: body.into(),
        japanese: id as i64 * 10,
        tick: id as f64 / 2.0,
        excluded: id.is_multiple_of(2),
        fresh: -(id as i64),
        raw: format!("raw {id}"),
    }
}

/// The names `sqlite_master` holds for `kind` (`table`, `index`, `trigger`), sorted.
fn names(c: &ModelContainer, kind: &str) -> Vec<String> {
    let mut out = Vec::new();
    c.with_connection(|conn| {
        conn.query(
            "SELECT name FROM sqlite_master WHERE type = ? ORDER BY name",
            &[Value::Text(kind.into())],
            &mut |row| {
                if let Ok(name) = row.get(0).as_text() {
                    out.push(name.to_string());
                }
            },
        )
    })
    .expect("sqlite_master");
    out
}

/// The column names `PRAGMA table_info` reports for `table`.
fn columns(c: &ModelContainer, table: &str) -> Vec<String> {
    let mut out = Vec::new();
    c.with_connection(|conn| {
        conn.query(
            &format!("PRAGMA table_info({})", ident(table)),
            &[],
            &mut |row| {
                if let Ok(name) = row.get(1).as_text() {
                    out.push(name.to_string());
                }
            },
        )
    })
    .expect("table_info");
    out
}

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
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        assert!(Instant::now() < deadline, "worker future never completed");
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

// --- the quoting itself ---------------------------------------------------------------------

#[test]
fn ident_double_quotes_and_doubles_embedded_quotes() {
    assert_eq!(ident("order"), "\"order\"");
    assert_eq!(ident("it's \"my\" notes"), "\"it's \"\"my\"\" notes\"");
    assert_eq!(ident("🚀 tab\there"), "\"🚀 tab\there\"");
    assert_eq!(ident(""), "\"\"");
}

#[test]
fn the_derive_keeps_every_name_as_written() {
    assert_eq!(Order::TABLE, "order");
    assert_eq!(Order::KEY, "primary");
    assert_eq!(Weird::TABLE, "it's \"my\" notes; -- 🚀");
    assert_eq!(Weird::KEY, "id; drop table x");
    let names: Vec<&str> = Weird::COLUMNS.iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        [
            "id; drop table x",
            "title 🧨",
            "body\twith tab",
            "日本語",
            "back`tick [bracket] 'single'",
            "excluded",
            "new",
            "raw \"literal\" \\n",
        ],
        "escapes resolve in a plain literal and stay put in a raw one"
    );
    assert_eq!(Weird::FTS_COLUMNS, ["title 🧨", "body\twith tab"]);
    assert_eq!(Weird::COMPOSITE_INDEXES, [["title 🧨", "日本語"]]);
    assert_eq!(Station::TABLE, "cat.my stations");
    assert_eq!(Station::SCHEMA, Some("cat"));
    assert_eq!(Weird::SCHEMA, None);
}

// --- keywords -------------------------------------------------------------------------------

#[test]
fn a_schema_of_keywords_opens_writes_faults_and_queries() {
    let path = temp_db("keywords");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Order]).expect("open");
        assert_eq!(names(&c, "table"), ["_day_schema", "order"]);
        assert!(
            names(&c, "index").contains(&"day_idx_order_group_select".to_string()),
            "{:?}",
            names(&c, "index")
        );
        assert_eq!(
            columns(&c, "order"),
            ["primary", "order", "select", "group", "where", "table"]
        );

        for row in [
            order(1, 30, "first", true),
            order(2, 20, "second", false),
            order(3, 10, "third", true),
        ] {
            c.insert(row);
        }
        let sql = c.record_sql(|| {}).expect("insert flush");
        // Three new rows, one multi-row upsert.
        assert_eq!(sql.len(), 1, "{sql:?}");
        assert_eq!(
            sql[0].matches("), (").count(),
            2,
            "three VALUES tuples in one statement: {sql:?}"
        );
        assert!(
            sql[0].starts_with(
                "INSERT INTO \"order\" (\"primary\", \"order\", \"select\", \"group\", \"where\", \"table\")"
            ),
            "{sql:?}"
        );
        assert!(
            sql[0].ends_with("ON CONFLICT(\"primary\") DO UPDATE SET \"order\" = excluded.\"order\", \"select\" = excluded.\"select\", \"group\" = excluded.\"group\", \"where\" = excluded.\"where\", \"table\" = excluded.\"table\""),
            "{sql:?}"
        );

        let store = c.cache::<Order>();
        let sql = c
            .record_sql(|| {
                store.elem(2).select().write("second, edited".into());
                store.elem(2).place().write(Some("here".into()));
            })
            .expect("update flush");
        assert_eq!(
            sql,
            ["UPDATE \"order\" SET \"select\" = ?, \"where\" = ? WHERE \"primary\" = ?"]
        );
        let sql = c
            .record_sql(|| {
                store.restructure("remove", Op::Delete, 3, |v| {
                    v.remove(3);
                });
            })
            .expect("delete flush");
        assert_eq!(sql, ["DELETE FROM \"order\" WHERE \"primary\" = ?"]);
    }
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Order]).expect("reopen");
        assert_eq!(c.table_count::<Order>().expect("count"), 2);
        assert_eq!(c.warm::<Order>().expect("warm"), 2);
        let two = c.get::<Order>(2u32).expect("faulted");
        assert_eq!(two.select().read(), "second, edited");
        assert_eq!(two.place().read(), Some("here".to_string()));

        let grouped = c
            .query::<Order>()
            .filter(Order::group().eq(true))
            .sort(Order::order().desc())
            .live();
        assert_eq!(grouped.ids(), [1]);
        let edited = c
            .query::<Order>()
            .filter(Order::select().contains("edited"))
            .live();
        assert_eq!(edited.ids(), [2]);
        let placed = c.query::<Order>().filter(Order::place().is_null()).live();
        assert_eq!(placed.ids(), [1]);
        let picked = c
            .query::<Order>()
            .filter(Order::order().is_in([20i64, 30]))
            .sort(Order::select().asc())
            .live();
        assert_eq!(picked.ids(), [1, 2]);
        assert_eq!(grouped.count(), 1);

        // Another connection's write to the keyword table, named the same way, merges in.
        {
            let mut other = Sqlite::at(&path).open().expect("second connection");
            other
                .execute(
                    "UPDATE \"order\" SET \"group\" = 1, \"select\" = 'from outside' WHERE \"primary\" = 2",
                    &[],
                )
                .expect("external update");
        }
        assert!(c.check_external().expect("check_external"));
        settle();
        assert_eq!(two.select().read(), "from outside");
        assert_eq!(
            grouped.ids(),
            [1, 2],
            "the live query saw the external write"
        );
    }
}

// --- hostile names --------------------------------------------------------------------------

#[test]
fn hostile_names_survive_ddl_the_fold_shadows_and_queries() {
    let path = temp_db("hostile");
    let table = Weird::TABLE;
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Weird]).expect("open");
        let tables = names(&c, "table");
        assert!(tables.contains(&table.to_string()), "{tables:?}");
        assert!(
            tables.contains(&format!("{table}_fts")),
            "the FTS shadow beside it: {tables:?}"
        );
        let indexes = names(&c, "index");
        assert!(
            indexes.contains(&format!("day_idx_{table}_日本語")),
            "{indexes:?}"
        );
        assert!(
            indexes.contains(&format!("day_idx_{table}_title 🧨_日本語")),
            "{indexes:?}"
        );
        let triggers = names(&c, "trigger");
        for suffix in ["_fts_ai", "_fts_ad", "_fts_au"] {
            assert!(
                triggers.contains(&format!("{table}{suffix}")),
                "{triggers:?}"
            );
        }
        assert_eq!(
            columns(&c, table),
            [
                "id; drop table x",
                "title 🧨",
                "body\twith tab",
                "日本語",
                "back`tick [bracket] 'single'",
                "excluded",
                "new",
                "raw \"literal\" \\n",
            ]
        );

        c.insert(weird(1, "rocket launch", "the body"));
        c.insert(weird(2, "quiet morning", "nothing to see"));
        c.insert(weird(3, "rocket science", "tab\tbody"));
        let sql = c.record_sql(|| {}).expect("insert flush");
        // Three new rows, one multi-row upsert.
        assert_eq!(sql.len(), 1, "{sql:?}");
        assert_eq!(
            sql[0].matches("), (").count(),
            2,
            "three VALUES tuples in one statement: {sql:?}"
        );
        assert!(
            sql[0].starts_with(
                "INSERT INTO \"it's \"\"my\"\" notes; -- 🚀\" (\"id; drop table x\", \"title 🧨\", \"body\twith tab\", \"日本語\", \"back`tick [bracket] 'single'\", \"excluded\", \"new\", \"raw \"\"literal\"\" \\n\")"
            ),
            "{sql:?}"
        );
        assert!(
            sql[0].contains("\"excluded\" = excluded.\"excluded\""),
            "a column called excluded, through the excluded pseudo-table: {sql:?}"
        );

        // The FTS triggers fired on insert; the index answers, and ranks.
        let rockets = c
            .query::<Weird>()
            .filter(Weird::fts().matches("rocket"))
            .sort(rank())
            .live();
        assert_eq!(rockets.count(), 2);
        let tabbed = c
            .query::<Weird>()
            .filter(Weird::fts().matches("body"))
            .live();
        assert_eq!(
            tabbed.ids(),
            [1, 3],
            "the column with a tab in its name is indexed"
        );

        // Every predicate form, on columns with quotes and a tab in their names.
        let store = c.cache::<Weird>();
        store.elem(2).title().write("rocket morning".into());
        store.elem(2).fresh().write(99);
        settle();
        assert_eq!(rockets.count(), 3, "the update trigger reindexed row 2");
        let fresh = c.query::<Weird>().filter(Weird::fresh().eq(99i64)).live();
        assert_eq!(fresh.ids(), [2]);
        let even = c
            .query::<Weird>()
            .filter(Weird::excluded().eq(true))
            .sort(Weird::japanese().desc())
            .live();
        assert_eq!(even.ids(), [2]);
        let raws = c
            .query::<Weird>()
            .filter(Weird::raw().starts_with("raw "))
            .sort(Weird::tick().desc())
            .live();
        assert_eq!(raws.ids(), [3, 2, 1]);
        let sql = c
            .record_sql(|| {
                store.restructure("remove", Op::Delete, 3, |v| {
                    v.remove(3);
                });
            })
            .expect("delete flush");
        assert_eq!(
            sql,
            ["DELETE FROM \"it's \"\"my\"\" notes; -- 🚀\" WHERE \"id; drop table x\" = ?"]
        );
        settle();
        assert_eq!(rockets.count(), 2, "the delete trigger removed row 3");
    }
    {
        // The fingerprint matches, so the reopen neither rebuilds the shadow nor loses rows.
        let c = ModelContainer::open(Sqlite::at(&path), schema![Weird]).expect("reopen");
        assert_eq!(c.table_count::<Weird>().expect("count"), 2);
        let one = c.get::<Weird>(1u64).expect("faulted");
        assert_eq!(one.title().read(), "rocket launch");
        assert_eq!(one.raw().read(), "raw 1");
        let rockets = c
            .query::<Weird>()
            .filter(Weird::fts().matches("rocket"))
            .live();
        assert_eq!(rockets.ids(), [1, 2]);
    }
}

#[test]
fn a_raw_query_over_a_hostile_table_reruns_when_the_fold_touches_it() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Weird]).expect("open");
    c.insert(weird(1, "one", ""));
    c.save().expect("seed");
    let raw = c.query_raw::<Weird>(
        format!(
            "SELECT {} FROM {} ORDER BY {}",
            ident(Weird::KEY),
            ident(Weird::TABLE),
            ident("日本語")
        ),
        vec![],
        &[Weird::TABLE],
    );
    assert_eq!(raw.ids(), [1]);
    // The fold's statement names the table through doubled quotes; the raw query's table
    // list names it plainly. The two still meet, so the query re-runs.
    c.insert(weird(2, "two", ""));
    c.save().expect("flush");
    settle();
    assert_eq!(raw.ids(), [1, 2]);
}

#[test]
fn the_worker_runs_a_hostile_schema() {
    let w = block_on(DatabaseWorker::open(|| {
        ModelContainer::open(Sqlite::memory(), schema![Weird, Order])
    }))
    .expect("worker open");
    block_on(w.write(|db| {
        db.insert(weird(7, "seven", "body"));
        db.insert(order(1, 1, "one", true));
        Ok(())
    }))
    .expect("write");
    let counts = block_on(w.read(|db| {
        Ok((
            db.table_count::<Weird>()?,
            db.table_count::<Order>()?,
            db.query::<Weird>()
                .filter(Weird::fts().matches("seven"))
                .live()
                .count(),
        ))
    }))
    .expect("read");
    assert_eq!(counts, (1, 1, 1));
    block_on(w.close()).expect("close");
}

// --- the spatial shadow ---------------------------------------------------------------------

#[test]
fn an_rtree_shadow_over_degree_sign_columns() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Place]).expect("open");
    assert!(
        names(&c, "table").contains(&"geo table_geo".to_string()),
        "{:?}",
        names(&c, "table")
    );
    for (id, lat, lon, name) in [
        (1, 35.0, 135.8, "Kyoto"),
        (2, 59.9, 10.7, "Oslo"),
        (3, 34.7, 135.5, "Osaka"),
    ] {
        c.insert(Place {
            id,
            lat,
            lon,
            name: name.into(),
        });
    }
    c.save().expect("seed");
    let japan = GeoRect {
        min_lat: 30.0,
        max_lat: 40.0,
        min_lon: 130.0,
        max_lon: 140.0,
    };
    let within = c
        .query::<Place>()
        .filter(Place::geo().within(japan))
        .sort(Place::name().asc())
        .live();
    assert_eq!(within.ids(), [1, 3]);
    // The update trigger names the columns `OF "lat°", "lon °"`: moving Oslo into the box
    // through either one re-indexes it.
    c.cache::<Place>().elem(2).lat().write(36.0);
    c.cache::<Place>().elem(2).lon().write(138.0);
    settle();
    assert_eq!(within.ids(), [1, 3, 2], "Kyoto, Osaka, then Oslo by name");
}

// --- a dot that is not a schema -------------------------------------------------------------

#[test]
fn a_dotted_table_name_is_a_plain_table_not_an_alias() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Dotted]).expect("open");
    assert!(names(&c, "table").contains(&"dot.ted".to_string()));
    c.insert(Dotted {
        id: 1,
        name: "one".into(),
    });
    c.save().expect("flush");
    // Before names were split by their declared schema, `dot` read as a detached database
    // and every query over this table answered empty.
    let all = c.query::<Dotted>().live();
    assert_eq!(all.ids(), [1]);
    assert_eq!(c.table_count::<Dotted>().expect("count"), 1);
}

// --- relations ------------------------------------------------------------------------------

#[test]
fn relations_over_keyword_tables_order_cascade_and_fold() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Group, Member]).expect("open");
    assert_eq!(
        columns(&c, "member; -- 👥"),
        ["id", "name", "group", "order"]
    );
    assert!(
        names(&c, "index").contains(&"day_idx_member; -- 👥_group".to_string()),
        "the foreign key's index: {:?}",
        names(&c, "index")
    );
    c.insert(Group {
        id: 1,
        name: "band".into(),
        ..Default::default()
    });
    c.insert(Group {
        id: 2,
        name: "crew".into(),
        ..Default::default()
    });
    for (id, name, group, order) in [(10, "a", 1, 2.0), (11, "b", 1, 1.0), (12, "c", 2, 0.0)] {
        c.insert(Member {
            id,
            name: name.into(),
            group: One::to(group),
            order,
        });
    }
    c.save().expect("seed");

    let groups = c.cache::<Group>();
    let members = c.cache::<Member>();
    assert_eq!(groups.elem(1).members().ids(), [11, 10], "by `order`");
    assert_eq!(groups.elem(2).members().ids(), [12]);

    // The fold writes the keyword foreign key by its quoted name.
    let sql = c
        .record_sql(|| {
            members.elem(12).group().write(One::to(1u32));
        })
        .expect("reparent flush");
    assert_eq!(
        sql,
        ["UPDATE \"member; -- 👥\" SET \"group\" = ? WHERE \"id\" = ?"]
    );
    assert_eq!(groups.elem(1).members().ids(), [12, 11, 10]);

    // A reopen-free lazy read goes through the relation's own SELECT.
    assert_eq!(groups.elem(1).members().count(), 3);

    // Cascade: the parent's DELETE and the children's, both quoted.
    let sql = c
        .record_sql(|| {
            groups.restructure("remove", Op::Delete, 1, |v| {
                v.remove(1);
            });
        })
        .expect("cascade flush");
    // One statement: the engine's own ON DELETE CASCADE takes the members (no undo stack
    // is installed, so nothing needs them walked in process).
    assert_eq!(sql, ["DELETE FROM \"group\" WHERE \"id\" = ?"]);
    assert_eq!(c.table_count::<Member>().expect("count"), 0);

    // The relation predicate compiles through the same quoted names.
    let crews = c
        .query::<Group>()
        .filter(Group::members().is_empty())
        .live();
    assert_eq!(crews.ids(), [2]);
}

#[test]
fn a_join_table_with_a_comment_marker_in_its_name() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Tag, Post]).expect("open");
    let join = "tag posts; -- 🙂";
    assert!(names(&c, "table").contains(&join.to_string()));
    assert_eq!(columns(&c, join), ["tags 🙂_id", "post_id"]);
    c.insert(Tag {
        id: 1,
        name: "rust".into(),
        ..Default::default()
    });
    c.insert(Post {
        id: 10,
        title: "quoting".into(),
        ..Default::default()
    });
    c.insert(Post {
        id: 11,
        title: "unrelated".into(),
        ..Default::default()
    });
    c.save().expect("seed");

    let tags = c.cache::<Tag>();
    let posts = c.cache::<Post>();
    let sql = c
        .record_sql(|| {
            assert!(tags.elem(1).posts().add(10));
        })
        .expect("link flush");
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(
        sql[0].starts_with("INSERT INTO \"tag posts; -- 🙂\" (\"tags 🙂_id\", \"post_id\")"),
        "{sql:?}"
    );
    assert_eq!(posts.elem(10).tags().ids(), [1]);
    assert!(tags.elem(1).posts().contains(10));

    let tagged = c
        .query::<Post>()
        .filter(Post::tags().any(Tag::name().eq("rust".to_string())))
        .live();
    assert_eq!(tagged.ids(), [10]);

    let sql = c
        .record_sql(|| {
            assert!(tags.elem(1).posts().remove(10));
        })
        .expect("unlink flush");
    assert_eq!(
        sql,
        ["DELETE FROM \"tag posts; -- 🙂\" WHERE \"tags 🙂_id\" = ? AND \"post_id\" = ?"]
    );
    settle();
    assert_eq!(tagged.ids(), [] as [day_model::ModelId<Post>; 0]);
}

// --- migration ------------------------------------------------------------------------------

#[test]
fn lightweight_migration_adds_and_drops_keyword_columns() {
    let path = temp_db("migrate");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Order]).expect("v1 open");
        c.insert(Order {
            place: Some("kept nowhere".into()),
            ..order(1, 5, "one", true)
        });
        c.save().expect("v1 flush");
    }
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![OrderNext]).expect("v2 open");
        assert_eq!(
            columns(&c, "order"),
            ["primary", "order", "select", "group", "table", "limit"],
            "`where` dropped, `limit` added"
        );
        let one = c
            .get::<OrderNext>(1u32)
            .expect("faulted across the migration");
        assert_eq!(one.select().read(), "one");
        assert_eq!(one.limit().read(), 0, "backfilled with the Rust default");
        one.limit().write(40);
        let sql = c.record_sql(|| {}).expect("v2 flush");
        assert_eq!(
            sql,
            ["UPDATE \"order\" SET \"limit\" = ? WHERE \"primary\" = ?"]
        );
    }
}

#[test]
fn a_table_another_tool_made_without_quotes_still_opens() {
    let path = temp_db("plain");
    {
        let mut conn = Sqlite::at(&path).open().expect("file");
        conn.execute_batch(
            "CREATE TABLE plain_notes (id INTEGER PRIMARY KEY, title TEXT NOT NULL); \
             INSERT INTO plain_notes (id, title) VALUES (1, 'made elsewhere');",
        )
        .expect("ddl");
    }
    let c = ModelContainer::open(Sqlite::at(&path), schema![PlainNote]).expect("open");
    assert_eq!(
        c.get::<PlainNote>(1u32).expect("faulted").title().read(),
        "made elsewhere"
    );
    c.insert(PlainNote {
        id: 2,
        title: "ours".into(),
    });
    c.save().expect("flush");
    assert_eq!(c.table_count::<PlainNote>().expect("count"), 2);
}

// --- undo -----------------------------------------------------------------------------------

#[test]
fn undo_writes_its_inverse_statements_with_quoted_names() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Order]).expect("open");
    c.insert(order(1, 1, "one", false));
    c.save().expect("seed");
    let stack = c.undo(10);
    let store = c.cache::<Order>();
    store.elem(1).select().write("changed".into());
    c.save().expect("edit flush");
    let sql = c
        .record_sql(|| {
            assert!(stack.undo());
        })
        .expect("undo flush");
    assert_eq!(
        sql,
        ["UPDATE \"order\" SET \"select\" = ? WHERE \"primary\" = ?"]
    );
    assert_eq!(store.elem(1).select().read(), "one");

    store.restructure("remove", Op::Delete, 1, |v| {
        v.remove(1);
    });
    c.save().expect("delete flush");
    let sql = c
        .record_sql(|| {
            assert!(stack.undo());
        })
        .expect("undo delete flush");
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(sql[0].starts_with("INSERT INTO \"order\""), "{sql:?}");
    assert_eq!(c.table_count::<Order>().expect("count"), 1);
}

// --- an attached database -------------------------------------------------------------------

const JAZZ: &[u8] = b"jazz-uuid-0001";
const NEWS: &[u8] = b"news-uuid-0002";

#[test]
fn an_attached_catalog_with_spaced_names_queries_matches_and_links() {
    let catalog = temp_db("catalog");
    {
        let mut conn = Sqlite::at(&catalog).open().expect("catalog file");
        conn.execute_batch(
            "CREATE TABLE \"my stations\" (uuid BLOB NOT NULL PRIMARY KEY, \"name 🎵\" TEXT NOT NULL); \
             CREATE VIRTUAL TABLE \"my stations_fts\" USING fts5(\"name 🎵\", content='');",
        )
        .expect("ddl");
        for (uuid, name) in [(JAZZ, "Smooth Jazz"), (NEWS, "All News")] {
            conn.execute(
                "INSERT INTO \"my stations\" (uuid, \"name 🎵\") VALUES (?, ?)",
                &[Value::Blob(uuid.to_vec()), Value::Text(name.into())],
            )
            .expect("row");
        }
        conn.execute(
            "INSERT INTO \"my stations_fts\" (rowid, \"name 🎵\") SELECT rowid, \"name 🎵\" FROM \"my stations\"",
            &[],
        )
        .expect("fts fill");
    }
    let c = ModelContainer::open(Sqlite::memory(), schema![Fav]).expect("own store");
    c.attach_database(
        "cat",
        catalog.to_str().expect("utf8 path"),
        Schema::new().with::<Station>(),
    )
    .expect("attach");

    let all = c.query::<Station>().sort(Station::name().asc()).live();
    assert_eq!(all.ids().len(), 2);
    let news = c.get::<Station>(all.ids()[0]).expect("resident");
    assert_eq!(news.name().read(), "All News");

    // `"cat"."my stations_fts"` in the FROM, `"my stations_fts" MATCH` in the WHERE.
    let jazz = c
        .query::<Station>()
        .filter(Station::fts().matches("jazz"))
        .live();
    assert_eq!(jazz.count(), 1);

    c.insert(Fav {
        id: 1,
        station: JAZZ.to_vec(),
        link: Linked::default(),
    });
    c.save().expect("favorite");
    let favorited = c
        .query::<Station>()
        .filter(Station::favs().is_empty())
        .live();
    assert_eq!(favorited.ids().len(), 1, "one station has no favorite");
    let linked = c
        .query::<Fav>()
        .filter(Fav::link().any(Station::name().contains("Jazz")))
        .live();
    assert_eq!(linked.ids(), [1]);

    c.detach_database("cat").expect("detach");
    assert!(all.ids().is_empty(), "a detached catalog answers empty");
}
