//! App updates through Tauri's updater, from the newest published GitHub release.
//!
//! Each step is a command of ours, like all other I/O, so the webview needs no updater permission.
//! Downloading is separate from restarting: the UI asks about open transactions in between, and a
//! download that fails has cost nothing.

use crate::state::AppState;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::Serialize;
use std::sync::Mutex;
use tauri::ipc::Channel;
use tauri::State;
use tauri_plugin_updater::{Update, UpdaterExt};

/// The update downloaded and waiting for a restart.
#[derive(Default)]
pub struct Updates {
    downloaded: Mutex<Option<(Update, Vec<u8>)>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

async fn latest<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<Option<Update>> {
    let failed = |e: tauri_plugin_updater::Error| {
        AppError::new(ErrorKind::Network, "Could not check for updates").with_detail(e.to_string())
    };
    app.updater().map_err(failed)?.check().await.map_err(failed)
}

/// What is keeping the app from restarting, if anything. Restarting kills running children, and
/// the UI's own prompts cover open transactions and pending edits but not these.
fn busy_with(state: &AppState) -> Option<&'static str> {
    let engine = state.engine();
    if !engine.active_runs().is_empty() {
        Some("a Refresh workflow is running")
    } else if engine.has_active_operations() {
        Some("a restore is running")
    } else if state.backups.any_running() {
        Some("a backup is running")
    } else {
        None
    }
}

fn refuse_if_busy(state: &AppState) -> AppResult<()> {
    match busy_with(state) {
        Some(what) => Err(AppError::new(
            ErrorKind::Refused,
            format!("pgWhy can't restart to update while {what}"),
        )
        .with_hint("Try again when it has finished")),
        None => Ok(()),
    }
}

/// Look for a newer release. `automatic` is the check at startup, which the settings can turn off.
#[tauri::command]
pub async fn update_check<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
    automatic: bool,
) -> AppResult<Option<UpdateInfo>> {
    let enabled = state
        .settings
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .check_for_updates;
    // A development build would replace itself with the release.
    if cfg!(debug_assertions) || (automatic && !enabled) {
        return Ok(None);
    }
    Ok(latest(&app).await?.map(|u| UpdateInfo {
        version: u.version.clone(),
        current_version: u.current_version.clone(),
        notes: u.body.clone(),
    }))
}

/// Download the newest release, checking its signature, and keep it for [`update_restart`].
#[tauri::command]
pub async fn update_download<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
    events: Channel<UpdateProgress>,
) -> AppResult<()> {
    let update = latest(&app)
        .await?
        .ok_or_else(|| AppError::new(ErrorKind::Config, "pgWhy is already up to date"))?;
    let mut downloaded = 0u64;
    let bytes = update
        .download(
            |chunk, total| {
                downloaded += chunk as u64;
                let _ = events.send(UpdateProgress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(|e| {
            AppError::new(ErrorKind::Network, "Could not download the update")
                .with_detail(e.to_string())
        })?;
    *state
        .updates
        .downloaded
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some((update, bytes));
    Ok(())
}

/// Install the downloaded update and restart. On Windows the installer takes over and ends this
/// process inside `install`, which is safe because nothing can be running by then.
#[tauri::command]
pub async fn update_restart<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
) -> AppResult<()> {
    refuse_if_busy(&state)?;
    let (update, bytes) = state
        .updates
        .downloaded
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .ok_or_else(|| AppError::new(ErrorKind::Config, "No update has been downloaded"))?;
    update.install(bytes).map_err(|e| {
        AppError::new(ErrorKind::Disk, "Could not install the update").with_detail(e.to_string())
    })?;
    crate::stop_background_work(&app).await;
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgwhy_core::secret::MemoryStore;
    use std::sync::Arc;

    #[test]
    fn a_running_backup_holds_off_the_restart() {
        let dirs = tempfile::tempdir().unwrap();
        let state = AppState::with_secrets(
            dirs.path().join("config"),
            dirs.path().join("data"),
            Arc::new(MemoryStore::default()),
        )
        .unwrap();
        assert!(refuse_if_busy(&state).is_ok());

        state
            .backups
            .track("b1", state.run_context().unwrap().cancel.clone());
        let e = refuse_if_busy(&state).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Refused);
        assert!(e.message.contains("a backup is running"), "{}", e.message);

        state.backups.untrack("b1");
        assert!(refuse_if_busy(&state).is_ok());
    }
}
