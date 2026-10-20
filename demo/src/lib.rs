//! Persistence Lab: inspect and exercise the same public APIs an ordinary Day app uses.
use day::prelude::*;
use day_persistence::Sqlite;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
mod model;
use model::{Database, Note, Trace};

day::resources!();
day::day_start!(options: window(), root);

day::routes! {
    /// Stable routes shared by native navigation and the portable dayscript.
    pub(crate) enum Page {
        Database => "database", Records => "records", Sql => "sql", Exchange => "exchange",
    }
}

/// App identity and translations are supplied before the first UI is constructed.
pub fn window() -> day::WindowOptions {
    res::window_options().size(1000.0, 760.0)
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
    trace: Trace,
    /// Whether the SQL log shows under the page; the toolbar toggle flips it.
    console: Signal<bool>,
    /// The page's share of a page split, kept across pages so the divider stays put.
    console_share: Signal<f64>,
}

impl Scene {
    /// A fresh in-memory database is safe for exploration; durable storage is an explicit action.
    fn new() -> Result<Self, day_persistence::DbError> {
        let trace = Signal::new(VecDeque::new());
        Ok(Self {
            db: Rc::new(RefCell::new(Database::open(Sqlite::memory(), trace)?)),
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
            trace,
            console: Signal::new(true),
            console_share: Signal::new(0.6),
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
            *self.db.borrow_mut() = Database::open(driver, self.trace)?;
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
    let exchange = scene;
    AnyPiece::new(
        nav(Signal::new(Page::Database))
            .title(res::str::app_title())
            .item(Page::Database, res::str::nav_database(), move || {
                page(db.clone(), database_page)
            })
            .item(Page::Records, res::str::nav_records(), move || {
                page(records.clone(), records_page)
            })
            .item(Page::Sql, res::str::nav_sql(), move || {
                page(sql.clone(), sql_page)
            })
            .item(Page::Exchange, res::str::nav_exchange(), move || {
                page(exchange.clone(), exchange_page)
            })
            .id("nav")
            .grow(),
    )
}

/// The status line over a page's controls, which scroll on phones, and the SQL log under them
/// in a split while the console is showing. The toggle rides each page, beside the log it
/// shows and hides.
fn page<P: Piece>(s: Scene, controls: fn(Scene) -> P) -> impl Piece {
    let status = s.status;
    let shown = s.console;
    let alone = s.clone();
    column((
        label(move || status.get()).id("status").padding(8.0),
        when(
            move || shown.get(),
            move || {
                split(
                    scroll(controls(s.clone()).padding(16.0)),
                    console(s.clone()),
                )
                .axis(SplitAxis::Vertical)
                .fraction(s.console_share)
                .id("console-split")
                .grow()
            },
        )
        .otherwise(move || scroll(controls(alone.clone()).padding(16.0)).grow()),
    ))
    .grow()
    .toolbar(
        toolbar_toggle("toggle-console", res::str::console_toggle(), shown)
            .image(res::vectors::console)
            .tooltip(res::str::console_toggle_help())
            .placement(ToolbarPlacement::Primary),
    )
}

/// The SQL log as a console: each statement the engine runs appends a line, and the view
/// follows the newest one.
fn console(s: Scene) -> impl Piece {
    let trace = s.trace;
    let follow = Signal::new(None);
    // Tracked on the trace, so every appended statement scrolls the log to its end. The jump
    // waits for the turn to settle, when the log's new lines have been laid out.
    Effect::new(move || {
        trace.track();
        day::reactive::at_turn_end(move || follow.set(Some(ScrollTarget::Bottom)));
    });
    column((
        row((
            label(res::str::console_title()).font(Font::Headline),
            spacer(),
            button(res::str::clear_log())
                .action(move || trace.update(VecDeque::clear))
                .id("clear-log"),
        ))
        .spacing(8.0),
        scroll(
            when(
                move || trace.with(VecDeque::is_empty),
                || {
                    label(res::str::console_empty())
                        .font(Font::Footnote)
                        .secondary()
                        .id("sql-log-empty")
                },
            )
            .otherwise(move || {
                label(move || trace.with(|lines| Vec::from(lines.clone()).join("\n")))
                    .font(Font::Footnote)
                    .monospace()
                    .id("sql-log")
                    .a11y(|a| a.label(res::str::console_title().format()))
            }),
        )
        .scroll_target(follow)
        .grow(),
    ))
    .spacing(8.0)
    .padding(12.0)
    .grow()
}

fn database_page(s: Scene) -> impl Piece {
    let memory = s.clone();
    let durable = s.clone();
    let integrity = s.clone();
    let checkpoint = s.clone();
    let vacuum = s.clone();
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
                let result = integrity
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
    .spacing(12.0)
}

fn records_page(s: Scene) -> impl Piece {
    let save = s.clone();
    let delete = s.clone();
    let reload = s.clone();
    let count = s.count;
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
    .spacing(10.0)
}

fn sql_page(s: Scene) -> impl Piece {
    let query = s.clone();
    let execute = s.clone();
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
    .spacing(12.0)
}

fn exchange_page(s: Scene) -> impl Piece {
    let export = s.clone();
    let import = s.clone();
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
    .spacing(12.0)
}
