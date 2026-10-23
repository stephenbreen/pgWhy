//! Protocol-level cancel (psql's Ctrl+C) for a [`super::QuerySession`].
//!
//! A cancel is two things: a flag the executor checks before starting each statement (so
//! queued statements are abandoned), and — only while a statement is actually on the wire — a
//! `CancelRequest` sent over a fresh socket with the session's own TLS policy. The flag is set
//! before `in_flight` is read and the executor sets `in_flight` before reading the flag, so
//! whichever side runs second sees the other: a statement is never started after a cancel, and
//! a running statement always gets the request. A cancel request that reaches the backend after
//! the statement finished is ignored by the server (it only acts on a running command).

use crate::conn::{classify, PgConn};
use crate::AppResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_postgres::CancelToken;

#[derive(Debug, Default)]
pub(crate) struct CancelShared {
    pub(crate) requested: AtomicBool,
    pub(crate) in_flight: AtomicBool,
}

/// Cheap, cloneable handle that cancels whatever its session is running. Usable from any task
/// while the run future is being awaited elsewhere.
#[derive(Clone)]
pub struct CancelHandle {
    token: CancelToken,
    tls: tokio_postgres_rustls::MakeRustlsConnect,
    pub(crate) shared: Arc<CancelShared>,
}

impl std::fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelHandle")
            .field("shared", &self.shared)
            .finish_non_exhaustive()
    }
}

impl CancelHandle {
    pub(crate) fn new(conn: &PgConn) -> Self {
        Self {
            token: conn.cancel_token(),
            tls: conn.cancel_tls(),
            shared: Arc::default(),
        }
    }

    /// Stop the current statement and abandon the queued ones. Returns whether a cancel
    /// request was sent to the server (false when nothing was on the wire — the run still
    /// stops before its next statement). The connection stays usable.
    pub async fn cancel(&self) -> AppResult<bool> {
        self.shared.requested.store(true, Ordering::SeqCst);
        if !self.shared.in_flight.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.token
            .cancel_query(self.tls.clone())
            .await
            .map_err(|e| classify(&e, "send the cancel request"))?;
        Ok(true)
    }

    pub(crate) fn requested(&self) -> bool {
        self.shared.requested.load(Ordering::SeqCst)
    }
}
