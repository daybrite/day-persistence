//! Storage operations shared by the UI and headless tests.
//! See <https://daybrite.dev/docs/persistence/> for the extension's public API.
use day::prelude::*;
use day_persistence::{Blob, DbError, Model, ModelContainer, Sort, Sqlite, Value, schema};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A typed row. Its generated field accessors participate in autosave and change tracking.
#[derive(Model, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[model(table = "notes")]
pub(crate) struct Note {
    #[model(id)]
    pub id: u32,
    pub title: String,
    pub done: bool,
}

/// A gallery image with its bytes inline: every fault of the row carries the picture.
#[derive(Model, Clone, Debug, Default, PartialEq)]
#[model(table = "photos")]
pub(crate) struct Photo {
    #[model(id)]
    pub id: u32,
    pub title: String,
    /// Where the bytes came from: a URL, or `generated`.
    pub source: String,
    pub size_bytes: u32,
    pub bytes: Vec<u8>,
}

/// The same image with the bytes lazy: a fault reads the row without them, and the gallery
/// asks for the picture when the row is shown (<https://daybrite.dev/docs/persistence/>).
#[derive(Model, Clone, Debug, Default, PartialEq)]
#[model(table = "lazy_photos")]
pub(crate) struct LazyPhoto {
    #[model(id)]
    pub id: u32,
    pub title: String,
    pub source: String,
    pub size_bytes: u32,
    pub bytes: Blob,
}

/// One image the seed will store in both gallery tables.
pub(crate) struct Picture {
    pub title: String,
    pub source: String,
    pub bytes: Vec<u8>,
}

/// The latest engine statements, oldest first. A window keeps one across reopened connections,
/// so its console reads as one session, and a signal, so the console redraws per statement.
pub(crate) type Trace = Signal<VecDeque<String>>;

/// Statements the engine has run since the window opened (or the gallery reset it): the
/// number the gallery's readouts compare eager and lazy scrolling by.
pub(crate) type StatementCount = Signal<u64>;

/// How many statements a trace retains.
pub(crate) const TRACE_LIMIT: usize = 200;

/// How many characters of one statement the trace keeps.
pub(crate) const TRACE_LINE_LIMIT: usize = 600;

/// The small lab database; each window owns its own connection.
pub(crate) struct Database {
    pub container: ModelContainer,
}

impl Database {
    /// The same driver API runs in native SQLite and the browser worker.
    pub fn open(driver: Sqlite, trace: Trace, statements: StatementCount) -> Result<Self, DbError> {
        let container = ModelContainer::open(
            driver.trace_sql(move |sql| {
                // Engine-internal frames (triggers, FTS shadows) arrive as `-- …` comments.
                if !sql.starts_with("--") {
                    statements.update(|n| *n += 1);
                }
                // Blob inserts expand to large hex strings. Bound both the native log and
                // the UI console before writing: simulator stderr can block the UI
                // thread on a full pipe when it receives the entire expanded statement.
                let line = match sql.char_indices().nth(TRACE_LINE_LIMIT) {
                    Some((cut, _)) => format!("{}… ({} chars)", &sql[..cut], sql.chars().count()),
                    None => sql.to_owned(),
                };
                day::log::debug!("SQL: {line}");
                trace.update(|lines| {
                    if lines.len() == TRACE_LIMIT {
                        lines.pop_front();
                    }
                    lines.push_back(line);
                });
            }),
            schema![Note, Photo, LazyPhoto],
        )?;
        Ok(Self { container })
    }

    /// Store the pictures in both gallery tables, the same ids in each, replacing what was
    /// there, in one flush.
    pub fn seed_gallery(&self, pictures: Vec<Picture>) -> Result<usize, DbError> {
        self.container.warm::<Photo>()?;
        self.container.warm::<LazyPhoto>()?;
        let eager: Vec<Photo> = pictures
            .iter()
            .enumerate()
            .map(|(i, p)| Photo {
                id: i as u32 + 1,
                title: p.title.clone(),
                source: p.source.clone(),
                size_bytes: p.bytes.len() as u32,
                bytes: p.bytes.clone(),
            })
            .collect();
        let lazy: Vec<LazyPhoto> = pictures
            .into_iter()
            .enumerate()
            .map(|(i, p)| LazyPhoto {
                id: i as u32 + 1,
                title: p.title,
                source: p.source,
                size_bytes: p.bytes.len() as u32,
                bytes: Blob::new(p.bytes),
            })
            .collect();
        let n = eager.len();
        self.container
            .cache::<Photo>()
            .update("gallery-seed", |rows| *rows = Keyed::new(eager));
        self.container
            .cache::<LazyPhoto>()
            .update("gallery-seed", |rows| *rows = Keyed::new(lazy));
        self.container.save()?;
        Ok(n)
    }

    /// How many pictures the gallery holds.
    pub fn gallery_count(&self) -> Result<u64, DbError> {
        self.container.table_count::<LazyPhoto>()
    }

    /// Picture bytes the eager table holds in memory right now: every resident row carries its
    /// picture, which is what the lazy table's memo is compared against.
    pub fn eager_resident_bytes(&self) -> usize {
        self.container.cache::<Photo>().with_untracked(|rows| {
            rows.keys()
                .into_iter()
                .filter_map(|k| rows.get(k).map(|p| p.bytes.len()))
                .sum()
        })
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
        store.restructure(
            "lab-delete",
            day::model::Op::Delete,
            u64::from(id),
            |rows| {
                rows.remove(u64::from(id));
            },
        );
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

    fn open(driver: Sqlite) -> Database {
        Database::open(driver, Signal::new(VecDeque::new()), Signal::new(0)).unwrap()
    }

    #[test]
    fn typed_edits_raw_sql_and_exchange_share_one_database() {
        let trace = Signal::new(VecDeque::new());
        let db = Database::open(Sqlite::memory(), trace, Signal::new(0)).unwrap();
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
        assert_eq!(db.notes().unwrap(), [] as [Note; 0]);
        db.replace(parse_notes(&export).unwrap()).unwrap();
        assert_eq!(db.notes().unwrap()[0].id, 7);
        assert!(
            db.query("SELECT count(*) AS total FROM notes")
                .unwrap()
                .contains("\"total\": 1")
        );
        assert!(trace.with_untracked(|lines| lines.iter().any(|s| s.contains("UPDATE"))));
        assert_eq!(
            db.container.integrity_check().unwrap(),
            [] as [std::string::String; 0]
        );
    }

    #[test]
    fn trace_spans_reopened_connections_and_keeps_the_latest_statements() {
        let trace = Signal::new(VecDeque::new());
        let first = Database::open(Sqlite::memory(), trace, Signal::new(0)).unwrap();
        first
            .execute("CREATE TABLE trace_fixture (n INTEGER)")
            .unwrap();
        drop(first);
        let second = Database::open(Sqlite::memory(), trace, Signal::new(0)).unwrap();
        assert!(trace.with_untracked(|lines| lines.iter().any(|s| s.contains("trace_fixture"))));
        for n in 0..TRACE_LIMIT {
            second.query(&format!("SELECT {n} AS fixture")).unwrap();
        }
        trace.with_untracked(|lines| {
            assert_eq!(lines.len(), TRACE_LIMIT);
            assert!(!lines.iter().any(|s| s.contains("trace_fixture")));
            assert!(
                lines
                    .iter()
                    .any(|s| s.contains(&format!("SELECT {} AS fixture", TRACE_LIMIT - 1)))
            );
        });
    }

    #[test]
    fn blob_and_unicode_traces_are_bounded_before_logging() {
        use std::cell::RefCell;

        // Capture this thread's actual logger output, including the native-console path.
        // Bounding only the UI trace still lets expanded blobs block simulator stderr.
        thread_local! {
            static LOGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        }
        struct TraceLogger;
        impl day::log::Log for TraceLogger {
            fn enabled(&self, _: &day::log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &day::log::Record<'_>) {
                if record.target() == module_path!().trim_end_matches("::tests") {
                    LOGS.with(|logs| logs.borrow_mut().push(record.args().to_string()));
                }
            }
            fn flush(&self) {}
        }
        day::log::set_logger(&TraceLogger).unwrap();
        day::log::set_max_level(day::log::LevelFilter::Debug);

        let trace = Signal::new(VecDeque::new());
        let statements = Signal::new(0);
        let db = Database::open(Sqlite::memory(), trace, statements).unwrap();
        trace.update(VecDeque::clear);
        statements.set(0);
        LOGS.with(|logs| logs.borrow_mut().clear());

        db.seed_gallery(vec![Picture {
            title: "Large blob fixture".into(),
            source: "synthetic fixture".into(),
            bytes: vec![0xab; 1 << 20],
        }])
        .unwrap();
        db.query(&format!("SELECT '{}' AS fixture", "é".repeat(1000)))
            .unwrap();

        let lines = trace.get_untracked();
        assert!(lines.iter().any(|line| line.starts_with("INSERT")));
        assert!(lines.iter().any(|line| line.contains("ééé")));
        assert!(lines.iter().any(|line| line.contains("… (")));
        assert!(
            lines
                .iter()
                .all(|line| line.chars().count() <= TRACE_LINE_LIMIT + 32)
        );
        assert_eq!(statements.get_untracked() as usize, lines.len());
        LOGS.with(|logs| {
            let expected: Vec<_> = lines.iter().map(|line| format!("SQL: {line}")).collect();
            assert!(
                *logs.borrow() == expected,
                "the native logger must receive the same bounded statements as the UI"
            );
        });
    }

    #[test]
    fn invalid_import_leaves_existing_records_untouched() {
        let db = open(Sqlite::memory());
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
        let path =
            std::env::temp_dir().join(format!("persistence-lab-test-{}.db", std::process::id()));
        {
            let db = open(Sqlite::at(&path));
            db.save_note(Note {
                id: 1,
                title: "Durable fixture".into(),
                done: true,
            })
            .unwrap();
        }
        {
            let db = open(Sqlite::at(&path));
            assert_eq!(db.notes().unwrap()[0].title, "Durable fixture");
        }
        std::fs::remove_file(path).unwrap();
    }
}

// ---------------------------------------------------------------------------------------------
// Generated pictures: a PNG encoder small enough to live here, so the gallery seeds offline
// (and in CI) with distinct, real image files rather than placeholder bytes.
// ---------------------------------------------------------------------------------------------

/// `n` distinct pictures, each a differently coloured gradient with a disc, as PNG files.
pub(crate) fn generated_pictures(n: usize) -> Vec<Picture> {
    (0..n)
        .map(|i| {
            let hue = (i as f32 * 137.508) % 360.0;
            Picture {
                title: format!("Generated {}", i + 1),
                source: "generated".into(),
                bytes: png_disc(96, 96, hue, i as u32),
            }
        })
        .collect()
}

fn hsv(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    ]
}

/// An RGB PNG: a vertical gradient of `hue` with a lighter disc whose position follows `seed`.
fn png_disc(w: u32, h: u32, hue: f32, seed: u32) -> Vec<u8> {
    let cx = 20.0 + (seed * 37 % 56) as f32;
    let cy = 20.0 + (seed * 53 % 56) as f32;
    let mut raw = Vec::with_capacity((w * (h * 3 + 1)) as usize);
    for y in 0..h {
        raw.push(0); // filter: none
        let t = y as f32 / h as f32;
        for x in 0..w {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
            let px = if d < 18.0 {
                hsv((hue + 180.0) % 360.0, 0.25, 0.98)
            } else {
                hsv(hue, 0.55, 0.55 + 0.35 * t)
            };
            raw.extend_from_slice(&px);
        }
    }
    let mut png = Vec::with_capacity(raw.len() + 128);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB, deflate, no filter method, no interlace
    png_chunk(&mut png, *b"IHDR", &ihdr);
    png_chunk(&mut png, *b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut png, *b"IEND", &[]);
    png
}

fn png_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A zlib stream of stored (uncompressed) deflate blocks: valid everywhere, no compressor.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod png_tests {
    use super::*;

    #[test]
    fn generated_pictures_are_valid_pngs() {
        let pics = generated_pictures(3);
        assert_eq!(pics.len(), 3);
        for p in &pics {
            assert!(p.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert_eq!(&p.bytes[12..16], b"IHDR");
            assert!(p.bytes.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "IEND crc");
        }
        assert_ne!(pics[0].bytes, pics[1].bytes);
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }
}
