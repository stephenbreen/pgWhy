//! Refresh workflow definitions (editor side): list/save/delete with the save-time gates —
//! target environment tag, source ≠ target, and attach-time script checks (no psql
//! meta-commands, no explicit transaction control, no-transaction statements need the
//! no-transaction mode). The engine re-checks all of this at run time (files are editable).

use crate::state::AppState;
use pgwhy_core::scripts::{has_meta_commands, ScriptId};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_workflow::definition::ScriptMode;
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
    let mut wf = workflow;
    if let Some(pgwhy_workflow::WorkflowSource::FromRemoteDb { source }) = &wf.source_mode {
        wf.source = source.clone();
    }
    let source = match wf.effective_source() {
        pgwhy_workflow::WorkflowSource::FromRemoteDb { source: src_ref } => {
            Some(state.manager.entry(&src_ref.connection)?)
        }
        _ => None,
    };
    let target = state.manager.entry(&wf.target.connection)?;
    pgwhy_workflow::validate_workflow(&wf, source.as_ref(), &target)?;
    // Validate external fixed archive path against PathGrants/backup_dir when present.
    if let pgwhy_workflow::WorkflowSource::FromBackupFile {
        archive: pgwhy_workflow::BackupSource::FixedPath { path },
    } = wf.effective_source()
    {
        state.check_path(&path, false)?;
    }

    // Never allow secret parameters to store a default on disk.
    for p in &mut wf.parameters {
        if p.kind == pgwhy_workflow::ParamType::Secret {
            p.default = None;
        }
    }

    if !wf.steps.is_empty() {
        wf.schema_version = pgwhy_workflow::WORKFLOW_SCHEMA_VERSION;
        let empty = std::collections::HashMap::new();
        let mode = pgwhy_workflow::CompileMode::Preview { values: &empty };
        let compiled = state.with_library(|lib| {
            let loader = |id: &str| {
                let sid = ScriptId(id.to_string());
                let info = lib.get(&sid).ok_or_else(|| {
                    AppError::new(
                        ErrorKind::Config,
                        format!("script \"{id}\" is no longer in the script library"),
                    )
                })?;
                let file = lib.read(&sid)?;
                Ok(pgwhy_workflow::LoadedScript {
                    id: id.to_string(),
                    name: info.name,
                    content: file.content,
                    sha256: file.fingerprint.sha256,
                })
            };
            pgwhy_workflow::compile_steps(&wf.steps, &wf.parameters, &mode, Some(&loader))
        })?;
        let problems: Vec<String> = compiled
            .iter()
            .flat_map(|c| {
                c.problems
                    .iter()
                    .map(move |p| format!("{}: {p}", c.step_name))
            })
            .collect();
        if !problems.is_empty() {
            return Err(AppError::new(
                ErrorKind::Refused,
                "one or more workflow steps have SQL problems",
            )
            .with_detail(problems.join("\n"))
            .with_data(serde_json::json!({ "problems": problems })));
        }
    } else if let Some(sid) = &wf.post_restore_script {
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
    if wf.is_v2_pipeline() {
        wf.schema_version = pgwhy_workflow::WORKFLOW_SCHEMA_VERSION;
    }

    // a changed definition can't keep a confirmation given for the old one
    let store = state.workflow_store();
    // run-derived fields are never taken from the UI
    match store.get(&wf.id) {
        Ok(old) => {
            let unchanged = old.source == wf.source
                && old.source_mode == wf.source_mode
                && old.target == wf.target
                && old.options.strategy == wf.options.strategy
                && old.options.pause_before_swap == wf.options.pause_before_swap
                && old.post_restore_script == wf.post_restore_script
                && old.parameters == wf.parameters
                && old.steps == wf.steps;
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

/// Compile every enabled step of `workflow` into preview SQL (secrets masked as `'••••••••'`)
/// along with static warnings and problems.
#[tauri::command]
pub fn workflow_preview_sql(
    state: State<'_, AppState>,
    workflow: RefreshWorkflow,
    params: Option<std::collections::HashMap<String, String>>,
) -> AppResult<Vec<pgwhy_workflow::CompiledStep>> {
    let values = params.unwrap_or_default();
    let mode = pgwhy_workflow::CompileMode::Preview { values: &values };
    let steps = workflow.effective_steps();
    state.with_library(|lib| {
        let loader = |id: &str| {
            let sid = ScriptId(id.to_string());
            let info = lib.get(&sid).ok_or_else(|| {
                AppError::new(
                    ErrorKind::Config,
                    format!("script \"{id}\" is no longer in the script library"),
                )
            })?;
            let file = lib.read(&sid)?;
            Ok(pgwhy_workflow::LoadedScript {
                id: id.to_string(),
                name: info.name,
                content: file.content,
                sha256: file.fingerprint.sha256,
            })
        };
        pgwhy_workflow::compile_steps(&steps, &workflow.parameters, &mode, Some(&loader))
    })
}

/// Convert a visual step inside `workflow` into an editable inline SQL step while preserving
/// `{{param}}` references.
#[tauri::command]
pub fn workflow_convert_step_to_sql(
    state: State<'_, AppState>,
    workflow: RefreshWorkflow,
    step_id: String,
) -> AppResult<pgwhy_workflow::WorkflowStep> {
    let steps = workflow.effective_steps();
    let step = steps
        .iter()
        .find(|s| s.id == step_id)
        .ok_or_else(|| AppError::new(ErrorKind::Config, format!("step \"{step_id}\" not found")))?;
    state.with_library(|lib| {
        let loader = |id: &str| {
            let sid = ScriptId(id.to_string());
            let info = lib.get(&sid).ok_or_else(|| {
                AppError::new(
                    ErrorKind::Config,
                    format!("script \"{id}\" is no longer in the script library"),
                )
            })?;
            let file = lib.read(&sid)?;
            Ok(pgwhy_workflow::LoadedScript {
                id: id.to_string(),
                name: info.name,
                content: file.content,
                sha256: file.fingerprint.sha256,
            })
        };
        pgwhy_workflow::convert_step_to_sql(step, &workflow.parameters, Some(&loader))
    })
}

/// Built-in starter workflow templates.
#[tauri::command]
pub fn workflow_templates() -> Vec<pgwhy_workflow::WorkflowTemplate> {
    pgwhy_workflow::starter_templates()
}

/// Export a saved workflow as a shareable v2 TOML string (secrets and local confirmations stripped).
#[tauri::command]
pub fn workflow_export_toml(state: State<'_, AppState>, id: WorkflowId) -> AppResult<String> {
    let wf = state.workflow_store().get(&id)?;
    wf.export_toml()
}

/// Parse and sanitize a shareable v1 or v2 workflow TOML string for import (does not save yet).
#[tauri::command]
pub fn workflow_import_toml(
    state: State<'_, AppState>,
    toml_text: String,
) -> AppResult<RefreshWorkflow> {
    let mut wf = RefreshWorkflow::import_toml(&toml_text)?;
    if state.workflow_store().get(&wf.id).is_ok() {
        wf.id = WorkflowId::new();
    }
    Ok(wf)
}

/// Extract unique `(schema, table)` pairs from a Custom or Directory backup archive's TOC
/// (`pg_restore --list`), powering Level 3 of Smart Catalog Schema Discovery.
#[tauri::command]
pub async fn workflow_archive_tables(
    state: State<'_, AppState>,
    path: std::path::PathBuf,
) -> AppResult<Vec<(String, String)>> {
    let norm = state.check_path(&path, false)?;
    let reg = state.tool_registry(false).await;
    let Some(pg_restore) = reg.highest(pgwhy_pgtools::ToolKind::PgRestore).cloned() else {
        return Ok(Vec::new());
    };
    let rc = state.run_context()?;
    let insp = pgwhy_pgtools::inspect_archive(
        &pg_restore,
        &norm,
        pgwhy_pgtools::ArchiveCheck::TocOnly,
        &rc,
    )
    .await?;
    Ok(insp.toc.map(|t| t.tables()).unwrap_or_default())
}
