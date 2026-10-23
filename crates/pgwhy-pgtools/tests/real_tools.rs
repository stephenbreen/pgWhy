//! Integration with REAL pg client tools against containerized Postgres: discovery picks the
//! installed tools, a backup authenticates through the temp pgpass only (keyed on a fake
//! "real hostname" with `hostaddr` doing the dialing — the tunnel shape), the archive's TOC
//! is parsed, restore lands in a second database, archive corruption is classified, DDL is
//! pattern-safe, and the version gate refuses pg_dump 16 against a PostgreSQL 17 server.
//!
//! Skips with a NOTICE when Docker or pg client tools are unavailable.

mod harness;

use harness::{PG_PASSWORD, PG_USER};
use pgwhy_pgtools::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Discover the machine's tools; `None` (with a NOTICE) unless both pg_dump and pg_restore of
/// at least `min_major` exist.
fn real_tools(min_major: u32) -> Option<(ToolRegistry, DiscoveredTool, DiscoveredTool)> {
    let reg = discover(&DiscoveryOptions::default());
    let dump = reg.select(ToolKind::PgDump, min_major).cloned();
    let restore = reg.select(ToolKind::PgRestore, min_major).cloned();
    match (dump, restore) {
        (Some(d), Some(r)) => Some((reg, d, r)),
        _ => {
            eprintln!(
                "NOTICE: no pg_dump/pg_restore >= {min_major} found (searched {:?}) — skipping real-tools test",
                reg.searched
            );
            None
        }
    }
}

fn psql_next_to(tool: &DiscoveredTool) -> PathBuf {
    tool.path
        .with_file_name(if cfg!(windows) { "psql.exe" } else { "psql" })
}

/// Run SQL via psql (test-only: password via PGPASSWORD is fine here).
fn psql(psql: &Path, ep: &ToolEndpoint, sql: &str) -> String {
    let out = Command::new(psql)
        .arg(format!("--dbname={}", ep.conninfo()))
        .args(["-v", "ON_ERROR_STOP=1", "-X", "-q", "-t", "-A", "-c", sql])
        .env("PGPASSWORD", PG_PASSWORD)
        .env("PGSSLMODE", "disable")
        .output()
        .expect("run psql");
    assert!(
        out.status.success(),
        "psql failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn endpoint(port: u16, db: &str) -> ToolEndpoint {
    ToolEndpoint {
        // A name that does not resolve: libpq dials hostaddr, while pgpass (and verify-full)
        // key on this — the shape a tunneled connection uses.
        host: "pgwhy-real-host.invalid".into(),
        hostaddr: Some("127.0.0.1".parse().unwrap()),
        port,
        user: PG_USER.into(),
        dbname: db.into(),
        sslmode: "prefer".into(),
        sslrootcert: None,
    }
}

/// Hostile database name: spaces, quotes, backslash, `=`, leading `--`.
const HOSTILE_DB: &str = r#"--x 'q\=d"b"#;

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[tokio::test]
async fn real_backup_toc_restore_roundtrip() {
    let Some((reg, pg_dump, pg_restore)) = real_tools(16) else {
        return;
    };
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let psql_bin = psql_next_to(&pg_dump);
    let admin = endpoint(pg.port, harness::PG_DB);

    // Gate picks a pg_dump for this server.
    let server_major = PgVersion::major_from_version_num(
        psql(&psql_bin, &admin, "SHOW server_version_num")
            .parse()
            .unwrap(),
    );
    assert_eq!(server_major, 16);
    assert!(require_pg_dump(&reg, server_major).unwrap().version.major >= 16);

    psql(
        &psql_bin,
        &admin,
        &format!("CREATE DATABASE {}", quote_ident(HOSTILE_DB)),
    );
    let src = endpoint(pg.port, HOSTILE_DB);
    psql(
        &psql_bin,
        &src,
        r#"CREATE TABLE public.t1(id int primary key, v text);
           INSERT INTO public.t1 SELECT g, g::text FROM generate_series(1,1000) g;
           CREATE INDEX t1_v_idx ON public.t1(v);
           CREATE TABLE public.audit(id int);
           INSERT INTO public.audit SELECT generate_series(1,5);
           CREATE SCHEMA "s p";
           CREATE TABLE "s p"."we""ird*"(a int);
           INSERT INTO "s p"."we""ird*" VALUES (1),(2),(3);
           CREATE TABLE "s p"."we""irdX"(decoy int);
           CREATE VIEW public.v1 AS SELECT * FROM public.t1;"#,
    );

    let tmp = tempfile::tempdir().unwrap();
    let ctx = RunContext::new(PgPassDir::new(tmp.path().join("pgpass")).unwrap());
    let password = ToolPassword::new(PG_PASSWORD);

    // Without a password the child cannot authenticate: the temp pgpass is the only path.
    let err = backup(
        &pg_dump,
        &src,
        None,
        &tmp.path().join("nopw.dump"),
        &BackupOptions::default(),
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth, "{err:?}");
    // A wrong password is classified as auth too, and never echoed.
    let err = backup(
        &pg_dump,
        &src,
        Some(&ToolPassword::new("wrong-pw-xyz")),
        &tmp.path().join("badpw.dump"),
        &BackupOptions::default(),
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(!serde_json::to_string(&err)
        .unwrap()
        .contains("wrong-pw-xyz"));

    // Real backup with progress.
    let out = tmp.path().join("backups").join("src.dump");
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev2 = events.clone();
    let opts = BackupOptions {
        exclude_table_data: vec![table_pattern("public", "audit")],
        estimated_objects: Some(3),
        log_path: Some(tmp.path().join("dump.log")),
        ..Default::default()
    };
    let res = backup(
        &pg_dump,
        &src,
        Some(&password),
        &out,
        &opts,
        &ctx,
        move |e| ev2.lock().unwrap().push(e),
    )
    .await
    .expect("backup");
    assert!(res.size_bytes > 0);
    assert_eq!(res.run.objects_done, 3, "{:?}", res.run.stderr_tail);
    assert!(!res.run.args.iter().any(|a| a.contains(PG_PASSWORD)));
    let last = events.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.objects_done, 3);
    assert_eq!(last.bytes_written, Some(res.size_bytes));
    assert!(std::fs::read_dir(tmp.path().join("pgpass"))
        .unwrap()
        .next()
        .is_none());
    let log = std::fs::read_to_string(tmp.path().join("dump.log")).unwrap();
    assert!(log.contains("dumping contents of table"), "{log}");

    // TOC
    let insp = inspect_archive(&pg_restore, &out, ArchiveCheck::Full, &ctx)
        .await
        .unwrap();
    assert!(insp.reusable, "{:?}", insp.reason);
    let toc = insp.toc.unwrap();
    assert_eq!(
        toc.header.dumped_by.as_ref().unwrap().major,
        pg_dump.version.major
    );
    assert_eq!(toc.header.dumped_from.as_ref().unwrap().major, 16);
    assert_eq!(toc.header.format.as_deref(), Some("CUSTOM"));
    assert_eq!(toc.count_of("TABLE DATA"), 3, "audit data excluded");
    assert!(require_pg_restore(&reg, toc.header.dumped_by.as_ref().unwrap()).is_ok());

    // Restore into a second database, parallel.
    psql(&psql_bin, &admin, "CREATE DATABASE restored");
    let dst = endpoint(pg.port, "restored");
    let r_events = Arc::new(Mutex::new(Vec::new()));
    let rev2 = r_events.clone();
    let total = toc.restorable_count(true);
    let restored = restore(
        &pg_restore,
        &dst,
        Some(&password),
        &out,
        &RestoreOptions {
            jobs: Some(2),
            total_items: Some(total),
            ..Default::default()
        },
        &ctx,
        move |e| rev2.lock().unwrap().push(e),
    )
    .await
    .expect("restore");
    assert!(restored.run.success);
    let last = r_events.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.objects_total, Some(total));
    assert!(
        last.objects_done > 0 && last.objects_done <= total,
        "{last:?}"
    );
    assert_eq!(
        psql(&psql_bin, &dst, "SELECT count(*) FROM public.t1"),
        "1000"
    );
    assert_eq!(
        psql(&psql_bin, &dst, r#"SELECT count(*) FROM "s p"."we""ird*""#),
        "3"
    );
    assert_eq!(
        psql(&psql_bin, &dst, "SELECT count(*) FROM public.audit"),
        "0"
    );

    // Restoring again: every CREATE collides. Exit-on-error stops; continue-on-error names
    // the failing objects.
    let err = restore(
        &pg_restore,
        &dst,
        Some(&password),
        &out,
        &RestoreOptions::default(),
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        FailureCause::from_error(&err),
        Some(FailureCause::ObjectErrors),
        "{err:?}"
    );
    let err = restore(
        &pg_restore,
        &dst,
        Some(&password),
        &out,
        &RestoreOptions {
            error_policy: ErrorPolicy::ContinueOnError,
            ..Default::default()
        },
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        FailureCause::from_error(&err),
        Some(FailureCause::ObjectErrors)
    );
    let data = err.data.unwrap();
    assert!(data["errors_ignored"].as_u64().unwrap() > 1, "{data}");
    let items = data["failed_items"].as_array().unwrap();
    assert!(items.len() > 1);
    assert!(items.iter().all(|i| i["object"].is_string()), "{items:?}");

    // Truncated archive → not reusable; restore classifies archive corruption.
    let bytes = std::fs::read(&out).unwrap();
    let trunc = tmp.path().join("trunc.dump");
    std::fs::write(&trunc, &bytes[..bytes.len() / 2]).unwrap();
    let v = inspect_archive(&pg_restore, &trunc, ArchiveCheck::Full, &ctx)
        .await
        .unwrap();
    assert!(!v.reusable);
    assert_eq!(
        v.cause,
        Some(FailureCause::ArchiveCorrupt),
        "{:?}",
        v.reason
    );
    psql(&psql_bin, &admin, "CREATE DATABASE restored2");
    let err = restore(
        &pg_restore,
        &endpoint(pg.port, "restored2"),
        Some(&password),
        &trunc,
        &RestoreOptions::default(),
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        FailureCause::from_error(&err),
        Some(FailureCause::ArchiveCorrupt),
        "{err:?}"
    );

    // DDL for exactly one table, with pattern metacharacters in both names.
    let ddl = dump_schema_ddl(
        &pg_dump,
        &src,
        Some(&password),
        "s p",
        r#"we"ird*"#,
        &DdlOptions::default(),
        &ctx,
    )
    .await
    .expect("ddl");
    assert!(ddl.contains(r#"CREATE TABLE "s p"."we""ird*""#), "{ddl}");
    assert!(!ddl.contains("decoy"), "wildcard leaked: {ddl}");
    assert!(
        !ddl.contains("\\restrict") && !ddl.contains("SET "),
        "{ddl}"
    );
    let err = dump_schema_ddl(
        &pg_dump,
        &src,
        Some(&password),
        "public",
        "nope",
        &DdlOptions::default(),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        FailureCause::from_error(&err),
        Some(FailureCause::MissingObject),
        "{err:?}"
    );

    // Retention over the real backup folder: protected + newest kept.
    std::fs::copy(&out, tmp.path().join("backups/older.dump")).unwrap();
    let plan = apply_retention(
        &tmp.path().join("backups"),
        &RetentionPolicy {
            keep_last: 1,
            ..Default::default()
        },
        &[out.clone()].into(),
    )
    .unwrap();
    assert!(plan.keep.contains(&out));
    assert!(std::fs::read_dir(tmp.path().join("pgpass"))
        .unwrap()
        .next()
        .is_none());
}

#[tokio::test]
async fn ae5_pg_dump_16_refuses_postgres_17() {
    let Some((_, pg_dump, _)) = real_tools(16) else {
        return;
    };
    if pg_dump.version.major >= 17 {
        eprintln!(
            "NOTICE: installed pg_dump is {} — AE5 real-tool test needs one older than 17",
            pg_dump.version.raw
        );
        return;
    }
    let Some(pg) = harness::pg(17).await else {
        return;
    };
    let psql_bin = psql_next_to(&pg_dump);
    let ep = endpoint(pg.port, harness::PG_DB);
    let major = PgVersion::major_from_version_num(
        psql(&psql_bin, &ep, "SHOW server_version_num")
            .parse()
            .unwrap(),
    );
    assert_eq!(major, 17, "{}", pg.image);

    // Registry restricted to the installed (older) tools' folder.
    let bindir = pg_dump.path.parent().unwrap().to_path_buf();
    let reg = discover(&DiscoveryOptions::isolated(vec![bindir.clone()], None));
    let err = require_pg_dump(&reg, major).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolVersion);
    assert_eq!(
        err.message,
        format!(
            "pg_dump {} cannot back up a PostgreSQL 17 server",
            pg_dump.version.major
        )
    );
    assert!(err
        .hint
        .as_deref()
        .unwrap()
        .contains("PostgreSQL 17 client tools"));
    assert!(err
        .detail
        .as_deref()
        .unwrap()
        .contains(&bindir.display().to_string()));

    // Bypassing the gate, the real pg_dump aborts — classified as a version problem and no
    // file is left behind.
    let tmp = tempfile::tempdir().unwrap();
    let ctx = RunContext::new(PgPassDir::new(tmp.path().join("pgpass")).unwrap());
    let out = tmp.path().join("x.dump");
    let err = backup(
        &pg_dump,
        &ep,
        Some(&ToolPassword::new(PG_PASSWORD)),
        &out,
        &BackupOptions::default(),
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolVersion, "{err:?}");
    assert!(!out.exists());
    assert!(std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .all(|e| e.file_name() == "pgpass"));
}
