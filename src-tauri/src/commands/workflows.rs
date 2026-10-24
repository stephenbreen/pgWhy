//! Refresh workflow definitions (editor side): list/save/delete with the save-time gates —
//! target environment tag, source ≠ target, and attach-time script checks (no psql
//! meta-commands, no explicit transaction control, no-transaction statements need the
//! no-transaction mode). The engine re-checks all of this at run time (files are editable).

use crate::state::AppState;
use pgwhy_core::scripts::{has_meta_commands, ScriptId};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_workflow::definition::{validate, ScriptMode};
use pgwhy_workflow::{RefreshWorkflow, WorkflowId, WorkflowStore};
use serde::Serialize;
use tauri::State;

impl AppState {
    pub fn workflow_store(&self) -> WorkflowStore {
        WorkflowStore::new(self.config.dir().join("workflows"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScriptCheck {
    pub ok: bool,
    /// why it can't be attached, if not ok
    pub problems: Vec<String>,
    /// statements that need the no-transaction mode
    pub needs_no_transaction: Vec<String>,
    pub content_hash: Option<String>,
}

/// Checks for a library script as a post-restore script.
pub fn check_script(state: &AppState, id: &ScriptId, mode: ScriptMode) -> AppResult<ScriptCheck> {
    let file = state.with_library(|lib| lib.read(id))?;
    let mut problems = Vec::new();
    if has_meta_commands(&file.content) {
        let cmds: Vec<String> = pgwhy_core::scripts::scan_meta_commands(&file.content)
            .into_iter()
            .map(|m| format!("{} (line {})", m.command, m.line))
            .collect();
        problems.push(format!(
            "contains psql meta-commands, which pgWhy can't run: {}",
            cmds.join(", ")
        ));
    }
    let analysis = pgwhy_core::query::analyze(&file.content);
    if !analysis.txn_control.is_empty() {
        problems.push("contains explicit transaction control (BEGIN/COMMIT/ROLLBACK/SAVEPOINT); pgWhy manages the transaction".into());
    }
    if !analysis.copy_client.is_empty() {
        problems.push("uses COPY FROM STDIN / TO STDOUT, which needs psql".into());
    }
    if !analysis.unterminated.is_empty() {
        problems.push("ends inside an unterminated string, comment or dollar-quoted body".into());
    }
    let needs: Vec<String> = analysis
        .needs_no_transaction
        .iter()
        .map(|(_, why)| why.clone())
        .collect();
    if !needs.is_empty() && mode == ScriptMode::Transaction {
        problems.push(format!(
            "has statements that can't run inside a transaction ({}); switch the script mode to \"no transaction\"",
            needs.join(", ")
        ));
    }
    Ok(ScriptCheck {
        ok: problems.is_empty(),
        problems,
        needs_no_transaction: needs,
        content_hash: Some(file.fingerprint.sha256),
    })
}

#[tauri::command]
pub fn workflow_list(state: State<'_, AppState>) -> AppResult<Vec<RefreshWorkflow>> {
    state.workflow_store().list()
}

#[tauri::command]
pub fn workflow_check_script(
    state: State<'_, AppState>,
    id: ScriptId,
    mode: ScriptMode,
) -> AppResult<ScriptCheck> {
    check_script(&state, &id, mode)
}

#[tauri::command]
pub fn workflow_save(
    state: State<'_, AppState>,
    workflow: RefreshWorkflow,
) -> AppResult<RefreshWorkflow> {
    let source = state.manager.entry(&workflow.source.connection)?;
    let target = state.manager.entry(&workflow.target.connection)?;
    validate(&workflow, &source, &target)?;
    let mut wf = workflow;
    if let Some(sid) = &wf.post_restore_script {
        let check = check_script(&state, &ScriptId(sid.clone()), wf.options.script_mode)?;
        if !check.ok {
            return Err(AppError::new(
                ErrorKind::Refused,
                "this post-restore script can't be attached",
            )
            .with_detail(check.problems.join("\n"))
            .with_data(serde_json::json!({ "problems": check.problems })));
        }
    }
    // a changed definition can't keep a confirmation given for the old one
    let store = state.workflow_store();
    // run-derived fields are never taken from the UI
    match store.get(&wf.id) {
        Ok(old) => {
            let unchanged = old.source == wf.source
                && old.target == wf.target
                && old.options.strategy == wf.options.strategy
                && old.post_restore_script == wf.post_restore_script;
            wf.remembered_confirmation = if unchanged {
                old.remembered_confirmation
            } else {
                None
            };
            wf.has_succeeded = old.has_succeeded && unchanged;
        }
        Err(_) => {
            wf.remembered_confirmation = None;
            wf.has_succeeded = false;
        }
    }
    store.save(&wf)?;
    Ok(wf)
}

#[tauri::command]
pub fn workflow_delete(state: State<'_, AppState>, id: WorkflowId) -> AppResult<()> {
    state.workflow_store().delete(&id)
}
