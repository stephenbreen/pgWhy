//! Standalone restore (v1.1, Epic 2): put an archive from disk into a database, on any
//! connection, without a Refresh workflow.
//!
//! Three target modes, all explicit (decided 2026-10-22 — `pg_restore --clean` is never offered):
//!
//! - [`RestoreMode::NewDatabase`] (default): create the database, restore into it, verify. A
//!   failure drops the half-restored database, or keeps it for inspection and says so.
//! - [`RestoreMode::Replace`]: create staging, restore, verify, then swap it over the target with
//!   the Refresh pipeline's swap step. (Follow-up commit: it needs the shared swap extraction.)
//! - [`RestoreMode::OnTop`]: drain the database's sessions and restore over it in place. Objects
//!   the archive does not contain survive — the dialog says so, and the confirmation is a typed
//!   database name.
//!
//! The archive's header was read with `pg_restore -l` before this runs (the app layer does that),
//! so the pg_restore version gate has already been applied and progress has a real total.

use crate::destructive::{acquire_lock, check_target, require_typed_name, DestructiveContext};
use crate::engine::Engine;
use crate::history::{
    EndpointRecord, OperationDetail, OperationKind, OperationRecord, OperationStep, RestoreDetail,
    RestoreMode, RunStatus, StepStatus, SwapPhase, SwapState, WorkDbState,
};
use crate::ops::StepProgress;
use crate::ops::{OpCtx, OpEvent, RunCancel};
use crate::staging::LocaleSpec;
use pgwhy_core::config::ConnectionEntry;
use pgwhy_core::{AppError, AppResult, ErrorClass, ErrorKind};
use pgwhy_pgtools::{ArchiveInspection, DiscoveredTool, RestoreOptions, ToolPassword};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::SystemTime;

/// The restore options a user can change. Defaults are the safe ones: ownership and privileges
/// are not restored (remote roles rarely exist locally), and the first failed item stops the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RestoreSettings {
    pub jobs: Option<u32>,
    pub no_owner: bool,
    pub no_privileges: bool,
    pub stop_on_first_error: bool,
}

impl Default for RestoreSettings {
    fn default() -> Self {
        Self {
            jobs: None,
            no_owner: true,
            no_privileges: true,
            stop_on_first_error: true,
        }
    }
}

/// Live events while a restore runs (the UI's progress pane).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RestoreEvent {
    /// The record exists and is being written; the UI uses this id to cancel or to open the
    /// history entry.
    Started {
        id: String,
        name: String,
        mode: RestoreMode,
    },
    Step {
        id: String,
        label: String,
        status: StepStatus,
    },
    Output {
        line: String,
    },
    Progress {
        progress: StepProgress,
    },
}

pub type RestoreSink = Arc<dyn Fn(RestoreEvent) + Send + Sync>;

/// One standalone restore. `database` is the name the operation acts on: the new database's name,
/// the database being replaced, or the existing database restored over.
pub struct RestoreRequest {
    pub connection: ConnectionEntry,
    pub database: String,
    pub archive: PathBuf,
    /// The header already read with `pg_restore -l` (drives the version gate and progress total).
    pub inspection: ArchiveInspection,
    pub mode: RestoreMode,
    /// Encoding/locale for a new database; `None` = the server's `template0` defaults.
    pub locale: Option<LocaleSpec>,
    pub settings: RestoreSettings,
    /// Required for [`RestoreMode::OnTop`] (and `Replace`): what the user typed.
    pub typed_name: Option<String>,
    /// A safety backup taken just before an `OnTop` restore, recorded so retention keeps it.
    pub backup: Option<PathBuf>,
    /// The `pg_restore` the app selected (version-gated against the archive's pg_dump).
    pub tool: DiscoveredTool,
    /// The connection's database password for the child process (never on argv).
    pub password: Option<ToolPassword>,
    pub on_event: Option<RestoreSink>,
}

fn endpoint_record(entry: &ConnectionEntry, database: &str) -> EndpointRecord {
    EndpointRecord {
        connection_id: entry.id.clone(),
        connection_name: entry.name.clone(),
        host: entry.host.clone(),
        port: entry.port,
        database: database.to_string(),
        user: entry.user.clone(),
        server_key: entry.server_key(),
        maintenance_db: entry.maintenance_db.clone(),
        sslmode: entry.sslmode.as_libpq().to_string(),
        tunnel: entry
            .tunnel
            .as_ref()
            .map(|t| format!("{}@{}:{}", t.user, t.host, t.port)),
        env_tag: entry.env_tag,
        server_version: None,
    }
}

fn target_of(entry: &ConnectionEntry, database: &str) -> crate::lifecycle::DbTarget {
    crate::lifecycle::DbTarget {
        connection_id: entry.id.clone(),
        connection_name: entry.name.clone(),
        server: entry.server_key(),
        env_tag: entry.env_tag,
        database: database.to_string(),
    }
}

fn cancelled() -> AppError {
    AppError::new(ErrorKind::Cancelled, "the restore was cancelled")
        .with_class(ErrorClass::Retriable)
}

impl Engine {
    /// Run a standalone restore and return its record. The record is persisted at every step
    /// boundary; a crash leaves it `Running` and `mark_stale_interrupted` fixes it at startup.
    pub async fn restore(&self, req: RestoreRequest) -> AppResult<OperationRecord> {
        check_tool_against_archive(&req.tool, &req.inspection)?;
        let target = target_of(&req.connection, &req.database);
        let ctx = restore_context(&target, req.mode);
        check_target(&ctx)?;
        if req.mode != RestoreMode::NewDatabase {
            require_typed_name(&ctx, req.typed_name.as_deref().unwrap_or(""))?;
        }
        let _lock = acquire_lock(self.locks(), &ctx)?;

        let mut rec = OperationRecord::new(
            OperationKind::Restore,
            format!(
                "Restore {} into {}",
                req.archive
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| req.archive.display().to_string()),
                req.database
            ),
            OperationDetail::Restore(Box::new(RestoreDetail {
                archive: req.inspection.clone(),
                source: None,
                target: endpoint_record(&req.connection, &req.database),
                mode: req.mode,
                backup: req.backup.clone(),
                swap: None,
            })),
        );
        for (id, label) in step_plan(req.mode) {
            rec.steps.push(OperationStep::pending(id, label));
        }
        self.save_operation(&rec)?;

        let cancel = RunCancel::new();
        self.register_operation_cancel(&rec.id, cancel.clone());
        if let Some(sink) = req.on_event.as_ref() {
            sink(RestoreEvent::Started {
                id: rec.id.0.clone(),
                name: rec.name.clone(),
                mode: req.mode,
            });
        }
        let result = self.restore_steps(&req, &mut rec, &cancel).await;
        self.unregister_operation_cancel(&rec.id);

        match result {
            Ok(()) => {
                if rec.status == RunStatus::Running {
                    rec.status = RunStatus::Succeeded;
                }
                rec.finished_at = Some(crate::staging::now_rfc3339());
                self.save_operation(&rec)?;
                Ok(rec)
            }
            Err(e) => {
                let step = rec
                    .steps
                    .iter()
                    .find(|s| s.status == StepStatus::Running || s.status == StepStatus::Failed)
                    .map(|s| s.id.clone());
                if let Some(id) = &step {
                    if let Some(s) = rec.step_mut(id) {
                        s.fail(e.clone());
                    }
                }
                rec.status = if e.kind == ErrorKind::Cancelled {
                    RunStatus::Cancelled
                } else {
                    RunStatus::FailedAtStep
                };
                rec.failed_step = step;
                rec.finished_at = Some(crate::staging::now_rfc3339());
                rec.error = Some(e.clone());
                self.save_operation(&rec)?;
                Err(e)
            }
        }
    }

    async fn restore_steps(
        &self,
        req: &RestoreRequest,
        rec: &mut OperationRecord,
        cancel: &RunCancel,
    ) -> AppResult<()> {
        let ops = self.refresh_ops();
        let emit = sink(req);
        let opctx = OpCtx::new(cancel.clone(), Arc::new(move |ev: OpEvent| emit(ev)));
        let maint = ops.maintenance(&req.connection.id).await?;

        // OnTop touches a live database: get pgWhy's own connections out of the way and keep new
        // ones out until the token drops. Dropped before the verify step — verifying needs a
        // connection (the Refresh pipeline verifies before its quiesce for the same reason).
        let quiesce = if req.mode == RestoreMode::OnTop {
            Some(
                ops.quiesce(&req.connection.id, vec![req.database.clone()])
                    .await?,
            )
        } else {
            None
        };

        // Where the restore lands: a fresh database, a staging database that is then swapped over
        // the target, or the target itself.
        let staging = match req.mode {
            RestoreMode::NewDatabase => req.database.clone(),
            RestoreMode::Replace => crate::staging::staging_name(
                &req.database,
                &crate::staging::compact_timestamp(SystemTime::now()),
            ),
            RestoreMode::OnTop => req.database.clone(),
        };

        if req.mode != RestoreMode::OnTop {
            set_step(
                self,
                rec,
                req.on_event.as_ref(),
                "create",
                StepStatus::Running,
            )?;
            if req.mode == RestoreMode::Replace {
                // The swap state is on disk *before* staging exists, so an interruption at any
                // point names the database to clean up.
                if let Some(s) = rec.step_mut("create") {
                    s.note = Some(format!("staging database {staging}"));
                }
                if let OperationDetail::Restore(r) = &mut rec.detail {
                    r.swap = Some(SwapState {
                        create_mode: false,
                        staging_db: Some(staging.clone()),
                        old_db: Some(crate::staging::old_name(&req.database)),
                        renames_completed: 0,
                        swap_phase: None,
                        work_db_state: WorkDbState::Created,
                        staging_kept: false,
                    });
                }
                self.save_operation(rec)?;
            }
            if let Err(e) = maint.create_database(&staging, req.locale.as_ref()).await {
                if req.mode == RestoreMode::Replace {
                    // nothing was created and the target was never touched
                    rec.warnings.push(format!(
                        "the staging database {staging} was not created, so {} is unchanged",
                        req.database
                    ));
                }
                return Err(e);
            }
            set_step(
                self,
                rec,
                req.on_event.as_ref(),
                "create",
                StepStatus::Succeeded,
            )?;
        }

        if req.mode == RestoreMode::OnTop {
            set_step(
                self,
                rec,
                req.on_event.as_ref(),
                "drain",
                StepStatus::Running,
            )?;
            let timeout = self.config().session_drain_timeout;
            crate::swap::drain(
                &*maint,
                std::slice::from_ref(&req.database),
                timeout,
                Some(cancel),
            )
            .await?;
            set_step(
                self,
                rec,
                req.on_event.as_ref(),
                "drain",
                StepStatus::Succeeded,
            )?;
        }
        if cancel.is_cancelled() {
            return Err(cancelled());
        }

        set_step(
            self,
            rec,
            req.on_event.as_ref(),
            "restore",
            StepStatus::Running,
        )?;
        let log = self.history().log_dir(&rec.id).join("restore.log");
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                AppError::new(
                    ErrorKind::Disk,
                    "could not create the folder for the restore log",
                )
                .with_detail(format!("{}: {e}", dir.display()))
            })?;
        }
        let opts = restore_options(&req.settings, &req.inspection, Some(log.clone()));
        let outcome = ops
            .restore_archive(
                &req.connection,
                &req.tool,
                req.password.as_ref(),
                &req.archive,
                &staging,
                &opts,
                &log,
                &opctx,
            )
            .await;

        drop(quiesce);
        // From here on a failure must leave a clean state: a new or staging database is dropped,
        // an in-place restore says plainly that what was applied stays.
        let tail = self
            .finish_restore(req, rec, &*ops, &*maint, &opctx, cancel, &staging, outcome)
            .await;
        if let Err(e) = tail {
            match req.mode {
                RestoreMode::NewDatabase => {
                    self.discard_half_restored(&req.database, rec, &*maint, cancel)
                        .await;
                }
                RestoreMode::Replace => {
                    // A swap that moved the target aside and could not put it back leaves the
                    // verified staging copy as the only complete database: dropping it would
                    // destroy the very thing "Complete swap" needs. Keep it, and say where
                    // everything is. (A successful rollback resets the counter to 0, so that
                    // case still cleans staging up as before.)
                    let renamed = rec.swap_state().map(|s| s.renames_completed).unwrap_or(0);
                    if renamed > 0 {
                        let old = crate::staging::old_name(&req.database);
                        rec.warnings.push(format!(
                            "the swap stopped part-way. Your original data is in {old}; the \
                             verified restored copy is kept as {staging}; {} may not exist. Use \
                             Complete swap to finish (renames {staging} to {}), or Roll back to \
                             rename {old} back.",
                            req.database, req.database
                        ));
                    } else {
                        self.discard_half_restored(&staging, rec, &*maint, cancel)
                            .await;
                    }
                }
                RestoreMode::OnTop => rec.warnings.push(
                    "the restore stopped part-way: objects applied before the failure are still \
                     in the database. Restore into a new database to avoid a mix."
                        .into(),
                ),
            }
            return Err(e);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_restore(
        &self,
        req: &RestoreRequest,
        rec: &mut OperationRecord,
        ops: &dyn crate::ops::RefreshOps,
        maint: &dyn crate::ops::MaintenanceOps,
        opctx: &OpCtx,
        cancel: &RunCancel,
        staging: &str,
        restore_outcome: AppResult<crate::ops::ToolStepDone>,
    ) -> AppResult<()> {
        let done = restore_outcome?;
        if let Some(s) = rec.step_mut("restore") {
            s.exit_code = done.exit_code;
            s.output_tail = done.output_tail;
            s.log_path = done.log_path;
            s.objects_done = Some(done.objects_done);
        }
        set_step(
            self,
            rec,
            req.on_event.as_ref(),
            "restore",
            StepStatus::Succeeded,
        )?;

        set_step(
            self,
            rec,
            req.on_event.as_ref(),
            "verify",
            StepStatus::Running,
        )?;
        let summary = ops
            .verify_database(&req.connection, staging, None, opctx)
            .await?;
        if let Some(s) = rec.step_mut("verify") {
            s.note = Some(summary);
        }
        set_step(
            self,
            rec,
            req.on_event.as_ref(),
            "verify",
            StepStatus::Succeeded,
        )?;

        if req.mode == RestoreMode::Replace {
            self.swap_replace(req, rec, ops, maint, cancel, staging)
                .await?;
        }
        Ok(())
    }

    /// Swap the verified staging database over the target. The shared step persists the swap
    /// state into this operation's record at every boundary, so an interruption can always be
    /// finished or rolled back.
    async fn swap_replace(
        &self,
        req: &RestoreRequest,
        rec: &mut OperationRecord,
        ops: &dyn crate::ops::RefreshOps,
        maint: &dyn crate::ops::MaintenanceOps,
        cancel: &RunCancel,
        staging: &str,
    ) -> AppResult<()> {
        use crate::swap::{swap_into_place, SwapJob, SwapProgress};

        set_step(
            self,
            rec,
            req.on_event.as_ref(),
            "swap",
            StepStatus::Running,
        )?;
        let old = crate::staging::old_name(&req.database);
        let id = rec.id.clone();
        let mut progress = SwapProgress {
            phase: rec.swap_state().and_then(|s| s.swap_phase),
            renames_completed: rec.swap_state().map(|s| s.renames_completed).unwrap_or(0),
            work_db_state: WorkDbState::Verified,
            warnings: Vec::new(),
        };
        // The record *is* the swap's persistence: every phase and rename is on disk before the
        // next one starts.
        let mut persist = |p: &SwapProgress| {
            if let OperationDetail::Restore(r) = &mut rec.detail {
                let swap = r.swap.get_or_insert(SwapState {
                    create_mode: false,
                    staging_db: Some(staging.to_string()),
                    old_db: Some(old.clone()),
                    renames_completed: 0,
                    swap_phase: None,
                    work_db_state: WorkDbState::Created,
                    staging_kept: false,
                });
                swap.swap_phase = p.phase;
                swap.renames_completed = p.renames_completed;
                swap.work_db_state = p.work_db_state;
            }
            for w in &p.warnings {
                if !rec.warnings.contains(w) {
                    rec.warnings.push(w.clone());
                }
            }
            self.save_operation(rec)
        };
        let in_window = AtomicBool::new(false);
        let job = SwapJob {
            ops,
            maint,
            connection: &req.connection.id,
            target: &req.database,
            staging,
            old: &old,
            create_mode: false,
            run_id: &id,
            owner: &id.0,
            timeout: self.config().session_drain_timeout,
            cancel: Some(cancel),
            in_window: &in_window,
            progress: progress.clone(),
            persist: &mut persist,
        };
        let outcome = swap_into_place(job).await;
        in_window.store(false, std::sync::atomic::Ordering::SeqCst);

        let swapped_in = match outcome {
            Ok(out) => {
                progress = out.progress;
                if let Some(e) = out.cleanup_error {
                    rec.warnings.push(format!(
                        "the swap finished but {old} could not be dropped ({}); drop it when you can",
                        e.message
                    ));
                    rec.status = RunStatus::SucceededCleanupRequired;
                }
                true
            }
            Err(e) => {
                // Recovery, not heroics: one rename done means the target is sitting under `old`,
                // and renaming it back is lossless. Zero renames means nothing moved. Two renames
                // mean the swap actually succeeded (only `_old` cleanup failed).
                let done = rec.swap_state().map(|s| s.renames_completed).unwrap_or(0);
                if done == 2 {
                    rec.warnings.push(format!(
                        "the swap completed but dropping {old} failed; {old} can be dropped when \
                         you are ready"
                    ));
                    progress.phase = Some(SwapPhase::Done);
                    progress.renames_completed = 2;
                    progress.work_db_state = WorkDbState::SwappedIn;
                    rec.status = RunStatus::SucceededCleanupRequired;
                    true
                } else {
                    if done == 1 {
                        let timeout = self.config().session_drain_timeout;
                        match crate::swap::rollback_rename(maint, &req.database, &old, timeout).await
                        {
                            Ok(rollback_warnings) => {
                                rec.warnings.extend(rollback_warnings);
                                rec.warnings.push(format!(
                                    "the swap failed and was rolled back: {} is unchanged",
                                    req.database
                                ));
                                // Nothing is left to recover: clear the rename counter so no
                                // later action tries to finish a swap that was undone.
                                if let OperationDetail::Restore(r) = &mut rec.detail {
                                    if let Some(swap) = &mut r.swap {
                                        swap.renames_completed = 0;
                                        swap.swap_phase = None;
                                        swap.work_db_state = WorkDbState::Dropped;
                                        swap.staging_db = None;
                                        swap.old_db = None;
                                    }
                                }
                            }
                            Err(back) => rec.warnings.push(format!(
                                "the swap failed and the rollback also failed ({}). {} is currently \
                                 named {old}; rename it back when its sessions are gone",
                                back.message, req.database
                            )),
                        }
                    }
                    return Err(e);
                }
            }
        };

        // The record's swap state tracks what the shared step last persisted.
        if let OperationDetail::Restore(r) = &mut rec.detail {
            r.swap = Some(SwapState {
                create_mode: false,
                staging_db: if swapped_in && progress.renames_completed == 2 {
                    None
                } else {
                    Some(staging.to_string())
                },
                old_db: Some(old.clone()),
                renames_completed: progress.renames_completed,
                swap_phase: progress.phase,
                work_db_state: progress.work_db_state,
                staging_kept: false,
            });
        }
        for w in &progress.warnings {
            if !rec.warnings.contains(w) {
                rec.warnings.push(w.clone());
            }
        }
        set_step(
            self,
            rec,
            req.on_event.as_ref(),
            "swap",
            StepStatus::Succeeded,
        )?;
        Ok(())
    }

    /// A failed restore must not leave behind a database it created (a new target, or the staging
    /// copy of a replace). Drop it, or say exactly where it is and why it stayed.
    async fn discard_half_restored(
        &self,
        db: &str,
        rec: &mut OperationRecord,
        maint: &dyn crate::ops::MaintenanceOps,
        cancel: &RunCancel,
    ) {
        if db.is_empty() {
            return;
        }
        if cancel.is_cancelled() {
            // the user stopped it; leave the database alone and say where it is
            rec.warnings.push(format!(
                "{db} holds whatever was restored before the cancel; drop it when you are done"
            ));
            return;
        }
        let timeout = std::time::Duration::from_secs(5);
        match crate::swap::drop_with_drain(maint, db, timeout).await {
            Ok(()) => rec.warnings.push(format!(
                "the database {db} created for this restore was dropped again"
            )),
            Err(e) => rec.warnings.push(format!(
                "the database {db} created for this restore could not be dropped ({}); it was kept \
                 for inspection",
                e.message
            )),
        }
    }
}

/// Mark a step, persist the record (crash truth) and tell the UI.
fn set_step(
    engine: &Engine,
    rec: &mut OperationRecord,
    sink: Option<&RestoreSink>,
    id: &str,
    status: StepStatus,
) -> AppResult<()> {
    let (label, step_id) = match rec.step_mut(id) {
        Some(s) => {
            if status == StepStatus::Running {
                s.start();
            } else {
                s.succeed();
            }
            s.status = status;
            (s.label.clone(), s.id.clone())
        }
        None => return Ok(()),
    };
    engine.save_operation(rec)?;
    if let Some(sink) = sink {
        sink(RestoreEvent::Step {
            id: step_id,
            label,
            status,
        });
    }
    Ok(())
}

fn restore_context(target: &crate::lifecycle::DbTarget, mode: RestoreMode) -> DestructiveContext {
    let action = match mode {
        RestoreMode::NewDatabase => format!("restore into {}", target.database),
        RestoreMode::Replace => format!("replace of {}", target.database),
        RestoreMode::OnTop => format!("restore over {}", target.database),
    };
    target.context(&action)
}

fn step_plan(mode: RestoreMode) -> Vec<(&'static str, String)> {
    match mode {
        RestoreMode::NewDatabase => vec![
            ("create", "Create database".to_string()),
            ("restore", "Restore the archive".to_string()),
            ("verify", "Verify".to_string()),
        ],
        RestoreMode::OnTop => vec![
            ("drain", "Terminate sessions on the database".to_string()),
            ("restore", "Restore the archive".to_string()),
            ("verify", "Verify".to_string()),
        ],
        RestoreMode::Replace => vec![
            ("create", "Create the staging database".to_string()),
            ("restore", "Restore the archive".to_string()),
            ("verify", "Verify".to_string()),
            ("swap", "Swap into place".to_string()),
        ],
    }
}

fn restore_options(
    settings: &RestoreSettings,
    inspection: &ArchiveInspection,
    log: Option<PathBuf>,
) -> RestoreOptions {
    RestoreOptions {
        no_owner: settings.no_owner,
        no_privileges: settings.no_privileges,
        jobs: settings.jobs,
        error_policy: if settings.stop_on_first_error {
            pgwhy_pgtools::ErrorPolicy::ExitOnError
        } else {
            pgwhy_pgtools::ErrorPolicy::ContinueOnError
        },
        total_items: inspection
            .toc
            .as_ref()
            .map(|t| t.restorable_count(!settings.no_privileges)),
        log_path: log,
    }
}

fn sink(req: &RestoreRequest) -> impl Fn(OpEvent) + Send + Sync + 'static {
    let out = req.on_event.clone();
    move |ev: OpEvent| {
        let Some(sink) = &out else { return };
        match ev {
            OpEvent::Output(line) => sink(RestoreEvent::Output { line }),
            OpEvent::Progress(p) => sink(RestoreEvent::Progress { progress: p }),
        }
    }
}

/// `pg_restore` must be at least the version of the `pg_dump` that wrote the archive; the app
/// layer calls this before starting, and again here so the engine cannot be driven around it.
pub fn check_tool_against_archive(
    tool: &DiscoveredTool,
    inspection: &ArchiveInspection,
) -> AppResult<()> {
    let Some(dumped_by) = inspection
        .toc
        .as_ref()
        .and_then(|t| t.header.dumped_by.as_ref())
    else {
        return Ok(()); // header unreadable; pg_restore's own header check will speak
    };
    if tool.version.major < dumped_by.major {
        return Err(AppError::new(
            ErrorKind::ToolVersion,
            format!(
                "pg_restore {} cannot restore an archive written by pg_dump {}",
                tool.version, dumped_by
            ),
        )
        .with_hint(format!(
            "install PostgreSQL {} client tools, or restore with a newer pgWhy",
            dumped_by
        )));
    }
    Ok(())
}
