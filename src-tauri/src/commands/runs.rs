//! Refresh workflow runs (U16 ↔ U15): preflight → one confirmation → start; live events as
//! the global `workflow-event` (runs outlive any tab); history, re-inventory and the
//! state-aware actions; retry; and the post-swap catalog invalidation.

use crate::state::AppState;
use pgwhy_core::config::ConnectionId;
use pgwhy_core::secret::{Secret, SecretKey, SecretKind};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_workflow::{
    ActionKind, HistoryEntry, OperationRecord, PreflightFix, PreflightReport, RunEvent, RunId,
    RunRecord, RunStatus, SecretNeed, StartRequest, StepId, WorkflowId,
};
use tauri::{AppHandle, Emitter, Runtime, State};

/// Warnings only the UI layer can know: open editor transactions and grids on the target.
fn caller_warnings(state: &AppState, target: &ConnectionId, database: &str) -> Vec<String> {
    let mut w = Vec::new();
    let open: Vec<_> = state
        .editors
        .open_transactions()
        .into_iter()
        .filter(|(_, c, db)| c == target && db == database)
        .collect();
    if !open.is_empty() {
        w.push(format!(
            "{} editor tab(s) have an open transaction on {database}; the swap rolls it back",
            open.len()
        ));
    }
    w
}

#[tauri::command]
pub async fn workflow_preflight(
    state: State<'_, AppState>,
    workflow_id: WorkflowId,
) -> AppResult<PreflightReport> {
    let wf = state.workflow_store().get(&workflow_id)?;
    let warnings = caller_warnings(&state, &wf.target.connection, &wf.target.database);
    Ok(state.engine().preflight(&wf, warnings).await)
}

#[tauri::command]
pub async fn workflow_retry_preflight(
    state: State<'_, AppState>,
    run_id: RunId,
    from: StepId,
) -> AppResult<PreflightReport> {
    let run = state.engine().get_run(&run_id)?;
    let warnings = caller_warnings(&state, &run.target.connection_id, &run.target.database);
    Ok(state.engine().retry(&run_id, from, warnings).await)
}

#[tauri::command]
pub fn workflow_discard_preflight(state: State<'_, AppState>, ticket: String) {
    state.engine().discard_preflight(&ticket);
}

#[tauri::command]
pub async fn workflow_apply_fix(
    state: State<'_, AppState>,
    workflow_id: WorkflowId,
    fix: PreflightFix,
) -> AppResult<()> {
    let wf = state.workflow_store().get(&workflow_id)?;
    state.engine().apply_preflight_fix(&wf, &fix).await
}

/// EC-10: store a secret preflight said is missing, then the UI re-runs preflight.
#[tauri::command]
pub fn workflow_provide_secret(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    kind: SecretNeed,
    value: String,
    persist: bool,
) -> AppResult<()> {
    let kind = match kind {
        SecretNeed::DbPassword => SecretKind::DbPassword,
        SecretNeed::SshKeyPassphrase => SecretKind::SshKeyPassphrase,
        SecretNeed::SshPassword => SecretKind::SshPassword,
    };
    state.vault.store(
        &SecretKey::new(&connection_id, kind),
        Secret::new(value),
        persist,
    )?;
    Ok(())
}

/// Start a confirmed run. Events go to every window as `workflow-event`; a successful swap
/// also invalidates the target's catalog so the tree and completion re-read it.
#[tauri::command]
pub async fn workflow_start<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    ticket: String,
    fingerprint: String,
    remember: bool,
) -> AppResult<RunId> {
    let emit_app = app.clone();
    let introspector = state.introspector.clone();
    let sink: pgwhy_workflow::EventSink = std::sync::Arc::new(move |ev: RunEvent| {
        if let RunEvent::RunFinished { status, record, .. } = &ev {
            if matches!(
                status,
                RunStatus::Succeeded | RunStatus::SucceededCleanupRequired
            ) {
                introspector.invalidate_db(&record.target.connection_id, &record.target.database);
            }
        }
        let _ = emit_app.emit("workflow-event", &ev);
    });
    state
        .engine()
        .start(
            &ticket,
            StartRequest {
                fingerprint,
                remember,
            },
            sink,
        )
        .await
}

#[tauri::command]
pub fn workflow_cancel(state: State<'_, AppState>, run_id: RunId) -> AppResult<()> {
    state.engine().cancel(&run_id)
}

#[tauri::command]
pub fn run_list(
    state: State<'_, AppState>,
    workflow_id: Option<WorkflowId>,
) -> AppResult<Vec<RunRecord>> {
    state.engine().list_runs(workflow_id.as_ref())
}

/// Refresh runs and one-off operations (backup/restore/drop/…) in one timeline, newest first.
#[tauri::command]
pub fn history_list(state: State<'_, AppState>) -> AppResult<Vec<HistoryEntry>> {
    state.engine().list_history()
}

/// One operation record from the merged history (`kind != refresh` in its [`HistoryEntry`]).
#[tauri::command]
pub fn operation_get(state: State<'_, AppState>, id: RunId) -> AppResult<OperationRecord> {
    state.engine().get_operation(&id)
}

#[tauri::command]
pub fn run_get(state: State<'_, AppState>, run_id: RunId) -> AppResult<RunRecord> {
    state.engine().get_run(&run_id)
}

#[tauri::command]
pub async fn run_reinventory(state: State<'_, AppState>, run_id: RunId) -> AppResult<RunRecord> {
    state.engine().reinventory(&run_id).await
}

#[tauri::command]
pub async fn run_action(
    state: State<'_, AppState>,
    run_id: RunId,
    action: ActionKind,
) -> AppResult<RunRecord> {
    if action == ActionKind::Retry {
        return Err(
            AppError::new(ErrorKind::Refused, "retry goes through preflight")
                .with_hint("use workflow_retry_preflight"),
        );
    }
    let rec = state.engine().perform(&run_id, action).await?;
    if matches!(action, ActionKind::CompleteSwap | ActionKind::RollbackSwap) {
        state
            .introspector
            .invalidate_db(&rec.target.connection_id, &rec.target.database);
    }
    Ok(rec)
}
