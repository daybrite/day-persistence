// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Delete rules without an undo stack run in the engine: the parent's one `DELETE` carries
//! `ON DELETE CASCADE` / `SET NULL` through the subtree in the flush, and the resident rows
//! it touched are re-read afterwards. With a stack installed the walk stays in process, so
//! the history can restore what left.

use std::cell::RefCell;
use std::rc::Rc;

use day_model::{ModelId, Op};
use day_persistence::{DbErrorKind, Many, Model, ModelContainer, One, Sqlite, Value, schema};
use day_reactive::Binding;

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "projects")]
struct Project {
    #[model(id)]
    id: u32,
    name: String,
    #[model(relation(target = Issue, inverse = "project", delete = "cascade"))]
    issues: Many<Issue>,
    #[model(relation(target = Label, inverse = "project", delete = "nullify"))]
    labels: Many<Label>,
    #[model(relation(target = Person, inverse = "project", delete = "deny"))]
    members: Many<Person>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "issues")]
struct Issue {
    #[model(id)]
    id: u32,
    project: One<Project>,
    title: String,
    #[model(relation(target = Comment, inverse = "issue", delete = "cascade"))]
    comments: Many<Comment>,
    #[model(relation(target = Tag, join = "issue_tags"))]
    tags: Many<Tag>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "comments")]
struct Comment {
    #[model(id)]
    id: u32,
    issue: One<Issue>,
    body: String,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "labels")]
struct Label {
    #[model(id)]
    id: u32,
    project: Option<One<Project>>,
    name: String,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "persons")]
struct Person {
    #[model(id)]
    id: u32,
    project: One<Project>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "tags")]
struct Tag {
    #[model(id)]
    id: u32,
    name: String,
    #[model(relation(target = Issue, join = "issue_tags"))]
    issues: Many<Issue>,
}

#[derive(Model, Clone, Default, PartialEq, Debug)]
#[model(table = "nodes")]
struct Node {
    #[model(id)]
    id: u32,
    parent: Option<One<Node>>,
    #[model(relation(target = Node, inverse = "parent", delete = "cascade"))]
    children: Many<Node>,
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

fn app_statements(trace: &Trace) -> Vec<String> {
    trace
        .borrow()
        .iter()
        .filter(|s| !s.starts_with("--"))
        .cloned()
        .collect()
}

fn temp_db(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "day-persistence-cascade-{}-{name}.db",
        std::process::id()
    ));
    for ext in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(std::path::PathBuf::from(p));
    }
    path
}

fn tracker_schema() -> day_persistence::Schema {
    schema![Project, Issue, Comment, Label, Person, Tag]
}

fn raw_count(c: &ModelContainer, sql: &str) -> i64 {
    let mut n = -1;
    c.with_connection(|conn| {
        conn.query(sql, &[], &mut |row| n = row.get(0).as_int().unwrap_or(-2))
    })
    .expect("count");
    n
}

/// Two projects, `issues` issues each with three comments, two tags, two labels, one member.
fn seed(c: &ModelContainer, issues: u32) {
    c.set_autosave(false);
    for p in 1..=2u32 {
        c.insert(Project {
            id: p,
            name: format!("project {p}"),
            ..Default::default()
        });
        c.insert(Label {
            id: p,
            project: Some(One::to(p)),
            name: format!("label {p}"),
        });
        c.insert(Person {
            id: p,
            project: One::to(p),
        });
    }
    for t in 1..=2u32 {
        c.insert(Tag {
            id: t,
            name: format!("tag {t}"),
            ..Default::default()
        });
    }
    let mut next_comment = 1u32;
    for i in 1..=issues * 2 {
        let project = if i <= issues { 1 } else { 2 };
        c.insert(Issue {
            id: i,
            project: One::to(project),
            title: format!("issue {i}"),
            ..Default::default()
        });
        for _ in 0..3 {
            c.insert(Comment {
                id: next_comment,
                issue: One::to(i),
                body: "c".into(),
            });
            next_comment += 1;
        }
        let issue = c.get::<Issue>(i).expect("resident");
        issue.tags().add(1u32);
        if i % 2 == 0 {
            issue.tags().add(2u32);
        }
    }
    c.save().expect("seed");
    c.set_autosave(true);
}

// --- the engine path ----------------------------------------------------------------------------

#[test]
fn deleting_a_project_is_one_statement_and_the_engine_takes_the_subtree() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, tracker_schema()).expect("open");
    seed(&c, 300);
    // Some of the subtree is resident, some is not.
    c.ensure::<Comment>([1u32, 2, 3, 900]).expect("fault");
    let comments_of_project_1 = c
        .query::<Comment>()
        .filter(Comment::issue().any(Issue::project().is(1u32)))
        .live_count();
    assert_eq!(comments_of_project_1.get(), 900);
    let tag_issues = c.get::<Tag>(1u32).unwrap().issues().count();
    assert_eq!(tag_issues, 600);

    // The member denies: reassign first, as an app would.
    assert_eq!(
        c.delete::<Project>(1u32).unwrap_err().kind,
        DbErrorKind::Deny
    );
    c.get::<Person>(1u32)
        .unwrap()
        .project()
        .write(One::to(2u32));
    c.save().expect("reassign");

    trace.borrow_mut().clear();
    c.delete::<Project>(1u32).expect("delete");
    c.save().expect("flush");
    let sql = app_statements(&trace);
    let deletes: Vec<&String> = sql.iter().filter(|s| s.starts_with("DELETE")).collect();
    assert_eq!(
        deletes,
        [&"DELETE FROM \"projects\" WHERE \"id\" = 1".to_string()],
        "one DELETE; the engine cascades 300 issues, 900 comments and 450 memberships: {sql:?}"
    );
    assert!(
        sql.len() < 30,
        "no per-row reads for a cascade the engine runs: {} statements",
        sql.len()
    );

    // The file: everything under project 1 is gone, nothing else moved.
    assert_eq!(raw_count(&c, "SELECT COUNT(*) FROM \"issues\""), 300);
    assert_eq!(raw_count(&c, "SELECT COUNT(*) FROM \"comments\""), 900);
    assert_eq!(raw_count(&c, "SELECT COUNT(*) FROM \"issue_tags\""), 450);
    assert_eq!(c.foreign_key_check().expect("fk").len(), 0);
    // The caches: the resident comments of project 1 were re-read away, the other stayed.
    let comments = c.cache::<Comment>();
    assert!(comments.with_untracked(|k| k.get(1).is_none()));
    assert!(comments.with_untracked(|k| k.get(2).is_none()));
    assert!(comments.with_untracked(|k| k.get(900).is_none()));
    assert!(c.try_get::<Issue>(1u32).unwrap().is_none());
    // The label was nullified, in the file and in memory.
    assert_eq!(c.get::<Label>(1u32).unwrap().project().peek(), None);
    assert_eq!(
        raw_count(
            &c,
            "SELECT COUNT(*) FROM \"labels\" WHERE \"project\" IS NULL"
        ),
        1
    );
    // Live queries and relation views followed.
    assert_eq!(comments_of_project_1.get(), 0);
    assert_eq!(c.get::<Tag>(1u32).unwrap().issues().count(), 300);
    assert_eq!(c.get::<Project>(2u32).unwrap().issues().count(), 300);
}

#[test]
fn a_deep_self_referential_chain_goes_in_one_statement() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, schema![Node]).expect("open");
    c.set_autosave(false);
    for id in 1..=300u32 {
        c.insert(Node {
            id,
            parent: (id > 1).then(|| One::to(id - 1)),
            ..Default::default()
        });
    }
    c.save().expect("seed");
    c.ensure::<Node>([150u32, 300]).expect("fault");
    trace.borrow_mut().clear();
    c.delete::<Node>(1u32).expect("delete");
    c.save().expect("flush");
    let sql = app_statements(&trace);
    assert_eq!(
        sql.iter().filter(|s| s.starts_with("DELETE")).count(),
        1,
        "{sql:?}"
    );
    assert!(sql.len() < 12, "{sql:?}");
    assert_eq!(c.table_count::<Node>().expect("count"), 0);
    assert_eq!(c.cache::<Node>().with_untracked(|k| k.len()), 0);
}

#[test]
fn children_inserted_in_the_same_turn_follow_their_parent() {
    // The engine never saw these rows, so the flush must not write them against a parent
    // its own DELETE removes (that was a FOREIGN KEY failure at COMMIT).
    let c = ModelContainer::open(Sqlite::memory(), tracker_schema()).expect("open");
    c.set_autosave(false);
    c.insert(Project {
        id: 9,
        name: "fresh".into(),
        ..Default::default()
    });
    for i in 1..=3u32 {
        c.insert(Issue {
            id: 90 + i,
            project: One::to(9u32),
            title: "new".into(),
            ..Default::default()
        });
        c.insert(Comment {
            id: 90 + i,
            issue: One::to(90 + i),
            body: "new".into(),
        });
    }
    c.insert(Label {
        id: 9,
        project: Some(One::to(9u32)),
        name: "pending".into(),
    });
    c.delete::<Project>(9u32).expect("delete");
    c.save().expect("flush");
    assert_eq!(c.table_count::<Project>().expect("count"), 0);
    assert_eq!(c.table_count::<Issue>().expect("count"), 0);
    assert_eq!(c.table_count::<Comment>().expect("count"), 0);
    assert_eq!(
        c.table_count::<Label>().expect("count"),
        1,
        "nullified, kept"
    );
    assert_eq!(c.get::<Label>(9u32).unwrap().project().peek(), None);
    assert_eq!(c.foreign_key_check().expect("fk").len(), 0);
}

#[test]
fn an_edited_child_and_its_deleted_parent_in_one_turn() {
    let c = ModelContainer::open(Sqlite::memory(), tracker_schema()).expect("open");
    seed(&c, 2);
    let comment = c.get::<Comment>(1u32).unwrap();
    comment
        .body()
        .write("edited just before the parent went".into());
    c.delete::<Issue>(1u32).expect("delete");
    c.save()
        .expect("flush: the UPDATE runs before the DELETE, then the engine takes the row");
    assert!(c.try_get::<Comment>(1u32).unwrap().is_none());
    assert_eq!(
        raw_count(&c, "SELECT COUNT(*) FROM \"comments\" WHERE \"issue\" = 1"),
        0
    );
}

#[test]
fn a_deleted_row_reads_as_childless_and_memberless_before_the_flush() {
    let c = ModelContainer::open(Sqlite::memory(), tracker_schema()).expect("open");
    seed(&c, 2);
    c.set_autosave(false);
    let project = c.get::<Project>(1u32).unwrap();
    let issue = c.get::<Issue>(1u32).unwrap();
    assert_eq!(project.issues().count(), 2);
    assert_eq!(issue.tags().count(), 1);
    assert_eq!(c.get::<Tag>(1u32).unwrap().issues().count(), 4);
    c.delete::<Issue>(1u32).expect("delete");
    // Mid-turn: the deleted issue holds nothing, and the tag no longer lists it.
    assert_eq!(issue.tags().count(), 0);
    assert_eq!(c.get::<Tag>(1u32).unwrap().issues().count(), 3);
    c.save().expect("flush");
    assert_eq!(c.get::<Tag>(1u32).unwrap().issues().count(), 3);
    assert_eq!(project.issues().count(), 1);
}

#[test]
fn a_worker_cascade_leaves_its_caches_coherent() {
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
        ModelContainer::open(Sqlite::at(&path), tracker_schema())
    }))
    .expect("worker");
    block_on(worker.write(|db| {
        seed(db, 20);
        db.ensure::<Comment>([1u32, 2])?;
        Ok(())
    }))
    .expect("seed");
    block_on(worker.write(|db| {
        db.get::<Person>(1u32)
            .unwrap()
            .project()
            .write(One::to(2u32));
        db.save()?;
        db.delete::<Project>(1u32)
    }))
    .expect("delete");
    let (issues, comments, resident) = block_on(worker.read(|db| {
        Ok((
            db.table_count::<Issue>()?,
            db.table_count::<Comment>()?,
            db.cache::<Comment>().with_untracked(|k| k.get(1).is_some()),
        ))
    }))
    .expect("read");
    assert_eq!((issues, comments, resident), (20, 60, false));
    block_on(worker.close()).expect("close");
}

// --- the undo path ---------------------------------------------------------------------------

#[test]
fn with_an_undo_stack_the_walk_stays_in_process_and_restores_the_subtree() {
    let (driver, trace) = traced(Sqlite::memory());
    let c = ModelContainer::open(driver, tracker_schema()).expect("open");
    seed(&c, 3);
    let stack = c.undo(10);
    c.get::<Person>(1u32)
        .unwrap()
        .project()
        .write(One::to(2u32));
    c.save().expect("reassign");
    stack.clear();

    trace.borrow_mut().clear();
    c.delete::<Project>(1u32).expect("delete");
    c.save().expect("flush");
    let sql = app_statements(&trace);
    assert!(
        sql.iter()
            .any(|s| s.starts_with("DELETE FROM \"issues\" WHERE \"id\" IN")),
        "under capture the children are walked and batched in process: {sql:?}"
    );
    assert!(
        sql.iter()
            .any(|s| s.starts_with("DELETE FROM \"comments\" WHERE \"id\" IN")),
        "{sql:?}"
    );
    assert_eq!(c.table_count::<Issue>().expect("count"), 3);
    assert_eq!(c.table_count::<Comment>().expect("count"), 9);

    assert!(stack.undo());
    c.save().expect("undo flush");
    assert_eq!(c.table_count::<Project>().expect("count"), 2);
    assert_eq!(c.table_count::<Issue>().expect("count"), 6);
    assert_eq!(c.table_count::<Comment>().expect("count"), 18);
    assert_eq!(raw_count(&c, "SELECT COUNT(*) FROM \"issue_tags\""), 9);
    assert_eq!(
        c.get::<Label>(1u32).unwrap().project().peek(),
        Some(One::to(1u32))
    );
    assert_eq!(c.foreign_key_check().expect("fk").len(), 0);
}

#[test]
fn the_dirty_index_keeps_a_large_walk_from_scanning_other_stores() {
    // Under capture the walk reads the child store's dirty rows per parent; the per-store
    // index means a thousand dirty comments cost nothing to a walk over issues. This pins
    // that the walk still finishes quickly rather than its exact cost.
    let c = ModelContainer::open(Sqlite::memory(), schema![Node]).expect("open");
    c.set_autosave(false);
    for id in 1..=2_000u32 {
        c.insert(Node {
            id,
            parent: (id > 1).then(|| One::to(1u32)),
            ..Default::default()
        });
    }
    c.save().expect("seed");
    let _stack = c.undo(10);
    let t = std::time::Instant::now();
    c.delete::<Node>(1u32).expect("delete");
    c.save().expect("flush");
    assert!(
        t.elapsed() < std::time::Duration::from_secs(5),
        "a 2 000-child cascade under capture took {:?}",
        t.elapsed()
    );
    assert_eq!(c.table_count::<Node>().expect("count"), 0);
}

// --- raw doors --------------------------------------------------------------------------------

#[test]
fn a_restructure_delete_bypassing_the_checked_door_still_cascades_in_the_engine() {
    let c = ModelContainer::open(Sqlite::memory(), tracker_schema()).expect("open");
    seed(&c, 2);
    c.get::<Person>(1u32)
        .unwrap()
        .project()
        .write(One::to(2u32));
    c.save().expect("reassign");
    let projects = c.cache::<Project>();
    projects.restructure("raw delete", Op::Delete, 1, |k| {
        k.remove(ModelId::<Project>::of(1u32).handle());
    });
    c.save().expect("flush");
    assert_eq!(c.table_count::<Issue>().expect("count"), 2);
    assert_eq!(c.table_count::<Comment>().expect("count"), 6);
    c.with_connection(|conn| {
        conn.execute(
            "SELECT 1 FROM \"projects\" WHERE \"id\" = ?",
            &[Value::Int(1)],
        )
        .map(|_| ())
    })
    .expect("query");
}

// --- transaction participants ---------------------------------------------------------------

#[test]
fn with_a_participant_installed_the_change_set_lists_every_cascaded_row() {
    use day_persistence::{
        Refresh, RowChange, RowChangeKind, SqliteConnection, TransactionParticipant, WriteOrigin,
    };

    struct Journal(Rc<RefCell<Vec<(String, RowChangeKind)>>>);
    impl TransactionParticipant for Journal {
        fn begin(
            &self,
            _: &mut dyn SqliteConnection,
            _: WriteOrigin,
        ) -> Result<(), day_persistence::DbError> {
            Ok(())
        }
        fn before_commit(
            &self,
            _: &mut dyn SqliteConnection,
            _: WriteOrigin,
            changes: &[RowChange],
        ) -> Result<Refresh, day_persistence::DbError> {
            let mut seen = self.0.borrow_mut();
            for change in changes {
                seen.push((change.table.to_string(), change.kind.clone()));
            }
            Ok(Refresh::None)
        }
    }

    let c = ModelContainer::open(Sqlite::memory(), tracker_schema()).expect("open");
    seed(&c, 4);
    let seen: Rc<RefCell<Vec<(String, RowChangeKind)>>> = Default::default();
    c.install_transaction_participant(Journal(seen.clone()))
        .expect("participant");
    c.get::<Person>(1u32)
        .unwrap()
        .project()
        .write(One::to(2u32));
    c.save().expect("reassign");
    seen.borrow_mut().clear();

    c.delete::<Project>(1u32).expect("delete");
    c.save().expect("flush");
    let seen = seen.borrow();
    let deleted = |table: &str| {
        seen.iter()
            .filter(|(t, k)| t == table && *k == RowChangeKind::Deleted)
            .count()
    };
    // A journal sees the whole subtree leave, as it did before the engine path existed.
    assert_eq!(deleted("projects"), 1);
    assert_eq!(deleted("issues"), 4);
    assert_eq!(deleted("comments"), 12);
    assert_eq!(deleted("issue_tags"), 6);
    assert!(
        seen.iter()
            .any(|(t, k)| t == "labels" && matches!(k, RowChangeKind::Updated(_))),
        "the nullified label is reported as an update: {seen:?}"
    );
    assert_eq!(c.table_count::<Comment>().expect("count"), 12);
}
