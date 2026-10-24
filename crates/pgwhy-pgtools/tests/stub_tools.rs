//! Stub-binary tests: tiny shell scripts stand in for pg_dump/pg_restore, answering
//! `--version`, printing verbose lines, and recording their argv, environment and the temp
//! pgpass contents so the runner's contract can be asserted without a server.

#![cfg(unix)]

use pgwhy_pgtools::discover::DiscoverySource;
use pgwhy_pgtools::endpoint::{schema_pattern, table_pattern};
use pgwhy_pgtools::*;
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PASSWORD: &str = "s3cr:et*pw";

/// Write an executable stub. `body` runs after the recorder; `$REC` is this run's record file.
fn stub(dir: &Path, name: &str, version: &str, body: &str) -> PathBuf {
    let rec_dir = dir.join("records");
    std::fs::create_dir_all(&rec_dir).unwrap();
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "{name} (PostgreSQL) {version}"; exit 0; fi
REC="{rec}/{name}-$$"
OUT=""
for a in "$@"; do
  printf 'ARG:%s\n' "$a" >> "$REC"
  case "$a" in --file=*) OUT="${{a#--file=}}";; esac
done
env | grep -E '^(PG|LC_|LANGUAGE)' | sed 's/^/ENV:/' >> "$REC"
if [ -n "$PGPASSFILE" ]; then
  echo "PGPASS_PATH:$PGPASSFILE" >> "$REC"
  # GNU stat first: BSD stat rejects -c, while GNU `stat -f` means "filesystem" and prints a report
  echo "PGPASS_MODE:$(stat -c %a "$PGPASSFILE" 2>/dev/null || stat -f %Lp "$PGPASSFILE")" >> "$REC"
  printf 'PGPASS:%s\n' "$(cat "$PGPASSFILE")" >> "$REC"
fi
{body}
"#,
        rec = rec_dir.display()
    );
    let path = dir.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn records(dir: &Path, name: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir.join("records"))
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{name}-"))
        })
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    out.sort();
    out
}

fn args_of(rec: &str) -> Vec<&str> {
    rec.lines().filter_map(|l| l.strip_prefix("ARG:")).collect()
}

fn line_value<'a>(rec: &'a str, prefix: &str) -> Option<&'a str> {
    rec.lines().find_map(|l| l.strip_prefix(prefix))
}

fn tool(path: &Path, kind: ToolKind, major: u32) -> DiscoveredTool {
    DiscoveredTool {
        kind,
        path: path.to_path_buf(),
        version: PgVersion {
            major,
            minor: 4,
            raw: format!("{major}.4"),
        },
        source: DiscoverySource::Configured,
    }
}

fn endpoint() -> ToolEndpoint {
    ToolEndpoint {
        host: "db.internal.example".into(),
        hostaddr: Some("127.0.0.1".parse().unwrap()),
        port: 61234,
        user: "app".into(),
        dbname: "shop".into(),
        sslmode: "verify-full".into(),
        sslrootcert: Some("/etc/ssl/root ca.pem".into()),
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    bin: PathBuf,
    pgpass: PathBuf,
    out: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    let pgpass = tmp.path().join("pgpass");
    let out = tmp.path().join("backups");
    std::fs::create_dir_all(&bin).unwrap();
    Env {
        bin,
        pgpass,
        out,
        _tmp: tmp,
    }
}

fn ctx(e: &Env) -> RunContext {
    RunContext::new(PgPassDir::new(&e.pgpass).unwrap())
}

fn dir_entries(p: &Path) -> Vec<String> {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

const DUMP_OK: &str = r#"
echo 'pg_dump: last built-in OID is 16383' >&2
echo 'pg_dump: reading schemas' >&2
echo 'pg_dump: dumping contents of table "public.t1"' >&2
printf 'PGDMP-0123456789' > "$OUT"
sleep 0.6
echo 'pg_dump: dumping contents of table "s2.we ird"' >&2
printf 'more-bytes' >> "$OUT"
exit 0
"#;

#[test]
fn discovery_finds_stubs_and_selects_versions() {
    let e = env();
    let d15 = e.bin.join("pg15");
    let d16 = e.bin.join("pg16");
    let d18 = e.bin.join("pg18");
    for d in [&d15, &d16, &d18] {
        std::fs::create_dir_all(d).unwrap();
    }
    stub(&d15, "pg_dump", "15.4", "exit 0");
    stub(&d16, "pg_dump", "16.15 (Homebrew)", "exit 0");
    stub(&d16, "pg_restore", "16.15 (Homebrew)", "exit 0");
    stub(&d18, "pg_dump", "18.0", "exit 0");
    // A symlinked duplicate of the 16 binary must be probed once.
    let linkdir = e.bin.join("links");
    std::fs::create_dir_all(&linkdir).unwrap();
    std::os::unix::fs::symlink(d16.join("pg_dump"), linkdir.join("pg_dump")).unwrap();
    // A broken "pg_dump" that prints nothing parseable is skipped.
    let broken = e.bin.join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("pg_dump"), "#!/bin/sh\necho nope\n").unwrap();
    std::fs::set_permissions(
        broken.join("pg_dump"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let path_env = std::env::join_paths([&d15, &linkdir, &broken, &d18]).unwrap();
    let reg = discover(&DiscoveryOptions::isolated(
        vec![d16.clone()],
        Some(path_env),
    ));
    assert_eq!(reg.majors(ToolKind::PgDump), vec![15, 16, 18]);
    assert_eq!(reg.of_kind(ToolKind::PgDump).count(), 3, "{:?}", reg.tools);
    assert_eq!(reg.searched.len(), 5);
    let pick = reg.select(ToolKind::PgDump, 16).unwrap();
    assert_eq!(pick.path, d16.join("pg_dump"));
    assert_eq!(pick.source, DiscoverySource::Configured);
    assert_eq!(reg.select(ToolKind::PgDump, 17).unwrap().version.major, 18);
    assert_eq!(require_pg_dump(&reg, 15).unwrap().version.major, 15);
    assert_eq!(
        require_pg_restore(&reg, &PgVersion::parse("16.2").unwrap())
            .unwrap()
            .version
            .major,
        16
    );
}

#[test]
fn version_gate_refuses_with_actionable_message() {
    let e = env();
    stub(&e.bin, "pg_dump", "15.4", "exit 0");
    let reg = discover(&DiscoveryOptions::isolated(
        vec![],
        Some(e.bin.clone().into()),
    ));
    let err = require_pg_dump(&reg, 17).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolVersion);
    assert_eq!(
        err.message,
        "pg_dump 15 cannot back up a PostgreSQL 17 server"
    );
    let hint = err.hint.unwrap();
    assert!(hint.contains("PostgreSQL 17 client tools"), "{hint}");
    assert!(hint.contains(&e.bin.display().to_string()), "{hint}");

    // pg_restore missing entirely → ToolMissing naming where it looked
    let err = require_pg_restore(&reg, &PgVersion::parse("15.4").unwrap()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolMissing);
    assert!(err.hint.unwrap().contains(&e.bin.display().to_string()));
}

#[tokio::test]
async fn backup_happy_path_progress_pgpass_and_argv() {
    let e = env();
    let bin = stub(&e.bin, "pg_dump", "16.4", DUMP_OK);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    let ctx = ctx(&e);
    let out = e.out.join("shop_20261008.dump");
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev2 = events.clone();
    let opts = BackupOptions {
        exclude_table_data: vec![table_pattern("public", "audit log")],
        estimated_objects: Some(2),
        ..Default::default()
    };
    let res = backup(
        &pg_dump,
        &endpoint(),
        Some(&ToolPassword::new(PASSWORD)),
        &out,
        &opts,
        &ctx,
        move |ev| ev2.lock().unwrap().push(ev),
    )
    .await
    .unwrap();

    assert_eq!(res.path, out);
    assert_eq!(res.size_bytes, 26);
    assert!(out.is_file());
    assert!(!e.out.join("shop_20261008.dump.partial").exists());
    assert_eq!(res.dumped_by.major, 16);
    assert_eq!(res.run.objects_done, 2);

    let events = events.lock().unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.objects_done, 2);
    assert_eq!(last.objects_total, Some(2));
    assert!(last.total_is_estimate);
    assert_eq!(last.current_object.as_deref(), Some("s2.we ird"));
    assert_eq!(last.bytes_written, Some(26));
    // a mid-run event saw the first table with bytes on disk
    assert!(events
        .iter()
        .any(|ev| ev.objects_done == 1 && ev.bytes_written.unwrap_or(0) > 0));

    let rec = &records(&e.bin, "pg_dump")[0];
    let args = args_of(rec);
    assert_eq!(
        args,
        vec![
            "--verbose",
            "--format=custom",
            &format!("--file={}", e.out.join("shop_20261008.dump.partial").display()),
            r#"--exclude-table-data="public"."audit log""#,
            "--dbname=host='db.internal.example' hostaddr='127.0.0.1' port='61234' user='app' dbname='shop'",
        ]
    );
    // The password never reaches argv or env; only the pgpass file holds it.
    assert!(!rec
        .lines()
        .filter(|l| !l.starts_with("PGPASS:"))
        .any(|l| l.contains("s3cr")));
    assert!(!rec.contains("ENV:PGPASSWORD"));
    assert!(rec.contains("ENV:PGSSLMODE=verify-full"));
    assert!(rec.contains("ENV:PGSSLROOTCERT=/etc/ssl/root ca.pem"));
    assert!(rec.contains("ENV:LC_MESSAGES=C"));
    assert_eq!(line_value(rec, "PGPASS_MODE:"), Some("600"));
    assert_eq!(
        line_value(rec, "PGPASS:"),
        Some(r"db.internal.example:61234:shop:app:s3cr\:et\*pw")
    );
    let pgpass_path = line_value(rec, "PGPASS_PATH:").unwrap();
    assert!(pgpass_path.starts_with(&e.pgpass.display().to_string()));
    assert!(
        !Path::new(pgpass_path).exists(),
        "pgpass deleted after exit"
    );
    assert!(dir_entries(&e.pgpass).is_empty());
}

#[tokio::test]
async fn inherited_pg_env_is_scrubbed() {
    // The ChildSpec must not pass a shell's PGPASSWORD/PGHOST through.
    let e = env();
    let bin = stub(&e.bin, "pg_dump", "16.4", "exit 0");
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    let spec = {
        let mut s = runner::ChildSpec::new(&pg_dump, vec![runner::dbname_arg(&endpoint())]);
        s.endpoint = Some(endpoint());
        s
    };
    let cmd = spec.build(Some(Path::new("/tmp/x.pgpass")));
    let std_cmd = cmd.as_std();
    let args: Vec<String> = std_cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args.len(), 1);
    assert!(!args[0].contains("password"));
    let envs: Vec<(String, Option<String>)> = std_cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    assert!(envs.contains(&("PGPASSFILE".into(), Some("/tmp/x.pgpass".into()))));
    assert!(!envs.iter().any(|(k, v)| k == "PGPASSWORD" && v.is_some()));
    // Any PG* var present in this process is explicitly removed.
    for (k, _) in std::env::vars() {
        if k.starts_with("PG")
            && ![
                "PGPASSFILE",
                "PGSSLMODE",
                "PGSSLROOTCERT",
                "PGAPPNAME",
                "PGCONNECT_TIMEOUT",
            ]
            .contains(&k.as_str())
        {
            assert!(envs.contains(&(k.clone(), None)), "{k} not scrubbed");
        }
    }
}

#[tokio::test]
async fn hostile_connection_fields_stay_inert() {
    let e = env();
    let mut ep = endpoint();
    ep.host = "--host=evil example".into();
    ep.user = r#"o'brien "x" \y"#.into();
    ep.dbname = "--dbname=other a=b".into();
    let out = e.out.join("x.dump");
    // stub doesn't write OUT; create the partial so the rename succeeds
    let body = "printf x > \"$OUT\"; exit 0";
    let bin = stub(&e.bin, "pg_dump", "16.4", body);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    backup(
        &pg_dump,
        &ep,
        Some(&ToolPassword::new("pw")),
        &out,
        &BackupOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap();
    let rec = &records(&e.bin, "pg_dump")[0];
    let args = args_of(rec);
    assert_eq!(args.len(), 4, "{args:?}");
    assert_eq!(
        args[3],
        r#"--dbname=host='--host=evil example' hostaddr='127.0.0.1' port='61234' user='o\'brien "x" \\y' dbname='--dbname=other a=b'"#
    );
    assert_eq!(
        line_value(rec, "PGPASS:"),
        Some(r#"--host=evil example:61234:--dbname=other a=b:o'brien "x" \\y:pw"#)
    );
}

#[tokio::test]
async fn drifted_verbose_output_degrades_to_bytes() {
    let e = env();
    let body = r#"
echo 'pg_dump: dumpin contents of tabel "x"' >&2
echo 'some future pg_dump line with "quotes" and ü' >&2
printf '\377\376 not utf8\n' >&2
printf 'abcdef' > "$OUT"
exit 0
"#;
    let bin = stub(&e.bin, "pg_dump", "19.0", body);
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev2 = events.clone();
    let res = backup(
        &tool(&bin, ToolKind::PgDump, 19),
        &endpoint(),
        None,
        &e.out.join("d.dump"),
        &BackupOptions::default(),
        &ctx(&e),
        move |ev| ev2.lock().unwrap().push(ev),
    )
    .await
    .unwrap();
    assert_eq!(res.run.objects_done, 0);
    let last = events.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.bytes_written, Some(6));
    assert_eq!(last.objects_done, 0);
    assert!(last.last_line.is_some());
    // No password supplied → no pgpass file at all.
    let rec = &records(&e.bin, "pg_dump")[0];
    assert!(!rec.contains("PGPASS_PATH:"));
}

#[tokio::test]
async fn failed_dump_classifies_and_cleans_up() {
    let e = env();
    let body = r#"
printf 'partial' > "$OUT"
echo 'pg_dump: error: connection to server at "127.0.0.1", port 61234 failed: FATAL:  password authentication failed for user "app"' >&2
exit 1
"#;
    let bin = stub(&e.bin, "pg_dump", "16.4", body);
    let out = e.out.join("f.dump");
    let err = backup(
        &tool(&bin, ToolKind::PgDump, 16),
        &endpoint(),
        Some(&ToolPassword::new(PASSWORD)),
        &out,
        &BackupOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Auth);
    assert_eq!(
        runner::FailureCause::from_error(&err),
        Some(FailureCause::Auth)
    );
    assert!(err.message.contains("password authentication failed"));
    assert!(!format!("{err:?}").contains("s3cr"));
    assert!(!out.exists());
    assert!(
        dir_entries(&e.out).is_empty(),
        "partial removed: {:?}",
        dir_entries(&e.out)
    );
    assert!(dir_entries(&e.pgpass).is_empty(), "pgpass removed");
    assert_eq!(err.data.unwrap()["exit_code"], 1);
}

#[tokio::test]
async fn cancel_kills_process_group_and_cleans_up() {
    let e = env();
    let pidfile = e.bin.join("grandchild.pid");
    let body = format!(
        r#"
echo 'pg_dump: dumping contents of table "public.big"' >&2
printf 'x' > "$OUT"
sleep 30 &
echo $! > "{}"
wait
"#,
        pidfile.display()
    );
    let bin = stub(&e.bin, "pg_dump", "16.4", &body);
    let ctx = ctx(&e);
    let cancel = ctx.cancel.clone();
    let out = e.out.join("c.dump");
    let started = Instant::now();
    let task = {
        let pg_dump = tool(&bin, ToolKind::PgDump, 16);
        let out = out.clone();
        let ctx = ctx.clone();
        tokio::spawn(async move {
            backup(
                &pg_dump,
                &endpoint(),
                Some(&ToolPassword::new(PASSWORD)),
                &out,
                &BackupOptions::default(),
                &ctx,
                |_| {},
            )
            .await
        })
    };
    // wait until the grandchild exists
    while !pidfile.exists() && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err.kind, ErrorKind::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    let gpid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // give the kernel a moment to reap
    let mut alive = true;
    for _ in 0..50 {
        let st = std::process::Command::new("kill")
            .args(["-0", &gpid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        if !st.success() {
            alive = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive, "grandchild {gpid} survived cancel");
    assert!(!out.exists());
    assert!(dir_entries(&e.out).is_empty());
    assert!(dir_entries(&e.pgpass).is_empty());
}

#[tokio::test]
async fn concurrent_runs_use_independent_pgpass_files() {
    let e = env();
    let body = "sleep 0.3; printf x > \"$OUT\"; exit 0";
    let bin = stub(&e.bin, "pg_dump", "16.4", body);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    let ctx = ctx(&e);
    let ep_a = endpoint();
    let mut ep_b = endpoint();
    ep_b.dbname = "other".into();
    let (pw_a, pw_b) = (ToolPassword::new("pw-a"), ToolPassword::new("pw-b"));
    let (out_a, out_b) = (e.out.join("a.dump"), e.out.join("b.dump"));
    let opts = BackupOptions::default();
    let a = backup(&pg_dump, &ep_a, Some(&pw_a), &out_a, &opts, &ctx, |_| {});
    let b = backup(&pg_dump, &ep_b, Some(&pw_b), &out_b, &opts, &ctx, |_| {});
    let (ra, rb) = tokio::join!(a, b);
    ra.unwrap();
    rb.unwrap();
    let recs = records(&e.bin, "pg_dump");
    assert_eq!(recs.len(), 2);
    let paths: HashSet<&str> = recs
        .iter()
        .map(|r| line_value(r, "PGPASS_PATH:").unwrap())
        .collect();
    assert_eq!(paths.len(), 2, "distinct files");
    let pw: HashSet<&str> = recs
        .iter()
        .map(|r| line_value(r, "PGPASS:").unwrap())
        .collect();
    assert!(pw.contains("db.internal.example:61234:shop:app:pw-a"));
    assert!(pw.contains("db.internal.example:61234:other:app:pw-b"));
    assert!(dir_entries(&e.pgpass).is_empty());
}

/// A pid that is certainly not running: a child we already reaped.
fn dead_pid() -> u32 {
    let mut c = std::process::Command::new("true").spawn().unwrap();
    let pid = c.id();
    c.wait().unwrap();
    pid
}

#[test]
fn startup_sweep_removes_only_dead_owners_files() {
    let e = env();
    let dir = PgPassDir::new(&e.pgpass).unwrap();
    // Simulate crashes: the guards never run.
    let orphan = dir.create(&endpoint(), &ToolPassword::new("pw")).unwrap();
    let orphan_cert = dir.create_root_cert("PEM").unwrap();
    let live = dir.create(&endpoint(), &ToolPassword::new("pw")).unwrap();
    let (orphan_path, orphan_cert_path, live_path) = (
        orphan.path().to_path_buf(),
        orphan_cert.path().to_path_buf(),
        live.path().to_path_buf(),
    );
    std::mem::forget(orphan);
    std::mem::forget(orphan_cert);
    std::mem::forget(live);
    // Re-own the orphans to a dead pid; `live` keeps this (running) process's pid, like a
    // second pgWhy instance mid-backup.
    let dead = dead_pid();
    let reown = |p: &Path| {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let rest = name.splitn(4, '-').nth(3).unwrap().to_string();
        let new = p.with_file_name(format!("pgwhy-run-{dead}-{rest}"));
        std::fs::rename(p, &new).unwrap();
        new
    };
    let orphan_path = reown(&orphan_path);
    let orphan_cert_path = reown(&orphan_cert_path);
    std::fs::write(e.pgpass.join("unrelated.txt"), "keep").unwrap();
    let mode = std::fs::metadata(&orphan_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(sweep_stale_pgpass(&e.pgpass).unwrap(), 2);
    assert!(!orphan_path.exists());
    assert!(!orphan_cert_path.exists());
    assert!(live_path.exists(), "a live owner's file must survive");
    assert!(e.pgpass.join("unrelated.txt").exists());
    assert_eq!(sweep_stale_pgpass(&e.pgpass.join("missing")).unwrap(), 0);
    std::fs::remove_file(live_path).unwrap();
}

#[test]
fn pgpass_dir_is_private_and_loose_dirs_are_refused() {
    let e = env();
    PgPassDir::new(&e.pgpass).unwrap();
    let mode = std::fs::metadata(&e.pgpass).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    let loose = e.bin.join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(PgPassDir::new(&loose).unwrap_err().kind, ErrorKind::Config);
    let link = e.bin.join("link");
    std::os::unix::fs::symlink(&e.pgpass, &link).unwrap();
    assert!(PgPassDir::new(&link).is_err(), "symlinked dir refused");
}

#[tokio::test]
async fn restore_args_progress_and_object_failures() {
    let e = env();
    let archive = e.out.join("r.dump");
    std::fs::create_dir_all(&e.out).unwrap();
    std::fs::write(&archive, "PGDMP").unwrap();
    let ok_body = r#"
echo 'pg_restore: connecting to database for restore' >&2
echo 'pg_restore: creating TABLE "public.t1"' >&2
echo 'pg_restore: processing data for table "public.t1"' >&2
echo 'pg_restore: creating INDEX "public.t1_v_idx"' >&2
exit 0
"#;
    let bin = stub(&e.bin, "pg_restore", "16.4", ok_body);
    let pg_restore = tool(&bin, ToolKind::PgRestore, 16);
    let staging = endpoint().with_dbname("shop_staging_20261008-101500");
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev2 = events.clone();
    let opts = RestoreOptions {
        jobs: Some(4),
        total_items: Some(3),
        ..Default::default()
    };
    let out = restore(
        &pg_restore,
        &staging,
        Some(&ToolPassword::new(PASSWORD)),
        &archive,
        &opts,
        &ctx(&e),
        move |ev| ev2.lock().unwrap().push(ev),
    )
    .await
    .unwrap();
    assert!(out.run.success);
    let last = events.lock().unwrap().last().cloned().unwrap();
    assert_eq!((last.objects_done, last.objects_total), (3, Some(3)));
    assert!(!last.total_is_estimate);
    let rec = &records(&e.bin, "pg_restore")[0];
    let args = args_of(rec);
    assert_eq!(
        &args[..5],
        &[
            "--verbose",
            "--no-owner",
            "--no-privileges",
            "--exit-on-error",
            "--jobs=4"
        ]
    );
    assert!(
        args[5].starts_with("--dbname=")
            && args[5].contains("dbname='shop_staging_20261008-101500'")
    );
    assert_eq!(&args[6..], &["--", archive.to_str().unwrap()]);
    assert_eq!(
        line_value(rec, "PGPASS:").unwrap(),
        r"db.internal.example:61234:shop_staging_20261008-101500:app:s3cr\:et\*pw"
    );

    // continue-on-error with a failing object → failure naming it
    let bad_body = r#"
echo 'pg_restore: creating INDEX "public.t1_v_idx"' >&2
echo 'pg_restore: error: could not execute query: ERROR:  relation "t1_v_idx" already exists' >&2
echo 'Command was: CREATE INDEX t1_v_idx ON public.t1 USING btree (v);' >&2
echo 'pg_restore: warning: errors ignored on restore: 1' >&2
exit 1
"#;
    let bin = stub(&e.bin, "pg_restore", "16.4", bad_body);
    let pg_restore = tool(&bin, ToolKind::PgRestore, 16);
    let opts = RestoreOptions {
        error_policy: ErrorPolicy::ContinueOnError,
        ..Default::default()
    };
    let err = restore(
        &pg_restore,
        &staging,
        None,
        &archive,
        &opts,
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ToolFailed);
    assert_eq!(
        FailureCause::from_error(&err),
        Some(FailureCause::ObjectErrors)
    );
    let data = err.data.unwrap();
    assert_eq!(data["failed_items"][0]["object"], "INDEX public.t1_v_idx");
    assert_eq!(data["errors_ignored"], 1);

    // missing archive → archive_corrupt cause, nothing spawned
    let err = restore(
        &pg_restore,
        &staging,
        None,
        &e.out.join("gone.dump"),
        &RestoreOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        FailureCause::from_error(&err),
        Some(FailureCause::ArchiveCorrupt)
    );
}

#[tokio::test]
async fn inspect_archive_verdicts() {
    let e = env();
    std::fs::create_dir_all(&e.out).unwrap();
    let good = e.out.join("good.dump");
    let bad = e.out.join("bad.dump");
    std::fs::write(&good, "PGDMP").unwrap();
    std::fs::write(&bad, "junk").unwrap();
    let body = r#"
case "$*" in
  *bad.dump*) echo 'pg_restore: error: input file does not appear to be a valid archive (too short?)' >&2; exit 1;;
esac
cat <<'EOF'
;
;     Dumped from database version: 16.11
;     Dumped by pg_dump version: 16.15 (Homebrew)
;
216; 1259 16384 TABLE public t1 postgres
3420; 0 16384 TABLE DATA public t1 postgres
EOF
exit 0
"#;
    let bin = stub(&e.bin, "pg_restore", "17.1", body);
    let pg_restore = tool(&bin, ToolKind::PgRestore, 17);
    let ctx = ctx(&e);
    let ok = inspect_archive(&pg_restore, &good, ArchiveCheck::Full, &ctx)
        .await
        .unwrap();
    assert!(ok.reusable);
    let toc = ok.toc.unwrap();
    assert_eq!(toc.header.dumped_by.as_ref().unwrap().major, 16);
    assert_eq!(toc.restorable_count(true), 2);
    let rec = records(&e.bin, "pg_restore");
    assert!(rec
        .iter()
        .any(|r| args_of(r) == vec!["--list", "--", good.to_str().unwrap()]));
    assert!(rec
        .iter()
        .any(|r| args_of(r) == vec!["--file=/dev/null", "--", good.to_str().unwrap()]));

    let v = inspect_archive(&pg_restore, &bad, ArchiveCheck::TocOnly, &ctx)
        .await
        .unwrap();
    assert!(!v.reusable);
    assert_eq!(v.cause, Some(FailureCause::ArchiveCorrupt));
    let v = inspect_archive(
        &pg_restore,
        &e.out.join("nope.dump"),
        ArchiveCheck::TocOnly,
        &ctx,
    )
    .await
    .unwrap();
    assert!(!v.reusable);
}

#[tokio::test]
async fn ddl_uses_pattern_safe_table_argument() {
    let e = env();
    let body = r#"
cat <<'EOF'
SET statement_timeout = 0;
\restrict k

CREATE TABLE "we.ird"."t*""x" (
    a integer
);
EOF
exit 0
"#;
    let bin = stub(&e.bin, "pg_dump", "16.4", body);
    let ddl = dump_schema_ddl(
        &tool(&bin, ToolKind::PgDump, 16),
        &endpoint(),
        Some(&ToolPassword::new(PASSWORD)),
        "we.ird",
        r#"t*"x"#,
        &DdlOptions::default(),
        &ctx(&e),
    )
    .await
    .unwrap();
    assert_eq!(
        ddl,
        "CREATE TABLE \"we.ird\".\"t*\"\"x\" (\n    a integer\n);\n"
    );
    let rec = &records(&e.bin, "pg_dump")[0];
    let args = args_of(rec);
    assert_eq!(args[2], r#"--table="we.ird"."t*""x""#);
    assert_eq!(&args[..2], &["--schema-only", "--format=plain"]);
}

#[test]
fn retention_on_real_files() {
    let e = env();
    std::fs::create_dir_all(&e.out).unwrap();
    let now = std::time::SystemTime::now();
    let mut paths = Vec::new();
    for (i, name) in ["a.dump", "b.dump", "c.dump", "d.dump", "e.dump"]
        .iter()
        .enumerate()
    {
        let p = e.out.join(name);
        let f = std::fs::File::create(&p).unwrap();
        f.set_modified(now - Duration::from_secs(100 - i as u64 * 10))
            .unwrap();
        paths.push(p);
    }
    std::fs::write(e.out.join("notes.txt"), "x").unwrap();
    std::fs::write(e.out.join("x.dump.partial"), "x").unwrap();
    let protected: HashSet<PathBuf> = [paths[0].clone()].into();
    let plan = apply_retention(&e.out, &RetentionPolicy::default(), &protected).unwrap();
    assert_eq!(plan.delete, vec![paths[1].clone()]);
    let mut left = dir_entries(&e.out);
    left.sort();
    assert_eq!(
        left,
        vec![
            "a.dump",
            "c.dump",
            "d.dump",
            "e.dump",
            "notes.txt",
            "x.dump.partial"
        ]
    );
}

/// A scoped, structure-only backup: the scope flags reach pg_dump, and the combinations that
/// pg_dump cannot honour are refused before anything runs.
#[tokio::test]
async fn scoped_and_structure_only_backups_map_to_pg_dump_flags() {
    let e = env();
    let bin = stub(&e.bin, "pg_dump", "16.4", DUMP_OK);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    let ctx = ctx(&e);
    let out = e.out.join("shop_scoped.dump");
    let opts = BackupOptions {
        format: DumpFormat::Custom,
        schemas: vec![schema_pattern("audit")],
        schema_only: true,
        ..Default::default()
    };
    let res = backup(
        &pg_dump,
        &endpoint(),
        Some(&ToolPassword::new("s3cr:et*pw")),
        &out,
        &opts,
        &ctx,
        |_| {},
    )
    .await
    .unwrap();
    assert!(res.path.exists());
    let recs = records(&e.bin, "pg_dump");
    let args = args_of(&recs[0]);
    assert!(args.contains(&"--schema-only"), "{args:?}");
    assert!(args.contains(&r#"--schema="audit""#), "{args:?}");
    assert!(!args.iter().any(|a| a.starts_with("--table=")), "{args:?}");

    // tables scope uses -t, never mixed with -n
    let out = e.out.join("shop_tables.dump");
    let opts = BackupOptions {
        tables: vec![table_pattern("public", "orders")],
        ..Default::default()
    };
    backup(
        &pg_dump,
        &endpoint(),
        Some(&ToolPassword::new("s3cr:et*pw")),
        &out,
        &opts,
        &ctx,
        |_| {},
    )
    .await
    .unwrap();
    let recs = records(&e.bin, "pg_dump");
    let args = args_of(&recs[1]);
    assert!(args.contains(&r#"--table="public"."orders""#), "{args:?}");

    // refusals happen before the tool runs
    let both = BackupOptions {
        schemas: vec![schema_pattern("audit")],
        tables: vec![table_pattern("public", "orders")],
        ..Default::default()
    };
    let err = backup(
        &pg_dump,
        &endpoint(),
        Some(&ToolPassword::new("s3cr:et*pw")),
        &e.out.join("nope.dump"),
        &both,
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);
    assert!(err.message.contains("not both"), "{}", err.message);

    let contradictory = BackupOptions {
        schema_only: true,
        data_only: true,
        ..Default::default()
    };
    let err = backup(
        &pg_dump,
        &endpoint(),
        Some(&ToolPassword::new("s3cr:et*pw")),
        &e.out.join("nope2.dump"),
        &contradictory,
        &ctx,
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);
}

fn pid_alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn wait_dead(pid: i32) -> bool {
    for _ in 0..80 {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn verify_mode_without_root_cert_gets_system_roots_file() {
    let e = env();
    let body = r#"
echo "ROOTCERT_PATH:$PGSSLROOTCERT" >> "$REC"
echo "ROOTCERT_MODE:$(stat -c %a "$PGSSLROOTCERT" 2>/dev/null || stat -f %Lp "$PGSSLROOTCERT")" >> "$REC"
echo "ROOTCERT_PEM:$(grep -c 'BEGIN CERTIFICATE' "$PGSSLROOTCERT")" >> "$REC"
printf x > "$OUT"
"#;
    let bin = stub(&e.bin, "pg_dump", "14.4", body);
    let pg_dump = tool(&bin, ToolKind::PgDump, 14);
    let mut ep = endpoint();
    ep.sslrootcert = None;
    backup(
        &pg_dump,
        &ep,
        None,
        &e.out.join("r.dump"),
        &BackupOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap();
    let rec = &records(&e.bin, "pg_dump")[0];
    let path = line_value(rec, "ROOTCERT_PATH:").unwrap();
    assert!(path.starts_with(&e.pgpass.display().to_string()));
    assert_eq!(line_value(rec, "ROOTCERT_MODE:"), Some("600"));
    let certs: usize = line_value(rec, "ROOTCERT_PEM:").unwrap().parse().unwrap();
    assert!(certs > 0, "system roots written as PEM");
    assert!(
        !Path::new(path).exists(),
        "root cert file deleted after exit"
    );
    assert!(dir_entries(&e.pgpass).is_empty());

    // Not verifying, or a configured cert: no generated file.
    for (mode, cert) in [("require", None), ("verify-full", Some("/etc/ssl/ca.pem"))] {
        let mut ep = endpoint();
        ep.sslmode = mode.into();
        ep.sslrootcert = cert.map(Into::into);
        backup(
            &pg_dump,
            &ep,
            None,
            &e.out.join(format!("{mode}.dump")),
            &BackupOptions::default(),
            &ctx(&e),
            |_| {},
        )
        .await
        .unwrap();
    }
    let recs = records(&e.bin, "pg_dump");
    assert!(recs
        .iter()
        .any(|r| r.contains("ENV:PGSSLROOTCERT=/etc/ssl/ca.pem")));
    assert!(recs.iter().any(|r| !r.contains("ENV:PGSSLROOTCERT")));
}

#[tokio::test]
async fn backup_output_is_owner_only_and_existing_dirs_untouched() {
    let e = env();
    let body = "echo secret >&2; printf x > \"$OUT\"";
    let bin = stub(&e.bin, "pg_dump", "16.4", body);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let out = e.out.join("nested").join("o.dump");
    let log = e.out.join("log.txt");
    // A pre-existing, deliberately loose folder keeps its mode.
    std::fs::create_dir_all(&e.out).unwrap();
    std::fs::set_permissions(&e.out, std::fs::Permissions::from_mode(0o755)).unwrap();
    let opts = BackupOptions {
        log_path: Some(log.clone()),
        ..Default::default()
    };
    backup(&pg_dump, &endpoint(), None, &out, &opts, &ctx(&e), |_| {})
        .await
        .unwrap();
    assert_eq!(mode(&e.out), 0o755);
    assert_eq!(mode(out.parent().unwrap()), 0o700);
    assert_eq!(mode(&out), 0o600);
    assert_eq!(mode(&log), 0o600);
}

#[tokio::test]
async fn child_umask_is_owner_only() {
    let e = env();
    let marker = e.bin.join("umask.txt");
    let body = format!("umask > \"{}\"; printf x > \"$OUT\"", marker.display());
    let bin = stub(&e.bin, "pg_dump", "16.4", &body);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    backup(
        &pg_dump,
        &endpoint(),
        None,
        &e.out.join("u.dump"),
        &BackupOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "0077");
}

#[tokio::test]
async fn aborted_task_kills_the_whole_process_group() {
    let e = env();
    let pidfile = e.bin.join("worker.pid");
    let body = format!("sleep 30 &\necho $! > \"{}\"\nwait", pidfile.display());
    let bin = stub(&e.bin, "pg_dump", "16.4", &body);
    let ctx = ctx(&e);
    let out = e.out.join("a.dump");
    let task = {
        let pg_dump = tool(&bin, ToolKind::PgDump, 16);
        let (out, ctx) = (out.clone(), ctx.clone());
        tokio::spawn(async move {
            backup(
                &pg_dump,
                &endpoint(),
                None,
                &out,
                &BackupOptions::default(),
                &ctx,
                |_| {},
            )
            .await
        })
    };
    let started = Instant::now();
    while !pidfile.exists() && started.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let worker: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(worker));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        wait_dead(worker).await,
        "worker {worker} survived the abort"
    );
    assert!(dir_entries(&e.pgpass).is_empty());
}

#[tokio::test]
async fn survivors_of_an_exited_leader_are_signalled() {
    let e = env();
    let pidfile = e.bin.join("straggler.pid");
    let body = format!(
        "sleep 30 >/dev/null 2>&1 &\necho $! > \"{}\"\nprintf x > \"$OUT\"",
        pidfile.display()
    );
    let bin = stub(&e.bin, "pg_dump", "16.4", &body);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    backup(
        &pg_dump,
        &endpoint(),
        None,
        &e.out.join("s.dump"),
        &BackupOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap();
    let straggler: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(wait_dead(straggler).await, "straggler {straggler} survived");
}

#[tokio::test]
async fn backup_never_clobbers_a_file_created_mid_run_or_a_preexisting_partial() {
    let e = env();
    let out = e.out.join("c.dump");
    // A file appears at the destination while pg_dump runs.
    let body = "printf x > \"$OUT\"; printf theirs > \"${OUT%.partial}\"";
    let bin = stub(&e.bin, "pg_dump", "16.4", body);
    let pg_dump = tool(&bin, ToolKind::PgDump, 16);
    let err = backup(
        &pg_dump,
        &endpoint(),
        None,
        &out,
        &BackupOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Config);
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "theirs");
    assert_eq!(
        dir_entries(&e.out),
        vec!["c.dump".to_string()],
        "partial removed"
    );

    // A pre-existing partial (not ours, e.g. left by an interrupted backup) neither blocks the
    // backup nor is touched: this run claims a fresh name beside it.
    let partial = e.out.join("d.dump.partial");
    std::fs::write(&partial, "mine").unwrap();
    let ok = stub(&e.bin, "pg_dump", "16.4", "printf ours > \"$OUT\"");
    let pg_dump = tool(&ok, ToolKind::PgDump, 16);
    backup(
        &pg_dump,
        &endpoint(),
        None,
        &e.out.join("d.dump"),
        &BackupOptions::default(),
        &ctx(&e),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&partial).unwrap(), "mine");
    assert_eq!(
        std::fs::read_to_string(e.out.join("d.dump")).unwrap(),
        "ours"
    );
    let mut left = dir_entries(&e.out);
    left.sort();
    assert_eq!(left, vec!["c.dump", "d.dump", "d.dump.partial"]);
}
