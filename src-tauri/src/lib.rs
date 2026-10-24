//! pgWhy desktop app entry point.

pub mod commands;
pub mod paths;
pub mod state;
pub mod tools;

use tauri::Manager;

/// Logging policy: never log credential values, key passphrases, or pgpass contents.
/// Connection parameters are logged only as discrete non-secret fields.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("PGWHY_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Commands and state shared by the real app and `tauri::test` command tests.
pub fn configure<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        commands::app::app_info,
        app_exit,
        close_ack,
        close_abort,
        commands::connections::list_connections,
        commands::connections::save_connection,
        commands::connections::delete_connection,
        commands::connections::test_connection,
        commands::connections::connect,
        commands::connections::disconnect,
        commands::connections::live_connections,
        commands::scripts::library_list,
        commands::scripts::library_rescan,
        commands::scripts::script_read,
        commands::scripts::script_save,
        commands::scripts::script_write,
        commands::scripts::script_rename,
        commands::scripts::script_delete,
        commands::scripts::script_set_tags,
        commands::scripts::script_recents,
        commands::scripts::script_recent_record,
        commands::scripts::script_recent_remove,
        commands::scripts::pick_open_path,
        commands::scripts::file_read,
        commands::scripts::file_write,
        commands::scripts::file_check,
        commands::scripts::meta_scan,
        commands::explorer::explorer_databases,
        commands::explorer::explorer_schemas,
        commands::explorer::explorer_schema,
        commands::explorer::explorer_search,
        commands::explorer::explorer_details,
        commands::explorer::explorer_refresh,
        commands::explorer::tree_state_get,
        commands::explorer::tree_state_set,
        commands::explorer::explorer_script_as,
        commands::explorer::explorer_script_options,
        commands::explorer::completion_schema,
        commands::editor::editor_open,
        commands::editor::editor_cancel,
        commands::editor::editor_txn_state,
        commands::editor::editor_close,
        commands::editor::editor_run,
        commands::results::result_subscribe,
        commands::results::result_window,
        commands::results::result_release,
        commands::results::result_copy,
        commands::results::result_export,
        commands::results::pick_save_path,
        commands::grid::grid_open,
        commands::grid::grid_requery,
        commands::grid::grid_edit_cell,
        commands::grid::grid_insert_row,
        commands::grid::grid_delete_rows,
        commands::grid::grid_preview,
        commands::grid::grid_commit,
        commands::grid::grid_discard,
        commands::grid::grid_close,
        commands::backup::backup_database,
        commands::databases::db_capabilities,
        commands::databases::db_create,
        commands::databases::db_rename,
        commands::databases::db_drop,
        commands::restore::pick_restore_archive,
        commands::restore::restore_inspect,
        commands::restore::restore_start,
        commands::restore::restore_cancel,
        commands::restore::restore_targets,
        commands::backup::backup_cancel,
        commands::backup::reveal_path,
        commands::settings::settings_get,
        commands::settings::settings_set,
        commands::settings::pick_folder,
        commands::runs::workflow_preflight,
        commands::runs::workflow_retry_preflight,
        commands::runs::workflow_discard_preflight,
        commands::runs::workflow_apply_fix,
        commands::runs::workflow_provide_secret,
        commands::runs::workflow_start,
        commands::runs::workflow_cancel,
        commands::runs::run_list,
        commands::runs::history_list,
        commands::runs::operation_get,
        commands::runs::run_get,
        commands::runs::run_reinventory,
        commands::runs::run_action,
        commands::workflows::workflow_list,
        commands::workflows::workflow_check_script,
        commands::workflows::workflow_save,
        commands::workflows::workflow_delete,
        commands::tunnels::trust_host_key,
        commands::tunnels::tunnel_statuses,
        commands::tunnels::tunnel_status,
    ])
}

/// Relay connection-manager events to the webview as `conn-event` (tab dispositions, badges).
async fn forward_connection_events(
    app: tauri::AppHandle,
    manager: pgwhy_core::conn::ConnectionManager,
) {
    use tauri::Emitter;
    let mut rx = manager.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => {
                let _ = app.emit("conn-event", ev);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        }
    }
}

/// Close-request handshake. The window's close is intercepted so the UI can ask about open
/// transactions; a webview that crashed (or never wired its listener) would otherwise leave an
/// app that cannot be closed. The UI acknowledges each request at once. A close request that
/// arrives while the previous one is still unacknowledged means the UI is not answering: the
/// backend then asks itself (a native dialog when transactions are open) and quits. There is
/// no timer, so a page that is merely slow is never closed out from under the user.
struct CloseGate {
    requested: std::sync::atomic::AtomicBool,
    acked: std::sync::atomic::AtomicBool,
}

static CLOSE_GATE: CloseGate = CloseGate {
    requested: std::sync::atomic::AtomicBool::new(false),
    acked: std::sync::atomic::AtomicBool::new(false),
};

/// Stop children and tunnels, then exit. Open transactions end with their connections: the
/// server rolls them back when the process goes.
async fn shutdown_and_exit<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    if let Some(state) = app.try_state::<state::AppState>() {
        // quitting mid-run kills the children and records the run as Interrupted
        let _ = state
            .engine()
            .shutdown(std::time::Duration::from_secs(5))
            .await;
        state.tunnels.close_all();
    }
    app.exit(0);
}

/// Quit after the UI has dealt with open transactions (R19): the window's close request is
/// intercepted and forwarded as `close-requested`; the UI asks per tab, then calls this.
///
/// Async so a slow shutdown (a live `pg_dump` being killed) never freezes the window while the
/// command is in flight.
#[tauri::command]
async fn app_exit<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> pgwhy_core::AppResult<()> {
    shutdown_and_exit(app).await;
    Ok(())
}

/// The UI received `close-requested` and is handling it (it may be waiting on the user).
#[tauri::command]
fn close_ack() {
    CLOSE_GATE
        .acked
        .store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The user chose to keep the app open: the next close request starts afresh.
#[tauri::command]
fn close_abort() {
    use std::sync::atomic::Ordering::SeqCst;
    CLOSE_GATE.acked.store(false, SeqCst);
    CLOSE_GATE.requested.store(false, SeqCst);
}

fn on_close_requested<R: tauri::Runtime>(window: &tauri::Window<R>, api: &tauri::CloseRequestApi) {
    use std::sync::atomic::Ordering::SeqCst;
    use tauri::Emitter;
    api.prevent_close();
    let pending = CLOSE_GATE.requested.swap(true, SeqCst);
    // Each request needs its own acknowledgement: a UI that answered an earlier one may have
    // died since (e.g. while its commit/rollback prompt was up).
    let answered = CLOSE_GATE.acked.swap(false, SeqCst);
    if pending && !answered {
        // asked before and the UI never answered: it is not coming
        tauri::async_runtime::spawn(force_quit(window.app_handle().clone()));
        return;
    }
    let _ = window.emit("close-requested", ());
}

/// Quit without the UI. Open transactions would be rolled back by the server when their
/// connections go, so when there are any, the user confirms that in a native dialog first.
async fn force_quit<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let open = app
        .try_state::<state::AppState>()
        .map(|s| s.editors.open_transactions().len())
        .unwrap_or(0);
    if open > 0 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.dialog()
            .message(format!(
                "The window is not responding. {open} editor tab{} {} an open transaction; \
                 quitting now rolls {} back.",
                if open == 1 { "" } else { "s" },
                if open == 1 { "has" } else { "have" },
                if open == 1 { "it" } else { "them" },
            ))
            .title("Quit pgWhy?")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Quit and roll back".into(),
                "Keep open".into(),
            ))
            .show(move |quit| {
                let _ = tx.send(quit);
            });
        if !rx.await.unwrap_or(false) {
            close_abort();
            return;
        }
    }
    shutdown_and_exit(app).await;
}

/// Single instance in the shipped app: the per-server run lock is process-local, so a second
/// launch must focus the running instance rather than start a competing one.
///
/// The e2e harness launches one process per spec file and expects a *fresh* app each time; with
/// the plugin compiled in, every launch after the first hands off to the still-running instance
/// and the specs drive a stale UI. So it is not registered in `--features e2e` builds.
#[cfg(feature = "e2e")]
fn with_single_instance<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
}

#[cfg(not(feature = "e2e"))]
fn with_single_instance<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
        use tauri::Manager;
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
    }))
}

/// WebdriverIO plugins, compiled in only for e2e builds (`--features e2e`).
#[cfg(feature = "e2e")]
fn with_e2e(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder
        .plugin(tauri_plugin_wdio::init())
        .plugin(tauri_plugin_wdio_webdriver::init())
}

#[cfg(not(feature = "e2e"))]
fn with_e2e(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder
}

/// Process start, for the cold-start budget (R17: usable connection list in < 2 s).
pub static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn run() {
    STARTED.get_or_init(std::time::Instant::now);
    init_tracing();
    with_single_instance(with_e2e(configure(tauri::Builder::default())))
        .setup(|app| {
            let paths = app.path();
            // PGWHY_CONFIG_DIR / PGWHY_DATA_DIR isolate test runs (e2e) from the user's real
            // config; PGWHY_SESSION_SECRETS=1 keeps credentials in memory instead of the keychain
            let config_dir = std::env::var_os("PGWHY_CONFIG_DIR")
                .map(Into::into)
                .unwrap_or(paths.app_config_dir()?);
            let data_dir = std::env::var_os("PGWHY_DATA_DIR")
                .map(Into::into)
                .unwrap_or(paths.app_data_dir()?);
            let state = if std::env::var("PGWHY_SESSION_SECRETS").is_ok_and(|v| v == "1") {
                state::AppState::with_secrets(
                    config_dir,
                    data_dir,
                    std::sync::Arc::new(pgwhy_core::secret::MemoryStore::unavailable()),
                )
            } else {
                state::AppState::load(config_dir, data_dir)
            }
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
            // idle introspection connections are closed by policy and reopen lazily
            let manager = state.manager.clone();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                manager.spawn_idle_sweeper();
                forward_connection_events(handle, manager).await;
            });
            let mut tunnel_events = state.tunnels.subscribe();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use tauri::Emitter;
                loop {
                    match tunnel_events.recv().await {
                        Ok(ev) => {
                            let _ = handle.emit("tunnel-event", ev);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    }
                }
            });
            // leftovers of a crashed run must not outlive it (temp pgpass = live credential)
            if let Ok(dir) = state.pgpass_dir() {
                let _ = pgwhy_pgtools::sweep_stale_pgpass(dir.path());
            }
            // runs that were mid-flight when pgWhy last stopped become Interrupted (with live
            // re-inventory offered from history)
            let _ = state.engine().mark_stale_interrupted();
            app.manage(state);
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                on_close_requested(window, api);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running pgWhy");
}

#[cfg(test)]
mod tests {
    use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
    use tauri::webview::InvokeRequest;

    #[test]
    fn app_info_command_round_trips_over_ipc() {
        let app = super::configure(mock_builder())
            .build(mock_context(noop_assets()))
            .expect("build mock app");
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview");
        let res = get_ipc_response(
            &webview,
            InvokeRequest {
                cmd: "app_info".into(),
                callback: tauri::ipc::CallbackFn(0),
                error: tauri::ipc::CallbackFn(1),
                // the origin the app is served from: wry serves custom protocols over http on Windows
                url: if cfg!(windows) {
                    "http://tauri.localhost"
                } else {
                    "tauri://localhost"
                }
                .parse()
                .unwrap(),
                body: tauri::ipc::InvokeBody::default(),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .expect("app_info response");
        let v: serde_json::Value = res.deserialize().unwrap();
        assert_eq!(v["name"], "pgWhy");
    }
}
