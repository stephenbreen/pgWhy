//! End-to-end Refresh runs against two containerized Postgres 16 servers ("remote" source and
//! "local" target) with the machine's real pg client tools: the happy path + restore failure
//! and retry, create-mode, drop-first, swap closing pgWhy's own and external sessions,
//! preflight refusals (prepared xacts, environment-tag re-check, COMMIT in the script, run
//! lock), and a kill between the swap's renames recovered by rollback. History is scanned for
//! the password. Skips with a NOTICE without Docker or pg tools ≥ 16.

mod harness;

use harness::{PgFixture, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{BoxFuture, ConnectionManager, ManagerOptions, NoTunnels, Purpose};
use pgwhy_core::scripts::Library;
use pgwhy_core::secret::{CredentialSource, MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_core::AppResult;
use pgwhy_pgtools::{DiscoveryOptions, PgPassDir, ToolKind};
use pgwhy_workflow::history::SessionInfo;
use pgwhy_workflow::ops::*;
use pgwhy_workflow::staging::LocaleSpec;
use pgwhy_workflow::*;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------------- fixture

async fn client(
    fix: &PgFixture,
    db: &str,
    app: &str,
) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
    let (c, conn) = tokio_postgres::connect(
        &format!("{} application_name={app}", fix.conninfo(db)),
        tokio_postgres::NoTls,
    )
    .await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(c)
}

async fn exec(fix: &PgFixture, db: &str, sql: &str) {
    client(fix, db, "test-setup")
        .await
        .unwrap()
        .batch_execute(sql)
        .await
        .unwrap();
}

async fn strings(fix: &PgFixture, db: &str, sql: &str) -> Vec<String> {
    let c = client(fix, db, "test-check").await.unwrap();
    c.query(sql, &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect()
}

async fn databases(fix: &PgFixture) -> Vec<String> {
    strings(
        fix,
        "postgres",
        "select datname::text from pg_database order by 1",
    )
    .await
}

fn entry(fix: &PgFixture, name: &str, tag: EnvTag) -> ConnectionEntry {
    ConnectionEntry {
        id: ConnectionId::new(),
        name: name.into(),
        group: None,
        host: fix.host.clone(),
        port: fix.port,
        database: "postgres".into(),
        user: PG_USER.into(),
        maintenance_db: "postgres".into(),
        sslmode: SslMode::Disable,
        ssl_root_cert: None,
        tunnel: None,
        env_tag: tag,
        password_saved: true,
    }
}

struct Env {
    remote: PgFixture,
    local: PgFixture,
    deps: Arc<RealDeps>,
    manager: ConnectionManager,
    lib: Arc<Mutex<Library>>,
    store: WorkflowStore,
    config: EngineConfig,
    engine: Engine,
    src: ConnectionEntry,
    tgt: ConnectionEntry,
    dir: tempfile::TempDir,
}

const SEED: &str = "create table customers (id int primary key, email text not null);
    insert into customers select g, 'user' || g || '@example.com' from generate_series(1, 50) g;
    create table orders (id int primary key, customer_id int references customers(id));
    insert into orders select g, 1 + g % 50 from generate_series(1, 200) g;
    create index orders_customer_idx on orders(customer_id);";

async fn env() -> Option<Env> {
    let reg = pgwhy_pgtools::discover(&DiscoveryOptions::default());
    if reg.select(ToolKind::PgDump, 16).is_none() || reg.select(ToolKind::PgRestore, 16).is_none() {
        eprintln!("NOTICE: no pg_dump/pg_restore >= 16 found (searched {:?}) — skipping workflow integration test", reg.searched);
        return None;
    }
    let (remote, local) = tokio::join!(
        harness::pg(16),
        harness::pg_with_args(16, &["-c", "max_prepared_transactions=10"])
    );
    let (remote, local) = (remote?, local?);
    exec(&remote, "postgres", "create database shop").await;
    exec(&remote, "shop", SEED).await;
    exec(&local, "postgres", "create database shop").await;
    exec(
        &local,
        "shop",
        "create table stale (x int); insert into stale values (1);",
    )
    .await;
    exec(
        &local,
        "postgres",
        "comment on database shop is 'the local copy'",
    )
    .await;

    let src = entry(&remote, "remote", EnvTag::Production);
    let tgt = entry(&local, "local", EnvTag::Local);
    let vault = Arc::new(Vault::new(Arc::new(MemoryStore::default())));
    for e in [&src, &tgt] {
        vault
            .store(
                &SecretKey::new(&e.id, SecretKind::DbPassword),
                Secret::new(PG_PASSWORD),
                true,
            )
            .unwrap();
    }
    let manager = ConnectionManager::new(vault, Arc::new(NoTunnels), ManagerOptions::default());
    manager.set_entries([src.clone(), tgt.clone()]);
    let dir = tempfile::tempdir().unwrap();
    let lib = Arc::new(Mutex::new(
        Library::create(dir.path().join("scripts")).unwrap(),
    ));
    let deps = Arc::new(RealDeps {
        manager: manager.clone(),
        tunnels: Arc::new(NoTunnels),
        scripts: lib.clone(),
        discovery: DiscoveryOptions::default(),
        pgpass_dir: PgPassDir::new(dir.path().join("pgpass")).unwrap(),
    });
    let store = WorkflowStore::new(dir.path().join("workflows"));
    let mut config = EngineConfig::new(dir.path().join("runs"), dir.path().join("backups"));
    config.workflows = Some(store.clone());
    config.session_drain_timeout = Duration::from_secs(5);
    let engine = Engine::with_ops(
        config.clone(),
        Arc::new(RealOps::new(deps.clone())),
        Some(deps.clone()),
    );
    Some(Env {
        remote,
        local,
        deps,
        manager,
        lib,
        store,
        config,
        engine,
        src,
        tgt,
        dir,
    })
}

impl Env {
    fn workflow(
        &self,
        target_db: &str,
        strategy: ReplaceStrategy,
        script: Option<&str>,
    ) -> RefreshWorkflow {
        let script_id = script.map(|content| {
            let name = format!("s{}.sql", uuid_like());
            self.lib.lock().unwrap().save(&name, content).unwrap().id.0
        });
        let wf = RefreshWorkflow {
            id: WorkflowId::new(),
            name: format!("Refresh {target_db}"),
            source: DbRef {
                connection: self.src.id.clone(),
                database: "shop".into(),
            },
            target: DbRef {
                connection: self.tgt.id.clone(),
                database: target_db.into(),
            },
            post_restore_script: script_id,
            options: RefreshOptions {
                strategy,
                ..RefreshOptions::default()
            },
            remembered_confirmation: None,
            has_succeeded: false,
        };
        self.store.save(&wf).unwrap();
        wf
    }

    async fn run(&self, engine: &Engine, report: &PreflightReport) -> RunRecord {
        let ticket = report
            .ticket
            .as_deref()
            .unwrap_or_else(|| panic!("preflight refused: {:#?}", report.checks));
        let fp = report.confirmation.as_ref().unwrap().fingerprint.clone();
        let id = engine
            .start(
                ticket,
                StartRequest {
                    fingerprint: fp,
                    remember: false,
                },
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(180), engine.wait(&id))
            .await
            .expect("run finished")
            .unwrap()
    }
}

fn uuid_like() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn assert_no_password_under(dir: &Path) {
    for ent in std::fs::read_dir(dir).unwrap().flatten() {
        let p = ent.path();
        if p.is_dir() {
            assert_no_password_under(&p);
        } else {
            let bytes = std::fs::read(&p).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(PG_PASSWORD),
                "password found in {}",
                p.display()
            );
        }
    }
}

fn check<'a>(r: &'a PreflightReport, id: &str) -> &'a PreflightCheck {
    r.check(id)
        .unwrap_or_else(|| panic!("no check {id}: {:#?}", r.checks))
}

// ---------------------------------------------------------------------------------- tests

/// Happy path, sessions closed at the swap, and no credential in history.
#[tokio::test]
async fn staging_swap_with_script_closes_sessions() {
    let Some(env) = env().await else { return };
    let wf = env.workflow(
        "shop",
        ReplaceStrategy::StagingSwap,
        Some("update customers set email = 'redacted@example.com';\ncreate table refreshed_marker (at timestamptz default now());"),
    );
    // pgWhy's own editor tab on the target, idle in an open transaction
    let tab = env
        .manager
        .open(&env.tgt.id, "shop", Purpose::EditorTab)
        .await
        .unwrap();
    tab.client.batch_execute("begin; select 1").await.unwrap();
    // an external session on the target
    let external = client(&env.local, "shop", "external-app").await.unwrap();
    external.batch_execute("select 1").await.unwrap();

    let report = env.engine.preflight(&wf, vec![]).await;
    assert!(report.refused.is_none(), "{:#?}", report.checks);
    let conf = report.confirmation.as_ref().unwrap();
    assert_eq!(conf.target_db, "shop");
    assert_eq!(conf.strategy, ReplaceStrategy::StagingSwap);
    assert!(!conf.create_mode);
    assert!(conf
        .target_server_label
        .contains(&env.local.port.to_string()));
    assert!(
        conf.warnings
            .iter()
            .any(|w| w.contains("pgWhy has 1 open connection")),
        "{:?}",
        conf.warnings
    );
    assert!(conf
        .warnings
        .iter()
        .any(|w| w.contains("other roles lose access")));
    assert_eq!(check(&report, "locale").status, CheckStatus::Ok);
    assert_eq!(check(&report, "script").status, CheckStatus::Ok);
    assert!(report.missing_secrets.is_empty());

    let rec = env.run(&env.engine, &report).await;
    assert_eq!(rec.status, RunStatus::Succeeded, "{:#?}", rec.error);

    // the target is the refreshed copy with the script applied
    let emails = strings(&env.local, "shop", "select distinct email from customers").await;
    assert_eq!(emails, vec!["redacted@example.com".to_string()]);
    let tables = strings(
        &env.local,
        "shop",
        "select tablename::text from pg_tables where schemaname = 'public' order by 1",
    )
    .await;
    assert_eq!(tables, vec!["customers", "orders", "refreshed_marker"]);
    let dbs = databases(&env.local).await;
    assert!(
        !dbs.iter()
            .any(|d| d.contains("_staging_") || d.ends_with("_old")),
        "{dbs:?}"
    );
    // marker comment cleared on the swapped-in database
    let comment = strings(&env.local, "postgres", "select coalesce(shobj_description(oid, 'pg_database'), '') from pg_database where datname = 'shop'").await;
    assert_eq!(comment, vec![String::new()]);

    // pgWhy's own tab closed client-side, the external session terminated
    assert!(tab.is_closed());
    assert!(external.simple_query("select 1").await.is_err());

    // report fields
    assert_eq!(rec.renames_completed, 2);
    assert!(rec
        .tools
        .pg_dump
        .as_ref()
        .unwrap()
        .version
        .starts_with("16"));
    let b = rec.backup.as_ref().unwrap();
    assert!(b.path.exists() && b.size_bytes > 0 && b.reusable);
    assert!(b.dumped_by.as_deref().unwrap().starts_with("16"));
    assert!(b.toc_items.unwrap() > 0);
    assert_eq!(rec.script.as_ref().unwrap().sha256.len(), 64);
    assert_eq!(rec.locale.as_ref().unwrap().encoding, "UTF8");
    assert_eq!(rec.credentials.source, CredentialSource::Keyring);
    assert_eq!(rec.source.database, "shop");
    assert_eq!(rec.target.port, env.local.port);
    assert!(rec
        .step(StepId::Dump)
        .unwrap()
        .log_path
        .as_ref()
        .unwrap()
        .exists());
    assert!(rec.step(StepId::Restore).unwrap().objects_done.unwrap() > 0);
    assert!(env.store.get(&wf.id).unwrap().has_succeeded);
    // credentials: none in history/logs, no pgpass left behind
    assert_no_password_under(&env.dir.path().join("runs"));
    assert_eq!(
        std::fs::read_dir(env.dir.path().join("pgpass"))
            .unwrap()
            .count(),
        0
    );
    // remembering is now offered (Local target, after a success)
    let again = env
        .engine
        .preflight(&env.store.get(&wf.id).unwrap(), vec![])
        .await;
    assert!(again.confirmation.as_ref().unwrap().can_remember);
    env.engine
        .discard_preflight(again.ticket.as_deref().unwrap());
}

/// A failing object → target untouched, report names it; fix, then retry
/// from restore reusing the validated backup.
#[tokio::test]
async fn restore_failure_then_retry_from_restore() {
    let Some(env) = env().await else { return };
    exec(&env.remote, "postgres", "create role remote_only").await;
    exec(
        &env.remote,
        "shop",
        "alter table orders owner to remote_only",
    )
    .await;
    let mut wf = env.workflow("shop", ReplaceStrategy::StagingSwap, None);
    wf.options.no_owner = false;
    wf.options.no_privileges = false;
    env.store.save(&wf).unwrap();

    let report = env.engine.preflight(&wf, vec![]).await;
    let rec = env.run(&env.engine, &report).await;
    assert_eq!(rec.status, RunStatus::FailedAtStep, "{:#?}", rec);
    assert_eq!(rec.failed_step, Some(StepId::Restore));
    let err = rec.error.as_ref().unwrap();
    assert!(
        err.message.contains("remote_only")
            || rec
                .failed_items
                .iter()
                .any(|i| i.error.contains("remote_only")),
        "{err:?} {:?}",
        rec.failed_items
    );
    assert!(!rec.failed_items.is_empty());
    assert_eq!(rec.headline.as_deref(), Some("shop is untouched."));
    assert_eq!(
        strings(
            &env.local,
            "shop",
            "select tablename::text from pg_tables where schemaname='public'"
        )
        .await,
        vec!["stale"]
    );
    let staging = rec.staging_db.clone().unwrap();
    assert!(databases(&env.local).await.contains(&staging));
    let offer = rec
        .actions
        .iter()
        .find(|a| a.retry.as_ref().is_some_and(|r| r.from == StepId::Restore))
        .unwrap();
    assert!(offer.enabled, "{:?}", offer.disabled_reason);

    // fix the cause on the target server, then retry from restore
    exec(&env.local, "postgres", "create role remote_only").await;
    let report = env.engine.retry(&rec.run_id, StepId::Restore, vec![]).await;
    assert!(report.refused.is_none(), "{:#?}", report);
    let r = report.retry.as_ref().unwrap();
    assert!(r.reuses_backup && r.begins_at == StepId::CreateDatabase);
    assert_eq!(check(&report, "backup").status, CheckStatus::Ok);
    assert_eq!(check(&report, "source").status, CheckStatus::Skipped);
    let rec2 = env.run(&env.engine, &report).await;
    assert_eq!(rec2.status, RunStatus::Succeeded, "{:#?}", rec2.error);
    assert_eq!(rec2.step(StepId::Dump).unwrap().status, StepStatus::Reused);
    assert_eq!(
        rec2.backup.as_ref().unwrap().path,
        rec.backup.as_ref().unwrap().path
    );
    assert_eq!(rec2.retry_of.as_ref(), Some(&rec.run_id));
    let owner = strings(
        &env.local,
        "shop",
        "select tableowner::text from pg_tables where tablename = 'orders'",
    )
    .await;
    assert_eq!(owner, vec!["remote_only"]);
    assert!(
        !databases(&env.local).await.contains(&staging),
        "the failed run's staging was dropped"
    );
}

#[tokio::test]
async fn create_mode_then_drop_first() {
    let Some(env) = env().await else { return };
    let mut wf = env.workflow("newshop", ReplaceStrategy::StagingSwap, None);
    let report = env.engine.preflight(&wf, vec![]).await;
    let conf = report.confirmation.as_ref().unwrap();
    assert!(conf.create_mode);
    assert!(conf.warnings.iter().any(|w| w.contains("will be created")));
    let rec = env.run(&env.engine, &report).await;
    assert_eq!(rec.status, RunStatus::Succeeded, "{:#?}", rec.error);
    assert!(rec.create_mode && rec.old_db.is_none());
    assert_eq!(
        strings(&env.local, "newshop", "select count(*)::text from orders").await,
        vec!["200"]
    );

    wf.options.strategy = ReplaceStrategy::DropFirst;
    env.store.save(&wf).unwrap();
    exec(&env.local, "newshop", "create table local_only (x int)").await;
    let report = env.engine.preflight(&wf, vec![]).await;
    let conf = report.confirmation.as_ref().unwrap();
    assert!(!conf.create_mode && conf.staging_db.is_none());
    assert!(conf
        .warnings
        .iter()
        .any(|w| w.contains("leave no local database")));
    let rec = env.run(&env.engine, &report).await;
    assert_eq!(rec.status, RunStatus::Succeeded, "{:#?}", rec.error);
    assert!(rec.step(StepId::Swap).is_none());
    assert_eq!(
        rec.step(StepId::DropTarget).unwrap().status,
        StepStatus::Succeeded
    );
    let tables = strings(
        &env.local,
        "newshop",
        "select tablename::text from pg_tables where schemaname='public' order by 1",
    )
    .await;
    assert_eq!(tables, vec!["customers", "orders"]);
    assert!(!databases(&env.local)
        .await
        .iter()
        .any(|d| d.contains("_staging_") || d.ends_with("_old")));
}

#[tokio::test]
async fn preflight_refusals_are_actionable_and_harmless() {
    let Some(env) = env().await else { return };
    let wf = env.workflow("shop", ReplaceStrategy::StagingSwap, None);

    // prepared transaction on the target → refused, fix offered, fix works
    exec(
        &env.local,
        "shop",
        "begin; insert into stale values (2); prepare transaction 'pgwhy-gid-1';",
    )
    .await;
    let report = env.engine.preflight(&wf, vec![]).await;
    assert!(report.ticket.is_none());
    let c = check(&report, "prepared_xacts");
    assert_eq!(c.status, CheckStatus::Fail);
    assert!(c.message.contains("pgwhy-gid-1"));
    let fix = c.fixes[0].clone();
    assert_eq!(fix.kind, PreflightFixKind::RollbackPrepared);
    env.engine.apply_preflight_fix(&wf, &fix).await.unwrap();
    let report = env.engine.preflight(&wf, vec![]).await;
    assert!(report.ticket.is_some(), "{:#?}", report.checks);

    // the pending ticket holds the server: another workflow on it is refused at the lock
    let other = env.workflow("other", ReplaceStrategy::StagingSwap, None);
    let blocked = env.engine.preflight(&other, vec![]).await;
    assert_eq!(check(&blocked, "run_lock").status, CheckStatus::Fail);
    assert_eq!(blocked.checks.len(), 1, "refused before anything else ran");
    env.engine
        .discard_preflight(report.ticket.as_deref().unwrap());

    // explicit COMMIT in the script → refused at preflight
    let with_commit = env.workflow(
        "shop",
        ReplaceStrategy::StagingSwap,
        Some("update stale set x = 3;\ncommit;"),
    );
    let report = env.engine.preflight(&with_commit, vec![]).await;
    let c = check(&report, "script");
    assert_eq!(c.status, CheckStatus::Fail);
    assert!(c.message.contains("COMMIT"), "{}", c.message);
    assert!(report.refused.is_some() && report.ticket.is_none());

    // a psql meta-command → refused
    let meta = env.workflow(
        "shop",
        ReplaceStrategy::StagingSwap,
        Some("\\c other\nselect 1;"),
    );
    let report = env.engine.preflight(&meta, vec![]).await;
    assert!(check(&report, "script").message.contains("meta-commands"));

    // CONCURRENTLY under single-transaction mode → refused with the no-txn hint; allowed
    // once the workflow opts into "run without transaction"
    let mut conc = env.workflow(
        "shop",
        ReplaceStrategy::StagingSwap,
        Some("create index concurrently c_idx on customers(email);"),
    );
    let report = env.engine.preflight(&conc, vec![]).await;
    assert!(check(&report, "script")
        .hint
        .as_deref()
        .unwrap()
        .contains("without transaction"));
    conc.options.script_mode = ScriptMode::NoTransaction;
    let report = env.engine.preflight(&conc, vec![]).await;
    assert_eq!(check(&report, "script").status, CheckStatus::Ok);
    env.engine
        .discard_preflight(report.ticket.as_deref().unwrap());

    // the target re-tagged Production outside the workflow editor → the environment-tag re-check refuses
    let mut prod = env.tgt.clone();
    prod.env_tag = EnvTag::Production;
    env.manager.upsert_entry(prod).await;
    let report = env.engine.preflight(&wf, vec![]).await;
    let c = check(&report, "environment_tag");
    assert_eq!(c.status, CheckStatus::Fail);
    assert!(c.message.contains("Local or Dev"));
    env.manager.upsert_entry(env.tgt.clone()).await;

    // nothing was touched
    assert_eq!(
        strings(&env.local, "shop", "select x::text from stale").await,
        vec!["1"]
    );
    assert!(!databases(&env.local)
        .await
        .iter()
        .any(|d| d.contains("_staging_")));
}

// --- kill between the renames: real ops with a maintenance connection whose 2nd rename hangs

struct HangSecondRename {
    inner: RealOps,
    renames: Arc<AtomicU32>,
}

struct HangMaint {
    inner: Box<dyn MaintenanceOps>,
    renames: Arc<AtomicU32>,
}

impl MaintenanceOps for HangMaint {
    fn server_major(&self) -> u32 {
        self.inner.server_major()
    }
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
    fn databases(&self) -> BoxFuture<'_, AppResult<Vec<DbFact>>> {
        self.inner.databases()
    }
    fn create_database<'a>(
        &'a self,
        name: &'a str,
        locale: Option<&'a LocaleSpec>,
    ) -> BoxFuture<'a, AppResult<()>> {
        self.inner.create_database(name, locale)
    }
    fn set_comment<'a>(
        &'a self,
        db: &'a str,
        comment: Option<&'a str>,
    ) -> BoxFuture<'a, AppResult<()>> {
        self.inner.set_comment(db, comment)
    }
    fn rename_database<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, AppResult<()>> {
        if self.renames.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
            return Box::pin(std::future::pending());
        }
        self.inner.rename_database(from, to)
    }
    fn drop_database<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<()>> {
        self.inner.drop_database(name)
    }
    fn prepared_xacts<'a>(&'a self, db: &'a str) -> BoxFuture<'a, AppResult<Vec<PreparedXact>>> {
        self.inner.prepared_xacts(db)
    }
    fn sessions<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<Vec<SessionInfo>>> {
        self.inner.sessions(dbs)
    }
    fn terminate<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<u32>> {
        self.inner.terminate(dbs)
    }
}

impl RefreshOps for HangSecondRename {
    fn dump<'a>(
        &'a self,
        plan: &'a RunPlan,
        out: &'a Path,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<DumpDone>> {
        self.inner.dump(plan, out, log, ctx)
    }
    fn inspect_backup<'a>(
        &'a self,
        plan: &'a RunPlan,
        path: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<BackupVerdict>> {
        self.inner.inspect_backup(plan, path, ctx)
    }
    fn restore<'a>(
        &'a self,
        plan: &'a RunPlan,
        archive: &'a Path,
        into_db: &'a str,
        total: Option<u64>,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>> {
        self.inner.restore(plan, archive, into_db, total, log, ctx)
    }
    fn run_script<'a>(
        &'a self,
        plan: &'a RunPlan,
        db: &'a str,
        s: &'a ScriptRecord,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ScriptDone>> {
        self.inner.run_script(plan, db, s, ctx)
    }
    fn verify<'a>(
        &'a self,
        plan: &'a RunPlan,
        db: &'a str,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>> {
        self.inner.verify(plan, db, ctx)
    }
    fn restore_archive<'a>(
        &'a self,
        target: &'a ConnectionEntry,
        tool: &'a pgwhy_pgtools::DiscoveredTool,
        password: Option<&'a pgwhy_pgtools::ToolPassword>,
        archive: &'a Path,
        into_db: &'a str,
        opts: &'a pgwhy_pgtools::RestoreOptions,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>> {
        self.inner
            .restore_archive(target, tool, password, archive, into_db, opts, log, ctx)
    }
    fn verify_database<'a>(
        &'a self,
        target: &'a ConnectionEntry,
        db: &'a str,
        sanity_query: Option<&'a str>,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>> {
        self.inner.verify_database(target, db, sanity_query, ctx)
    }
    fn maintenance<'a>(
        &'a self,
        target: &'a ConnectionId,
    ) -> BoxFuture<'a, AppResult<Box<dyn MaintenanceOps>>> {
        Box::pin(async move {
            let inner = self.inner.maintenance(target).await?;
            Ok(Box::new(HangMaint {
                inner,
                renames: self.renames.clone(),
            }) as Box<dyn MaintenanceOps>)
        })
    }
    fn probe<'a>(&'a self, conn: &'a ConnectionId, db: &'a str) -> BoxFuture<'a, AppResult<()>> {
        self.inner.probe(conn, db)
    }
    fn quiesce<'a>(
        &'a self,
        target: &'a ConnectionId,
        dbs: Vec<String>,
    ) -> BoxFuture<'a, AppResult<QuiesceToken>> {
        self.inner.quiesce(target, dbs)
    }
}

#[tokio::test]
async fn kill_between_renames_recovers_by_rollback() {
    let Some(env) = env().await else { return };
    let wf = env.workflow("shop", ReplaceStrategy::StagingSwap, None);
    let ops = HangSecondRename {
        inner: RealOps::new(env.deps.clone()),
        renames: Arc::default(),
    };
    let engine = Engine::with_ops(env.config.clone(), Arc::new(ops), Some(env.deps.clone()));
    let report = engine.preflight(&wf, vec![]).await;
    let ticket = report
        .ticket
        .clone()
        .unwrap_or_else(|| panic!("{:#?}", report.checks));
    let fp = report.confirmation.as_ref().unwrap().fingerprint.clone();
    let id = engine
        .start(
            &ticket,
            StartRequest {
                fingerprint: fp,
                remember: false,
            },
            Arc::new(|_| {}),
        )
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        if engine.get_run(&id).unwrap().renames_completed == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never reached the rename window"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // the outage window: the target name doesn't exist
    let err = client(&env.local, "shop", "external-app")
        .await
        .expect_err("target name is free mid-swap");
    let msg = err
        .as_db_error()
        .map(|d| d.message().to_string())
        .unwrap_or_else(|| format!("{err:?}"));
    assert!(msg.contains("does not exist"), "{msg}");
    assert!(
        engine.cancel(&id).is_err(),
        "cancel refused between renames"
    );

    // quit
    engine.shutdown(Duration::from_millis(300)).await;
    let rec = engine.get_run(&id).unwrap();
    assert_eq!(rec.status, RunStatus::Interrupted);
    assert_eq!(rec.renames_completed, 1);

    // restart: fresh engine, live re-inventory → complete or roll back only
    let fresh = Engine::with_ops(
        env.config.clone(),
        Arc::new(RealOps::new(env.deps.clone())),
        Some(env.deps.clone()),
    );
    let rec = fresh.reinventory(&id).await.unwrap();
    let mut kinds: Vec<_> = rec.actions.iter().map(|a| a.kind).collect();
    kinds.sort_by_key(|k| format!("{k:?}"));
    assert_eq!(
        kinds,
        vec![ActionKind::CompleteSwap, ActionKind::RollbackSwap]
    );
    assert!(rec.actions.iter().all(|a| a.enabled), "{:?}", rec.actions);
    assert!(rec.headline.as_deref().unwrap().starts_with("Outage"));
    assert!(fresh.perform(&id, ActionKind::DropStaging).await.is_err());

    let rec = fresh.perform(&id, ActionKind::RollbackSwap).await.unwrap();
    assert_eq!(rec.renames_completed, 0);
    // the previous database is back and connectable, comment restored
    assert_eq!(
        strings(&env.local, "shop", "select x::text from stale").await,
        vec!["1"]
    );
    let comment = strings(
        &env.local,
        "postgres",
        "select shobj_description(oid, 'pg_database') from pg_database where datname = 'shop'",
    )
    .await;
    assert_eq!(comment, vec!["the local copy"]);
    // staging remains, marked, and can now be dropped
    let staging = rec.staging_db.clone().unwrap();
    assert!(databases(&env.local).await.contains(&staging));
    let rec = fresh.perform(&id, ActionKind::DropStaging).await.unwrap();
    assert!(!databases(&env.local).await.contains(&staging));
    assert!(rec.actions.iter().any(|a| a.kind == ActionKind::Retry));
    assert_no_password_under(&env.dir.path().join("runs"));
}
