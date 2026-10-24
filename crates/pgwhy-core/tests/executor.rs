//! Query execution core integration tests against containerized Postgres.

mod harness;

use harness::{PgFixture, PG_DB, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{ConnectionManager, ManagerOptions, NoTunnels, PgConn, Purpose};
use pgwhy_core::query::{
    split, CollectSink, CollectedEvent, ErrorPolicy, QuerySession, ResultSink, RowBatch,
    RunOptions, RunOutcome, ScriptMode, SessionState, StatementDone, TxnStatus,
};
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_core::ErrorKind;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Env {
    manager: ConnectionManager,
    id: ConnectionId,
}

impl Env {
    fn new(pg: &PgFixture) -> Self {
        let entry = ConnectionEntry {
            id: ConnectionId::new(),
            name: "exec".into(),
            group: None,
            host: pg.host.clone(),
            port: pg.port,
            database: PG_DB.into(),
            user: PG_USER.into(),
            maintenance_db: "postgres".into(),
            sslmode: SslMode::Prefer,
            ssl_root_cert: None,
            tunnel: None,
            env_tag: EnvTag::Local,
            password_saved: true,
        };
        let vault = Arc::new(Vault::new(Arc::new(MemoryStore::default())));
        vault
            .store(
                &SecretKey::new(&entry.id, SecretKind::DbPassword),
                Secret::new(PG_PASSWORD),
                true,
            )
            .unwrap();
        let manager = ConnectionManager::new(vault, Arc::new(NoTunnels), ManagerOptions::default());
        let id = entry.id.clone();
        manager.set_entries([entry]);
        Self { manager, id }
    }

    async fn conn(&self) -> PgConn {
        self.manager
            .open(&self.id, PG_DB, Purpose::EditorTab)
            .await
            .unwrap()
    }

    async fn session(&self) -> QuerySession {
        QuerySession::new(self.conn().await)
    }
}

async fn run(s: &QuerySession, sql: &str) -> (pgwhy_core::query::RunSummary, CollectSink) {
    run_with(s, sql, &RunOptions::default()).await
}

async fn run_with(
    s: &QuerySession,
    sql: &str,
    opts: &RunOptions,
) -> (pgwhy_core::query::RunSummary, CollectSink) {
    let mut sink = CollectSink::new();
    let summary = s.run_text(sql, opts, &mut sink).await.unwrap();
    (summary, sink)
}

#[tokio::test]
async fn multi_statement_results_and_notices_arrive_in_order() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;
    let sql = "select 1 as a, ''::text as empty, null::int as nothing;\n\
               do $$ begin raise notice 'hello'; raise notice 'world'; end $$;\n\
               select g as n from generate_series(1, 3) g;\n\
               create temp table t(x int);\n\
               insert into t values (1), (2);";
    let (summary, sink) = run(&s, sql).await;
    assert_eq!(summary.outcome, RunOutcome::Completed);
    assert_eq!(
        (summary.total, summary.succeeded, summary.failed),
        (5, 5, 0)
    );
    use CollectedEvent::*;
    assert_eq!(
        sink.log,
        vec![
            Started { index: 0 },
            Columns { index: 0 },
            Rows { index: 0, count: 1 },
            Completed { index: 0 },
            Started { index: 1 },
            Notice {
                index: Some(1),
                message: "hello".into()
            },
            Notice {
                index: Some(1),
                message: "world".into()
            },
            Completed { index: 1 },
            Started { index: 2 },
            Columns { index: 2 },
            Rows { index: 2, count: 3 },
            Completed { index: 2 },
            Started { index: 3 },
            Completed { index: 3 },
            Started { index: 4 },
            Completed { index: 4 },
        ]
    );
    let first = &sink.statements[0];
    let names: Vec<_> = first.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["a", "empty", "nothing"]);
    let types: Vec<_> = first
        .columns
        .iter()
        .map(|c| c.type_name.as_deref().unwrap())
        .collect();
    assert_eq!(types, ["int4", "text", "int4"]);
    // NULL distinct from empty string
    assert_eq!(
        first.rows,
        vec![vec![Some("1".into()), Some(String::new()), None]]
    );
    assert_eq!(sink.statements[2].done.as_ref().unwrap().rows_returned, 3);
    let insert = sink.statements[4].done.as_ref().unwrap();
    assert_eq!(
        (insert.command.as_str(), insert.rows_affected),
        ("INSERT", 2)
    );
    assert_eq!(
        sink.statements[3].done.as_ref().unwrap().command,
        "CREATE TABLE"
    );
    assert_eq!(s.state(), SessionState::Idle);
}

#[tokio::test]
async fn error_positions_map_to_the_original_text_and_policy_applies() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;
    // multibyte characters both before the failing statement and inside it, before the error
    let sql = "select 'héllo wörld';\n  -- 日本語のコメント\n  select '日本', bogus_col from (select 1) x;\nselect 3;";
    let (summary, sink) = run(&s, sql).await;
    assert_eq!(summary.outcome, RunOutcome::StoppedOnError { index: 1 });
    assert_eq!(
        (summary.succeeded, summary.failed, summary.skipped),
        (1, 1, 1)
    );
    let (k, err) = summary.first_error.clone().unwrap();
    assert_eq!(k, 1);
    assert_eq!(err.kind, ErrorKind::Sql);
    assert_eq!(err.sqlstate.as_deref(), Some("42703"));
    let pos = err.position.expect("position");
    assert!(sql[pos..].starts_with("bogus_col"), "{:?}", &sql[pos..]);
    assert_eq!(sink.statements.len(), 2, "statement 3 never started");

    // syntax error: position points at the offending token
    let sql2 = "select 1;\nselect 'ü' frm t;";
    let (summary, _) = run(&s, sql2).await;
    let (_, err) = summary.first_error.unwrap();
    assert_eq!(err.sqlstate.as_deref(), Some("42601"));
    assert!(sql2[err.position.unwrap()..].starts_with("t;"), "{err:?}");

    // continue policy runs the rest
    let opts = RunOptions {
        on_error: ErrorPolicy::Continue,
        ..RunOptions::default()
    };
    let (summary, sink) = run_with(&s, sql, &opts).await;
    assert_eq!(summary.outcome, RunOutcome::Completed);
    assert_eq!(
        (summary.succeeded, summary.failed, summary.skipped),
        (2, 1, 0)
    );
    assert_eq!(sink.statements[2].rows, vec![vec![Some("3".into())]]);
    assert_eq!(s.state(), SessionState::Idle);
}

#[tokio::test]
async fn transaction_state_tracks_begin_commit_rollback_and_failures() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;
    let mut watch = s.watch_state();

    let (_, sink) = run(
        &s,
        "begin; create table t(x int); insert into t values (1);",
    )
    .await;
    assert_eq!(s.txn_status(), TxnStatus::InTransaction);
    assert_eq!(s.state(), SessionState::InTransaction);
    assert!(sink.log.contains(&CollectedEvent::TxnChanged {
        status: TxnStatus::InTransaction
    }));
    assert_eq!(*watch.borrow_and_update(), SessionState::InTransaction);
    assert_eq!(s.resync_txn().await.unwrap(), TxnStatus::InTransaction);

    run(&s, "commit").await;
    assert_eq!(s.txn_status(), TxnStatus::Idle);
    assert_eq!(s.resync_txn().await.unwrap(), TxnStatus::Idle);

    // an error inside the block → failed; further statements fail with 25P02
    let (summary, _) = run(&s, "begin; insert into t values (1/0);").await;
    assert!(matches!(
        summary.outcome,
        RunOutcome::StoppedOnError { index: 1 }
    ));
    assert_eq!(s.state(), SessionState::FailedTransaction);
    let (summary, _) = run(&s, "select 1").await;
    assert_eq!(
        summary.first_error.unwrap().1.sqlstate.as_deref(),
        Some("25P02")
    );
    assert_eq!(s.resync_txn().await.unwrap(), TxnStatus::Failed);
    run(&s, "rollback").await;
    assert_eq!(s.txn_status(), TxnStatus::Idle);

    // describe failures inside a transaction surface the real error, not 25P02
    let (summary, _) = run(&s, "begin; select * from missing_table;").await;
    assert_eq!(
        summary.first_error.unwrap().1.sqlstate.as_deref(),
        Some("42P01")
    );
    assert_eq!(s.txn_status(), TxnStatus::Failed);
    assert_eq!(s.rollback().await.unwrap(), TxnStatus::Idle);

    // savepoints recover a failed block
    run(
        &s,
        "begin; savepoint a; insert into t values (1/0); rollback to savepoint a; insert into t values (2);",
    )
    .await;
    // stop-on-error stopped at the failing insert; recover explicitly
    assert_eq!(s.txn_status(), TxnStatus::Failed);
    run(&s, "rollback to savepoint a; insert into t values (2);").await;
    assert_eq!(s.txn_status(), TxnStatus::InTransaction);
    assert_eq!(s.commit().await.unwrap(), TxnStatus::Idle);
    let (_, sink) = run(&s, "select count(*) from t").await;
    assert_eq!(sink.statements[0].rows, vec![vec![Some("2".into())]]);

    // a COMMIT that fails (deferred constraint) rolls back: probe resolves it to idle
    run(
        &s,
        "create table p(id int primary key); \
         create table c(pid int references p deferrable initially deferred);",
    )
    .await;
    let (summary, _) = run(&s, "begin; insert into c values (42); commit;").await;
    assert_eq!(
        summary.first_error.unwrap().1.sqlstate.as_deref(),
        Some("23503")
    );
    assert_eq!(s.txn_status(), TxnStatus::Idle);
    assert_eq!(s.resync_txn().await.unwrap(), TxnStatus::Idle);
}

/// Runs a long statement on a background task and cancels it.
async fn cancel_long_query(
    s: Arc<QuerySession>,
) -> (pgwhy_core::query::RunSummary, CollectSink, Duration) {
    let runner = {
        let s = s.clone();
        tokio::spawn(async move {
            let mut sink = CollectSink::new();
            let summary = s
                .run_text(
                    "select pg_sleep(30); select 'queued'",
                    &RunOptions::default(),
                    &mut sink,
                )
                .await
                .unwrap();
            (summary, sink)
        })
    };
    // wait until the statement is on the wire
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.state() != SessionState::Running && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    assert!(
        s.cancel_handle().cancel().await.unwrap(),
        "a cancel request was sent"
    );
    let (summary, sink) = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("cancel returns promptly")
        .unwrap();
    (summary, sink, t0.elapsed())
}

#[tokio::test]
async fn cancel_stops_the_statement_abandons_the_queue_and_keeps_the_connection() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = Arc::new(env.session().await);

    let (summary, sink, took) = cancel_long_query(s.clone()).await;
    assert!(took < Duration::from_secs(5));
    assert_eq!(summary.outcome, RunOutcome::Cancelled { index: 0 });
    assert_eq!(summary.skipped, 1);
    assert!(sink.statements[0].cancelled);
    assert_eq!(sink.statements.len(), 1, "queued statement never started");
    // usable, and still idle
    assert_eq!(s.state(), SessionState::Idle);
    let (summary, sink) = run(&s, "select 'still here'").await;
    assert_eq!(summary.outcome, RunOutcome::Completed);
    assert_eq!(
        sink.statements[0].rows,
        vec![vec![Some("still here".into())]]
    );

    // inside a transaction the server aborts the block on cancel: failed, not lost
    run(&s, "begin").await;
    let (summary, _, _) = cancel_long_query(s.clone()).await;
    assert_eq!(summary.outcome, RunOutcome::Cancelled { index: 0 });
    assert_eq!(s.state(), SessionState::FailedTransaction);
    assert_eq!(s.resync_txn().await.unwrap(), TxnStatus::Failed);
    assert_eq!(s.rollback().await.unwrap(), TxnStatus::Idle);

    // cancelling with nothing running is a no-op and does not poison the next run
    assert!(!s.cancel_handle().cancel().await.unwrap());
    let (summary, _) = run(&s, "select 1").await;
    assert_eq!(summary.outcome, RunOutcome::Completed);

    // a statement_timeout is a failure, not a user cancel
    let (summary, _) = run(&s, "set statement_timeout = '100ms'; select pg_sleep(5);").await;
    assert_eq!(summary.outcome, RunOutcome::StoppedOnError { index: 1 });
    assert_eq!(
        summary.first_error.unwrap().1.sqlstate.as_deref(),
        Some("57014")
    );
}

async fn terminate(env: &Env, pid: i32) {
    let admin = env.conn().await;
    admin
        .client
        .execute("select pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap();
}

#[tokio::test]
async fn server_kill_mid_transaction_signals_connection_and_transaction_lost() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);

    // killed while idle in transaction: the next run reports the loss
    let s = env.session().await;
    let mut watch = s.watch_state();
    run(&s, "begin; create table lost(x int);").await;
    assert_eq!(s.state(), SessionState::InTransaction);
    terminate(&env, s.conn().info.backend_pid).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        watch.wait_for(|st| *st == SessionState::Disconnected),
    )
    .await
    .expect("watch sees the disconnect")
    .unwrap();
    assert!(s.transaction_lost());
    let err = s
        .run_text("select 1", &RunOptions::default(), &mut CollectSink::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Disconnected);
    assert!(err.message.contains("rolled back"), "{err:?}");

    // killed while a statement is running inside a transaction
    let s = Arc::new(env.session().await);
    run(&s, "begin").await;
    let pid = s.conn().info.backend_pid;
    let runner = {
        let s = s.clone();
        tokio::spawn(async move {
            let mut sink = CollectSink::new();
            s.run_text(
                "select pg_sleep(30); select 2",
                &RunOptions::default(),
                &mut sink,
            )
            .await
            .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    terminate(&env, pid).await;
    let summary = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        summary.outcome,
        RunOutcome::ConnectionLost {
            index: 0,
            transaction_lost: true
        }
    );
    assert_eq!(summary.first_error.unwrap().1.kind, ErrorKind::Disconnected);
    assert_eq!(s.state(), SessionState::Disconnected);
    assert!(s.transaction_lost());
}

/// Counts rows without keeping them, recording batch sizes.
#[derive(Default)]
struct CountingSink {
    rows: u64,
    batches: usize,
    max_batch: usize,
    last_cell: Option<String>,
    done: Option<StatementDone>,
}

impl ResultSink for CountingSink {
    fn rows(&mut self, _index: usize, rows: RowBatch<'_>) {
        self.rows += rows.len() as u64;
        self.batches += 1;
        self.max_batch = self.max_batch.max(rows.len());
        self.last_cell = rows.get(rows.len() - 1, 0).map(str::to_string);
    }

    fn completed(&mut self, _index: usize, done: &StatementDone) {
        self.done = Some(done.clone());
    }
}

#[tokio::test]
async fn hundred_thousand_rows_stream_in_bounded_batches() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;
    let mut sink = CountingSink::default();
    let opts = RunOptions {
        batch_rows: 1000,
        ..RunOptions::default()
    };
    let summary = s
        .run_text(
            "select g, repeat('x', 50) as pad from generate_series(1, 100000) g",
            &opts,
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(summary.outcome, RunOutcome::Completed);
    assert_eq!(sink.rows, 100_000);
    let done = sink.done.unwrap();
    assert_eq!((done.rows_returned, done.rows_affected), (100_000, 100_000));
    // rows were handed over incrementally, never more than one batch held
    assert!(sink.max_batch <= 1000, "max batch {}", sink.max_batch);
    assert!(sink.batches >= 100, "batches {}", sink.batches);
    assert_eq!(sink.last_cell.as_deref(), Some("100000"));
}

#[tokio::test]
async fn copy_from_client_is_refused_and_the_connection_stays_usable() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;
    let (summary, _) = run(
        &s,
        "create temp table c(x int); copy c from stdin; select 1",
    )
    .await;
    assert_eq!(summary.outcome, RunOutcome::StoppedOnError { index: 1 });
    let (_, err) = summary.first_error.unwrap();
    assert_eq!(err.kind, ErrorKind::Refused);
    let (summary, _) = run(&s, "select 1").await;
    assert_eq!(summary.outcome, RunOutcome::Completed);
}

#[tokio::test]
async fn run_script_single_transaction_and_no_transaction_modes() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;

    // failure at statement 3 rolls everything back
    let script = "create table s1(x int);\ninsert into s1 values (1);\nselect 1/0;\ninsert into s1 values (2);";
    let report = s
        .run_script(
            script,
            ScriptMode::SingleTransaction,
            &mut CollectSink::new(),
        )
        .await
        .unwrap();
    assert!(!report.committed);
    let err = report.error().unwrap();
    assert!(err.message.starts_with("statement 3 failed"), "{err:?}");
    assert_eq!(s.txn_status(), TxnStatus::Idle);
    let (summary, _) = run(&s, "select * from s1").await;
    assert_eq!(
        summary.first_error.unwrap().1.sqlstate.as_deref(),
        Some("42P01"),
        "s1 was rolled back"
    );

    // success commits
    let report = s
        .run_script(
            "create table s2(x int); insert into s2 values (1), (2);",
            ScriptMode::SingleTransaction,
            &mut CollectSink::new(),
        )
        .await
        .unwrap();
    assert!(report.committed && report.error().is_none());
    let (_, sink) = run(&s, "select count(*) from s2").await;
    assert_eq!(sink.statements[0].rows, vec![vec![Some("2".into())]]);

    // refusals before anything is sent
    let err = s
        .run_script(
            "insert into s2 values (3); commit;",
            ScriptMode::SingleTransaction,
            &mut CollectSink::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Refused);
    let concurrently = "create index concurrently s2_x on s2(x);";
    let err = s
        .run_script(
            concurrently,
            ScriptMode::SingleTransaction,
            &mut CollectSink::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Refused);
    let (_, sink) = run(&s, "select count(*) from s2").await;
    assert_eq!(
        sink.statements[0].rows,
        vec![vec![Some("2".into())]],
        "nothing ran"
    );

    // the no-transaction option runs CONCURRENTLY scripts
    let report = s
        .run_script(
            concurrently,
            ScriptMode::NoTransaction,
            &mut CollectSink::new(),
        )
        .await
        .unwrap();
    assert!(report.committed, "{:?}", report.error());

    // a deferred-constraint failure at COMMIT is reported as a commit error
    let report = s
        .run_script(
            "create table p(id int primary key); \
             create table c(pid int references p deferrable initially deferred); \
             insert into c values (7);",
            ScriptMode::SingleTransaction,
            &mut CollectSink::new(),
        )
        .await
        .unwrap();
    assert!(!report.committed);
    assert!(report.commit_error.is_some());
    assert_eq!(s.txn_status(), TxnStatus::Idle);
    let (summary, _) = run(&s, "select * from p").await;
    assert!(summary.first_error.is_some(), "p was not committed");
}

#[tokio::test]
async fn run_selection_ranges_report_positions_against_the_whole_buffer() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;
    let buffer = "select 'ignored';\nselect 'ñ', nope;\n";
    let start = buffer.find("select 'ñ'").unwrap();
    let statements = pgwhy_core::query::split_range(buffer, start..buffer.len());
    assert_eq!(statements.len(), 1);
    let mut sink = CollectSink::new();
    let summary = s
        .run(buffer, &statements, &RunOptions::default(), &mut sink)
        .await
        .unwrap();
    let pos = summary.first_error.unwrap().1.position.unwrap();
    assert!(buffer[pos..].starts_with("nope"));
    assert_eq!(split(buffer).len(), 2);
}

/// With `standard_conforming_strings = off` the server reads `'\'; delete from t; --'` as one
/// literal. A splitter that assumed `on` would send `delete from t` as its own statement.
#[tokio::test]
async fn scs_off_hostile_script_never_runs_a_partial_statement() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let hostile = "select '\\'; delete from t; --'";
    let verify = "select count(*) from t";

    // the setting flips inside the script itself: the rest must be re-split after the SET
    let s = env.session().await;
    let script = format!(
        "create temp table t(x int);\ninsert into t values (1);\n\
         set standard_conforming_strings = off;\n{hostile};\n{verify};"
    );
    let (summary, sink) = run(&s, &script).await;
    assert_eq!(summary.outcome, RunOutcome::Completed, "{summary:?}");
    assert_eq!((summary.total, summary.failed), (5, 0));
    assert_eq!(
        sink.statements[3].rows,
        vec![vec![Some("'; delete from t; --".into())]]
    );
    assert_eq!(
        sink.statements[4].rows,
        vec![vec![Some("1".into())]],
        "t untouched"
    );

    // the setting was already off when the run starts (read from the server, not assumed)
    let (summary, sink) = run(&s, &format!("{hostile};\n{verify};")).await;
    assert_eq!((summary.total, summary.failed), (2, 0), "{summary:?}");
    assert_eq!(sink.statements[1].rows, vec![vec![Some("1".into())]]);

    // so does the session's own splitter, used for selections
    let ranges = s.split_range(hostile, 0..hostile.len()).await;
    assert_eq!(ranges.len(), 1);

    // back on: `\` is data again and the same text really is two statements
    run(&s, "reset standard_conforming_strings").await;
    let script = "select '\\'; select 2";
    let (summary, _) = run(&s, script).await;
    assert_eq!((summary.total, summary.failed), (2, 0), "{summary:?}");

    // run_script reads the setting too
    run(&s, "set standard_conforming_strings = off").await;
    let sql = format!("{hostile};\nselect 1");
    let report = s
        .run_script(&sql, ScriptMode::NoTransaction, &mut CollectSink::new())
        .await
        .unwrap();
    assert!(report.committed, "{report:?}");
    assert_eq!(report.summary.total, 2);
}

/// The describe cache must not outlive DDL on the same session: `select *` has to pick up the
/// new column (and its type) on the next run, not replay the cached describe.
#[tokio::test]
async fn describe_cache_is_invalidated_by_ddl() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = env.session().await;
    run(
        &s,
        "drop table if exists describe_cache_t; create table describe_cache_t(a int)",
    )
    .await;

    let sql = "select * from describe_cache_t";
    let (_, first) = run(&s, sql).await;
    let types: Vec<String> = first.statements[0]
        .columns
        .iter()
        .map(|c| c.type_name.clone().unwrap_or_default())
        .collect();
    assert_eq!(types, ["int4"]);

    // same statement text, cached describe would still say one int4 column
    run(&s, "alter table describe_cache_t add column b text").await;
    let (_, second) = run(&s, sql).await;
    let types: Vec<String> = second.statements[0]
        .columns
        .iter()
        .map(|c| c.type_name.clone().unwrap_or_default())
        .collect();
    assert_eq!(types, ["int4", "text"], "stale describe after DDL");
    assert_eq!(
        second.statements[0]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

/// `check_can_start` is the pre-flight an editor tab uses before dropping its previous results:
/// it must refuse exactly when `run` would, and go back to allowing starts afterwards.
#[tokio::test]
async fn check_can_start_refuses_while_a_run_is_in_flight() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg);
    let s = Arc::new(env.session().await);
    assert!(s.check_can_start().is_ok());

    let running = s.clone();
    let handle = tokio::spawn(async move {
        let mut sink = CollectSink::new();
        running
            .run_text("select pg_sleep(1)", &RunOptions::default(), &mut sink)
            .await
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while s.state() != SessionState::Running {
        assert!(Instant::now() < deadline, "the run never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let e = s.check_can_start().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused, "{e:?}");

    let summary = handle.await.unwrap().unwrap();
    assert_eq!(summary.outcome, RunOutcome::Completed);
    assert!(s.check_can_start().is_ok());
}
