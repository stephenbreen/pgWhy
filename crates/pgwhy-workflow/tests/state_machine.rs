//! State-machine tests over the scripted fake: every transition of the run diagram driven
//! with injected failures → terminal state, report fields, and the state-aware action set.

mod support;

use pgwhy_core::{AppError, ErrorClass, ErrorKind};
use pgwhy_workflow::history::*;
use pgwhy_workflow::ops::{BackupVerdict, PreparedXact, Resume};
use pgwhy_workflow::retry;
use pgwhy_workflow::staging::{Marker, MarkerRole};
use pgwhy_workflow::*;
use std::time::Duration;
use support::*;

fn step_status(rec: &RunRecord, id: StepId) -> StepStatus {
    rec.step(id).unwrap().status
}

fn object_error() -> AppError {
    AppError::new(ErrorKind::ToolFailed, "pg_restore failed on 1 item(s); first: INDEX public.t_idx: ERROR: boom").with_data(
        serde_json::json!({
            "tool": "pg_restore",
            "cause": "object_errors",
            "exit_code": 1,
            "stderr_tail": ["pg_restore: error: could not execute query: ERROR: boom"],
            "failed_items": [{ "object": "INDEX public.t_idx", "error": "ERROR: boom", "command": "CREATE INDEX t_idx ..." }],
        }),
    )
}

/// Build the retry plan the way `Engine::retry` does (minus the server-side preflight).
fn retry_plan(h: &Harness, rec: &RunRecord, from: StepId) -> RunPlan {
    let offer = retry::plan_retry(rec, from, rec.inventory.as_ref()).unwrap();
    let mut plan = h.plan(rec.strategy);
    if offer.reuses_work_db {
        plan.staging_db = rec.staging_db.clone().unwrap();
    }
    let inv = rec.inventory.as_ref().unwrap();
    let mut drop_before_create = Vec::new();
    if !offer.reuses_work_db && rec.strategy == ReplaceStrategy::StagingSwap {
        if let Some(s) = inv.staging.as_ref().filter(|s| s.exists && s.owned_by_run) {
            drop_before_create.push(s.name.clone());
        }
    }
    plan.resume = Some(Resume {
        retry_of: rec.run_id.clone(),
        from: offer.from,
        begins_at: offer.begins_at,
        backup: offer.reuses_backup.then(|| rec.backup.clone().unwrap()),
        adopt_work_db: offer.reuses_work_db,
        work_db_state: rec.work_db_state,
        drop_before_create,
    });
    plan.create_mode = rec.create_mode;
    plan
}

#[tokio::test]
async fn happy_path_staging_then_swap() {
    let h = Harness::new();
    let mut plan = h.plan(ReplaceStrategy::StagingSwap);
    Harness::with_script(&mut plan, ScriptMode::Transaction);
    let staging = plan.staging_db.clone();
    let rec = h.run(plan).await;

    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert_eq!(rec.renames_completed, 2);
    assert_eq!(rec.swap_phase, Some(SwapPhase::Done));
    assert_eq!(rec.work_db_state, WorkDbState::SwappedIn);
    for s in [
        StepId::Dump,
        StepId::CreateDatabase,
        StepId::Restore,
        StepId::PostRestore,
        StepId::Verify,
        StepId::Swap,
    ] {
        assert_eq!(step_status(&rec, s), StepStatus::Succeeded, "{s:?}");
        assert!(rec.step(s).unwrap().duration_ms.is_some());
    }
    assert!(
        rec.step(StepId::DropTarget).is_none(),
        "staging-swap has no drop step"
    );
    // the target is the restored copy, marker cleared; staging and _old are gone; the
    // external session was terminated
    assert!(h.fake.has_db("app"));
    assert_eq!(h.fake.comment("app"), None);
    assert!(!h.fake.has_db(&staging));
    assert!(!h.fake.has_db("app_old"));
    // quiesce covered target + staging and was released
    let q = h.fake.quiesced.lock().unwrap().clone();
    assert!(q[0].contains(&"app".to_string()) && q[0].contains(&staging));
    assert_eq!(
        h.fake
            .quiesce_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    // report fields
    let b = rec.backup.as_ref().unwrap();
    assert!(b.reusable && b.path.exists() && b.toc_items == Some(3));
    assert_eq!(rec.script.as_ref().unwrap().sha256, "abc123");
    assert_eq!(rec.staging_db.as_deref(), Some(staging.as_str()));
    assert_eq!(rec.old_db.as_deref(), Some("app_old"));
    assert!(rec.actions.is_empty());
    assert!(rec.headline.is_none());
    assert!(rec.inventory.as_ref().unwrap().target.exists);
    // persisted == returned
    assert_eq!(h.engine.get_run(&rec.run_id).unwrap(), rec);
    // and it shows up in the merged history as a Refresh row (operations share this list)
    let entries = h.engine.list_history().unwrap();
    let entry = entries
        .iter()
        .find(|e| e.id == rec.run_id)
        .expect("run is in the merged history");
    assert_eq!(entry.kind, HistoryKind::Refresh);
    assert_eq!(entry.status, RunStatus::Succeeded);
    assert_eq!(entry.target.as_deref(), Some("app on local"));
    // the row carries what the run view needs, and the backup it could restore
    assert_eq!(entry.workflow_id.as_ref(), Some(&rec.workflow_id));
    let offer = entry
        .backup
        .as_ref()
        .expect("the run's backup is restorable");
    assert_eq!(
        offer.connection.name, "local",
        "restore onto the target server"
    );
    assert_eq!(offer.database, "app", "the dump holds the source database");
    assert_eq!(offer.path, rec.backup.as_ref().unwrap().path);
    // events: started … finished, one start/finish per executed step, swap phases in order
    let kinds = h.event_kinds();
    assert_eq!(kinds.first().map(String::as_str), Some("run_started"));
    assert_eq!(kinds.last().map(String::as_str), Some("run_finished"));
    assert_eq!(kinds.iter().filter(|k| *k == "step_started").count(), 6);
    assert_eq!(kinds.iter().filter(|k| *k == "step_finished").count(), 6);
    assert!(kinds.iter().any(|k| k == "progress") && kinds.iter().any(|k| k == "output"));
    let phases: Vec<SwapPhase> = h
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            RunEvent::SwapPhase { phase, .. } => Some(*phase),
            _ => None,
        })
        .collect();
    assert_eq!(
        phases,
        vec![
            SwapPhase::Quiescing,
            SwapPhase::CheckingPreparedTransactions,
            SwapPhase::TerminatingSessions,
            SwapPhase::RenamingTargetToOld,
            SwapPhase::RenamingStagingToTarget,
            SwapPhase::DroppingOld,
            SwapPhase::Done
        ]
    );
    // order of renames: target → _old, then staging → target
    let calls = h.fake.calls.lock().unwrap().clone();
    let renames: Vec<_> = calls
        .iter()
        .filter(|c| c.starts_with("rename"))
        .cloned()
        .collect();
    assert_eq!(
        renames,
        vec![
            "rename app app_old".to_string(),
            format!("rename {staging} app")
        ]
    );
}

#[tokio::test]
async fn create_mode_renames_staging_into_place() {
    let h = Harness::new();
    h.fake.server.lock().unwrap().dbs.remove("app");
    let plan = h.plan(ReplaceStrategy::StagingSwap);
    assert!(plan.create_mode);
    let rec = h.run(plan).await;
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert!(rec.create_mode);
    assert_eq!(rec.renames_completed, 2);
    assert!(rec.old_db.is_none());
    assert_eq!(h.fake.calls_matching("rename"), 1);
    assert!(h.fake.has_db("app"));
    assert_eq!(step_status(&rec, StepId::PostRestore), StepStatus::Skipped);
}

#[tokio::test]
async fn drop_first_runs_without_swap_substeps() {
    let h = Harness::new();
    let rec = h.run(h.plan(ReplaceStrategy::DropFirst)).await;
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert!(rec.step(StepId::Swap).is_none());
    assert_eq!(step_status(&rec, StepId::DropTarget), StepStatus::Succeeded);
    assert_eq!(h.fake.calls_matching("rename"), 0);
    assert_eq!(
        h.fake.calls_matching("restore app"),
        1,
        "restored straight into the target name"
    );
    assert!(h.fake.has_db("app"));
    assert_eq!(
        h.fake.comment("app"),
        None,
        "work-target marker cleared on success"
    );
    assert!(rec.staging_db.is_none());
}

#[tokio::test]
async fn dump_failure_leaves_no_backup_and_offers_redump() {
    let h = Harness::new();
    h.fake.fail(
        Point::Dump,
        AppError::new(ErrorKind::Network, "pg_dump lost the server"),
    );
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.status, RunStatus::FailedAtStep);
    assert_eq!(rec.failed_step, Some(StepId::Dump));
    assert!(rec.backup.is_none());
    assert_eq!(
        step_status(&rec, StepId::CreateDatabase),
        StepStatus::Pending
    );
    assert_eq!(rec.headline.as_deref(), Some("app is untouched."));
    assert_eq!(
        h.fake.comment("app").as_deref(),
        Some("my app"),
        "target untouched"
    );
    let r = retry_offer(&rec, StepId::Dump).unwrap();
    assert!(r.enabled);
    assert!(!r.retry.as_ref().unwrap().reuses_backup);
    assert_eq!(
        kinds(&rec),
        vec![ActionKind::Retry],
        "no staging to clean up"
    );
}

#[tokio::test]
async fn redump_retry_disabled_while_source_unreachable() {
    let h = Harness::new();
    h.fake.fail(
        Point::Dump,
        AppError::new(ErrorKind::Network, "pg_dump lost the server"),
    );
    h.fake
        .source_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    let r = retry_offer(&rec, StepId::Dump).unwrap();
    assert!(!r.enabled);
    assert!(r
        .disabled_reason
        .as_ref()
        .unwrap()
        .contains("source server can't be reached"));
}

#[tokio::test]
async fn create_staging_failure_drops_partial_and_retries_with_backup() {
    let h = Harness::new();
    h.fake.fail(
        Point::SetComment,
        AppError::new(ErrorKind::Sql, "comment failed"),
    );
    let plan = h.plan(ReplaceStrategy::StagingSwap);
    let staging = plan.staging_db.clone();
    let rec = h.run(plan).await;
    assert_eq!(rec.status, RunStatus::FailedAtStep);
    assert_eq!(rec.failed_step, Some(StepId::CreateDatabase));
    assert!(!h.fake.has_db(&staging), "partial staging dropped");
    assert_eq!(rec.work_db_state, WorkDbState::Absent);
    let r = retry_offer(&rec, StepId::CreateDatabase).unwrap();
    assert!(r.enabled, "{:?}", r.disabled_reason);
    let o = r.retry.as_ref().unwrap();
    assert!(o.reuses_backup && o.begins_at == StepId::CreateDatabase);
}

#[tokio::test]
async fn restore_failure_keeps_target_and_retry_reuses_backup() {
    let h = Harness::new();
    h.fake.fail(Point::Restore, object_error());
    let plan = h.plan(ReplaceStrategy::StagingSwap);
    let staging = plan.staging_db.clone();
    let rec = h.run(plan).await;

    assert_eq!(rec.status, RunStatus::FailedAtStep);
    assert_eq!(rec.failed_step, Some(StepId::Restore));
    assert_eq!(rec.failed_items.len(), 1);
    assert_eq!(
        rec.failed_items[0].object.as_deref(),
        Some("INDEX public.t_idx")
    );
    assert!(rec
        .error
        .as_ref()
        .unwrap()
        .message
        .contains("INDEX public.t_idx"));
    assert_eq!(rec.step(StepId::Restore).unwrap().exit_code, Some(1));
    assert!(!rec.step(StepId::Restore).unwrap().output_tail.is_empty());
    assert_eq!(rec.work_db_state, WorkDbState::Partial);
    assert_eq!(
        h.fake.comment("app").as_deref(),
        Some("my app"),
        "target untouched"
    );
    assert_eq!(h.fake.calls_matching("rename"), 0);
    assert!(
        h.fake.has_db(&staging),
        "partial staging kept for inspection/retry"
    );
    assert_eq!(rec.headline.as_deref(), Some("app is untouched."));
    assert!(rec.backup.as_ref().unwrap().reusable);

    let natural = retry_offer(&rec, StepId::Restore).unwrap();
    assert!(natural.enabled);
    let o = natural.retry.as_ref().unwrap();
    assert_eq!(
        (o.begins_at, o.reuses_backup, o.reuses_work_db),
        (StepId::CreateDatabase, true, false)
    );
    assert!(retry_offer(&rec, StepId::Dump).is_some_and(|a| a.enabled));
    assert!(enabled_kinds(&rec).contains(&ActionKind::DropStaging));
    assert!(enabled_kinds(&rec).contains(&ActionKind::KeepStaging));

    // retry from restore reuses the backup and succeeds
    let retry = retry_plan(&h, &rec, StepId::Restore);
    let rec2 = h.run(retry).await;
    assert_eq!(rec2.status, RunStatus::Succeeded, "{:?}", rec2.error);
    assert_eq!(rec2.retry_of.as_ref(), Some(&rec.run_id));
    assert_eq!(step_status(&rec2, StepId::Dump), StepStatus::Reused);
    assert_eq!(
        rec2.backup.as_ref().unwrap().path,
        rec.backup.as_ref().unwrap().path
    );
    assert_eq!(
        rec2.backup.as_ref().unwrap().from_run.as_ref(),
        Some(&rec.run_id)
    );
    assert_eq!(h.fake.calls_matching("dump"), 1, "no second dump");
    assert!(
        !h.fake.has_db(&staging),
        "the failed run's partial staging was dropped"
    );
    // the original record now has nothing left to drop
    let rec = h.engine.reinventory(&rec.run_id).await.unwrap();
    assert!(!kinds(&rec).contains(&ActionKind::DropStaging));
}

#[tokio::test]
async fn archive_corrupt_marks_backup_non_reusable_and_redirects_to_redump() {
    let h = Harness::new();
    h.fake.fail(
        Point::Restore,
        AppError::new(
            ErrorKind::ToolFailed,
            "The backup file is unreadable or truncated",
        )
        .with_data(serde_json::json!({ "cause": "archive_corrupt", "exit_code": 1 })),
    );
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    let b = rec.backup.as_ref().unwrap();
    assert!(!b.reusable);
    assert_eq!(b.cause, Some(pgwhy_pgtools::FailureCause::ArchiveCorrupt));
    let natural = retry_offer(&rec, StepId::Restore).unwrap();
    assert!(!natural.enabled);
    assert!(natural
        .disabled_reason
        .as_ref()
        .unwrap()
        .contains("can't be reused"));
    let redump = rec
        .actions
        .iter()
        .find(|a| {
            a.retry
                .as_ref()
                .is_some_and(|r| r.begins_at == StepId::Dump)
        })
        .unwrap();
    assert!(redump.enabled);
}

#[tokio::test]
async fn backup_deleted_on_disk_disables_reuse_with_reason() {
    let h = Harness::new();
    h.fake.fail(Point::Restore, object_error());
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    std::fs::remove_file(&rec.backup.as_ref().unwrap().path).unwrap();
    let rec = h.engine.reinventory(&rec.run_id).await.unwrap();
    let natural = retry_offer(&rec, StepId::Restore).unwrap();
    assert!(!natural.enabled);
    assert!(natural
        .disabled_reason
        .as_ref()
        .unwrap()
        .contains("no longer exists"));
}

#[tokio::test]
async fn post_restore_failure_txn_mode_keeps_clean_staging() {
    let h = Harness::new();
    h.fake.fail(
        Point::Script,
        AppError::new(ErrorKind::Sql, "statement 1 failed: boom"),
    );
    let mut plan = h.plan(ReplaceStrategy::StagingSwap);
    Harness::with_script(&mut plan, ScriptMode::Transaction);
    let rec = h.run(plan).await;
    assert_eq!(rec.failed_step, Some(StepId::PostRestore));
    assert_eq!(
        rec.work_db_state,
        WorkDbState::Restored,
        "rolled back: staging clean"
    );
    let o = retry_offer(&rec, StepId::PostRestore).unwrap();
    assert!(o.enabled);
    let r = o.retry.as_ref().unwrap();
    assert_eq!((r.begins_at, r.reuses_work_db), (StepId::PostRestore, true));

    // retry with an edited script runs the new content against the kept staging
    let mut retry = retry_plan(&h, &rec, StepId::PostRestore);
    Harness::with_script(&mut retry, ScriptMode::Transaction);
    retry.script.as_mut().unwrap().sha256 = "def456".into();
    let rec2 = h.run(retry).await;
    assert_eq!(rec2.status, RunStatus::Succeeded, "{:?}", rec2.error);
    assert_eq!(rec2.script.as_ref().unwrap().sha256, "def456");
    assert_eq!(
        rec.script.as_ref().unwrap().sha256,
        "abc123",
        "original snapshot stays in the failed run"
    );
    assert_eq!(step_status(&rec2, StepId::Restore), StepStatus::Reused);
    assert_eq!(
        h.fake.calls_matching(&format!(
            "script {} def456",
            rec.staging_db.as_ref().unwrap()
        )),
        1
    );
}

#[tokio::test]
async fn post_restore_failure_no_txn_mode_requires_rerestore() {
    let h = Harness::new();
    h.fake.fail(
        Point::Script,
        AppError::new(ErrorKind::Sql, "statement 3 failed"),
    );
    let mut plan = h.plan(ReplaceStrategy::StagingSwap);
    Harness::with_script(&mut plan, ScriptMode::NoTransaction);
    let rec = h.run(plan).await;
    assert_eq!(rec.work_db_state, WorkDbState::Mutated);
    let r = retry_offer(&rec, StepId::PostRestore)
        .unwrap()
        .retry
        .clone()
        .unwrap();
    assert_eq!(
        (r.begins_at, r.reuses_work_db, r.reuses_backup),
        (StepId::CreateDatabase, false, true)
    );
}

#[tokio::test]
async fn verify_failure_offers_reverify() {
    let h = Harness::new();
    h.fake.fail(
        Point::Verify,
        AppError::new(
            ErrorKind::Sql,
            "verification failed: app_staging has no user tables",
        ),
    );
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.failed_step, Some(StepId::Verify));
    let r = retry_offer(&rec, StepId::Verify)
        .unwrap()
        .retry
        .clone()
        .unwrap();
    assert_eq!((r.begins_at, r.reuses_work_db), (StepId::Verify, true));
    let rec2 = h.run(retry_plan(&h, &rec, StepId::Verify)).await;
    assert_eq!(rec2.status, RunStatus::Succeeded);
    assert_eq!(
        step_status(&rec2, StepId::CreateDatabase),
        StepStatus::Reused
    );
}

#[tokio::test]
async fn prepared_transactions_refuse_the_swap_before_any_rename() {
    let h = Harness::new();
    h.fake.server.lock().unwrap().prepared.push(PreparedXact {
        gid: "tx-77".into(),
        database: "app".into(),
        owner: "app".into(),
        prepared_at: "now".into(),
    });
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.status, RunStatus::FailedAtSwap);
    assert_eq!(rec.renames_completed, 0);
    assert!(rec.error.as_ref().unwrap().message.contains("tx-77"));
    assert_eq!(h.fake.calls_matching("rename"), 0);
    // pre-rename swap failure: retry swap, never rollback
    let k = kinds(&rec);
    assert!(!k.contains(&ActionKind::RollbackSwap) && !k.contains(&ActionKind::CompleteSwap));
    let r = retry_offer(&rec, StepId::Swap).unwrap();
    assert!(r.enabled);
    assert_eq!(r.retry.as_ref().unwrap().begins_at, StepId::Swap);
    assert_eq!(
        h.fake
            .quiesce_active
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "quiesce released"
    );
}

#[tokio::test]
async fn reconnecting_client_fails_swap_retryably_naming_it() {
    let h = Harness::new();
    h.fake.add_session("app", 999, "AutoReconnectApp", true);
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.status, RunStatus::FailedAtSwap);
    let e = rec.error.as_ref().unwrap();
    assert_eq!(e.class, ErrorClass::Retriable);
    assert!(
        e.message.contains("pid 999") && e.message.contains("AutoReconnectApp"),
        "{}",
        e.message
    );
    assert!(h.fake.has_db("app"), "target intact under its own name");
    assert_eq!(rec.renames_completed, 0);
    assert!(rec
        .inventory
        .as_ref()
        .unwrap()
        .sessions
        .iter()
        .any(|s| s.pid == 999));
}

#[tokio::test]
async fn occupied_old_name_refuses_and_unmarked_is_never_dropped() {
    let h = Harness::new();
    h.fake.server.lock().unwrap().dbs.insert(
        "app_old".into(),
        support::FakeDb {
            comment: Some("real data".into()),
            sessions: vec![],
        },
    );
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.status, RunStatus::FailedAtSwap);
    assert!(rec
        .error
        .as_ref()
        .unwrap()
        .message
        .contains("may be a real database"));
    let drop_old = rec.action(ActionKind::DropOld).unwrap();
    assert!(!drop_old.enabled);
    assert!(drop_old
        .disabled_reason
        .as_ref()
        .unwrap()
        .contains("not marked by pgWhy"));
    assert!(h
        .engine
        .perform(&rec.run_id, ActionKind::DropOld)
        .await
        .is_err());
    assert!(h.fake.has_db("app_old"));
}

#[tokio::test]
async fn second_rename_failure_leaves_only_complete_or_rollback() {
    let h = Harness::new();
    h.fake.fail(
        Point::Rename(2),
        AppError::new(ErrorKind::Sql, "rename failed"),
    );
    let plan = h.plan(ReplaceStrategy::StagingSwap);
    let staging = plan.staging_db.clone();
    let rec = h.run(plan).await;
    assert_eq!(rec.status, RunStatus::FailedAtSwap);
    assert_eq!(rec.renames_completed, 1);
    assert!(!h.fake.has_db("app") && h.fake.has_db("app_old") && h.fake.has_db(&staging));
    let mut k = kinds(&rec);
    k.sort_by_key(|k| format!("{k:?}"));
    assert_eq!(k, vec![ActionKind::CompleteSwap, ActionKind::RollbackSwap]);
    assert!(rec
        .headline
        .as_ref()
        .unwrap()
        .starts_with("Outage: app does not exist"));
    // _old carries a self-describing marker
    let m = Marker::parse(&h.fake.comment("app_old").unwrap()).unwrap();
    assert_eq!(
        (m.role, m.original_comment.as_deref()),
        (MarkerRole::Old, Some("my app"))
    );
    // drop actions are refused in the window
    assert!(h
        .engine
        .perform(&rec.run_id, ActionKind::DropStaging)
        .await
        .is_err());

    let rec = h
        .engine
        .perform(&rec.run_id, ActionKind::RollbackSwap)
        .await
        .unwrap();
    assert!(h.fake.has_db("app") && !h.fake.has_db("app_old"));
    assert_eq!(
        h.fake.comment("app").as_deref(),
        Some("my app"),
        "original comment restored"
    );
    assert_eq!(rec.renames_completed, 0);
    assert!(rec.headline.as_deref() == Some("app is untouched."));
    assert!(enabled_kinds(&rec).contains(&ActionKind::DropStaging));
    assert!(retry_offer(&rec, StepId::Swap).is_some_and(|a| a.enabled));
}

#[tokio::test]
async fn kill_between_renames_then_recover_by_completing() {
    let h = Harness::new();
    h.fake.hang(Point::Rename(2));
    let plan = h.plan(ReplaceStrategy::StagingSwap);
    let staging = plan.staging_db.clone();
    let id = h.engine.start_plan(plan, h.sink()).unwrap();
    // wait until the record shows the first rename done
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r = h.engine.get_run(&id).unwrap();
        if r.renames_completed == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never reached renames_completed=1"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let err = h.engine.cancel(&id).unwrap_err();
    assert!(err.message.contains("between its two renames"));
    h.engine.shutdown(Duration::from_millis(200)).await;

    let rec = h.engine.get_run(&id).unwrap();
    assert_eq!(rec.status, RunStatus::Interrupted);
    assert_eq!(rec.renames_completed, 1);
    assert_eq!(rec.swap_phase, Some(SwapPhase::RenamingStagingToTarget));
    assert_eq!(step_status(&rec, StepId::Swap), StepStatus::Interrupted);
    // recorded-state actions (no inventory yet) are already complete-or-rollback only
    let mut k = kinds(&rec);
    k.sort_by_key(|k| format!("{k:?}"));
    assert_eq!(k, vec![ActionKind::CompleteSwap, ActionKind::RollbackSwap]);

    // "restart": a new engine over the same history re-inventories live
    let fresh = Engine::with_ops(h.engine.config().clone(), h.fake.clone(), None);
    assert!(fresh.mark_stale_interrupted().unwrap().is_empty());
    let rec = fresh.reinventory(&id).await.unwrap();
    let inv = rec.inventory.as_ref().unwrap();
    assert!(
        !inv.target.exists
            && inv.old.as_ref().unwrap().exists
            && inv.staging.as_ref().unwrap().owned_by_run
    );
    assert_eq!(enabled_kinds(&rec).len(), 2);
    let rec = fresh.perform(&id, ActionKind::CompleteSwap).await.unwrap();
    assert_eq!(rec.status, RunStatus::Succeeded);
    assert_eq!(rec.renames_completed, 2);
    assert!(h.fake.has_db("app") && !h.fake.has_db("app_old") && !h.fake.has_db(&staging));
    assert!(rec.actions.is_empty());
}

#[tokio::test]
async fn unreachable_server_during_recovery_disables_every_action() {
    let h = Harness::new();
    h.fake.fail(
        Point::Rename(2),
        AppError::new(ErrorKind::Sql, "rename failed"),
    );
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    h.fake
        .maint_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let rec = h.engine.reinventory(&rec.run_id).await.unwrap();
    assert!(!rec.inventory.as_ref().unwrap().reachable);
    assert_eq!(rec.actions.len(), 2, "still complete/rollback only");
    assert!(rec.actions.iter().all(|a| !a.enabled
        && a.disabled_reason
            .as_ref()
            .unwrap()
            .contains("can't be reached")));
}

#[tokio::test]
async fn failed_old_drop_is_success_with_cleanup() {
    let h = Harness::new();
    *h.fake.undroppable.lock().unwrap() = Some("app_old".into());
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.status, RunStatus::SucceededCleanupRequired);
    assert_eq!(rec.renames_completed, 2);
    assert_eq!(enabled_kinds(&rec), vec![ActionKind::DropOld]);
    assert!(!kinds(&rec).contains(&ActionKind::RollbackSwap));
    *h.fake.undroppable.lock().unwrap() = None;
    let rec = h
        .engine
        .perform(&rec.run_id, ActionKind::DropOld)
        .await
        .unwrap();
    assert_eq!(rec.status, RunStatus::Succeeded);
    assert!(!h.fake.has_db("app_old"));
}

#[tokio::test]
async fn cancel_mid_restore_keeps_target_and_records_the_run() {
    let h = Harness::new();
    h.fake.hang(Point::Restore);
    let id = h
        .engine
        .start_plan(h.plan(ReplaceStrategy::StagingSwap), h.sink())
        .unwrap();
    while h.fake.calls_matching("restore") == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    h.engine.cancel(&id).unwrap();
    let rec = h.engine.wait(&id).await.unwrap();
    assert_eq!(rec.status, RunStatus::Cancelled);
    assert_eq!(step_status(&rec, StepId::Restore), StepStatus::Cancelled);
    assert_eq!(h.fake.comment("app").as_deref(), Some("my app"));
    assert!(enabled_kinds(&rec).contains(&ActionKind::DropStaging));
    assert!(enabled_kinds(&rec).contains(&ActionKind::KeepStaging));
    let rec = h
        .engine
        .perform(&id, ActionKind::KeepStaging)
        .await
        .unwrap();
    assert!(rec.staging_kept);
    assert!(!kinds(&rec).contains(&ActionKind::KeepStaging));
    let rec = h
        .engine
        .perform(&id, ActionKind::DropStaging)
        .await
        .unwrap();
    assert!(!h.fake.has_db(rec.staging_db.as_ref().unwrap()));
}

#[tokio::test]
async fn quit_mid_restore_is_interrupted() {
    let h = Harness::new();
    h.fake.hang(Point::Restore);
    let id = h
        .engine
        .start_plan(h.plan(ReplaceStrategy::StagingSwap), h.sink())
        .unwrap();
    while h.fake.calls_matching("restore") == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    h.engine.shutdown(Duration::from_secs(5)).await;
    let rec = h.engine.get_run(&id).unwrap();
    assert_eq!(rec.status, RunStatus::Interrupted);
    assert_eq!(rec.renames_completed, 0);
    assert_eq!(step_status(&rec, StepId::Restore), StepStatus::Interrupted);
    assert!(
        h.engine
            .start_plan(h.plan(ReplaceStrategy::StagingSwap), h.sink())
            .is_err(),
        "no new runs while quitting"
    );
}

#[tokio::test]
async fn staging_unmarked_by_the_user_is_never_offered_for_drop() {
    let h = Harness::new();
    h.fake.fail(Point::Restore, object_error());
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    let staging = rec.staging_db.clone().unwrap();
    h.fake
        .server
        .lock()
        .unwrap()
        .dbs
        .get_mut(&staging)
        .unwrap()
        .comment = Some("I took this over".into());
    let rec = h.engine.reinventory(&rec.run_id).await.unwrap();
    let d = rec.action(ActionKind::DropStaging).unwrap();
    assert!(!d.enabled);
    assert!(d
        .disabled_reason
        .as_ref()
        .unwrap()
        .contains("may be a real database"));
}

#[tokio::test]
async fn drop_first_restore_failure_leads_with_no_working_database() {
    let h = Harness::new();
    h.fake.fail(Point::Restore, object_error());
    let rec = h.run(h.plan(ReplaceStrategy::DropFirst)).await;
    assert_eq!(rec.status, RunStatus::FailedAtStep);
    assert!(rec
        .headline
        .as_ref()
        .unwrap()
        .starts_with("You currently have no working local database"));
    let k = kinds(&rec);
    assert!(
        !k.contains(&ActionKind::DropStaging) && !k.contains(&ActionKind::KeepStaging),
        "no staging actions for drop-first"
    );
    let r = retry_offer(&rec, StepId::Restore).unwrap();
    assert!(r.enabled);
    assert_eq!(r.retry.as_ref().unwrap().begins_at, StepId::CreateDatabase);
}

#[tokio::test]
async fn run_lock_refuses_a_second_run_on_the_same_server() {
    let h = Harness::new();
    h.fake.hang(Point::Restore);
    let id = h
        .engine
        .start_plan(h.plan(ReplaceStrategy::StagingSwap), h.sink())
        .unwrap();
    let err = h
        .engine
        .start_plan(h.plan(ReplaceStrategy::StagingSwap), h.sink())
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Refused);
    assert!(err.message.contains("already using the server"));
    h.engine.cancel(&id).unwrap();
    h.engine.wait(&id).await.unwrap();
    // released at run end
    h.fake.unhang(Point::Restore);
    assert_eq!(
        h.run(h.plan(ReplaceStrategy::StagingSwap)).await.status,
        RunStatus::Succeeded
    );
}

#[tokio::test]
async fn stale_running_records_become_interrupted_at_startup() {
    let h = Harness::new();
    h.fake.hang(Point::Rename(1));
    let id = h
        .engine
        .start_plan(h.plan(ReplaceStrategy::StagingSwap), h.sink())
        .unwrap();
    while h.fake.calls_matching("rename") == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // simulate a crash: a second engine over the same history sees a `running` record
    let fresh = Engine::with_ops(h.engine.config().clone(), h.fake.clone(), None);
    let stale = fresh.mark_stale_interrupted().unwrap();
    assert_eq!(stale, vec![id.clone()]);
    let rec = fresh.get_run(&id).unwrap();
    assert_eq!(rec.status, RunStatus::Interrupted);
    assert_eq!(rec.swap_phase, Some(SwapPhase::RenamingTargetToOld));
    // stopped *at* rename 1 with the server unscanned: conservatively complete/rollback only
    assert_eq!(rec.actions.len(), 2);
    // live scan shows the rename never happened → ordinary pre-rename state
    let rec = fresh.reinventory(&id).await.unwrap();
    assert!(rec.inventory.as_ref().unwrap().target.exists);
    assert!(retry_offer(&rec, StepId::Swap).is_some_and(|a| a.enabled));
    assert!(!kinds(&rec).contains(&ActionKind::RollbackSwap));
}

#[tokio::test]
async fn new_backup_failing_validation_fails_the_dump() {
    let h = Harness::new();
    *h.fake.verdict.lock().unwrap() = Some(BackupVerdict {
        reusable: false,
        reason: Some("truncated".into()),
        cause: None,
        toc_items: None,
        dumped_by: None,
    });
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.failed_step, Some(StepId::Dump));
    assert!(!rec.backup.as_ref().unwrap().reusable);
}

#[test]
fn retention_protects_retryable_runs_only() {
    let mk = |id: &str, status: RunStatus, started: &str, path: &str| {
        let mut r: RunRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "run_id": id, "workflow_id": "wf", "workflow_name": "w", "started_at": started,
            "status": status, "strategy": "staging_swap", "create_mode": false,
            "source": {"connection_id":"s","connection_name":"s","host":"h","port":1,"database":"d","user":"u","maintenance_db":"postgres","sslmode":"prefer","env_tag":"production","server_key":{"host":"h","port":1,"via":null}},
            "target": {"connection_id":"t","connection_name":"t","host":"h","port":2,"database":"d","user":"u","maintenance_db":"postgres","sslmode":"prefer","env_tag":"local","server_key":{"host":"h","port":2,"via":null}},
            "work_db_state": "absent", "tools": {}, "options": {}, "steps": [], "renames_completed": 0,
            "credentials": {"source":"keyring","target":"none"}, "confirmation_fingerprint": "f"
        }))
        .unwrap();
        r.backup = Some(BackupRecord {
            path: path.into(),
            size_bytes: 1,
            dumped_by: None,
            reusable: true,
            reason: None,
            cause: None,
            toc_items: None,
            from_run: None,
        });
        r
    };
    let records = vec![
        mk("a", RunStatus::FailedAtStep, "2026-01-01", "/b/a.dump"),
        mk("b", RunStatus::Succeeded, "2026-01-02", "/b/b.dump"),
        mk("c", RunStatus::FailedAtStep, "2026-01-03", "/b/c.dump"),
    ];
    let p = retry::protected_backups(&records);
    assert!(
        p.contains(std::path::Path::new("/b/c.dump")),
        "latest failure is retryable"
    );
    assert!(
        !p.contains(std::path::Path::new("/b/a.dump")),
        "superseded by a later success"
    );
    assert!(!p.contains(std::path::Path::new("/b/b.dump")));
}

#[tokio::test]
async fn a_rebuilt_engine_shares_the_server_locks() {
    // A settings change rebuilds the engine while a drop or restore may hold a server's lock on
    // the old one: the new engine must see that lock, not start with an empty table.
    let h = Harness::new();
    let config = EngineConfig::new(h.dir.path().join("runs"), h.dir.path().join("elsewhere"));
    let rebuilt = h.engine.rebuilt_with_ops(config, h.fake.clone(), None);
    let server = pgwhy_core::config::ServerKey {
        host: "local".into(),
        port: 5001,
        via: None,
    };
    let guard = h
        .engine
        .locks()
        .try_acquire(&server, "the old engine's drop")
        .unwrap();
    let refused = rebuilt
        .locks()
        .try_acquire(&server, "a new restore")
        .unwrap_err();
    assert!(
        refused.message.contains("the old engine's drop"),
        "{}",
        refused.message
    );
    drop(guard);
    rebuilt
        .locks()
        .try_acquire(&server, "a new restore")
        .unwrap();
}

#[tokio::test]
async fn reinventory_never_writes_while_someone_holds_the_server() {
    // The scan runs unlocked; a recovery action may save the record meanwhile. With the server
    // lock held elsewhere, reinventory must return its view without overwriting the record.
    let h = Harness::new();
    h.fake.fail(Point::Restore, object_error());
    let rec = h.run(h.plan(ReplaceStrategy::StagingSwap)).await;
    assert_eq!(rec.status, RunStatus::FailedAtStep);

    // "perform" holds the lock and has just saved its own version of the record
    let _guard = h
        .engine
        .locks()
        .try_acquire(&rec.target.server_key, "a cleanup in progress")
        .unwrap();
    let mut theirs = h.engine.history().load(&rec.run_id).unwrap();
    theirs.warnings.push("saved by the cleanup".into());
    theirs.inventory = None;
    h.engine.history().save(&theirs).unwrap();

    let view = h.engine.reinventory(&rec.run_id).await.unwrap();
    assert!(
        view.inventory.is_some(),
        "the caller still gets a fresh inventory"
    );
    let on_disk = h.engine.history().load(&rec.run_id).unwrap();
    assert!(on_disk.warnings.iter().any(|w| w == "saved by the cleanup"));
    assert!(
        on_disk.inventory.is_none(),
        "nothing was written over the cleanup's save"
    );

    // once the lock is free, the inventory lands on the latest record, keeping its changes
    drop(_guard);
    h.engine.reinventory(&rec.run_id).await.unwrap();
    let on_disk = h.engine.history().load(&rec.run_id).unwrap();
    assert!(on_disk.inventory.is_some());
    assert!(on_disk.warnings.iter().any(|w| w == "saved by the cleanup"));
}
