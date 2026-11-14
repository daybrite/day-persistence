//! Persistence Lab: inspect and exercise the same public APIs an ordinary Day app uses.
use day::ImageSource;
use day::prelude::*;
use day_persistence::{BlobField, Sort, Sqlite};
use std::{cell::RefCell, collections::VecDeque, rc::Rc, sync::Arc};
mod model;
use model::{
    Database, LazyPhoto, LazyPhotoFields, Note, Photo, PhotoFields, Picture, StatementCount, Trace,
};

day::resources!();
day::day_start!(options: window(), root);

day::routes! {
    /// Stable routes shared by native navigation and the portable dayscript.
    pub(crate) enum Page {
        Database => "database", Records => "records", Sql => "sql", Exchange => "exchange",
        Gallery => "gallery",
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
    /// Every statement the engine ran, counted: what the gallery compares eager and lazy by.
    statements: StatementCount,
    /// Whether the gallery shows the lazy table (the default) or the eager one.
    gallery_lazy: Signal<bool>,
    /// How many pictures the gallery holds, refreshed after a seed and when the page opens.
    gallery_count: Signal<String>,
    /// Whether the SQL log shows under the page; the toolbar toggle flips it.
    console: Signal<bool>,
    /// The page's share of a page split, kept across pages so the divider stays put.
    console_share: Signal<f64>,
}

impl Scene {
    /// A fresh in-memory database is safe for exploration; durable storage is an explicit action.
    fn new() -> Result<Self, day_persistence::DbError> {
        let trace = Signal::new(VecDeque::new());
        let statements = Signal::new(0);
        Ok(Self {
            db: Rc::new(RefCell::new(Database::open(
                Sqlite::memory(),
                trace,
                statements,
            )?)),
            status: Signal::new(res::str::memory_open().format()),
            name: Signal::new("lab.db".into()),
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
            statements,
            gallery_lazy: Signal::new(true),
            gallery_count: Signal::new("0".into()),
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
            *self.db.borrow_mut() = Database::open(driver, self.trace, self.statements)?;
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
                self.status.set(res::str::invalid_import_ids().format());
            }
            Err(model::ImportError::Json(error)) => self
                .status
                .set(res::str::invalid_json(error.to_string()).format()),
        }
    }
}

impl Scene {
    /// Bring the gallery's count up to date from the table.
    fn refresh_gallery_count(&self) {
        let count = self.db.borrow().gallery_count();
        match count {
            Ok(n) => self.gallery_count.set(n.to_string()),
            Err(error) => self
                .status
                .set(res::str::operation_failed(error.to_string()).format()),
        }
    }

    /// Store pictures in both gallery tables and report.
    fn store_pictures(&self, pictures: Vec<Picture>) {
        let stored = self.db.borrow().seed_gallery(pictures);
        match stored {
            Ok(n) => self.status.set(res::str::seed_done(n as i64).format()),
            Err(error) => self
                .status
                .set(res::str::operation_failed(error.to_string()).format()),
        }
        self.refresh_gallery_count();
    }

    /// The offline seed: distinct PNGs drawn here, so the gallery works without a network and
    /// the dayscript never reaches for one.
    fn seed_generated(&self) {
        self.store_pictures(model::generated_pictures(GALLERY_SIZE));
    }

    /// The online seed: sample photos from picsum.photos, one request each, with the
    /// generated pictures as the fallback when the network says no.
    fn seed_download(&self) {
        let scene = self.clone();
        day::task(async move {
            let mut pictures = Vec::with_capacity(GALLERY_SIZE);
            for i in 0..GALLERY_SIZE {
                scene.status.set(
                    res::str::seed_downloading(res::str::SeedDownloading {
                        done: i as i64,
                        total: GALLERY_SIZE as i64,
                    })
                    .format(),
                );
                let url = format!("https://picsum.photos/seed/persistence-lab-{i}/160/160.jpg");
                let request = day_part_http::Request::get(&url);
                let reason = match day_part_http::fetch_future(request).await {
                    Ok(response)
                        if (200..300).contains(&response.status) && !response.body.is_empty() =>
                    {
                        pictures.push(Picture {
                            title: format!("Sample photo {}", i + 1),
                            source: url,
                            bytes: response.body,
                        });
                        continue;
                    }
                    Ok(response) => format!("HTTP {}", response.status),
                    Err(error) => error.to_string(),
                };
                scene.store_pictures(model::generated_pictures(GALLERY_SIZE));
                scene.status.set(res::str::seed_failed(reason).format());
                return;
            }
            scene.store_pictures(pictures);
        });
    }
}

/// How many pictures a seed stores.
const GALLERY_SIZE: usize = 120;

/// Compose one window's connection and controls: <https://daybrite.dev/docs/navigation/>.
pub fn root() -> AnyPiece {
    let scene = match Scene::new() {
        Ok(scene) => scene,
        Err(error) => return AnyPiece::new(label(res::str::operation_failed(error.to_string()))),
    };
    let db = scene.clone();
    let records = scene.clone();
    let sql = scene.clone();
    let gallery = scene.clone();
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
            .item(Page::Gallery, res::str::nav_gallery(), move || {
                page(gallery.clone(), gallery_page)
            })
            .id(day::id!(nav))
            .grow(Dimension::Both),
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
        label(move || status.get())
            .id(day::id!(status))
            .padding(8.0),
        when(
            move || shown.get(),
            move || {
                split(
                    scroll(controls(s.clone()).padding(16.0)),
                    console(s.clone()),
                )
                .axis(SplitAxis::Vertical)
                .fraction(s.console_share)
                .id(day::id!(console_split))
                .grow(Dimension::Both)
            },
        )
        .otherwise(move || scroll(controls(alone.clone()).padding(16.0)).grow(Dimension::Both)),
    ))
    .grow(Dimension::Both)
    .toolbar(
        toolbar_toggle(day::id!(toggle_console), res::str::console_toggle(), shown)
            .image(res::vectors::console)
            .tooltip(res::str::console_toggle_help())
            .placement(ToolbarPlacement::Primary),
    )
}

/// The SQL log as a console: each statement the engine runs appends a line, and the view
/// follows the newest one.
#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
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
                .id(day::id!(clear_log)),
        ))
        .spacing(8.0),
        scroll(
            when(
                move || trace.with(VecDeque::is_empty),
                || {
                    label(res::str::console_empty())
                        .font(Font::Footnote)
                        .secondary()
                        .id(day::id!(sql_log_empty))
                },
            )
            .otherwise(move || {
                label(move || trace.with(|lines| Vec::from(lines.clone()).join("\n")))
                    .font(Font::Footnote)
                    .monospace()
                    .id(day::id!(sql_log))
                    .a11y(|a| a.label(res::str::console_title().format()))
            }),
        )
        .scroll_target(follow)
        .grow(Dimension::Both),
    ))
    .spacing(8.0)
    .padding(12.0)
    .grow(Dimension::Both)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
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
                .id(day::id!(database_name))
                .a11y(|a| a.label(res::str::database_name().format())),
        ),
        button(res::str::open_memory())
            .action(move || memory.open(false))
            .id(day::id!(open_memory)),
        button(res::str::open_durable())
            .action(move || durable.open(true))
            .id(day::id!(open_durable)),
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
            .id(day::id!(integrity)),
        button(res::str::checkpoint())
            .action(move || {
                let result = checkpoint
                    .db
                    .borrow()
                    .container
                    .checkpoint()
                    .and_then(|done| {
                        if done.is_complete() {
                            Ok(())
                        } else {
                            Err(day_persistence::DbError::driver(
                                res::str::checkpoint_busy(res::str::CheckpointBusy {
                                    folded: done.checkpointed_frames,
                                    frames: done.log_frames,
                                })
                                .format(),
                            ))
                        }
                    });
                checkpoint.finish(result);
            })
            .id(day::id!(checkpoint)),
        button(res::str::vacuum())
            .action(move || {
                let result = vacuum.db.borrow().container.vacuum();
                vacuum.finish(result);
            })
            .id(day::id!(vacuum)),
    ))
    .spacing(12.0)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
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
                .id(day::id!(note_id))
                .a11y(|a| a.label(res::str::note_id().format())),
        ),
        labeled(
            res::str::note_title(),
            text_field(s.title)
                .id(day::id!(note_title))
                .a11y(|a| a.label(res::str::note_title().format())),
        ),
        labeled(
            res::str::note_done(),
            toggle(s.done)
                .id(day::id!(note_done))
                .a11y(|a| a.label(res::str::note_done().format())),
        ),
        button(res::str::save_note())
            .action(move || save.save_note())
            .id(day::id!(save_note)),
        button(res::str::delete_note())
            .action(move || delete.delete_note())
            .id(day::id!(delete_note)),
        button(res::str::refresh())
            .action(move || reload.finish(Ok(())))
            .id(day::id!(refresh_records)),
        labeled(
            res::str::record_count(),
            label(move || count.get())
                .id(day::id!(record_count))
                .a11y(|a| a.label(res::str::record_count().format())),
        ),
        text_area(s.records)
            .editable(false)
            .id(day::id!(records))
            .height(180.0)
            .a11y(|a| a.label(res::str::records_result().format())),
    ))
    .spacing(10.0)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
fn sql_page(s: Scene) -> impl Piece {
    let query = s.clone();
    let execute = s.clone();
    column((
        label(res::str::sql_help()).max_width(640.0),
        text_area(s.sql)
            .spellcheck(false)
            .id(day::id!(sql_input))
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
            .id(day::id!(run_query)),
        button(res::str::run_statements())
            .action(move || {
                let result = execute.db.borrow().execute(&execute.sql.get_untracked());
                execute.finish(result);
            })
            .id(day::id!(run_statements)),
        text_area(s.results)
            .editable(false)
            .id(day::id!(sql_results))
            .height(220.0)
            .a11y(|a| a.label(res::str::sql_result().format())),
    ))
    .spacing(12.0)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
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
            .id(day::id!(export_json)),
        text_area(s.exchange)
            .spellcheck(false)
            .id(day::id!(exchange_json))
            .height(220.0)
            .a11y(|a| a.label(res::str::exchange_json().format())),
        button(res::str::import_json())
            .action(move || import.import())
            .id(day::id!(import_json)),
    ))
    .spacing(12.0)
}

/// Eager against lazy, side by side: the same pictures in two tables, one list at a time,
/// with the counters that tell them apart.
#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
fn gallery_page(s: Scene) -> impl Piece {
    s.refresh_gallery_count();
    let seed = s.clone();
    let download = s.clone();
    let reset = s.clone();
    let stats = s.db.clone();
    let memo = s.db.clone();
    let eager_bytes = s.db.clone();
    let eager = s.db.clone();
    let lazy_db = s.db.clone();
    let lazy = s.gallery_lazy;
    let statements = s.statements;
    let count = s.gallery_count;
    column((
        label(res::str::gallery_help()).max_width(640.0),
        row((
            button(res::str::seed_generated())
                .action(move || seed.seed_generated())
                .id(day::id!(seed_generated)),
            button(res::str::seed_download())
                .action(move || download.seed_download())
                .id(day::id!(seed_download)),
            button(res::str::reset_counters())
                .action(move || reset.statements.set(0))
                .id(day::id!(reset_counters)),
        ))
        .spacing(8.0),
        labeled(
            res::str::gallery_count(),
            label(move || count.get())
                .id(day::id!(gallery_count))
                .a11y(|a| a.label(res::str::gallery_count().format())),
        ),
        labeled(
            res::str::gallery_lazy(),
            toggle(lazy)
                .id(day::id!(gallery_lazy))
                .a11y(|a| a.label(res::str::gallery_lazy().format())),
        ),
        labeled(
            res::str::gallery_statements(),
            label(move || statements.get().to_string())
                .id(day::id!(gallery_statements))
                .a11y(|a| a.label(res::str::gallery_statements().format())),
        ),
        labeled(
            res::str::gallery_resident(),
            label(move || {
                // Re-read after every statement: a fault or a flush changes what is resident.
                statements.track();
                let db = stats.borrow();
                format!(
                    "{} / {}",
                    db.container
                        .cache::<Photo>()
                        .with_untracked(day::model::Keyed::len),
                    db.container
                        .cache::<LazyPhoto>()
                        .with_untracked(day::model::Keyed::len)
                )
            })
            .id(day::id!(gallery_resident))
            .a11y(|a| a.label(res::str::gallery_resident().format())),
        ),
        labeled(
            res::str::gallery_eager_bytes(),
            label(move || {
                statements.track();
                let db = eager_bytes.borrow();
                res::str::gallery_kib((db.eager_resident_bytes() / 1024) as i64).format()
            })
            .id(day::id!(gallery_eager_bytes))
            .a11y(|a| a.label(res::str::gallery_eager_bytes().format())),
        ),
        labeled(
            res::str::gallery_memo(),
            label(move || {
                statements.track();
                let db = memo.borrow();
                res::str::gallery_memo_value(res::str::GalleryMemoValue {
                    kib: (db.container.blob_cache_bytes() / 1024) as i64,
                    count: db.container.blob_cache_len() as i64,
                })
                .format()
            })
            .id(day::id!(gallery_memo))
            .a11y(|a| a.label(res::str::gallery_memo().format())),
        ),
        when(move || lazy.get(), move || lazy_gallery(lazy_db.clone()))
            .otherwise(move || eager_gallery(eager.clone())),
    ))
    .spacing(10.0)
}

/// The lazy table: a row fault reads the title and size, and the picture's bytes come from
/// the blob column when the row is shown, through the memo once seen.
#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
fn lazy_gallery(db: Rc<RefCell<Database>>) -> impl Piece {
    let query = db
        .borrow()
        .container
        .query::<LazyPhoto>()
        .sort(Sort::asc("id"))
        .live();
    column((
        label(res::str::gallery_lazy_title()).font(Font::Headline),
        list(query, |slot: ModelSlot<LazyPhoto>| {
            row((
                image(move || ImageSource::Bytes(slot.bytes().bytes().unwrap_or_default()))
                    .frame(64.0, 64.0),
                column((
                    label(move || slot.title().read()),
                    label(move || format!("{} bytes", slot.size_bytes().read()))
                        .font(Font::Footnote)
                        .secondary(),
                ))
                .spacing(2.0),
            ))
            .spacing(12.0)
        })
        .row_height(RowHeight::Uniform(76.0))
        .id(day::id!(gallery_list_lazy))
        .height(440.0),
    ))
    .spacing(8.0)
}

/// The eager table: every faulted row carries its picture, so scrolling costs the bytes.
#[expect(
    clippy::needless_pass_by_value,
    reason = "a page builder takes its own handle, which each call site clones for it"
)]
fn eager_gallery(db: Rc<RefCell<Database>>) -> impl Piece {
    let query = db
        .borrow()
        .container
        .query::<Photo>()
        .sort(Sort::asc("id"))
        .live();
    column((
        label(res::str::gallery_eager_title()).font(Font::Headline),
        list(query, |slot: ModelSlot<Photo>| {
            row((
                image(move || ImageSource::Bytes(Arc::new(slot.bytes().read()))).frame(64.0, 64.0),
                column((
                    label(move || slot.title().read()),
                    label(move || format!("{} bytes", slot.size_bytes().read()))
                        .font(Font::Footnote)
                        .secondary(),
                ))
                .spacing(2.0),
            ))
            .spacing(12.0)
        })
        .row_height(RowHeight::Uniform(76.0))
        .id(day::id!(gallery_list_eager))
        .height(440.0),
    ))
    .spacing(8.0)
}
