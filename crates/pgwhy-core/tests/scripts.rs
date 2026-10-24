//! Script workbench core: library index, recents, file states, meta-command scan.
//! Filesystem only (temp dirs); no database.

use pgwhy_core::config::{ConfigStore, ConnectionId};
use pgwhy_core::scripts::*;
use std::path::{Path, PathBuf};

fn lib_dir() -> (tempfile::TempDir, Library) {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(dir.path()).unwrap();
    (dir, lib)
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn index_text(lib: &Library) -> String {
    std::fs::read_to_string(lib.index_path()).unwrap()
}

#[test]
fn save_then_search_by_name_path_and_tag() {
    let (_dir, mut lib) = lib_dir();
    let daily = lib.save("reports/Daily Revenue", "select 1;").unwrap();
    assert_eq!(daily.path, "reports/Daily Revenue.sql");
    assert_eq!(daily.name, "Daily Revenue");
    assert_eq!(daily.folder, "reports");
    let cleanup = lib.save("ops/cleanup.sql", "delete from t;").unwrap();
    lib.set_tags(&cleanup.id, &["Maintenance".into(), "post-restore".into()])
        .unwrap();

    let ids = |q: &str| -> Vec<ScriptId> { lib.search(q).into_iter().map(|s| s.id).collect() };
    assert_eq!(ids("daily"), vec![daily.id.clone()]);
    assert_eq!(ids("REVENUE"), vec![daily.id.clone()]);
    assert_eq!(ids("reports/"), vec![daily.id.clone()]);
    assert_eq!(ids("maint"), vec![cleanup.id.clone()]);
    assert_eq!(ids("ops restore"), vec![cleanup.id.clone()]); // every term must match
    assert!(ids("daily maint").is_empty());
    assert_eq!(ids("").len(), 2);
    assert_eq!(lib.all_tags(), ["Maintenance", "post-restore"]);

    assert_eq!(lib.read(&daily.id).unwrap().content, "select 1;");
    assert_eq!(
        lib.get(&cleanup.id).unwrap().tags,
        ["Maintenance", "post-restore"]
    );

    // Saving to the same path again keeps the id; reopening restores ids and tags.
    let again = lib.save("reports/Daily Revenue.sql", "select 2;").unwrap();
    assert_eq!(again.id, daily.id);
    let reopened = Library::open(lib.root()).unwrap();
    assert_eq!(reopened.list(), lib.list());
}

#[test]
fn external_file_appears_untagged_on_rescan() {
    let (dir, mut lib) = lib_dir();
    write(dir.path(), "adhoc/new one.sql", "select now();");
    write(dir.path(), "notes.txt", "kept alongside the scripts");
    write(dir.path(), "data.csv", "not a library file");
    write(dir.path(), ".git/HEAD.sql", "hidden");
    let report = lib.rescan().unwrap();
    assert_eq!(report.added.len(), 2);
    let mut added: Vec<_> = report.added.iter().map(|id| lib.get(id).unwrap()).collect();
    added.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(added[0].path, "adhoc/new one.sql");
    assert_eq!(added[1].path, "notes.txt");
    assert_eq!(added[1].name, "notes.txt");
    let s = &added[0];
    assert!(s.tags.is_empty());
    assert!(!s.missing);
    // Nothing changed since: rescan is a no-op and does not rewrite the index.
    let before = index_text(&lib);
    assert!(lib.rescan().unwrap().is_empty());
    assert_eq!(index_text(&lib), before);
}

#[test]
fn external_rename_keeps_stable_id() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("a/old.sql", "select 'renamed';").unwrap();
    lib.set_tags(&s.id, &["keep".into()]).unwrap();
    // `git mv a/old.sql b/new.sql`, done outside the app.
    std::fs::create_dir_all(dir.path().join("b")).unwrap();
    std::fs::rename(dir.path().join("a/old.sql"), dir.path().join("b/new.sql")).unwrap();

    let report = lib.rescan().unwrap();
    assert_eq!(
        report.moved,
        [ScriptMove {
            id: s.id.clone(),
            from: "a/old.sql".into(),
            to: "b/new.sql".into()
        }]
    );
    assert!(report.added.is_empty() && report.missing.is_empty());
    let moved = lib.get(&s.id).unwrap();
    assert_eq!(moved.path, "b/new.sql");
    assert_eq!(moved.tags, ["keep"]);
    assert_eq!(lib.list().len(), 1);
}

#[test]
fn rename_with_concurrent_copy_does_not_steal_present_id() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("x.sql", "select 42;").unwrap();
    write(dir.path(), "copy.sql", "select 42;");
    let report = lib.rescan().unwrap();
    assert!(report.moved.is_empty());
    assert_eq!(report.added.len(), 1);
    assert_eq!(lib.get(&s.id).unwrap().path, "x.sql");
}

#[test]
fn external_delete_marks_missing_then_restores() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("gone.sql", "select 'bye';").unwrap();
    std::fs::remove_file(dir.path().join("gone.sql")).unwrap();
    let report = lib.rescan().unwrap();
    assert_eq!(report.missing, vec![s.id.clone()]);
    let info = lib.get(&s.id).unwrap();
    assert!(info.missing, "entry kept, marked missing");
    assert!(lib.read(&s.id).is_err());
    assert!(lib.rename(&s.id, "elsewhere.sql").is_err());
    // Persisted: reopening still knows the id.
    assert!(
        Library::open(lib.root())
            .unwrap()
            .get(&s.id)
            .unwrap()
            .missing
    );

    // Restored later at a new path with the same content (e.g. a checkout) → same id.
    write(dir.path(), "back/again.sql", "select 'bye';");
    let report = lib.rescan().unwrap();
    assert_eq!(report.restored, vec![s.id.clone()]);
    let info = lib.get(&s.id).unwrap();
    assert!(!info.missing);
    assert_eq!(info.path, "back/again.sql");

    // Deleting through the app removes file and entry.
    lib.delete(&s.id).unwrap();
    assert!(lib.get(&s.id).is_none());
    assert!(!dir.path().join("back/again.sql").exists());
}

#[test]
fn external_edit_updates_hash() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("e.sql", "select 1;").unwrap();
    std::fs::write(dir.path().join("e.sql"), "select 1, 2, 3;").unwrap();
    let report = lib.rescan().unwrap();
    assert_eq!(report.changed, vec![s.id.clone()]);
    let info = lib.get(&s.id).unwrap();
    assert_ne!(info.fingerprint.sha256, s.fingerprint.sha256);
    assert_eq!(info.fingerprint.size, 15);
}

#[test]
fn in_app_rename_and_move_keep_id() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("one.sql", "select 1;").unwrap();
    let other = lib.save("two.sql", "select 2;").unwrap();
    let moved = lib.rename(&s.id, "folder/uno").unwrap();
    assert_eq!(moved.id, s.id);
    assert_eq!(moved.path, "folder/uno.sql");
    assert!(dir.path().join("folder/uno.sql").is_file());
    assert!(!dir.path().join("one.sql").exists());
    assert!(lib.rename(&s.id, "two.sql").is_err(), "refuses to clobber");
    assert!(lib.rename(&other.id, "../escape.sql").is_err());
    assert_eq!(lib.write(&s.id, "select 'one';").unwrap().id, s.id);
    assert_eq!(lib.read(&s.id).unwrap().content, "select 'one';");
}

#[test]
fn index_is_deterministic_relative_and_sorted() {
    let (dir, mut lib) = lib_dir();
    let b = lib.save("b.sql", "select 'b';").unwrap();
    let a = lib.save("z/a.sql", "select 'a';").unwrap();
    lib.set_tags(&a.id, &["zeta".into(), "Alpha".into()])
        .unwrap();
    let first = index_text(&lib);
    // Re-saving the same content and tags yields byte-identical output except mtimes, and a
    // fresh open + rescan without changes never rewrites the file.
    lib.set_tags(&a.id, &["Alpha".into(), "zeta".into()])
        .unwrap();
    let mut reopened = Library::open(dir.path()).unwrap();
    assert!(reopened.rescan().unwrap().is_empty());
    assert_eq!(index_text(&reopened), index_text(&lib));

    let json: serde_json::Value = serde_json::from_str(&first).unwrap();
    let scripts = json["scripts"].as_array().unwrap();
    let paths: Vec<&str> = scripts
        .iter()
        .map(|s| s["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["b.sql", "z/a.sql"]);
    assert_eq!(scripts[1]["tags"], serde_json::json!(["Alpha", "zeta"]));
    assert_eq!(scripts[0]["id"], b.id.0.as_str());
    assert!(first.ends_with("}\n"), "pretty JSON with trailing newline");
    let root = dir.path().canonicalize().unwrap();
    assert!(!first.contains(root.to_str().unwrap()), "no absolute paths");

    // A checkout touches mtimes without changing content: no rewrite, ids intact.
    for rel in ["b.sql", "z/a.sql"] {
        let p = dir.path().join(rel);
        let content = std::fs::read(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        std::fs::write(&p, content).unwrap();
    }
    let before = index_text(&reopened);
    assert!(reopened.rescan().unwrap().is_empty());
    assert_eq!(index_text(&reopened), before);
    assert_eq!(reopened.get(&a.id).unwrap().path, "z/a.sql");
}

#[test]
fn library_survives_clone_to_another_location() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("deep/x.sql", "select 'x';").unwrap();
    lib.set_tags(&s.id, &["t".into()]).unwrap();
    // "git clone" = copy the tree (scripts + index) somewhere else.
    let clone = tempfile::tempdir().unwrap();
    write(clone.path(), "deep/x.sql", "select 'x';");
    std::fs::copy(lib.index_path(), clone.path().join(INDEX_FILE_NAME)).unwrap();
    drop(dir);
    let cloned = Library::open(clone.path()).unwrap();
    let info = cloned.get(&s.id).unwrap();
    assert_eq!(
        (info.path.as_str(), info.tags.as_slice()),
        ("deep/x.sql", ["t".to_string()].as_slice())
    );
    assert!(!info.missing);
}

#[test]
fn corrupt_index_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(INDEX_FILE_NAME), "<<<<<<< HEAD\n").unwrap();
    let err = Library::open(dir.path()).unwrap_err();
    assert_eq!(err.kind, pgwhy_core::ErrorKind::Config);
    assert!(Library::open(dir.path().join("nope")).is_err());
    assert!(Library::create(dir.path().join("made/here")).is_ok());
}

#[test]
fn id_for_path_links_disk_paths() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("q.sql", "select 1;").unwrap();
    assert_eq!(
        lib.id_for_path(&dir.path().join("q.sql")),
        Some(s.id.clone())
    );
    assert_eq!(lib.id_for_path(Path::new("q.sql")), Some(s.id));
    assert_eq!(lib.id_for_path(Path::new("/somewhere/else.sql")), None);
}

// ---- recents --------------------------------------------------------------------------

fn run_use(path: &Path, conn: &str, db: &str) -> RecentUse {
    RecentUse {
        path: path.to_path_buf(),
        library_id: None,
        action: RecentAction::Run,
        connection_id: Some(ConnectionId(conn.into())),
        database: Some(db.into()),
    }
}

#[test]
fn recents_restore_last_connection_across_restart() {
    // yesterday's run shows up today and reopens with its last connection.
    let cfg = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(cfg.path());
    let files = tempfile::tempdir().unwrap();
    let script = files.path().join("yesterday.sql");
    std::fs::write(&script, "select 1;").unwrap();

    let mut recents = Recents::load(&store).unwrap();
    let day_ms = 24 * 60 * 60 * 1000;
    let now = 1_800_000_000_000u64;
    recents
        .record_at(run_use(&script, "local-id", "app_dev"), now - day_ms)
        .unwrap();
    recents.save(&store).unwrap();

    let today = Recents::load(&store).unwrap();
    let status = today.status(None);
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].state, RecentState::Available);
    assert_eq!(status[0].resolved_path, script);
    assert_eq!(
        status[0].entry.connection_id,
        Some(ConnectionId("local-id".into()))
    );
    assert_eq!(status[0].entry.database.as_deref(), Some("app_dev"));
    assert_eq!(status[0].entry.last_used_ms, now - day_ms);
}

#[test]
fn recents_order_by_last_use_and_cap() {
    // absolute on every platform: `/scripts` has no drive letter on Windows
    let dir = PathBuf::from(if cfg!(windows) {
        r"C:\scripts"
    } else {
        "/scripts"
    });
    let mut r = Recents::default();
    for i in 0..(MAX_RECENT_SCRIPTS + 5) {
        let used = RecentUse {
            path: dir.join(format!("{i}.sql")),
            library_id: None,
            action: RecentAction::Open,
            connection_id: None,
            database: None,
        };
        r.record_at(used, 1000 + i as u64).unwrap();
    }
    assert_eq!(r.entries().len(), MAX_RECENT_SCRIPTS);
    assert_eq!(r.entries()[0].path, dir.join("34.sql"));
    // Re-using an old one moves it to the front without duplicating it.
    r.record_at(run_use(&dir.join("10.sql"), "c", "d"), 5000)
        .unwrap();
    assert_eq!(r.entries()[0].path, dir.join("10.sql"));
    assert_eq!(r.entries().len(), MAX_RECENT_SCRIPTS);
    assert_eq!(
        r.entries()
            .iter()
            .filter(|e| e.path == dir.join("10.sql"))
            .count(),
        1
    );
    let times: Vec<u64> = r.entries().iter().map(|e| e.last_used_ms).collect();
    assert!(times.windows(2).all(|w| w[0] >= w[1]));
}

#[test]
fn recents_missing_file_is_not_found_and_removable() {
    let files = tempfile::tempdir().unwrap();
    let a = files.path().join("a.sql");
    let b = files.path().join("b.sql");
    std::fs::write(&a, "select 1;").unwrap();
    std::fs::write(&b, "select 2;").unwrap();
    let mut r = Recents::default();
    r.record(run_use(&a, "c", "d")).unwrap();
    r.record(run_use(&b, "c", "d")).unwrap();
    std::fs::remove_file(&a).unwrap();
    let states: Vec<_> = r
        .status(None)
        .into_iter()
        .map(|s| (s.entry.path, s.state))
        .collect();
    assert_eq!(
        states,
        [
            (b.clone(), RecentState::Available),
            (a.clone(), RecentState::NotFound)
        ]
    );
    assert!(r.remove(&a));
    assert!(!r.remove(&a));
    assert_eq!(r.entries().len(), 1);
}

#[test]
fn recents_follow_library_moves() {
    let (dir, mut lib) = lib_dir();
    let s = lib.save("x.sql", "select 'follow me';").unwrap();
    let mut r = Recents::default();
    r.record(RecentUse {
        path: lib.absolute_path(&s.id).unwrap(),
        library_id: Some(s.id.clone()),
        action: RecentAction::Save,
        connection_id: None,
        database: None,
    })
    .unwrap();
    std::fs::create_dir_all(dir.path().join("moved")).unwrap();
    std::fs::rename(dir.path().join("x.sql"), dir.path().join("moved/x.sql")).unwrap();

    // Without the library the stale path is not found; with it, the id resolves.
    assert_eq!(r.status(None)[0].state, RecentState::NotFound);
    lib.rescan().unwrap();
    let st = &r.status(Some(&lib))[0];
    assert_eq!(st.state, RecentState::Available);
    assert_eq!(st.resolved_path, lib.root().join("moved/x.sql"));
    assert!(r.sync_with_library(&lib));
    assert_eq!(r.entries()[0].path, lib.root().join("moved/x.sql"));
}

// ---- open-buffer file states ----------------------------------------------------------

#[test]
fn buffer_file_states_outside_library() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loose.sql");
    let loaded = write_script_file(&path, "select 1;").unwrap();
    assert_eq!(
        check_file(&path, &loaded).unwrap().state,
        FileState::Unchanged
    );
    std::fs::write(&path, "select 10;").unwrap();
    let check = check_file(&path, &loaded).unwrap();
    assert_eq!(check.state, FileState::ChangedOnDisk);
    let reloaded = read_script_file(&path).unwrap();
    assert_eq!(
        Some(reloaded.fingerprint.clone()).map(|f| f.sha256),
        check.current.map(|f| f.sha256)
    );
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        check_file(&path, &reloaded.fingerprint).unwrap().state,
        FileState::DeletedOnDisk
    );
}

#[test]
fn library_read_fingerprint_feeds_check_file() {
    let (_dir, mut lib) = lib_dir();
    let s = lib.save("buf.sql", "select 1;").unwrap();
    let loaded = lib.read(&s.id).unwrap().fingerprint;
    assert_eq!(loaded.sha256, s.fingerprint.sha256);
    let path = lib.absolute_path(&s.id).unwrap();
    assert_eq!(
        check_file(&path, &loaded).unwrap().state,
        FileState::Unchanged
    );
}

// ---- meta-command scan ----------------------------------------------------------------

#[test]
fn meta_scan_true_and_false_positives() {
    let script = r#"-- \c in a comment is fine
/* and \i here
   \set x too */
select 'C:\temp\new' as path, E'tab\there\\' as esc, "odd\name" as q;
do $body$
begin
  raise notice '\c inside dollar body';
end
$body$;
  \c other_db
select 1 \gexec
\copy t to 'out.csv'
"#;
    let found = scan_meta_commands(script);
    let summary: Vec<(&str, u32, u32, MetaPosition)> = found
        .iter()
        .map(|m| (m.command.as_str(), m.line, m.column, m.position))
        .collect();
    assert_eq!(
        summary,
        [
            ("\\c", 10, 3, MetaPosition::LineStart),
            ("\\gexec", 11, 10, MetaPosition::Inline),
            ("\\copy", 12, 1, MetaPosition::LineStart),
        ]
    );
    assert_eq!(found[0].text, "\\c other_db");
    assert!(has_meta_commands(script));
    assert!(!has_meta_commands(
        "select E'\\\\n', '\\', $$\\c$$, \"\\i\";\n-- \\set\n/* \\q */"
    ));
}
