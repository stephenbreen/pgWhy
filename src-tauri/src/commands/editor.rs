//! Editor tab commands: one dedicated connection + query session per tab,
//! never silently reconnected; runs stream statement events over a Channel while rows go to
//! core-side result buffers the grid pulls windows from.

use crate::state::AppState;
use pgwhy_core::config::ConnectionId;
use pgwhy_core::conn::{ConnInfo, Purpose};
use pgwhy_core::query::{
    BufferEvent, BufferOptions, BufferSink, CancelHandle, ErrorPolicy, QuerySession, RunOptions,
    SessionState,
};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::ipc::Channel;
use tauri::State;

pub struct EditorTab {
    pub session: Arc<QuerySession>,
    pub cancel: CancelHandle,
    pub connection_id: ConnectionId,
    pub database: String,
    /// Set once the tab is closed: a run still in flight releases whatever it produced.
    closed: AtomicBool,
}

/// Open editor sessions by UI tab id.
#[derive(Default)]
pub struct Editors {
    tabs: Mutex<HashMap<String, Arc<EditorTab>>>,
}

impl Editors {
    pub fn get(&self, tab_id: &str) -> AppResult<Arc<EditorTab>> {
        self.tabs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(tab_id)
            .cloned()
            .ok_or_else(|| {
                AppError::new(ErrorKind::Disconnected, "this tab is not connected")
                    .with_hint("reconnect the tab")
            })
    }

    fn insert(&self, tab_id: String, tab: EditorTab) -> Option<Arc<EditorTab>> {
        self.tabs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(tab_id, Arc::new(tab))
    }

    fn remove(&self, tab_id: &str) -> Option<Arc<EditorTab>> {
        self.tabs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(tab_id)
    }

    /// Tabs with an open transaction (quit-time ask, swap warnings).
    pub fn open_transactions(&self) -> Vec<(String, ConnectionId, String)> {
        self.tabs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, t)| {
                matches!(
                    t.session.state(),
                    SessionState::InTransaction | SessionState::FailedTransaction
                )
            })
            .map(|(id, t)| (id.clone(), t.connection_id.clone(), t.database.clone()))
            .collect()
    }
}

/// (Re)open a tab's dedicated connection. The new connection is made first and only then
/// swaps in for the old session, so a failed reconnect leaves the tab as it was. A session with
/// an open transaction or a run in flight is never dropped silently: the UI commits or rolls
/// back (`editor_close`) first.
#[tauri::command]
pub async fn editor_open(
    state: State<'_, AppState>,
    tab_id: String,
    connection_id: ConnectionId,
    database: String,
) -> AppResult<ConnInfo> {
    if let Ok(old) = state.editors.get(&tab_id) {
        match old.session.state() {
            SessionState::InTransaction | SessionState::FailedTransaction => {
                return Err(
                    AppError::new(ErrorKind::Refused, "this tab has an open transaction")
                        .with_hint("commit or roll back before reconnecting"),
                )
            }
            SessionState::Running => {
                return Err(
                    AppError::new(ErrorKind::Refused, "this tab is running a query")
                        .with_hint("cancel the run before reconnecting"),
                )
            }
            _ => {}
        }
    }
    let conn = state
        .manager
        .open(&connection_id, &database, Purpose::EditorTab)
        .await?;
    let info = conn.info.clone();
    let session = QuerySession::new(conn);
    let cancel = session.cancel_handle();
    let old = state.editors.insert(
        tab_id.clone(),
        EditorTab {
            session: Arc::new(session),
            cancel,
            connection_id,
            database,
            closed: AtomicBool::new(false),
        },
    );
    if old.is_some() {
        // the old session's results belong to a connection that is gone
        state.release_results_of(&tab_id);
    }
    Ok(info)
}

#[tauri::command]
pub async fn editor_cancel(state: State<'_, AppState>, tab_id: String) -> AppResult<()> {
    let tab = state.editors.get(&tab_id)?;
    tab.cancel.cancel().await?;
    Ok(())
}

#[tauri::command]
pub fn editor_txn_state(state: State<'_, AppState>, tab_id: String) -> AppResult<SessionState> {
    Ok(state
        .editors
        .get(&tab_id)
        .map(|t| t.session.state())
        .unwrap_or(SessionState::Disconnected))
}

/// Close a tab's session; `txn` says what to do with an open transaction ("commit" /
/// "rollback"). Closing with an open transaction and no instruction is refused. A run in
/// flight is cancelled first, and whatever it still produces is released when it ends.
#[tauri::command]
pub async fn editor_close(
    state: State<'_, AppState>,
    tab_id: String,
    txn: Option<String>,
) -> AppResult<()> {
    let Some(tab) = state.editors.remove(&tab_id) else {
        return Ok(());
    };
    tab.closed.store(true, Ordering::SeqCst);
    if tab.session.state() == SessionState::Running {
        let _ = tab.cancel.cancel().await;
        let mut rx = tab.session.watch_state();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            rx.wait_for(|s| *s != SessionState::Running),
        )
        .await;
    }
    // a refused close leaves the tab open and usable
    let restore = |tab: Arc<EditorTab>| {
        tab.closed.store(false, Ordering::SeqCst);
        state
            .editors
            .tabs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(tab_id.clone(), tab);
    };
    let open = matches!(
        tab.session.state(),
        SessionState::InTransaction | SessionState::FailedTransaction
    );
    match (open, txn.as_deref()) {
        (false, _) => {}
        (true, Some("commit")) => {
            if let Err(e) = tab.session.commit().await {
                restore(tab);
                return Err(e);
            }
        }
        (true, Some("rollback")) => {
            let _ = tab.session.rollback().await;
        }
        (true, _) => {
            restore(tab);
            return Err(
                AppError::new(ErrorKind::Refused, "this tab has an open transaction")
                    .with_hint("commit or roll back before closing"),
            );
        }
    }
    // The tab is gone: its buffered results must not outlive it (they are up to a few hundred
    // megabytes each, and no later eviction pass will ever run for this owner).
    state.release_results_of(&tab_id);
    Ok(())
}

/// Run the statements of `sql` within the byte range `from..to` (the selection, or all of
/// it). Statement events stream over `events`; rows land in result buffers owned by the tab.
#[tauri::command]
pub async fn editor_run(
    state: State<'_, AppState>,
    tab_id: String,
    sql: String,
    from: usize,
    to: usize,
    policy: String,
    events: Channel<BufferEvent>,
) -> AppResult<()> {
    let tab = state.editors.get(&tab_id)?;
    let opts = RunOptions {
        on_error: if policy == "continue" {
            ErrorPolicy::Continue
        } else {
            ErrorPolicy::StopOnError
        },
        ..RunOptions::default()
    };
    // Retention limits come from Settings: a single wide result set can hit the byte cap long
    // before the row cap, and both are reported as `truncated`.
    let buffer_opts = {
        let s = state.settings.lock().unwrap_or_else(|p| p.into_inner());
        BufferOptions {
            row_cap: s.result_row_cap,
            byte_cap: (s.result_byte_cap_mb as usize).saturating_mul(1024 * 1024),
            ..BufferOptions::default()
        }
    };
    // Only drop the previous results once we know this run can start: a refused run (busy or
    // disconnected) must not cost the user the results they were looking at.
    tab.session.check_can_start()?;
    // the previous run's results go once a new run starts (unless pending edits hold them)
    for id in state.results.results_of(&tab_id) {
        state.release_result(&id);
    }
    let ch = events.clone();
    let mut sink = BufferSink::new(
        state.results.clone(),
        tab_id.clone(),
        buffer_opts,
        move |ev| {
            let _ = ch.send(ev);
        },
    );
    // run_within splits under the session's real `standard_conforming_strings` (and re-splits
    // if a statement changes it), so a backslash in a literal cannot cut a statement in two.
    let outcome = tab
        .session
        .run_within(&sql, from..to, &opts, &mut sink)
        .await;
    if let Ok(summary) = &outcome {
        sink.finish(summary);
    }
    // The tab was closed mid-run: the sink kept appending under an owner nobody will evict.
    if tab.closed.load(Ordering::SeqCst) {
        state.release_results_of(&tab_id);
    }
    outcome.map(|_| ())
}
