//! Standalone restore against a real server (v1.1 Epic 2): a real `pg_dump` archive restored by
//! the app's own pipeline into a new database, plus the "never leave a half database" rule.
//! Skips with a NOTICE without Docker or pg client tools.

mod harness;

use harness::{PgFixture, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{ConnectionManager, ManagerOptions, NoTunnels};
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_pgtools::{
    ArchiveCheck, BackupOptions, DiscoveryOptions, PgPassDir, RunContext, ToolEndpoint, ToolKind,
    ToolPassword,
};
use pgwhy_workflow::*;
use std::sync::Arc;
use std::time::Duration;

fn entry(fix: &PgFixture) -> ConnectionEntry {
    ConnectionEntry {
        id: ConnectionId::new(),
        name: "local".into(),
        group: None,
        host: fix.host.clone(),
        port: fix.port,
        database: "postgres".into(),
        user: PG_USER.into(),
        maintenance_db: "postgres".into(),
        sslmode: SslMode::Disable,
        ssl_root_cert: None,
        tunnel: None,
        env_tag: EnvTag::Local,
        password_saved: true,
    }
}

struct Env {
    fix: PgFixture,
    engine: Engine,
    target: ConnectionEntry,
    dir: tempfile::TempDir,
}

async fn env() -> Option<Env> {
    let reg = pgwhy_pgtools::discover(&DiscoveryOptions::default());
    if reg.select(ToolKind::PgDump, 16).is_none() || reg.select(ToolKind::PgRestore, 16).is_none() {
        eprintln!("NOTICE: no pg_dump/pg_restore >= 16 — skipping restore integration test");
        return None;
    }
    let fix = harness::pg(16).await?;
    let target = entry(&fix);
    let vault = Arc::new(Vault::new(Arc::new(MemoryStore::default())));
    vault
        .store(
            &SecretKey::new(&target.id, SecretKind::DbPassword),
            Secret::new(PG_PASSWORD),
            true,
        )
        .unwrap();
    let manager = ConnectionManager::new(vault, Arc::new(NoTunnels), ManagerOptions::default());
    manager.set_entries([target.clone()]);
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(RealDeps {
        manager,
        tunnels: Arc::new(NoTunnels),
        scripts: Arc::new(std::sync::Mutex::new(
            pgwhy_core::scripts::Library::create(dir.path().join("scripts")).unwrap(),
        )),
        discovery: DiscoveryOptions::default(),
        pgpass_dir: PgPassDir::new(dir.path().join("pgpass")).unwrap(),
    });
    let mut config = EngineConfig::new(dir.path().join("runs"), dir.path().join("backups"));
    config.session_drain_timeout = Duration::from_secs(10);
    let engine = Engine::with_ops(
        config,
        Arc::new(RealOps::new(deps.clone())),
        Some(deps.clone()),
    );
    Some(Env {
        fix,
        engine,
        target,
        dir,
    })
}

impl Env {
    async fn exec(&self, db: &str, sql: &str) {
        let (c, conn) = tokio_postgres::connect(&self.fix.conninfo(db), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        c.batch_execute(sql).await.unwrap();
    }

    async fn count(&self, db: &str, table: &str) -> i64 {
        let (c, conn) = tokio_postgres::connect(&self.fix.conninfo(db), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        c.query_one(&format!("select count(*) from {table}"), &[])
            .await
            .unwrap()
            .get(0)
    }

    /// A real custom-format archive of `db`, made with the machine's pg_dump and the app's own
    /// pgpass path.
    async fn dump(&self, db: &str, out: &std::path::Path) -> pgwhy_pgtools::ArchiveInspection {
        let reg = pgwhy_pgtools::discover(&DiscoveryOptions::default());
        let pg_dump = reg.select(ToolKind::PgDump, 16).unwrap().clone();
        let pg_restore = reg.select(ToolKind::PgRestore, 16).unwrap().clone();
        let ctx = RunContext::new(PgPassDir::new(self.dir.path().join("pgpass-dump")).unwrap());
        let ep = ToolEndpoint {
            host: self.fix.host.clone(),
            hostaddr: None,
            port: self.fix.port,
            user: PG_USER.into(),
            dbname: db.into(),
            sslmode: "disable".into(),
            sslrootcert: None,
        };
        pgwhy_pgtools::backup(
            &pg_dump,
            &ep,
            Some(&ToolPassword::new(PG_PASSWORD)),
            out,
            &BackupOptions {
                format: pgwhy_pgtools::DumpFormat::Custom,
                log_path: Some(out.with_extension("log")),
                ..Default::default()
            },
            &ctx,
            |_| {},
        )
        .await
        .unwrap();
        pgwhy_pgtools::inspect_archive(&pg_restore, out, ArchiveCheck::TocOnly, &ctx)
            .await
            .unwrap()
    }

    fn request(
        &self,
        database: &str,
        archive: std::path::PathBuf,
        inspection: pgwhy_pgtools::ArchiveInspection,
        mode: RestoreMode,
    ) -> RestoreRequest {
        let reg = pgwhy_pgtools::discover(&DiscoveryOptions::default());
        RestoreRequest {
            connection: self.target.clone(),
            database: database.into(),
            archive,
            inspection,
            mode,
            locale: None,
            settings: RestoreSettings::default(),
            typed_name: None,
            backup: None,
            tool: reg.select(ToolKind::PgRestore, 16).unwrap().clone(),
            password: Some(ToolPassword::new(PG_PASSWORD)),
            on_event: None,
        }
    }
}

#[tokio::test]
async fn restores_a_real_archive_into_a_new_database() {
    let Some(env) = env().await else { return };
    env.exec("postgres", "create database src_db").await;
    env.exec(
        "src_db",
        "create table t (id int primary key, name text); \
         insert into t select g, 'row ' || g from generate_series(1, 25) g;",
    )
    .await;

    let archive = env.dir.path().join("src.dump");
    let inspection = env.dump("src_db", &archive).await;
    assert!(inspection.reusable, "{:?}", inspection.reason);
    assert_eq!(
        inspection.toc.as_ref().unwrap().header.dbname.as_deref(),
        Some("src_db")
    );

    let rec = env
        .engine
        .restore(env.request("restored_db", archive, inspection, RestoreMode::NewDatabase))
        .await
        .unwrap();
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert_eq!(env.count("restored_db", "t").await, 25);
    assert_eq!(
        rec.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["create", "restore", "verify"]
    );
    assert!(rec.steps[1].objects_done.unwrap_or(0) > 0);
    // the archive is on record and the operation is in history
    assert_eq!(env.engine.get_operation(&rec.id).unwrap(), rec);
    assert_eq!(env.engine.list_history().unwrap().len(), 1);
}

#[tokio::test]
async fn restoring_on_top_keeps_the_database_and_its_own_objects() {
    let Some(env) = env().await else { return };
    env.exec("postgres", "create database src_db").await;
    env.exec(
        "src_db",
        "create table t (id int primary key); insert into t values (1), (2);",
    )
    .await;
    let archive = env.dir.path().join("src.dump");
    let inspection = env.dump("src_db", &archive).await;

    // An existing database with an object the archive does not contain. On-top is additive:
    // it applies what the archive has and never drops what it lacks. (A table that already
    // exists is a conflict pg_restore reports — that is why "replace" is a separate mode.)
    env.exec("postgres", "create database live_db").await;
    env.exec("live_db", "create table only_here (x int);").await;

    let mut req = env.request("live_db", archive, inspection, RestoreMode::OnTop);
    req.typed_name = Some("live_db".into());
    let rec = env.engine.restore(req).await.unwrap();
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert_eq!(env.count("live_db", "t").await, 2);
    // on-top is additive: the object the archive does not contain survives
    assert_eq!(env.count("live_db", "only_here").await, 0);
    assert!(rec.restore().unwrap().backup.is_none());
}

#[tokio::test]
async fn a_corrupt_archive_fails_before_creating_anything() {
    let Some(env) = env().await else { return };
    let archive = env.dir.path().join("broken.dump");
    std::fs::write(&archive, b"this is not an archive").unwrap();
    let reg = pgwhy_pgtools::discover(&DiscoveryOptions::default());
    let pg_restore = reg.select(ToolKind::PgRestore, 16).unwrap().clone();
    let ctx = RunContext::new(PgPassDir::new(env.dir.path().join("pgpass-inspect")).unwrap());
    let inspection =
        pgwhy_pgtools::inspect_archive(&pg_restore, &archive, ArchiveCheck::TocOnly, &ctx)
            .await
            .unwrap();
    assert!(
        !inspection.reusable,
        "an unreadable archive is not reusable"
    );

    let rec = env
        .engine
        .restore(env.request(
            "never_created",
            archive,
            inspection,
            RestoreMode::NewDatabase,
        ))
        .await
        .unwrap_err();
    // the create happened first, so the failure is at the restore step and the database is
    // cleaned up again — nothing is left behind
    assert_eq!(rec.kind, pgwhy_core::ErrorKind::ToolFailed, "{rec:?}");
    let (c, conn) = tokio_postgres::connect(&env.fix.conninfo("postgres"), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let n: i64 = c
        .query_one(
            "select count(*) from pg_database where datname = 'never_created'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0, "the half database was dropped");
}

#[tokio::test]
async fn replaces_an_existing_database_and_keeps_its_name() {
    let Some(env) = env().await else { return };
    env.exec("postgres", "create database src_db").await;
    env.exec(
        "src_db",
        "create table t (id int primary key); insert into t select g from generate_series(1, 25) g;",
    )
    .await;
    let archive = env.dir.path().join("src.dump");
    let inspection = env.dump("src_db", &archive).await;

    // the database that will be replaced, with something the archive does not have
    env.exec("postgres", "create database live_db").await;
    env.exec(
        "live_db",
        "create table t (id int primary key); insert into t values (1); \
         create table only_in_the_old_copy (x int);",
    )
    .await;

    let mut req = env.request("live_db", archive, inspection, RestoreMode::Replace);
    req.typed_name = Some("live_db".into());
    let rec = env.engine.restore(req).await.unwrap();
    assert_eq!(
        rec.status,
        RunStatus::Succeeded,
        "{:?} / {:?}",
        rec.error,
        rec.warnings
    );

    // the name is kept and the contents are the archive's, not the old copy's
    assert_eq!(env.count("live_db", "t").await, 25);
    let (c, conn) = tokio_postgres::connect(&env.fix.conninfo("live_db"), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let old_table: Option<String> = c
        .query_one(
            "select to_regclass('public.only_in_the_old_copy')::text",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        old_table.is_none(),
        "the old copy was replaced wholesale: {old_table:?}"
    );

    // no staging database and no `_old` left behind
    let dbs: Vec<String> = c
        .query("select datname::text from pg_database order by 1", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    assert!(
        !dbs.iter()
            .any(|d| d.starts_with("live_db_staging") || d == "live_db_old"),
        "{dbs:?}"
    );
    let swap = rec.restore().unwrap().swap.as_ref().unwrap();
    assert_eq!(swap.renames_completed, 2);
    assert_eq!(swap.work_db_state, pgwhy_workflow::WorkDbState::SwappedIn);
}
