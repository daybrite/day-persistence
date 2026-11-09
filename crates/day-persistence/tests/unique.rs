// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! `#[model(unique("a", "b"))]`: a multi-column UNIQUE constraint, kept as a unique index,
//! part of the fingerprint, closed by the lightweight migration in both directions.

use std::cell::RefCell;
use std::rc::Rc;

use day_persistence::{DbErrorKind, Model, ModelContainer, One, Sqlite, schema};
use day_reactive::Binding;

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "projects")]
struct Project {
    #[model(id)]
    id: u32,
    #[model(unique)]
    key: String,
}

/// Version 1: no constraint over (project, number).
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "issues")]
struct IssueV1 {
    #[model(id)]
    id: u32,
    project: One<Project>,
    number: u32,
    title: String,
}

/// Version 2: the constraint, plus a second one with awkward names.
#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(
    table = "issues",
    unique("project", "number"),
    unique("title", "release train")
)]
struct Issue {
    #[model(id)]
    id: u32,
    project: One<Project>,
    number: u32,
    title: String,
    #[model(column = "release train")]
    train: u32,
}

type Trace = Rc<RefCell<Vec<String>>>;

fn traced(driver: Sqlite) -> (Sqlite, Trace) {
    let trace: Trace = Default::default();
    let sink = trace.clone();
    (
        driver.trace_sql(move |sql| sink.borrow_mut().push(sql.to_string())),
        trace,
    )
}

fn temp_db(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "day-persistence-unique-{}-{name}.db",
        std::process::id()
    ));
    for ext in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
    path
}

fn index_names(c: &ModelContainer, table: &str) -> Vec<String> {
    let mut names = Vec::new();
    c.with_connection(|conn| {
        conn.query(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ? ORDER BY name",
            &[day_persistence::Value::Text(table.into())],
            &mut |row| names.push(row.get(0).as_text().unwrap_or("").to_string()),
        )
    })
    .expect("indexes");
    names
}

fn fingerprint(c: &ModelContainer, table: &str) -> String {
    let mut fp = String::new();
    c.with_connection(|conn| {
        conn.query(
            "SELECT fingerprint FROM _day_schema WHERE table_name = ?",
            &[day_persistence::Value::Text(table.into())],
            &mut |row| fp = row.get(0).as_text().unwrap_or("").to_string(),
        )
    })
    .expect("fingerprint");
    fp
}

fn issue(id: u32, project: u32, number: u32, title: &str, train: u32) -> Issue {
    Issue {
        id,
        project: One::to(project),
        number,
        title: title.into(),
        train,
    }
}

#[test]
fn the_derive_exposes_the_constraint_and_the_ddl_creates_a_unique_index() {
    assert_eq!(
        Issue::UNIQUE_INDEXES,
        [&["project", "number"][..], &["title", "release train"][..]]
    );
    assert!(IssueV1::UNIQUE_INDEXES.is_empty());
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Project, Issue]).expect("open");
    let ddl: Vec<String> = trace
        .borrow()
        .iter()
        .filter(|s| s.starts_with("CREATE UNIQUE INDEX"))
        .cloned()
        .collect();
    assert!(
        ddl.contains(&"CREATE UNIQUE INDEX IF NOT EXISTS \"day_uidx_issues_project_number\" ON \"issues\"(\"project\", \"number\")".to_string()),
        "{ddl:?}"
    );
    assert!(
        ddl.contains(&"CREATE UNIQUE INDEX IF NOT EXISTS \"day_uidx_issues_title_release train\" ON \"issues\"(\"title\", \"release train\")".to_string()),
        "quoted names survive: {ddl:?}"
    );
    assert!(index_names(&c, "issues").contains(&"day_uidx_issues_project_number".to_string()));
}

#[test]
fn a_duplicate_pair_is_refused_at_the_flush_and_the_row_can_be_taken_back() {
    let c = ModelContainer::open(Sqlite::memory(), schema![Project, Issue]).expect("open");
    c.set_autosave(false);
    c.insert(Project {
        id: 1,
        key: "A".into(),
    });
    c.insert(Project {
        id: 2,
        key: "B".into(),
    });
    c.insert(issue(1, 1, 1, "one", 1));
    c.insert(issue(2, 1, 2, "two", 1));
    c.insert(issue(3, 2, 1, "one", 2)); // same number, other project; same title, other train
    c.save().expect("distinct pairs");

    c.insert(issue(4, 1, 1, "four", 1));
    let err = c.save().expect_err("duplicate (project, number)");
    assert_eq!(err.kind, DbErrorKind::Driver);
    assert!(
        err.to_string().contains("UNIQUE constraint failed"),
        "{err}"
    );
    // The failed insert stays dirty for retry; taking the row back clears it.
    c.delete::<Issue>(4u32).expect("take back");
    c.save().expect("clean again");
    assert_eq!(c.table_count::<Issue>().expect("count"), 3);

    // The second constraint, over the awkwardly named column.
    c.insert(issue(5, 2, 7, "one", 1));
    let err = c.save().expect_err("duplicate (title, release train)");
    assert!(
        err.to_string().contains("UNIQUE constraint failed"),
        "{err}"
    );
    c.delete::<Issue>(5u32).expect("take back");
    c.save().expect("clean again");

    // An update that walks into the constraint is refused too.
    c.get::<Issue>(2u32).unwrap().number().write(1);
    let err = c.save().expect_err("update into a duplicate");
    assert!(
        err.to_string().contains("UNIQUE constraint failed"),
        "{err}"
    );
    c.get::<Issue>(2u32).unwrap().number().write(2);
    c.save().expect("walked back");
}

#[test]
fn the_constraint_is_part_of_the_fingerprint() {
    let path = temp_db("fingerprint");
    let before = {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Project, IssueV1]).expect("v1");
        fingerprint(&c, "issues")
    };
    let after = {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Project, Issue]).expect("v2");
        fingerprint(&c, "issues")
    };
    assert_ne!(before, after, "declaring the constraint changes the schema");
}

#[test]
fn migration_adds_the_index_to_an_existing_table_and_drops_it_when_undeclared() {
    let path = temp_db("migrate");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Project, IssueV1]).expect("v1");
        c.insert(Project {
            id: 1,
            key: "A".into(),
        });
        for (id, number) in [(1u32, 1u32), (2, 2), (3, 3)] {
            c.insert(IssueV1 {
                id,
                project: One::to(1u32),
                number,
                title: format!("t{id}"),
            });
        }
        c.save().expect("seed");
        assert!(
            !index_names(&c, "issues")
                .iter()
                .any(|n| n.starts_with("day_uidx_")),
            "v1 has no unique index"
        );
    }
    {
        let (driver, trace) = traced(Sqlite::at(&path));
        let c = ModelContainer::open(driver, schema![Project, Issue]).expect("v2 opens");
        let sql = trace.borrow();
        assert!(
            sql.iter()
                .any(|s| s.starts_with("ALTER TABLE \"issues\" ADD COLUMN \"release train\"")),
            "the new column arrived lightweight: {sql:?}"
        );
        assert!(
            sql.iter().any(|s| s
                .contains("CREATE UNIQUE INDEX IF NOT EXISTS \"day_uidx_issues_project_number\"")),
            "{sql:?}"
        );
        assert!(
            !sql.iter().any(|s| s.contains("CREATE TABLE \"issues\"")),
            "no rebuild for an index and a column: {sql:?}"
        );
        drop(sql);
        let names = index_names(&c, "issues");
        assert!(names.contains(&"day_uidx_issues_project_number".to_string()));
        assert!(names.contains(&"day_uidx_issues_title_release train".to_string()));
        assert_eq!(c.table_count::<Issue>().expect("count"), 3, "rows intact");
    }
    {
        // Back to v1: the index Day created and the declaration no longer names is dropped.
        let c =
            ModelContainer::open(Sqlite::at(&path), schema![Project, IssueV1]).expect("v1 again");
        let names = index_names(&c, "issues");
        assert!(
            !names.iter().any(|n| n.starts_with("day_uidx_")),
            "dropped: {names:?}"
        );
        assert_eq!(c.table_count::<IssueV1>().expect("count"), 3);
    }
}

#[test]
fn migration_onto_duplicate_data_fails_at_open_and_leaves_the_file_alone() {
    let path = temp_db("duplicates");
    {
        let c = ModelContainer::open(Sqlite::at(&path), schema![Project, IssueV1]).expect("v1");
        c.insert(Project {
            id: 1,
            key: "A".into(),
        });
        for id in [1u32, 2] {
            c.insert(IssueV1 {
                id,
                project: One::to(1u32),
                number: 1, // the pair repeats
                title: format!("t{id}"),
            });
        }
        c.save().expect("seed");
    }
    let err = match ModelContainer::open(Sqlite::at(&path), schema![Project, Issue]) {
        Ok(_) => panic!("the unique index cannot be built over duplicates"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("UNIQUE constraint failed"),
        "{err}"
    );
    // The old shape still opens, with its rows.
    let c = ModelContainer::open(Sqlite::at(&path), schema![Project, IssueV1]).expect("v1 again");
    assert_eq!(c.table_count::<IssueV1>().expect("count"), 2);
    assert!(
        !index_names(&c, "issues")
            .iter()
            .any(|n| n.starts_with("day_uidx_")),
        "the failed migration left no index behind"
    );
}
