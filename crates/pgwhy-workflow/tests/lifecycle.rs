//! Lifecycle verbs over the scripted fake (v1.1 Epic 4): guardrails, drain, prepared-transaction
//! refusal, the shared run lock, and the operation record each verb leaves behind.

mod support;

use pgwhy_core::config::{ConnectionEntry, EnvTag};
use pgwhy_core::ErrorKind;
use pgwhy_workflow::ops::PreparedXact;
use pgwhy_workflow::*;
use support::*;

fn target_of(e: &ConnectionEntry, database: &str) -> DbTarget {
    DbTarget {
        connection_id: e.id.clone(),
        connection_name: e.name.clone(),
        server: e.server_key(),
        env_tag: e.env_tag,
        database: database.into(),
    }
}

fn local(h: &Harness) -> DbTarget {
    target_of(&h.target, "app")
}

#[tokio::test]
async fn create_database_records_every_step_and_persists() {
    let h = Harness::new();
    let mut t = local(&h);
    t.database = "fresh".into();

    let rec = h
        .engine
        .create_database(CreateDatabaseRequest {
            target: t,
            locale: None,
            comment: Some("scratch db".into()),
        })
        .await
        .unwrap();

    assert_eq!(rec.kind, OperationKind::Create);
    assert_eq!(rec.status, RunStatus::Succeeded);
    assert_eq!(rec.steps.len(), 1);
    assert_eq!(rec.steps[0].status, StepStatus::Succeeded);
    assert!(rec.finished_at.is_some());
    assert!(matches!(
        &rec.detail,
        OperationDetail::Create { name, .. } if name == "fresh"
    ));
    assert!(h.fake.has_db("fresh"));
    // the comment was applied in a second statement, not treated as part of the create
    assert!(h.fake.calls_matching("create fresh") >= 1);
    assert!(h.fake.calls_matching("comment fresh") >= 1);
    // persisted: history can answer "what did I do"
    assert_eq!(h.engine.get_operation(&rec.id).unwrap(), rec);
}

#[tokio::test]
async fn create_database_keeps_the_record_when_the_server_refuses() {
    let h = Harness::new();
    h.fake.fail(
        Point::CreateDb,
        pgwhy_core::AppError::new(
            ErrorKind::Permission,
            "permission denied to create database",
        ),
    );
    let mut t = local(&h);
    t.database = "fresh".into();

    let e = h
        .engine
        .create_database(CreateDatabaseRequest {
            target: t,
            locale: None,
            comment: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Permission);
    assert!(!h.fake.has_db("fresh"));

    let ops = h.engine.list_operations().unwrap();
    assert_eq!(ops.len(), 1, "the failed attempt is still in history");
    assert_eq!(ops[0].status, RunStatus::FailedAtStep);
    assert_eq!(ops[0].failed_step.as_deref(), Some("create"));
    assert!(ops[0].error.is_some());
    assert_eq!(ops[0].steps[0].status, StepStatus::Failed);
}

#[tokio::test]
async fn destructive_verbs_refuse_a_production_tag_before_any_sql() {
    let h = Harness::new();
    let mut prod = target_of(&h.target, "app");
    prod.env_tag = EnvTag::Production;

    let e = h
        .engine
        .drop_database(DropDatabaseRequest {
            target: prod.clone(),
            typed_name: "app".into(),
            backup: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert!(e.message.contains("Production"), "{}", e.message);
    // nothing was sent, nothing was recorded
    assert_eq!(h.fake.calls_matching("drop"), 0);
    assert!(h.engine.list_operations().unwrap().is_empty());
    assert!(h.fake.has_db("app"));

    // and a rename is gated the same way
    let e = h
        .engine
        .rename_database(RenameDatabaseRequest {
            target: prod,
            to: "app2".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert_eq!(h.fake.calls_matching("rename"), 0);
}

#[tokio::test]
async fn drop_requires_the_exact_database_name() {
    let h = Harness::new();
    for wrong in ["", "ap", "App", "app_local"] {
        let e = h
            .engine
            .drop_database(DropDatabaseRequest {
                target: local(&h),
                typed_name: wrong.into(),
                backup: None,
            })
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Refused, "{wrong:?}");
        assert!(h.fake.has_db("app"));
    }
    assert_eq!(h.fake.calls_matching("drop"), 0);
    assert!(h.engine.list_operations().unwrap().is_empty());
}

#[tokio::test]
async fn drop_drains_sessions_and_records_the_backup_it_was_given() {
    let h = Harness::new();
    let backup = h.dir.path().join("app_predrop.dump");
    std::fs::write(&backup, b"dump").unwrap();

    let rec = h
        .engine
        .drop_database(DropDatabaseRequest {
            target: local(&h),
            typed_name: "app".into(),
            backup: Some(backup.clone()),
        })
        .await
        .unwrap();

    assert_eq!(rec.status, RunStatus::Succeeded);
    assert_eq!(
        rec.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["check", "drop"]
    );
    assert!(rec.steps.iter().all(|s| s.status == StepStatus::Succeeded));
    assert!(!h.fake.has_db("app"), "database is gone");
    assert!(
        matches!(&rec.detail, OperationDetail::Drop { database, backup: Some(b), connection: Some(c) }
            if database == "app" && b == &backup && c.name == "local"),
        "{:?}",
        rec.detail
    );
    assert_eq!(h.engine.get_operation(&rec.id).unwrap(), rec);

    // retention sees it as protected for as long as the record exists
    let protected = pgwhy_workflow::retry::protected_backups_with_operations(
        &[],
        &h.engine.list_operations().unwrap(),
    );
    assert!(protected.contains(&backup));
}

#[tokio::test]
async fn drop_refuses_prepared_transactions_instead_of_discarding_them() {
    let h = Harness::new();
    h.fake.server.lock().unwrap().prepared.push(PreparedXact {
        gid: "tx-1".into(),
        database: "app".into(),
        owner: "me".into(),
        prepared_at: "2026-10-22T10:00:00Z".into(),
    });

    let e = h
        .engine
        .drop_database(DropDatabaseRequest {
            target: local(&h),
            typed_name: "app".into(),
            backup: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert!(e.message.contains("prepared transaction"), "{}", e.message);
    assert!(e.hint.unwrap_or_default().contains("ROLLBACK PREPARED"));
    assert!(h.fake.has_db("app"), "nothing was dropped");

    let ops = h.engine.list_operations().unwrap();
    assert_eq!(ops[0].failed_step.as_deref(), Some("check"));
}

#[tokio::test]
async fn drop_fails_retriably_naming_a_reconnecting_client() {
    let h = Harness::new();
    // a client that reconnects every time it is terminated
    h.fake.add_session("app", 777, "always-on", true);

    let e = h
        .engine
        .drop_database(DropDatabaseRequest {
            target: local(&h),
            typed_name: "app".into(),
            backup: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert!(e.is_retriable());
    assert!(e.message.contains("always-on"), "{}", e.message);
    assert!(h.fake.has_db("app"));

    let ops = h.engine.list_operations().unwrap();
    assert_eq!(ops[0].status, RunStatus::FailedAtStep);
    assert_eq!(ops[0].failed_step.as_deref(), Some("drop"));
}

#[tokio::test]
async fn rename_drains_and_keeps_the_contents() {
    let h = Harness::new();
    let rec = h
        .engine
        .rename_database(RenameDatabaseRequest {
            target: local(&h),
            to: "app_dev".into(),
        })
        .await
        .unwrap();

    assert_eq!(rec.status, RunStatus::Succeeded);
    assert!(h.fake.has_db("app_dev"));
    assert!(!h.fake.has_db("app"));
    assert!(matches!(
        &rec.detail,
        OperationDetail::Rename { database, to } if database == "app" && to == "app_dev"
    ));
    // the session the fake had (4242/psql) was terminated, not carried over
    assert!(h.fake.server.lock().unwrap().dbs["app_dev"]
        .sessions
        .is_empty());
}

#[tokio::test]
async fn rename_to_the_same_name_is_a_no_op_without_sql() {
    let h = Harness::new();
    let rec = h
        .engine
        .rename_database(RenameDatabaseRequest {
            target: local(&h),
            to: "app".into(),
        })
        .await
        .unwrap();
    assert_eq!(rec.steps.len(), 0);
    assert_eq!(h.fake.calls_matching("rename"), 0);
    assert!(h.fake.has_db("app"));
}

#[tokio::test]
async fn a_held_server_lock_refuses_the_operation_retriably() {
    let h = Harness::new();
    let t = local(&h);
    let _held = h
        .engine
        .locks()
        .try_acquire(&t.server, "the Refresh \"Local app\" run")
        .unwrap();

    let e = h
        .engine
        .create_database(CreateDatabaseRequest {
            target: t,
            locale: None,
            comment: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert!(e.is_retriable());
    assert!(e.message.contains("Refresh"), "{}", e.message);
    assert_eq!(h.fake.calls_matching("create"), 0);
}
