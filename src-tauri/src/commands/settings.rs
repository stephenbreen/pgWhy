//! App settings (top-N rows, backup retention, library and tool locations).

use crate::state::AppState;
use pgwhy_core::config::AppSettings;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::Serialize;
use tauri::State;

#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub settings: AppSettings,
    pub defaults: SettingsDefaults,
    /// pg client tools currently found, for the settings page
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsDefaults {
    pub backup_dir: String,
    pub script_library_dir: String,
}

#[tauri::command]
pub async fn settings_get(
    state: State<'_, AppState>,
    rescan_tools: bool,
) -> AppResult<SettingsView> {
    let registry = state.tool_registry(rescan_tools).await;
    let tools = registry
        .tools
        .iter()
        .map(|t| format!("{} {} — {}", t.kind.name(), t.version, t.path.display()))
        .collect();
    Ok(SettingsView {
        settings: state
            .settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone(),
        defaults: SettingsDefaults {
            backup_dir: state.data_dir.join("backups").display().to_string(),
            script_library_dir: state.data_dir.join("scripts").display().to_string(),
        },
        tools,
    })
}

#[tauri::command]
pub async fn settings_set(
    state: State<'_, AppState>,
    settings: AppSettings,
) -> AppResult<SettingsView> {
    if settings.top_n == 0 || settings.top_n > 1_000_000 {
        return Err(AppError::new(
            ErrorKind::Config,
            "top rows must be between 1 and 1,000,000",
        ));
    }
    if settings.backup_retention == 0 {
        return Err(AppError::new(ErrorKind::Config, "keep at least one backup"));
    }
    if settings.result_row_cap == 0 {
        return Err(AppError::new(
            ErrorKind::Config,
            "the result row cap must be at least 1",
        ));
    }
    if settings.result_byte_cap_mb == 0 {
        return Err(AppError::new(
            ErrorKind::Config,
            "the result memory cap must be at least 1 MB",
        ));
    }
    // Folders that decide where files are written or which binaries run come only from the
    // backend's folder picker (or are unchanged / cleared back to the default).
    let current = state
        .settings
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let picked = |dir: &std::path::Path| state.grants.has_dir(dir);
    for (label, old, new) in [
        ("backup folder", &current.backup_dir, &settings.backup_dir),
        (
            "script library folder",
            &current.script_library_dir,
            &settings.script_library_dir,
        ),
    ] {
        if let Some(dir) = new.as_deref().filter(|d| old.as_deref() != Some(*d)) {
            if !picked(dir) {
                return Err(not_picked(label, dir));
            }
        }
    }
    for dir in &settings.pg_tools_dirs {
        if !current.pg_tools_dirs.contains(dir) && !picked(dir) {
            return Err(not_picked("tools folder", dir));
        }
    }
    let engine_changed = current.backup_dir != settings.backup_dir
        || current.backup_retention != settings.backup_retention
        || current.pg_tools_dirs != settings.pg_tools_dirs;
    if engine_changed && !state.engine().active_runs().is_empty() {
        return Err(AppError::new(
            ErrorKind::Refused,
            "a refresh is running; change the backup folder, retention or tool folders once it finishes",
        ));
    }
    {
        // compare, persist and swap under one lock so two saves cannot interleave
        let mut live = state.settings.lock().unwrap_or_else(|p| p.into_inner());
        let library_changed = live.script_library_dir != settings.script_library_dir;
        state.config.save_settings(&settings)?;
        // budgets and the idle policy are read on use, so they apply now, not after a restart
        state
            .results
            .set_options(crate::state::registry_options(&settings));
        state
            .manager
            .set_idle_close(std::time::Duration::from_secs(settings.idle_close_secs));
        *live = settings;
        if library_changed {
            *state.library.lock().unwrap_or_else(|p| p.into_inner()) = None; // reopened lazily at the new location
        }
    }
    if engine_changed {
        state.rebuild_engine()?;
    }
    settings_get(state, true).await
}

fn not_picked(what: &str, dir: &std::path::Path) -> AppError {
    AppError::new(
        ErrorKind::Refused,
        format!("{} was not chosen with the folder picker", dir.display()),
    )
    .with_hint(format!("use Choose… to set the {what}"))
}

/// Native folder picker for Settings. The picked folder is remembered so `settings_set` accepts
/// it; the webview cannot name a folder it did not get from here.
#[tauri::command]
pub async fn pick_folder<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
) -> AppResult<Option<std::path::PathBuf>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |p| {
        let _ = tx.send(p.and_then(|p| p.into_path().ok()));
    });
    let path = rx.await.ok().flatten();
    if let Some(p) = &path {
        state.grants.grant_dir(p);
    }
    Ok(path)
}
