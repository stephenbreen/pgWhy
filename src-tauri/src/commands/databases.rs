//! Database lifecycle commands (v1.1 Epic 4): capabilities, create, rename, drop.
//!
//! The verbs themselves live in `pgwhy_workflow::lifecycle` (guardrails, drain, prepared-xact
//! refusal, operation record); this layer resolves the connection entry into a [`DbTarget`],
//! reads a locale when the user asked to copy one, optionally takes the pre-drop safety backup,
//! and invalidates the catalog cache afterwards so the explorer reflects the change.

use crate::commands::backup::{chrono_stamp, run_backup};
use crate::state::AppState;
use pgwhy_core::config::{ConnectionEntry, ConnectionId};
use pgwhy_core::conn::Purpose;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_workflow::lifecycle::DbTarget;
use pgwhy_workflow::{
    check_drop, locale_from_row, locale_sql, CreateDatabaseRequest, DropDatabaseRequest,
    OperationRecord, RenameDatabaseRequest,
};
use serde::Serialize;
use tauri::State;

/// What the create/rename/drop UI may offer on this connection.
#[derive(Debug, Clone, Serialize)]
pub struct DbCapabilitiesView {
    /// `rolsuper OR rolcreatedb` for the connected role: create and replace need it.
    pub can_create_database: bool,
    pub is_superuser: bool,
    pub server_version: String,
    pub current_user: String,
}

pub(crate) fn target(entry: &ConnectionEntry, database: &str) -> DbTarget {
    DbTarget {
        connection_id: entry.id.clone(),
        connection_name: entry.name.clone(),
        server: entry.server_key(),
        env_tag: entry.env_tag,
        database: database.to_string(),
    }
}

/// Read the server's role capabilities once, for the dialog's disabled reasons.
#[tauri::command]
pub async fn db_capabilities(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
) -> AppResult<DbCapabilitiesView> {
    let entry = state.manager.entry(&connection_id)?;
    let conn = state
        .manager
        .open(&connection_id, &entry.maintenance_db, Purpose::Maintenance)
        .await?;
    let row = conn
        .client
        .query_one(
            "select current_user::text, rolsuper, rolcreatedb, \
             current_setting('server_version')::text \
             from pg_catalog.pg_roles where rolname = current_user",
            &[],
        )
        .await
        .map_err(|e| pgwhy_core::conn::classify(&e, "read the server's database capabilities"))?;
    let is_superuser: bool = row.get(1);
    let can_create: bool = row.get(2);
    Ok(DbCapabilitiesView {
        can_create_database: is_superuser || can_create,
        is_superuser,
        server_version: row.get(3),
        current_user: row.get(0),
    })
}

/// The locale of `database`, in the shape `CREATE DATABASE` needs.
pub(crate) async fn read_locale(
    state: &AppState,
    entry: &ConnectionEntry,
    database: &str,
) -> AppResult<pgwhy_workflow::LocaleSpec> {
    let conn = state
        .manager
        .open(&entry.id, &entry.maintenance_db, Purpose::Maintenance)
        .await?;
    let sql = locale_sql(conn.info.server_version_num, "datname = $1");
    let row = conn
        .client
        .query_opt(&sql, &[&database])
        .await
        .map_err(|e| pgwhy_core::conn::classify(&e, "read the database's locale"))?;
    let row = row.ok_or_else(|| {
        AppError::new(
            ErrorKind::Config,
            format!("database {database} does not exist"),
        )
    })?;
    Ok(locale_from_row(&row))
}

/// Create a database. `locale_from` copies another database's encoding/locale; without it the
/// server's `template0` defaults apply.
#[tauri::command]
pub async fn db_create(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    name: String,
    locale_from: Option<String>,
    comment: Option<String>,
) -> AppResult<OperationRecord> {
    let entry = state.manager.entry(&connection_id)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::new(
            ErrorKind::Config,
            "the database name cannot be empty",
        ));
    }
    if name.len() > 63 {
        return Err(AppError::new(
            ErrorKind::Config,
            "PostgreSQL limits database names to 63 bytes",
        ));
    }
    let locale = match locale_from.as_deref().filter(|d| !d.is_empty()) {
        Some(source) => Some(read_locale(&state, &entry, source).await?),
        None => None,
    };
    let rec = state
        .engine()
        .create_database(CreateDatabaseRequest {
            target: target(&entry, &name),
            locale,
            comment,
        })
        .await?;
    state.introspector.invalidate_server(&connection_id);
    Ok(rec)
}

/// Rename a database (drains its sessions first).
#[tauri::command]
pub async fn db_rename(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
    to: String,
) -> AppResult<OperationRecord> {
    let entry = state.manager.entry(&connection_id)?;
    let rec = state
        .engine()
        .rename_database(RenameDatabaseRequest {
            target: target(&entry, &database),
            to: to.trim().to_string(),
        })
        .await?;
    state.introspector.invalidate_server(&connection_id);
    Ok(rec)
}

/// Drop a database. `typed_name` must equal `database` (checked here, before the safety backup,
/// and again inside the verb). `backup_first` takes a normal custom-format backup into the
/// connection's backup folder and records it so retention keeps it.
#[tauri::command]
pub async fn db_drop(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
    typed_name: String,
    backup_first: bool,
) -> AppResult<OperationRecord> {
    let entry = state.manager.entry(&connection_id)?;
    let t = target(&entry, &database);
    // Refuse (tag, typed name) before writing a backup file or touching the server.
    check_drop(&t, &typed_name)?;

    let mut backup = None;
    if backup_first {
        let id = format!("predrop-{}-{}", database, chrono_stamp());
        let done = run_backup(
            &state,
            &id,
            &connection_id,
            &database,
            &crate::commands::backup::BackupRequest::default(),
            None,
        )
        .await?;
        backup = Some(done.path);
    }

    let rec = state
        .engine()
        .drop_database(DropDatabaseRequest {
            target: t,
            typed_name,
            backup,
        })
        .await?;
    state.introspector.invalidate_server(&connection_id);
    Ok(rec)
}
