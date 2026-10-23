//! Lifecycle verbs against a real Postgres (v1.1 Epic 4): the generated `CREATE DATABASE`
//! syntax, the real drain/terminate path, and the prepared-transaction refusal. Skips with a
//! NOTICE without Docker.

mod harness;

use harness::{PgFixture, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{ConnectionManager, ManagerOptions, NoTunnels};
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_pgtools::{DiscoveryOptions, PgPassDir};
use pgwhy_workflow::*;
use std::sync::Arc;
use std::time::Duration;

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
    fix: PgFixture,
    engine: Engine,
    dir: tempfile::TempDir,
    target: ConnectionEntry,
}

async fn env(args: &[&str]) -> Option<Env> {
    let fix = harness::pg_with_args(16, args).await?;
    let target = entry(&fix, "local", EnvTag::Local);
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
        manager: manager.clone(),
        tunnels: Arc::new(NoTunnels),
        scripts: Arc::new(std::sync::Mutex::new(
            pgwhy_core::scripts::Library::create(dir.path().join("scripts")).unwrap(),
        )),
        discovery: DiscoveryOptions::default(),
        pgpass_dir: PgPassDir::new(dir.path().join("pgpass")).unwrap(),
    });
    let mut config = EngineConfig::new(dir.path().join("runs"), dir.path().join("backups"));
    config.session_drain_timeout = Duration::from_secs(10);
    let engine = Engine::with_ops(config, Arc::new(RealOps::new(deps.clone())), Some(deps));
    Some(Env {
        fix,
        engine,
        dir,
        target,
    })
}

impl Env {
    fn target(&self, db: &str) -> DbTarget {
        DbTarget {
            connection_id: self.target.id.clone(),
            connection_name: self.target.name.clone(),
            server: self.target.server_key(),
            env_tag: EnvTag::Local,
            database: db.into(),
        }
    }

    async fn client(&self, db: &str, app: &str) -> tokio_postgres::Client {
        let (c, conn) = tokio_postgres::connect(
            &format!("{} application_name={app}", self.fix.conninfo(db)),
            tokio_postgres::NoTls,
        )
        .await
        .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        c
    }

    async fn databases(&self) -> Vec<String> {
        self.client("postgres", "test-check")
            .await
            .query("select datname::text from pg_database order by 1", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect()
    }
}

/// The locale of `postgres`, read the way the app's create command does.
async fn postgres_locale(env: &Env) -> LocaleSpec {
    let c = env.client("postgres", "test-locale").await;
    let row = c
        .query_one(&locale_sql(160_000, "datname = 'postgres'"), &[])
        .await
        .unwrap();
    locale_from_row(&row)
}

#[tokio::test]
async fn create_rename_and_drop_against_a_real_server() {
    let Some(env) = env(&[]).await else { return };
    let locale = postgres_locale(&env).await;

    let created = env
        .engine
        .create_database(CreateDatabaseRequest {
            target: env.target("scratch_a"),
            locale: Some(locale.clone()),
            comment: Some("made by the test".into()),
        })
        .await
        .unwrap();
    assert_eq!(created.status, RunStatus::Succeeded, "{:?}", created.error);
    let dbs = env.databases().await;
    assert!(dbs.contains(&"scratch_a".to_string()), "{dbs:?}");
    // the comment landed (a separate statement after the create)
    let c = env.client("postgres", "test-check").await;
    let comment: Option<String> = c
        .query_one(
            "select shobj_description(oid, 'pg_database') from pg_database where datname = 'scratch_a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(comment.as_deref(), Some("made by the test"));

    let renamed = env
        .engine
        .rename_database(RenameDatabaseRequest {
            target: env.target("scratch_a"),
            to: "scratch_b".into(),
        })
        .await
        .unwrap();
    assert_eq!(renamed.status, RunStatus::Succeeded, "{:?}", renamed.error);
    let dbs = env.databases().await;
    assert!(!dbs.contains(&"scratch_a".to_string()), "{dbs:?}");
    assert!(dbs.contains(&"scratch_b".to_string()), "{dbs:?}");

    let dropped = env
        .engine
        .drop_database(DropDatabaseRequest {
            target: env.target("scratch_b"),
            typed_name: "scratch_b".into(),
            backup: None,
        })
        .await
        .unwrap();
    assert_eq!(dropped.status, RunStatus::Succeeded, "{:?}", dropped.error);
    assert!(!env.databases().await.contains(&"scratch_b".to_string()));

    // history: three operations, all terminal, all readable back
    let ops = env.engine.list_operations().unwrap();
    assert_eq!(ops.len(), 3);
    assert!(ops.iter().all(|o| o.status == RunStatus::Succeeded));
    assert_eq!(env.engine.list_history().unwrap().len(), 3);
    drop(env.dir);
}

#[tokio::test]
async fn drop_terminates_live_sessions_first() {
    let Some(env) = env(&[]).await else { return };
    env.engine
        .create_database(CreateDatabaseRequest {
            target: env.target("busy_db"),
            locale: None,
            comment: None,
        })
        .await
        .unwrap();

    // a real session attached to the database we are about to drop
    let _client = env.client("busy_db", "pgwhy-test-client").await;

    let rec = env
        .engine
        .drop_database(DropDatabaseRequest {
            target: env.target("busy_db"),
            typed_name: "busy_db".into(),
            backup: None,
        })
        .await
        .unwrap();
    assert_eq!(rec.status, RunStatus::Succeeded, "{:?}", rec.error);
    assert!(!env.databases().await.contains(&"busy_db".to_string()));
}

#[tokio::test]
async fn drop_refuses_a_database_with_a_prepared_transaction() {
    let Some(env) = env(&["-c", "max_prepared_transactions=10"]).await else {
        return;
    };
    env.engine
        .create_database(CreateDatabaseRequest {
            target: env.target("prep_db"),
            locale: None,
            comment: None,
        })
        .await
        .unwrap();

    let c = env.client("prep_db", "test-prepared").await;
    c.batch_execute(
        "create table t(x int); begin; insert into t values (1); prepare transaction 'g1';",
    )
    .await
    .unwrap();

    let e = env
        .engine
        .drop_database(DropDatabaseRequest {
            target: env.target("prep_db"),
            typed_name: "prep_db".into(),
            backup: None,
        })
        .await
        .unwrap_err();
    assert_eq!(e.kind, pgwhy_core::ErrorKind::Refused, "{e:?}");
    assert!(e.message.contains("prepared transaction"), "{}", e.message);
    // the prepared transaction is still there — pgWhy never discards one
    let still: i64 = env
        .client("postgres", "test-check")
        .await
        .query_one(
            "select count(*) from pg_prepared_xacts where gid = 'g1'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(still, 1);
    assert!(env.databases().await.contains(&"prep_db".to_string()));

    // leaving it prepared, clean up so the container is not left with an open xact
    let _ = c.batch_execute("rollback prepared 'g1'").await;
}
