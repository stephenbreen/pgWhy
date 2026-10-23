//! Sequential multi-statement executor over one dedicated connection.
//!
//! Each statement range is sent on its own with the simple-query protocol: it streams rows,
//! returns every type as text (NULL distinct from `''`), and accepts utility statements —
//! everything an editor needs, with no per-type decoding. The protocol does not carry column
//! type OIDs through tokio-postgres's API, so for row-returning statements a best-effort
//! `prepare` (describe, never executed) supplies them first; see [`RunOptions::describe`].
//!
//! Results go to a [`ResultSink`] synchronously, in protocol order. Rows arrive in
//! [`RowBatch`]es that borrow the wire buffers (`Option<&str>` per cell, no per-cell
//! allocation) — U12 appends them into its columnar arena. A batch is flushed when it reaches
//! [`RunOptions::batch_rows`] or the socket has nothing more ready, so the executor never
//! holds more than one batch and rows reach the sink incrementally; TCP backpressure does the
//! rest when the sink is slow.
//!
//! Notices are interleaved by draining the connection's notice channel before each protocol
//! message is handled: the driver forwards a notice before it forwards any later message of
//! the same response, so a notice is never reported after the statement that raised it.
//!
//! Every statement on the connection must go through the session for its transaction status
//! ([`super::txn`]) to stay true; [`QuerySession::resync_txn`] re-derives it from the server.

use super::cancel::CancelHandle;
use super::splitter::{
    analyze_with, classify, split_range_with, StatementInfo, StatementRange, TxnControl,
};
use super::txn::{
    SessionState, StatementOutcome, TxnStatus, TxnTracker, IN_FAILED_SQL_TRANSACTION, PROBE_SQL,
};
use crate::conn::{classify as classify_pg, Notice, PgConn};
use crate::{AppError, AppResult, ErrorClass, ErrorKind};
use futures_util::{FutureExt, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_postgres::error::ErrorPosition;
use tokio_postgres::{SimpleQueryMessage, SimpleQueryRow};

// ---------------------------------------------------------------------------------------------
// Result shapes
// ---------------------------------------------------------------------------------------------

/// One result column. Types come from the describe step and are absent when it was skipped or
/// failed (the grid then treats every value as text).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_oid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
}

/// A batch of rows borrowed from the protocol buffers. Values are the server's text output;
/// `None` is SQL NULL.
#[derive(Clone, Copy)]
pub struct RowBatch<'a> {
    rows: &'a [SimpleQueryRow],
}

impl<'a> RowBatch<'a> {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Cell `(row, col)`; `None` for NULL or out of range.
    pub fn get(&self, row: usize, col: usize) -> Option<&'a str> {
        self.rows
            .get(row)
            .and_then(|r| r.try_get(col).ok().flatten())
    }

    pub fn rows(&self) -> impl ExactSizeIterator<Item = RowRef<'a>> + 'a {
        self.rows.iter().map(RowRef)
    }
}

impl std::fmt::Debug for RowBatch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowBatch")
            .field("len", &self.len())
            .finish()
    }
}

/// One borrowed row.
#[derive(Clone, Copy)]
pub struct RowRef<'a>(&'a SimpleQueryRow);

impl<'a> RowRef<'a> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, col: usize) -> Option<&'a str> {
        self.0.try_get(col).ok().flatten()
    }

    pub fn cells(&self) -> impl ExactSizeIterator<Item = Option<&'a str>> + 'a {
        let row = self.0;
        (0..row.len()).map(move |i| row.try_get(i).ok().flatten())
    }
}

/// A statement that finished successfully.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StatementDone {
    /// Best-effort command label from the statement text ([`StatementInfo::command`]).
    pub command: String,
    /// The count from the server's command tag (rows inserted/updated/deleted/selected).
    pub rows_affected: u64,
    /// Rows actually streamed to the sink.
    pub rows_returned: u64,
    pub duration_ms: f64,
}

/// A statement failure, with its position mapped onto the original text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SqlError {
    pub kind: ErrorKind,
    pub class: ErrorClass,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlstate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The server's CONTEXT (`where`) — PL/pgSQL call stacks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Byte offset into the full text passed to `run` (not the statement), for decoration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
}

impl SqlError {
    fn plain(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            class: kind.default_class(),
            message: message.into(),
            severity: None,
            sqlstate: None,
            detail: None,
            hint: None,
            context: None,
            position: None,
        }
    }

    /// From a tokio-postgres error raised by statement text that starts at byte `base` of the
    /// full document.
    fn from_pg(err: &tokio_postgres::Error, stmt: &str, base: usize) -> Self {
        let app = classify_pg(err, "run the statement");
        let Some(db) = err.as_db_error() else {
            let mut e = Self::plain(app.kind, app.message);
            e.class = app.class;
            e.detail = app.detail;
            return e;
        };
        let position = match db.position() {
            Some(ErrorPosition::Original(p)) => Some(base + char_position_to_byte(stmt, *p)),
            _ => None,
        };
        Self {
            kind: app.kind,
            class: app.class,
            message: db.message().to_string(),
            severity: Some(db.severity().to_string()),
            sqlstate: Some(db.code().code().to_string()),
            detail: db.detail().map(str::to_string),
            hint: db.hint().map(str::to_string),
            context: db.where_().map(str::to_string),
            position,
        }
    }

    /// The taxonomy form, for callers that surface the failure as an operation error.
    pub fn to_app_error(&self) -> AppError {
        let mut e = AppError::new(self.kind, self.message.clone()).with_class(self.class);
        let mut detail = self.detail.clone().unwrap_or_default();
        if let Some(code) = &self.sqlstate {
            if !detail.is_empty() {
                detail.push('\n');
            }
            detail.push_str("SQLSTATE ");
            detail.push_str(code);
        }
        if !detail.is_empty() {
            e = e.with_detail(detail);
        }
        if let Some(h) = &self.hint {
            e = e.with_hint(h.clone());
        }
        e.with_data(serde_json::json!({
            "sqlstate": self.sqlstate,
            "position": self.position,
        }))
    }
}

/// Postgres reports error positions as a 1-based *character* index into the statement; map it
/// to a byte offset into that statement's UTF-8 text (clamped to its end).
pub fn char_position_to_byte(stmt: &str, position: u32) -> usize {
    let n = (position as usize).saturating_sub(1);
    stmt.char_indices()
        .nth(n)
        .map(|(b, _)| b)
        .unwrap_or(stmt.len())
}

/// Receives one run's results, in order. All methods default to no-ops.
///
/// Per statement: `started`, then zero or more result sets (`columns` followed by `rows`
/// batches; a second `columns` for the same index starts a new result set), then exactly one
/// of `completed` / `failed` / `cancelled`. `notice` may arrive at any point; its index is the
/// statement that raised it, `None` for notices left over from before the run.
pub trait ResultSink: Send {
    fn started(&mut self, _index: usize, _statement: &StatementRange) {}
    fn columns(&mut self, _index: usize, _columns: &[ColumnMeta]) {}
    fn rows(&mut self, _index: usize, _rows: RowBatch<'_>) {}
    fn notice(&mut self, _index: Option<usize>, _notice: &Notice) {}
    fn completed(&mut self, _index: usize, _done: &StatementDone) {}
    fn failed(&mut self, _index: usize, _error: &SqlError) {}
    /// The statement was cancelled; queued statements are abandoned.
    fn cancelled(&mut self, _index: usize) {}
    /// Transaction status changed after a statement (for live tab badges).
    fn txn_changed(&mut self, _status: TxnStatus) {}
}

/// What to do with the remaining statements after one fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPolicy {
    #[default]
    StopOnError,
    Continue,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunOptions {
    pub on_error: ErrorPolicy,
    /// Describe row-returning statements first to learn column types. Skipped in a failed
    /// transaction (it would only fail). Inside an open transaction a describe failure is
    /// reported as the statement's error — it is the same error execution would raise, and
    /// the server has already aborted the block.
    pub describe: bool,
    /// Rows per [`ResultSink::rows`] call, at most.
    pub batch_rows: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            on_error: ErrorPolicy::StopOnError,
            describe: true,
            batch_rows: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunOutcome {
    /// Every statement ran (some may have failed under [`ErrorPolicy::Continue`]).
    Completed,
    StoppedOnError {
        index: usize,
    },
    /// Cancelled while running statement `index`, or before starting it.
    Cancelled {
        index: usize,
    },
    /// The connection died at statement `index`. `transaction_lost` when a transaction was
    /// open — the server rolled it back (R19 "transaction lost").
    ConnectionLost {
        index: usize,
        transaction_lost: bool,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RunSummary {
    pub outcome: RunOutcome,
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    /// Statements never started (stopped, cancelled or connection lost).
    pub skipped: usize,
    /// The first failure, by statement index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_error: Option<(usize, SqlError)>,
    pub txn: TxnStatus,
    pub duration_ms: f64,
}

// ---------------------------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------------------------

struct AbortOnDrop(Option<tokio::task::AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

struct RunLock {
    notices: Option<mpsc::UnboundedReceiver<Notice>>,
}

/// One dedicated connection plus its run lock, transaction status and cancel handle — what
/// an editor tab, a grid, or a workflow step holds.
/// Bounded per-session cache of describe results, keyed by statement text. Describing costs a
/// round trip (Parse + Describe) before every row-returning statement; re-running the same text
/// (F5 on the same query, a script loop) should not pay it twice.
///
/// Cleared whenever the session runs a statement that returns no rows: in-session DDL can change
/// a later statement's result columns, and the cache must never outlive that. Cross-session DDL
/// between two identical runs can still leave a stale *type label* on a column (row text comes
/// from the server either way), which is the accepted cost of not paying a round trip per run.
#[derive(Default)]
struct DescribeCache {
    entries: std::collections::HashMap<String, Arc<Vec<(u32, String)>>>,
    order: std::collections::VecDeque<String>,
}

impl DescribeCache {
    const CAP: usize = 128;

    fn get(&self, text: &str) -> Option<Arc<Vec<(u32, String)>>> {
        self.entries.get(text).cloned()
    }

    fn put(&mut self, text: &str, cols: Vec<(u32, String)>) {
        if self.entries.len() >= Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
        self.entries.insert(text.to_string(), Arc::new(cols));
        self.order.push_back(text.to_string());
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

pub struct QuerySession {
    conn: PgConn,
    lock: tokio::sync::Mutex<RunLock>,
    txn: Mutex<TxnTracker>,
    describe: Mutex<DescribeCache>,
    running: AtomicBool,
    /// Set when a statement saw the server end the session (a FATAL error arrives before the
    /// socket closes, so `PgConn::is_closed` can lag behind).
    lost: AtomicBool,
    /// The session's `standard_conforming_strings`, as last read from the server (the default,
    /// `on`, until the first read). The splitter needs it: with it off, `'\'` does not end a
    /// string, so a wrongly split script would send a fragment of a literal as a statement.
    /// Refreshed at the start of every run and after any statement that may change it (see
    /// [`may_change_scs`]); ranges handed to [`Self::run`] are the caller's responsibility.
    scs: AtomicBool,
    state: Arc<watch::Sender<SessionState>>,
    cancel: CancelHandle,
    _watcher: AbortOnDrop,
}

impl std::fmt::Debug for QuerySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuerySession")
            .field("conn", &self.conn)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// Clears the running flag even when the run future is dropped mid-way.
struct Running<'a>(&'a QuerySession);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
        self.0
            .cancel
            .shared
            .in_flight
            .store(false, Ordering::SeqCst);
        self.0.publish();
    }
}

enum StmtResult {
    Done(StatementDone),
    Error(SqlError),
    Cancelled,
    /// Cancelled before it was sent: no transaction effect.
    Abandoned,
    Lost(SqlError),
}

impl QuerySession {
    /// Wrap a freshly opened connection. Takes its notice stream. Assumes no transaction is
    /// open (true for a new connection); call [`Self::resync_txn`] otherwise.
    pub fn new(conn: PgConn) -> Self {
        let notices = conn.take_notices();
        let state = Arc::new(watch::channel(SessionState::Idle).0);
        let watcher = tokio::runtime::Handle::try_current().ok().map(|rt| {
            let closed = conn.closed();
            let tx = state.clone();
            rt.spawn(async move {
                closed.await;
                tx.send_replace(SessionState::Disconnected);
            })
            .abort_handle()
        });
        let cancel = CancelHandle::new(&conn);
        Self {
            conn,
            lock: tokio::sync::Mutex::new(RunLock { notices }),
            txn: Mutex::new(TxnTracker::default()),
            describe: Mutex::new(DescribeCache::default()),
            running: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            scs: AtomicBool::new(true),
            state,
            cancel,
            _watcher: AbortOnDrop(watcher),
        }
    }

    pub fn conn(&self) -> &PgConn {
        &self.conn
    }

    /// Give the connection back (e.g. to hand it to another consumer). Its notice stream stays
    /// with the dropped session.
    pub fn into_conn(self) -> PgConn {
        self.conn
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Last known server transaction status (kept after a disconnect, see
    /// [`Self::transaction_lost`]).
    pub fn txn_status(&self) -> TxnStatus {
        self.txn.lock().map(|t| t.status()).unwrap_or_default()
    }

    fn is_closed(&self) -> bool {
        self.lost.load(Ordering::SeqCst) || self.conn.is_closed()
    }

    pub fn state(&self) -> SessionState {
        if self.is_closed() {
            SessionState::Disconnected
        } else if self.running.load(Ordering::SeqCst) {
            SessionState::Running
        } else {
            SessionState::from_txn(self.txn_status())
        }
    }

    /// State changes, including `Disconnected` when the server ends the connection while idle.
    pub fn watch_state(&self) -> watch::Receiver<SessionState> {
        self.state.subscribe()
    }

    /// True when the connection is gone while a transaction was open — its work was rolled back
    /// by the server and the tab must say so (R19 / SM-6).
    pub fn transaction_lost(&self) -> bool {
        self.is_closed() && self.txn_status() != TxnStatus::Idle
    }

    fn publish(&self) {
        self.state.send_replace(self.state());
    }

    fn set_txn(&self, status: TxnStatus) {
        if let Ok(mut t) = self.txn.lock() {
            t.set(status);
        }
    }

    fn acquire(&self) -> AppResult<tokio::sync::MutexGuard<'_, RunLock>> {
        let guard = self.lock.try_lock().map_err(|_| busy_error())?;
        if self.is_closed() {
            return Err(disconnected_error(self.txn_status() != TxnStatus::Idle).to_app_error());
        }
        Ok(guard)
    }

    /// Whether [`Self::run`] would be refused right now (busy or disconnected), *without*
    /// starting it. Callers that release something to make room for the run (an editor tab
    /// dropping its previous results) check this first, so a refused run loses nothing.
    pub fn check_can_start(&self) -> AppResult<()> {
        if self.lock.try_lock().is_err() {
            return Err(busy_error());
        }
        if self.is_closed() {
            return Err(disconnected_error(self.txn_status() != TxnStatus::Idle).to_app_error());
        }
        Ok(())
    }

    fn start_running(&self) -> Running<'_> {
        self.cancel.shared.requested.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        self.publish();
        Running(self)
    }

    /// Split `sql` and run every statement.
    pub async fn run_text<S: ResultSink + ?Sized>(
        &self,
        sql: &str,
        options: &RunOptions,
        sink: &mut S,
    ) -> AppResult<RunSummary> {
        self.run_within(sql, 0..sql.len(), options, sink).await
    }

    /// Split and run only `within` of `sql` (e.g. the editor selection), with the splitter
    /// following the session's `standard_conforming_strings`: it is read before splitting and
    /// re-read (re-splitting the rest) after any statement that may change it, so
    /// `set standard_conforming_strings = off; select '\'; …` splits as the server will lex it.
    pub async fn run_within<S: ResultSink + ?Sized>(
        &self,
        sql: &str,
        within: std::ops::Range<usize>,
        options: &RunOptions,
        sink: &mut S,
    ) -> AppResult<RunSummary> {
        let mut lock = self.acquire()?;
        let _running = self.start_running();
        self.refresh_scs().await;
        let scs = self.scs.load(Ordering::SeqCst);
        let statements = split_range_with(sql, within.clone(), scs);
        Ok(self
            .run_locked(&mut lock, sql, &statements, Some(within), options, sink)
            .await)
    }

    /// Split `within` of `sql` as this session's server will lex it. Reads the setting first
    /// unless a run is in flight (then the last known value is used). For callers that need the
    /// ranges before running; [`Self::run_within`] does this itself.
    pub async fn split_range(
        &self,
        sql: &str,
        within: std::ops::Range<usize>,
    ) -> Vec<StatementRange> {
        if self.lock.try_lock().is_ok() {
            self.refresh_scs().await;
        }
        split_range_with(sql, within, self.scs.load(Ordering::SeqCst))
    }

    /// Read `standard_conforming_strings` from the server into the cache. On failure (aborted
    /// transaction, lost connection) the cached value stays: a failed transaction cannot have
    /// changed it, and the `ROLLBACK` that ends it triggers a re-read.
    async fn refresh_scs(&self) -> bool {
        if let Ok(msgs) = self
            .conn
            .client
            .simple_query("show standard_conforming_strings")
            .await
        {
            for m in &msgs {
                if let SimpleQueryMessage::Row(r) = m {
                    if let Ok(Some(v)) = r.try_get(0) {
                        self.scs.store(v == "on", Ordering::SeqCst);
                    }
                }
            }
        }
        self.scs.load(Ordering::SeqCst)
    }

    /// Run `statements` (ranges into `sql`, e.g. from [`super::splitter::split_range`] over
    /// the selection) sequentially. Statement failures are reported through the sink and the
    /// summary; `Err` only when the run could not start (busy, disconnected).
    pub async fn run<S: ResultSink + ?Sized>(
        &self,
        sql: &str,
        statements: &[StatementRange],
        options: &RunOptions,
        sink: &mut S,
    ) -> AppResult<RunSummary> {
        let mut lock = self.acquire()?;
        let _running = self.start_running();
        Ok(self
            .run_locked(&mut lock, sql, statements, None, options, sink)
            .await)
    }

    async fn run_locked<S: ResultSink + ?Sized>(
        &self,
        lock: &mut RunLock,
        sql: &str,
        statements: &[StatementRange],
        resplit: Option<std::ops::Range<usize>>,
        options: &RunOptions,
        sink: &mut S,
    ) -> RunSummary {
        // `resplit` is the span `statements` was split from: when a statement may have flipped
        // `standard_conforming_strings`, the rest of that span is split again under the new value.
        let mut statements = statements.to_vec();
        let started = Instant::now();
        let mut summary = RunSummary {
            outcome: RunOutcome::Completed,
            total: statements.len(),
            succeeded: 0,
            failed: 0,
            skipped: 0,
            first_error: None,
            txn: self.txn_status(),
            duration_ms: 0.0,
        };
        drain_notices(&mut lock.notices, None, &mut Vec::new(), None, sink);

        let mut next = 0;
        while next < statements.len() {
            let i = next;
            next += 1;
            let st = statements[i].clone();
            let st = &st;
            if self.cancel.requested() {
                summary.outcome = RunOutcome::Cancelled { index: i };
                summary.skipped = statements.len() - i;
                break;
            }
            let text = st.text(sql);
            let info = classify(text);
            sink.started(i, st);
            let before = self.txn_status();
            let result = if info.copy_client {
                let mut e = SqlError::plain(
                    ErrorKind::Refused,
                    "COPY to or from the client (STDIN/STDOUT) is not supported here",
                );
                e.hint = Some(
                    "COPY to/from a server-side file instead, or use the import/export actions"
                        .into(),
                );
                e.position = Some(st.trimmed.start);
                // never sent: no transaction effect
                sink.failed(i, &e);
                summary.failed += 1;
                summary.first_error.get_or_insert((i, e));
                if options.on_error == ErrorPolicy::StopOnError {
                    summary.outcome = RunOutcome::StoppedOnError { index: i };
                    summary.skipped = statements.len() - i - 1;
                    break;
                }
                continue;
            } else {
                self.exec_one(lock, i, st, text, &info, options, sink).await
            };
            // a lost connection keeps the last status: it is what `transaction_lost` reports
            let outcome = match &result {
                StmtResult::Done(_) => Some(StatementOutcome::Ok),
                StmtResult::Abandoned | StmtResult::Lost(_) => None,
                _ => Some(StatementOutcome::Error),
            };
            if let (Some(outcome), Ok(mut t)) = (outcome, self.txn.lock()) {
                t.after_statement(info.txn_control, outcome);
            }
            let mut stop = None;
            match result {
                StmtResult::Done(done) => {
                    summary.succeeded += 1;
                    sink.completed(i, &done);
                }
                StmtResult::Error(e) => {
                    summary.failed += 1;
                    sink.failed(i, &e);
                    summary.first_error.get_or_insert((i, e));
                    if options.on_error == ErrorPolicy::StopOnError {
                        stop = Some(RunOutcome::StoppedOnError { index: i });
                    }
                }
                StmtResult::Cancelled | StmtResult::Abandoned => {
                    sink.cancelled(i);
                    stop = Some(RunOutcome::Cancelled { index: i });
                }
                StmtResult::Lost(e) => {
                    summary.failed += 1;
                    sink.failed(i, &e);
                    summary.first_error.get_or_insert((i, e));
                    stop = Some(RunOutcome::ConnectionLost {
                        index: i,
                        transaction_lost: before != TxnStatus::Idle,
                    });
                }
            }
            let lost = matches!(stop, Some(RunOutcome::ConnectionLost { .. }));
            if !lost && self.txn.lock().map(|t| t.uncertain()).unwrap_or(false) {
                if let Ok(status) = self.probe().await {
                    self.set_txn(status);
                }
            }
            if let (Some(within), false) = (&resplit, lost) {
                if may_change_scs(text, &info) {
                    let before = self.scs.load(Ordering::SeqCst);
                    if self.refresh_scs().await != before {
                        let scs = self.scs.load(Ordering::SeqCst);
                        statements.truncate(i + 1);
                        statements.extend(split_range_with(sql, st.range.end..within.end, scs));
                    }
                }
            }
            let after = self.txn_status();
            if after != before && !lost {
                sink.txn_changed(after);
            }
            self.publish();
            if let Some(outcome) = stop {
                summary.outcome = outcome;
                summary.skipped = statements.len() - i - 1;
                break;
            }
        }
        drain_notices(
            &mut lock.notices,
            statements.len().checked_sub(1),
            &mut Vec::new(),
            None,
            sink,
        );
        summary.total = statements.len();
        summary.txn = self.txn_status();
        summary.duration_ms = ms(started.elapsed());
        summary
    }

    #[allow(clippy::too_many_arguments)]
    async fn exec_one<S: ResultSink + ?Sized>(
        &self,
        lock: &mut RunLock,
        index: usize,
        st: &StatementRange,
        text: &str,
        info: &StatementInfo,
        options: &RunOptions,
        sink: &mut S,
    ) -> StmtResult {
        let started = Instant::now();
        let base = st.trimmed.start;
        let shared = &self.cancel.shared;
        shared.in_flight.store(true, Ordering::SeqCst);
        if shared.requested.load(Ordering::SeqCst) {
            shared.in_flight.store(false, Ordering::SeqCst);
            return StmtResult::Abandoned;
        }
        let result = self
            .exec_stream(lock, index, text, base, info, options, sink, started)
            .await;
        if !info.returns_rows {
            // DDL (or any non-row statement) can change what a later statement returns
            self.describe
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        }
        shared.in_flight.store(false, Ordering::SeqCst);
        drain_notices(&mut lock.notices, Some(index), &mut Vec::new(), None, sink);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn exec_stream<S: ResultSink + ?Sized>(
        &self,
        lock: &mut RunLock,
        index: usize,
        text: &str,
        base: usize,
        info: &StatementInfo,
        options: &RunOptions,
        sink: &mut S,
        started: Instant,
    ) -> StmtResult {
        let client = &self.conn.client;
        let txn = self.txn_status();
        let mut described: Option<Arc<Vec<(u32, String)>>> = None;
        if options.describe && info.returns_rows && txn != TxnStatus::Failed {
            described = self
                .describe
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(text);
        }
        if options.describe && info.returns_rows && txn != TxnStatus::Failed && described.is_none()
        {
            match client.prepare(text).await {
                Ok(stmt) => {
                    let cols: Vec<(u32, String)> = stmt
                        .columns()
                        .iter()
                        .map(|c| (c.type_().oid(), c.type_().name().to_string()))
                        .collect();
                    self.describe
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .put(text, cols.clone());
                    described = Some(Arc::new(cols));
                }
                Err(e) => {
                    if let Some(r) = self.lost_or_cancelled(&e) {
                        return r;
                    }
                    if txn == TxnStatus::InTransaction && e.as_db_error().is_some() {
                        return StmtResult::Error(SqlError::from_pg(&e, text, base));
                    }
                }
            }
        }

        let stream = match client.simple_query_raw(text).await {
            Ok(s) => s,
            Err(e) => return self.stream_error(&e, text, base),
        };
        let mut stream = std::pin::pin!(stream);
        let cap = options.batch_rows.max(1);
        let mut batch: Vec<SimpleQueryRow> = Vec::with_capacity(cap.min(4096));
        let mut result_sets = 0usize;
        let mut rows_returned = 0u64;
        let mut rows_affected = 0u64;
        loop {
            let next = match stream.next().now_or_never() {
                Some(m) => m,
                None => {
                    flush(&mut batch, index, sink);
                    stream.next().await
                }
            };
            drain_notices(
                &mut lock.notices,
                Some(index),
                &mut batch,
                Some(index),
                sink,
            );
            match next {
                None => break,
                Some(Ok(SimpleQueryMessage::RowDescription(cols))) => {
                    flush(&mut batch, index, sink);
                    let types = if result_sets == 0 {
                        described.as_ref().filter(|d| d.len() == cols.len())
                    } else {
                        None
                    };
                    let meta: Vec<ColumnMeta> = cols
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ColumnMeta {
                            name: c.name().to_string(),
                            type_oid: types.map(|t| t[i].0),
                            type_name: types.map(|t| t[i].1.clone()),
                        })
                        .collect();
                    result_sets += 1;
                    sink.columns(index, &meta);
                }
                Some(Ok(SimpleQueryMessage::Row(row))) => {
                    rows_returned += 1;
                    batch.push(row);
                    if batch.len() >= cap {
                        flush(&mut batch, index, sink);
                    }
                }
                Some(Ok(SimpleQueryMessage::CommandComplete(n))) => {
                    flush(&mut batch, index, sink);
                    rows_affected = n;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    flush(&mut batch, index, sink);
                    return self.stream_error(&e, text, base);
                }
            }
        }
        StmtResult::Done(StatementDone {
            command: info.command.clone(),
            rows_affected,
            rows_returned,
            duration_ms: ms(started.elapsed()),
        })
    }

    fn lost_or_cancelled(&self, e: &tokio_postgres::Error) -> Option<StmtResult> {
        let fatal = e
            .as_db_error()
            .is_some_and(|db| matches!(db.severity(), "FATAL" | "PANIC"));
        if fatal || e.is_closed() || self.conn.is_closed() {
            self.lost.store(true, Ordering::SeqCst);
            let mut err = disconnected_error(self.txn_status() != TxnStatus::Idle);
            if let Some(db) = e.as_db_error() {
                err.detail = Some(format!("{} (SQLSTATE {})", db.message(), db.code().code()));
                err.sqlstate = Some(db.code().code().to_string());
            }
            return Some(StmtResult::Lost(err));
        }
        let cancelled = e
            .as_db_error()
            .is_some_and(|db| db.code().code() == "57014");
        if cancelled && self.cancel.requested() {
            return Some(StmtResult::Cancelled);
        }
        None
    }

    fn stream_error(&self, e: &tokio_postgres::Error, text: &str, base: usize) -> StmtResult {
        self.lost_or_cancelled(e)
            .unwrap_or_else(|| StmtResult::Error(SqlError::from_pg(e, text, base)))
    }

    /// Ask the server which transaction state the connection is in; see [`super::txn`].
    async fn probe(&self) -> AppResult<TxnStatus> {
        match self.conn.client.simple_query(PROBE_SQL).await {
            Ok(msgs) => {
                let idle = msgs.iter().any(|m| match m {
                    SimpleQueryMessage::Row(r) => r.try_get(0).ok().flatten() == Some("t"),
                    _ => false,
                });
                Ok(if idle {
                    TxnStatus::Idle
                } else {
                    TxnStatus::InTransaction
                })
            }
            Err(e)
                if e.as_db_error()
                    .is_some_and(|db| db.code().code() == IN_FAILED_SQL_TRANSACTION) =>
            {
                Ok(TxnStatus::Failed)
            }
            Err(e) => Err(classify_pg(&e, "check the transaction status")),
        }
    }

    /// Re-derive the transaction status from the server (one cheap, side-effect-free round
    /// trip). Use before an R19 close prompt when certainty matters, or after the connection
    /// was used outside this session.
    pub async fn resync_txn(&self) -> AppResult<TxnStatus> {
        let _lock = self.acquire()?;
        let status = self.probe().await?;
        self.set_txn(status);
        self.publish();
        Ok(status)
    }

    /// Run one internal control statement (not reported to any sink); notices it raises are
    /// discarded.
    async fn control_locked(&self, lock: &mut RunLock, sql: &str) -> Result<(), SqlError> {
        let info = classify(sql);
        let res = self.conn.client.simple_query(sql).await;
        if let Some(rx) = lock.notices.as_mut() {
            while rx.try_recv().is_ok() {}
        }
        let res = match res {
            Ok(_) => Ok(()),
            Err(e) => Err(match self.lost_or_cancelled(&e) {
                Some(StmtResult::Lost(err)) => err,
                _ => SqlError::from_pg(&e, sql, 0),
            }),
        };
        if !self.is_closed() {
            let outcome = match &res {
                Ok(()) => StatementOutcome::Ok,
                Err(_) => StatementOutcome::Error,
            };
            if let Ok(mut t) = self.txn.lock() {
                t.after_statement(info.txn_control, outcome);
            }
        }
        if res.is_err()
            && !self.is_closed()
            && self.txn.lock().map(|t| t.uncertain()).unwrap_or(false)
        {
            if let Ok(s) = self.probe().await {
                self.set_txn(s);
            }
        }
        self.publish();
        res
    }

    /// `COMMIT` the open transaction (R19 close/quit prompt). In a failed transaction the
    /// server rolls back instead; the result is still `Idle`.
    pub async fn commit(&self) -> AppResult<TxnStatus> {
        let mut lock = self.acquire()?;
        self.control_locked(&mut lock, "commit")
            .await
            .map_err(|e| e.to_app_error())?;
        Ok(self.txn_status())
    }

    /// `ROLLBACK` the open transaction (R19 close/quit prompt).
    pub async fn rollback(&self) -> AppResult<TxnStatus> {
        let mut lock = self.acquire()?;
        self.control_locked(&mut lock, "rollback")
            .await
            .map_err(|e| e.to_app_error())?;
        Ok(self.txn_status())
    }

    /// Run a whole script as the post-restore step (R22).
    ///
    /// [`ScriptMode::SingleTransaction`] wraps it in `BEGIN … COMMIT`, stopping at the first
    /// failure and rolling back; it refuses — before sending anything — scripts that contain
    /// transaction control or statements that cannot run in a transaction block (offer
    /// [`ScriptMode::NoTransaction`] for those). `NoTransaction` runs each statement in
    /// autocommit, stopping at the first failure (earlier statements stay applied).
    ///
    /// `Err` means the script could not start; a statement failure is reported in the
    /// [`ScriptReport`] (see [`ScriptReport::error`]).
    pub async fn run_script<S: ResultSink + ?Sized>(
        &self,
        sql: &str,
        mode: ScriptMode,
        sink: &mut S,
    ) -> AppResult<ScriptReport> {
        // The analysis (and the pre-checks below) split under the setting as it stands now;
        // `run_locked` re-splits the rest if the script itself changes it.
        let mut lock = self.acquire()?;
        let scs = self.refresh_scs().await;
        let analysis = analyze_with(sql, scs);
        let name = |i: usize| classify(analysis.statements[i].text(sql)).command;
        if mode == ScriptMode::SingleTransaction {
            if let Some(&i) = analysis.txn_control.first() {
                return Err(AppError::new(
                    ErrorKind::Refused,
                    format!(
                        "the script controls its own transaction ({} at statement {}); \
                         it runs inside a single transaction already",
                        name(i),
                        i + 1
                    ),
                )
                .with_hint("remove the BEGIN/COMMIT/ROLLBACK/SAVEPOINT statements")
                .with_data(serde_json::json!({ "statement_index": i })));
            }
            if let Some((i, why)) = analysis.needs_no_transaction.first() {
                return Err(AppError::new(
                    ErrorKind::Refused,
                    format!("statement {}: {why}", i + 1),
                )
                .with_hint("enable \"run without transaction\" for this script")
                .with_data(serde_json::json!({ "statement_index": i })));
            }
        }
        if self.txn_status() != TxnStatus::Idle {
            return Err(AppError::new(
                ErrorKind::Refused,
                "the connection already has an open transaction",
            ));
        }
        let _running = self.start_running();
        let options = RunOptions {
            on_error: ErrorPolicy::StopOnError,
            describe: false,
            ..RunOptions::default()
        };
        if mode == ScriptMode::NoTransaction {
            let summary = self
                .run_locked(
                    &mut lock,
                    sql,
                    &analysis.statements,
                    Some(0..sql.len()),
                    &options,
                    sink,
                )
                .await;
            let committed = summary.outcome == RunOutcome::Completed && summary.failed == 0;
            return Ok(ScriptReport {
                mode,
                summary,
                committed,
                commit_error: None,
            });
        }
        if let Err(e) = self.control_locked(&mut lock, "begin").await {
            return Err(e.to_app_error());
        }
        let summary = self
            .run_locked(
                &mut lock,
                sql,
                &analysis.statements,
                Some(0..sql.len()),
                &options,
                sink,
            )
            .await;
        let lost = matches!(summary.outcome, RunOutcome::ConnectionLost { .. });
        let mut report = ScriptReport {
            mode,
            summary,
            committed: false,
            commit_error: None,
        };
        if lost {
            return Ok(report);
        }
        if report.summary.outcome == RunOutcome::Completed && report.summary.failed == 0 {
            match self.control_locked(&mut lock, "commit").await {
                Ok(()) => report.committed = true,
                Err(e) => {
                    report.commit_error = Some(e);
                    if self.txn_status() != TxnStatus::Idle && !self.is_closed() {
                        let _ = self.control_locked(&mut lock, "rollback").await;
                    }
                }
            }
        } else if let Err(e) = self.control_locked(&mut lock, "rollback").await {
            tracing::warn!(error = %e.message, "rollback after a failed post-restore script failed");
        }
        report.summary.txn = self.txn_status();
        Ok(report)
    }
}

/// Whether running `text` may have changed `standard_conforming_strings` on the session: it
/// names the setting (`SET`, `set_config`, `ALTER … SET` in a `DO` body), or it resets state
/// (`RESET`, `DISCARD`) or ends/rolls back a transaction that could have held a `SET`.
fn may_change_scs(text: &str, info: &StatementInfo) -> bool {
    info.txn_control.is_some_and(|t| t != TxnControl::Begin)
        || matches!(info.command.as_str(), "RESET" | "DISCARD")
        || text
            .to_ascii_lowercase()
            .contains("standard_conforming_strings")
}

/// How [`QuerySession::run_script`] wraps a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptMode {
    #[default]
    SingleTransaction,
    NoTransaction,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScriptReport {
    pub mode: ScriptMode,
    pub summary: RunSummary,
    /// Single-transaction: the COMMIT succeeded. No-transaction: every statement succeeded.
    pub committed: bool,
    /// The COMMIT itself failed (e.g. a deferred constraint); nothing was applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_error: Option<SqlError>,
}

impl ScriptReport {
    /// The failure as an operation error naming the statement, or `None` on success.
    pub fn error(&self) -> Option<AppError> {
        if self.committed {
            return None;
        }
        if let Some(e) = &self.commit_error {
            let mut app = e.to_app_error();
            app.message = format!("the script's transaction failed to commit: {}", e.message);
            return Some(app);
        }
        if let Some((i, e)) = &self.summary.first_error {
            let mut app = e.to_app_error();
            app.message = format!("statement {} failed: {}", i + 1, e.message);
            return Some(app.with_data(serde_json::json!({
                "statement_index": i,
                "sqlstate": e.sqlstate,
                "position": e.position,
            })));
        }
        Some(match self.summary.outcome {
            RunOutcome::Cancelled { index } => AppError::new(
                ErrorKind::Cancelled,
                format!("the script was cancelled at statement {}", index + 1),
            ),
            _ => AppError::internal("the script did not complete"),
        })
    }
}

/// The one "a run is already in flight" refusal (shared by [`QuerySession::acquire`] and
/// [`QuerySession::check_can_start`] so both paths phrase it identically).
fn busy_error() -> AppError {
    AppError::new(
        ErrorKind::Refused,
        "a query is already running on this connection",
    )
}

fn disconnected_error(in_transaction: bool) -> SqlError {
    let mut e = SqlError::plain(
        ErrorKind::Disconnected,
        "the connection to the server was lost",
    );
    if in_transaction {
        e.message =
            "the connection to the server was lost; the open transaction was rolled back".into();
    }
    e.hint = Some("reconnect to continue".into());
    e
}

fn flush<S: ResultSink + ?Sized>(batch: &mut Vec<SimpleQueryRow>, index: usize, sink: &mut S) {
    if !batch.is_empty() {
        sink.rows(index, RowBatch { rows: batch });
        batch.clear();
    }
}

/// Deliver pending notices; rows already batched for `batch_index` go first so a notice is
/// never reported ahead of rows that preceded it.
fn drain_notices<S: ResultSink + ?Sized>(
    rx: &mut Option<mpsc::UnboundedReceiver<Notice>>,
    index: Option<usize>,
    batch: &mut Vec<SimpleQueryRow>,
    batch_index: Option<usize>,
    sink: &mut S,
) {
    let Some(rx) = rx.as_mut() else { return };
    let mut first = true;
    while let Ok(n) = rx.try_recv() {
        if first {
            if let Some(bi) = batch_index {
                flush(batch, bi, sink);
            }
            first = false;
        }
        sink.notice(index, &n);
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------------------------
// A sink that keeps everything (tests, small workflow results, the U11 mock pane)
// ---------------------------------------------------------------------------------------------

/// One entry of [`CollectSink::log`], recording the order events arrived in.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CollectedEvent {
    Started {
        index: usize,
    },
    Columns {
        index: usize,
    },
    Rows {
        index: usize,
        count: usize,
    },
    Notice {
        index: Option<usize>,
        message: String,
    },
    Completed {
        index: usize,
    },
    Failed {
        index: usize,
    },
    Cancelled {
        index: usize,
    },
    TxnChanged {
        status: TxnStatus,
    },
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CollectedStatement {
    pub range: Option<StatementRange>,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Option<String>>>,
    pub done: Option<StatementDone>,
    pub error: Option<SqlError>,
    pub cancelled: bool,
}

/// Collects owned copies of everything. Keeps at most `max_rows` rows per statement (all
/// when `None`) but still counts the rest in [`StatementDone::rows_returned`].
#[derive(Debug, Clone, Default)]
pub struct CollectSink {
    pub max_rows: Option<usize>,
    pub statements: Vec<CollectedStatement>,
    pub notices: Vec<(Option<usize>, Notice)>,
    pub log: Vec<CollectedEvent>,
}

impl CollectSink {
    pub fn new() -> Self {
        Self::default()
    }

    fn at(&mut self, index: usize) -> &mut CollectedStatement {
        if self.statements.len() <= index {
            self.statements
                .resize_with(index + 1, CollectedStatement::default);
        }
        &mut self.statements[index]
    }
}

impl ResultSink for CollectSink {
    fn started(&mut self, index: usize, statement: &StatementRange) {
        self.at(index).range = Some(statement.clone());
        self.log.push(CollectedEvent::Started { index });
    }

    fn columns(&mut self, index: usize, columns: &[ColumnMeta]) {
        self.at(index).columns = columns.to_vec();
        self.log.push(CollectedEvent::Columns { index });
    }

    fn rows(&mut self, index: usize, rows: RowBatch<'_>) {
        let max = self.max_rows.unwrap_or(usize::MAX);
        let st = self.at(index);
        for r in rows.rows() {
            if st.rows.len() >= max {
                break;
            }
            st.rows
                .push(r.cells().map(|c| c.map(str::to_string)).collect());
        }
        self.log.push(CollectedEvent::Rows {
            index,
            count: rows.len(),
        });
    }

    fn notice(&mut self, index: Option<usize>, notice: &Notice) {
        self.notices.push((index, notice.clone()));
        self.log.push(CollectedEvent::Notice {
            index,
            message: notice.message.clone(),
        });
    }

    fn completed(&mut self, index: usize, done: &StatementDone) {
        self.at(index).done = Some(done.clone());
        self.log.push(CollectedEvent::Completed { index });
    }

    fn failed(&mut self, index: usize, error: &SqlError) {
        self.at(index).error = Some(error.clone());
        self.log.push(CollectedEvent::Failed { index });
    }

    fn cancelled(&mut self, index: usize) {
        self.at(index).cancelled = true;
        self.log.push(CollectedEvent::Cancelled { index });
    }

    fn txn_changed(&mut self, status: TxnStatus) {
        self.log.push(CollectedEvent::TxnChanged { status });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_positions_map_to_bytes() {
        let s = "select 'héllo', x";
        // 'x' is the 17th character but byte 17 (é is two bytes)
        assert_eq!(char_position_to_byte(s, 17), 17);
        assert_eq!(&s[char_position_to_byte(s, 17)..], "x");
        assert_eq!(char_position_to_byte(s, 1), 0);
        assert_eq!(char_position_to_byte(s, 0), 0);
        assert_eq!(char_position_to_byte(s, 999), s.len());
        let s = "select '日本', bogus";
        assert_eq!(&s[char_position_to_byte(s, 14)..], "bogus");
    }
}
