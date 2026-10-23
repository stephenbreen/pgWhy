//! Result buffers (U12): columnar text storage, the binary window protocol, pending grid
//! edits, TSV copy, CSV/JSON export, and the per-owner retention registry.
//!
//! Rows never leave the core as JSON. A [`ResultSet`] keeps every column as one UTF-8 blob
//! plus cumulative end offsets and a NULL bitmap — a few bytes of overhead per cell and no
//! per-cell allocation — and the grid pulls `[start, start+count)` windows encoded by
//! [`ResultSet::encode_window`] in exactly the layout `ui/src/grid/protocol.ts` decodes.
//! Windows are pull-based, so the core queue is bounded by construction whatever the UI does.
//!
//! Pending grid edits (U13) live here too, as an overlay on the immutable buffer, so the
//! grid, copy, export, the SQL preview and the commit all read one truth: the buffer keeps
//! the *original* values (which the commit needs for its `WHERE` clauses) and every reader
//! renders the overlay on top.
//!
//! [`BufferSink`] adapts the executor's [`ResultSink`] into result sets plus a stream of
//! [`BufferEvent`]s for the editor; [`ResultRegistry`] owns the sets by id and bounds what a
//! tab retains.

use super::edit::EditTarget;
use super::executor::{ColumnMeta, ResultSink, RowBatch, RunOutcome, RunSummary, SqlError};
use super::splitter::StatementRange;
use super::txn::TxnStatus;
use crate::conn::Notice;
use crate::{AppError, AppResult, ErrorKind};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

/// Rows a single result set retains before it is flagged `truncated` (plan EC-18).
pub const DEFAULT_ROW_CAP: usize = 1_000_000;

/// Bytes a single result set retains before it is flagged `truncated`. A row cap alone does
/// not bound memory — 1M rows of wide text (JSON, `bytea`, long comments) is gigabytes — so the
/// buffer stops at whichever cap comes first and reports it the same way.
pub const DEFAULT_BYTE_CAP: usize = 256 * 1024 * 1024;

/// Largest window one request can return; the grid asks for 256-row windows.
pub const MAX_WINDOW_ROWS: usize = 65_536;

/// Row flags on the wire (`ui/src/grid/protocol.ts`).
pub const ROW_CLEAN: u8 = 0;
pub const ROW_UPDATED: u8 = 1;
pub const ROW_INSERTED: u8 = 2;
pub const ROW_DELETED: u8 = 3;

/// Rows per lock acquisition while exporting, so a long export never starves a stream.
const EXPORT_CHUNK_ROWS: usize = 4096;

// ---------------------------------------------------------------------------------------------
// Row sources
// ---------------------------------------------------------------------------------------------

/// Something rows can be appended from without copying cells into owned strings.
pub(crate) trait RowSource {
    fn row_count(&self) -> usize;
    fn cell(&self, row: usize, col: usize) -> Option<&str>;
}

impl RowSource for RowBatch<'_> {
    fn row_count(&self) -> usize {
        self.len()
    }

    fn cell(&self, row: usize, col: usize) -> Option<&str> {
        self.get(row, col)
    }
}

impl RowSource for [tokio_postgres::Row] {
    fn row_count(&self) -> usize {
        self.len()
    }

    fn cell(&self, row: usize, col: usize) -> Option<&str> {
        self[row].try_get::<_, Option<&str>>(col).ok().flatten()
    }
}

struct SliceRow<'a>(&'a [Option<&'a str>]);

impl RowSource for SliceRow<'_> {
    fn row_count(&self) -> usize {
        1
    }

    fn cell(&self, _row: usize, col: usize) -> Option<&str> {
        self.0.get(col).copied().flatten()
    }
}

// ---------------------------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct ColumnBuf {
    blob: Vec<u8>,
    ends: Vec<u32>,
    nulls: Vec<u64>,
}

impl ColumnBuf {
    fn push(&mut self, value: Option<&str>) {
        let row = self.ends.len();
        match value {
            Some(v) => self.blob.extend_from_slice(v.as_bytes()),
            None => {
                let word = row / 64;
                if self.nulls.len() <= word {
                    self.nulls.resize(word + 1, 0);
                }
                self.nulls[word] |= 1 << (row % 64);
            }
        }
        // the caller checked the blob stays within u32 (see `Inner::fits`)
        self.ends.push(self.blob.len() as u32);
    }

    fn is_null(&self, row: usize) -> bool {
        self.nulls
            .get(row / 64)
            .is_some_and(|w| w & (1 << (row % 64)) != 0)
    }

    fn get(&self, row: usize) -> Option<&str> {
        if row >= self.ends.len() || self.is_null(row) {
            return None;
        }
        let start = if row == 0 { 0 } else { self.ends[row - 1] } as usize;
        let end = self.ends[row] as usize;
        // bytes were appended from `&str`, so this never fails
        std::str::from_utf8(&self.blob[start..end]).ok()
    }

    fn bytes(&self) -> usize {
        self.blob.len() + self.ends.len() * 4 + self.nulls.len() * 8
    }
}

/// How a result set's stream ended (or that it is still going).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultState {
    Streaming,
    /// The statement finished; every row (up to the cap) is here.
    Complete,
    /// The user cancelled; the rows streamed so far are kept (exportable as partial).
    Cancelled,
    /// The statement failed or the connection died mid-stream — same semantics as cancel.
    Interrupted,
}

#[derive(Default)]
struct InsertedRow {
    cells: Vec<Option<String>>,
    set: Vec<bool>,
}

#[derive(Default)]
struct Pending {
    /// (row, col) → new value, for buffered rows only.
    updates: BTreeMap<(usize, usize), Option<String>>,
    deleted: BTreeSet<usize>,
    inserted: Vec<InsertedRow>,
    /// Bumped by every mutation.
    version: u64,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deleted.is_empty() && self.inserted.is_empty()
    }

    fn row_updated(&self, row: usize) -> bool {
        self.updates
            .range((row, 0)..=(row, usize::MAX))
            .next()
            .is_some()
    }
}

struct Inner {
    cols: Vec<ColumnBuf>,
    rows: usize,
    seen: u64,
    truncated: bool,
    state: ResultState,
    pending: Pending,
    committing: bool,
    /// Retained-byte ceiling for `cols` (see [`DEFAULT_BYTE_CAP`]).
    max_bytes: usize,
}

impl Inner {
    fn visible_rows(&self) -> usize {
        self.rows + self.pending.inserted.len()
    }

    /// The value the grid shows at `(row, col)` and whether it is a pending edit.
    fn render(&self, row: usize, col: usize) -> (Option<&str>, bool) {
        if row < self.rows {
            if !self.pending.updates.is_empty() {
                if let Some(v) = self.pending.updates.get(&(row, col)) {
                    return (v.as_deref(), true);
                }
            }
            (self.cols.get(col).and_then(|c| c.get(row)), false)
        } else {
            match self.pending.inserted.get(row - self.rows) {
                Some(r) => (
                    r.cells.get(col).and_then(|v| v.as_deref()),
                    r.set.get(col).copied().unwrap_or(false),
                ),
                None => (None, false),
            }
        }
    }

    fn row_flag(&self, row: usize) -> u8 {
        if row >= self.rows {
            ROW_INSERTED
        } else if self.pending.deleted.contains(&row) {
            ROW_DELETED
        } else if self.pending.row_updated(row) {
            ROW_UPDATED
        } else {
            ROW_CLEAN
        }
    }

    fn original_row(&self, row: usize) -> Vec<Option<String>> {
        self.cols
            .iter()
            .map(|c| c.get(row).map(str::to_string))
            .collect()
    }

    /// Whether `row` still fits: every column's `u32` offset stays addressable, and the set's
    /// retained-byte ceiling is not crossed.
    fn fits<S: RowSource + ?Sized>(&self, src: &S, row: usize) -> bool {
        let mut added = 0usize;
        for (c, buf) in self.cols.iter().enumerate() {
            let add = src.cell(row, c).map_or(0, str::len);
            if buf.blob.len() + add > u32::MAX as usize {
                return false;
            }
            added += add;
        }
        self.retained_bytes() + added <= self.max_bytes
    }

    /// Approximate retained heap bytes: cell text plus the offset and NULL-bitmap overhead.
    fn retained_bytes(&self) -> usize {
        self.cols.iter().map(ColumnBuf::bytes).sum()
    }

    fn counts(&self) -> PendingCounts {
        let p = &self.pending;
        let mut updated_rows = 0;
        let mut last = None;
        for &(row, _) in p.updates.keys() {
            if last != Some(row) {
                last = Some(row);
                if !p.deleted.contains(&row) {
                    updated_rows += 1;
                }
            }
        }
        PendingCounts {
            updated_cells: p.updates.len(),
            updated_rows,
            inserted_rows: p.inserted.len(),
            deleted_rows: p.deleted.len(),
            total_changes: updated_rows + p.inserted.len() + p.deleted.len(),
        }
    }
}

/// Pending-edit tallies. `total_changes` is the number of statements a commit would run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingCounts {
    pub updated_cells: usize,
    /// Rows with at least one updated cell and not also deleted.
    pub updated_rows: usize,
    pub inserted_rows: usize,
    pub deleted_rows: usize,
    pub total_changes: usize,
}

/// One pending change, as the commit consumes it. `row` is the grid row index (inserted rows
/// follow the buffered rows). `values` are `(column index, new value)`; `None` is SQL NULL.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingChange {
    Delete {
        row: usize,
        /// Every column's original value (the key columns' are what the `WHERE` uses).
        original: Vec<Option<String>>,
    },
    Update {
        row: usize,
        original: Vec<Option<String>>,
        values: Vec<(usize, Option<String>)>,
    },
    /// Only the columns the user set; the rest take their defaults.
    Insert {
        row: usize,
        values: Vec<(usize, Option<String>)>,
    },
}

/// A snapshot of a result set's status, for status rows and IPC replies.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResultInfo {
    pub id: String,
    pub columns: Vec<ColumnMeta>,
    /// Buffered rows (the stream's rows, up to the cap).
    pub row_count: usize,
    /// Rows the grid shows: buffered + pending inserts.
    pub visible_rows: usize,
    /// Rows the server sent, including those dropped past the cap.
    pub rows_seen: u64,
    pub truncated: bool,
    pub state: ResultState,
    pub pending: PendingCounts,
    pub approx_bytes: usize,
    pub editable: bool,
}

/// One statement's result rows plus its pending-edit overlay. Safe to append to (streaming)
/// and read from (window requests, copy, export) concurrently.
pub struct ResultSet {
    id: String,
    columns: Vec<ColumnMeta>,
    cap: usize,
    inner: RwLock<Inner>,
    edit_target: RwLock<Option<Arc<EditTarget>>>,
}

impl std::fmt::Debug for ResultSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultSet")
            .field("id", &self.id)
            .field("columns", &self.columns.len())
            .field("rows", &self.row_count())
            .finish_non_exhaustive()
    }
}

fn refused(message: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Refused, message)
}

impl ResultSet {
    /// A new, empty, streaming result set with a fresh id. `cap` is the retention cap
    /// ([`DEFAULT_ROW_CAP`]); `0` is treated as 1.
    pub fn new(columns: Vec<ColumnMeta>, cap: usize) -> Self {
        Self::with_id(uuid::Uuid::new_v4().to_string(), columns, cap)
    }

    /// Same, with explicit retention limits. `byte_cap` bounds the retained buffer bytes (see
    /// [`DEFAULT_BYTE_CAP`]); `0` is treated as 1.
    pub fn with_limits(columns: Vec<ColumnMeta>, cap: usize, byte_cap: usize) -> Self {
        Self::with_id_limits(uuid::Uuid::new_v4().to_string(), columns, cap, byte_cap)
    }

    pub fn with_id(id: impl Into<String>, columns: Vec<ColumnMeta>, cap: usize) -> Self {
        Self::with_id_limits(id, columns, cap, DEFAULT_BYTE_CAP)
    }

    fn with_id_limits(
        id: impl Into<String>,
        columns: Vec<ColumnMeta>,
        cap: usize,
        byte_cap: usize,
    ) -> Self {
        let cols = columns.iter().map(|_| ColumnBuf::default()).collect();
        Self {
            id: id.into(),
            columns,
            cap: cap.max(1),
            inner: RwLock::new(Inner {
                cols,
                rows: 0,
                seen: 0,
                truncated: false,
                state: ResultState::Streaming,
                pending: Pending::default(),
                committing: false,
                max_bytes: byte_cap.max(1),
            }),
            edit_target: RwLock::new(None),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }

    pub fn row_cap(&self) -> usize {
        self.cap
    }

    /// Retained-byte ceiling for the buffer.
    pub fn byte_cap(&self) -> usize {
        self.read().max_bytes
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|p| p.into_inner())
    }

    /// Buffered rows (excludes pending inserts).
    pub fn row_count(&self) -> usize {
        self.read().rows
    }

    /// Rows the grid shows: buffered rows followed by pending inserts.
    pub fn visible_row_count(&self) -> usize {
        self.read().visible_rows()
    }

    pub fn rows_seen(&self) -> u64 {
        self.read().seen
    }

    pub fn truncated(&self) -> bool {
        self.read().truncated
    }

    pub fn state(&self) -> ResultState {
        self.read().state
    }

    /// Partial = truncated at the cap, or the stream did not complete.
    pub fn is_partial(&self) -> bool {
        let i = self.read();
        i.truncated || i.state != ResultState::Complete
    }

    /// Approximate retained heap bytes (buffer only).
    pub fn approx_bytes(&self) -> usize {
        self.read().retained_bytes()
    }

    pub fn info(&self) -> ResultInfo {
        let i = self.read();
        ResultInfo {
            id: self.id.clone(),
            columns: self.columns.clone(),
            row_count: i.rows,
            visible_rows: i.visible_rows(),
            rows_seen: i.seen,
            truncated: i.truncated,
            state: i.state,
            pending: i.counts(),
            approx_bytes: i.retained_bytes(),
            editable: self.edit_target().is_some(),
        }
    }

    /// Attach the edit target (top-N grids). Without one the grid is read-only.
    pub fn set_edit_target(&self, target: Option<EditTarget>) {
        *self.edit_target.write().unwrap_or_else(|p| p.into_inner()) = target.map(Arc::new);
    }

    pub fn edit_target(&self) -> Option<Arc<EditTarget>> {
        self.edit_target
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    // ------------------------------------------------------------------ appending

    pub(crate) fn append_source<S: RowSource + ?Sized>(&self, src: &S) -> usize {
        let n = src.row_count();
        if n == 0 {
            return 0;
        }
        let mut i = self.write();
        let mut kept = 0;
        for row in 0..n {
            i.seen += 1;
            if i.rows >= self.cap || i.truncated || !i.fits(src, row) {
                i.truncated = true;
                continue;
            }
            for (c, buf) in i.cols.iter_mut().enumerate() {
                buf.push(src.cell(row, c));
            }
            i.rows += 1;
            kept += 1;
        }
        kept
    }

    /// Append one batch from the executor. Returns the rows retained (the rest were past the
    /// cap and only counted).
    pub fn append_batch(&self, batch: RowBatch<'_>) -> usize {
        self.append_source(&batch)
    }

    /// Append one row (missing trailing cells are NULL). Returns false when it was dropped at
    /// the cap.
    pub fn append_row(&self, cells: &[Option<&str>]) -> bool {
        self.append_source(&SliceRow(cells)) == 1
    }

    /// End the stream. Only the first transition away from `Streaming` sticks.
    pub fn finish(&self, state: ResultState) {
        let mut i = self.write();
        if i.state == ResultState::Streaming {
            i.state = state;
        }
    }

    // ------------------------------------------------------------------ reading

    /// The value shown at `(row, col)` (pending edits applied); `None` for NULL or out of
    /// range.
    pub fn cell(&self, row: usize, col: usize) -> Option<String> {
        self.read().render(row, col).0.map(str::to_string)
    }

    /// The buffered (pre-edit) value; `None` for NULL, inserted rows, or out of range.
    pub fn original_cell(&self, row: usize, col: usize) -> Option<String> {
        let i = self.read();
        if row >= i.rows {
            return None;
        }
        i.cols.get(col).and_then(|c| c.get(row)).map(str::to_string)
    }

    /// [`ROW_CLEAN`] / [`ROW_UPDATED`] / [`ROW_INSERTED`] / [`ROW_DELETED`].
    pub fn row_flag(&self, row: usize) -> u8 {
        self.read().row_flag(row)
    }

    /// Encode rows `[start, start+count)` in the `ui/src/grid/protocol.ts` chunk layout.
    ///
    /// Only rows that exist are returned: during a stream a request past the rows received so
    /// far gets a shorter (possibly empty) window and the grid re-requests as `rows` events
    /// report growth. `count` is clamped to [`MAX_WINDOW_ROWS`].
    pub fn encode_window(&self, start: usize, count: usize) -> Vec<u8> {
        let i = self.read();
        let total = i.visible_rows();
        let count = count
            .min(MAX_WINDOW_ROWS)
            .min(total.saturating_sub(start))
            .min(u32::MAX as usize);
        let ncols = self.columns.len().min(u16::MAX as usize);
        let bits = count.div_ceil(8);
        let mut out = Vec::with_capacity(10 + count + ncols * (2 * bits + 8 + count * 12));
        out.extend_from_slice(&(start.min(u32::MAX as usize) as u32).to_le_bytes());
        out.extend_from_slice(&(count as u32).to_le_bytes());
        out.extend_from_slice(&(ncols as u16).to_le_bytes());
        out.extend((0..count).map(|r| i.row_flag(start + r)));
        let mut ends: Vec<u32> = Vec::with_capacity(count);
        for c in 0..ncols {
            let nulls_at = out.len();
            out.resize(nulls_at + 2 * bits, 0);
            let pend_at = nulls_at + bits;
            let len_at = out.len();
            out.extend_from_slice(&[0; 4]);
            let blob_at = out.len();
            ends.clear();
            for r in 0..count {
                let (v, pending) = i.render(start + r, c);
                match v {
                    Some(s) => out.extend_from_slice(s.as_bytes()),
                    None => out[nulls_at + r / 8] |= 1 << (r % 8),
                }
                if pending {
                    out[pend_at + r / 8] |= 1 << (r % 8);
                }
                ends.push((out.len() - blob_at) as u32);
            }
            let blob_len = (out.len() - blob_at) as u32;
            out[len_at..len_at + 4].copy_from_slice(&blob_len.to_le_bytes());
            for e in &ends {
                out.extend_from_slice(&e.to_le_bytes());
            }
        }
        out
    }

    /// Copy rows `r0..=r1`, columns `c0..=c1` (grid indices, clamped) as TSV the way
    /// Excel/SSMS paste it: values containing a tab, newline or `"` are quoted with `"`
    /// doubled; NULL is an empty field; rows end with `\n`. Pending values are rendered and
    /// rows marked for deletion are included (the user selected them).
    pub fn copy_tsv(
        &self,
        r0: usize,
        r1: usize,
        c0: usize,
        c1: usize,
        with_headers: bool,
    ) -> String {
        self.copy_tsv_limited(r0, r1, c0, c1, with_headers, usize::MAX)
            .unwrap_or_default()
    }

    /// [`Self::copy_tsv`], but refusing instead of materialising more than `max_bytes` of text
    /// (a select-all copy of a large result would otherwise freeze the app building a string
    /// the clipboard cannot usefully hold). The error names exporting as the way to get it all.
    pub fn copy_tsv_limited(
        &self,
        r0: usize,
        r1: usize,
        c0: usize,
        c1: usize,
        with_headers: bool,
        max_bytes: usize,
    ) -> AppResult<String> {
        let i = self.read();
        let mut out = String::new();
        let ncols = self.columns.len();
        if ncols == 0 {
            return Ok(out);
        }
        let (c0, c1) = (c0.min(c1), c1.max(c0).min(ncols - 1));
        if c0 > c1 {
            return Ok(out);
        }
        if with_headers {
            for c in c0..=c1 {
                if c > c0 {
                    out.push('\t');
                }
                push_tsv(&mut out, &self.columns[c].name);
            }
            out.push('\n');
        }
        let total = i.visible_rows();
        if total == 0 {
            return Ok(out);
        }
        let (r0, r1) = (r0.min(r1), r1.max(r0).min(total - 1));
        for r in r0..=r1 {
            for c in c0..=c1 {
                if c > c0 {
                    out.push('\t');
                }
                if let Some(v) = i.render(r, c).0 {
                    push_tsv(&mut out, v);
                }
            }
            out.push('\n');
            if out.len() > max_bytes {
                return Err(refused(format!(
                    "the selection is larger than {} MB to copy",
                    max_bytes / (1024 * 1024)
                ))
                .with_hint(
                    "export it to CSV or JSON instead — that streams to a file without a copy limit",
                ));
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ pending edits

    fn editable(&self) -> AppResult<RwLockWriteGuard<'_, Inner>> {
        let i = self.write();
        if i.state == ResultState::Streaming {
            return Err(refused(
                "the result is still loading; edit it once it finishes",
            ));
        }
        if i.committing {
            return Err(refused("a commit is in progress"));
        }
        Ok(i)
    }

    fn check_cell(&self, i: &Inner, row: usize, col: usize) -> AppResult<()> {
        if col >= self.columns.len() {
            return Err(refused(format!("column {col} does not exist")));
        }
        if row >= i.visible_rows() {
            return Err(refused(format!("row {row} does not exist")));
        }
        Ok(())
    }

    /// Set a pending value (`None` = SQL NULL; `Some("")` stays an empty string). Setting a
    /// buffered cell back to its original value clears the edit.
    pub fn set_cell(&self, row: usize, col: usize, value: Option<String>) -> AppResult<()> {
        let mut i = self.editable()?;
        self.check_cell(&i, row, col)?;
        if row < i.rows {
            let original = i.cols[col].get(row);
            if original == value.as_deref() {
                i.pending.updates.remove(&(row, col));
            } else {
                i.pending.updates.insert((row, col), value);
            }
        } else {
            let r = i.rows;
            let ins = &mut i.pending.inserted[row - r];
            ins.cells[col] = value;
            ins.set[col] = true;
        }
        i.pending.version += 1;
        Ok(())
    }

    /// Drop a pending cell value (an inserted row's cell goes back to "use the default").
    pub fn clear_cell(&self, row: usize, col: usize) -> AppResult<()> {
        let mut i = self.editable()?;
        self.check_cell(&i, row, col)?;
        if row < i.rows {
            i.pending.updates.remove(&(row, col));
        } else {
            let r = i.rows;
            let ins = &mut i.pending.inserted[row - r];
            ins.cells[col] = None;
            ins.set[col] = false;
        }
        i.pending.version += 1;
        Ok(())
    }

    /// Append a blank pending row; returns its grid row index. Cells left unset take the
    /// column default on commit (they render as NULL meanwhile).
    pub fn insert_row(&self) -> AppResult<usize> {
        let mut i = self.editable()?;
        let n = self.columns.len();
        i.pending.inserted.push(InsertedRow {
            cells: vec![None; n],
            set: vec![false; n],
        });
        i.pending.version += 1;
        Ok(i.visible_rows() - 1)
    }

    /// Mark a buffered row for deletion, or drop a pending inserted row (later inserted rows
    /// shift up by one).
    pub fn delete_row(&self, row: usize) -> AppResult<()> {
        let mut i = self.editable()?;
        if row >= i.visible_rows() {
            return Err(refused(format!("row {row} does not exist")));
        }
        if row < i.rows {
            i.pending.deleted.insert(row);
        } else {
            let r = i.rows;
            i.pending.inserted.remove(row - r);
        }
        i.pending.version += 1;
        Ok(())
    }

    /// Unmark a pending deletion (its cell edits, if any, come back).
    pub fn undelete_row(&self, row: usize) -> AppResult<()> {
        let mut i = self.editable()?;
        i.pending.deleted.remove(&row);
        i.pending.version += 1;
        Ok(())
    }

    /// Discard every pending edit; returns what was discarded.
    pub fn discard_pending(&self) -> AppResult<PendingCounts> {
        let mut i = self.write();
        if i.committing {
            return Err(refused("a commit is in progress"));
        }
        let counts = i.counts();
        let version = i.pending.version + 1;
        i.pending = Pending {
            version,
            ..Pending::default()
        };
        Ok(counts)
    }

    pub fn pending_counts(&self) -> PendingCounts {
        self.read().counts()
    }

    pub fn has_pending(&self) -> bool {
        !self.read().pending.is_empty()
    }

    /// Every pending change in commit order: deletes, then updates, then inserts (so a key
    /// freed by a delete can be reused by an update or insert), each by row.
    pub fn pending_changes(&self) -> Vec<PendingChange> {
        self.pending_snapshot().1
    }

    pub(crate) fn pending_snapshot(&self) -> (u64, Vec<PendingChange>) {
        let i = self.read();
        let p = &i.pending;
        let mut out = Vec::new();
        for &row in &p.deleted {
            out.push(PendingChange::Delete {
                row,
                original: i.original_row(row),
            });
        }
        let mut by_row: BTreeMap<usize, Vec<(usize, Option<String>)>> = BTreeMap::new();
        for (&(row, col), v) in &p.updates {
            if !p.deleted.contains(&row) {
                by_row.entry(row).or_default().push((col, v.clone()));
            }
        }
        for (row, values) in by_row {
            out.push(PendingChange::Update {
                row,
                original: i.original_row(row),
                values,
            });
        }
        for (k, ins) in p.inserted.iter().enumerate() {
            let values = ins
                .cells
                .iter()
                .zip(&ins.set)
                .enumerate()
                .filter(|(_, (_, set))| **set)
                .map(|(c, (v, _))| (c, v.clone()))
                .collect();
            out.push(PendingChange::Insert {
                row: i.rows + k,
                values,
            });
        }
        (p.version, out)
    }

    /// Freeze edits for the duration of a commit.
    pub(crate) fn begin_commit(&self) -> AppResult<CommitGuard<'_>> {
        let mut i = self.write();
        if i.committing {
            return Err(refused("a commit is already in progress"));
        }
        i.committing = true;
        Ok(CommitGuard(self))
    }

    /// Clear pending edits after a successful commit, if they are still the committed set.
    pub(crate) fn clear_committed(&self, version: u64) {
        let mut i = self.write();
        if i.pending.version == version {
            i.pending = Pending {
                version: version + 1,
                ..Pending::default()
            };
        }
    }

    // ------------------------------------------------------------------ export

    /// Export as CSV (RFC 4180 quoting, UTF-8, `\n` line ends). NULL is written as an empty
    /// field — and empty strings as `""`, so the two stay distinguishable — or as
    /// `opts.null_literal` when set. See [`Self::export_json`] for the shared semantics.
    pub fn export_csv(
        &self,
        path: &Path,
        opts: &CsvOptions,
        progress: Option<&mut dyn FnMut(ExportProgress) -> bool>,
    ) -> AppResult<ExportReport> {
        let ncols = self.columns.len();
        let mut first = true;
        self.export(path, progress, |w, row, i| {
            if first {
                first = false;
                if opts.header {
                    for (c, col) in self.columns.iter().enumerate() {
                        if c > 0 {
                            w.write_all(b",")?;
                        }
                        write_csv(w, &col.name, true, opts.escape_formulas)?;
                    }
                    w.write_all(b"\n")?;
                }
            }
            let Some(row) = row else { return Ok(()) };
            for c in 0..ncols {
                if c > 0 {
                    w.write_all(b",")?;
                }
                match i.render(row, c).0 {
                    Some(v) => write_csv(w, v, opts.null_literal.is_none(), opts.escape_formulas)?,
                    None => {
                        if let Some(lit) = &opts.null_literal {
                            write_csv(w, lit, false, opts.escape_formulas)?;
                        }
                    }
                }
            }
            w.write_all(b"\n")
        })
    }

    /// Export as a JSON array of objects, one per row (keys are column names; duplicates get
    /// `_2`, `_3`… suffixes). Numeric and boolean columns become JSON numbers/booleans,
    /// `json`/`jsonb` values are embedded as-is when they parse, everything else is a string;
    /// NULL is `null`.
    ///
    /// Both exports run from the buffer with pending edits applied (deleted rows omitted,
    /// inserted rows included), write a temp file in the target directory and rename it into
    /// place — a failure or cancel (the progress callback returning `false`) never leaves a
    /// partial file. A truncated or incomplete result exports what was retained and reports
    /// `partial`.
    pub fn export_json(
        &self,
        path: &Path,
        progress: Option<&mut dyn FnMut(ExportProgress) -> bool>,
    ) -> AppResult<ExportReport> {
        let keys = json_keys(&self.columns);
        let kinds: Vec<JsonKind> = self.columns.iter().map(json_kind).collect();
        let mut first = true;
        let mut any = false;
        self.export(path, progress, |w, row, i| {
            if first {
                first = false;
                w.write_all(b"[")?;
            }
            let Some(row) = row else {
                return w.write_all(if any { b"\n]\n" } else { b"]\n" });
            };
            w.write_all(if any { b",\n{" } else { b"\n{" })?;
            any = true;
            for (c, key) in keys.iter().enumerate() {
                if c > 0 {
                    w.write_all(b",")?;
                }
                w.write_all(key.as_bytes())?;
                w.write_all(b":")?;
                match i.render(row, c).0 {
                    None => w.write_all(b"null")?,
                    Some(v) => write_json_value(w, v, kinds[c])?,
                }
            }
            w.write_all(b"}")
        })
    }

    /// Shared export driver: calls `write_row(w, Some(row), inner)` per exported row (chunked
    /// read locks), then `write_row(w, None, inner)` once at the end.
    fn export(
        &self,
        path: &Path,
        mut progress: Option<&mut dyn FnMut(ExportProgress) -> bool>,
        mut write_row: impl FnMut(&mut dyn Write, Option<usize>, &Inner) -> io::Result<()>,
    ) -> AppResult<ExportReport> {
        let (total, partial, truncated, pending) = {
            let i = self.read();
            (
                i.visible_rows(),
                i.truncated || i.state != ResultState::Complete,
                i.truncated,
                i.counts().total_changes,
            )
        };
        let mut rows = 0u64;
        let bytes = write_atomic(path, |w| {
            let mut start = 0;
            loop {
                let end = (start + EXPORT_CHUNK_ROWS).min(total);
                {
                    let i = self.read();
                    for r in start..end.min(i.visible_rows()) {
                        if r < i.rows && i.pending.deleted.contains(&r) {
                            continue;
                        }
                        write_row(w, Some(r), &i).map_err(|e| io_error(&e, path))?;
                        rows += 1;
                    }
                    if end >= total {
                        write_row(w, None, &i).map_err(|e| io_error(&e, path))?;
                    }
                }
                if let Some(cb) = progress.as_mut() {
                    if !cb(ExportProgress {
                        rows_written: rows,
                        total_rows: total as u64,
                    }) {
                        return Err(AppError::new(
                            ErrorKind::Cancelled,
                            "the export was cancelled",
                        ));
                    }
                }
                if end >= total {
                    return Ok(());
                }
                start = end;
            }
        })?;
        Ok(ExportReport {
            path: path.display().to_string(),
            rows,
            bytes,
            pending_edits_included: pending,
            partial,
            truncated,
        })
    }
}

/// Clears the `committing` flag when the commit ends, however it ends.
pub(crate) struct CommitGuard<'a>(&'a ResultSet);

impl Drop for CommitGuard<'_> {
    fn drop(&mut self) {
        self.0.write().committing = false;
    }
}

// ---------------------------------------------------------------------------------------------
// Export helpers
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CsvOptions {
    #[serde(default = "yes")]
    pub header: bool,
    /// Write NULL as this text (e.g. `NULL`) instead of an empty field.
    #[serde(default)]
    pub null_literal: Option<String>,
    /// Neutralise spreadsheet formula injection (CWE-1236): a cell starting with `=`, `+`,
    /// `-`, `@`, TAB or CR is prefixed with `'`, so Excel/Sheets show it as text instead of
    /// evaluating it. Values come from the server and can be attacker-controlled; leave this on
    /// for exports a human will open in a spreadsheet, off for machine-to-machine use.
    #[serde(default)]
    pub escape_formulas: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExportProgress {
    pub rows_written: u64,
    pub total_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExportReport {
    pub path: String,
    pub rows: u64,
    pub bytes: u64,
    /// Pending changes rendered into the file ("includes N pending edits").
    pub pending_edits_included: usize,
    /// Truncated at the cap, or the stream was cancelled/interrupted.
    pub partial: bool,
    pub truncated: bool,
}

fn io_error(e: &io::Error, path: &Path) -> AppError {
    use io::ErrorKind as K;
    let shown = path.display();
    let (kind, message, hint) = match e.kind() {
        K::NotFound => (
            ErrorKind::Config,
            format!("cannot export to {shown}: the folder does not exist"),
            "choose an existing folder",
        ),
        K::PermissionDenied | K::ReadOnlyFilesystem => (
            ErrorKind::Disk,
            format!("cannot export to {shown}: permission denied"),
            "choose a folder you can write to",
        ),
        K::StorageFull | K::QuotaExceeded => (
            ErrorKind::Disk,
            format!("cannot export to {shown}: the disk is full"),
            "free some space or export elsewhere",
        ),
        K::IsADirectory | K::InvalidInput | K::InvalidFilename => (
            ErrorKind::Config,
            format!("cannot export to {shown}: not a valid file path"),
            "choose a file name",
        ),
        _ => (
            ErrorKind::Disk,
            format!("cannot export to {shown}: the write failed"),
            "check the destination and try again",
        ),
    };
    AppError::new(kind, message)
        .with_hint(hint)
        .with_detail(e.to_string())
}

/// Write via a temp file in the destination directory, renamed into place on success; the
/// temp file is removed on any failure. Returns the bytes written.
fn write_atomic(path: &Path, body: impl FnOnce(&mut dyn Write) -> AppResult<()>) -> AppResult<u64> {
    if path.is_dir() {
        return Err(io_error(
            &io::Error::from(io::ErrorKind::IsADirectory),
            path,
        ));
    }
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".pgwhy-export-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|e| io_error(&e, path))?;
    {
        let mut w = BufWriter::with_capacity(256 * 1024, tmp.as_file_mut());
        body(&mut w)?;
        w.flush().map_err(|e| io_error(&e, path))?;
    }
    tmp.as_file().sync_all().map_err(|e| io_error(&e, path))?;
    let bytes = tmp
        .as_file()
        .metadata()
        .map_err(|e| io_error(&e, path))?
        .len();
    tmp.persist(path).map_err(|e| io_error(&e.error, path))?;
    Ok(bytes)
}

fn push_tsv(out: &mut String, v: &str) {
    if v.contains(['\t', '\n', '\r', '"']) {
        out.push('"');
        for ch in v.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    } else {
        out.push_str(v);
    }
}

/// `quote_empty`: write `""` for an empty string so it differs from NULL's empty field.
fn write_csv(
    w: &mut dyn Write,
    v: &str,
    quote_empty: bool,
    escape_formulas: bool,
) -> io::Result<()> {
    let formula = escape_formulas
        && v.as_bytes()
            .first()
            .is_some_and(|c| matches!(c, b'=' | b'+' | b'-' | b'@' | b'\t' | b'\r'));
    if formula || (v.is_empty() && quote_empty) || v.contains([',', '"', '\n', '\r']) {
        w.write_all(b"\"")?;
        if formula {
            w.write_all(b"'")?;
        }
        for (k, part) in v.split('"').enumerate() {
            if k > 0 {
                w.write_all(b"\"\"")?;
            }
            w.write_all(part.as_bytes())?;
        }
        w.write_all(b"\"")
    } else {
        w.write_all(v.as_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonKind {
    Number,
    Bool,
    Json,
    Text,
}

/// By type oid when known, else by type name (`int4` from the executor's describe, or
/// `integer`/`numeric(10,2)` from `format_type` for top-N grids).
fn json_kind(col: &ColumnMeta) -> JsonKind {
    match col.type_oid {
        Some(20 | 21 | 23 | 26 | 700 | 701 | 1700) => return JsonKind::Number,
        Some(16) => return JsonKind::Bool,
        Some(114 | 3802) => return JsonKind::Json,
        Some(_) => return JsonKind::Text,
        None => {}
    }
    let Some(name) = col.type_name.as_deref() else {
        return JsonKind::Text;
    };
    let name = name.trim().to_ascii_lowercase();
    if name.ends_with(']') || name.starts_with('_') {
        return JsonKind::Text;
    }
    let base = name.split('(').next().unwrap_or("").trim();
    match base {
        "int2" | "int4" | "int8" | "smallint" | "integer" | "bigint" | "float4" | "float8"
        | "real" | "double precision" | "numeric" | "decimal" | "oid" => JsonKind::Number,
        "bool" | "boolean" => JsonKind::Bool,
        "json" | "jsonb" => JsonKind::Json,
        _ => JsonKind::Text,
    }
}

/// JSON's number grammar (Postgres also prints `NaN`/`Infinity`, which JSON lacks).
fn is_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut p = 0;
    let digits = |p: &mut usize| {
        let s = *p;
        while *p < b.len() && b[*p].is_ascii_digit() {
            *p += 1;
        }
        *p > s
    };
    if p < b.len() && b[p] == b'-' {
        p += 1;
    }
    if p < b.len() && b[p] == b'0' {
        p += 1;
    } else if !(p < b.len() && b[p].is_ascii_digit() && digits(&mut p)) {
        return false;
    }
    if p < b.len() && b[p] == b'.' {
        p += 1;
        if !digits(&mut p) {
            return false;
        }
    }
    if p < b.len() && (b[p] == b'e' || b[p] == b'E') {
        p += 1;
        if p < b.len() && (b[p] == b'+' || b[p] == b'-') {
            p += 1;
        }
        if !digits(&mut p) {
            return false;
        }
    }
    p == b.len()
}

fn write_json_string(w: &mut dyn Write, v: &str) -> io::Result<()> {
    serde_json::to_writer(&mut *w, v).map_err(io::Error::other)
}

fn write_json_value(w: &mut dyn Write, v: &str, kind: JsonKind) -> io::Result<()> {
    match kind {
        JsonKind::Number if is_json_number(v) => w.write_all(v.as_bytes()),
        JsonKind::Bool if matches!(v, "t" | "true") => w.write_all(b"true"),
        JsonKind::Bool if matches!(v, "f" | "false") => w.write_all(b"false"),
        JsonKind::Json if serde_json::from_str::<serde::de::IgnoredAny>(v).is_ok() => {
            w.write_all(v.as_bytes())
        }
        _ => write_json_string(w, v),
    }
}

/// Serialized, de-duplicated object keys.
fn json_keys(cols: &[ColumnMeta]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::new();
    cols.iter()
        .map(|c| {
            let mut key = c.name.clone();
            let mut n = 2;
            while !used.insert(key.clone()) {
                key = format!("{}_{n}", c.name);
                n += 1;
            }
            serde_json::to_string(&key).unwrap_or_else(|_| "\"\"".into())
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------

/// Retention limits for [`ResultRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegistryOptions {
    /// Rows one owner (editor tab / grid) may retain across its result sets before older
    /// ones are evicted.
    pub owner_row_budget: usize,
    /// Bytes one owner may retain across its result sets. The row budget alone does not bound
    /// memory: a couple of wide result sets fit 2M rows and still cost gigabytes.
    pub owner_byte_budget: usize,
}

impl Default for RegistryOptions {
    fn default() -> Self {
        Self {
            owner_row_budget: 2 * DEFAULT_ROW_CAP,
            owner_byte_budget: 2 * DEFAULT_BYTE_CAP,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvictedResult {
    pub result_id: String,
    pub rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingHold {
    pub result_id: String,
    pub pending: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvictionReport {
    /// Evicted, oldest first — the UI shows a notice for each.
    pub evicted: Vec<EvictedResult>,
    /// Over budget but kept because they hold pending edits (evicting one needs an explicit
    /// discard, [`ResultRegistry::evict`] with `discard_pending`).
    pub held_by_pending: Vec<PendingHold>,
    pub retained_rows: usize,
    pub budget: usize,
    pub retained_bytes: usize,
    pub budget_bytes: usize,
}

struct Entry {
    set: Arc<ResultSet>,
    owner: String,
    seq: u64,
    run: u64,
}

#[derive(Default)]
struct OwnerState {
    run: u64,
    focused: Option<String>,
}

#[derive(Default)]
struct RegInner {
    seq: u64,
    sets: HashMap<String, Entry>,
    owners: HashMap<String, OwnerState>,
}

/// Result sets by id, grouped by owner (an editor tab or grid id), with a per-owner retained
/// row budget. Eviction is explicit (callers run [`Self::enforce_budget`]; [`BufferSink`]
/// does after each statement) and never touches the owner's current run, its focused result,
/// a result still streaming, or one holding pending edits.
pub struct ResultRegistry {
    inner: Mutex<RegInner>,
    /// read on every enforcement, so [`ResultRegistry::set_options`] applies without a restart
    opts: Mutex<RegistryOptions>,
}

impl Default for ResultRegistry {
    fn default() -> Self {
        Self::new(RegistryOptions::default())
    }
}

impl std::fmt::Debug for ResultRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultRegistry")
            .field("opts", &self.options())
            .finish_non_exhaustive()
    }
}

impl ResultRegistry {
    pub fn new(opts: RegistryOptions) -> Self {
        Self {
            inner: Mutex::new(RegInner::default()),
            opts: Mutex::new(opts),
        }
    }

    pub fn options(&self) -> RegistryOptions {
        *self.opts.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Change the per-owner budgets (Settings). Results already held are re-checked the next
    /// time their owner's budget is enforced.
    pub fn set_options(&self, opts: RegistryOptions) {
        *self.opts.lock().unwrap_or_else(|p| p.into_inner()) = opts;
    }

    fn lock(&self) -> MutexGuard<'_, RegInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn get(&self, id: &str) -> Option<Arc<ResultSet>> {
        self.lock().sets.get(id).map(|e| e.set.clone())
    }

    /// [`Self::get`] as an error the UI can show.
    pub fn require(&self, id: &str) -> AppResult<Arc<ResultSet>> {
        self.get(id).ok_or_else(|| {
            AppError::new(
                ErrorKind::Refused,
                "this result is no longer available (it was released or evicted)",
            )
            .with_hint("run the query again")
        })
    }

    /// Start a new run for `owner`: results registered from now on form its protected
    /// current run; earlier ones become evictable. Returns the run number.
    pub fn begin_run(&self, owner: &str) -> u64 {
        let mut g = self.lock();
        let st = g.owners.entry(owner.to_string()).or_default();
        st.run += 1;
        st.run
    }

    /// Register a result under `owner`, in its current run.
    pub fn register(&self, owner: &str, set: Arc<ResultSet>) {
        let mut g = self.lock();
        g.seq += 1;
        let seq = g.seq;
        let run = g.owners.entry(owner.to_string()).or_default().run;
        g.sets.insert(
            set.id().to_string(),
            Entry {
                set,
                owner: owner.to_string(),
                seq,
                run,
            },
        );
    }

    /// The result the owner currently shows (never evicted), or `None`.
    pub fn set_focused(&self, owner: &str, id: Option<&str>) {
        self.lock()
            .owners
            .entry(owner.to_string())
            .or_default()
            .focused = id.map(str::to_string);
    }

    /// The owner's result ids, oldest first.
    pub fn results_of(&self, owner: &str) -> Vec<String> {
        let g = self.lock();
        let mut v: Vec<_> = g.sets.values().filter(|e| e.owner == owner).collect();
        v.sort_by_key(|e| e.seq);
        v.into_iter().map(|e| e.set.id().to_string()).collect()
    }

    pub fn owner_of(&self, id: &str) -> Option<String> {
        self.lock().sets.get(id).map(|e| e.owner.clone())
    }

    pub fn retained_rows(&self, owner: &str) -> usize {
        self.lock()
            .sets
            .values()
            .filter(|e| e.owner == owner)
            .map(|e| e.set.row_count())
            .sum()
    }

    /// Evict the owner's oldest evictable results until it is within both budgets.
    pub fn enforce_budget(&self, owner: &str) -> EvictionReport {
        let RegistryOptions {
            owner_row_budget: budget,
            owner_byte_budget: budget_bytes,
        } = self.options();
        let mut g = self.lock();
        let (run, focused) = g
            .owners
            .get(owner)
            .map(|s| (s.run, s.focused.clone()))
            .unwrap_or_default();
        let mut mine: Vec<(u64, String, usize, usize, bool, bool)> = g
            .sets
            .values()
            .filter(|e| e.owner == owner)
            .map(|e| {
                let protected = e.run == run
                    || focused.as_deref() == Some(e.set.id())
                    || e.set.state() == ResultState::Streaming;
                (
                    e.seq,
                    e.set.id().to_string(),
                    e.set.row_count(),
                    e.set.approx_bytes(),
                    protected,
                    e.set.has_pending(),
                )
            })
            .collect();
        mine.sort_by_key(|m| m.0);
        let mut retained: usize = mine.iter().map(|m| m.2).sum();
        let mut retained_bytes: usize = mine.iter().map(|m| m.3).sum();
        let mut report = EvictionReport {
            budget,
            budget_bytes,
            ..EvictionReport::default()
        };
        for (_, id, rows, bytes, protected, pending) in mine {
            if retained <= budget && retained_bytes <= budget_bytes {
                break;
            }
            if protected {
                continue;
            }
            if pending {
                let n = g
                    .sets
                    .get(&id)
                    .map_or(0, |e| e.set.pending_counts().total_changes);
                report.held_by_pending.push(PendingHold {
                    result_id: id,
                    pending: n,
                });
                continue;
            }
            g.sets.remove(&id);
            retained -= rows;
            retained_bytes -= bytes;
            report.evicted.push(EvictedResult {
                result_id: id,
                rows,
            });
        }
        report.retained_rows = retained;
        report.retained_bytes = retained_bytes;
        report
    }

    /// Evict one result now. A result holding pending edits is refused unless
    /// `discard_pending` (the UI's explicit "discard N pending edits" confirmation).
    pub fn evict(&self, id: &str, discard_pending: bool) -> AppResult<Option<EvictedResult>> {
        let mut g = self.lock();
        let Some(e) = g.sets.get(id) else {
            return Ok(None);
        };
        let pending = e.set.pending_counts().total_changes;
        if pending > 0 && !discard_pending {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("this result holds {pending} pending edit(s)"),
            )
            .with_hint("commit or discard the pending edits first")
            .with_data(serde_json::json!({ "pending": pending })));
        }
        let rows = e.set.row_count();
        g.sets.remove(id);
        Ok(Some(EvictedResult {
            result_id: id.to_string(),
            rows,
        }))
    }

    /// Drop a result (the UI closed it). Pending edits go with it — the caller confirmed.
    pub fn release(&self, id: &str) -> Option<Arc<ResultSet>> {
        self.lock().sets.remove(id).map(|e| e.set)
    }

    /// Drop every result of an owner (tab closed); returns the released ids.
    pub fn release_owner(&self, owner: &str) -> Vec<String> {
        let mut g = self.lock();
        let ids: Vec<String> = g
            .sets
            .iter()
            .filter(|(_, e)| e.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            g.sets.remove(id);
        }
        g.owners.remove(owner);
        ids
    }
}

// ---------------------------------------------------------------------------------------------
// Sink adapter
// ---------------------------------------------------------------------------------------------

/// What [`BufferSink`] reports to its observer — the editor's event stream. Serializes as
/// `{"type": "...", ...}` (snake_case), close to `QueryEvent` in `ui/src/lib/ipc/query.ts`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BufferEvent {
    /// `from`/`to`: byte range of the statement text sent (trimmed) in the submitted text.
    StatementStarted {
        index: usize,
        from: usize,
        to: usize,
    },
    /// A row-returning statement produced a new result set (fetch rows by `result_id`).
    Result {
        index: usize,
        result_id: String,
        columns: Vec<ColumnMeta>,
    },
    /// Rows retained so far (throttled; the final count is also in `StatementDone`).
    Rows {
        index: usize,
        result_id: String,
        row_count: usize,
        truncated: bool,
    },
    StatementDone {
        index: usize,
        command: String,
        rows_affected: u64,
        duration_ms: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        result_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        row_count: Option<usize>,
        truncated: bool,
    },
    /// The statement failed; rows streamed before the failure stay in `result_id`.
    Error {
        index: usize,
        error: SqlError,
        #[serde(skip_serializing_if = "Option::is_none")]
        result_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        row_count: Option<usize>,
    },
    Notice {
        index: Option<usize>,
        notice: Notice,
    },
    /// Cancelled; rows streamed so far stay in `result_id` (partial + banner).
    Cancelled {
        index: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        result_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        row_count: Option<usize>,
    },
    Txn {
        status: TxnStatus,
    },
    /// Older results of this owner were evicted to stay within its budget.
    Evicted {
        report: EvictionReport,
    },
    Finished {
        statements: usize,
        succeeded: usize,
        errors: usize,
        skipped: usize,
        cancelled: bool,
        connection_lost: bool,
        transaction_lost: bool,
        txn: TxnStatus,
        duration_ms: f64,
    },
}

/// Tuning for [`BufferSink`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BufferOptions {
    pub row_cap: usize,
    /// Retained-byte ceiling per result set ([`DEFAULT_BYTE_CAP`]); whatever comes first with
    /// `row_cap` truncates the result.
    pub byte_cap: usize,
    /// `Rows` events at most this often…
    pub progress_interval: Duration,
    /// …or after this many new rows, whichever comes first (the first batch always reports).
    pub progress_rows: usize,
}

impl Default for BufferOptions {
    fn default() -> Self {
        Self {
            row_cap: DEFAULT_ROW_CAP,
            byte_cap: DEFAULT_BYTE_CAP,
            progress_interval: Duration::from_millis(100),
            progress_rows: 10_000,
        }
    }
}

struct Current {
    index: usize,
    set: Arc<ResultSet>,
    last_emit: Option<Instant>,
    emitted_rows: usize,
}

/// A [`ResultSink`] that buffers every result set into a [`ResultSet`] registered under
/// `owner`, reporting progress through `observer`. Construct one per run (it starts a new
/// registry run for the owner); call [`Self::finish`] with the run's summary.
pub struct BufferSink<F: FnMut(BufferEvent) + Send> {
    registry: Arc<ResultRegistry>,
    owner: String,
    opts: BufferOptions,
    observer: F,
    current: Option<Current>,
    results: Vec<String>,
}

impl<F: FnMut(BufferEvent) + Send> BufferSink<F> {
    pub fn new(
        registry: Arc<ResultRegistry>,
        owner: impl Into<String>,
        opts: BufferOptions,
        observer: F,
    ) -> Self {
        let owner = owner.into();
        registry.begin_run(&owner);
        Self {
            registry,
            owner,
            opts,
            observer,
            current: None,
            results: Vec::new(),
        }
    }

    /// Ids of the result sets created by this run, in order.
    pub fn results(&self) -> &[String] {
        &self.results
    }

    /// Emit `Finished` from the run's summary.
    pub fn finish(&mut self, summary: &RunSummary) {
        self.close(None, ResultState::Interrupted);
        let (connection_lost, transaction_lost) = match summary.outcome {
            RunOutcome::ConnectionLost {
                transaction_lost, ..
            } => (true, transaction_lost),
            _ => (false, false),
        };
        (self.observer)(BufferEvent::Finished {
            statements: summary.total,
            succeeded: summary.succeeded,
            errors: summary.failed,
            skipped: summary.skipped,
            cancelled: matches!(summary.outcome, RunOutcome::Cancelled { .. }),
            connection_lost,
            transaction_lost,
            txn: summary.txn,
            duration_ms: summary.duration_ms,
        });
    }

    /// End the current result set of statement `index` (any statement when `None`); returns
    /// its id and row count.
    fn close(&mut self, index: Option<usize>, state: ResultState) -> Option<(String, usize, bool)> {
        if index.is_some_and(|i| self.current.as_ref().is_some_and(|c| c.index != i)) {
            return None;
        }
        let cur = self.current.take()?;
        cur.set.finish(state);
        let rows = cur.set.row_count();
        let truncated = cur.set.truncated();
        if rows != cur.emitted_rows {
            (self.observer)(BufferEvent::Rows {
                index: cur.index,
                result_id: cur.set.id().to_string(),
                row_count: rows,
                truncated,
            });
        }
        Some((cur.set.id().to_string(), rows, truncated))
    }

    fn enforce(&mut self) {
        let report = self.registry.enforce_budget(&self.owner);
        if !report.evicted.is_empty() {
            (self.observer)(BufferEvent::Evicted { report });
        }
    }
}

impl<F: FnMut(BufferEvent) + Send> Drop for BufferSink<F> {
    fn drop(&mut self) {
        // a dropped run future leaves the last set streaming forever otherwise
        if let Some(cur) = self.current.take() {
            cur.set.finish(ResultState::Interrupted);
        }
    }
}

impl<F: FnMut(BufferEvent) + Send> ResultSink for BufferSink<F> {
    fn started(&mut self, index: usize, statement: &StatementRange) {
        (self.observer)(BufferEvent::StatementStarted {
            index,
            from: statement.trimmed.start,
            to: statement.trimmed.end,
        });
    }

    fn columns(&mut self, index: usize, columns: &[ColumnMeta]) {
        // a second result set from the same statement (e.g. a multi-statement procedure)
        self.close(None, ResultState::Complete);
        let set = Arc::new(ResultSet::with_limits(
            columns.to_vec(),
            self.opts.row_cap,
            self.opts.byte_cap,
        ));
        self.registry.register(&self.owner, set.clone());
        self.results.push(set.id().to_string());
        (self.observer)(BufferEvent::Result {
            index,
            result_id: set.id().to_string(),
            columns: columns.to_vec(),
        });
        self.current = Some(Current {
            index,
            set,
            last_emit: None,
            emitted_rows: 0,
        });
        self.enforce();
    }

    fn rows(&mut self, index: usize, rows: RowBatch<'_>) {
        let Some(cur) = self.current.as_mut().filter(|c| c.index == index) else {
            return;
        };
        cur.set.append_batch(rows);
        let count = cur.set.row_count();
        let due = match cur.last_emit {
            None => true,
            Some(t) => {
                t.elapsed() >= self.opts.progress_interval
                    || count.saturating_sub(cur.emitted_rows) >= self.opts.progress_rows
            }
        };
        if due && count != cur.emitted_rows {
            cur.last_emit = Some(Instant::now());
            cur.emitted_rows = count;
            let truncated = cur.set.truncated();
            let result_id = cur.set.id().to_string();
            (self.observer)(BufferEvent::Rows {
                index,
                result_id,
                row_count: count,
                truncated,
            });
        }
    }

    fn notice(&mut self, index: Option<usize>, notice: &Notice) {
        (self.observer)(BufferEvent::Notice {
            index,
            notice: notice.clone(),
        });
    }

    fn completed(&mut self, index: usize, done: &super::executor::StatementDone) {
        let closed = self.close(Some(index), ResultState::Complete);
        (self.observer)(BufferEvent::StatementDone {
            index,
            command: done.command.clone(),
            rows_affected: done.rows_affected,
            duration_ms: done.duration_ms,
            result_id: closed.as_ref().map(|c| c.0.clone()),
            row_count: closed.as_ref().map(|c| c.1),
            truncated: closed.as_ref().is_some_and(|c| c.2),
        });
        self.enforce();
    }

    fn failed(&mut self, index: usize, error: &SqlError) {
        let closed = self.close(Some(index), ResultState::Interrupted);
        (self.observer)(BufferEvent::Error {
            index,
            error: error.clone(),
            result_id: closed.as_ref().map(|c| c.0.clone()),
            row_count: closed.as_ref().map(|c| c.1),
        });
    }

    fn cancelled(&mut self, index: usize) {
        let closed = self.close(Some(index), ResultState::Cancelled);
        (self.observer)(BufferEvent::Cancelled {
            index,
            result_id: closed.as_ref().map(|c| c.0.clone()),
            row_count: closed.as_ref().map(|c| c.1),
        });
    }

    fn txn_changed(&mut self, status: TxnStatus) {
        (self.observer)(BufferEvent::Txn { status });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_numbers() {
        for ok in ["0", "-1", "1.50", "1e+20", "-0.5E-3", "123"] {
            assert!(is_json_number(ok), "{ok}");
        }
        for bad in [
            "NaN",
            "Infinity",
            "-Infinity",
            "01",
            "1.",
            ".5",
            "",
            "-",
            "1e",
            "+1",
        ] {
            assert!(!is_json_number(bad), "{bad}");
        }
    }

    #[test]
    fn json_kinds_by_oid_and_name() {
        let m = |oid: Option<u32>, name: Option<&str>| ColumnMeta {
            name: "x".into(),
            type_oid: oid,
            type_name: name.map(str::to_string),
        };
        assert_eq!(json_kind(&m(Some(23), None)), JsonKind::Number);
        assert_eq!(json_kind(&m(None, Some("numeric(10,2)"))), JsonKind::Number);
        assert_eq!(json_kind(&m(None, Some("integer[]"))), JsonKind::Text);
        assert_eq!(json_kind(&m(None, Some("boolean"))), JsonKind::Bool);
        assert_eq!(json_kind(&m(None, Some("jsonb"))), JsonKind::Json);
        assert_eq!(json_kind(&m(Some(25), Some("int4"))), JsonKind::Text);
    }
}
