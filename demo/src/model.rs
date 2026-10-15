//! Storage operations shared by the UI and headless tests.
//! See https://daybrite.dev/docs/persistence/ for the extension's public API.
use day::prelude::*;
use day_persistence::{DbError, Model, ModelContainer, Sort, Sqlite, Value, schema};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

/// A typed row. Its generated field accessors participate in autosave and change tracking.
#[derive(Model, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[model(table = "notes")]
pub(crate) struct Note {
    #[model(id)]
    pub id: u32,
    pub title: String,
    pub done: bool,
}

/// The small lab database and a bounded SQL trace; each window owns its own connection.
pub(crate) struct Database {
    pub container: ModelContainer,
    pub trace: Rc<RefCell<VecDeque<String>>>,
}

impl Database {
    /// The same driver API runs in native SQLite and the browser worker.
    pub fn open(driver: Sqlite) -> Result<Self, DbError> {
        let trace = Rc::new(RefCell::new(VecDeque::new()));
        let sink = trace.clone();
        let container = ModelContainer::open(
            driver.trace_sql(move |sql| {
                day::log::debug!("SQL: {sql}");
                let mut lines = sink.borrow_mut();
                if lines.len() == 200 {
                    lines.pop_front();
                }
                lines.push_back(sql.to_owned());
            }),
            schema![Note],
        )?;
        Ok(Self { container, trace })
    }

    /// Enumerate the query, never the cache: the cache contains only resident rows.
    pub fn notes(&self) -> Result<Vec<Note>, DbError> {
        self.container
            .query::<Note>()
            .sort(Sort::asc("id"))
            .live()
            .try_collect()
    }

    /// Editing an existing row uses generated bindings; insertion uses the container.
    pub fn save_note(&self, note: Note) -> Result<(), DbError> {
        if let Some(row) = self.container.try_get::<Note>(note.id)? {
            row.title().write(note.title);
            row.done().write(note.done);
        } else {
            self.container.insert(note);
        }
        self.container.save()
    }

    /// A structural deletion flows through the same persistence change log as field edits.
    pub fn delete(&self, id: u32) -> Result<(), DbError> {
        let store = self.container.cache::<Note>();
        self.container.ensure::<Note>([id])?;
        store.restructure("lab-delete", day::model::Op::Delete, id as u64, |rows| {
            rows.remove(id as u64);
        });
        self.container.save()
    }

    /// Read one statement into portable JSON, retaining column names and SQLite values.
    pub fn query(&self, sql: &str) -> Result<String, DbError> {
        let mut rows = Vec::new();
        self.container.try_with_connection(|conn| {
            conn.query_named(sql, &[], &mut |names, row| {
                if rows.len() < 500 {
                    let values: serde_json::Map<String, serde_json::Value> = names
                        .iter()
                        .enumerate()
                        .map(|(i, name)| {
                            let value = match row.get(i) {
                                Value::Null => serde_json::Value::Null,
                                Value::Int(n) => n.into(),
                                Value::Real(n) => serde_json::json!(n),
                                Value::Text(s) => s.into(),
                                Value::Blob(b) => serde_json::json!(b),
                            };
                            (name.clone(), value)
                        })
                        .collect();
                    rows.push(values);
                }
            })
        })?;
        // Statements with RETURNING can write, too: refresh typed residents after raw SQL.
        self.container.rescan()?;
        serde_json::to_string_pretty(&rows).map_err(|e| DbError::driver(e.to_string()))
    }

    /// Raw writes bypass the model's change log, so explicitly invalidate its cached rows.
    pub fn execute(&self, sql: &str) -> Result<(), DbError> {
        self.container
            .try_with_connection(|conn| conn.execute_batch(sql))?;
        self.container.rescan()
    }

    /// Import validates every row before mutation, then flushes the replacement atomically.
    pub fn replace(&self, notes: Vec<Note>) -> Result<(), DbError> {
        self.container.warm::<Note>()?;
        self.container
            .cache::<Note>()
            .update("lab-import", |rows| *rows = Keyed::new(notes));
        self.container.save()
    }
}

/// Parse an exchange document before changing the database; duplicate IDs are ambiguous.
pub(crate) fn parse_notes(text: &str) -> Result<Vec<Note>, ImportError> {
    let notes: Vec<Note> = serde_json::from_str(text).map_err(ImportError::Json)?;
    let mut ids = std::collections::HashSet::new();
    for note in &notes {
        if note.id == 0 || !ids.insert(note.id) {
            return Err(ImportError::Ids);
        }
    }
    Ok(notes)
}

#[derive(Debug)]
pub(crate) enum ImportError {
    Json(serde_json::Error),
    Ids,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_edits_raw_sql_and_exchange_share_one_database() {
        let db = Database::open(Sqlite::memory()).unwrap();
        db.save_note(Note {
            id: 7,
            title: "Synthetic fixture".into(),
            done: false,
        })
        .unwrap();
        db.execute("UPDATE notes SET title='Edited fixture', done=1 WHERE id=7")
            .unwrap();
        assert_eq!(db.notes().unwrap()[0].title, "Edited fixture");
        assert!(db.notes().unwrap()[0].done);
        let export = serde_json::to_string(&db.notes().unwrap()).unwrap();
        db.delete(7).unwrap();
        assert!(db.notes().unwrap().is_empty());
        db.replace(parse_notes(&export).unwrap()).unwrap();
        assert_eq!(db.notes().unwrap()[0].id, 7);
        assert!(
            db.query("SELECT count(*) AS total FROM notes")
                .unwrap()
                .contains("\"total\": 1")
        );
        assert!(db.trace.borrow().iter().any(|s| s.contains("UPDATE")));
        assert!(db.container.integrity_check().unwrap().is_empty());
    }

    #[test]
    fn invalid_import_leaves_existing_records_untouched() {
        let db = Database::open(Sqlite::memory()).unwrap();
        db.save_note(Note {
            id: 1,
            title: "Retained fixture".into(),
            done: false,
        })
        .unwrap();
        assert!(
            parse_notes(r#"[{"id":1,"title":"a","done":false},{"id":1,"title":"b","done":true}]"#)
                .is_err()
        );
        assert!(parse_notes("malformed fixture").is_err());
        assert_eq!(db.notes().unwrap()[0].title, "Retained fixture");
    }

    #[test]
    fn named_database_survives_reopen() {
        let path = std::env::temp_dir().join(format!(
            "persistence-lab-test-{}.sqlite",
            std::process::id()
        ));
        {
            let db = Database::open(Sqlite::at(&path)).unwrap();
            db.save_note(Note {
                id: 1,
                title: "Durable fixture".into(),
                done: true,
            })
            .unwrap();
        }
        {
            let db = Database::open(Sqlite::at(&path)).unwrap();
            assert_eq!(db.notes().unwrap()[0].title, "Durable fixture");
        }
        std::fs::remove_file(path).unwrap();
    }
}
