//! Swap sub-steps and the recovery actions that finish or undo an interrupted swap.
//!
//! Staging-then-swap: quiesce pgWhy's own connections to target + staging (+ `_old`) →
//! refuse on prepared transactions / an occupied `_old` name → terminate sessions on target
//! and staging and poll until none remain (short timeout; on timeout fail retryably naming
//! the blocking pids/applications) → persist, stamp the target with an `Old` marker carrying
//! its original comment, rename target→`_old` → persist `renames_completed = 1` → rename
//! staging→target → persist 2 → clear the marker → drop `_old` (marker-verified; a failure is
//! success-with-cleanup). Renames retry through "being accessed by other users" by
//! re-terminating until the deadline (reconnecting clients race the rename). Cancel is
//! refused from the first rename until the second completes.
//!
//! Create-mode: quiesce, drain staging only, rename staging→target, then persist 2.

use crate::engine::{RunEvent, Runner};
use crate::history::{
    OperationDetail, OperationRecord, RunId, RunRecord, RunStatus, SessionInfo, StepStatus,
    SwapPhase, WorkDbState,
};
use crate::ops::{object_in_use, DbFact, MaintenanceOps, PreparedXact, RefreshOps, RunCancel};
use crate::staging::{Marker, MarkerRole};
use pgwhy_core::{AppError, AppResult, ErrorClass, ErrorKind};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

const POLL: Duration = Duration::from_millis(150);

/// The swap's outage window, from the first rename's start to the second's end, as a flag that
/// cancel and the swap settle *together*.
///
/// A bare flag is racy: a cancel that read "not in the window" and then raised the cancel flag
/// could land after the swap's last cancel check and before it entered the window, and be
/// acknowledged yet silently ignored. Here entering the window checks cancellation and cancel
/// checks the window under one lock, so a cancel is either seen by the swap (which then does not
/// start renaming) or refused by the caller (the swap is already committed to finish).
#[derive(Debug, Clone, Default)]
pub(crate) struct SwapWindow(Arc<Mutex<bool>>);

impl SwapWindow {
    fn state(&self) -> std::sync::MutexGuard<'_, bool> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        *self.state()
    }

    /// Open the window unless `cancel` already fired (then nothing is committed to yet).
    pub(crate) fn enter(&self, cancel: Option<&RunCancel>) -> AppResult<()> {
        let mut open = self.state();
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Err(AppError::new(ErrorKind::Cancelled, "the run was cancelled"));
        }
        *open = true;
        Ok(())
    }

    pub(crate) fn leave(&self) {
        *self.state() = false;
    }

    /// Cancel `cancel` unless the window is open. `false` means refused: the swap is between its
    /// renames and will finish, and the cancel flag was not raised.
    pub(crate) fn cancel(&self, cancel: &RunCancel) -> bool {
        let open = self.state();
        if *open {
            return false;
        }
        cancel.cancel();
        true
    }
}

fn blocked_error(dbs: &[String], sessions: &[SessionInfo]) -> AppError {
    let list = sessions
        .iter()
        .map(SessionInfo::describe)
        .collect::<Vec<_>>()
        .join(", ");
    AppError::new(ErrorKind::Refused, format!("sessions are still connected to {}: {list}", dbs.join(", ")))
        .with_class(ErrorClass::Retriable)
        .with_hint("close the application holding the database (it may be reconnecting automatically), then retry the swap")
        .with_data(json!({ "sessions": sessions }))
}

pub(crate) fn prepared_refusal(db: &str, px: &[PreparedXact]) -> AppError {
    let gids = px
        .iter()
        .map(|p| format!("'{}' (owner {})", p.gid, p.owner))
        .collect::<Vec<_>>()
        .join(", ");
    AppError::new(ErrorKind::Refused, format!("{db} has {} prepared transaction(s): {gids}", px.len()))
        .with_hint("commit or roll them back first (ROLLBACK PREPARED 'gid'); pgWhy never discards them blindly")
        .with_data(json!({ "prepared": px }))
}

/// Terminate every session on `dbs` and poll until none remain, or fail naming the blockers.
pub(crate) async fn drain(
    maint: &dyn MaintenanceOps,
    dbs: &[String],
    timeout: Duration,
    cancel: Option<&RunCancel>,
) -> AppResult<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Err(AppError::new(ErrorKind::Cancelled, "the run was cancelled"));
        }
        maint.terminate(dbs).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let sessions = maint.sessions(dbs).await?;
        if sessions.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(blocked_error(dbs, &sessions));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Retry an operation through "database is being accessed by other users" by re-terminating
/// `drain_dbs` until the deadline.
async fn with_drain<'a, F>(
    maint: &'a dyn MaintenanceOps,
    drain_dbs: &[String],
    timeout: Duration,
    mut op: F,
) -> AppResult<()>
where
    F: FnMut() -> pgwhy_core::conn::BoxFuture<'a, AppResult<()>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        match op().await {
            Ok(()) => return Ok(()),
            Err(e) if object_in_use(&e) => {
                if Instant::now() >= deadline {
                    let sessions = maint.sessions(drain_dbs).await.unwrap_or_default();
                    return Err(blocked_error(drain_dbs, &sessions));
                }
                let _ = maint.terminate(drain_dbs).await;
                tokio::time::sleep(POLL).await;
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) async fn rename_with_drain(
    maint: &dyn MaintenanceOps,
    from: &str,
    to: &str,
    timeout: Duration,
) -> AppResult<()> {
    let drain_dbs = vec![from.to_string()];
    with_drain(maint, &drain_dbs, timeout, || {
        maint.rename_database(from, to)
    })
    .await
}

pub(crate) async fn drop_with_drain(
    maint: &dyn MaintenanceOps,
    db: &str,
    timeout: Duration,
) -> AppResult<()> {
    drop_with_drain_cancellable(maint, db, timeout, None).await
}

/// [`drop_with_drain`] that stops draining when `cancel` fires (before the drop statement runs).
pub(crate) async fn drop_with_drain_cancellable(
    maint: &dyn MaintenanceOps,
    db: &str,
    timeout: Duration,
    cancel: Option<&RunCancel>,
) -> AppResult<()> {
    let drain_dbs = vec![db.to_string()];
    drain(maint, &drain_dbs, timeout, cancel).await?;
    with_drain(maint, &drain_dbs, timeout, || maint.drop_database(db)).await
}

/// Drop `db` only if it carries a pgWhy marker naming `run` (and `role`, when given).
/// Already gone is success.
pub(crate) async fn drop_marked(
    maint: &dyn MaintenanceOps,
    db: &str,
    run: &RunId,
    role: Option<MarkerRole>,
    timeout: Duration,
) -> AppResult<()> {
    let dbs = maint.databases().await?;
    let Some(fact) = dbs.iter().find(|d| d.name == db) else {
        return Ok(());
    };
    let Some(marker) = fact.comment.as_deref().and_then(Marker::parse) else {
        return Err(AppError::new(
            ErrorKind::Refused,
            format!(
                "{db} is not marked by pgWhy — it may be a real database, so it was not dropped"
            ),
        ));
    };
    if marker.run_id != run.0 {
        return Err(AppError::new(
            ErrorKind::Refused,
            format!("{db} belongs to run {}, not {run}", marker.run_id),
        ));
    }
    if role.is_some_and(|r| r != marker.role) {
        return Err(AppError::new(
            ErrorKind::Refused,
            format!("{db} is not the database this action expects"),
        ));
    }
    drop_with_drain(maint, db, timeout).await
}

/// The persisted half of the swap step's state. Both records keep it: a Refresh run in its own
/// fields, a replace-restore in [`crate::history::SwapState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwapProgress {
    pub phase: Option<SwapPhase>,
    pub renames_completed: u8,
    pub work_db_state: WorkDbState,
    pub warnings: Vec<String>,
}

impl Default for SwapProgress {
    fn default() -> Self {
        Self {
            phase: None,
            renames_completed: 0,
            work_db_state: WorkDbState::Absent,
            warnings: Vec::new(),
        }
    }
}

/// Everything the swap needs, from whoever is running it (the Refresh runner, or a replace
/// restore). `persist` is called at every crash-recovery boundary — after each phase change and
/// each completed rename — so an interruption can always be finished or rolled back.
pub(crate) struct SwapJob<'a> {
    pub ops: &'a dyn RefreshOps,
    pub maint: &'a dyn MaintenanceOps,
    pub connection: &'a pgwhy_core::config::ConnectionId,
    pub target: &'a str,
    pub staging: &'a str,
    pub old: &'a str,
    /// The target did not exist before this operation: staging is renamed straight into place.
    pub create_mode: bool,
    pub run_id: &'a RunId,
    /// Marker owner: the workflow id, or the operation's own id.
    pub owner: &'a str,
    pub timeout: Duration,
    pub cancel: Option<&'a RunCancel>,
    pub in_window: &'a SwapWindow,
    pub progress: SwapProgress,
    pub persist: &'a mut (dyn FnMut(&SwapProgress) -> AppResult<()> + Send),
}

pub(crate) struct SwapOutcome {
    pub progress: SwapProgress,
    /// Set when the swap finished but dropping `<target>_old` failed (success + cleanup).
    pub cleanup_error: Option<AppError>,
}

/// After `rename_database(from, to)` returned `err`, did the rename commit anyway? A refused
/// rename (sessions still connected, 55006) provably did nothing. Any other error is ambiguous,
/// so the server's database list decides: `to` exists and `from` does not means it landed. If
/// the list cannot be read either, say so in the warnings and assume nothing moved (the
/// pre-existing behaviour), so the user is told to check.
async fn rename_landed(job: &mut SwapJob<'_>, from: &str, to: &str, err: &AppError) -> bool {
    if crate::ops::object_in_use(err) {
        return false;
    }
    match job.maint.databases().await {
        Ok(dbs) => {
            let has = |n: &str| dbs.iter().any(|d| d.name == n);
            has(to) && !has(from)
        }
        Err(e) => {
            job.progress.warnings.push(format!(
                "renaming {from} to {to} failed ({}) and the server could not be asked whether it \
                 took effect ({}); check which of the two databases exists",
                err.message, e.message
            ));
            false
        }
    }
}

fn set_phase(job: &mut SwapJob<'_>, phase: SwapPhase) -> AppResult<()> {
    job.progress.phase = Some(phase);
    (job.persist)(&job.progress)
}

async fn clear_marker(job: &mut SwapJob<'_>, db: &str) {
    if let Err(e) = job.maint.set_comment(db, None).await {
        job.progress.warnings.push(format!(
            "could not clear pgWhy's marker comment on {db}: {}",
            e.message
        ));
    }
}

/// Why `staging` cannot be swapped in by `run`, judged from a fresh database list: it is gone,
/// carries no pgWhy marker, or is marked for another run or role. `None` means it is this run's.
pub(crate) fn staging_problem(dbs: &[DbFact], staging: &str, run: &RunId) -> Option<AppError> {
    let Some(fact) = dbs.iter().find(|d| d.name == staging) else {
        return Some(
            AppError::new(
                ErrorKind::Refused,
                format!("the staging database {staging} no longer exists, so nothing was renamed"),
            )
            .with_hint("run the workflow again to build a new one"),
        );
    };
    match fact.comment.as_deref().and_then(Marker::parse) {
        Some(m) if m.run_id == run.0 && m.role == MarkerRole::Staging => None,
        Some(m) => Some(AppError::new(
            ErrorKind::Refused,
            format!(
                "{staging} is not this run's staging database (it is marked for run {} as {:?}), so nothing was renamed",
                m.run_id, m.role
            ),
        )),
        None => Some(AppError::new(
            ErrorKind::Refused,
            format!("{staging} is not marked by pgWhy and may be a real database, so nothing was renamed"),
        )),
    }
}

/// The staging-then-swap Swap step (and its create-mode variant), shared by the Refresh runner
/// and a replace-restore. On error the caller's record holds the last persisted state, which is
/// what makes recovery possible.
pub(crate) async fn swap_into_place(mut job: SwapJob<'_>) -> AppResult<SwapOutcome> {
    let target = job.target.to_string();
    let staging = job.staging.to_string();
    let old = job.old.to_string();
    let create_mode = job.create_mode;
    let mut cleanup_error = None;

    set_phase(&mut job, SwapPhase::Quiescing)?;
    let _quiesce = job
        .ops
        .quiesce(
            job.connection,
            vec![target.clone(), staging.clone(), old.clone()],
        )
        .await?;
    let dbs = job.maint.databases().await?;
    let target_fact = dbs.iter().find(|d| d.name == target).cloned();
    match (create_mode, &target_fact) {
        (true, Some(_)) => {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("{target} was created on the target server after this run started"),
            )
            .with_hint("run the workflow again to replace it"))
        }
        (false, None) => {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("{target} no longer exists on the target server"),
            )
            .with_hint("run the workflow again; it will create the database"))
        }
        _ => {}
    }
    // The staging database is what takes the target's place: check it is there, and ours, from
    // the list just fetched — before rename 1 moves the target away. Otherwise a staging
    // database dropped or replaced since the verify step would leave the target renamed to `_old`
    // with nothing to put in its place.
    if let Some(e) = staging_problem(&dbs, &staging, job.run_id) {
        let _ = (job.persist)(&job.progress);
        return Err(e);
    }
    if !create_mode {
        set_phase(&mut job, SwapPhase::CheckingPreparedTransactions)?;
        let px = job.maint.prepared_xacts(&target).await?;
        if !px.is_empty() {
            let e = prepared_refusal(&target, &px);
            let _ = (job.persist)(&job.progress);
            return Err(e);
        }
        if let Some(o) = dbs.iter().find(|d| d.name == old) {
            let why = match o.comment.as_deref().and_then(Marker::parse) {
                Some(m) => format!(
                    "it was left by run {}; drop it from that run's history",
                    m.run_id
                ),
                None => "it is not marked by pgWhy and may be a real database".to_string(),
            };
            let e = AppError::new(
                ErrorKind::Refused,
                format!("a database named {old} already exists: {why}"),
            );
            let _ = (job.persist)(&job.progress);
            return Err(e);
        }
    }

    set_phase(&mut job, SwapPhase::TerminatingSessions)?;
    let drain_dbs = if create_mode {
        vec![staging.clone()]
    } else {
        vec![target.clone(), staging.clone()]
    };
    drain(job.maint, &drain_dbs, job.timeout, job.cancel).await?;

    // Entering the window re-checks cancellation under the same lock `cancel` takes, so a cancel
    // that lands after the drain is honoured here, or refused by `cancel` — never dropped.
    if create_mode {
        job.in_window.enter(job.cancel)?;
        set_phase(&mut job, SwapPhase::RenamingStagingToTarget)?;
        let r = rename_with_drain(job.maint, &staging, &target, job.timeout).await;
        job.in_window.leave();
        r?;
        job.progress.renames_completed = 2;
        job.progress.work_db_state = WorkDbState::SwappedIn;
        (job.persist)(&job.progress)?;
        clear_marker(&mut job, &target).await;
        set_phase(&mut job, SwapPhase::Done)?;
        return Ok(SwapOutcome {
            progress: job.progress,
            cleanup_error,
        });
    }

    // A comment that is already our Old marker means an earlier attempt stamped it and stopped
    // before renaming: keep the *original* comment it saved.
    let current = target_fact.and_then(|f| f.comment);
    let original_comment = match current.as_deref().and_then(Marker::parse) {
        Some(m) if m.role == MarkerRole::Old => m.original_comment,
        Some(_) => None,
        None => current,
    };
    let mut marker = Marker::new(MarkerRole::Old, &job.run_id.0, job.owner, &target);
    marker.original_comment = original_comment.clone();

    job.in_window.enter(job.cancel)?;
    // Persisted (renames_completed = 0, phase) before the first rename starts.
    if let Err(e) = set_phase(&mut job, SwapPhase::RenamingTargetToOld) {
        job.in_window.leave();
        return Err(e);
    }
    if let Err(e) = job
        .maint
        .set_comment(&target, Some(&marker.to_comment()))
        .await
    {
        job.in_window.leave();
        job.progress.phase = Some(SwapPhase::TerminatingSessions);
        let _ = (job.persist)(&job.progress);
        return Err(e);
    }
    if let Err(e) = rename_with_drain(job.maint, &target, &old, job.timeout).await {
        // An error is not proof that nothing moved: a dropped connection or tunnel can lose the
        // reply to a rename the server already committed. Ask the server before persisting.
        if rename_landed(&mut job, &target, &old, &e).await {
            // The target is sitting under `old`: record it so recovery (rollback / complete)
            // sees the truth. The Old marker stays on the renamed database.
            job.in_window.leave();
            job.progress.renames_completed = 1;
            let _ = (job.persist)(&job.progress);
            return Err(e);
        }
        let _ = job
            .maint
            .set_comment(&target, original_comment.as_deref())
            .await;
        job.in_window.leave();
        job.progress.phase = Some(SwapPhase::TerminatingSessions);
        let _ = (job.persist)(&job.progress);
        return Err(e);
    }
    job.progress.renames_completed = 1;
    // Persisted before the second rename starts.
    set_phase(&mut job, SwapPhase::RenamingStagingToTarget)?;
    if let Err(e) = rename_with_drain(job.maint, &staging, &target, job.timeout).await {
        // Same reasoning as the first rename: if the server did finish it, the swap is
        // complete and must be recorded as such, not reported as half-done.
        if rename_landed(&mut job, &staging, &target, &e).await {
            job.progress.warnings.push(format!(
                "the connection reported an error ({}) but the server had already renamed \
                 {staging} to {target}; the swap is complete",
                e.message
            ));
        } else {
            job.in_window.leave();
            return Err(e);
        }
    }
    job.progress.renames_completed = 2;
    job.progress.work_db_state = WorkDbState::SwappedIn;
    (job.persist)(&job.progress)?;
    job.in_window.leave();
    clear_marker(&mut job, &target).await;

    set_phase(&mut job, SwapPhase::DroppingOld)?;
    if let Err(e) = drop_marked(
        job.maint,
        &old,
        job.run_id,
        Some(MarkerRole::Old),
        job.timeout,
    )
    .await
    {
        cleanup_error = Some(e);
    }
    set_phase(&mut job, SwapPhase::Done)?;
    Ok(SwapOutcome {
        progress: job.progress,
        cleanup_error,
    })
}

impl Runner {
    /// The Refresh pipeline's Swap step: the shared swap, with the run record as its persistence
    /// and the run's own event stream for phase changes.
    pub(crate) async fn swap(&mut self) -> AppResult<()> {
        let maint = self.maint().await?;
        let target = self.plan.target_db().to_string();
        let staging = self.plan.staging_db.clone();
        let old = self.plan.old_db.clone();
        let timeout = self.inner.config.session_drain_timeout;
        let run_id = self.run_id();
        let owner = self.plan.workflow.id.0.clone();
        let connection = self.plan.target.id.clone();
        let create_mode = self.plan.create_mode;

        // Disjoint field borrows: the persist closure writes the record while the job reads the
        // plan, the ops and the cancel flag.
        let Runner {
            inner,
            rec,
            sink,
            cancel,
            in_window,
            ..
        } = self;
        let ops = inner.ops.clone();
        let history = inner.history.clone();
        let progress = SwapProgress {
            phase: rec.swap_phase,
            renames_completed: rec.renames_completed,
            work_db_state: rec.work_db_state,
            warnings: Vec::new(),
        };
        // Emit a swap-phase event when the phase *changes*: the shared step persists several
        // times per phase (after each rename), and the UI's phase list must stay one row each.
        let mut last_phase: Option<SwapPhase> = None;
        let mut persist = |p: &SwapProgress| {
            rec.swap_phase = p.phase;
            rec.renames_completed = p.renames_completed;
            rec.work_db_state = p.work_db_state;
            for w in &p.warnings {
                if !rec.warnings.contains(w) {
                    rec.warnings.push(w.clone());
                }
            }
            if let Some(phase) = p.phase.filter(|ph| last_phase != Some(*ph)) {
                last_phase = Some(phase);
                sink(RunEvent::SwapPhase {
                    run_id: run_id.clone(),
                    phase,
                    renames_completed: p.renames_completed,
                });
            }
            history.save(rec)
        };

        let job = SwapJob {
            ops: &*ops,
            maint: &*maint,
            connection: &connection,
            target: &target,
            staging: &staging,
            old: &old,
            create_mode,
            run_id: &run_id,
            owner: &owner,
            timeout,
            cancel: Some(cancel),
            in_window,
            progress,
            persist: &mut persist,
        };
        let outcome = swap_into_place(job).await;
        let outcome = outcome?;
        rec.swap_phase = outcome.progress.phase;
        rec.renames_completed = outcome.progress.renames_completed;
        rec.work_db_state = outcome.progress.work_db_state;
        for w in outcome.progress.warnings {
            if !rec.warnings.contains(&w) {
                rec.warnings.push(w);
            }
        }
        self.cleanup_error = outcome.cleanup_error;
        self.save()?;
        Ok(())
    }
}

/// Drop-first's DropTarget step: quiesce the target, refuse on prepared transactions,
/// terminate its sessions and drop it.
pub(crate) async fn drop_target(r: &mut Runner) -> AppResult<()> {
    let target = r.plan.target_db().to_string();
    let timeout = r.inner.config.session_drain_timeout;
    let ops = r.inner.ops.clone();
    let quiesce = ops.quiesce(&r.plan.target.id, vec![target.clone()]).await?;
    let maint = r.maint().await?;
    if !maint.databases().await?.iter().any(|d| d.name == target) {
        r.rec.work_db_state = WorkDbState::Absent;
        return Ok(());
    }
    let px = maint.prepared_xacts(&target).await?;
    if !px.is_empty() {
        return Err(prepared_refusal(&target, &px));
    }
    let dbs = vec![target.clone()];
    drain(&*maint, &dbs, timeout, Some(&r.cancel)).await?;
    r.check_cancel()?;
    with_drain(&*maint, &dbs, timeout, || maint.drop_database(&target)).await?;
    r.rec.work_db_state = WorkDbState::Absent;
    // Released now: post-restore and verify connect to the new database by its name.
    drop(quiesce);
    Ok(())
}

/// Undo a swap stopped between its renames: `_old` → target, original comment restored.
/// Staging stays under its own name (offered for drop afterwards).
/// Roll a half-finished swap back: `old` becomes `target` again, with its original comment.
/// Refuses unless `old` carries pgWhy's Old marker for `target`, so a database that merely has
/// that name is never renamed over anything. Returns warnings (e.g. a comment that would not go
/// back).
pub(crate) async fn rollback_rename(
    maint: &dyn MaintenanceOps,
    target: &str,
    old: &str,
    timeout: Duration,
) -> AppResult<Vec<String>> {
    let dbs = maint.databases().await?;
    let marker = dbs
        .iter()
        .find(|d| d.name == old)
        .and_then(|d| d.comment.as_deref())
        .and_then(Marker::parse)
        .filter(|m| m.role == MarkerRole::Old && m.original_target == target)
        .ok_or_else(|| {
            AppError::new(
                ErrorKind::Refused,
                format!("{old} is not marked as the previous {target}"),
            )
        })?;
    drain(maint, &[old.to_string()], timeout, None).await?;
    rename_with_drain(maint, old, target, timeout).await?;
    let mut warnings = Vec::new();
    if let Err(e) = maint
        .set_comment(target, marker.original_comment.as_deref())
        .await
    {
        warnings.push(format!(
            "could not restore the comment on {target}: {}",
            e.message
        ));
    }
    Ok(warnings)
}

pub(crate) async fn rollback(
    ops: &dyn RefreshOps,
    maint: &dyn MaintenanceOps,
    rec: &mut RunRecord,
    timeout: Duration,
) -> AppResult<()> {
    let target = rec.target.database.clone();
    let old = rec
        .old_db
        .clone()
        .ok_or_else(|| AppError::internal("no _old name recorded"))?;
    let staging = rec.staging_db.clone().unwrap_or_default();
    let _q = ops
        .quiesce(
            &rec.target.connection_id,
            vec![target.clone(), old.clone(), staging],
        )
        .await?;
    let warnings = rollback_rename(maint, &target, &old, timeout).await?;
    rec.renames_completed = 0;
    rec.swap_phase = None;
    rec.warnings.extend(warnings);
    if rec.status != RunStatus::Interrupted {
        rec.status = RunStatus::FailedAtSwap;
    }
    rec.warnings.push(format!(
        "rolled back: {target} is the previous database again"
    ));
    Ok(())
}

/// Finish a swap stopped between its renames: staging → target, then drop `_old`.
pub(crate) async fn complete(
    ops: &dyn RefreshOps,
    maint: &dyn MaintenanceOps,
    rec: &mut RunRecord,
    timeout: Duration,
) -> AppResult<()> {
    let target = rec.target.database.clone();
    let old = rec.old_db.clone().unwrap_or_default();
    let staging = rec
        .staging_db
        .clone()
        .ok_or_else(|| AppError::internal("no staging name recorded"))?;
    let _q = ops
        .quiesce(
            &rec.target.connection_id,
            vec![target.clone(), old.clone(), staging.clone()],
        )
        .await?;
    // Same ownership check the swap itself makes: never rename a database that is not this
    // run's staging copy into the target's place.
    if let Some(e) = staging_problem(&maint.databases().await?, &staging, &rec.run_id) {
        return Err(e);
    }
    drain(maint, std::slice::from_ref(&staging), timeout, None).await?;
    rename_with_drain(maint, &staging, &target, timeout).await?;
    rec.renames_completed = 2;
    rec.work_db_state = WorkDbState::SwappedIn;
    rec.swap_phase = Some(SwapPhase::Done);
    if let Err(e) = maint.set_comment(&target, None).await {
        rec.warnings.push(format!(
            "could not clear pgWhy's marker comment on {target}: {}",
            e.message
        ));
    }
    if let Some(e) = rec.error.take() {
        rec.warnings.push(format!(
            "swap completed after recovery; the run had stopped with: {}",
            e.message
        ));
    }
    rec.failed_step = None;
    rec.status = match drop_marked(maint, &old, &rec.run_id, Some(MarkerRole::Old), timeout).await {
        Ok(()) => RunStatus::Succeeded,
        Err(e) => {
            rec.warnings
                .push(format!("{old} could not be dropped: {}", e.message));
            RunStatus::SucceededCleanupRequired
        }
    };
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Recovery of a replace-restore (an operation record) — same moves as for a Refresh run
// ---------------------------------------------------------------------------------------------

/// What recovery needs from an operation record: the names a replace-restore recorded.
struct OperationSwap {
    connection: pgwhy_core::config::ConnectionId,
    target: String,
    staging: String,
    old: String,
}

fn operation_swap(rec: &OperationRecord) -> AppResult<OperationSwap> {
    let OperationDetail::Restore(r) = &rec.detail else {
        return Err(AppError::new(
            ErrorKind::Refused,
            "only a replace-restore has a swap to recover",
        ));
    };
    let swap = r.swap.as_ref().ok_or_else(|| {
        AppError::new(
            ErrorKind::Refused,
            "this operation recorded no swap to recover",
        )
    })?;
    Ok(OperationSwap {
        connection: r.target.connection_id.clone(),
        target: r.target.database.clone(),
        staging: swap
            .staging_db
            .clone()
            .ok_or_else(|| AppError::internal("no staging name recorded"))?,
        old: swap
            .old_db
            .clone()
            .ok_or_else(|| AppError::internal("no _old name recorded"))?,
    })
}

fn swap_state_mut(rec: &mut OperationRecord) -> Option<&mut crate::history::SwapState> {
    match &mut rec.detail {
        OperationDetail::Restore(r) => r.swap.as_mut(),
        _ => None,
    }
}

/// Roll a replace-restore's half-finished swap back: `_old` becomes the target again. Staging
/// stays under its own name (the record then offers to drop it).
pub(crate) async fn rollback_operation(
    ops: &dyn RefreshOps,
    maint: &dyn MaintenanceOps,
    rec: &mut OperationRecord,
    timeout: Duration,
) -> AppResult<()> {
    let n = operation_swap(rec)?;
    let _q = ops
        .quiesce(
            &n.connection,
            vec![n.target.clone(), n.old.clone(), n.staging.clone()],
        )
        .await?;
    let warnings = rollback_rename(maint, &n.target, &n.old, timeout).await?;
    if let Some(s) = swap_state_mut(rec) {
        s.renames_completed = 0;
        s.swap_phase = None;
        s.work_db_state = WorkDbState::Verified;
    }
    rec.warnings.extend(warnings);
    rec.warnings.push(format!(
        "rolled back: {} is the previous database again; the restored copy is still in {}",
        n.target, n.staging
    ));
    Ok(())
}

/// Finish a replace-restore's swap stopped between its renames: staging → target, marker cleared,
/// then `_old` dropped. Refuses unless the staging database is still this operation's own.
pub(crate) async fn complete_operation(
    ops: &dyn RefreshOps,
    maint: &dyn MaintenanceOps,
    rec: &mut OperationRecord,
    timeout: Duration,
) -> AppResult<()> {
    let n = operation_swap(rec)?;
    let _q = ops
        .quiesce(
            &n.connection,
            vec![n.target.clone(), n.old.clone(), n.staging.clone()],
        )
        .await?;
    let dbs = maint.databases().await?;
    if let Some(e) = staging_problem(&dbs, &n.staging, &rec.id) {
        return Err(e);
    }
    if dbs.iter().any(|d| d.name == n.target) {
        return Err(AppError::new(
            ErrorKind::Refused,
            format!(
                "{} exists again; the swap cannot be completed over it",
                n.target
            ),
        ));
    }
    drain(maint, std::slice::from_ref(&n.staging), timeout, None).await?;
    rename_with_drain(maint, &n.staging, &n.target, timeout).await?;
    if let Some(s) = swap_state_mut(rec) {
        s.renames_completed = 2;
        s.work_db_state = WorkDbState::SwappedIn;
        s.swap_phase = Some(SwapPhase::Done);
        s.staging_db = None;
    }
    if let Err(e) = maint.set_comment(&n.target, None).await {
        rec.warnings.push(format!(
            "could not clear pgWhy's marker comment on {}: {}",
            n.target, e.message
        ));
    }
    if let Some(e) = rec.error.take() {
        rec.warnings.push(format!(
            "swap completed after recovery; the restore had stopped with: {}",
            e.message
        ));
    }
    rec.failed_step = None;
    if let Some(step) = rec.step_mut("swap") {
        step.status = StepStatus::Succeeded;
        step.error = None;
    }
    rec.status = match drop_marked(maint, &n.old, &rec.id, Some(MarkerRole::Old), timeout).await {
        Ok(()) => RunStatus::Succeeded,
        Err(e) => {
            rec.warnings
                .push(format!("{} could not be dropped: {}", n.old, e.message));
            RunStatus::SucceededCleanupRequired
        }
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(name: &str, comment: Option<String>) -> DbFact {
        DbFact {
            name: name.into(),
            comment,
            owner: "pgwhy".into(),
        }
    }

    #[test]
    fn a_cancel_before_the_window_stops_the_swap_and_one_inside_it_is_refused() {
        // cancelled first: the swap never enters the window
        let (w, c) = (SwapWindow::default(), RunCancel::new());
        assert!(w.cancel(&c), "outside the window a cancel is honoured");
        assert!(c.is_cancelled());
        assert_eq!(w.enter(Some(&c)).unwrap_err().kind, ErrorKind::Cancelled);
        assert!(!w.is_open());

        // window first: the cancel is refused and the flag is not raised, so it cannot linger
        // and misclassify a later failure as a cancellation
        let (w, c) = (SwapWindow::default(), RunCancel::new());
        w.enter(Some(&c)).unwrap();
        assert!(w.is_open());
        assert!(!w.cancel(&c));
        assert!(!c.is_cancelled());
        w.leave();
        assert!(w.cancel(&c), "honoured again once the window closed");
    }

    #[test]
    fn every_interleaving_of_cancel_and_enter_is_honoured_or_refused() {
        for _ in 0..200 {
            let (w, c) = (SwapWindow::default(), RunCancel::new());
            let (w2, c2) = (w.clone(), c.clone());
            let canceller = std::thread::spawn(move || w2.cancel(&c2));
            let entered = w.enter(Some(&c)).is_ok();
            let cancelled = canceller.join().unwrap();
            // the swap is committed XOR the cancel took effect — never both, never neither
            assert_ne!(entered, cancelled);
        }
    }

    #[test]
    fn staging_must_exist_and_carry_this_runs_staging_marker() {
        let run = RunId("run-1".into());
        let mine = Marker::new(MarkerRole::Staging, "run-1", "wf", "app").to_comment();
        let other = Marker::new(MarkerRole::Staging, "run-2", "wf", "app").to_comment();
        let wrong_role = Marker::new(MarkerRole::Old, "run-1", "wf", "app").to_comment();

        let ok = [fact("app_staging", Some(mine))];
        assert!(staging_problem(&ok, "app_staging", &run).is_none());
        assert!(staging_problem(&ok, "missing", &run)
            .unwrap()
            .message
            .contains("no longer exists"));
        let foreign = [fact("app_staging", Some(other))];
        assert!(staging_problem(&foreign, "app_staging", &run)
            .unwrap()
            .message
            .contains("run-2"));
        let unmarked = [fact("app_staging", Some("my notes".into()))];
        assert!(staging_problem(&unmarked, "app_staging", &run)
            .unwrap()
            .message
            .contains("not marked"));
        let role = [fact("app_staging", Some(wrong_role))];
        assert!(staging_problem(&role, "app_staging", &run).is_some());
    }
}
