//! pgWhy Refresh workflow engine: definition model, preflight gates, run state machine
//! (staging-then-swap, drop-first, create-mode), run history, and retry.
//!
//! Integration surface (what the Tauri layer wires to):
//! - [`Engine::preflight`] / [`Engine::retry`] → [`PreflightReport`] (checks, confirmation,
//!   missing secrets, ticket) — nothing destructive;
//! - [`Engine::start`] with the ticket + [`StartRequest`] → run id, [`RunEvent`]s on the sink;
//! - [`Engine::cancel`], [`Engine::shutdown`] (app quit), [`Engine::mark_stale_interrupted`]
//!   (app start);
//! - [`Engine::reinventory`] / [`Engine::perform`] for the state-aware [`ActionOffer`]s on a
//!   [`RunRecord`]; [`Engine::list_runs`] / [`Engine::get_run`] for history.

pub mod definition;
pub mod destructive;
pub mod engine;
pub mod history;
pub mod lifecycle;
pub mod lock;
pub mod ops;
pub mod preflight;
pub mod real;
pub mod restore;
pub mod retry;
pub mod staging;
mod swap;

pub use definition::{
    DbRef, ErrorPolicy, RefreshOptions, RefreshWorkflow, RememberedConfirmation, ReplaceStrategy,
    ScriptMode, WorkflowId, WorkflowStore,
};
pub use destructive::{acquire_lock, check_target, require_typed_name, DestructiveContext};
pub use engine::{Engine, EngineConfig, EventSink, RunEvent, StartRequest};
pub use history::{
    ActionKind, ActionOffer, BackupRecord, ConnectionRef, DbPresence, EndpointRecord,
    HistoryBackup, HistoryEntry, HistoryKind, HistoryStore, Inventory, OperationDetail,
    OperationKind, OperationRecord, OperationStep, RestoreDetail, RestoreMode, RetryOffer, RunId,
    RunRecord, RunStatus, ScriptRecord, SessionInfo, StepId, StepRecord, StepStatus, SwapPhase,
    SwapState, WorkDbState,
};
pub use lifecycle::{
    check_drop, CreateDatabaseRequest, DbTarget, DropDatabaseRequest, RenameDatabaseRequest,
};
pub use lock::{RunLockGuard, RunLocks};
pub use ops::{MaintenanceOps, RefreshOps, RunPlan, StepProgress};
pub use preflight::{
    CheckStatus, Confirmation, MissingSecret, PreflightCheck, PreflightFix, PreflightFixKind,
    PreflightReport, SecretNeed,
};
pub use real::{LoadedScript, RealDeps, RealOps, ScriptSource};
pub use restore::{
    check_tool_against_archive, RestoreEvent, RestoreRequest, RestoreSettings, RestoreSink,
};
pub use staging::{locale_from_row, locale_sql, LocaleProvider, LocaleSpec, Marker, MarkerRole};
