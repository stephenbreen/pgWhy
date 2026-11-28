//! Run history: one JSON record per run (report schema ER-2), rewritten atomically at every
//! step boundary and before every swap rename, so a crash leaves the last truth on disk.
//!
//! The record doubles as the failure/success report the UI renders: endpoints as discrete
//! fields (never conninfo strings), tool versions, per-step timings / exit codes / output tail
//! and full log path, the backup and its reusability verdict, the post-restore script snapshot
//! and hash, the encoding/locale used, `renames_completed`, failed TOC items, the final live
//! inventory and the state-aware action set. Credentials are recorded by *source* only
//! (keyring / session / none) — never a value.

use crate::definition::{require_safe_id, RefreshOptions, ReplaceStrategy, ScriptMode, WorkflowId};
use crate::staging::{LocaleSpec, Marker};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, ServerKey};
use pgwhy_core::secret::CredentialSource;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_pgtools::runner::FailedItem;
use pgwhy_pgtools::FailureCause;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const RECORD_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

impl RunId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The pipeline steps. Staging-then-swap runs Dump → CreateDatabase (staging) → Restore →
/// PostRestore → Verify → Swap; drop-first runs Dump → DropTarget → CreateDatabase (target)
/// → Restore → PostRestore → Verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepId {
    Dump,
    DropTarget,
    CreateDatabase,
    Restore,
    PostRestore,
    Verify,
    Swap,
}

impl StepId {
    pub fn label(self, strategy: ReplaceStrategy) -> &'static str {
        match (self, strategy) {
            (StepId::Dump, _) => "Back up source",
            (StepId::DropTarget, _) => "Drop target database",
            (StepId::CreateDatabase, ReplaceStrategy::StagingSwap) => "Create staging database",
            (StepId::CreateDatabase, ReplaceStrategy::DropFirst) => "Create target database",
            (StepId::Restore, _) => "Restore",
            (StepId::PostRestore, _) => "Post-restore script",
            (StepId::Verify, _) => "Verify",
            (StepId::Swap, _) => "Swap into place",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// Not part of this run (e.g. no post-restore script, create-mode drop).
    Skipped,
    /// Satisfied by an earlier run this one retries (reused backup / staging).
    Reused,
    /// The app quit or crashed while this step ran.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    pub step: StepId,
    pub label: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Last lines of tool output / script messages.
    #[serde(default)]
    pub output_tail: Vec<String>,
    /// Full tool output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objects_done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl StepRecord {
    pub fn pending(step: StepId, strategy: ReplaceStrategy) -> Self {
        Self {
            step,
            label: step.label(strategy).to_string(),
            status: StepStatus::Pending,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            exit_code: None,
            output_tail: Vec::new(),
            log_path: None,
            objects_done: None,
            error: None,
            note: None,
        }
    }
}

/// Terminal (and the one live) run states — the state machine's exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    /// Both renames done, but dropping `<target>_old` failed: success plus a cleanup action.
    SucceededCleanupRequired,
    FailedAtStep,
    FailedAtSwap,
    Cancelled,
    /// The app quit or crashed mid-run; recovery re-inventories the server.
    Interrupted,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        self != RunStatus::Running
    }

    pub fn is_success(self) -> bool {
        matches!(
            self,
            RunStatus::Succeeded | RunStatus::SucceededCleanupRequired
        )
    }

    /// Failed, cancelled or interrupted — the states with retry/recovery actions.
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            RunStatus::FailedAtStep
                | RunStatus::FailedAtSwap
                | RunStatus::Cancelled
                | RunStatus::Interrupted
        )
    }
}

/// A server endpoint as discrete fields (never a conninfo string).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointRecord {
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub maintenance_db: String,
    pub sslmode: String,
    /// `user@host:port` of the SSH tunnel, when tunneled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    pub env_tag: EnvTag,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// Identity of the server (run-lock key).
    pub server_key: ServerKey,
}

impl EndpointRecord {
    pub fn of(entry: &ConnectionEntry, database: &str) -> Self {
        Self {
            connection_id: entry.id.clone(),
            connection_name: entry.name.clone(),
            host: entry.host.clone(),
            port: entry.port,
            database: database.to_string(),
            user: entry.user.clone(),
            maintenance_db: entry.maintenance_db.clone(),
            sslmode: entry.sslmode.as_libpq().to_string(),
            tunnel: entry
                .tunnel
                .as_ref()
                .map(|t| format!("{}@{}:{}", t.user, t.host, t.port)),
            env_tag: entry.env_tag,
            server_version: None,
            server_key: entry.server_key(),
        }
    }

    /// "name (host:port)" for messages.
    pub fn server_label(&self) -> String {
        format!("{} ({}:{})", self.connection_name, self.host, self.port)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRecord {
    pub path: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolsRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_dump: Option<ToolRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg_restore: Option<ToolRecord>,
}

/// The backup a run produced or reused, with its reusability verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupRecord {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// `Dumped by pg_dump version` (gates pg_restore on reuse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dumped_by: Option<String>,
    pub reusable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<FailureCause>,
    /// Restorable TOC items (restore progress total).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toc_items: Option<u64>,
    /// Set when this run reused another run's backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_run: Option<RunId>,
}

/// The post-restore script a run executed. The plan carries the full text (the run needs it);
/// the persisted record carries only [`ScriptRecord::redacted`]: scripts routinely hold
/// credentials or personal data (anonymisation rules, role passwords), and the record is written
/// to disk at every step and sent to the UI. Nothing reads the text back from a record: a retry
/// re-reads the current script from the library, and the hash says whether it changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRecord {
    pub id: String,
    pub name: String,
    pub mode: ScriptMode,
    pub sha256: String,
    /// Empty in a persisted record (records written before this was redacted still carry it).
    #[serde(default)]
    pub content: String,
    /// Size of the script in bytes, kept in place of the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl ScriptRecord {
    /// The copy that is stored in history: name, mode, hash and size, never the text.
    pub fn redacted(&self) -> Self {
        Self {
            id: self.id.clone(),
            name: self.name.clone(),
            mode: self.mode,
            sha256: self.sha256.clone(),
            content: String::new(),
            size_bytes: self
                .size_bytes
                .or((!self.content.is_empty()).then_some(self.content.len() as u64)),
        }
    }
}

/// Lifecycle of the database a run restores into: staging (staging-then-swap) or the target
/// name itself (drop-first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkDbState {
    Absent,
    /// Created (and marked); nothing restored yet.
    Created,
    /// A restore started but did not finish cleanly.
    Partial,
    Restored,
    /// The post-restore script committed.
    ScriptApplied,
    /// A no-transaction script failed part-way: not reusable for a script retry.
    Mutated,
    Verified,
    /// Renamed to the target name (staging-then-swap success).
    SwappedIn,
    Dropped,
}

impl WorkDbState {
    /// The work database holds a complete, unmutated restore (retry can resume after it).
    pub fn restored_cleanly(self) -> bool {
        matches!(
            self,
            WorkDbState::Restored | WorkDbState::ScriptApplied | WorkDbState::Verified
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialsRecord {
    pub source: CredentialSource,
    pub target: CredentialSource,
}

/// One server session on a database the swap touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub pid: i32,
    pub database: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl SessionInfo {
    pub fn describe(&self) -> String {
        let app = self
            .application_name
            .as_deref()
            .filter(|a| !a.is_empty())
            .unwrap_or("unknown application");
        match &self.client_addr {
            Some(addr) => format!("pid {} ({app} from {addr})", self.pid),
            None => format!("pid {} ({app})", self.pid),
        }
    }
}

/// A database the run cares about, as found by a live scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbPresence {
    pub name: String,
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<Marker>,
    /// The marker names this run (or the run it retried) — safe to drop from this record.
    pub owned_by_run: bool,
    pub sessions: u32,
}

/// Live re-inventory of the target server (never derived from recorded state alone).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub scanned_at: String,
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    pub target: DbPresence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging: Option<DbPresence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<DbPresence>,
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    /// Whether the source answered (gates "re-dump" retries); None = not probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_reachable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_error: Option<AppError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Retry,
    DropStaging,
    KeepStaging,
    DropOld,
    RollbackSwap,
    CompleteSwap,
}

/// What a retry would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryOffer {
    /// The step the user retries ("retry from restore").
    pub from: StepId,
    /// Where the new run actually begins (e.g. retry-from-restore recreates staging first).
    pub begins_at: StepId,
    pub reuses_backup: bool,
    /// Resumes against the existing staging / partial target.
    pub reuses_work_db: bool,
}

/// One state-aware action for a run record (ER-1). Disabled offers say why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionOffer {
    pub kind: ActionKind,
    pub label: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// The database the action touches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryOffer>,
}

/// Swap sub-steps, recorded so an interrupted swap says exactly where it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapPhase {
    Quiescing,
    CheckingPreparedTransactions,
    TerminatingSessions,
    RenamingTargetToOld,
    RenamingStagingToTarget,
    DroppingOld,
    Done,
}

/// One run, start to finish (ER-2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub schema_version: u32,
    pub run_id: RunId,
    pub workflow_id: WorkflowId,
    pub workflow_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<RunId>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub status: RunStatus,
    pub strategy: ReplaceStrategy,
    /// The target did not exist at preflight: staging is renamed into place, no `_old`.
    pub create_mode: bool,
    pub source: EndpointRecord,
    pub target: EndpointRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging_db: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_db: Option<String>,
    pub work_db_state: WorkDbState,
    /// The user chose to keep staging for inspection (suppresses cleanup prompts).
    #[serde(default)]
    pub staging_kept: bool,
    pub tools: ToolsRecord,
    /// Includes the error policy, script mode, exclusions, jobs, ownership options.
    pub options: RefreshOptions,
    pub steps: Vec<StepRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<LocaleSpec>,
    /// How many swap renames have completed (0/1/2), persisted before each rename starts and
    /// after the last one — so after a crash it can lag reality by at most one, which is why
    /// recovery re-inventories. Create-mode writes 2 once its single rename is done.
    pub renames_completed: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_phase: Option<SwapPhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<StepId>,
    #[serde(default)]
    pub failed_items: Vec<FailedItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    /// What the report must lead with (outage, "no working local database", "target
    /// untouched", …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<Inventory>,
    #[serde(default)]
    pub actions: Vec<ActionOffer>,
    pub credentials: CredentialsRecord,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub confirmation_fingerprint: String,
    #[serde(default)]
    pub confirmation_remembered: bool,
}

impl RunRecord {
    pub fn step(&self, id: StepId) -> Option<&StepRecord> {
        self.steps.iter().find(|s| s.step == id)
    }

    pub fn step_mut(&mut self, id: StepId) -> Option<&mut StepRecord> {
        self.steps.iter_mut().find(|s| s.step == id)
    }

    /// The database restores went into: staging, or the target name for drop-first.
    pub fn work_db(&self) -> &str {
        match self.strategy {
            ReplaceStrategy::StagingSwap => {
                self.staging_db.as_deref().unwrap_or(&self.target.database)
            }
            ReplaceStrategy::DropFirst => &self.target.database,
        }
    }

    pub fn action(&self, kind: ActionKind) -> Option<&ActionOffer> {
        self.actions.iter().find(|a| a.kind == kind)
    }
}

// ---------------------------------------------------------------------------------------------
// Operations (v1.1): user-initiated one-off actions recorded beside Refresh runs
// ---------------------------------------------------------------------------------------------

/// The one-off actions that share the run store and the history view.
///
/// These are not workflows: there is no source/destination pipeline, no retry-from-step and no
/// preflight ticket. They are recorded so that "what did I just drop, and where is the backup"
/// has an answer, in the same timeline as the Refresh runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Create,
    Rename,
    Drop,
    Backup,
    Restore,
}

impl OperationKind {
    pub fn label(self) -> &'static str {
        match self {
            OperationKind::Create => "Create database",
            OperationKind::Rename => "Rename database",
            OperationKind::Drop => "Drop database",
            OperationKind::Backup => "Backup",
            OperationKind::Restore => "Restore",
        }
    }

    /// Whether the action can destroy data — the history view marks these.
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            OperationKind::Drop | OperationKind::Rename | OperationKind::Restore
        )
    }
}

/// How a standalone restore puts the archive into place (decided 2026-10-22: `--clean` is never
/// offered; "replace" means staging-then-swap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    /// Create a new database and restore into it. The default.
    NewDatabase,
    /// Restore into staging, then swap it over the named target (staging→swap).
    Replace,
    /// Restore over an existing database in place, leaving objects the archive lacks.
    OnTop,
}

/// The swap half of a replace-restore, in the same shape a Refresh run persists: enough state to
/// finish or roll back a swap that a crash interrupted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapState {
    /// The target did not exist: staging is renamed into place, no `_old`.
    pub create_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging_db: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_db: Option<String>,
    /// How many swap renames completed (0/1/2).
    pub renames_completed: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_phase: Option<SwapPhase>,
    pub work_db_state: WorkDbState,
    #[serde(default)]
    pub staging_kept: bool,
}

impl RunRecord {
    /// This run's swap state, in the shared shape (so restore can reuse swap recovery).
    pub fn swap_state(&self) -> SwapState {
        SwapState {
            create_mode: self.create_mode,
            staging_db: self.staging_db.clone(),
            old_db: self.old_db.clone(),
            renames_completed: self.renames_completed,
            swap_phase: self.swap_phase,
            work_db_state: self.work_db_state,
            staging_kept: self.staging_kept,
        }
    }
}

/// One step of an operation. Same shape and reporting spirit as [`StepRecord`], but with its own
/// step vocabulary: an operation is not a Refresh pipeline, so it must not borrow [`StepId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationStep {
    /// Stable id: `inspect`, `create`, `restore`, `verify`, `swap`, `drop`, `rename`, `backup`.
    pub id: String,
    pub label: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Last lines of tool output.
    #[serde(default)]
    pub output_tail: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objects_done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl OperationStep {
    pub fn pending(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            status: StepStatus::Pending,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            exit_code: None,
            output_tail: Vec::new(),
            log_path: None,
            objects_done: None,
            error: None,
            note: None,
        }
    }

    pub fn start(&mut self) {
        self.status = StepStatus::Running;
        self.started_at = Some(crate::staging::now_rfc3339());
    }

    pub fn succeed(&mut self) {
        self.status = StepStatus::Succeeded;
        self.finished_at = Some(crate::staging::now_rfc3339());
    }

    pub fn fail(&mut self, error: AppError) {
        self.status = StepStatus::Failed;
        self.finished_at = Some(crate::staging::now_rfc3339());
        self.error = Some(error);
    }
}

/// The connection a backup came from — enough to offer "restore this backup" later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRef {
    pub id: ConnectionId,
    pub name: String,
}

/// A backup a history row can hand to the restore dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryBackup {
    pub path: PathBuf,
    /// The connection to restore onto.
    pub connection: ConnectionRef,
    /// The database the backup holds (used as the default new-database name).
    pub database: String,
}

/// What an operation acted on. Serializes as `{"kind": "<operation>", ...}` so the record is
/// self-describing on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationDetail {
    Create {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locale: Option<LocaleSpec>,
    },
    Rename {
        database: String,
        to: String,
    },
    Drop {
        database: String,
        /// A backup taken just before the drop, kept by retention (see `retry::protected_backups`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup: Option<PathBuf>,
        /// Where that backup came from, so history can offer to restore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<ConnectionRef>,
    },
    Backup {
        path: PathBuf,
        format: pgwhy_pgtools::DumpFormat,
        /// The database that was dumped.
        #[serde(default)]
        database: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<ConnectionRef>,
    },
    Restore(Box<RestoreDetail>),
}

/// The restore half of an [`OperationRecord`] (boxed in the enum: it carries the archive
/// inspection and two endpoints, and history is written once per operation, not hot).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestoreDetail {
    /// The archive as inspected: header, size and the reusability verdict.
    pub archive: pgwhy_pgtools::ArchiveInspection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EndpointRecord>,
    pub target: EndpointRecord,
    pub mode: RestoreMode,
    /// A safety backup taken just before an in-place ("on top") restore, kept by retention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
    /// Set for a replace-restore: enough state to finish or roll back an interrupted swap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<SwapState>,
}

/// A one-off operation, start to finish. Shares [`RunId`], [`RunStatus`], [`ToolsRecord`] and the
/// `runs/<id>.json` + log-directory layout with [`RunRecord`], so one store and one history view
/// cover both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub schema_version: u32,
    pub id: RunId,
    pub kind: OperationKind,
    /// User-facing name for the history row, e.g. `Restore app.dump into app_local`.
    pub name: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub status: RunStatus,
    #[serde(default)]
    pub steps: Vec<OperationStep>,
    pub detail: OperationDetail,
    #[serde(default)]
    pub tools: ToolsRecord,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<String>,
}

impl OperationRecord {
    pub fn new(kind: OperationKind, name: impl Into<String>, detail: OperationDetail) -> Self {
        Self {
            schema_version: RECORD_SCHEMA_VERSION,
            id: RunId::new(),
            kind,
            name: name.into(),
            started_at: crate::staging::now_rfc3339(),
            finished_at: None,
            status: RunStatus::Running,
            steps: Vec::new(),
            detail,
            tools: ToolsRecord::default(),
            warnings: Vec::new(),
            error: None,
            failed_step: None,
        }
    }

    pub fn step(&self, id: &str) -> Option<&OperationStep> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn step_mut(&mut self, id: &str) -> Option<&mut OperationStep> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    /// The restore half of this record, when it was a restore.
    pub fn restore(&self) -> Option<&RestoreDetail> {
        match &self.detail {
            OperationDetail::Restore(r) => Some(r),
            _ => None,
        }
    }

    /// The archive this operation restored, when it was a restore.
    pub fn archive_path(&self) -> Option<&Path> {
        self.restore().map(|r| r.archive.path.as_path())
    }

    /// The swap half of a replace-restore, when this operation had one.
    pub fn swap_state(&self) -> Option<&SwapState> {
        self.restore().and_then(|r| r.swap.as_ref())
    }

    /// Mark the record and its running step interrupted (app quit or crash).
    pub fn mark_interrupted(&mut self, why: &str) {
        self.status = RunStatus::Interrupted;
        self.finished_at = Some(crate::staging::now_rfc3339());
        for s in self.steps.iter_mut() {
            if s.status == StepStatus::Running {
                s.status = StepStatus::Interrupted;
                s.finished_at = self.finished_at.clone();
                self.failed_step.get_or_insert_with(|| s.id.clone());
            }
        }
        self.error = Some(
            AppError::new(ErrorKind::Cancelled, why).with_class(pgwhy_core::ErrorClass::Retriable),
        );
    }
}

/// The kind of a history row, flat so the UI can switch on one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    Refresh,
    Create,
    Rename,
    Drop,
    Backup,
    Restore,
}

impl From<OperationKind> for HistoryKind {
    fn from(k: OperationKind) -> Self {
        match k {
            OperationKind::Create => HistoryKind::Create,
            OperationKind::Rename => HistoryKind::Rename,
            OperationKind::Drop => HistoryKind::Drop,
            OperationKind::Backup => HistoryKind::Backup,
            OperationKind::Restore => HistoryKind::Restore,
        }
    }
}

/// One row of the merged history (Refresh runs and one-off operations, newest first).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: RunId,
    pub kind: HistoryKind,
    pub name: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub status: RunStatus,
    /// One line the UI can show without loading the full record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headline: Option<String>,
    /// `database on connection`, when the record names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Set for a Refresh row: what the run view needs to open it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<WorkflowId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    /// A backup this row owns and could restore, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<HistoryBackup>,
    /// True for a record file that exists on disk but could not be parsed (for example one
    /// truncated by a power loss mid-write). `id` is the file's stem and `headline` names the
    /// file. The record may be of a run that was mid-swap, so the file is never deleted or
    /// renamed: it stays where it is for inspection. Additive: absent in older payloads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unreadable: bool,
}

impl HistoryEntry {
    /// The row shown for a record file that could not be read.
    fn from_unreadable(file: &str, why: &str) -> Self {
        Self {
            id: RunId(file.strip_suffix(".json").unwrap_or(file).to_string()),
            kind: HistoryKind::Refresh,
            name: format!("Unreadable record {file}"),
            started_at: String::new(),
            finished_at: None,
            status: RunStatus::Interrupted,
            headline: Some(format!(
                "{file} in the history folder could not be read ({why}). It was left in place; \
                 a run or operation recorded there may need manual recovery."
            )),
            target: None,
            workflow_id: None,
            workflow_name: None,
            backup: None,
            unreadable: true,
        }
    }

    fn from_run(r: &RunRecord) -> Self {
        Self {
            id: r.run_id.clone(),
            kind: HistoryKind::Refresh,
            name: format!("Refresh \"{}\" → {}", r.workflow_name, r.target.database),
            started_at: r.started_at.clone(),
            finished_at: r.finished_at.clone(),
            status: r.status,
            headline: r.headline.clone(),
            target: Some(format!(
                "{} on {}",
                r.target.database, r.target.connection_name
            )),
            workflow_id: Some(r.workflow_id.clone()),
            workflow_name: Some(r.workflow_name.clone()),
            // A run's backup is a copy of the *source*; restoring it belongs on the target
            // connection, where the user's local server lives.
            backup: r
                .backup
                .as_ref()
                .filter(|b| b.reusable)
                .map(|b| HistoryBackup {
                    path: b.path.clone(),
                    connection: ConnectionRef {
                        id: r.target.connection_id.clone(),
                        name: r.target.connection_name.clone(),
                    },
                    database: r.source.database.clone(),
                }),
            unreadable: false,
        }
    }

    fn from_operation(o: &OperationRecord) -> Self {
        let target = match &o.detail {
            OperationDetail::Create { name, .. } => Some(name.clone()),
            OperationDetail::Rename { database, to } => Some(format!("{database} → {to}")),
            OperationDetail::Drop { database, .. } => Some(database.clone()),
            OperationDetail::Backup {
                path,
                database,
                connection,
                ..
            } => Some(match connection {
                Some(c) if !database.is_empty() => format!("{database} on {}", c.name),
                _ => path.display().to_string(),
            }),
            OperationDetail::Restore(r) => Some(match &r.swap {
                Some(s) => format!(
                    "{} on {} ({:?} → {})",
                    r.target.database,
                    r.target.connection_name,
                    r.mode,
                    s.staging_db.as_deref().unwrap_or("staging")
                ),
                None => format!("{} on {}", r.target.database, r.target.connection_name),
            }),
        };
        let headline = o.error.as_ref().map(|e| e.message.clone()).or_else(|| {
            o.steps
                .last()
                .map(|s| format!("{} — {}", s.label, status_word(s.status)))
        });
        let backup = match &o.detail {
            OperationDetail::Drop {
                database,
                backup: Some(path),
                connection: Some(connection),
            } => Some(HistoryBackup {
                path: path.clone(),
                connection: connection.clone(),
                database: database.clone(),
            }),
            OperationDetail::Backup {
                path,
                database,
                connection: Some(connection),
                ..
            } => Some(HistoryBackup {
                path: path.clone(),
                connection: connection.clone(),
                database: database.clone(),
            }),
            _ => None,
        };
        Self {
            id: o.id.clone(),
            kind: HistoryKind::from(o.kind),
            name: o.name.clone(),
            started_at: o.started_at.clone(),
            finished_at: o.finished_at.clone(),
            status: o.status,
            headline,
            target,
            workflow_id: None,
            workflow_name: None,
            backup,
            unreadable: false,
        }
    }
}

fn status_word(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "pending",
        StepStatus::Running => "running",
        StepStatus::Succeeded => "done",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
        StepStatus::Skipped => "skipped",
        StepStatus::Reused => "reused",
        StepStatus::Interrupted => "interrupted",
    }
}

/// Run records under one directory: `<dir>/<run-id>.json`, logs in `<dir>/<run-id>/`.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<dir>/<id>.json`. Ids come from the UI, so one that is not a single safe path component
    /// (a separator, `..`, empty) is an error rather than a path outside the store.
    pub fn record_path(&self, id: &RunId) -> AppResult<PathBuf> {
        require_safe_id("the run id", &id.0)?;
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// Where a run's full tool logs go.
    pub fn log_dir(&self, id: &RunId) -> AppResult<PathBuf> {
        require_safe_id("the run id", &id.0)?;
        Ok(self.dir.join(id.0.as_str()))
    }

    /// Atomically replace the record (temp file + rename in the same directory).
    pub fn save(&self, rec: &RunRecord) -> AppResult<()> {
        self.write(&rec.run_id, rec)
    }

    /// Atomically replace an operation record. Same layout as a run record, so one directory and
    /// one history listing cover both.
    pub fn save_operation(&self, rec: &OperationRecord) -> AppResult<()> {
        self.write(&rec.id, rec)
    }

    fn write<T: Serialize>(&self, id: &RunId, rec: &T) -> AppResult<()> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, e))?;
        let bytes = serde_json::to_vec_pretty(rec).map_err(|e| {
            AppError::internal("could not serialize the history record").with_detail(e.to_string())
        })?;
        let path = self.record_path(id)?;
        let tmp = self
            .dir
            .join(format!(".{id}.{}.tmp", uuid::Uuid::new_v4().simple()));
        // Flush the temp file's data to disk *before* the rename publishes it: otherwise a power
        // loss can leave the new name pointing at a truncated or empty file, which for a run
        // that is mid-swap is the only record of where the databases are.
        let write_tmp = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()
        };
        if let Err(e) = write_tmp() {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(&tmp, e));
        }
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            io(&path, e)
        })?;
        // ...and the directory entry after it, so the rename itself survives. Unix only: Windows
        // cannot open a directory for sync, and NTFS journals the rename metadata anyway.
        #[cfg(unix)]
        if let Ok(d) = std::fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    fn read<T: serde::de::DeserializeOwned>(&self, id: &RunId) -> AppResult<T> {
        let path = self.record_path(id)?;
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::new(ErrorKind::Config, format!("{id} is not in the history"))
            } else {
                io(&path, e)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|e| {
            AppError::new(
                ErrorKind::Config,
                format!("{} is not a valid history record", path.display()),
            )
            .with_detail(e.to_string())
        })
    }

    pub fn load(&self, id: &RunId) -> AppResult<RunRecord> {
        self.read(id)
    }

    pub fn load_operation(&self, id: &RunId) -> AppResult<OperationRecord> {
        self.read(id)
    }

    /// All Refresh records (optionally for one workflow), newest first. Unreadable files are
    /// skipped; operation records are ignored here.
    pub fn list(&self, workflow: Option<&WorkflowId>) -> AppResult<Vec<RunRecord>> {
        let mut out: Vec<RunRecord> = self
            .read_all()?
            .into_iter()
            .filter_map(|r| match r {
                Stored::Run(rec) => Some(rec),
                Stored::Operation(_) | Stored::Unreadable { .. } => None,
            })
            .filter(|rec| workflow.is_none_or(|w| &rec.workflow_id == w))
            .collect();
        sort_newest_first(&mut out, |r| (r.started_at.clone(), r.run_id.0.clone()));
        Ok(out)
    }

    /// One-off operations, newest first.
    pub fn list_operations(&self) -> AppResult<Vec<OperationRecord>> {
        let mut out: Vec<OperationRecord> = self
            .read_all()?
            .into_iter()
            .filter_map(|r| match r {
                Stored::Operation(rec) => Some(rec),
                Stored::Run(_) | Stored::Unreadable { .. } => None,
            })
            .collect();
        sort_newest_first(&mut out, |r| (r.started_at.clone(), r.id.0.clone()));
        Ok(out)
    }

    /// Refresh runs and operations in one timeline, newest first.
    pub fn list_entries(&self) -> AppResult<Vec<HistoryEntry>> {
        let mut out: Vec<HistoryEntry> = self
            .read_all()?
            .into_iter()
            .map(|r| match r {
                Stored::Run(rec) => HistoryEntry::from_run(&rec),
                Stored::Operation(rec) => HistoryEntry::from_operation(&rec),
                Stored::Unreadable { file, why } => HistoryEntry::from_unreadable(&file, &why),
            })
            .collect();
        sort_newest_first(&mut out, |e| (e.started_at.clone(), e.id.0.clone()));
        // Unreadable records have no date to sort by, but they are the rows most worth seeing:
        // put them first (the sort is stable, so the rest keeps its order).
        out.sort_by_key(|e| !e.unreadable);
        Ok(out)
    }

    /// Every record file, parsed into whichever shape it declares. Files that are neither come
    /// back as `Stored::Unreadable`: one corrupt record must not hide the history, and it must
    /// not silently vanish from it either.
    fn read_all(&self) -> AppResult<Vec<Stored>> {
        Ok(self
            .read_all_files()?
            .into_iter()
            .map(|(_, stored)| stored)
            .collect())
    }

    /// [`read_all`](Self::read_all) with each record's own file, so pruning removes exactly the
    /// file a record was read from (never one derived from an id inside the JSON).
    fn read_all_files(&self) -> AppResult<Vec<(PathBuf, Stored)>> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&self.dir, e)),
        };
        let mut out = Vec::new();
        for ent in rd.flatten() {
            let p = ent.path();
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %p.display(), "unreadable history record: {e}");
                    out.push((
                        p,
                        Stored::Unreadable {
                            file: name,
                            why: e.to_string(),
                        },
                    ));
                    continue;
                }
            };
            let run_err = match serde_json::from_slice::<RunRecord>(&bytes) {
                Ok(rec) => {
                    out.push((p, Stored::Run(rec)));
                    continue;
                }
                Err(e) => e,
            };
            if let Ok(rec) = serde_json::from_slice::<OperationRecord>(&bytes) {
                out.push((p, Stored::Operation(rec)));
                continue;
            }
            tracing::warn!(path = %p.display(), "unreadable history record: {run_err}");
            out.push((
                p,
                Stored::Unreadable {
                    file: name,
                    why: run_err.to_string(),
                },
            ));
        }
        Ok(out)
    }

    /// Delete the oldest finished records, keeping the newest `keep_runs_per_workflow` Refresh
    /// records of each workflow and the newest `keep_operations` one-off operations (a record
    /// and its log folder go together). Returns how many records were removed.
    ///
    /// A record is never removed while it still matters, whatever its age: one that is running,
    /// interrupted, stopped mid-swap, or that still offers recovery (kept staging, an `_old`
    /// database to drop, a swap to complete or roll back). Files that cannot be parsed are left
    /// alone too: they may be the only trace of a run that was mid-swap.
    pub fn prune(&self, keep_runs_per_workflow: usize, keep_operations: usize) -> AppResult<usize> {
        let stored = self.read_all_files()?;
        let mut runs: HashMap<WorkflowId, Vec<(&PathBuf, &RunRecord)>> = HashMap::new();
        let mut ops: Vec<(&PathBuf, &OperationRecord)> = Vec::new();
        for (path, s) in &stored {
            match s {
                Stored::Run(r) => runs
                    .entry(r.workflow_id.clone())
                    .or_default()
                    .push((path, r)),
                Stored::Operation(o) => ops.push((path, o)),
                Stored::Unreadable { .. } => {}
            }
        }
        let mut doomed: Vec<(&PathBuf, &RunId)> = Vec::new();
        for group in runs.values_mut() {
            sort_newest_first(group, |(_, r)| (r.started_at.clone(), r.run_id.0.clone()));
            doomed.extend(
                group
                    .iter()
                    .skip(keep_runs_per_workflow)
                    .filter(|(_, r)| !run_needs_keeping(r))
                    .map(|(p, r)| (*p, &r.run_id)),
            );
        }
        sort_newest_first(&mut ops, |(_, o)| (o.started_at.clone(), o.id.0.clone()));
        doomed.extend(
            ops.iter()
                .skip(keep_operations)
                .filter(|(_, o)| !operation_needs_keeping(o))
                .map(|(p, o)| (*p, &o.id)),
        );

        let mut removed = 0;
        for (path, id) in doomed {
            match std::fs::remove_file(path) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "could not prune a history record: {e}");
                    continue;
                }
            }
            // The log folder is named by the id the record declares; only follow it when that is
            // a safe single component, matches the record's file name and is a real directory (never a link out of the store).
            let own_logs = path.file_stem().is_some_and(|s| s == id.0.as_str());
            if let (true, Ok(logs)) = (own_logs, self.log_dir(id)) {
                if std::fs::symlink_metadata(&logs).is_ok_and(|m| m.is_dir()) {
                    if let Err(e) = std::fs::remove_dir_all(&logs) {
                        tracing::warn!(path = %logs.display(), "could not prune run logs: {e}");
                    }
                }
            }
        }
        Ok(removed)
    }
}

/// How many Refresh records per workflow, and how many one-off operations, are kept before the
/// oldest finished ones are pruned.
pub const KEEP_RUNS_PER_WORKFLOW: usize = 50;
pub const KEEP_OPERATIONS: usize = 50;

/// Whether a record must outlive pruning: it is live, or the user may still need to act on it.
fn run_needs_keeping(r: &RunRecord) -> bool {
    use ActionKind::*;
    !r.status.is_terminal()
        || r.status == RunStatus::Interrupted
        || r.status == RunStatus::SucceededCleanupRequired
        || r.staging_kept
        || r.renames_completed == 1
        || r.actions.iter().any(|a| {
            matches!(
                a.kind,
                DropStaging | KeepStaging | DropOld | RollbackSwap | CompleteSwap
            )
        })
}

fn operation_needs_keeping(o: &OperationRecord) -> bool {
    !o.status.is_terminal()
        || o.status == RunStatus::Interrupted
        || o.status == RunStatus::SucceededCleanupRequired
        || o.swap_state().is_some_and(|s| {
            s.staging_kept
                || s.renames_completed == 1
                // staging still named: a swap that never finished may have left it behind
                || (s.staging_db.is_some() && !o.status.is_success())
        })
}

/// A record file, in whichever shape it declares. Both shapes are large and this value lives
/// only for the duration of one `list` call, so the size skew is not worth boxing.
#[allow(clippy::large_enum_variant)]
enum Stored {
    Run(RunRecord),
    Operation(OperationRecord),
    /// A `.json` file that is neither: kept on disk and surfaced by `list_entries`.
    Unreadable {
        file: String,
        why: String,
    },
}

/// Newest first, id descending as the tie-break (same order the run list always used).
fn sort_newest_first<T>(items: &mut [T], key: impl Fn(&T) -> (String, String)) {
    items.sort_by(|a, b| {
        let (a_started, a_id) = key(a);
        let (b_started, b_id) = key(b);
        b_started.cmp(&a_started).then_with(|| b_id.cmp(&a_id))
    });
}

fn io(path: &Path, e: std::io::Error) -> AppError {
    let kind = if e.kind() == std::io::ErrorKind::StorageFull {
        ErrorKind::Disk
    } else {
        ErrorKind::Config
    };
    AppError::new(kind, format!("could not access {}", path.display())).with_detail(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspection(path: &str) -> pgwhy_pgtools::ArchiveInspection {
        pgwhy_pgtools::ArchiveInspection {
            path: path.into(),
            size_bytes: Some(1024),
            reusable: true,
            reason: None,
            cause: None,
            toc: None,
        }
    }

    fn target(db: &str) -> EndpointRecord {
        EndpointRecord {
            connection_id: ConnectionId::new(),
            connection_name: "Local dev".into(),
            host: "127.0.0.1".into(),
            port: 5432,
            database: db.into(),
            user: "me".into(),
            server_key: ServerKey {
                host: "127.0.0.1".into(),
                port: 5432,
                via: None,
            },
            maintenance_db: "postgres".into(),
            sslmode: "prefer".into(),
            tunnel: None,
            env_tag: EnvTag::Local,
            server_version: None,
        }
    }

    fn restore_op(name: &str, archive: &str) -> OperationRecord {
        OperationRecord::new(
            OperationKind::Restore,
            name,
            OperationDetail::Restore(Box::new(RestoreDetail {
                archive: inspection(archive),
                source: None,
                target: target("app_local"),
                mode: RestoreMode::NewDatabase,
                backup: None,
                swap: None,
            })),
        )
    }

    #[test]
    fn operation_records_round_trip_and_serialize_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());

        let mut op = restore_op("Restore app.dump into app_local", "/tmp/app.dump");
        op.steps.push(OperationStep::pending("restore", "Restore"));
        op.step_mut("restore").unwrap().start();
        store.save_operation(&op).unwrap();

        let loaded = store.load_operation(&op.id).unwrap();
        assert_eq!(loaded, op);
        assert_eq!(loaded.kind, OperationKind::Restore);
        assert!(loaded.kind.is_destructive());
        assert_eq!(loaded.archive_path().unwrap(), Path::new("/tmp/app.dump"));

        // the record is self-describing on disk: the detail carries its own tag
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.record_path(&op.id).unwrap()).unwrap())
                .unwrap();
        assert_eq!(json["detail"]["kind"], "restore");
        assert_eq!(json["detail"]["mode"], "new_database");
        assert_eq!(json["status"], "running");
    }

    #[test]
    fn entries_merge_operations_newest_first_and_carry_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());

        let mut older = restore_op("Restore older", "/tmp/older.dump");
        older.started_at = "2026-10-01T10:00:00Z".into();
        older.status = RunStatus::Succeeded;
        older.finished_at = Some("2026-10-01T10:00:05Z".into());
        store.save_operation(&older).unwrap();

        let mut newer = OperationRecord::new(
            OperationKind::Drop,
            "Drop scratch",
            OperationDetail::Drop {
                database: "scratch".into(),
                backup: Some("/tmp/scratch_predrop.dump".into()),
                connection: Some(ConnectionRef {
                    id: ConnectionId::new(),
                    name: "Local dev".into(),
                }),
            },
        );
        newer.started_at = "2026-10-02T10:00:00Z".into();
        store.save_operation(&newer).unwrap();

        let entries = store.list_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, newer.id, "newest first");
        assert_eq!(entries[0].kind, HistoryKind::Drop);
        assert_eq!(entries[0].target.as_deref(), Some("scratch"));
        assert_eq!(entries[1].kind, HistoryKind::Restore);
        assert_eq!(entries[1].target.as_deref(), Some("app_local on Local dev"));

        // the run-scoped listing must not see operations
        assert!(store.list(None).unwrap().is_empty());
        assert_eq!(store.list_operations().unwrap().len(), 2);
    }

    #[test]
    fn one_corrupt_record_does_not_hide_the_rest_of_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        let op = restore_op("Restore app.dump", "/tmp/app.dump");
        store.save_operation(&op).unwrap();
        std::fs::write(dir.path().join("broken.json"), b"{not json").unwrap();
        std::fs::write(dir.path().join(".hidden.json"), b"{}").unwrap();

        // the good record is still listed; the broken one is flagged rather than dropped
        let entries = store.list_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].unreadable);
        assert_eq!(entries[1].id, op.id);
    }

    #[test]
    fn a_truncated_record_is_surfaced_not_hidden_and_stays_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        let op = restore_op("Restore app.dump", "/tmp/app.dump");
        store.save_operation(&op).unwrap();

        // a power loss mid-write: a record cut off half-way
        let bad = dir.path().join("run-lost.json");
        let full = serde_json::to_vec(&op).unwrap();
        std::fs::write(&bad, &full[..full.len() / 2]).unwrap();

        let entries = store.list_entries().unwrap();
        assert_eq!(entries.len(), 2);
        // surfaced first, flagged, and naming the file
        assert!(entries[0].unreadable);
        assert_eq!(entries[0].id.0, "run-lost");
        assert!(entries[0]
            .headline
            .as_deref()
            .unwrap()
            .contains("run-lost.json"));
        assert!(!entries[1].unreadable);
        assert_eq!(entries[1].id, op.id);
        // never deleted or renamed
        assert!(bad.exists());
        // typed listings still return only the records that parse
        assert!(store.list(None).unwrap().is_empty());
        assert_eq!(store.list_operations().unwrap().len(), 1);
    }

    #[test]
    fn saving_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        let mut op = restore_op("Restore app.dump", "/tmp/app.dump");
        store.save_operation(&op).unwrap();
        op.status = RunStatus::Succeeded;
        store.save_operation(&op).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![format!("{}.json", op.id)]);
        assert_eq!(
            store.load_operation(&op.id).unwrap().status,
            RunStatus::Succeeded
        );
    }

    #[test]
    fn a_recorded_backup_is_restorable_from_history() {
        let mut rec = OperationRecord::new(
            OperationKind::Backup,
            "Back up app on Local dev",
            OperationDetail::Backup {
                path: "/backups/app_20261022.dump".into(),
                format: pgwhy_pgtools::DumpFormat::Custom,
                database: "app".into(),
                connection: Some(ConnectionRef {
                    id: ConnectionId::new(),
                    name: "Local dev".into(),
                }),
            },
        );
        rec.status = RunStatus::Succeeded;
        let entry = HistoryEntry::from_operation(&rec);
        assert_eq!(entry.kind, HistoryKind::Backup);
        assert_eq!(entry.target.as_deref(), Some("app on Local dev"));
        let offer = entry.backup.expect("a backup row can be restored");
        assert_eq!(
            offer.path,
            std::path::PathBuf::from("/backups/app_20261022.dump")
        );
        assert_eq!(offer.database, "app");
        assert_eq!(offer.connection.name, "Local dev");

        // a drop's safety backup is offered too, on its own connection
        let drop = OperationRecord::new(
            OperationKind::Drop,
            "Drop scratch",
            OperationDetail::Drop {
                database: "scratch".into(),
                backup: Some("/backups/scratch_predrop.dump".into()),
                connection: Some(ConnectionRef {
                    id: ConnectionId::new(),
                    name: "Local dev".into(),
                }),
            },
        );
        let entry = HistoryEntry::from_operation(&drop);
        assert_eq!(
            entry.backup.map(|b| b.path),
            Some(std::path::PathBuf::from("/backups/scratch_predrop.dump"))
        );
    }

    #[test]
    fn interrupted_marks_the_running_step_and_names_it() {
        let mut op = restore_op("Restore app.dump", "/tmp/app.dump");
        op.steps
            .push(OperationStep::pending("create", "Create database"));
        op.steps.push(OperationStep::pending("restore", "Restore"));
        op.steps[0].succeed();
        op.step_mut("restore").unwrap().start();

        op.mark_interrupted("pgWhy stopped unexpectedly");

        assert_eq!(op.status, RunStatus::Interrupted);
        assert_eq!(op.steps[0].status, StepStatus::Succeeded);
        assert_eq!(op.steps[1].status, StepStatus::Interrupted);
        assert_eq!(op.failed_step.as_deref(), Some("restore"));
        assert!(op.error.is_some());
        assert!(op.finished_at.is_some());
    }

    #[test]
    fn safety_backups_and_archives_in_use_are_protected_from_retention() {
        use crate::retry::protected_backups_with_operations;

        let dropped = OperationRecord::new(
            OperationKind::Drop,
            "Drop scratch",
            OperationDetail::Drop {
                database: "scratch".into(),
                backup: Some("/tmp/scratch_predrop.dump".into()),
                connection: Some(ConnectionRef {
                    id: ConnectionId::new(),
                    name: "Local dev".into(),
                }),
            },
        );
        let mut running = restore_op("Restore", "/tmp/live.dump");
        running.status = RunStatus::Running;
        let mut finished = restore_op("Restore done", "/tmp/done.dump");
        finished.status = RunStatus::Succeeded;

        let protected =
            protected_backups_with_operations(&[], &[dropped.clone(), running.clone(), finished]);
        assert!(protected.contains(Path::new("/tmp/scratch_predrop.dump")));
        assert!(protected.contains(Path::new("/tmp/live.dump")));
        assert!(!protected.contains(Path::new("/tmp/done.dump")));
    }
}

#[cfg(test)]
mod hygiene_tests {
    use super::*;

    fn restore_op(started: &str, status: RunStatus) -> OperationRecord {
        let mut op = OperationRecord::new(
            OperationKind::Restore,
            "Restore",
            OperationDetail::Restore(Box::new(RestoreDetail {
                archive: pgwhy_pgtools::ArchiveInspection {
                    path: "/tmp/a.dump".into(),
                    size_bytes: Some(1),
                    reusable: true,
                    reason: None,
                    cause: None,
                    toc: None,
                },
                source: None,
                target: EndpointRecord {
                    connection_id: ConnectionId::new(),
                    connection_name: "Local".into(),
                    host: "127.0.0.1".into(),
                    port: 5432,
                    database: "app".into(),
                    user: "me".into(),
                    maintenance_db: "postgres".into(),
                    sslmode: "prefer".into(),
                    tunnel: None,
                    env_tag: EnvTag::Local,
                    server_version: None,
                    server_key: ServerKey {
                        host: "127.0.0.1".into(),
                        port: 5432,
                        via: None,
                    },
                },
                mode: RestoreMode::NewDatabase,
                backup: None,
                swap: None,
            })),
        );
        op.started_at = started.into();
        op.status = status;
        op
    }

    #[test]
    fn ids_that_could_leave_the_store_are_refused_at_the_path_choke_points() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        for bad in ["", "..", "../x", "a/b", "a\\b", "/etc/passwd", ".hidden"] {
            let id = RunId(bad.into());
            assert!(store.record_path(&id).is_err(), "{bad:?}");
            assert!(store.log_dir(&id).is_err(), "{bad:?}");
            assert!(store.load(&id).is_err(), "{bad:?}");
        }
        let mut op = restore_op("2026-01-01T00:00:00Z", RunStatus::Succeeded);
        op.id = RunId("../../escape".into());
        assert!(store.save_operation(&op).is_err());
        assert!(!dir.path().parent().unwrap().join("escape.json").exists());
        let ok = RunId::new();
        assert!(store.record_path(&ok).is_ok() && store.log_dir(&ok).is_ok());
    }

    #[test]
    fn pruning_keeps_the_newest_and_everything_still_live_or_awaiting_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        let mut finished = Vec::new();
        for day in 1..=5 {
            let op = restore_op(&format!("2026-01-0{day}T00:00:00Z"), RunStatus::Succeeded);
            std::fs::create_dir_all(store.log_dir(&op.id).unwrap()).unwrap();
            std::fs::write(store.log_dir(&op.id).unwrap().join("restore.log"), b"x").unwrap();
            store.save_operation(&op).unwrap();
            finished.push(op);
        }
        // old but not finished with: a running one, an interrupted one, an unfinished swap
        let running = restore_op("2025-12-01T00:00:00Z", RunStatus::Running);
        let interrupted = restore_op("2025-12-02T00:00:00Z", RunStatus::Interrupted);
        let mut mid_swap = restore_op("2025-12-03T00:00:00Z", RunStatus::FailedAtStep);
        if let OperationDetail::Restore(r) = &mut mid_swap.detail {
            r.swap = Some(SwapState {
                create_mode: false,
                staging_db: Some("app_staging".into()),
                old_db: Some("app_old".into()),
                renames_completed: 1,
                swap_phase: None,
                work_db_state: WorkDbState::Verified,
                staging_kept: false,
            });
        }
        let leftover_old = restore_op("2025-12-04T00:00:00Z", RunStatus::SucceededCleanupRequired);
        let abandoned = restore_op("2025-12-05T00:00:00Z", RunStatus::Cancelled);
        for o in [&running, &interrupted, &mid_swap, &leftover_old, &abandoned] {
            store.save_operation(o).unwrap();
        }
        let unreadable = dir.path().join("broken.json");
        std::fs::write(&unreadable, b"{not json").unwrap();

        // keep the 2 newest; the cancelled one is finished business and goes with the old ones
        let removed = store.prune(50, 2).unwrap();
        let left: Vec<RunId> = store
            .list_operations()
            .unwrap()
            .into_iter()
            .map(|o| o.id)
            .collect();
        let has = |o: &OperationRecord| left.contains(&o.id);
        assert!(has(&finished[4]) && has(&finished[3]), "newest kept");
        assert!(!has(&finished[0]) && !has(&finished[1]) && !has(&finished[2]));
        assert!(!has(&abandoned));
        assert!(has(&running) && has(&interrupted) && has(&mid_swap) && has(&leftover_old));
        assert_eq!(removed, 4);
        // a pruned record's logs go with it; a kept one's stay; unreadable files are untouched
        assert!(!store.log_dir(&finished[0].id).unwrap().exists());
        assert!(store.log_dir(&finished[4].id).unwrap().exists());
        assert!(unreadable.exists());
    }
}
