//! Workflows v2 state-machine tests: pause-before-swap (inspect staging → complete swap / drop),
//! `from_backup_file` entry mode (Custom & Plain `.sql`), `in_place_target` dry-run rollback,
//! and secret redaction.

mod support;

use pgwhy_core::config::EnvTag;
use pgwhy_workflow::*;
use std::path::PathBuf;
use std::sync::Arc;
use support::Harness;

#[tokio::test]
async fn pause_before_swap_leaves_staging_for_inspection_then_completes_swap() {
    let h = Harness::new();
    let mut plan = h.plan(ReplaceStrategy::StagingSwap);
    plan.pause_before_swap = true;

    let rec = h.run(plan).await;
    assert_eq!(
        rec.status,
        RunStatus::PausedForInspection,
        "{:?}",
        rec.error
    );
    assert_eq!(
        rec.step(StepId::Verify).unwrap().status,
        StepStatus::Succeeded
    );
    assert_eq!(rec.step(StepId::Swap).unwrap().status, StepStatus::Pending);
    assert_eq!(rec.renames_completed, 0);
    assert!(rec.staging_db.is_some());
    assert!(
        rec.headline
            .as_deref()
            .unwrap_or("")
            .contains("Paused before swap"),
        "{:?}",
        rec.headline
    );

    // State-aware actions: CompleteSwap, Retry(PostRestore), DropStaging — all enabled.
    let complete = rec
        .action(ActionKind::CompleteSwap)
        .expect("CompleteSwap offered");
    assert!(complete.enabled, "{complete:?}");
    let retry = rec.action(ActionKind::Retry).expect("Retry offered");
    assert!(retry.enabled, "{retry:?}");
    assert_eq!(retry.retry.as_ref().unwrap().begins_at, StepId::PostRestore);
    assert!(retry.retry.as_ref().unwrap().reuses_work_db);
    let drop_st = rec
        .action(ActionKind::DropStaging)
        .expect("DropStaging offered");
    assert!(drop_st.enabled, "{drop_st:?}");

    // Complete swap after inspecting staging.
    let done = h
        .engine
        .perform(&rec.run_id, ActionKind::CompleteSwap)
        .await
        .unwrap();
    assert_eq!(done.status, RunStatus::Succeeded, "{:?}", done.error);
    assert_eq!(done.renames_completed, 2);
    assert_eq!(done.work_db_state, WorkDbState::SwappedIn);
    assert_eq!(
        done.step(StepId::Swap).unwrap().status,
        StepStatus::Succeeded
    );
}

#[tokio::test]
async fn pause_before_swap_can_drop_staging_and_leave_target_untouched() {
    let h = Harness::new();
    let mut plan = h.plan(ReplaceStrategy::StagingSwap);
    plan.pause_before_swap = true;

    let rec = h.run(plan).await;
    assert_eq!(rec.status, RunStatus::PausedForInspection);

    let dropped = h
        .engine
        .perform(&rec.run_id, ActionKind::DropStaging)
        .await
        .unwrap();
    assert_eq!(dropped.status, RunStatus::Cancelled);
    assert_eq!(dropped.work_db_state, WorkDbState::Dropped);
    // Target database survived untouched.
    assert!(dropped.inventory.as_ref().unwrap().target.exists);
}

#[tokio::test]
async fn from_backup_file_skips_dump_and_never_deletes_external_archive() {
    let h = Harness::new();
    let ext_dir = tempfile::tempdir().unwrap();
    let archive = ext_dir.path().join("user_backup.dump");
    std::fs::write(&archive, b"fake custom archive").unwrap();

    let mut plan = h.plan(ReplaceStrategy::StagingSwap);
    plan.workflow.source_mode = Some(WorkflowSource::FromBackupFile {
        archive: BackupSource::FixedPath {
            path: archive.clone(),
        },
    });
    plan.external_archive = Some((
        BackupRecord {
            path: archive.clone(),
            size_bytes: 19,
            dumped_by: Some("16.4".into()),
            reusable: true,
            reason: None,
            cause: None,
            toc_items: Some(5),
            from_run: None,
        },
        pgwhy_pgtools::ArchiveKind::Custom,
    ));

    let steps = plan.steps();
    assert!(!steps.contains(&StepId::Dump), "{steps:?}");
    assert!(steps.contains(&StepId::Restore));
    assert!(steps.contains(&StepId::Swap));

    let rec = h.run(plan).await;
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert!(
        archive.exists(),
        "external user archive must never be deleted"
    );
}

#[tokio::test]
async fn in_place_dry_run_steps_and_no_workflow_success_flag() {
    let h = Harness::new();
    let mut plan = h.plan(ReplaceStrategy::StagingSwap);
    plan.workflow.source_mode = Some(WorkflowSource::InPlaceTarget {
        pre_run_backup: true,
    });
    plan.dry_run = true;
    plan.pre_run_backup = false;
    plan.pipeline = vec![
        CompiledPipelineStep {
            step_id: "s1".into(),
            step_name: "Delete users".into(),
            no_transaction: false,
            exec: CompiledExec::Sql {
                sql: "DELETE FROM \"public\".\"users\";".into(),
                preview_sql: "DELETE FROM \"public\".\"users\";".into(),
            },
        },
        CompiledPipelineStep {
            step_id: "s2".into(),
            step_name: "Verify count".into(),
            no_transaction: false,
            exec: CompiledExec::Verify {
                assertion: VerifyAssertion::RowCount {
                    schema: "public".into(),
                    table: "users".into(),
                    op: CompareOp::Eq,
                    expected: 0,
                    conditions: Vec::new(),
                    combinator: LogicalOp::And,
                },
                sql: "SELECT count(*) FROM \"public\".\"users\";".into(),
                preview_sql: "SELECT count(*) FROM \"public\".\"users\";".into(),
            },
        },
    ];

    let steps = plan.steps();
    assert_eq!(steps, vec![StepId::PostRestore, StepId::Verify]);

    let rec = h.run(plan).await;
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert!(rec.dry_run);
    assert!(rec.staging_db.is_none());
    assert!(rec.old_db.is_none());
    assert!(
        rec.step(StepId::Verify)
            .and_then(|s| s.note.as_deref())
            .unwrap_or("")
            .contains("dry run"),
        "{:?}",
        rec.step(StepId::Verify)
    );
}

#[test]
fn start_request_debug_redacts_secrets_and_templates_compile() {
    let mut secret_params = std::collections::HashMap::new();
    secret_params.insert("test_password".into(), "SuperSecret123!".into());
    let req = StartRequest {
        fingerprint: "fp".into(),
        remember: false,
        typed_name: Some("prod_db".into()),
        params: std::collections::HashMap::new(),
        secret_params,
        pause_before_swap: Some(true),
        dry_run: false,
    };
    let dbg = format!("{req:?}");
    assert!(!dbg.contains("SuperSecret123!"), "{dbg}");
    assert!(dbg.contains("redacted"), "{dbg}");

    // Every built-in starter template compiles cleanly in Preview mode with zero problems.
    let empty = std::collections::HashMap::new();
    let mode = CompileMode::Preview { values: &empty };
    for tpl in starter_templates() {
        let compiled = compile_steps(&tpl.workflow.steps, &tpl.workflow.parameters, &mode, None)
            .unwrap_or_else(|e| panic!("template {} failed to compile: {e}", tpl.id));
        assert!(!compiled.is_empty(), "template {} has steps", tpl.id);
        for c in &compiled {
            assert!(
                c.problems.is_empty(),
                "template {} step {} had problems: {:?}",
                tpl.id,
                c.step_name,
                c.problems
            );
        }
    }

    // Validate Staging/Production target allows workflows (typed confirmation enforced at start).
    let src = pgwhy_core::config::ConnectionEntry {
        id: pgwhy_core::config::ConnectionId("s".into()),
        name: "src".into(),
        group: None,
        host: "src.local".into(),
        port: 5432,
        database: "app".into(),
        user: "u".into(),
        maintenance_db: "postgres".into(),
        sslmode: pgwhy_core::config::SslMode::Disable,
        ssl_root_cert: None,
        tunnel: None,
        env_tag: EnvTag::Local,
        password_saved: false,
    };
    let mut prod = src.clone();
    prod.id = pgwhy_core::config::ConnectionId("p".into());
    prod.host = "prod.example".into();
    prod.env_tag = EnvTag::Production;
    let mut tpl = starter_templates()[0].workflow.clone();
    tpl.target = DbRef {
        connection: prod.id.clone(),
        database: "app".into(),
    };
    validate_workflow(&tpl, None, &prod).unwrap();
    let _ = PathBuf::from("unused");
    let _ = Arc::new(());
}
