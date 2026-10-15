//! Persistence Lab: inspect and exercise the same public APIs an ordinary Day app uses.
use day::prelude::*;
use day_persistence::Sqlite;
use std::{cell::RefCell, rc::Rc};
mod model;
use model::{Database, Note};

day::resources!();
day::day_start!(options: window(), root);

day::routes! {
    /// Stable routes shared by native navigation and the portable dayscript.
    pub(crate) enum Page {
        Database => "database", Records => "records", Sql => "sql", Exchange => "exchange", Log => "log",
    }
}

/// App identity and translations are supplied before the first UI is constructed.
pub fn window() -> day::WindowOptions {
    day::WindowOptions {
        locales: Some((res::locales::DEFAULT, res::locales::CATALOG)),
        title_fn: Some(|| res::str::app_title().format()),
        size: Size::new(1000.0, 760.0),
        ..Default::default()
    }
}

#[derive(Clone)]
struct Scene {
    db: Rc<RefCell<Database>>,
    status: Signal<String>,
    name: Signal<String>,
    id: Signal<String>,
    title: Signal<String>,
    done: Signal<bool>,
    records: Signal<String>,
    count: Signal<String>,
    sql: Signal<String>,
    results: Signal<String>,
    exchange: Signal<String>,
    trace: Signal<String>,
}

impl Scene {
    /// A fresh in-memory database is safe for exploration; durable storage is an explicit action.
    fn new() -> Result<Self, day_persistence::DbError> {
        Ok(Self {
            db: Rc::new(RefCell::new(Database::open(Sqlite::memory())?)),
            status: Signal::new(res::str::memory_open().format()),
            name: Signal::new("lab.sqlite".into()),
            id: Signal::new("1".into()),
            title: Signal::new(String::new()),
            done: Signal::new(false),
            records: Signal::new("[]".into()),
            count: Signal::new("0".into()),
            // SQL and JSON are editable data, not app-owned UI translations.
            sql: Signal::new("SELECT id, title, done FROM notes ORDER BY id".into()),
            results: Signal::new(String::new()),
            exchange: Signal::new("[]".into()),
            trace: Signal::new(String::new()),
        })
    }

    /// Refresh snapshots after commands; storage errors must never masquerade as an empty table.
    fn refresh(&self) -> Result<(), day_persistence::DbError> {
        let db = self.db.borrow();
        let notes = db.notes()?;
        self.count.set(notes.len().to_string());
        self.records.set(
            serde_json::to_string_pretty(&notes)
                .map_err(|e| day_persistence::DbError::driver(e.to_string()))?,
        );
        self.trace.set(
            db.trace
                .borrow()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
        );
        Ok(())
    }

    /// Format failures on the UI thread, while keeping detailed diagnostics in the console.
    fn finish(&self, result: Result<(), day_persistence::DbError>) {
        match result.and_then(|()| self.refresh()) {
            Ok(()) => self.status.set(res::str::operation_ok().format()),
            Err(error) => {
                day::log::warn!("persistence lab: {error}");
                self.status
                    .set(res::str::operation_failed(error.to_string()).format());
            }
        }
    }

    fn save_note(&self) {
        let Ok(id) = self.id.get_untracked().parse::<u32>() else {
            self.status.set(res::str::invalid_id().format());
            return;
        };
        if id == 0 {
            self.status.set(res::str::invalid_id().format());
            return;
        }
        let result = self.db.borrow().save_note(Note {
            id,
            title: self.title.get_untracked(),
            done: self.done.get_untracked(),
        });
        if result.is_ok() {
            day::log::info!("saved note {id}");
        }
        self.finish(result);
    }

    fn delete_note(&self) {
        let Ok(id) = self.id.get_untracked().parse::<u32>() else {
            self.status.set(res::str::invalid_id().format());
            return;
        };
        let result = self.db.borrow().delete(id);
        if result.is_ok() {
            day::log::info!("deleted note {id}");
        }
        self.finish(result);
    }

    fn open(&self, durable: bool) {
        let name = self.name.get_untracked();
        if durable
            && (name.is_empty()
                || name.contains("..")
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        {
            self.status.set(res::str::invalid_name().format());
            return;
        }
        let result = (|| {
            self.db.borrow().container.save()?;
            let driver = if durable {
                #[cfg(target_arch = "wasm32")]
                {
                    Sqlite::at(&name)
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    Sqlite::app_data(&name)?
                }
            } else {
                Sqlite::memory()
            };
            *self.db.borrow_mut() = Database::open(driver)?;
            Ok(())
        })();
        self.finish(result);
    }

    fn import(&self) {
        match model::parse_notes(&self.exchange.get_untracked()) {
            Ok(notes) => {
                let result = self.db.borrow().replace(notes);
                self.finish(result);
            }
            Err(model::ImportError::Ids) => {
                self.status.set(res::str::invalid_import_ids().format())
            }
            Err(model::ImportError::Json(error)) => self
                .status
                .set(res::str::invalid_json(error.to_string()).format()),
        }
    }
}

/// Compose one window's connection and controls: https://daybrite.dev/docs/navigation/.
pub fn root() -> AnyPiece {
    let scene = match Scene::new() {
        Ok(scene) => scene,
        Err(error) => return AnyPiece::new(label(res::str::operation_failed(error.to_string()))),
    };
    let db = scene.clone();
    let records = scene.clone();
    let sql = scene.clone();
    let exchange = scene.clone();
    let log = scene;
    AnyPiece::new(
        nav(Signal::new(Page::Database))
            .title(res::str::app_title())
            .item(Page::Database, res::str::nav_database(), move || {
                database_page(db.clone())
            })
            .item(Page::Records, res::str::nav_records(), move || {
                records_page(records.clone())
            })
            .item(Page::Sql, res::str::nav_sql(), move || {
                sql_page(sql.clone())
            })
            .item(Page::Exchange, res::str::nav_exchange(), move || {
                exchange_page(exchange.clone())
            })
            .item(Page::Log, res::str::nav_log(), move || {
                log_page(log.clone())
            })
            .id("nav")
            .grow(),
    )
}

/// All pages scroll on phones; native fields retain their editing and accessibility semantics.
fn page(status: Signal<String>, content: impl Piece) -> impl Piece {
    column((
        label(move || status.get()).id("status").padding(8.0),
        scroll(content.padding(16.0)).grow(),
    ))
    .grow()
}

fn database_page(s: Scene) -> impl Piece {
    let memory = s.clone();
    let durable = s.clone();
    let integrity = s.clone();
    let checkpoint = s.clone();
    let vacuum = s.clone();
    page(
        s.status,
        column((
            label(res::str::database_help()).max_width(640.0),
            labeled(
                res::str::database_name(),
                text_field(s.name)
                    .id("database-name")
                    .a11y(|a| a.label(res::str::database_name().format())),
            ),
            button(res::str::open_memory())
                .action(move || memory.open(false))
                .id("open-memory"),
            button(res::str::open_durable())
                .action(move || durable.open(true))
                .id("open-durable"),
            button(res::str::integrity_check())
                .action(move || {
                    let result =
                        integrity
                            .db
                            .borrow()
                            .container
                            .integrity_check()
                            .and_then(|rows| {
                                if rows.is_empty() {
                                    Ok(())
                                } else {
                                    Err(day_persistence::DbError::driver(rows.join("\n")))
                                }
                            });
                    integrity.finish(result);
                })
                .id("integrity"),
            button(res::str::checkpoint())
                .action(move || {
                    let result = checkpoint.db.borrow().container.checkpoint();
                    checkpoint.finish(result);
                })
                .id("checkpoint"),
            button(res::str::vacuum())
                .action(move || {
                    let result = vacuum.db.borrow().container.vacuum();
                    vacuum.finish(result);
                })
                .id("vacuum"),
        ))
        .spacing(12.0),
    )
}

fn records_page(s: Scene) -> impl Piece {
    let save = s.clone();
    let delete = s.clone();
    let reload = s.clone();
    let count = s.count;
    page(
        s.status,
        column((
            label(res::str::records_help()).max_width(640.0),
            labeled(
                res::str::note_id(),
                text_field(s.id)
                    .id("note-id")
                    .a11y(|a| a.label(res::str::note_id().format())),
            ),
            labeled(
                res::str::note_title(),
                text_field(s.title)
                    .id("note-title")
                    .a11y(|a| a.label(res::str::note_title().format())),
            ),
            labeled(
                res::str::note_done(),
                toggle(s.done)
                    .id("note-done")
                    .a11y(|a| a.label(res::str::note_done().format())),
            ),
            button(res::str::save_note())
                .action(move || save.save_note())
                .id("save-note"),
            button(res::str::delete_note())
                .action(move || delete.delete_note())
                .id("delete-note"),
            button(res::str::refresh())
                .action(move || reload.finish(Ok(())))
                .id("refresh-records"),
            labeled(
                res::str::record_count(),
                label(move || count.get())
                    .id("record-count")
                    .a11y(|a| a.label(res::str::record_count().format())),
            ),
            text_area(s.records)
                .editable(false)
                .id("records")
                .height(180.0)
                .a11y(|a| a.label(res::str::records_result().format())),
        ))
        .spacing(10.0),
    )
}

fn sql_page(s: Scene) -> impl Piece {
    let query = s.clone();
    let execute = s.clone();
    page(
        s.status,
        column((
            label(res::str::sql_help()).max_width(640.0),
            text_area(s.sql)
                .spellcheck(false)
                .id("sql-input")
                .height(120.0)
                .a11y(|a| a.label(res::str::sql_input().format())),
            button(res::str::run_query())
                .action(move || {
                    let result = query
                        .db
                        .borrow()
                        .query(&query.sql.get_untracked())
                        .map(|out| query.results.set(out));
                    query.finish(result);
                })
                .id("run-query"),
            button(res::str::run_statements())
                .action(move || {
                    let result = execute.db.borrow().execute(&execute.sql.get_untracked());
                    execute.finish(result);
                })
                .id("run-statements"),
            text_area(s.results)
                .editable(false)
                .id("sql-results")
                .height(220.0)
                .a11y(|a| a.label(res::str::sql_result().format())),
        ))
        .spacing(12.0),
    )
}

fn exchange_page(s: Scene) -> impl Piece {
    let export = s.clone();
    let import = s.clone();
    page(
        s.status,
        column((
            label(res::str::exchange_help()).max_width(640.0),
            button(res::str::export_json())
                .action(move || {
                    let result = export
                        .refresh()
                        .map(|()| export.exchange.set(export.records.get_untracked()));
                    export.finish(result);
                })
                .id("export-json"),
            text_area(s.exchange)
                .spellcheck(false)
                .id("exchange-json")
                .height(220.0)
                .a11y(|a| a.label(res::str::exchange_json().format())),
            button(res::str::import_json())
                .action(move || import.import())
                .id("import-json"),
        ))
        .spacing(12.0),
    )
}

fn log_page(s: Scene) -> impl Piece {
    let refresh = s.clone();
    let clear = s.clone();
    page(
        s.status,
        column((
            label(res::str::log_help()).max_width(640.0),
            button(res::str::refresh())
                .action(move || refresh.finish(Ok(())))
                .id("refresh-log"),
            button(res::str::clear_log())
                .action(move || {
                    clear.db.borrow().trace.borrow_mut().clear();
                    clear.trace.set(String::new());
                })
                .id("clear-log"),
            text_area(s.trace)
                .editable(false)
                .id("sql-log")
                .height(300.0)
                .a11y(|a| a.label(res::str::nav_log().format())),
        ))
        .spacing(12.0),
    )
}
