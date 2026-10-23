use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// Basic app identity for the shell (window title, about box) and the IPC smoke test.
#[tauri::command]
pub fn app_info() -> AppInfo {
    AppInfo {
        name: "pgWhy",
        version: env!("CARGO_PKG_VERSION"),
    }
}
