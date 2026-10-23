//! SSH tunnel commands: trust prompts (TOFU / changed key) and status for badges.

use crate::state::AppState;
use pgwhy_core::config::ConnectionId;
use pgwhy_core::tunnel::TunnelStatus;
use pgwhy_core::AppResult;
use tauri::State;

/// Record an explicit trust decision for a host key the user was shown (type + fingerprint).
#[tauri::command]
pub fn trust_host_key(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
) -> AppResult<()> {
    state
        .tunnels
        .trust_host_key(&host, port, &key_type, &fingerprint)
}

#[tauri::command]
pub fn tunnel_statuses(state: State<'_, AppState>) -> Vec<TunnelStatus> {
    state.tunnels.statuses()
}

#[tauri::command]
pub fn tunnel_status(state: State<'_, AppState>, connection_id: ConnectionId) -> TunnelStatus {
    state.tunnels.status(&connection_id)
}
