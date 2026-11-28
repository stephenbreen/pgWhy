//! Live server activity & lock monitoring (`pg_stat_activity` + `pg_locks`) and backend
//! control (`pg_cancel_backend` / `pg_terminate_backend`).

use crate::state::AppState;
use pgwhy_core::config::ConnectionId;
use pgwhy_core::conn::Purpose;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::State;

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub pid: i32,
    pub database: String,
    pub username: String,
    pub application_name: String,
    pub client_addr: String,
    pub state: String,
    pub wait_event_type: String,
    pub wait_event: String,
    pub backend_type: String,
    pub query_duration_secs: Option<f64>,
    pub xact_duration_secs: Option<f64>,
    pub state_duration_secs: Option<f64>,
    pub query: String,
    pub blocking_pids: Vec<i32>,
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockInfo {
    pub pid: i32,
    pub database: String,
    pub lock_type: String,
    pub relation: String,
    pub mode: String,
    pub granted: bool,
    pub fastpath: bool,
    pub virtual_xid: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivitySnapshot {
    pub sessions: Vec<SessionInfo>,
    pub locks: Vec<LockInfo>,
    pub captured_at_ms: u64,
    pub server_version: String,
    pub total_count: usize,
    pub active_count: usize,
    pub idle_in_txn_count: usize,
    pub blocked_count: usize,
}

fn validate_pid(pid: i32) -> AppResult<()> {
    if pid <= 0 {
        return Err(AppError::new(
            ErrorKind::Config,
            format!("invalid backend PID {pid}"),
        ));
    }
    Ok(())
}

/// Snapshot active sessions (`pg_stat_activity` + `pg_blocking_pids`) and locks (`pg_locks`).
/// When `database` is `Some`, filters to that database; when `None`, returns server-wide activity.
#[tauri::command]
pub async fn activity_snapshot(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: Option<String>,
) -> AppResult<ActivitySnapshot> {
    let entry = state.manager.entry(&connection_id)?;
    let conn = state
        .manager
        .open(&connection_id, &entry.maintenance_db, Purpose::Maintenance)
        .await?;

    let db_filter = database.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let session_rows = conn
        .client
        .query(
            "SELECT \
               pid, \
               COALESCE(datname, '') AS database, \
               COALESCE(usename, '') AS username, \
               COALESCE(application_name, '') AS application_name, \
               COALESCE(host(client_addr), '') AS client_addr, \
               COALESCE(state, '') AS state, \
               COALESCE(wait_event_type, '') AS wait_event_type, \
               COALESCE(wait_event, '') AS wait_event, \
               COALESCE(backend_type, '') AS backend_type, \
               EXTRACT(EPOCH FROM (clock_timestamp() - query_start))::float8 AS query_duration_secs, \
               EXTRACT(EPOCH FROM (clock_timestamp() - xact_start))::float8 AS xact_duration_secs, \
               EXTRACT(EPOCH FROM (clock_timestamp() - state_change))::float8 AS state_duration_secs, \
               COALESCE(query, '') AS query, \
               pg_catalog.pg_blocking_pids(pid) AS blocking_pids, \
               pid = pg_catalog.pg_backend_pid() AS is_self \
             FROM pg_catalog.pg_stat_activity \
             WHERE ($1::text IS NULL OR datname = $1) \
             ORDER BY \
               CASE WHEN state = 'active' THEN 0 \
                    WHEN state = 'idle in transaction' THEN 1 \
                    WHEN state = 'idle in transaction (aborted)' THEN 2 \
                    ELSE 3 END, \
               query_start ASC NULLS LAST, \
               pid ASC",
            &[&db_filter],
        )
        .await
        .map_err(|e| pgwhy_core::conn::classify(&e, "read server activity (pg_stat_activity)"))?;

    let sessions: Vec<SessionInfo> = session_rows
        .into_iter()
        .map(|row| SessionInfo {
            pid: row.get(0),
            database: row.get(1),
            username: row.get(2),
            application_name: row.get(3),
            client_addr: row.get(4),
            state: row.get(5),
            wait_event_type: row.get(6),
            wait_event: row.get(7),
            backend_type: row.get(8),
            query_duration_secs: row.get(9),
            xact_duration_secs: row.get(10),
            state_duration_secs: row.get(11),
            query: row.get(12),
            blocking_pids: row.get::<_, Option<Vec<i32>>>(13).unwrap_or_default(),
            is_self: row.get(14),
        })
        .collect();

    let lock_rows = conn
        .client
        .query(
            "SELECT \
               l.pid, \
               COALESCE(d.datname, '') AS database, \
               COALESCE(l.locktype, '') AS lock_type, \
               COALESCE(n.nspname || '.' || c.relname, '') AS relation, \
               COALESCE(l.mode, '') AS mode, \
               l.granted, \
               COALESCE(l.fastpath, false) AS fastpath, \
               COALESCE(l.virtualtransaction, '') AS virtual_xid \
             FROM pg_catalog.pg_locks l \
             LEFT JOIN pg_catalog.pg_database d ON d.oid = l.database \
             LEFT JOIN pg_catalog.pg_class c ON c.oid = l.relation \
             LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE l.pid IS NOT NULL \
               AND l.pid <> pg_catalog.pg_backend_pid() \
               AND ($1::text IS NULL OR d.datname = $1 OR l.database IS NULL) \
             ORDER BY l.granted ASC, l.pid ASC, l.locktype ASC \
             LIMIT 500",
            &[&db_filter],
        )
        .await
        .map_err(|e| pgwhy_core::conn::classify(&e, "read server locks (pg_locks)"))?;

    let locks: Vec<LockInfo> = lock_rows
        .into_iter()
        .map(|row| LockInfo {
            pid: row.get(0),
            database: row.get(1),
            lock_type: row.get(2),
            relation: row.get(3),
            mode: row.get(4),
            granted: row.get(5),
            fastpath: row.get(6),
            virtual_xid: row.get(7),
        })
        .collect();

    let total_count = sessions.len();
    let active_count = sessions
        .iter()
        .filter(|s| !s.is_self && s.state == "active")
        .count();
    let idle_in_txn_count = sessions
        .iter()
        .filter(|s| s.state.starts_with("idle in transaction"))
        .count();
    let blocked_count = sessions
        .iter()
        .filter(|s| !s.blocking_pids.is_empty())
        .count();

    let captured_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let v = conn.info.server_version_num;
    let server_version = if v >= 100_000 {
        format!("{}.{}", v / 10_000, v % 10_000)
    } else if v > 0 {
        format!("{}.{}.{}", v / 10_000, (v / 100) % 100, v % 100)
    } else {
        String::new()
    };

    Ok(ActivitySnapshot {
        sessions,
        locks,
        captured_at_ms,
        server_version,
        total_count,
        active_count,
        idle_in_txn_count,
        blocked_count,
    })
}

/// Request graceful cancellation of a running query (`pg_cancel_backend`).
#[tauri::command]
pub async fn activity_cancel_backend(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    pid: i32,
) -> AppResult<bool> {
    validate_pid(pid)?;
    let entry = state.manager.entry(&connection_id)?;
    let conn = state
        .manager
        .open(&connection_id, &entry.maintenance_db, Purpose::Maintenance)
        .await?;
    let row = conn
        .client
        .query_one("SELECT pg_catalog.pg_cancel_backend($1)", &[&pid])
        .await
        .map_err(|e| pgwhy_core::conn::classify(&e, "cancel backend query"))?;
    Ok(row.get::<_, Option<bool>>(0).unwrap_or(false))
}

/// Terminate a backend session (`pg_terminate_backend`).
#[tauri::command]
pub async fn activity_terminate_backend(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    pid: i32,
) -> AppResult<bool> {
    validate_pid(pid)?;
    let entry = state.manager.entry(&connection_id)?;
    let conn = state
        .manager
        .open(&connection_id, &entry.maintenance_db, Purpose::Maintenance)
        .await?;
    let row = conn
        .client
        .query_one("SELECT pg_catalog.pg_terminate_backend($1)", &[&pid])
        .await
        .map_err(|e| pgwhy_core::conn::classify(&e, "terminate backend session"))?;
    Ok(row.get::<_, Option<bool>>(0).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::validate_pid;

    #[test]
    fn rejects_non_positive_pids() {
        assert!(validate_pid(0).is_err());
        assert!(validate_pid(-42).is_err());
        assert!(validate_pid(12345).is_ok());
    }
}
