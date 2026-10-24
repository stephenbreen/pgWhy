//! Script workbench commands: library CRUD + search, recents, plain-file
//! open/save with on-disk change detection, and the psql meta-command scan.

use crate::commands::blocking;
use crate::state::AppState;
use pgwhy_core::scripts::{
    self, FileCheck, FileFingerprint, MetaCommand, RecentStatus, RecentUse, RescanReport,
    ScriptFile, ScriptId, ScriptInfo,
};
use pgwhy_core::AppResult;
use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, State};

#[derive(Debug, Clone, Serialize)]
pub struct LibrarySnapshot {
    pub root: PathBuf,
    pub scripts: Vec<ScriptInfo>,
    pub tags: Vec<String>,
}

fn snapshot(lib: &scripts::Library, query: Option<&str>) -> LibrarySnapshot {
    LibrarySnapshot {
        root: lib.root().to_path_buf(),
        scripts: match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => lib.search(q),
            None => lib.list(),
        },
        tags: lib.all_tags(),
    }
}

#[tauri::command]
pub async fn library_list<R: tauri::Runtime>(
    app: AppHandle<R>,
    query: Option<String>,
) -> AppResult<LibrarySnapshot> {
    blocking(app, move |state| {
        state.with_library(|lib| Ok(snapshot(lib, query.as_deref())))
    })
    .await
}

#[derive(Debug, Clone, Serialize)]
pub struct RescanResult {
    pub report: RescanReport,
    pub library: LibrarySnapshot,
}

/// Called when the window regains focus: picks up external adds/renames/deletes. Walks the
/// library, so it runs off the main thread.
#[tauri::command]
pub async fn library_rescan<R: tauri::Runtime>(app: AppHandle<R>) -> AppResult<RescanResult> {
    blocking(app, |state| {
        state.with_library(|lib| {
            let report = lib.rescan()?;
            let mut recents = state
                .script_recents
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if recents.sync_with_library(lib) {
                recents.save(&state.config)?;
            }
            Ok(RescanResult {
                report,
                library: snapshot(lib, None),
            })
        })
    })
    .await
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenedScript {
    pub info: ScriptInfo,
    pub path: PathBuf,
    pub file: ScriptFile,
}

#[tauri::command]
pub async fn script_read<R: tauri::Runtime>(
    app: AppHandle<R>,
    id: ScriptId,
) -> AppResult<OpenedScript> {
    blocking(app, move |state| {
        state.with_library(|lib| {
            let file = lib.read(&id)?;
            let info = lib.get(&id).ok_or_else(|| missing(&id))?;
            let path = lib.absolute_path(&id).ok_or_else(|| missing(&id))?;
            Ok(OpenedScript { info, path, file })
        })
    })
    .await
}

fn missing(id: &ScriptId) -> pgwhy_core::AppError {
    pgwhy_core::AppError::new(
        pgwhy_core::ErrorKind::Config,
        format!("script {} is not in the library", id.0),
    )
}

/// Create (or overwrite by path) a library script.
#[tauri::command]
pub async fn script_save<R: tauri::Runtime>(
    app: AppHandle<R>,
    path: String,
    content: String,
) -> AppResult<OpenedScript> {
    blocking(app, move |state| {
        state.with_library(|lib| {
            let info = lib.save(&path, &content)?;
            let abs = lib
                .absolute_path(&info.id)
                .ok_or_else(|| missing(&info.id))?;
            let file = lib.read(&info.id)?;
            Ok(OpenedScript {
                info,
                path: abs,
                file,
            })
        })
    })
    .await
}

#[tauri::command]
pub async fn script_write<R: tauri::Runtime>(
    app: AppHandle<R>,
    id: ScriptId,
    content: String,
) -> AppResult<ScriptInfo> {
    blocking(app, move |state| {
        state.with_library(|lib| lib.write(&id, &content))
    })
    .await
}

#[tauri::command]
pub async fn script_rename<R: tauri::Runtime>(
    app: AppHandle<R>,
    id: ScriptId,
    path: String,
) -> AppResult<ScriptInfo> {
    blocking(app, move |state| {
        state.with_library(|lib| lib.rename(&id, &path))
    })
    .await
}

#[tauri::command]
pub async fn script_delete<R: tauri::Runtime>(app: AppHandle<R>, id: ScriptId) -> AppResult<()> {
    blocking(app, move |state| state.with_library(|lib| lib.delete(&id))).await
}

#[tauri::command]
pub async fn script_set_tags<R: tauri::Runtime>(
    app: AppHandle<R>,
    id: ScriptId,
    tags: Vec<String>,
) -> AppResult<ScriptInfo> {
    blocking(app, move |state| {
        state.with_library(|lib| lib.set_tags(&id, &tags))
    })
    .await
}

#[tauri::command]
pub async fn script_recents<R: tauri::Runtime>(app: AppHandle<R>) -> AppResult<Vec<RecentStatus>> {
    blocking(app, |state| {
        let lib = state.library.lock().unwrap_or_else(|p| p.into_inner());
        Ok(state
            .script_recents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .status(lib.as_ref()))
    })
    .await
}

/// Record an open/save/run. Library scripts are recognized by path.
#[tauri::command]
pub async fn script_recent_record<R: tauri::Runtime>(
    app: AppHandle<R>,
    mut used: RecentUse,
) -> AppResult<()> {
    blocking(app, move |state| {
        // Only paths the backend has already vouched for may enter the recents list, which is
        // itself an access list for the file commands.
        state.check_path(&used.path, false)?;
        if used.library_id.is_none() {
            used.library_id = state.with_library(|lib| Ok(lib.id_for_path(&used.path)))?;
        }
        let mut recents = state
            .script_recents
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        recents.record(used)?;
        recents.save(&state.config)
    })
    .await
}

#[tauri::command]
pub async fn script_recent_remove<R: tauri::Runtime>(
    app: AppHandle<R>,
    path: PathBuf,
) -> AppResult<()> {
    blocking(app, move |state| {
        let mut recents = state
            .script_recents
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if recents.remove(&path) {
            recents.save(&state.config)?;
        }
        Ok(())
    })
    .await
}

/// Native open dialog for files the Scripts view can open: SQL scripts and plain-text /
/// Markdown notes. The picked path is remembered so the file commands accept it.
#[tauri::command]
pub async fn pick_open_path<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
) -> AppResult<Option<PathBuf>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Scripts and notes", &["sql", "txt", "md"])
        .add_filter("SQL", &["sql"])
        .add_filter("Text", &["txt", "md"])
        .pick_file(move |p| {
            let _ = tx.send(p.and_then(|p| p.into_path().ok()));
        });
    let path = rx.await.ok().flatten();
    if let Some(p) = &path {
        state.grants.grant_file(p);
    }
    Ok(path)
}

#[tauri::command]
pub async fn file_read<R: tauri::Runtime>(
    app: AppHandle<R>,
    path: PathBuf,
) -> AppResult<ScriptFile> {
    blocking(app, move |state| {
        scripts::read_script_file(&state.check_path(&path, false)?)
    })
    .await
}

#[tauri::command]
pub async fn file_write<R: tauri::Runtime>(
    app: AppHandle<R>,
    path: PathBuf,
    content: String,
) -> AppResult<FileFingerprint> {
    blocking(app, move |state| {
        scripts::write_script_file(&state.check_path(&path, false)?, &content)
    })
    .await
}

#[tauri::command]
pub async fn file_check<R: tauri::Runtime>(
    app: AppHandle<R>,
    path: PathBuf,
    fingerprint: FileFingerprint,
) -> AppResult<FileCheck> {
    blocking(app, move |state| {
        scripts::check_file(&state.check_path(&path, false)?, &fingerprint)
    })
    .await
}

#[tauri::command]
pub fn meta_scan(text: String) -> Vec<MetaCommand> {
    scripts::scan_meta_commands(&text)
}
