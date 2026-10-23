//! The retry matrix and the state-aware action set (ER-1), derived from a run record **and a
//! live inventory** — never from recorded state alone.
//!
//! Rules (plan, High-Level Technical Design):
//! - While the swap is between its two renames (`0 < renames_completed < 2`, or the scan shows
//!   the target name free with `<target>_old` holding the working copy), the only actions are
//!   {complete swap, roll back}; staging/`_old` drop offers are suppressed so one cleanup click
//!   can never destroy both copies.
//! - Backup reuse needs the recorded reusability verdict *and* the file; an archive-corrupt
//!   verdict redirects to re-dump. Staging reuse needs it to exist, carry this run's marker and
//!   be in a clean state (a failed no-transaction script makes it non-reusable).
//! - Every offer is gated on live reachability and disabled with a reason, never hidden.

use crate::definition::{ReplaceStrategy, ScriptMode};
use crate::history::*;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use std::collections::HashSet;
use std::path::PathBuf;

/// True when the working database exists only under the `_old` name.
pub fn in_swap_window(rec: &RunRecord, inv: Option<&Inventory>) -> bool {
    if rec.strategy != ReplaceStrategy::StagingSwap || rec.create_mode || rec.status.is_success() {
        return false;
    }
    match inv.filter(|i| i.reachable) {
        Some(i) => {
            let old_holds = i.old.as_ref().is_some_and(|o| o.exists);
            !i.target.exists && old_holds
        }
        // Can't look: trust the record, conservatively.
        // `renames_completed` can lag by one, so a run that stopped *at* the first rename may
        // already be in the window.
        None => {
            rec.renames_completed == 1
                || (rec.renames_completed == 0
                    && rec.status.is_failure()
                    && rec.swap_phase == Some(SwapPhase::RenamingTargetToOld))
        }
    }
}

/// The step a failed/cancelled/interrupted run stopped at.
pub fn stopped_at(rec: &RunRecord) -> Option<StepId> {
    rec.failed_step.or_else(|| {
        rec.steps
            .iter()
            .find(|s| {
                matches!(
                    s.status,
                    StepStatus::Running
                        | StepStatus::Interrupted
                        | StepStatus::Failed
                        | StepStatus::Cancelled
                )
            })
            .map(|s| s.step)
    })
}

/// What the report must lead with.
pub fn headline(rec: &RunRecord, inv: Option<&Inventory>) -> Option<String> {
    let target = &rec.target.database;
    if in_swap_window(rec, inv) {
        let old = rec.old_db.as_deref().unwrap_or("the _old database");
        return Some(format!(
            "Outage: {target} does not exist right now — the previous database is held as {old}. Complete the swap or roll back."
        ));
    }
    if !rec.status.is_failure() {
        return None;
    }
    match rec.strategy {
        ReplaceStrategy::DropFirst => {
            let dropped = rec
                .step(StepId::DropTarget)
                .is_some_and(|s| s.status == StepStatus::Succeeded)
                || rec.create_mode
                || inv.is_some_and(|i| i.reachable && !i.target.exists);
            let work_started = rec
                .step(StepId::CreateDatabase)
                .is_some_and(|s| s.status != StepStatus::Pending);
            if dropped || work_started {
                Some(format!(
                    "You currently have no working local database: {target} was dropped and the new copy is missing or incomplete."
                ))
            } else {
                Some(format!("{target} is untouched."))
            }
        }
        ReplaceStrategy::StagingSwap => {
            if rec.create_mode {
                Some(format!("{target} has not been created yet."))
            } else {
                Some(format!("{target} is untouched."))
            }
        }
    }
}

fn offer(kind: ActionKind, label: impl Into<String>, db: Option<&str>) -> ActionOffer {
    ActionOffer {
        kind,
        label: label.into(),
        enabled: true,
        disabled_reason: None,
        database: db.map(str::to_string),
        retry: None,
    }
}

fn disable(mut a: ActionOffer, reason: impl Into<String>) -> ActionOffer {
    if a.enabled {
        a.enabled = false;
        a.disabled_reason = Some(reason.into());
    }
    a
}

fn unreachable_reason(inv: &Inventory) -> String {
    format!(
        "the target server can't be reached: {}",
        inv.error
            .as_ref()
            .map(|e| e.message.as_str())
            .unwrap_or("unknown error")
    )
}

/// Why a database can't be dropped from this record, or None when it can.
fn drop_refusal(p: &DbPresence) -> Option<String> {
    if !p.exists {
        return Some(format!("{} no longer exists", p.name));
    }
    match &p.marker {
        None => Some(format!(
            "{} is not marked by pgWhy — it may be a real database",
            p.name
        )),
        Some(m) if !p.owned_by_run => Some(format!("{} now belongs to run {}", p.name, m.run_id)),
        Some(_) => None,
    }
}

/// The full state-aware action set for a record given a (preferably fresh) inventory.
pub fn derive_actions(rec: &RunRecord, inv: Option<&Inventory>) -> Vec<ActionOffer> {
    let reachable = inv.is_some_and(|i| i.reachable);
    let gate = |a: ActionOffer| -> ActionOffer {
        match inv {
            Some(i) if !i.reachable => disable(a, unreachable_reason(i)),
            None => disable(a, "re-check the target server first"),
            _ => a,
        }
    };
    let mut out = Vec::new();

    if in_swap_window(rec, inv) {
        let staging = rec.staging_db.as_deref();
        let mut complete = gate(offer(
            ActionKind::CompleteSwap,
            "Complete the swap",
            staging,
        ));
        let mut rollback = gate(offer(
            ActionKind::RollbackSwap,
            "Roll back to the previous database",
            rec.old_db.as_deref(),
        ));
        if let Some(i) = inv.filter(|_| reachable) {
            match i.staging.as_ref() {
                Some(s) if s.exists && s.owned_by_run => {}
                Some(s) => {
                    complete = disable(
                        complete,
                        drop_refusal(s).unwrap_or_else(|| format!("{} is not usable", s.name)),
                    )
                }
                None => complete = disable(complete, "the staging database is unknown"),
            }
            match i.old.as_ref() {
                Some(o) if o.exists && o.marker.is_some() => {}
                _ => {
                    rollback = disable(
                        rollback,
                        "the previous database is not available under its _old name",
                    )
                }
            }
            if i.target.exists {
                rollback = disable(rollback, format!("{} already exists", rec.target.database));
            }
        }
        out.push(complete);
        out.push(rollback);
        return out;
    }

    // Success with leftovers: cleanup only.
    if rec.status.is_success() {
        if let Some(o) = inv.and_then(|i| i.old.as_ref()).filter(|o| o.exists) {
            let a = gate(offer(
                ActionKind::DropOld,
                format!("Drop {}", o.name),
                Some(&o.name),
            ));
            out.push(match drop_refusal(o) {
                Some(r) => disable(a, r),
                None => a,
            });
        }
        return out;
    }
    if !rec.status.is_failure() {
        return out;
    }

    out.extend(retry_offers(rec, inv));

    if rec.strategy == ReplaceStrategy::StagingSwap {
        if let Some(s) = inv.and_then(|i| i.staging.as_ref()).filter(|s| s.exists) {
            let d = gate(offer(
                ActionKind::DropStaging,
                format!("Drop {}", s.name),
                Some(&s.name),
            ));
            out.push(match drop_refusal(s) {
                Some(r) => disable(d, r),
                None => d,
            });
            if !rec.staging_kept && s.owned_by_run {
                out.push(offer(
                    ActionKind::KeepStaging,
                    format!("Keep {} for inspection", s.name),
                    Some(&s.name),
                ));
            }
        }
    }
    if let Some(o) = inv.and_then(|i| i.old.as_ref()).filter(|o| o.exists) {
        let d = gate(offer(
            ActionKind::DropOld,
            format!("Drop {}", o.name),
            Some(&o.name),
        ));
        out.push(match drop_refusal(o) {
            Some(r) => disable(d, r),
            None => d,
        });
    }
    out
}

fn backup_problem(rec: &RunRecord) -> Option<String> {
    match &rec.backup {
        None => Some("this run has no backup to reuse".into()),
        Some(b) if !b.reusable => Some(format!(
            "the backup can't be reused: {}",
            b.reason.as_deref().unwrap_or("it failed validation")
        )),
        Some(b) if !b.path.exists() => Some(format!(
            "the backup file {} no longer exists",
            b.path.display()
        )),
        Some(_) => None,
    }
}

fn work_db_problem(rec: &RunRecord, inv: Option<&Inventory>) -> Option<String> {
    let Some(i) = inv.filter(|i| i.reachable) else {
        return Some("the target server can't be checked".into());
    };
    let presence = match rec.strategy {
        ReplaceStrategy::StagingSwap => i.staging.as_ref(),
        ReplaceStrategy::DropFirst => Some(&i.target),
    };
    match presence {
        Some(p) if p.exists && p.owned_by_run => None,
        Some(p) if p.exists => Some(
            drop_refusal(p).unwrap_or_else(|| format!("{} is not this run's database", p.name)),
        ),
        Some(p) => Some(format!("{} no longer exists", p.name)),
        None => Some("the staging database is unknown".into()),
    }
}

/// Retry offers per the matrix: the natural resume point first, then "re-dump from scratch".
pub fn retry_offers(rec: &RunRecord, inv: Option<&Inventory>) -> Vec<ActionOffer> {
    if !rec.status.is_failure() || in_swap_window(rec, inv) {
        return Vec::new();
    }
    let Some(failed) = stopped_at(rec) else {
        return Vec::new();
    };
    let state = rec.work_db_state;
    let txn_script = rec.options.script_mode == ScriptMode::Transaction;
    let backup_bad = backup_problem(rec);
    let work_bad = work_db_problem(rec, inv);

    // (begins_at, reuses_work_db) for the natural resume.
    let natural: (StepId, bool) = match (rec.strategy, failed) {
        (_, StepId::Dump) => (StepId::Dump, false),
        (ReplaceStrategy::DropFirst, StepId::DropTarget) => (StepId::DropTarget, false),
        (_, StepId::CreateDatabase | StepId::Restore) => (StepId::CreateDatabase, false),
        (_, StepId::PostRestore)
            if txn_script && state.restored_cleanly() && work_bad.is_none() =>
        {
            (StepId::PostRestore, true)
        }
        (_, StepId::PostRestore) => (StepId::CreateDatabase, false),
        (_, StepId::Verify) if state.restored_cleanly() && work_bad.is_none() => {
            (StepId::Verify, true)
        }
        (_, StepId::Verify) => (StepId::CreateDatabase, false),
        (_, StepId::Swap) if state == WorkDbState::Verified && work_bad.is_none() => {
            (StepId::Swap, true)
        }
        (_, StepId::Swap) => (StepId::CreateDatabase, false),
        (_, StepId::DropTarget) => (StepId::CreateDatabase, false),
    };

    let mut out = Vec::new();
    let make = |from: StepId, begins_at: StepId, reuses_work_db: bool| -> ActionOffer {
        let reuses_backup = begins_at != StepId::Dump;
        let label = match (from == begins_at, reuses_backup) {
            (_, false) => "Re-dump from scratch and run again".to_string(),
            (true, _) => format!("Retry from \"{}\"", from.label(rec.strategy)),
            (false, _) => format!(
                "Retry from \"{}\" (starting at \"{}\")",
                from.label(rec.strategy),
                begins_at.label(rec.strategy)
            ),
        };
        let mut a = offer(ActionKind::Retry, label, None);
        a.retry = Some(RetryOffer {
            from,
            begins_at,
            reuses_backup,
            reuses_work_db,
        });
        let mut a = match inv {
            Some(i) if !i.reachable => disable(a, unreachable_reason(i)),
            None => disable(a, "re-check the target server first"),
            _ => a,
        };
        if reuses_backup {
            if let Some(r) = &backup_bad {
                a = disable(a, r.clone());
            }
        } else if let Some(i) = inv {
            if i.source_reachable == Some(false) {
                let why = i
                    .source_error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_default();
                a = disable(a, format!("the source server can't be reached: {why}"));
            }
        }
        if reuses_work_db {
            if let Some(r) = &work_bad {
                a = disable(a, r.clone());
            }
        }
        a
    };
    let (begins, reuse) = natural;
    let natural_offer = make(failed, begins, reuse);
    // A backup that can't be reused redirects the natural retry to re-dump.
    if begins != StepId::Dump && backup_bad.is_some() {
        out.push(natural_offer);
        out.push(make(failed, StepId::Dump, false));
    } else {
        out.push(natural_offer);
        if begins != StepId::Dump {
            out.push(make(StepId::Dump, StepId::Dump, false));
        }
    }
    out
}

/// Validate a retry request against the current offers: the enabled offer for `from`, or a
/// refusal naming why.
pub fn plan_retry(rec: &RunRecord, from: StepId, inv: Option<&Inventory>) -> AppResult<RetryOffer> {
    if in_swap_window(rec, inv) {
        return Err(AppError::new(
            ErrorKind::Refused,
            "the swap stopped between its two renames; complete it or roll it back instead of retrying",
        ));
    }
    if !rec.status.is_failure() {
        return Err(AppError::new(
            ErrorKind::Refused,
            "only failed, cancelled or interrupted runs can be retried",
        ));
    }
    let offers = retry_offers(rec, inv);
    let pick = offers
        .iter()
        .filter_map(|a| a.retry.as_ref().map(|r| (a, r)))
        .find(|(a, r)| (r.from == from || r.begins_at == from) && a.enabled)
        .or_else(|| {
            offers
                .iter()
                .filter_map(|a| a.retry.as_ref().map(|r| (a, r)))
                .find(|(_, r)| r.from == from || r.begins_at == from)
        });
    match pick {
        Some((a, r)) if a.enabled => Ok(r.clone()),
        Some((a, _)) => Err(AppError::new(
            ErrorKind::Refused,
            format!(
                "can't retry: {}",
                a.disabled_reason
                    .as_deref()
                    .unwrap_or("prerequisites are not met")
            ),
        )),
        None => Err(AppError::new(
            ErrorKind::Refused,
            format!(
                "retrying from \"{}\" isn't possible for this run",
                from.label(rec.strategy)
            ),
        )),
    }
}

/// Backups retention must keep: those referenced by failed runs that can still be retried
/// (no later successful run of the same workflow).
///
/// See [`protected_backups_with_operations`] for the one-off operations' share.
pub fn protected_backups(records: &[RunRecord]) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for r in records {
        let Some(b) = &r.backup else { continue };
        if !(r.status.is_failure() || r.status == RunStatus::Running) || !b.reusable {
            continue;
        }
        let superseded = records.iter().any(|o| {
            o.workflow_id == r.workflow_id && o.status.is_success() && o.started_at > r.started_at
        });
        if !superseded {
            out.insert(b.path.clone());
        }
    }
    out
}

/// [`protected_backups`] plus what one-off operations must keep:
///
/// - a pre-destruction safety backup (drop/rename took one) is kept for as long as its record
///   exists — it is the undo button for that action;
/// - the archive a *running* restore is reading is never rotated away mid-run.
pub fn protected_backups_with_operations(
    runs: &[RunRecord],
    operations: &[OperationRecord],
) -> HashSet<PathBuf> {
    let mut out = protected_backups(runs);
    for op in operations {
        match &op.detail {
            OperationDetail::Drop {
                backup: Some(path), ..
            } => {
                out.insert(path.clone());
            }
            OperationDetail::Restore(r) => {
                // a pre-restore safety backup is the undo button for an in-place restore
                if let Some(path) = &r.backup {
                    out.insert(path.clone());
                }
                // and the archive a *running* restore is reading must not be rotated away
                if op.status == RunStatus::Running {
                    out.insert(r.archive.path.clone());
                }
            }
            _ => {}
        }
    }
    out
}
