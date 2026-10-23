//! Standalone restore over the scripted fake (v1.1 Epic 2): the three target modes, the
//! guardrails, and what a failure leaves behind.

mod support;

use pgwhy_core::config::EnvTag;
use pgwhy_core::ErrorKind;
use pgwhy_pgtools::toc::TocHeader;
use pgwhy_pgtools::Toc;
use pgwhy_workflow::*;
use std::path::PathBuf;
use std::sync::Arc;
use support::*;

fn inspection(dumped_by: &str, entries: usize) -> pgwhy_pgtools::ArchiveInspection {
    let toc = Toc {
        header: TocHeader {
            dbname: Some("app".into()),
            dumped_by: pgwhy_pgtools::PgVersion::parse(dumped_by),
            dumped_from: pgwhy_pgtools::PgVersion::parse("16.11"),
            ..Default::default()
        },
        entries: (0..entries)
            .map(|i| pgwhy_pgtools::TocEntry {
                dump_id: i as u32,
                table_oid: 0,
                oid: 0,
                desc: "TABLE DATA".into(),
                schema: Some("public".into()),
                rest: "t".into(),
                raw: String::new(),
            })
            .collect(),
    };
    pgwhy_pgtools::ArchiveInspection {
        path: "/tmp/app.dump".into(),
        size_bytes: Some(4096),
        reusable: true,
        reason: None,
        cause: None,
        toc: Some(toc),
    }
}

fn request(h: &Harness, db: &str, mode: RestoreMode) -> RestoreRequest {
    RestoreRequest {
        connection: h.target.clone(),
        database: db.into(),
        archive: PathBuf::from("/tmp/app.dump"),
        inspection: inspection("16.15", 3),
        mode,
        locale: None,
        settings: RestoreSettings::default(),
        typed_name: None,
        backup: None,
        tool: fake_tool(),
        password: None,
        on_event: None,
    }
}

#[tokio::test]
async fn new_database_creates_restores_and_verifies() {
    let h = Harness::new();
    let rec = h
        .engine
        .restore(request(&h, "restored_db", RestoreMode::NewDatabase))
        .await
        .unwrap();

    assert_eq!(rec.kind, OperationKind::Restore);
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert_eq!(
        rec.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["create", "restore", "verify"]
    );
    assert!(rec.steps.iter().all(|s| s.status == StepStatus::Succeeded));
    assert!(h.fake.has_db("restored_db"));
    // the restore ran against the database the create made
    assert!(h.fake.calls_matching("restore restored_db") >= 1);
    assert!(h.fake.calls_matching("verify restored_db") >= 1);
    // the record is in history and persisted
    assert_eq!(h.engine.get_operation(&rec.id).unwrap(), rec);
    let entries = h.engine.list_history().unwrap();
    assert!(entries
        .iter()
        .any(|e| e.id == rec.id && e.kind == HistoryKind::Restore));
    let detail = rec.restore().unwrap();
    assert_eq!(detail.mode, RestoreMode::NewDatabase);
    assert_eq!(detail.archive.toc.as_ref().unwrap().entries.len(), 3);
    assert!(detail.target.connection_name == "local");
}

#[tokio::test]
async fn a_failed_restore_into_a_new_database_drops_the_half_database() {
    let h = Harness::new();
    h.fake.fail(
        Point::Restore,
        pgwhy_core::AppError::new(ErrorKind::ToolFailed, "pg_restore failed on 1 item(s)")
            .with_data(serde_json::json!({ "failed_items": [{ "object": "INDEX public.t_idx" }] })),
    );
    let e = h
        .engine
        .restore(request(&h, "restored_db", RestoreMode::NewDatabase))
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::ToolFailed);
    assert!(
        !h.fake.has_db("restored_db"),
        "the half database was dropped"
    );

    let rec = &h.engine.list_operations().unwrap()[0];
    assert_eq!(rec.status, RunStatus::FailedAtStep);
    assert_eq!(rec.failed_step.as_deref(), Some("restore"));
    assert!(
        rec.warnings.iter().any(|w| w.contains("was dropped")),
        "{:?}",
        rec.warnings
    );
}

#[tokio::test]
async fn a_failed_verification_also_cleans_up_the_new_database() {
    let h = Harness::new();
    h.fake.fail(
        Point::Verify,
        pgwhy_core::AppError::new(ErrorKind::Sql, "verification failed: no user tables"),
    );
    let e = h
        .engine
        .restore(request(&h, "restored_db", RestoreMode::NewDatabase))
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Sql);
    assert!(!h.fake.has_db("restored_db"));
    let rec = &h.engine.list_operations().unwrap()[0];
    assert_eq!(rec.failed_step.as_deref(), Some("verify"));
}

#[tokio::test]
async fn on_top_needs_the_exact_typed_name_and_leaves_the_database_alone() {
    let h = Harness::new();
    let mut req = request(&h, "app", RestoreMode::OnTop);
    req.typed_name = Some("ap".into());
    let e = h.engine.restore(req).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert_eq!(h.fake.calls_matching("restore"), 0);
    assert!(h.fake.has_db("app"));
    assert!(h.engine.list_operations().unwrap().is_empty());
}

#[tokio::test]
async fn on_top_drains_sessions_and_restores_over_the_existing_database() {
    let h = Harness::new();
    h.fake.add_session("app", 9001, "still-here", false);
    let mut req = request(&h, "app", RestoreMode::OnTop);
    req.typed_name = Some("app".into());
    req.backup = Some(h.dir.path().join("app_predrop.dump"));

    let rec = h.engine.restore(req).await.unwrap();
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert_eq!(
        rec.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["drain", "restore", "verify"]
    );
    assert!(h.fake.has_db("app"), "on-top never recreates the database");
    // the sessions were terminated before the restore
    let sessions: Vec<_> = h
        .fake
        .server
        .lock()
        .unwrap()
        .dbs
        .get("app")
        .map(|d| d.sessions.clone())
        .unwrap_or_default();
    assert!(sessions.is_empty(), "{sessions:?}");
    // the safety backup is recorded, so retention protects it
    let protected = pgwhy_workflow::retry::protected_backups_with_operations(
        &[],
        &h.engine.list_operations().unwrap(),
    );
    assert!(protected.contains(&h.dir.path().join("app_predrop.dump")));
}

#[tokio::test]
async fn on_top_failure_says_the_objects_applied_so_far_remain() {
    let h = Harness::new();
    h.fake.fail(
        Point::Restore,
        pgwhy_core::AppError::new(ErrorKind::ToolFailed, "pg_restore failed on 1 item(s)"),
    );
    let mut req = request(&h, "app", RestoreMode::OnTop);
    req.typed_name = Some("app".into());
    let e = h.engine.restore(req).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::ToolFailed);
    assert!(h.fake.has_db("app"));
    let rec = &h.engine.list_operations().unwrap()[0];
    assert!(
        rec.warnings
            .iter()
            .any(|w| w.contains("still") && w.contains("database")),
        "{:?}",
        rec.warnings
    );
}

#[tokio::test]
async fn a_production_tag_is_refused_before_any_sql() {
    let h = Harness::new();
    let mut req = request(&h, "restored_db", RestoreMode::NewDatabase);
    req.connection.env_tag = EnvTag::Production;
    let e = h.engine.restore(req).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert_eq!(h.fake.calls_matching("create"), 0);
    assert!(h.engine.list_operations().unwrap().is_empty());
}

#[tokio::test]
async fn a_pg_restore_older_than_the_archive_is_refused() {
    let tool = pgwhy_pgtools::DiscoveredTool {
        version: pgwhy_pgtools::PgVersion::parse("16.15").unwrap(),
        ..fake_tool()
    };
    let e = check_tool_against_archive(&tool, &inspection("17.2", 1)).unwrap_err();
    assert_eq!(e.kind, ErrorKind::ToolVersion);
    assert!(e.message.contains("pg_dump 17.2"), "{}", e.message);
    // same major is fine
    assert!(check_tool_against_archive(&tool, &inspection("16.15", 1)).is_ok());
}

#[tokio::test]
async fn progress_and_steps_are_reported_live_and_the_restore_can_be_cancelled() {
    let h = Harness::new();
    h.fake.hang(Point::Restore);
    let events = Arc::new(std::sync::Mutex::new(Vec::<RestoreEvent>::new()));
    let sink_events = events.clone();
    let mut req = request(&h, "restored_db", RestoreMode::NewDatabase);
    req.on_event = Some(Arc::new(move |e| sink_events.lock().unwrap().push(e)));

    let engine = h.engine.clone();
    let task = tokio::spawn(async move { engine.restore(req).await });

    // wait for the record id from the Started event, then cancel it
    let id = loop {
        if let Some(RestoreEvent::Started { id, .. }) = events
            .lock()
            .unwrap()
            .iter()
            .find(|e| matches!(e, RestoreEvent::Started { .. }))
        {
            break id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    // keep cancelling until the hung restore notices (registration raced the Started event)
    for _ in 0..200 {
        if h.engine.cancel_operation(&RunId(id.clone())) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let e = task.await.unwrap().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled, "{e:?}");
    let log = events.lock().unwrap().clone();
    assert!(log
        .iter()
        .any(|e| matches!(e, RestoreEvent::Progress { .. })));
    let statuses: Vec<_> = log
        .iter()
        .filter_map(|e| match e {
            RestoreEvent::Step { id, status, .. } => Some((id.clone(), *status)),
            _ => None,
        })
        .collect();
    assert!(
        statuses.contains(&("create".to_string(), StepStatus::Succeeded)),
        "{statuses:?}"
    );
    assert!(
        statuses.contains(&("restore".to_string(), StepStatus::Running)),
        "{statuses:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Replace mode: restore into staging, then swap it over the target
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn replace_restores_into_staging_and_swaps_it_over_the_target() {
    let h = Harness::new();
    let mut req = request(&h, "app", RestoreMode::Replace);
    req.typed_name = Some("app".into());

    let rec = h.engine.restore(req).await.unwrap();
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert_eq!(
        rec.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["create", "restore", "verify", "swap"]
    );
    assert!(rec.steps.iter().all(|s| s.status == StepStatus::Succeeded));

    // the target kept its name; the staging copy and `_old` are gone; the marker is cleared
    assert!(h.fake.has_db("app"));
    let names = h.fake.db_names();
    assert_eq!(
        names,
        vec!["app".to_string(), "postgres".to_string()],
        "{names:?}"
    );
    assert_eq!(h.fake.comment("app"), None, "the swap marker was cleared");

    // the record says what happened, and a crash would have known how to finish it
    let swap = rec.restore().unwrap().swap.as_ref().unwrap();
    assert_eq!(swap.renames_completed, 2);
    assert_eq!(swap.work_db_state, WorkDbState::SwappedIn);
    assert_eq!(swap.staging_db, None);
    assert_eq!(rec.swap_state().unwrap().old_db.as_deref(), Some("app_old"));
    assert_eq!(h.engine.get_operation(&rec.id).unwrap(), rec);
}

#[tokio::test]
async fn a_replace_that_fails_before_the_swap_leaves_the_target_untouched() {
    let h = Harness::new();
    h.fake.fail(
        Point::Restore,
        pgwhy_core::AppError::new(ErrorKind::ToolFailed, "pg_restore failed on 1 item(s)"),
    );
    let mut req = request(&h, "app", RestoreMode::Replace);
    req.typed_name = Some("app".into());

    let e = h.engine.restore(req).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::ToolFailed);
    assert!(h.fake.has_db("app"), "the target is still there");
    assert_eq!(
        h.fake.comment("app").as_deref(),
        Some("my app"),
        "even its comment is untouched"
    );
    let names = h.fake.db_names();
    assert_eq!(
        names,
        vec!["app".to_string(), "postgres".to_string()],
        "the staging copy was dropped again: {names:?}"
    );

    let rec = &h.engine.list_operations().unwrap()[0];
    assert_eq!(rec.status, RunStatus::FailedAtStep);
    assert_eq!(rec.failed_step.as_deref(), Some("restore"));
    assert!(
        rec.warnings.iter().any(|w| w.contains("was dropped again")),
        "{:?}",
        rec.warnings
    );
}

#[tokio::test]
async fn a_failed_second_rename_is_rolled_back_so_the_target_survives() {
    let h = Harness::new();
    // the second rename (staging -> target) fails with something a retry cannot fix
    h.fake.fail(
        Point::Rename(2),
        pgwhy_core::AppError::new(ErrorKind::Permission, "must be owner of database app"),
    );
    let mut req = request(&h, "app", RestoreMode::Replace);
    req.typed_name = Some("app".into());

    let e = h.engine.restore(req).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Permission);
    // the first rename put the working database under `app_old`; the rollback renamed it back
    assert!(
        h.fake.has_db("app"),
        "the target was rolled back into place"
    );
    assert!(!h.fake.has_db("app_old"));
    assert_eq!(h.fake.comment("app").as_deref(), Some("my app"));

    let rec = &h.engine.list_operations().unwrap()[0];
    assert_eq!(rec.failed_step.as_deref(), Some("swap"));
    assert!(
        rec.warnings.iter().any(|w| w.contains("rolled back")),
        "{:?}",
        rec.warnings
    );
}

#[tokio::test]
async fn a_failed_rollback_keeps_the_staging_copy_so_the_swap_can_be_completed() {
    let h = Harness::new();
    // the second rename fails, and so does the rollback rename that follows it
    h.fake.fail(
        Point::Rename(2),
        pgwhy_core::AppError::new(ErrorKind::Permission, "must be owner of database app"),
    );
    h.fake.fail(
        Point::Rename(3),
        pgwhy_core::AppError::new(ErrorKind::Permission, "must be owner of database app_old"),
    );
    let mut req = request(&h, "app", RestoreMode::Replace);
    req.typed_name = Some("app".into());

    let e = h.engine.restore(req).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Permission);
    assert!(!h.fake.has_db("app"), "the target is parked under app_old");
    assert!(h.fake.has_db("app_old"), "the original data is safe");
    let staging: Vec<String> = h
        .fake
        .db_names()
        .into_iter()
        .filter(|n| n.contains("_staging_"))
        .collect();
    assert_eq!(staging.len(), 1, "the verified copy was not dropped");

    let rec = &h.engine.list_operations().unwrap()[0];
    let swap = rec.swap_state().unwrap();
    assert_eq!(swap.renames_completed, 1);
    assert_eq!(swap.staging_db.as_deref(), Some(staging[0].as_str()));
    assert_eq!(swap.old_db.as_deref(), Some("app_old"));
    assert!(
        rec.warnings
            .iter()
            .any(|w| w.contains("app_old") && w.contains(&staging[0])),
        "{:?}",
        rec.warnings
    );
}

#[tokio::test]
async fn a_second_rename_whose_reply_was_lost_is_recorded_as_done() {
    let h = Harness::new();
    // the server renames staging -> app, then the connection drops before the reply
    h.fake.lose_rename_reply.lock().unwrap().insert(2);
    let mut req = request(&h, "app", RestoreMode::Replace);
    req.typed_name = Some("app".into());

    let rec = h.engine.restore(req).await.unwrap();
    assert!(h.fake.has_db("app"), "the restored copy is in place");
    assert_eq!(rec.swap_state().unwrap().renames_completed, 2);
    assert_eq!(
        h.fake.calls_matching("rename"),
        2,
        "no rollback was attempted on a swap that had completed"
    );
    assert!(
        rec.warnings.iter().any(|w| w.contains("already renamed")),
        "{:?}",
        rec.warnings
    );
}

#[tokio::test]
async fn a_first_rename_whose_reply_was_lost_is_recorded_as_one_rename_done() {
    let h = Harness::new();
    // the server renames app -> app_old, the reply is lost, and the rollback is refused too
    h.fake.lose_rename_reply.lock().unwrap().insert(1);
    h.fake.fail(
        Point::Rename(2),
        pgwhy_core::AppError::new(ErrorKind::Permission, "must be owner of database app_old"),
    );
    let mut req = request(&h, "app", RestoreMode::Replace);
    req.typed_name = Some("app".into());

    let e = h.engine.restore(req).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Disconnected);
    assert!(h.fake.has_db("app_old") && !h.fake.has_db("app"));
    let rec = &h.engine.list_operations().unwrap()[0];
    assert_eq!(
        rec.swap_state().unwrap().renames_completed,
        1,
        "the record must say the target was already renamed"
    );
}

#[tokio::test]
async fn a_swap_that_completes_but_cannot_drop_old_is_a_cleanup_not_a_failure() {
    let h = Harness::new();
    h.fake.undroppable.lock().unwrap().replace("app_old".into());
    let mut req = request(&h, "app", RestoreMode::Replace);
    req.typed_name = Some("app".into());

    let rec = h.engine.restore(req).await.unwrap();
    assert_eq!(
        rec.status,
        RunStatus::SucceededCleanupRequired,
        "{:?}",
        rec.error
    );
    assert!(h.fake.has_db("app"), "the swap did happen");
    assert!(
        h.fake.has_db("app_old"),
        "the old copy could not be dropped"
    );
    assert!(
        rec.warnings
            .iter()
            .any(|w| w.contains("could not be dropped")),
        "{:?}",
        rec.warnings
    );
    // the record still names `old`, so history tells the user what to drop
    assert_eq!(rec.swap_state().unwrap().old_db.as_deref(), Some("app_old"));
}
