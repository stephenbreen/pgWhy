//! Standalone restore commands (v1.1 Epic 2): pick and inspect an archive, start a restore,
//! cancel it. The pipeline itself lives in `pgwhy_workflow::restore`; this layer resolves the
//! connection, re-inspects the archive (never trusting the webview's copy), gates the pg_restore
//! version, resolves the password, optionally takes a safety backup, and forwards live events.

use crate::commands::backup::{chrono_stamp, run_backup};
use crate::state::AppState;
use pgwhy_core::config::ConnectionId;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_pgtools::{ArchiveCheck, ArchiveInspection, DiscoveredTool, ToolKind};
use pgwhy_workflow::restore::{RestoreEvent, RestoreSettings};
use pgwhy_workflow::{check_target, require_typed_name, RestoreMode, RestoreRequest, RunId};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri::State;

/// What the restore dialog shows before the user commits to anything.
#[derive(Debug, Clone, Serialize)]
pub struct RestoreInspectionView {
    pub archive: ArchiveInspection,
    pub tool: String,
    /// The source database recorded in the archive header, when readable.
    pub dbname: Option<String>,
    pub dumped_from: Option<String>,
    pub dumped_by: Option<String>,
    pub toc_entries: Option<u32>,
    /// Refusals that must disable the Start button (plain SQL, unreadable, version gate, …).
    pub blockers: Vec<String>,
}

fn plain_sql(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 64];
    let Ok(n) = f.read(&mut head) else {
        return false;
    };
    let text = String::from_utf8_lossy(&head[..n]);
    !text.starts_with("PGDMP") && text.contains("PostgreSQL database dump")
}

/// Native file picker for a restore source.
#[tauri::command]
pub async fn pick_restore_archive<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: State<'_, AppState>,
) -> AppResult<Option<PathBuf>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("PostgreSQL archives", &["dump", "backup", "tar", "gz"])
        .pick_file(move |p| {
            let _ = tx.send(p.and_then(|p| p.into_path().ok()));
        });
    let path = rx.await.ok().flatten();
    if let Some(p) = &path {
        state.grants.grant_file(p);
    }
    Ok(path)
}

/// Read an archive's header before starting: versions (for the gate), source database, TOC size,
/// and any reason the archive cannot be restored.
#[tauri::command]
pub async fn restore_inspect(
    state: State<'_, AppState>,
    path: PathBuf,
) -> AppResult<RestoreInspectionView> {
    // Archives come from the restore picker or from the backup folder (history rows).
    let path = state.check_path(&path, false)?;
    let mut blockers = Vec::new();
    if path.is_dir() {
        blockers.push(
            "this is a directory-format archive; choose the single-file (.dump) archive instead"
                .to_string(),
        );
    }
    let registry = state.tool_registry(false).await;
    let inspect_tool = registry
        .highest(ToolKind::PgRestore)
        .cloned()
        .ok_or_else(|| {
            AppError::new(
                ErrorKind::ToolMissing,
                "no pg_restore was found on this machine, so archives cannot be read",
            )
            .with_hint("install PostgreSQL client tools, or set their folder in Settings")
        })?;
    let ctx = state.run_context()?;
    let inspection =
        pgwhy_pgtools::inspect_archive(&inspect_tool, &path, ArchiveCheck::TocOnly, &ctx).await?;

    if plain_sql(&path) {
        blockers.push(
            "this is a plain-SQL dump (pg_dump -Fp), which restores through psql. pgWhy restores \
             custom-format archives; take the backup in custom format instead"
                .to_string(),
        );
    }
    if !inspection.reusable {
        blockers.push(
            inspection
                .reason
                .clone()
                .unwrap_or_else(|| "pg_restore could not read this archive".into()),
        );
    }

    let header = inspection.toc.as_ref().map(|t| &t.header);
    let dumped_by = header.and_then(|h| h.dumped_by.clone());
    // The gate: pg_restore must be at least the pg_dump that wrote the archive.
    let tool = match &dumped_by {
        Some(v) => match pgwhy_pgtools::require_pg_restore(&registry, v) {
            Ok(t) => Some(t),
            Err(e) => {
                blockers.push(e.message.clone());
                registry.highest(ToolKind::PgRestore).cloned()
            }
        },
        None => registry.highest(ToolKind::PgRestore).cloned(),
    };
    let tool = tool.ok_or_else(|| {
        AppError::new(
            ErrorKind::ToolMissing,
            "no pg_restore was found on this machine",
        )
    })?;

    Ok(RestoreInspectionView {
        archive: inspection.clone(),
        tool: format!("{} {}", tool.path.display(), tool.version),
        dbname: header.and_then(|h| h.dbname.clone()),
        dumped_from: header.and_then(|h| h.dumped_from.as_ref().map(|v| v.raw.clone())),
        dumped_by: dumped_by.map(|v| v.raw),
        toc_entries: header.and_then(|h| h.toc_entries),
        blockers,
    })
}

/// Everything the dialog collected.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RestoreStartRequest {
    pub path: PathBuf,
    /// The new database name, or the existing one being restored over.
    pub database: String,
    pub mode: RestoreMode,
    /// Copy encoding/locale from this database when creating a new one.
    #[serde(default)]
    pub locale_from: Option<String>,
    #[serde(default)]
    pub settings: RestoreSettings,
    /// Required for `on_top`.
    #[serde(default)]
    pub typed_name: Option<String>,
    /// Take a custom-format backup before an in-place restore.
    #[serde(default)]
    pub backup_first: bool,
}

/// The destructive-operation gate for a restore, with the same context the engine builds.
fn check_restore(
    entry: &pgwhy_core::config::ConnectionEntry,
    request: &RestoreStartRequest,
) -> AppResult<()> {
    let db = &request.database;
    let action = match request.mode {
        RestoreMode::NewDatabase => format!("restore into {db}"),
        RestoreMode::Replace => format!("replace of {db}"),
        RestoreMode::OnTop => format!("restore over {db}"),
    };
    let ctx = crate::commands::databases::target(entry, db).context(&action);
    check_target(&ctx)?;
    if request.mode != RestoreMode::NewDatabase {
        require_typed_name(&ctx, request.typed_name.as_deref().unwrap_or(""))?;
    }
    Ok(())
}

/// Start a restore. Events stream over `events`; the finished (or failed) record is the result.
#[tauri::command]
pub async fn restore_start(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    request: RestoreStartRequest,
    events: Channel<RestoreEvent>,
) -> AppResult<pgwhy_workflow::OperationRecord> {
    let entry = state.manager.entry(&connection_id)?;
    // Refuse (tag, typed name) before any side effect: the safety backup below writes a dump of
    // the target to disk, which a refused restore must never do. Re-checked inside the engine.
    check_restore(&entry, &request)?;
    let request_path = state.check_path(&request.path, false)?;
    let registry = state.tool_registry(false).await;
    let inspect_tool = registry
        .highest(ToolKind::PgRestore)
        .cloned()
        .ok_or_else(|| AppError::new(ErrorKind::ToolMissing, "no pg_restore was found"))?;
    let ctx = state.run_context()?;
    // Re-inspect server-side: the webview's copy is display only, and the gate must not be
    // bypassable by a stale or edited dialog.
    let inspection =
        pgwhy_pgtools::inspect_archive(&inspect_tool, &request_path, ArchiveCheck::TocOnly, &ctx)
            .await?;
    if plain_sql(&request_path) {
        return Err(AppError::new(
            ErrorKind::Refused,
            "plain-SQL dumps cannot be restored by pgWhy (they need psql)",
        )
        .with_hint("restore a custom-format archive, or run the .sql file with psql"));
    }
    if !inspection.reusable {
        return Err(AppError::new(
            ErrorKind::ToolFailed,
            inspection
                .reason
                .clone()
                .unwrap_or_else(|| "the archive cannot be read".into()),
        )
        .with_hint("take a new backup of the source database"));
    }
    let dumped_by = inspection
        .toc
        .as_ref()
        .and_then(|t| t.header.dumped_by.clone())
        .ok_or_else(|| {
            AppError::new(
                ErrorKind::ToolVersion,
                "the archive does not record which pg_dump wrote it, so the pg_restore version cannot be checked",
            )
        })?;
    let tool: DiscoveredTool = pgwhy_pgtools::require_pg_restore(&registry, &dumped_by)?;

    let locale = match request.mode {
        RestoreMode::NewDatabase => {
            match request.locale_from.as_deref().filter(|s| !s.is_empty()) {
                Some(source) => {
                    Some(crate::commands::databases::read_locale(&state, &entry, source).await?)
                }
                None => None,
            }
        }
        // A replaced database keeps its locale and encoding: the staging copy has to match the
        // database it will become.
        RestoreMode::Replace => {
            Some(crate::commands::databases::read_locale(&state, &entry, &request.database).await?)
        }
        RestoreMode::OnTop => None,
    };

    // The child's password: resolved here (keychain), never on argv.
    let (_endpoint, password, _lease) = state.tool_endpoint(&entry, &request.database).await?;

    let mut backup = None;
    if request.mode != RestoreMode::NewDatabase && request.backup_first {
        let id = format!("prerestore-{}-{}", request.database, chrono_stamp());
        let done = run_backup(
            &state,
            &id,
            &connection_id,
            &request.database,
            &crate::commands::backup::BackupRequest::default(),
            None,
        )
        .await?;
        backup = Some(done.path);
    }

    let ch = events.clone();
    let sink: pgwhy_workflow::RestoreSink = Arc::new(move |e| {
        let _ = ch.send(e);
    });
    let rec = state
        .engine()
        .restore(RestoreRequest {
            connection: entry.clone(),
            database: request.database.clone(),
            archive: request_path,
            inspection,
            mode: request.mode,
            locale,
            settings: request.settings,
            typed_name: request.typed_name,
            backup,
            tool,
            password,
            on_event: Some(sink),
        })
        .await?;
    state.introspector.invalidate_server(&connection_id);
    Ok(rec)
}

/// Cancel a running restore by its record id (from the `started` event).
#[tauri::command]
pub fn restore_cancel(state: State<'_, AppState>, run_id: RunId) -> AppResult<bool> {
    Ok(state.engine().cancel_operation(&run_id))
}

/// The connection's own capabilities + database list, for the dialog's target pickers.
#[tauri::command]
pub async fn restore_targets(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
) -> AppResult<RestoreTargets> {
    let dbs = state.introspector.databases(&connection_id).await?;
    let caps =
        crate::commands::databases::db_capabilities(state.clone(), connection_id.clone()).await;
    Ok(RestoreTargets {
        databases: dbs
            .iter()
            .filter(|d| !d.is_template)
            .map(|d| d.name.clone())
            .collect(),
        can_create_database: caps
            .as_ref()
            .map(|c| c.can_create_database)
            .unwrap_or(false),
        capabilities_error: caps.err().map(|e| e.message),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreTargets {
    pub databases: Vec<String>,
    pub can_create_database: bool,
    pub capabilities_error: Option<String>,
}
