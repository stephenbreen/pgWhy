//! Tauri commands. Each module is a thin adapter over a pgwhy-* crate: argument decoding,
//! state lookup, and error conversion only — no business logic here.

pub mod app;
pub mod backup;
pub mod connections;
pub mod databases;
pub mod editor;
pub mod explorer;
pub mod grid;
pub mod restore;
pub mod results;
pub mod runs;
pub mod scripts;
pub mod settings;
pub mod tunnels;
pub mod workflows;

/// Run blocking work (file and library I/O) off the main thread. Sync commands run on it and
/// would freeze the window for as long as the disk takes.
pub(crate) async fn blocking<R, T, F>(app: tauri::AppHandle<R>, f: F) -> pgwhy_core::AppResult<T>
where
    R: tauri::Runtime,
    T: Send + 'static,
    F: FnOnce(&crate::state::AppState) -> pgwhy_core::AppResult<T> + Send + 'static,
{
    use tauri::Manager;
    tokio::task::spawn_blocking(move || f(&app.state::<crate::state::AppState>()))
        .await
        .map_err(|e| {
            pgwhy_core::AppError::internal("background task failed").with_detail(e.to_string())
        })?
}
