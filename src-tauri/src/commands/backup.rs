//! Standalone backup of any database (R12, EC-20): pg_dump custom format into the
//! per-connection backup folder, with live progress and retention (keep last K).

use crate::state::AppState;
use pgwhy_core::config::ConnectionId;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_pgtools::{BackupOptions, CancelHandle, DumpFormat, ProgressEvent, RetentionPolicy};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::ipc::Channel;
use tauri::State;

#[derive(Default)]
pub struct Backups {
    running: Mutex<HashMap<String, CancelHandle>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackupEvent {
    Started {
        path: PathBuf,
        pg_dump: String,
        server_version: String,
    },
    Progress {
        progress: ProgressEvent,
    },
    Finished {
        path: PathBuf,
        size_bytes: u64,
        duration_ms: u64,
        removed_by_retention: Vec<PathBuf>,
    },
    Failed {
        error: AppError,
    },
}

/// What to back up. The app builds the pg_dump patterns from these; the webview never sends raw
/// argv.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackupScope {
    Database,
    Schema { schema: String },
    Tables { tables: Vec<TableRef> },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct BackupRequest {
    /// `custom` (single file) or `directory` (parallel).
    pub format: String,
    pub jobs: Option<u32>,
    /// `all`, `schema_only` or `data_only`.
    pub data: String,
    pub scope: BackupScope,
}

impl Default for BackupRequest {
    fn default() -> Self {
        Self {
            format: "custom".into(),
            jobs: None,
            data: "all".into(),
            scope: BackupScope::Database,
        }
    }
}

/// What a finished backup produced (the command turns this into events; drop/restore use the
/// path for their operation record).
#[derive(Debug, Clone)]
pub(crate) struct BackupDone {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub duration_ms: u64,
    pub removed_by_retention: Vec<PathBuf>,
}

/// Safe single path component from a user-visible name.
pub(crate) fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        "db".into()
    } else {
        s
    }
}

impl AppState {
    /// `<backup root>/<connection>-<id8>/<database>/` — shared with workflow backups so one
    /// retention policy covers both.
    pub fn backup_folder(
        &self,
        id: &ConnectionId,
        connection_name: &str,
        database: &str,
    ) -> PathBuf {
        let root = self.backup_dir();
        let short: String = id.0.chars().take(8).collect();
        root.join(format!("{}-{short}", sanitize(connection_name)))
            .join(sanitize(database))
    }
}

#[tauri::command]
pub async fn backup_database(
    state: State<'_, AppState>,
    backup_id: String,
    connection_id: ConnectionId,
    database: String,
    request: BackupRequest,
    events: Channel<BackupEvent>,
) -> AppResult<()> {
    let result = run_backup(
        &state,
        &backup_id,
        &connection_id,
        &database,
        &request,
        Some(&events),
    )
    .await;
    state
        .backups
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&backup_id);
    match result {
        Ok(done) => {
            record_backup(&state, &connection_id, &database, &request, &done);
            let _ = events.send(BackupEvent::Finished {
                path: done.path,
                size_bytes: done.size_bytes,
                duration_ms: done.duration_ms,
                removed_by_retention: done.removed_by_retention,
            });
            Ok(())
        }
        Err(e) => {
            let _ = events.send(BackupEvent::Failed { error: e.clone() });
            Err(e)
        }
    }
}

/// Standalone backups are operations too: recording one puts it in the merged history, from where
/// it can be restored anywhere. A history write that fails must not turn a good backup into a
/// failed command, so it is only logged.
fn record_backup(
    state: &AppState,
    connection_id: &ConnectionId,
    database: &str,
    request: &BackupRequest,
    done: &BackupDone,
) {
    let Ok(entry) = state.manager.entry(connection_id) else {
        return;
    };
    let mut rec = pgwhy_workflow::OperationRecord::new(
        pgwhy_workflow::OperationKind::Backup,
        format!("Back up {} on {}", database, entry.name),
        pgwhy_workflow::OperationDetail::Backup {
            path: done.path.clone(),
            format: if request.format == "directory" {
                DumpFormat::Directory
            } else {
                DumpFormat::Custom
            },
            database: database.to_string(),
            connection: Some(pgwhy_workflow::history::ConnectionRef {
                id: entry.id.clone(),
                name: entry.name.clone(),
            }),
        },
    );
    let mut step =
        pgwhy_workflow::history::OperationStep::pending("backup", format!("Back up {database}"));
    step.start();
    step.succeed();
    rec.steps.push(step);
    rec.status = pgwhy_workflow::RunStatus::Succeeded;
    rec.finished_at = Some(pgwhy_workflow::staging::now_rfc3339());
    rec.warnings = done
        .removed_by_retention
        .iter()
        .map(|p| format!("retention removed {}", p.display()))
        .collect();
    if let Err(e) = state.engine().save_operation(&rec) {
        tracing::warn!(error = %e, "could not record the backup in history");
    }
}

fn send(events: Option<&Channel<BackupEvent>>, event: BackupEvent) {
    if let Some(ch) = events {
        let _ = ch.send(event);
    }
}

/// Take a backup. `events` is optional so internal callers (the pre-drop safety backup) reuse
/// exactly this path — folder, naming, cancellation registry, retention — without a webview.
pub(crate) async fn run_backup(
    state: &AppState,
    backup_id: &str,
    connection_id: &ConnectionId,
    database: &str,
    request: &BackupRequest,
    events: Option<&Channel<BackupEvent>>,
) -> AppResult<BackupDone> {
    let entry = state.manager.entry(connection_id)?;
    let conn = state.manager.introspection(connection_id, database).await?;
    let server_major = conn.info.server_major();
    let row = conn
        .client
        .query_one(
            "select current_setting('server_version'), \
             (select count(*) from pg_class c join pg_namespace n on n.oid = c.relnamespace \
              where c.relkind in ('r','p','m') and n.nspname not in ('pg_catalog','information_schema') \
              and n.nspname not like 'pg_toast%' and n.nspname not like 'pg_temp%')::int8",
            &[],
        )
        .await
        .map_err(|e| pgwhy_core::conn::classify(&e, "count objects to back up"))?;
    let server_version: String = row.get(0);
    let estimated: i64 = row.get(1);
    drop(conn);

    let registry = state.tool_registry(false).await;
    let pg_dump = pgwhy_pgtools::require_pg_dump(&registry, server_major)?;
    let folder = state.backup_folder(connection_id, &entry.name, database);
    std::fs::create_dir_all(&folder).map_err(|e| {
        AppError::new(
            ErrorKind::Disk,
            format!("could not create {}", folder.display()),
        )
        .with_detail(e.to_string())
    })?;
    let stamp = chrono_stamp();
    let (format, suffix) = match request.format.as_str() {
        "directory" => (DumpFormat::Directory, "dir"),
        "custom" => (DumpFormat::Custom, "dump"),
        other => {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("unknown backup format {other}"),
            ))
        }
    };
    // Names are second-granular, so two backups of the same database inside one second (a manual
    // backup followed by a pre-restore safety backup, say) would collide on "already exists".
    let mut stem = format!("{}_{stamp}", sanitize(database));
    let mut n = 1;
    while folder.join(format!("{stem}.{suffix}")).exists()
        || folder.join(format!("{stem}.log")).exists()
    {
        n += 1;
        stem = format!("{}_{stamp}_{n}", sanitize(database));
    }
    let out = folder.join(format!("{stem}.{suffix}"));
    let log = folder.join(format!("{stem}.log"));

    let (endpoint, password, _lease) = state.tool_endpoint(&entry, database).await?;
    let ctx = state.run_context()?;
    state
        .backups
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(backup_id.to_string(), ctx.cancel.clone());
    send(
        events,
        BackupEvent::Started {
            path: out.clone(),
            pg_dump: format!("{} {}", pg_dump.path.display(), pg_dump.version),
            server_version,
        },
    );
    // Patterns are built here, from the typed scope, with the same quoting the rest of the app
    // uses for identifiers.
    let (schemas, tables) = match &request.scope {
        BackupScope::Database => (Vec::new(), Vec::new()),
        BackupScope::Schema { schema } => (
            vec![pgwhy_pgtools::endpoint::schema_pattern(schema)],
            Vec::new(),
        ),
        BackupScope::Tables { tables } => (
            Vec::new(),
            tables
                .iter()
                .map(|t| pgwhy_pgtools::endpoint::table_pattern(&t.schema, &t.name))
                .collect(),
        ),
    };
    let scoped = !schemas.is_empty() || !tables.is_empty();
    let (schema_only, data_only) = match request.data.as_str() {
        "all" => (false, false),
        "schema_only" => (true, false),
        "data_only" => (false, true),
        other => {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("unknown backup contents {other}"),
            ))
        }
    };
    let opts = BackupOptions {
        format,
        exclude_table_data: Vec::new(),
        jobs: request
            .jobs
            .filter(|&j| j > 1 && format == DumpFormat::Directory),
        // A scoped backup's table count is a different query; skip the estimate rather than lie.
        estimated_objects: (!scoped).then(|| estimated.max(0) as u64),
        schemas,
        tables,
        schema_only,
        data_only,
        log_path: Some(log),
    };
    let progress_events = events.cloned();
    let outcome = pgwhy_pgtools::backup(
        &pg_dump,
        &endpoint,
        password.as_ref(),
        &out,
        &opts,
        &ctx,
        move |p| {
            send(
                progress_events.as_ref(),
                BackupEvent::Progress { progress: p },
            );
        },
    )
    .await?;

    let keep = state
        .settings
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .backup_retention
        .max(1) as usize;
    let plan = pgwhy_pgtools::apply_retention(
        &folder,
        &RetentionPolicy {
            keep_last: keep,
            suffixes: vec![".dump".into(), ".dir".into()],
        },
        &HashSet::new(),
    )?;
    // logs follow their dumps
    let _ = pgwhy_pgtools::apply_retention(
        &folder,
        &RetentionPolicy {
            keep_last: keep,
            suffixes: vec![".log".into()],
        },
        &HashSet::new(),
    );
    Ok(BackupDone {
        path: outcome.path,
        size_bytes: outcome.size_bytes,
        duration_ms: outcome.run.duration_ms,
        removed_by_retention: plan.delete,
    })
}

#[tauri::command]
pub fn backup_cancel(state: State<'_, AppState>, backup_id: String) {
    if let Some(c) = state
        .backups
        .running
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&backup_id)
    {
        c.cancel();
    }
}

/// Open a folder in the OS file manager (backup folder, run logs).
#[tauri::command]
pub async fn reveal_path<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    path: PathBuf,
) -> AppResult<()> {
    crate::commands::blocking(app, move |state| {
        let path = state.check_path(&path, true)?;
        let target = if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(path.clone())
        };
        #[cfg(target_os = "macos")]
        let cmd = std::process::Command::new("open").arg(&target).spawn();
        #[cfg(target_os = "windows")]
        let cmd = std::process::Command::new("explorer").arg(&target).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let cmd = std::process::Command::new("xdg-open").arg(&target).spawn();
        cmd.map(|_| ()).map_err(|e| {
            AppError::new(ErrorKind::Config, "could not open the folder").with_detail(e.to_string())
        })
    })
    .await
}

/// Local timestamp for file names, `YYYYmmdd-HHMMSS` (UTC; no tz dependency).
pub(crate) fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil inverse.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_734), (2026, 10, 8));
    }

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("my db/../x"), "my_db_.._x");
        assert_eq!(sanitize(".."), "db");
    }
}
