//! Result-set access for the grid: binary windows over a per-result Channel, TSV copy,
//! CSV/JSON export — all served from the core-side buffer.

use crate::state::AppState;
use pgwhy_core::query::CsvOptions;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;

/// Largest TSV copy the core will build. A bigger selection is refused with a pointer at
/// export: the text would be materialised three times (Rust string, JSON payload, JS string)
/// and the clipboard cannot usefully hold it anyway.
const MAX_COPY_BYTES: usize = 32 * 1024 * 1024;

/// The grid's binary channel per result id.
#[derive(Default)]
pub struct ResultChannels {
    channels: Mutex<HashMap<String, Channel<InvokeResponseBody>>>,
}

impl ResultChannels {
    /// Forget the channels of released results, so the map does not grow for the life of the
    /// process (an entry holds a webview callback).
    pub(crate) fn forget(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut map = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        for id in ids {
            map.remove(id);
        }
    }
}

impl AppState {
    /// Release one result buffer and stop streaming its windows.
    pub(crate) fn release_result(&self, result_id: &str) {
        self.result_channels.forget(&[result_id.to_string()]);
        self.results.release(result_id);
    }

    /// Release every result buffer of an owner (a closed tab or grid), returning the ids.
    pub(crate) fn release_results_of(&self, owner: &str) -> Vec<String> {
        let ids = self.results.release_owner(owner);
        self.result_channels.forget(&ids);
        ids
    }
}

#[tauri::command]
pub fn result_subscribe(
    state: State<'_, AppState>,
    result_id: String,
    channel: Channel<InvokeResponseBody>,
) -> AppResult<()> {
    state.results.require(&result_id)?;
    state
        .result_channels
        .channels
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(result_id, channel);
    Ok(())
}

/// Answer a window request on the result's channel (fire-and-forget from the UI; the reply
/// is the binary chunk, see ui/src/grid/protocol.ts).
#[tauri::command]
pub async fn result_window(
    state: State<'_, AppState>,
    result_id: String,
    start: usize,
    count: usize,
) -> AppResult<()> {
    let set = state.results.require(&result_id)?;
    let channel = state
        .result_channels
        .channels
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&result_id)
        .cloned();
    let Some(ch) = channel else {
        return Err(AppError::internal("no grid is subscribed to this result"));
    };
    // Encoding a big window is real work; keep it off the main thread.
    tokio::task::spawn_blocking(move || {
        ch.send(InvokeResponseBody::Raw(set.encode_window(start, count)))
            .map_err(|e| {
                AppError::internal("could not send rows to the grid").with_detail(e.to_string())
            })
    })
    .await
    .map_err(|e| AppError::internal("window task failed").with_detail(e.to_string()))?
}

#[tauri::command]
pub fn result_release(state: State<'_, AppState>, result_id: String) {
    state.release_result(&result_id);
}

/// Copy a cell range as TSV. Runs off the main thread and refuses very large selections.
#[tauri::command]
pub async fn result_copy(
    state: State<'_, AppState>,
    result_id: String,
    r0: usize,
    r1: usize,
    c0: usize,
    c1: usize,
) -> AppResult<String> {
    let set = state.results.require(&result_id)?;
    tokio::task::spawn_blocking(move || set.copy_tsv_limited(r0, r1, c0, c1, false, MAX_COPY_BYTES))
        .await
        .map_err(|e| AppError::internal("copy task failed").with_detail(e.to_string()))?
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportOptions {
    pub format: String,
    pub path: PathBuf,
    #[serde(default)]
    pub csv_null_literal: bool,
    #[serde(default = "yes")]
    pub header: bool,
    /// Excel-safe CSV (CWE-1236): default on, because exported values can be
    /// attacker-controlled. Machine-to-machine consumers pass `false`.
    #[serde(default = "yes")]
    pub escape_formulas: bool,
}

fn yes() -> bool {
    true
}

#[tauri::command]
pub async fn result_export(
    state: State<'_, AppState>,
    result_id: String,
    mut options: ExportOptions,
) -> AppResult<pgwhy_core::query::ExportReport> {
    let set = state.results.require(&result_id)?;
    // Only a path from the backend's save dialog (or inside the backup/library folders).
    options.path = state.check_path(&options.path, false)?;
    tokio::task::spawn_blocking(move || match options.format.as_str() {
        "csv" => set.export_csv(
            &options.path,
            &CsvOptions {
                header: options.header,
                null_literal: options.csv_null_literal.then(|| "NULL".to_string()),
                escape_formulas: options.escape_formulas,
            },
            None,
        ),
        "json" => set.export_json(&options.path, None),
        other => Err(AppError::new(
            ErrorKind::Config,
            format!("unknown export format {other}"),
        )),
    })
    .await
    .map_err(|e| AppError::internal("export task failed").with_detail(e.to_string()))?
}

/// Native save dialog for exports.
#[tauri::command]
pub async fn pick_save_path<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
    default_name: String,
    format: String,
) -> AppResult<Option<PathBuf>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&default_name)
        .add_filter(format.to_uppercase(), &[format.as_str()])
        .save_file(move |p| {
            let _ = tx.send(p.and_then(|p| p.into_path().ok()));
        });
    let path = rx.await.ok().flatten();
    if let Some(p) = &path {
        state.grants.grant_file(p);
    }
    Ok(path)
}
