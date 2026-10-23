//! The seam between the state machine and the outside world.
//!
//! The engine ([`crate::engine`]) owns ordering, persistence, markers, `renames_completed`,
//! cancel rules and recovery; everything that touches a server or spawns a tool goes through
//! [`RefreshOps`] / [`MaintenanceOps`]. [`crate::real::RealOps`] implements them over the
//! connection manager and pgwhy-pgtools; tests substitute a scripted fake to drive every
//! transition with injected failures.

use crate::definition::RefreshWorkflow;
use crate::history::{BackupRecord, CredentialsRecord, RunId, ScriptRecord, SessionInfo, StepId};
use crate::staging::{self, LocaleSpec};
use pgwhy_core::config::{ConnectionEntry, ConnectionId};
use pgwhy_core::conn::BoxFuture;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_pgtools::{DiscoveredTool, FailureCause, ToolPassword};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::watch;

/// The pg tools selected (and version-gated) at preflight.
#[derive(Debug, Clone)]
pub struct PlannedTools {
    pub pg_dump: DiscoveredTool,
    pub pg_restore: DiscoveredTool,
}

/// What a retry run inherits from the run it retries.
#[derive(Debug, Clone)]
pub struct Resume {
    pub retry_of: RunId,
    /// The step the user asked to retry.
    pub from: StepId,
    /// Where this run begins (see the retry matrix in [`crate::retry`]).
    pub begins_at: StepId,
    /// The validated backup to reuse when `begins_at` is after the dump.
    pub backup: Option<BackupRecord>,
    /// Resume against the existing work database (`plan.staging_db`, or the target name for
    /// drop-first) instead of creating one. It is re-stamped with this run's marker.
    pub adopt_work_db: bool,
    /// State of the adopted work database (from the retried run's record).
    pub work_db_state: crate::history::WorkDbState,
    /// Marker-verified leftovers of the retried run to drop before creating a new database.
    pub drop_before_create: Vec<String>,
}

/// Everything a run needs, resolved by preflight before the confirmation (EC-10): entries
/// and tags snapshotted at run start (SM-13), tools, names, locale, the script snapshot and
/// credentials — held in memory only for the run.
#[derive(Debug, Clone)]
pub struct RunPlan {
    pub run_id: RunId,
    pub workflow: RefreshWorkflow,
    pub source: ConnectionEntry,
    pub target: ConnectionEntry,
    pub staging_db: String,
    pub old_db: String,
    /// The target did not exist at preflight.
    pub create_mode: bool,
    pub tools: Option<PlannedTools>,
    pub locale: Option<LocaleSpec>,
    pub script: Option<ScriptRecord>,
    pub source_password: Option<ToolPassword>,
    pub target_password: Option<ToolPassword>,
    pub credentials: CredentialsRecord,
    pub source_version: Option<String>,
    pub target_version: Option<String>,
    /// Catalog pre-count of tables to dump ("~Y").
    pub estimated_objects: Option<u64>,
    /// Folder this workflow's backups live in (retention applies per folder).
    pub backup_dir: PathBuf,
    pub resume: Option<Resume>,
    pub warnings: Vec<String>,
    pub fingerprint: String,
    pub remembered: bool,
}

impl RunPlan {
    /// A plan with names derived from the workflow and `now`; everything else empty. Preflight
    /// fills in the rest; tests use it directly.
    pub fn new(
        workflow: RefreshWorkflow,
        source: ConnectionEntry,
        target: ConnectionEntry,
        backup_dir: PathBuf,
        now: SystemTime,
    ) -> Self {
        let target_db = workflow.target.database.clone();
        let fingerprint = workflow.confirmation_fingerprint(None);
        Self {
            run_id: RunId::new(),
            staging_db: staging::staging_name(&target_db, &staging::compact_timestamp(now)),
            old_db: staging::old_name(&target_db),
            workflow,
            source,
            target,
            create_mode: false,
            tools: None,
            locale: None,
            script: None,
            source_password: None,
            target_password: None,
            credentials: CredentialsRecord {
                source: pgwhy_core::secret::CredentialSource::None,
                target: pgwhy_core::secret::CredentialSource::None,
            },
            source_version: None,
            target_version: None,
            estimated_objects: None,
            backup_dir,
            resume: None,
            warnings: Vec::new(),
            fingerprint,
            remembered: false,
        }
    }

    pub fn target_db(&self) -> &str {
        &self.workflow.target.database
    }

    pub fn source_db(&self) -> &str {
        &self.workflow.source.database
    }

    /// The database restores go into.
    pub fn work_db(&self) -> &str {
        match self.workflow.options.strategy {
            crate::definition::ReplaceStrategy::StagingSwap => &self.staging_db,
            crate::definition::ReplaceStrategy::DropFirst => self.target_db(),
        }
    }

    /// The steps of this run, in order (before resume is applied).
    pub fn steps(&self) -> Vec<StepId> {
        use crate::definition::ReplaceStrategy::*;
        let mut s = vec![StepId::Dump];
        if self.workflow.options.strategy == DropFirst {
            s.push(StepId::DropTarget);
        }
        s.extend([
            StepId::CreateDatabase,
            StepId::Restore,
            StepId::PostRestore,
            StepId::Verify,
        ]);
        if self.workflow.options.strategy == StagingSwap {
            s.push(StepId::Swap);
        }
        s
    }
}

/// Live progress of one step (R12: step / object ~x of ~y / bytes / elapsed — no percentage).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepProgress {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_object: Option<String>,
    pub objects_done: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objects_total: Option<u64>,
    /// Render the total as "~Y" (dump pre-count).
    pub total_is_estimate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    pub elapsed_ms: u64,
}

/// What an op reports while it runs.
#[derive(Debug, Clone)]
pub enum OpEvent {
    Progress(StepProgress),
    /// One line of tool output / script message.
    Output(String),
}

/// Cancellation for one run: a flag the engine and SQL steps watch, plus the pgtools handle
/// that kills the active child's process group.
#[derive(Debug, Clone)]
pub struct RunCancel {
    tx: Arc<watch::Sender<bool>>,
    tools: pgwhy_pgtools::CancelHandle,
}

impl Default for RunCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl RunCancel {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
            tools: pgwhy_pgtools::CancelHandle::new(),
        }
    }

    pub fn cancel(&self) {
        self.tx.send_replace(true);
        self.tools.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    pub fn tools(&self) -> &pgwhy_pgtools::CancelHandle {
        &self.tools
    }
}

/// Per-op context: cancellation and the event emitter.
#[derive(Clone)]
pub struct OpCtx {
    pub cancel: RunCancel,
    emit: Arc<dyn Fn(OpEvent) + Send + Sync>,
}

impl OpCtx {
    pub fn new(cancel: RunCancel, emit: Arc<dyn Fn(OpEvent) + Send + Sync>) -> Self {
        Self { cancel, emit }
    }

    pub fn emit(&self, ev: OpEvent) {
        (self.emit)(ev)
    }

    pub fn output(&self, line: impl Into<String>) {
        self.emit(OpEvent::Output(line.into()))
    }
}

/// Outcome of a child-process step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolStepDone {
    pub exit_code: Option<i32>,
    pub output_tail: Vec<String>,
    pub log_path: Option<PathBuf>,
    pub objects_done: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DumpDone {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub dumped_by: String,
    pub tool: ToolStepDone,
}

/// Reusability verdict of a backup file (retry matrix: reuse requires it, not just a parse).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupVerdict {
    pub reusable: bool,
    pub reason: Option<String>,
    pub cause: Option<FailureCause>,
    pub toc_items: Option<u64>,
    pub dumped_by: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptDone {
    pub statements: usize,
    pub output_tail: Vec<String>,
}

/// A database on the target server as the maintenance connection sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbFact {
    pub name: String,
    pub comment: Option<String>,
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedXact {
    pub gid: String,
    pub database: String,
    pub owner: String,
    pub prepared_at: String,
}

/// Opaque guard returned by [`RefreshOps::quiesce`]; dropping it releases the quiesce.
pub type QuiesceToken = Box<dyn std::any::Any + Send>;

/// Server/tool operations of a run (real or fake).
pub trait RefreshOps: Send + Sync {
    /// `pg_dump` the source to `out` (custom format), full output appended to `log`.
    fn dump<'a>(
        &'a self,
        plan: &'a RunPlan,
        out: &'a Path,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<DumpDone>>;
    /// Reusability verdict for a backup file (`pg_restore -l`).
    fn inspect_backup<'a>(
        &'a self,
        plan: &'a RunPlan,
        path: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<BackupVerdict>>;
    /// `pg_restore` `archive` into `into_db` on the target server. Errors carry pgtools'
    /// `data` (`cause`, `failed_items`, `stderr_tail`, `exit_code`, `log_path`).
    #[allow(clippy::too_many_arguments)]
    fn restore<'a>(
        &'a self,
        plan: &'a RunPlan,
        archive: &'a Path,
        into_db: &'a str,
        total_items: Option<u64>,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>>;
    /// Run the post-restore script against `db` in the script's mode.
    fn run_script<'a>(
        &'a self,
        plan: &'a RunPlan,
        db: &'a str,
        script: &'a ScriptRecord,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ScriptDone>>;
    /// `pg_restore` `archive` into `into_db` on `target`, with a caller-chosen tool. This is the
    /// standalone-restore path: no workflow, no source, no plan.
    #[allow(clippy::too_many_arguments)]
    fn restore_archive<'a>(
        &'a self,
        target: &'a ConnectionEntry,
        tool: &'a pgwhy_pgtools::DiscoveredTool,
        password: Option<&'a ToolPassword>,
        archive: &'a Path,
        into_db: &'a str,
        opts: &'a pgwhy_pgtools::RestoreOptions,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>>;

    /// Connect to `target`/`db` and run the sanity check (`None` = the default "has user
    /// tables"), closing the connection before returning.
    fn verify_database<'a>(
        &'a self,
        target: &'a ConnectionEntry,
        db: &'a str,
        sanity_query: Option<&'a str>,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>>;

    /// Connect to `db` and run the sanity check (closing the connection before returning).
    fn verify<'a>(
        &'a self,
        plan: &'a RunPlan,
        db: &'a str,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>>;
    /// A maintenance-DB connection on the target server (renames can't run from inside the
    /// target).
    fn maintenance<'a>(
        &'a self,
        target: &'a ConnectionId,
    ) -> BoxFuture<'a, AppResult<Box<dyn MaintenanceOps>>>;
    /// Is `db` on `conn` reachable right now (gates retry buttons)?
    fn probe<'a>(&'a self, conn: &'a ConnectionId, db: &'a str) -> BoxFuture<'a, AppResult<()>>;
    /// Get pgWhy's own connections to `dbs` on the target's server out of the way and keep
    /// new ones out until the token drops.
    fn quiesce<'a>(
        &'a self,
        target: &'a ConnectionId,
        dbs: Vec<String>,
    ) -> BoxFuture<'a, AppResult<QuiesceToken>>;
}

/// Statements on the target server's maintenance database.
pub trait MaintenanceOps: Send + Sync {
    fn server_major(&self) -> u32;
    fn is_closed(&self) -> bool;
    fn databases(&self) -> BoxFuture<'_, AppResult<Vec<DbFact>>>;
    /// `CREATE DATABASE … TEMPLATE template0` with `locale` (or the server defaults).
    fn create_database<'a>(
        &'a self,
        name: &'a str,
        locale: Option<&'a LocaleSpec>,
    ) -> BoxFuture<'a, AppResult<()>>;
    fn set_comment<'a>(
        &'a self,
        db: &'a str,
        comment: Option<&'a str>,
    ) -> BoxFuture<'a, AppResult<()>>;
    /// `ALTER DATABASE … RENAME`. Sessions on `from` fail it with [`object_in_use`].
    fn rename_database<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, AppResult<()>>;
    /// `DROP DATABASE IF EXISTS`.
    fn drop_database<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<()>>;
    fn prepared_xacts<'a>(&'a self, db: &'a str) -> BoxFuture<'a, AppResult<Vec<PreparedXact>>>;
    /// Sessions on `dbs`, excluding this connection.
    fn sessions<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<Vec<SessionInfo>>>;
    /// `pg_terminate_backend` every session on `dbs`; returns how many were signalled.
    fn terminate<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<u32>>;
}

/// SQLSTATE 55006: "database … is being accessed by other users".
pub const SQLSTATE_OBJECT_IN_USE: &str = "55006";

/// Whether an error is the server refusing a rename/drop because sessions are attached.
pub fn object_in_use(err: &AppError) -> bool {
    err.data
        .as_ref()
        .and_then(|d| d.get("sqlstate"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == SQLSTATE_OBJECT_IN_USE)
}

/// The error a fake should return for an in-use database (mirrors the server's).
pub fn object_in_use_error(db: &str) -> AppError {
    AppError::new(
        ErrorKind::Sql,
        format!("database \"{db}\" is being accessed by other users"),
    )
    .with_data(serde_json::json!({ "sqlstate": SQLSTATE_OBJECT_IN_USE }))
}
