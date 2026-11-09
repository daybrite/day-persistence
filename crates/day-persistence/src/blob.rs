// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Lazy blob columns: a [`Blob`] field is a `BLOB` column that a row fault never reads. The
//! row arrives with the column as `NULL` (the fault `SELECT` names it as a literal `NULL`),
//! the field decodes to [`Blob::stored`], and the bytes come when asked for, one point
//! `SELECT` per read, through a memo bounded in bytes rather than rows
//! ([`ModelContainer::set_blob_cache_limit`]).
//!
//! Writes travel inside the struct: [`Blob::new`] holds the bytes as *pending* until the
//! flush writes them, after which the container settles the field back to `stored` under
//! its own author, so nothing re-dirties, and parks the bytes in the memo so the next read
//! is a hit. A pending value answers reads directly, so a write made this turn is visible
//! before it is flushed, like any other field.
//!
//! The upsert the fold emits protects a stored value: a lazy column's conflict clause is
//! `c = COALESCE(excluded.c, c)`, and an `UPDATE` carrying a `NULL` for a lazy column (a
//! stored value the row rewrote with no new bytes) skips that column, so rewriting a row's
//! other fields never erases the bytes the row already has.
//!
//! The derive places lazy columns last in a new table's DDL. SQLite stores a large value in
//! an overflow chain and a select that stops at an earlier column never reads that chain,
//! so even raw `SELECT id, filename` over the table stays cheap.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use crate::{ColumnValue, DbError, Model, ModelContainer, SqlType, Value};

/// A lazy `BLOB` column. `Default` is the stored, unread state; [`Blob::new`] carries bytes
/// to the next flush. The bytes are read through the field's accessor
/// ([`BlobField::bytes`] and friends), never through the value itself.
#[derive(Clone, Default)]
pub struct Blob {
    pending: Option<Arc<Vec<u8>>>,
}

impl Blob {
    /// Bytes to write at the next flush.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Blob {
        Blob {
            pending: Some(Arc::new(bytes.into())),
        }
    }

    /// The stored state: whatever the file holds, read on demand.
    pub const fn stored() -> Blob {
        Blob { pending: None }
    }

    /// Whether this value carries bytes the flush has not written yet.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The unflushed bytes, if any.
    pub fn pending(&self) -> Option<&Arc<Vec<u8>>> {
        self.pending.as_ref()
    }

    /// Hand the pending bytes over and become stored: what the container does after the
    /// flush that wrote them.
    pub fn take_pending(&mut self) -> Option<Arc<Vec<u8>>> {
        self.pending.take()
    }
}

impl PartialEq for Blob {
    /// Two stored values are equal (both mean "the file's bytes"); pending values compare by
    /// content; a pending and a stored value differ, since one has an unflushed write.
    fn eq(&self, other: &Self) -> bool {
        match (&self.pending, &other.pending) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

impl std::fmt::Debug for Blob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.pending {
            Some(b) => write!(f, "Blob(pending {} bytes)", b.len()),
            None => f.write_str("Blob(stored)"),
        }
    }
}

impl ColumnValue for Blob {
    const SQL_TYPE: SqlType = SqlType::Blob;
    /// Pending bytes go to the file; a stored value writes `NULL`, which the fold's upsert
    /// and update statements treat as "leave the column alone".
    fn to_sqlite_value(&self) -> Value {
        match &self.pending {
            Some(b) => Value::Blob(b.as_ref().clone()),
            None => Value::Null,
        }
    }
    /// A fault never reads the column, so whatever arrives decodes to the stored state.
    fn from_sqlite_value(_: Value) -> Result<Self, DbError> {
        Ok(Blob::stored())
    }
}

// ---------------------------------------------------------------------------------------------
// The accessor surface
// ---------------------------------------------------------------------------------------------

/// What a [`Blob`] field's accessor offers beyond the plain field: the bytes, on demand.
///
/// Every read is tracked through the field, so a view that shows the bytes re-renders when
/// they are written. A pending value (written this turn, not yet flushed) answers from
/// memory; otherwise the container answers from its byte-bounded memo or the file.
pub trait BlobField {
    /// The whole value. Empty when the column is `NULL` or the row is gone.
    fn bytes(&self) -> Result<Arc<Vec<u8>>, DbError>;
    /// The value's length in bytes, without reading it (`length(col)` in SQL).
    fn len(&self) -> Result<usize, DbError>;
    /// Whether the value is empty or `NULL`.
    fn is_empty(&self) -> Result<bool, DbError> {
        Ok(self.len()? == 0)
    }
    /// `range` of the value, clamped to its length: `substr(col, …)` in SQL, so only the
    /// requested bytes cross from the engine.
    fn read_range(&self, range: Range<usize>) -> Result<Vec<u8>, DbError>;
    /// Write new bytes: the field becomes pending until the next flush.
    fn write_bytes(&self, bytes: impl Into<Vec<u8>>);
    /// A `std::io::Read` over the value, reading it in chunks through [`BlobField::read_range`].
    fn reader(&self) -> BlobReader;
}

impl<S, M> BlobField for day_model::Field<S, M, Blob>
where
    S: day_model::Source<M> + Copy + 'static,
    M: Model + 'static,
{
    fn bytes(&self) -> Result<Arc<Vec<u8>>, DbError> {
        if let Some(pending) = day_reactive::Binding::read(self).pending {
            return Ok(pending);
        }
        let (container, store, key) = locate(*self)?;
        container.blob_bytes(store, key, column_of::<M>(self.label()))
    }

    fn len(&self) -> Result<usize, DbError> {
        if let Some(pending) = day_reactive::Binding::read(self).pending {
            return Ok(pending.len());
        }
        let (container, store, key) = locate(*self)?;
        container.blob_len(store, key, column_of::<M>(self.label()))
    }

    fn read_range(&self, range: Range<usize>) -> Result<Vec<u8>, DbError> {
        if let Some(pending) = day_reactive::Binding::read(self).pending {
            return Ok(slice(&pending, range));
        }
        let (container, store, key) = locate(*self)?;
        container.blob_range(store, key, column_of::<M>(self.label()), range)
    }

    fn write_bytes(&self, bytes: impl Into<Vec<u8>>) {
        // The memo would otherwise keep answering with the old bytes until the flush.
        if let Ok((container, store, key)) = locate(*self) {
            container.blob_forget_field(store, key, column_of::<M>(self.label()));
        }
        day_reactive::Binding::write(self, Blob::new(bytes));
    }

    fn reader(&self) -> BlobReader {
        let field = *self;
        BlobReader {
            read_range: Box::new(move |r| field.read_range(r)),
            len: Box::new(move || field.len()),
            pos: 0,
            end: None,
        }
    }
}

type RangeReadFn = Box<dyn Fn(Range<usize>) -> Result<Vec<u8>, DbError>>;
type LenFn = Box<dyn Fn() -> Result<usize, DbError>>;

/// A `std::io::Read` over a lazy blob, in 256 KiB chunks.
pub struct BlobReader {
    read_range: RangeReadFn,
    len: LenFn,
    pos: usize,
    end: Option<usize>,
}

impl BlobReader {
    /// How much of the value one `read` asks the engine for at most.
    pub const CHUNK: usize = 256 * 1024;
}

impl std::io::Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = match self.end {
            Some(e) => e,
            None => {
                let e = (self.len)().map_err(std::io::Error::other)?;
                self.end = Some(e);
                e
            }
        };
        if self.pos >= end || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(Self::CHUNK).min(end - self.pos);
        let chunk = (self.read_range)(self.pos..self.pos + want).map_err(std::io::Error::other)?;
        let n = chunk.len().min(buf.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        self.pos += n;
        Ok(n)
    }
}

fn slice(bytes: &[u8], range: Range<usize>) -> Vec<u8> {
    let start = range.start.min(bytes.len());
    let end = range.end.clamp(start, bytes.len());
    bytes[start..end].to_vec()
}

/// The stored column name of the field labelled `field` on `M`.
fn column_of<M: Model>(field: &'static str) -> &'static str {
    M::COLUMNS
        .iter()
        .find(|c| c.field == field)
        .map(|c| c.name)
        .unwrap_or(field)
}

/// The container holding this field's store, with the store id and the row's key handle.
fn locate<S, M>(field: day_model::Field<S, M, Blob>) -> Result<(ModelContainer, u64, u64), DbError>
where
    S: day_model::Source<M> + Copy + 'static,
    M: 'static,
{
    let mut parts = Vec::new();
    day_model::Source::components(field, &mut parts);
    let (Some(&store), Some(&key)) = (parts.first(), parts.get(1)) else {
        return Err(DbError::driver(
            "a Blob field can only be read through a row of a container's store",
        ));
    };
    crate::relations::container_for_store(store)
        .map(|c| (c, store, key))
        .ok_or_else(|| {
            DbError::driver("a Blob field was read on a store no open ModelContainer holds")
        })
}

// ---------------------------------------------------------------------------------------------
// The memo
// ---------------------------------------------------------------------------------------------

/// Bytes read or written recently, bounded in bytes: the oldest entry leaves first, and a
/// value larger than the whole limit is served but never kept.
#[derive(Default)]
pub(crate) struct BlobMemo {
    entries: HashMap<(u64, u64, &'static str), Arc<Vec<u8>>>,
    order: VecDeque<(u64, u64, &'static str)>,
    bytes: usize,
}

impl BlobMemo {
    pub(crate) fn get(&mut self, k: (u64, u64, &'static str)) -> Option<Arc<Vec<u8>>> {
        let hit = self.entries.get(&k).cloned()?;
        // Most recently used goes to the back.
        if let Some(i) = self.order.iter().position(|e| *e == k) {
            self.order.remove(i);
            self.order.push_back(k);
        }
        Some(hit)
    }

    pub(crate) fn insert(&mut self, k: (u64, u64, &'static str), v: Arc<Vec<u8>>, limit: usize) {
        self.forget_one(k);
        if v.len() > limit {
            return;
        }
        self.bytes += v.len();
        self.entries.insert(k, v);
        self.order.push_back(k);
        while self.bytes > limit {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.entries.remove(&old) {
                self.bytes -= v.len();
            }
        }
    }

    pub(crate) fn forget_one(&mut self, k: (u64, u64, &'static str)) {
        if let Some(v) = self.entries.remove(&k) {
            self.bytes -= v.len();
            if let Some(i) = self.order.iter().position(|e| *e == k) {
                self.order.remove(i);
            }
        }
    }

    /// Drop every entry of one row (any column).
    pub(crate) fn forget_row(&mut self, store: u64, key: u64) {
        let keys: Vec<_> = self
            .entries
            .keys()
            .filter(|(s, k, _)| *s == store && *k == key)
            .copied()
            .collect();
        for k in keys {
            self.forget_one(k);
        }
    }

    /// Drop every entry of one store.
    pub(crate) fn forget_store(&mut self, store: u64) {
        let keys: Vec<_> = self
            .entries
            .keys()
            .filter(|(s, _, _)| *s == store)
            .copied()
            .collect();
        for k in keys {
            self.forget_one(k);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}
