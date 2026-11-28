//! Recovery of a standalone replace-restore whose swap stopped between its renames, and what
//! quitting does to operations in flight.

mod support;

use pgwhy_core::{AppError, ErrorKind};
use pgwhy_pgtools::toc::TocHeader;
use pgwhy_workflow::staging::{Marker, MarkerRole};
use pgwhy_workflow::*;
use std::path::PathBuf;
use std::time::Duration;
use support::*;

fn request(h: &Harness, db: &str, mode: RestoreMode) -> RestoreRequest {
    RestoreRequest {
        connection: h.target.clone(),
        database: db.into(),
        archive: PathBuf::from("/tmp/app.dump"),
        inspection: pgwhy_pgtools::ArchiveInspection {
            path: "/tmp/app.dump".into(),
            size_bytes: Some(4096),
            reusable: true,
            reason: None,
            cause: None,
            toc: Some(pgwhy_pgtools::Toc {
                header: TocHeader::default(),
                entries: Vec::new(),
            }),
        },
        mode,
        locale: None,
        settings: RestoreSettings::default(),
        typed_name: Some(db.into()),
        backup: None,
        tool: fake_tool(),
        password: None,
        on_event: None,
    }
}

/// A replace whose second rename and its automatic rollback both fail: the target is parked as
/// `app_old`, the verified copy sits in staging, and the record says one rename is done.
async fn interrupted_swap(h: &Harness) -> (OperationRecord, String) {
    h.fake.fail(
        Point::Rename(2),
        AppError::new(ErrorKind::Permission, "must be owner of database app"),
    );
    h.fake.fail(
        Point::Rename(3),
        AppError::new(ErrorKind::Permission, "must be owner of database app_old"),
    );
    h.engine
        .restore(request(h, "app", RestoreMode::Replace))
        .await
        .unwrap_err();
    let rec = h.engine.list_operations().unwrap().remove(0);
    let staging = rec.swap_state().unwrap().staging_db.clone().unwrap();
    (rec, staging)
}

fn kinds(a: &[ActionOffer]) -> Vec<(ActionKind, bool)> {
    a.iter().map(|a| (a.kind, a.enabled)).collect()
}

#[tokio::test]
async fn an_interrupted_replace_can_be_completed() {
    let h = Harness::new();
    let (rec, staging) = interrupted_swap(&h).await;
    assert!(!h.fake.has_db("app") && h.fake.has_db("app_old"));

    let offers = h.engine.operation_actions(&rec.id).await.unwrap();
    assert_eq!(
        kinds(&offers),
        vec![
            (ActionKind::CompleteSwap, true),
            (ActionKind::RollbackSwap, true)
        ]
    );

    let done = h
        .engine
        .perform_operation(&rec.id, ActionKind::CompleteSwap)
        .await
        .unwrap();
    assert_eq!(done.status, RunStatus::Succeeded);
    assert!(h.fake.has_db("app"), "the restored copy is the target");
    assert_eq!(h.fake.comment("app"), None, "pgWhy's marker is cleared");
    assert!(!h.fake.has_db(&staging) && !h.fake.has_db("app_old"));
    let swap = done.swap_state().unwrap();
    assert_eq!(swap.renames_completed, 2);
    assert_eq!(swap.work_db_state, WorkDbState::SwappedIn);
    // persisted, and nothing is left to offer
    assert_eq!(h.engine.get_operation(&rec.id).unwrap(), done);
    assert!(h
        .engine
        .operation_actions(&rec.id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_interrupted_replace_can_be_rolled_back_and_its_staging_dropped() {
    let h = Harness::new();
    let (rec, staging) = interrupted_swap(&h).await;

    let done = h
        .engine
        .perform_operation(&rec.id, ActionKind::RollbackSwap)
        .await
        .unwrap();
    assert!(h.fake.has_db("app") && !h.fake.has_db("app_old"));
    assert_eq!(
        h.fake.comment("app").as_deref(),
        Some("my app"),
        "the original comment is back"
    );
    assert_eq!(done.swap_state().unwrap().renames_completed, 0);
    assert!(h.fake.has_db(&staging), "the restored copy is kept");

    // the kept staging copy is now a leftover that can be dropped, and nothing else is offered
    let offers = h.engine.operation_actions(&rec.id).await.unwrap();
    assert_eq!(kinds(&offers), vec![(ActionKind::DropStaging, true)]);
    let done = h
        .engine
        .perform_operation(&rec.id, ActionKind::DropStaging)
        .await
        .unwrap();
    assert!(!h.fake.has_db(&staging));
    assert!(done.swap_state().unwrap().staging_db.is_none());
    assert!(h.fake.has_db("app"));
}

#[tokio::test]
async fn completing_refuses_a_staging_database_that_is_not_the_operations() {
    let h = Harness::new();
    let (rec, staging) = interrupted_swap(&h).await;

    // someone replaced the staging database's marker with another run's
    let foreign = Marker::new(MarkerRole::Staging, "someone-else", "wf", "app").to_comment();
    h.fake
        .server
        .lock()
        .unwrap()
        .dbs
        .get_mut(&staging)
        .unwrap()
        .comment = Some(foreign);

    let offers = h.engine.operation_actions(&rec.id).await.unwrap();
    let complete = offers
        .iter()
        .find(|a| a.kind == ActionKind::CompleteSwap)
        .unwrap();
    assert!(!complete.enabled, "{complete:?}");
    let e = h
        .engine
        .perform_operation(&rec.id, ActionKind::CompleteSwap)
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert!(
        h.fake.has_db(&staging) && !h.fake.has_db("app"),
        "nothing moved"
    );
    // rolling back is still possible: the previous data is safe under app_old
    assert!(offers
        .iter()
        .any(|a| a.kind == ActionKind::RollbackSwap && a.enabled));
}

#[tokio::test]
async fn an_unreachable_server_disables_recovery_and_other_operations_offer_nothing() {
    let h = Harness::new();
    let (rec, _) = interrupted_swap(&h).await;
    h.fake
        .maint_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let offers = h.engine.operation_actions(&rec.id).await.unwrap();
    assert!(!offers.is_empty() && offers.iter().all(|a| !a.enabled));
    assert!(h
        .engine
        .perform_operation(&rec.id, ActionKind::CompleteSwap)
        .await
        .is_err());
    h.fake
        .maint_down
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // a restore into a new database has no swap to recover
    let plain = h
        .engine
        .restore(request(&h, "fresh", RestoreMode::NewDatabase))
        .await
        .unwrap();
    assert!(h
        .engine
        .operation_actions(&plain.id)
        .await
        .unwrap()
        .is_empty());
    assert!(h
        .engine
        .perform_operation(&plain.id, ActionKind::CompleteSwap)
        .await
        .is_err());
    // retry is not an operation action
    assert!(h
        .engine
        .perform_operation(&rec.id, ActionKind::Retry)
        .await
        .is_err());
}

#[tokio::test]
async fn a_replace_stamps_its_staging_database_with_its_own_marker() {
    let h = Harness::new();
    let (rec, staging) = interrupted_swap(&h).await;
    let m = Marker::parse(&h.fake.comment(&staging).unwrap()).unwrap();
    assert_eq!(m.role, MarkerRole::Staging);
    assert_eq!(m.run_id, rec.id.0);
    assert_eq!(m.original_target, "app");
}

#[tokio::test]
async fn quitting_cancels_a_running_restore_and_waits_for_it() {
    let h = Harness::new();
    h.fake.hang(Point::Restore);
    let engine = h.engine.clone();
    let req = request(&h, "fresh", RestoreMode::NewDatabase);
    let task = tokio::spawn(async move { engine.restore(req).await });
    // wait until the restore is registered
    for _ in 0..200 {
        if h.engine.has_active_operations() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(h.engine.has_active_operations());

    h.engine.shutdown(Duration::from_secs(5)).await;

    let e = task.await.unwrap().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled);
    assert!(!h.engine.has_active_operations());
    let rec = h.engine.list_operations().unwrap().remove(0);
    assert_eq!(rec.status, RunStatus::Interrupted);
}

#[tokio::test]
async fn quitting_cancels_a_drop_that_is_still_draining_sessions() {
    let h = Harness::new();
    // a client that reconnects forever keeps the drain polling
    h.fake.add_session("app", 777, "pool", true);
    let mut config = h.engine.config().clone();
    config.session_drain_timeout = Duration::from_secs(30);
    let engine = h.engine.rebuilt_with_ops(config, h.fake.clone(), None);
    let target = pgwhy_workflow::lifecycle::DbTarget {
        connection_id: h.target.id.clone(),
        connection_name: h.target.name.clone(),
        server: h.target.server_key(),
        env_tag: h.target.env_tag,
        database: "app".into(),
    };
    let e2 = engine.clone();
    let task = tokio::spawn(async move {
        e2.drop_database(DropDatabaseRequest {
            target,
            typed_name: "app".into(),
            backup: None,
        })
        .await
    });
    for _ in 0..200 {
        if engine.has_active_operations() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    engine.shutdown(Duration::from_secs(5)).await;
    let e = task.await.unwrap().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled);
    assert!(h.fake.has_db("app"), "nothing was dropped");
    let rec = engine.list_operations().unwrap().remove(0);
    assert_eq!(rec.status, RunStatus::Interrupted);
}
