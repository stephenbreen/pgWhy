//! The Refresh run state machine and the engine API the app wires to.
//!
//! Flow: [`Engine::preflight`] runs the gate sequence and parks a *ticket* (resolved plan +
//! run lock) → the UI shows the single confirmation → [`Engine::start`] spawns the run on a
//! tokio task that emits [`RunEvent`]s and persists the [`RunRecord`] at every step boundary
//! and before every swap rename. Failures land in a terminal state with a live re-inventory
//! and a state-aware action set; [`Engine::retry`], [`Engine::perform`] and
//! [`Engine::reinventory`] act on records afterwards. [`Engine::shutdown`] is the app-quit
//! hook (`Interrupted`, `renames_completed` recorded); [`Engine::mark_stale_interrupted`] is
//! the startup hook for records a crash left `running`.
//!
//! Cancel kills the active child / cancels the active statement, and is refused only while the
//! swap is between its two renames (the outage window — finishing is the safe exit).

use crate::definition::{RefreshWorkflow, ReplaceStrategy, ScriptMode, WorkflowStore};
use crate::history::*;
use crate::lock::{RunLockGuard, RunLocks};
use crate::ops::*;
use crate::preflight::{self, PreflightReport};
use crate::real::{RealDeps, RealOps};
use crate::retry;
use crate::staging::{self, now_rfc3339, Marker, MarkerRole};
use pgwhy_core::config::{ConnectionId, ServerKey};
use pgwhy_core::{AppError, AppResult, ErrorClass, ErrorKind};
use pgwhy_pgtools::FailureCause;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::watch;

/// Engine settings.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Run records (`<id>.json`) and logs (`<id>/`).
    pub runs_dir: PathBuf,
    /// Backups go to `<backup_root>/<workflow-id>/`.
    pub backup_root: PathBuf,
    /// Backups kept per workflow besides those protected for retry (an app setting).
    pub retention_keep: usize,
    /// How long the swap waits for sessions to end (terminate + poll) before failing
    /// retryably with the blocking sessions named.
    pub session_drain_timeout: Duration,
    /// A preflight ticket not confirmed within this long must be re-checked.
    pub ticket_ttl: Duration,
    /// When set, a successful run marks the stored workflow `has_succeeded`, and a confirmed
    /// "remember" is saved to it.
    pub workflows: Option<WorkflowStore>,
}

impl EngineConfig {
    pub fn new(runs_dir: impl Into<PathBuf>, backup_root: impl Into<PathBuf>) -> Self {
        Self {
            runs_dir: runs_dir.into(),
            backup_root: backup_root.into(),
            retention_keep: 3,
            session_drain_timeout: Duration::from_secs(10),
            ticket_ttl: Duration::from_secs(15 * 60),
            workflows: None,
        }
    }
}

/// Live run events, in order, for one run. Serialized with an `event` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        run_id: RunId,
        record: Box<RunRecord>,
    },
    StepStarted {
        run_id: RunId,
        step: StepId,
        label: String,
    },
    Progress {
        run_id: RunId,
        step: StepId,
        progress: StepProgress,
    },
    Output {
        run_id: RunId,
        step: StepId,
        line: String,
    },
    SwapPhase {
        run_id: RunId,
        phase: SwapPhase,
        renames_completed: u8,
    },
    StepFinished {
        run_id: RunId,
        step: StepId,
        status: StepStatus,
        duration_ms: u64,
        error: Option<AppError>,
    },
    RunFinished {
        run_id: RunId,
        status: RunStatus,
        record: Box<RunRecord>,
    },
}

/// Where a run's events go (e.g. a Tauri `Channel`).
pub type EventSink = Arc<dyn Fn(RunEvent) + Send + Sync>;

/// The user's answer to the confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    /// `PreflightReport.confirmation.fingerprint` — proves the user confirmed this plan.
    pub fingerprint: String,
    /// Remember this confirmation (honoured only for Local targets after a first success).
    #[serde(default)]
    pub remember: bool,
}

struct ActiveRun {
    cancel: RunCancel,
    in_window: Arc<AtomicBool>,
    done: watch::Receiver<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

struct Ticket {
    plan: RunPlan,
    lock: RunLockGuard,
    created: Instant,
    can_remember: bool,
}

pub(crate) struct Inner {
    pub(crate) config: EngineConfig,
    pub(crate) ops: Arc<dyn RefreshOps>,
    pub(crate) real: Option<Arc<RealDeps>>,
    pub(crate) history: HistoryStore,
    pub(crate) locks: RunLocks,
    // Run state below is shared by an engine and the ones [`Engine::rebuilt`] from it, so a
    // settings change never hands out a second, empty lock table or loses a run's cancel handle.
    tickets: Arc<Mutex<HashMap<String, Ticket>>>,
    active: Arc<Mutex<HashMap<RunId, ActiveRun>>>,
    /// Cancellation handles for one-off operations (restore, …), keyed by their record id.
    operation_cancels: Arc<Mutex<HashMap<RunId, RunCancel>>>,
    shutting_down: Arc<AtomicBool>,
}

/// The Refresh engine. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Engine {
    /// The production engine over the app's connection manager, tunnels, script library and
    /// discovered pg tools.
    pub fn new(config: EngineConfig, deps: RealDeps) -> Self {
        let deps = Arc::new(deps);
        let ops: Arc<dyn RefreshOps> = Arc::new(RealOps::new(deps.clone()));
        Self::with_ops(config, ops, Some(deps))
    }

    /// An engine over custom ops (tests: a scripted fake, or real ops wrapped to inject
    /// failures). Without `real`, [`Engine::preflight`] is unavailable and runs start from a
    /// hand-built plan via [`Engine::start_plan`].
    pub fn with_ops(
        config: EngineConfig,
        ops: Arc<dyn RefreshOps>,
        real: Option<Arc<RealDeps>>,
    ) -> Self {
        Self::from_parts(config, ops, real, None)
    }

    /// A production engine over new settings (backup folder, retention, tool folders) that
    /// shares this engine's run state: per-server locks, pending confirmations, active runs and
    /// operation cancels. Work already in flight keeps the engine it began on, and still
    /// excludes — and can be cancelled through — the new one.
    pub fn rebuilt(&self, config: EngineConfig, deps: RealDeps) -> Self {
        let deps = Arc::new(deps);
        let ops: Arc<dyn RefreshOps> = Arc::new(RealOps::new(deps.clone()));
        self.rebuilt_with_ops(config, ops, Some(deps))
    }

    /// [`Engine::rebuilt`] over custom ops (tests), as [`Engine::with_ops`] is to [`Engine::new`].
    pub fn rebuilt_with_ops(
        &self,
        config: EngineConfig,
        ops: Arc<dyn RefreshOps>,
        real: Option<Arc<RealDeps>>,
    ) -> Self {
        Self::from_parts(config, ops, real, Some(&self.inner))
    }

    fn from_parts(
        config: EngineConfig,
        ops: Arc<dyn RefreshOps>,
        real: Option<Arc<RealDeps>>,
        share: Option<&Inner>,
    ) -> Self {
        let history = HistoryStore::new(config.runs_dir.clone());
        Self {
            inner: Arc::new(Inner {
                config,
                ops,
                real,
                history,
                locks: share.map(|s| s.locks.clone()).unwrap_or_default(),
                tickets: share.map(|s| s.tickets.clone()).unwrap_or_default(),
                active: share.map(|s| s.active.clone()).unwrap_or_default(),
                operation_cancels: share
                    .map(|s| s.operation_cancels.clone())
                    .unwrap_or_default(),
                shutting_down: share.map(|s| s.shutting_down.clone()).unwrap_or_default(),
            }),
        }
    }

    pub fn history(&self) -> &HistoryStore {
        &self.inner.history
    }

    pub fn locks(&self) -> &RunLocks {
        &self.inner.locks
    }

    pub fn config(&self) -> &EngineConfig {
        &self.inner.config
    }

    // ------------------------------------------------------------------ preflight / start

    fn real(&self) -> AppResult<Arc<RealDeps>> {
        self.inner
            .real
            .clone()
            .ok_or_else(|| AppError::internal("preflight needs the real engine dependencies"))
    }

    fn prune_tickets(&self) {
        let ttl = self.inner.config.ticket_ttl;
        lock(&self.inner.tickets).retain(|_, t| t.created.elapsed() < ttl);
    }

    fn park(&self, report: &mut PreflightReport, parked: Option<(RunPlan, RunLockGuard, bool)>) {
        if let Some((plan, lock_guard, can_remember)) = parked {
            let id = uuid::Uuid::new_v4().to_string();
            report.ticket = Some(id.clone());
            lock(&self.inner.tickets).insert(
                id,
                Ticket {
                    plan,
                    lock: lock_guard,
                    created: Instant::now(),
                    can_remember,
                },
            );
        }
    }

    /// Run the gate sequence for `workflow` (nothing destructive). On success the report
    /// carries a `ticket` and the `confirmation` to show; the target server stays locked for
    /// this ticket until [`start`](Self::start), [`discard_preflight`](Self::discard_preflight)
    /// or expiry. `caller_warnings` (open transactions, pending grid edits the UI knows of)
    /// are added to the confirmation.
    pub async fn preflight(
        &self,
        workflow: &RefreshWorkflow,
        caller_warnings: Vec<String>,
    ) -> PreflightReport {
        self.prune_tickets();
        // A new preflight of the same workflow replaces its pending ticket (and lock).
        lock(&self.inner.tickets).retain(|_, t| t.plan.workflow.id != workflow.id);
        let deps = match self.real() {
            Ok(d) => d,
            Err(e) => return PreflightReport::refused_early(workflow, e),
        };
        let (mut report, parked) =
            preflight::run(&deps, &self.inner, workflow, caller_warnings, None).await;
        self.park(&mut report, parked);
        report
    }

    /// Prepare a retry of a failed/cancelled/interrupted run from `from` (retry matrix): the
    /// prerequisites (backup + verdict, staging state, live reachability) are validated and the
    /// full gate sequence re-runs (fresh credentials, environment-tag re-check, current script content).
    /// Confirm with [`start`](Self::start) like any run.
    pub async fn retry(
        &self,
        run_id: &RunId,
        from: StepId,
        caller_warnings: Vec<String>,
    ) -> PreflightReport {
        self.prune_tickets();
        let deps = match self.real() {
            Ok(d) => d,
            Err(e) => return PreflightReport::refused_bare(e),
        };
        let rec = match self.retry_record(run_id) {
            Ok(r) => r,
            Err(e) => return PreflightReport::refused_bare(e),
        };
        let workflow = match self.current_workflow(&rec) {
            Ok(w) => w,
            Err(e) => return PreflightReport::refused_bare(e),
        };
        lock(&self.inner.tickets).retain(|_, t| t.plan.workflow.id != workflow.id);
        let inv = scan_inventory(&*self.inner.ops, &rec, None, true).await;
        let offer = match retry::plan_retry(&rec, from, Some(&inv)) {
            Ok(o) => o,
            Err(e) => {
                let mut rep = PreflightReport::refused_early(&workflow, e);
                rep.checks.insert(
                    0,
                    preflight::PreflightCheck::fail(
                        "retry",
                        "Retry prerequisites",
                        rep.refused
                            .as_ref()
                            .map(|e| e.message.clone())
                            .unwrap_or_default(),
                        None,
                    ),
                );
                return rep;
            }
        };
        let ctx = preflight::RetryContext {
            record: rec,
            offer,
            inventory: inv,
        };
        let (mut report, parked) =
            preflight::run(&deps, &self.inner, &workflow, caller_warnings, Some(ctx)).await;
        self.park(&mut report, parked);
        report
    }

    fn retry_record(&self, run_id: &RunId) -> AppResult<RunRecord> {
        if self.is_running(run_id) {
            return Err(AppError::new(
                ErrorKind::Refused,
                "that run is still running",
            ));
        }
        self.inner.history.load(run_id)
    }

    /// The workflow as it is now, refusing when what the run vouched for changed.
    fn current_workflow(&self, rec: &RunRecord) -> AppResult<RefreshWorkflow> {
        let Some(store) = &self.inner.config.workflows else {
            return Err(AppError::internal("retry needs the workflow store"));
        };
        let wf = store.get(&rec.workflow_id)?;
        if wf.source.connection != rec.source.connection_id
            || wf.source.database != rec.source.database
            || wf.target.connection != rec.target.connection_id
            || wf.target.database != rec.target.database
            || wf.options.strategy != rec.strategy
        {
            return Err(AppError::new(
                ErrorKind::Refused,
                "the workflow's source, target or strategy changed since this run; start a new run instead",
            ));
        }
        Ok(wf)
    }

    /// Release a pending preflight ticket (the user closed the confirmation).
    pub fn discard_preflight(&self, ticket: &str) {
        lock(&self.inner.tickets).remove(ticket);
    }

    /// Start a confirmed run. Errors when the ticket expired or the fingerprint doesn't match
    /// the preflighted plan.
    pub async fn start(
        &self,
        ticket: &str,
        request: StartRequest,
        sink: EventSink,
    ) -> AppResult<RunId> {
        let t = lock(&self.inner.tickets).remove(ticket).ok_or_else(|| {
            AppError::new(ErrorKind::Refused, "this confirmation has expired")
                .with_hint("run the workflow again")
        })?;
        if t.created.elapsed() >= self.inner.config.ticket_ttl {
            return Err(
                AppError::new(ErrorKind::Refused, "this confirmation has expired")
                    .with_hint("run the workflow again"),
            );
        }
        if request.fingerprint != t.plan.fingerprint {
            return Err(AppError::new(
                ErrorKind::Refused,
                "the confirmation does not match the checked plan; run the workflow again",
            ));
        }
        if request.remember && t.can_remember {
            if let Some(store) = &self.inner.config.workflows {
                if let Ok(mut wf) = store.get(&t.plan.workflow.id) {
                    wf.remembered_confirmation = Some(crate::definition::RememberedConfirmation {
                        fingerprint: t.plan.fingerprint.clone(),
                        remembered_at: now_rfc3339(),
                    });
                    if let Err(e) = store.save(&wf) {
                        tracing::warn!(error = %e, "could not save the remembered confirmation");
                    }
                }
            }
        }
        self.spawn(t.plan, t.lock, sink)
    }

    /// Start a run from an already-resolved plan, taking the run lock (tests / fakes).
    pub fn start_plan(&self, plan: RunPlan, sink: EventSink) -> AppResult<RunId> {
        let guard = self.inner.locks.try_acquire(
            &plan.target.server_key(),
            format!("the Refresh \"{}\"", plan.workflow.name),
        )?;
        self.spawn(plan, guard, sink)
    }

    fn spawn(&self, plan: RunPlan, guard: RunLockGuard, sink: EventSink) -> AppResult<RunId> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(AppError::new(ErrorKind::Refused, "pgWhy is quitting"));
        }
        let run_id = plan.run_id.clone();
        let cancel = RunCancel::new();
        let in_window = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = watch::channel(false);
        let mut runner = Runner::new(
            self.inner.clone(),
            plan,
            sink,
            cancel.clone(),
            in_window.clone(),
        );
        runner.save_quiet();
        let inner = self.inner.clone();
        let id = run_id.clone();
        // Hold the active-map lock across spawn so the task can't finish (and remove itself)
        // before it is registered.
        let mut active = lock(&self.inner.active);
        let task = tokio::spawn(async move {
            let _guard = guard;
            runner.execute().await;
            drop(_guard);
            lock(&inner.active).remove(&id);
            let _ = done_tx.send(true);
        });
        active.insert(
            run_id.clone(),
            ActiveRun {
                cancel,
                in_window,
                done: done_rx,
                task: Some(task),
            },
        );
        Ok(run_id)
    }

    // ------------------------------------------------------------------ live runs

    pub fn is_running(&self, run_id: &RunId) -> bool {
        lock(&self.inner.active).contains_key(run_id)
    }

    pub fn active_runs(&self) -> Vec<RunId> {
        lock(&self.inner.active).keys().cloned().collect()
    }

    /// Cancel a run: kills the active pg_dump/pg_restore or cancels the active statement.
    /// Refused between the swap's two renames.
    pub fn cancel(&self, run_id: &RunId) -> AppResult<()> {
        let active = lock(&self.inner.active);
        let run = active
            .get(run_id)
            .ok_or_else(|| AppError::new(ErrorKind::Refused, "that run is not running"))?;
        if run.in_window.load(Ordering::SeqCst) {
            return Err(AppError::new(
                ErrorKind::Refused,
                "the swap is between its two renames; stopping now would leave no database under the target name — it finishes in a moment",
            ));
        }
        run.cancel.cancel();
        Ok(())
    }

    /// Wait for a run to finish and return its record.
    pub async fn wait(&self, run_id: &RunId) -> AppResult<RunRecord> {
        let rx = lock(&self.inner.active).get(run_id).map(|r| r.done.clone());
        if let Some(mut rx) = rx {
            let _ = rx.wait_for(|d| *d).await;
        }
        self.inner.history.load(run_id)
    }

    /// App-quit hook: cancel every run (except one inside the rename window, which gets the
    /// grace period to finish), wait up to `grace`, then stop whatever is left. Every stopped
    /// run is recorded `Interrupted` with `renames_completed` as persisted.
    pub async fn shutdown(&self, grace: Duration) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        lock(&self.inner.tickets).clear();
        let runs: Vec<(RunId, watch::Receiver<bool>)> = {
            let active = lock(&self.inner.active);
            active
                .iter()
                .map(|(id, r)| {
                    if !r.in_window.load(Ordering::SeqCst) {
                        r.cancel.cancel();
                    }
                    (id.clone(), r.done.clone())
                })
                .collect()
        };
        let deadline = tokio::time::Instant::now() + grace;
        for (_, mut rx) in runs.clone() {
            let _ = tokio::time::timeout_at(deadline, rx.wait_for(|d| *d)).await;
        }
        for (id, _) in runs {
            let task = lock(&self.inner.active)
                .get_mut(&id)
                .and_then(|r| r.task.take());
            let Some(task) = task else { continue };
            task.abort();
            let _ = task.await;
            lock(&self.inner.active).remove(&id);
            if let Ok(mut rec) = self.inner.history.load(&id) {
                if !rec.status.is_terminal() {
                    mark_interrupted(&mut rec, "pgWhy quit while the run was in progress");
                    let _ = self.inner.history.save(&rec);
                }
            }
        }
    }

    /// Startup hook: records still `running` (a crash or kill) become `Interrupted`. Their
    /// actions are recorded-state-only until [`reinventory`](Self::reinventory) scans live.
    pub fn mark_stale_interrupted(&self) -> AppResult<Vec<RunId>> {
        let mut out = Vec::new();
        for mut rec in self.inner.history.list(None)? {
            if rec.status == RunStatus::Running && !self.is_running(&rec.run_id) {
                mark_interrupted(
                    &mut rec,
                    "pgWhy stopped unexpectedly while the run was in progress",
                );
                self.inner.history.save(&rec)?;
                out.push(rec.run_id);
            }
        }
        for mut rec in self.inner.history.list_operations()? {
            if rec.status == RunStatus::Running && !self.is_running(&rec.id) {
                rec.mark_interrupted(
                    "pgWhy stopped unexpectedly while the operation was in progress",
                );
                self.inner.history.save_operation(&rec)?;
                out.push(rec.id);
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ records & actions

    pub fn get_run(&self, run_id: &RunId) -> AppResult<RunRecord> {
        self.inner.history.load(run_id)
    }

    pub fn list_runs(
        &self,
        workflow: Option<&crate::definition::WorkflowId>,
    ) -> AppResult<Vec<RunRecord>> {
        self.inner.history.list(workflow)
    }

    /// The target server's maintenance-database handle, for the lifecycle verbs.
    pub(crate) async fn maintenance(
        &self,
        connection_id: &pgwhy_core::config::ConnectionId,
    ) -> AppResult<Box<dyn crate::ops::MaintenanceOps>> {
        self.inner.ops.maintenance(connection_id).await
    }

    /// The ops layer (tool restore/verify/maintenance, quiesce) for verbs outside the Refresh
    /// state machine.
    pub(crate) fn refresh_ops(&self) -> Arc<dyn crate::ops::RefreshOps> {
        self.inner.ops.clone()
    }

    /// Register an operation's cancel handle so the UI can cancel it by record id.
    pub(crate) fn register_operation_cancel(&self, id: &RunId, cancel: crate::ops::RunCancel) {
        lock(&self.inner.operation_cancels).insert(id.clone(), cancel);
    }

    pub(crate) fn unregister_operation_cancel(&self, id: &RunId) {
        lock(&self.inner.operation_cancels).remove(id);
    }

    /// Cancel a running one-off operation. Returns whether one was running under `id`.
    pub fn cancel_operation(&self, id: &RunId) -> bool {
        match lock(&self.inner.operation_cancels).get(id) {
            Some(c) => {
                c.cancel();
                true
            }
            None => false,
        }
    }

    /// Refresh runs and one-off operations in one timeline, newest first.
    pub fn list_history(&self) -> AppResult<Vec<HistoryEntry>> {
        self.inner.history.list_entries()
    }

    /// One operation record, by id.
    pub fn get_operation(&self, id: &RunId) -> AppResult<OperationRecord> {
        self.inner.history.load_operation(id)
    }

    /// Persist an operation record (callers rewrite it at every step boundary, exactly like a run
    /// record, so a crash leaves the last truth on disk).
    pub fn save_operation(&self, rec: &OperationRecord) -> AppResult<()> {
        self.inner.history.save_operation(rec)
    }

    /// Every operation record on disk, newest first.
    pub fn list_operations(&self) -> AppResult<Vec<OperationRecord>> {
        self.inner.history.list_operations()
    }

    /// Fresh live inventory of the target server (and a source reachability probe) for a
    /// finished run; updates and persists its inventory, headline and action set.
    pub async fn reinventory(&self, run_id: &RunId) -> AppResult<RunRecord> {
        let mut rec = self.retry_record(run_id)?;
        let inv = scan_inventory(&*self.inner.ops, &rec, None, rec.status.is_failure()).await;
        // The scan is slow and runs unlocked, so `perform` may have saved this record meanwhile.
        // Saving our stale copy would clobber that. Take the server lock, re-load the record and
        // apply the inventory to the fresh copy. If the lock is busy, someone is working on this
        // server: hand back the refreshed view without writing it.
        match self.inner.locks.try_acquire(
            &rec.target.server_key,
            format!("inventory of run {}", rec.run_id),
        ) {
            Ok(_guard) => {
                let mut fresh = self.retry_record(run_id)?;
                apply_inventory(&mut fresh, inv);
                self.inner.history.save(&fresh)?;
                Ok(fresh)
            }
            Err(_) => {
                apply_inventory(&mut rec, inv);
                Ok(rec)
            }
        }
    }

    /// Perform a cleanup/recovery action on a finished run (drop/keep staging, drop `_old`,
    /// roll back or complete an interrupted swap). The action must be in the record's live
    /// action set; it runs under the target server's run lock.
    pub async fn perform(&self, run_id: &RunId, action: ActionKind) -> AppResult<RunRecord> {
        if action == ActionKind::Retry {
            return Err(AppError::new(ErrorKind::Refused, "use retry for retries"));
        }
        let mut rec = self.retry_record(run_id)?;
        let _guard = self.inner.locks.try_acquire(
            &rec.target.server_key,
            format!("cleanup of run {}", rec.run_id),
        )?;
        let maint: Arc<dyn MaintenanceOps> = Arc::from(
            self.inner
                .ops
                .maintenance(&rec.target.connection_id)
                .await?,
        );
        let inv = scan_inventory(&*self.inner.ops, &rec, Some(&*maint), false).await;
        let offers = retry::derive_actions(&rec, Some(&inv));
        let offer = offers
            .iter()
            .find(|a| a.kind == action)
            .cloned()
            .ok_or_else(|| {
                AppError::new(
                    ErrorKind::Refused,
                    "that action is not available for this run in its current state",
                )
            })?;
        if !offer.enabled {
            return Err(AppError::new(
                ErrorKind::Refused,
                offer
                    .disabled_reason
                    .unwrap_or_else(|| "that action is not available right now".into()),
            ));
        }
        let timeout = self.inner.config.session_drain_timeout;
        let ops = self.inner.ops.clone();
        let result = match action {
            ActionKind::KeepStaging => {
                rec.staging_kept = true;
                Ok(())
            }
            ActionKind::DropStaging => {
                let db = offer.database.clone().unwrap_or_default();
                crate::swap::drop_marked(&*maint, &db, &rec.run_id, None, timeout)
                    .await
                    .map(|_| {
                        rec.work_db_state = WorkDbState::Dropped;
                    })
            }
            ActionKind::DropOld => {
                let db = offer.database.clone().unwrap_or_default();
                crate::swap::drop_marked(&*maint, &db, &rec.run_id, Some(MarkerRole::Old), timeout)
                    .await
                    .map(|_| {
                        if rec.status == RunStatus::SucceededCleanupRequired {
                            rec.status = RunStatus::Succeeded;
                        }
                    })
            }
            ActionKind::RollbackSwap => {
                crate::swap::rollback(&*ops, &*maint, &mut rec, timeout).await
            }
            ActionKind::CompleteSwap => {
                let r = crate::swap::complete(&*ops, &*maint, &mut rec, timeout).await;
                if r.is_ok() {
                    self.mark_workflow_succeeded(&rec);
                }
                r
            }
            ActionKind::Retry => unreachable!(),
        };
        if let Err(e) = &result {
            rec.warnings
                .push(format!("{} failed: {}", offer.label, e.message));
        }
        let inv = scan_inventory(
            &*self.inner.ops,
            &rec,
            Some(&*maint),
            rec.status.is_failure(),
        )
        .await;
        apply_inventory(&mut rec, inv);
        self.inner.history.save(&rec)?;
        result.map(|_| rec)
    }

    /// Fix a preflight finding: drop a marked leftover (marker-verified) or `ROLLBACK PREPARED`
    /// a named prepared transaction on the target. Re-run preflight afterwards.
    pub async fn apply_preflight_fix(
        &self,
        workflow: &RefreshWorkflow,
        fix: &preflight::PreflightFix,
    ) -> AppResult<()> {
        let deps = self.real()?;
        let target = deps.manager.entry(&workflow.target.connection)?;
        let _guard = self.inner.locks.try_acquire(
            &target.server_key(),
            format!("a fix for the Refresh \"{}\"", workflow.name),
        )?;
        preflight::apply_fix(
            &deps,
            &*self.inner.ops,
            workflow,
            fix,
            self.inner.config.session_drain_timeout,
        )
        .await
    }

    pub(crate) fn mark_workflow_succeeded(&self, rec: &RunRecord) {
        mark_workflow_succeeded(&self.inner, rec);
    }
}

pub(crate) fn mark_workflow_succeeded(inner: &Inner, rec: &RunRecord) {
    let Some(store) = &inner.config.workflows else {
        return;
    };
    match store.get(&rec.workflow_id) {
        Ok(mut wf) if !wf.has_succeeded => {
            wf.has_succeeded = true;
            if let Err(e) = store.save(&wf) {
                tracing::warn!(error = %e, "could not mark the workflow as having succeeded");
            }
        }
        _ => {}
    }
}

fn mark_interrupted(rec: &mut RunRecord, why: &str) {
    rec.status = RunStatus::Interrupted;
    rec.finished_at = Some(now_rfc3339());
    for s in rec.steps.iter_mut() {
        if s.status == StepStatus::Running {
            s.status = StepStatus::Interrupted;
            s.finished_at = rec.finished_at.clone();
            rec.failed_step = Some(s.step);
        }
    }
    rec.error = Some(AppError::new(ErrorKind::Cancelled, why).with_class(ErrorClass::Retriable));
    rec.inventory = None;
    rec.headline = retry::headline(rec, None);
    rec.actions = retry::derive_actions(rec, None);
}

pub(crate) fn apply_inventory(rec: &mut RunRecord, inv: Inventory) {
    rec.headline = retry::headline(rec, Some(&inv));
    rec.actions = retry::derive_actions(rec, Some(&inv));
    rec.inventory = Some(inv);
}

fn presence(dbs: &[DbFact], name: &str, run: &RunId, sessions: &[SessionInfo]) -> DbPresence {
    let fact = dbs.iter().find(|d| d.name == name);
    let marker = fact
        .and_then(|f| f.comment.as_deref())
        .and_then(Marker::parse);
    DbPresence {
        name: name.to_string(),
        exists: fact.is_some(),
        owned_by_run: marker.as_ref().is_some_and(|m| m.run_id == run.0),
        marker,
        sessions: sessions.iter().filter(|s| s.database == name).count() as u32,
    }
}

/// Live re-inventory of the target server for `rec` (plus an optional source probe).
pub(crate) async fn scan_inventory(
    ops: &dyn RefreshOps,
    rec: &RunRecord,
    maint: Option<&dyn MaintenanceOps>,
    probe_source: bool,
) -> Inventory {
    let owned;
    let maint: AppResult<&dyn MaintenanceOps> = match maint {
        Some(m) if !m.is_closed() => Ok(m),
        _ => match ops.maintenance(&rec.target.connection_id).await {
            Ok(m) => {
                owned = m;
                Ok(&*owned)
            }
            Err(e) => Err(e),
        },
    };
    let target_name = rec.target.database.clone();
    let mut inv = Inventory {
        scanned_at: now_rfc3339(),
        reachable: false,
        error: None,
        target: DbPresence {
            name: target_name.clone(),
            exists: false,
            marker: None,
            owned_by_run: false,
            sessions: 0,
        },
        staging: None,
        old: None,
        sessions: Vec::new(),
        source_reachable: None,
        source_error: None,
    };
    match maint {
        Err(e) => inv.error = Some(e),
        Ok(m) => match m.databases().await {
            Err(e) => inv.error = Some(e),
            Ok(dbs) => {
                let mut names = vec![target_name.clone()];
                names.extend(rec.staging_db.clone());
                names.extend(rec.old_db.clone());
                let sessions = m.sessions(&names).await.unwrap_or_default();
                inv.reachable = true;
                inv.target = presence(&dbs, &target_name, &rec.run_id, &sessions);
                if rec.strategy == ReplaceStrategy::StagingSwap {
                    inv.staging = rec
                        .staging_db
                        .as_deref()
                        .map(|s| presence(&dbs, s, &rec.run_id, &sessions));
                    inv.old = rec
                        .old_db
                        .as_deref()
                        .map(|o| presence(&dbs, o, &rec.run_id, &sessions));
                }
                inv.sessions = sessions;
            }
        },
    }
    if probe_source {
        match ops
            .probe(&rec.source.connection_id, &rec.source.database)
            .await
        {
            Ok(()) => inv.source_reachable = Some(true),
            Err(e) => {
                inv.source_reachable = Some(false);
                inv.source_error = Some(e);
            }
        }
    }
    inv
}

fn new_record(plan: &RunPlan) -> RunRecord {
    let strategy = plan.workflow.options.strategy;
    let mut source = EndpointRecord::of(&plan.source, plan.source_db());
    source.server_version = plan.source_version.clone();
    let mut target = EndpointRecord::of(&plan.target, plan.target_db());
    target.server_version = plan.target_version.clone();
    let resume = plan.resume.as_ref();
    RunRecord {
        schema_version: RECORD_SCHEMA_VERSION,
        run_id: plan.run_id.clone(),
        workflow_id: plan.workflow.id.clone(),
        workflow_name: plan.workflow.name.clone(),
        retry_of: resume.map(|r| r.retry_of.clone()),
        started_at: now_rfc3339(),
        finished_at: None,
        status: RunStatus::Running,
        strategy,
        create_mode: plan.create_mode,
        source,
        target,
        staging_db: (strategy == ReplaceStrategy::StagingSwap).then(|| plan.staging_db.clone()),
        old_db: (strategy == ReplaceStrategy::StagingSwap && !plan.create_mode)
            .then(|| plan.old_db.clone()),
        work_db_state: match resume {
            Some(r) if r.adopt_work_db => r.work_db_state,
            _ => WorkDbState::Absent,
        },
        staging_kept: false,
        tools: ToolsRecord {
            pg_dump: plan.tools.as_ref().map(|t| ToolRecord {
                path: t.pg_dump.path.clone(),
                version: t.pg_dump.version.raw.clone(),
            }),
            pg_restore: plan.tools.as_ref().map(|t| ToolRecord {
                path: t.pg_restore.path.clone(),
                version: t.pg_restore.version.raw.clone(),
            }),
        },
        options: plan.workflow.options.clone(),
        steps: plan
            .steps()
            .into_iter()
            .map(|s| StepRecord::pending(s, strategy))
            .collect(),
        backup: None,
        script: plan.script.clone(),
        locale: plan.locale.clone(),
        renames_completed: 0,
        swap_phase: None,
        failed_step: None,
        failed_items: Vec::new(),
        error: None,
        headline: None,
        inventory: None,
        actions: Vec::new(),
        credentials: plan.credentials,
        warnings: plan.warnings.clone(),
        confirmation_fingerprint: plan.fingerprint.clone(),
        confirmation_remembered: plan.remembered,
    }
}

const STEP_TAIL: usize = 60;

/// One run in flight. Owned by its task; the swap sub-steps live in [`crate::swap`].
pub(crate) struct Runner {
    pub(crate) inner: Arc<Inner>,
    pub(crate) plan: RunPlan,
    pub(crate) rec: RunRecord,
    pub(crate) sink: EventSink,
    pub(crate) cancel: RunCancel,
    pub(crate) in_window: Arc<AtomicBool>,
    maint: Option<Arc<dyn MaintenanceOps>>,
    step_started: Option<Instant>,
    step_lines: Arc<Mutex<VecDeque<String>>>,
    /// Set when the swap finished but dropping `_old` failed.
    pub(crate) cleanup_error: Option<AppError>,
}

impl Runner {
    fn new(
        inner: Arc<Inner>,
        plan: RunPlan,
        sink: EventSink,
        cancel: RunCancel,
        in_window: Arc<AtomicBool>,
    ) -> Self {
        let rec = new_record(&plan);
        Self {
            inner,
            plan,
            rec,
            sink,
            cancel,
            in_window,
            maint: None,
            step_started: None,
            step_lines: Arc::default(),
            cleanup_error: None,
        }
    }

    pub(crate) fn emit(&self, ev: RunEvent) {
        (self.sink)(ev)
    }

    pub(crate) fn save(&self) -> AppResult<()> {
        self.inner.history.save(&self.rec)
    }

    pub(crate) fn save_quiet(&self) {
        if let Err(e) = self.save() {
            tracing::warn!(error = %e, run = %self.rec.run_id, "could not write the run record");
        }
    }

    pub(crate) fn run_id(&self) -> RunId {
        self.rec.run_id.clone()
    }

    pub(crate) async fn maint(&mut self) -> AppResult<Arc<dyn MaintenanceOps>> {
        if let Some(m) = self.maint.as_ref().filter(|m| !m.is_closed()) {
            return Ok(m.clone());
        }
        let m: Arc<dyn MaintenanceOps> =
            Arc::from(self.inner.ops.maintenance(&self.plan.target.id).await?);
        self.maint = Some(m.clone());
        Ok(m)
    }

    pub(crate) fn step_ctx(&self, step: StepId) -> OpCtx {
        let sink = self.sink.clone();
        let run_id = self.run_id();
        let lines = self.step_lines.clone();
        OpCtx::new(
            self.cancel.clone(),
            Arc::new(move |ev| match ev {
                OpEvent::Progress(progress) => sink(RunEvent::Progress {
                    run_id: run_id.clone(),
                    step,
                    progress,
                }),
                OpEvent::Output(line) => {
                    {
                        let mut l = lock(&lines);
                        l.push_back(line.clone());
                        while l.len() > STEP_TAIL {
                            l.pop_front();
                        }
                    }
                    sink(RunEvent::Output {
                        run_id: run_id.clone(),
                        step,
                        line,
                    });
                }
            }),
        )
    }

    fn begin_step(&mut self, step: StepId) {
        lock(&self.step_lines).clear();
        self.step_started = Some(Instant::now());
        let label = step.label(self.rec.strategy).to_string();
        if let Some(s) = self.rec.step_mut(step) {
            s.status = StepStatus::Running;
            s.started_at = Some(now_rfc3339());
        }
        self.save_quiet();
        self.emit(RunEvent::StepStarted {
            run_id: self.run_id(),
            step,
            label,
        });
    }

    fn end_step(&mut self, step: StepId, status: StepStatus, error: Option<AppError>) {
        let duration_ms = self
            .step_started
            .take()
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let lines: Vec<String> = lock(&self.step_lines).iter().cloned().collect();
        if let Some(s) = self.rec.step_mut(step) {
            s.status = status;
            s.finished_at = Some(now_rfc3339());
            s.duration_ms = Some(duration_ms);
            if s.output_tail.is_empty() {
                s.output_tail = lines;
            }
            s.error = error.clone();
        }
        self.save_quiet();
        self.emit(RunEvent::StepFinished {
            run_id: self.run_id(),
            step,
            status,
            duration_ms,
            error,
        });
    }

    fn mark_step(&mut self, step: StepId, status: StepStatus, note: Option<String>) {
        if let Some(s) = self.rec.step_mut(step) {
            s.status = status;
            s.note = note;
        }
    }

    pub(crate) fn check_cancel(&self) -> AppResult<()> {
        if self.cancel.is_cancelled() {
            return Err(AppError::new(ErrorKind::Cancelled, "the run was cancelled"));
        }
        Ok(())
    }

    /// The whole state machine.
    pub(crate) async fn execute(&mut self) {
        let steps = self.plan.steps();
        let begins_at = self
            .plan
            .resume
            .as_ref()
            .map(|r| r.begins_at)
            .unwrap_or(StepId::Dump);
        let begin_idx = steps.iter().position(|s| *s == begins_at).unwrap_or(0);
        self.emit(RunEvent::RunStarted {
            run_id: self.run_id(),
            record: Box::new(self.rec.clone()),
        });

        for (i, step) in steps.iter().copied().enumerate() {
            if i < begin_idx {
                self.reuse_step(step);
                continue;
            }
            if self.skip(step) {
                self.mark_step(step, StepStatus::Skipped, None);
                self.save_quiet();
                continue;
            }
            if let Err(e) = self.check_cancel() {
                self.begin_step(step);
                return self.fail(step, e).await;
            }
            self.begin_step(step);
            let res = match step {
                StepId::Dump => self.dump().await,
                StepId::DropTarget => crate::swap::drop_target(self).await,
                StepId::CreateDatabase => self.create_database().await,
                StepId::Restore => self.restore().await,
                StepId::PostRestore => self.post_restore().await,
                StepId::Verify => self.verify().await,
                StepId::Swap => self.swap().await,
            };
            match res {
                Ok(()) => self.end_step(step, StepStatus::Succeeded, None),
                Err(e) => return self.fail(step, e).await,
            }
        }
        self.succeed().await;
    }

    fn skip(&self, step: StepId) -> bool {
        match step {
            StepId::PostRestore => self.plan.script.is_none(),
            StepId::DropTarget => self.plan.create_mode,
            _ => false,
        }
    }

    fn reuse_step(&mut self, step: StepId) {
        let Some(resume) = self.plan.resume.clone() else {
            return;
        };
        let from = format!("from run {}", resume.retry_of);
        match step {
            StepId::Dump => {
                if let Some(mut b) = resume.backup.clone() {
                    b.from_run.get_or_insert(resume.retry_of.clone());
                    self.rec.backup = Some(b);
                }
                self.mark_step(
                    step,
                    StepStatus::Reused,
                    Some(format!("backup reused {from}")),
                );
            }
            StepId::DropTarget => self.mark_step(
                step,
                StepStatus::Reused,
                Some(format!("target already dropped {from}")),
            ),
            StepId::PostRestore if self.plan.script.is_none() => {
                self.mark_step(step, StepStatus::Skipped, None)
            }
            _ => self.mark_step(
                step,
                StepStatus::Reused,
                Some(format!("{} kept {from}", self.plan.work_db())),
            ),
        }
        self.save_quiet();
    }

    // ------------------------------------------------------------------ steps

    async fn dump(&mut self) -> AppResult<()> {
        let ts = staging::compact_timestamp(SystemTime::now());
        let file = format!("{}_{}.dump", sanitize(self.plan.source_db()), ts);
        let out = self.plan.backup_dir.join(file);
        let log_dir = self.inner.history.log_dir(&self.rec.run_id);
        std::fs::create_dir_all(&log_dir).map_err(|e| {
            AppError::new(ErrorKind::Disk, "could not create the run's log folder")
                .with_detail(format!("{}: {e}", log_dir.display()))
        })?;
        let log = log_dir.join("dump.log");
        let ctx = self.step_ctx(StepId::Dump);
        let ops = self.inner.ops.clone();
        let res = ops.dump(&self.plan, &out, &log, &ctx).await;
        let done = match res {
            Ok(d) => d,
            Err(e) => {
                self.record_tool_error(StepId::Dump, &e);
                return Err(e);
            }
        };
        if let Some(s) = self.rec.step_mut(StepId::Dump) {
            s.exit_code = done.tool.exit_code;
            s.output_tail = done.tool.output_tail.clone();
            s.log_path = done.tool.log_path.clone();
            s.objects_done = Some(done.tool.objects_done);
        }
        let verdict = ops.inspect_backup(&self.plan, &done.path, &ctx).await?;
        self.rec.backup = Some(BackupRecord {
            path: done.path.clone(),
            size_bytes: done.size_bytes,
            dumped_by: verdict.dumped_by.clone().or(Some(done.dumped_by.clone())),
            reusable: verdict.reusable,
            reason: verdict.reason.clone(),
            cause: verdict.cause,
            toc_items: verdict.toc_items,
            from_run: None,
        });
        if !verdict.reusable {
            return Err(AppError::new(
                ErrorKind::ToolFailed,
                format!(
                    "the new backup failed validation: {}",
                    verdict.reason.unwrap_or_default()
                ),
            )
            .with_hint("retry to take a new backup"));
        }
        Ok(())
    }

    fn record_tool_error(&mut self, step: StepId, e: &AppError) {
        let Some(data) = e.data.as_ref() else { return };
        let tail: Vec<String> = data
            .get("stderr_tail")
            .or_else(|| data.get("output_tail"))
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default();
        let exit = data
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map(|c| c as i32);
        let log = data
            .get("log_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        if let Some(s) = self.rec.step_mut(step) {
            if !tail.is_empty() {
                s.output_tail = tail[tail.len().saturating_sub(STEP_TAIL)..].to_vec();
            }
            s.exit_code = exit.or(s.exit_code);
            s.log_path = log.or(s.log_path.clone());
        }
        if let Some(items) = data.get("failed_items") {
            if let Ok(items) = serde_json::from_value(items.clone()) {
                self.rec.failed_items = items;
            }
        }
    }

    async fn create_database(&mut self) -> AppResult<()> {
        let maint = self.maint().await?;
        let run_id = self.run_id();
        let wf_id = self.plan.workflow.id.0.clone();
        let target = self.plan.target_db().to_string();
        let name = self.plan.work_db().to_string();
        let timeout = self.inner.config.session_drain_timeout;
        if let Some(resume) = self.plan.resume.clone() {
            for db in &resume.drop_before_create {
                crate::swap::drop_marked(&*maint, db, &resume.retry_of, None, timeout).await?;
                self.step_ctx(StepId::CreateDatabase)
                    .output(format!("dropped {db} left by run {}", resume.retry_of));
            }
        }
        let dbs = maint.databases().await?;
        if dbs.iter().any(|d| d.name == name) {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("a database named {name} already exists on the target server"),
            )
            .with_hint(
                "it may be a real database; drop or rename it yourself if it is not needed",
            ));
        }
        maint
            .create_database(&name, self.plan.locale.as_ref())
            .await?;
        self.rec.work_db_state = WorkDbState::Created;
        let role = match self.rec.strategy {
            ReplaceStrategy::StagingSwap => MarkerRole::Staging,
            ReplaceStrategy::DropFirst => MarkerRole::WorkTarget,
        };
        let marker = Marker::new(role, &run_id.0, &wf_id, &target);
        if let Err(e) = maint.set_comment(&name, Some(&marker.to_comment())).await {
            // Created moments ago by this step: dropping it is safe even without the marker.
            let _ = maint.drop_database(&name).await;
            self.rec.work_db_state = WorkDbState::Absent;
            return Err(e);
        }
        self.step_ctx(StepId::CreateDatabase)
            .output(format!("created {name}"));
        Ok(())
    }

    async fn restore(&mut self) -> AppResult<()> {
        let backup = self
            .rec
            .backup
            .clone()
            .ok_or_else(|| AppError::internal("no backup to restore"))?;
        let db = self.plan.work_db().to_string();
        let log = self
            .inner
            .history
            .log_dir(&self.rec.run_id)
            .join("restore.log");
        if let Some(parent) = log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.rec.work_db_state = WorkDbState::Partial;
        self.save_quiet();
        let ctx = self.step_ctx(StepId::Restore);
        let ops = self.inner.ops.clone();
        match ops
            .restore(&self.plan, &backup.path, &db, backup.toc_items, &log, &ctx)
            .await
        {
            Ok(done) => {
                if let Some(s) = self.rec.step_mut(StepId::Restore) {
                    s.exit_code = done.exit_code;
                    s.output_tail = done.output_tail;
                    s.log_path = done.log_path;
                    s.objects_done = Some(done.objects_done);
                }
                self.rec.work_db_state = WorkDbState::Restored;
                Ok(())
            }
            Err(e) => {
                self.record_tool_error(StepId::Restore, &e);
                if FailureCause::from_error(&e) == Some(FailureCause::ArchiveCorrupt) {
                    if let Some(b) = self.rec.backup.as_mut() {
                        b.reusable = false;
                        b.reason = Some(e.message.clone());
                        b.cause = Some(FailureCause::ArchiveCorrupt);
                    }
                }
                Err(e)
            }
        }
    }

    async fn post_restore(&mut self) -> AppResult<()> {
        let Some(script) = self.plan.script.clone() else {
            return Ok(());
        };
        self.rec.script = Some(script.clone());
        let db = self.plan.work_db().to_string();
        let ctx = self.step_ctx(StepId::PostRestore);
        let ops = self.inner.ops.clone();
        match ops.run_script(&self.plan, &db, &script, &ctx).await {
            Ok(done) => {
                if let Some(s) = self.rec.step_mut(StepId::PostRestore) {
                    s.objects_done = Some(done.statements as u64);
                    s.output_tail = done.output_tail;
                }
                self.rec.work_db_state = WorkDbState::ScriptApplied;
                Ok(())
            }
            Err(e) => {
                self.record_tool_error(StepId::PostRestore, &e);
                self.rec.work_db_state = match script.mode {
                    // Rolled back: the restore underneath is still clean.
                    ScriptMode::Transaction => WorkDbState::Restored,
                    ScriptMode::NoTransaction => WorkDbState::Mutated,
                };
                Err(e)
            }
        }
    }

    async fn verify(&mut self) -> AppResult<()> {
        let db = self.plan.work_db().to_string();
        let ctx = self.step_ctx(StepId::Verify);
        let ops = self.inner.ops.clone();
        let note = ops.verify(&self.plan, &db, &ctx).await?;
        ctx.output(note.clone());
        if let Some(s) = self.rec.step_mut(StepId::Verify) {
            s.note = Some(note);
        }
        self.rec.work_db_state = WorkDbState::Verified;
        if self.rec.strategy == ReplaceStrategy::DropFirst {
            // The restored target is now the user's database: drop pgWhy's marker.
            let maint = self.maint().await?;
            if let Err(e) = maint.set_comment(&db, None).await {
                self.rec.warnings.push(format!(
                    "could not clear pgWhy's marker comment on {db}: {}",
                    e.message
                ));
            }
            self.rec.work_db_state = WorkDbState::SwappedIn;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ endings

    async fn fail(&mut self, step: StepId, err: AppError) {
        let quitting = self.inner.shutting_down.load(Ordering::SeqCst);
        let cancelled = err.kind == ErrorKind::Cancelled || self.cancel.is_cancelled();
        let (step_status, status) = if cancelled && quitting {
            (StepStatus::Interrupted, RunStatus::Interrupted)
        } else if cancelled {
            (StepStatus::Cancelled, RunStatus::Cancelled)
        } else if step == StepId::Swap {
            (StepStatus::Failed, RunStatus::FailedAtSwap)
        } else {
            (StepStatus::Failed, RunStatus::FailedAtStep)
        };
        self.in_window.store(false, Ordering::SeqCst);
        self.end_step(step, step_status, Some(err.clone()));
        self.rec.failed_step = Some(step);
        self.rec.error = Some(err);
        self.finish(status).await;
    }

    async fn succeed(&mut self) {
        let status = if self.cleanup_error.is_some() {
            RunStatus::SucceededCleanupRequired
        } else {
            RunStatus::Succeeded
        };
        if let Some(e) = self.cleanup_error.clone() {
            self.rec.warnings.push(format!(
                "the swap succeeded but {} could not be dropped: {}",
                self.plan.old_db, e.message
            ));
        }
        self.finish(status).await;
        if status.is_success() {
            mark_workflow_succeeded(&self.inner, &self.rec);
        }
    }

    async fn finish(&mut self, status: RunStatus) {
        self.rec.status = status;
        self.rec.finished_at = Some(now_rfc3339());
        let maint = self.maint().await.ok();
        let inv = scan_inventory(
            &*self.inner.ops,
            &self.rec,
            maint.as_deref(),
            status.is_failure(),
        )
        .await;
        apply_inventory(&mut self.rec, inv);
        self.save_quiet();
        self.apply_retention();
        self.save_quiet();
        // Release server connections before announcing the end (the UI reconnects on it).
        self.maint = None;
        self.emit(RunEvent::RunFinished {
            run_id: self.run_id(),
            status,
            record: Box::new(self.rec.clone()),
        });
    }

    fn apply_retention(&mut self) {
        let records = self
            .inner
            .history
            .list(Some(&self.rec.workflow_id))
            .unwrap_or_default();
        let mut protected = retry::protected_backups(&records);
        if let Some(b) = &self.rec.backup {
            if self.rec.status.is_failure() {
                protected.insert(b.path.clone());
            }
        }
        let policy = pgwhy_pgtools::RetentionPolicy {
            keep_last: self.inner.config.retention_keep,
            suffixes: vec![".dump".into()],
        };
        if let Err(e) = pgwhy_pgtools::apply_retention(&self.plan.backup_dir, &policy, &protected) {
            self.rec
                .warnings
                .push(format!("backup retention: {}", e.message));
        }
    }
}

/// File-name-safe rendering of a database name.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    if s.is_empty() {
        "db".into()
    } else {
        s
    }
}

/// The run lock key for a connection id via the real manager (helper for the app layer).
pub fn server_key_of(deps: &RealDeps, id: &ConnectionId) -> AppResult<ServerKey> {
    Ok(deps.manager.entry(id)?.server_key())
}
