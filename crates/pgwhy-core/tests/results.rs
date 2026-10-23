//! Result buffer tests (U12): window protocol, pending overlay, copy, export, retention, and
//! streaming through the executor into a buffer against containerized Postgres.

mod harness;

use harness::{PgFixture, PG_DB, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{ConnectionManager, ManagerOptions, NoTunnels, Notice, Purpose};
use pgwhy_core::query::*;
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_core::ErrorKind;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// A decoder mirroring ui/src/grid/protocol.ts `decodeChunk`
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Chunk {
    start: u32,
    count: u32,
    flags: Vec<u8>,
    /// cells[col][row]
    cells: Vec<Vec<Option<String>>>,
    /// pending[col][row]
    pending: Vec<Vec<bool>>,
}

fn decode(buf: &[u8]) -> Chunk {
    let u32_at = |p: usize| u32::from_le_bytes(buf[p..p + 4].try_into().unwrap());
    let mut p = 0;
    let start = u32_at(p);
    p += 4;
    let count = u32_at(p);
    p += 4;
    let ncols = u16::from_le_bytes(buf[p..p + 2].try_into().unwrap()) as usize;
    p += 2;
    let n = count as usize;
    let flags = buf[p..p + n].to_vec();
    p += n;
    let bits = n.div_ceil(8);
    let mut cells = Vec::new();
    let mut pending = Vec::new();
    for _ in 0..ncols {
        let nulls = &buf[p..p + bits];
        p += bits;
        let pend = &buf[p..p + bits];
        p += bits;
        let blob_len = u32_at(p) as usize;
        p += 4;
        let blob = &buf[p..p + blob_len];
        p += blob_len;
        let mut col = Vec::with_capacity(n);
        let mut pcol = Vec::with_capacity(n);
        let mut prev = 0usize;
        for i in 0..n {
            let end = u32_at(p) as usize;
            p += 4;
            let is_null = nulls[i >> 3] & (1 << (i & 7)) != 0;
            col.push(if is_null {
                assert_eq!(end, prev, "a NULL cell has no bytes");
                None
            } else {
                Some(String::from_utf8(blob[prev..end].to_vec()).unwrap())
            });
            pcol.push(pend[i >> 3] & (1 << (i & 7)) != 0);
            prev = end;
        }
        assert_eq!(prev, blob_len);
        cells.push(col);
        pending.push(pcol);
    }
    assert_eq!(p, buf.len(), "no trailing bytes");
    Chunk {
        start,
        count,
        flags,
        cells,
        pending,
    }
}

fn meta(name: &str, oid: Option<u32>, ty: Option<&str>) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        type_oid: oid,
        type_name: ty.map(str::to_string),
    }
}

/// id int, name text, payload jsonb, ok bool
fn sample() -> ResultSet {
    let set = ResultSet::new(
        vec![
            meta("id", Some(23), Some("int4")),
            meta("name", Some(25), Some("text")),
            meta("payload", Some(3802), Some("jsonb")),
            meta("ok", Some(16), Some("bool")),
        ],
        DEFAULT_ROW_CAP,
    );
    let rows: Vec<[Option<&str>; 4]> = vec![
        [Some("1"), Some("alice"), Some(r#"{"a": 1}"#), Some("t")],
        [Some("2"), Some(""), None, Some("f")],
        [Some("3"), None, Some("[1, 2]"), None],
        [
            Some("4"),
            Some("日本語 ünïcödé"),
            Some("not json"),
            Some("t"),
        ],
        [
            Some("5"),
            Some("tab\there \"q\"\nline"),
            Some("3"),
            Some("f"),
        ],
    ];
    for r in &rows {
        assert!(set.append_row(r));
    }
    set.finish(ResultState::Complete);
    set
}

#[test]
fn window_round_trips_nulls_empty_and_multibyte() {
    let set = sample();
    let c = decode(&set.encode_window(0, 256));
    assert_eq!((c.start, c.count), (0, 5));
    assert_eq!(c.flags, vec![ROW_CLEAN; 5]);
    assert_eq!(c.cells.len(), 4);
    assert_eq!(c.cells[1][0].as_deref(), Some("alice"));
    assert_eq!(
        c.cells[1][1].as_deref(),
        Some(""),
        "empty string is not NULL"
    );
    assert_eq!(c.cells[1][2], None, "NULL is not an empty string");
    assert_eq!(c.cells[1][3].as_deref(), Some("日本語 ünïcödé"));
    assert_eq!(c.cells[2][1], None);
    assert!(c.pending.iter().flatten().all(|p| !p));

    // a window in the middle: ends are relative to that window's blob
    let c = decode(&set.encode_window(3, 2));
    assert_eq!((c.start, c.count), (3, 2));
    assert_eq!(c.cells[0], vec![Some("4".into()), Some("5".into())]);
    assert_eq!(c.cells[1][1].as_deref(), Some("tab\there \"q\"\nline"));

    // beyond the rows streamed so far: only what exists, possibly nothing
    let c = decode(&set.encode_window(4, 100));
    assert_eq!(c.count, 1);
    let c = decode(&set.encode_window(10, 100));
    assert_eq!((c.start, c.count), (10, 0));
    assert_eq!(c.cells.len(), 4);
}

#[test]
fn window_grows_while_streaming() {
    let set = ResultSet::new(vec![meta("n", None, None)], DEFAULT_ROW_CAP);
    assert_eq!(decode(&set.encode_window(0, 256)).count, 0);
    for i in 0..300 {
        set.append_row(&[Some(&i.to_string())]);
    }
    let c = decode(&set.encode_window(256, 256));
    assert_eq!(c.count, 44);
    assert_eq!(c.cells[0][0].as_deref(), Some("256"));
    assert_eq!(set.state(), ResultState::Streaming);
    assert!(set.is_partial());
    // edits wait for the stream to finish
    let e = set.set_cell(0, 0, Some("x".into())).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
}

#[test]
fn cap_truncates_but_keeps_counting() {
    let set = ResultSet::new(vec![meta("n", None, None)], 10);
    for i in 0..25 {
        set.append_row(&[Some(&i.to_string())]);
    }
    assert_eq!(set.row_count(), 10);
    assert_eq!(set.rows_seen(), 25);
    assert!(set.truncated());
    set.finish(ResultState::Complete);
    assert!(set.is_partial(), "truncated is partial even when complete");
    let info = set.info();
    assert!(info.truncated);
    assert_eq!(info.rows_seen, 25);
}

#[test]
fn pending_overlay_renders_everywhere() {
    let set = sample();
    set.set_cell(0, 1, Some("ALICE".into())).unwrap();
    set.set_cell(2, 1, Some(String::new())).unwrap(); // NULL → ''
    set.set_cell(1, 1, None).unwrap(); // '' → NULL
    set.delete_row(3).unwrap();
    let ins = set.insert_row().unwrap();
    assert_eq!(ins, 5);
    set.set_cell(ins, 0, Some("6".into())).unwrap();
    set.set_cell(ins, 1, Some("new".into())).unwrap();
    // setting a cell back to its original value clears the edit
    set.set_cell(4, 0, Some("50".into())).unwrap();
    set.set_cell(4, 0, Some("5".into())).unwrap();

    let counts = set.pending_counts();
    assert_eq!(counts.updated_cells, 3);
    assert_eq!(counts.updated_rows, 3);
    assert_eq!(counts.deleted_rows, 1);
    assert_eq!(counts.inserted_rows, 1);
    assert_eq!(counts.total_changes, 5);

    let c = decode(&set.encode_window(0, 256));
    assert_eq!(c.count, 6);
    assert_eq!(
        c.flags,
        vec![
            ROW_UPDATED,
            ROW_UPDATED,
            ROW_UPDATED,
            ROW_DELETED,
            ROW_CLEAN,
            ROW_INSERTED
        ]
    );
    assert_eq!(c.cells[1][0].as_deref(), Some("ALICE"));
    assert_eq!(c.cells[1][1], None);
    assert_eq!(c.cells[1][2].as_deref(), Some(""));
    assert_eq!(c.cells[1][5].as_deref(), Some("new"));
    assert_eq!(c.cells[2][5], None, "unset inserted cells show as NULL");
    assert!(c.pending[1][0] && c.pending[1][1] && c.pending[1][2]);
    assert!(!c.pending[0][0] && !c.pending[0][4]);
    assert!(c.pending[0][5] && c.pending[1][5] && !c.pending[2][5]);
    // originals stay in the buffer
    assert_eq!(set.original_cell(0, 1).as_deref(), Some("alice"));

    let tsv = set.copy_tsv(0, 2, 0, 1, true);
    assert_eq!(tsv, "id\tname\n1\tALICE\n2\t\n3\t\n");

    let changes = set.pending_changes();
    assert!(matches!(changes[0], PendingChange::Delete { row: 3, .. }));
    assert!(matches!(
        changes.last(),
        Some(PendingChange::Insert { row: 5, .. })
    ));

    // deleting an inserted row just drops it; undelete restores a buffered one
    set.delete_row(5).unwrap();
    set.undelete_row(3).unwrap();
    assert_eq!(set.visible_row_count(), 5);
    let discarded = set.discard_pending().unwrap();
    assert_eq!(discarded.updated_cells, 3);
    assert!(!set.has_pending());
    assert_eq!(set.cell(0, 1).as_deref(), Some("alice"));
}

#[test]
fn tsv_quoting_matches_excel() {
    let set = sample();
    let tsv = set.copy_tsv(3, 4, 1, 1, false);
    assert_eq!(tsv, "日本語 ünïcödé\n\"tab\there \"\"q\"\"\nline\"\n");
    // ranges are clamped and may be given backwards
    assert_eq!(set.copy_tsv(9, 4, 9, 3, false), "f\n");
}

#[test]
fn csv_and_json_exports() {
    let dir = tempfile::tempdir().unwrap();
    let set = sample();
    set.set_cell(0, 1, Some("al,ice".into())).unwrap();
    set.delete_row(4).unwrap();

    let csv = dir.path().join("out.csv");
    let r = set
        .export_csv(
            &csv,
            &CsvOptions {
                header: true,
                null_literal: None,
                escape_formulas: false,
            },
            None,
        )
        .unwrap();
    assert_eq!(r.rows, 4);
    assert_eq!(r.pending_edits_included, 2);
    assert!(!r.partial);
    let text = std::fs::read_to_string(&csv).unwrap();
    assert_eq!(r.bytes, text.len() as u64);
    assert_eq!(
        text,
        "id,name,payload,ok\n\
         1,\"al,ice\",\"{\"\"a\"\": 1}\",t\n\
         2,\"\",,f\n\
         3,,\"[1, 2]\",\n\
         4,日本語 ünïcödé,not json,t\n"
    );

    let csv2 = dir.path().join("lit.csv");
    set.export_csv(
        &csv2,
        &CsvOptions {
            header: false,
            null_literal: Some("NULL".into()),
            escape_formulas: false,
        },
        None,
    )
    .unwrap();
    let text = std::fs::read_to_string(&csv2).unwrap();
    assert_eq!(text.lines().nth(1), Some("2,,NULL,f"));
    assert_eq!(text.lines().nth(2), Some("3,NULL,\"[1, 2]\",NULL"));

    let json = dir.path().join("out.json");
    let r = set.export_json(&json, None).unwrap();
    assert_eq!(r.rows, 4);
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(
        v,
        serde_json::json!([
            {"id": 1, "name": "al,ice", "payload": {"a": 1}, "ok": true},
            {"id": 2, "name": "", "payload": null, "ok": false},
            {"id": 3, "name": null, "payload": [1, 2], "ok": null},
            {"id": 4, "name": "日本語 ünïcödé", "payload": "not json", "ok": true},
        ])
    );

    // empty result: valid JSON / header-only CSV
    let empty = ResultSet::new(vec![meta("a", None, None)], 10);
    empty.finish(ResultState::Complete);
    let p = dir.path().join("empty.json");
    empty.export_json(&p, None).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&p).unwrap()).unwrap(),
        serde_json::json!([])
    );
    let p = dir.path().join("empty.csv");
    empty
        .export_csv(
            &p,
            &CsvOptions {
                header: true,
                null_literal: None,
                escape_formulas: false,
            },
            None,
        )
        .unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\n");
}

#[test]
fn json_handles_duplicate_names_and_special_numbers() {
    let set = ResultSet::new(
        vec![
            meta("a", None, Some("numeric")),
            meta("a", None, Some("double precision")),
            meta("b", None, Some("integer[]")),
        ],
        10,
    );
    set.append_row(&[Some("1.50"), Some("NaN"), Some("{1,2}")]);
    set.finish(ResultState::Cancelled);
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.json");
    let r = set.export_json(&p, None).unwrap();
    assert!(r.partial, "a cancelled stream exports as partial");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(
        v,
        serde_json::json!([{"a": 1.50, "a_2": "NaN", "b": "{1,2}"}])
    );
}

#[cfg(unix)]
#[test]
fn unwritable_or_missing_destination_leaves_no_file() {
    use std::os::unix::fs::PermissionsExt;
    let set = sample();
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let e = set.export_json(&locked.join("x.json"), None).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Disk, "{e:?}");
    assert_eq!(std::fs::read_dir(&locked).unwrap().count(), 0);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    let e = set
        .export_csv(&dir.path().join("nope/x.csv"), &CsvOptions::default(), None)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);

    let e = set.export_json(dir.path(), None).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config, "a directory is not a file");

    // cancelled from the progress callback: no file, no temp file
    let target = dir.path().join("cancelled.csv");
    let mut cb = |_p: ExportProgress| false;
    let e = set
        .export_csv(&target, &CsvOptions::default(), Some(&mut cb))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["locked".to_string()], "{names:?}");
}

fn filled(rows: usize) -> Arc<ResultSet> {
    let set = ResultSet::new(vec![meta("n", None, None)], DEFAULT_ROW_CAP);
    for i in 0..rows {
        set.append_row(&[Some(&i.to_string())]);
    }
    set.finish(ResultState::Complete);
    Arc::new(set)
}

#[test]
fn registry_evicts_oldest_but_never_current_focused_or_pending() {
    let reg = ResultRegistry::new(RegistryOptions {
        owner_row_budget: 100,
        owner_byte_budget: usize::MAX,
    });
    let tab = "tab-1";
    // run 1: three results
    reg.begin_run(tab);
    let a = filled(60);
    let b = filled(60);
    let c = filled(60);
    for s in [&a, &b, &c] {
        reg.register(tab, s.clone());
    }
    // all in the current run: nothing evictable
    let r = reg.enforce_budget(tab);
    assert!(r.evicted.is_empty());
    assert_eq!(r.retained_rows, 180);

    // run 2 makes run 1 evictable; b is focused, a holds pending edits
    reg.begin_run(tab);
    let d = filled(10);
    reg.register(tab, d.clone());
    reg.set_focused(tab, Some(b.id()));
    a.set_cell(0, 0, Some("x".into())).unwrap();
    let r = reg.enforce_budget(tab);
    assert_eq!(
        r.evicted,
        vec![EvictedResult {
            result_id: c.id().to_string(),
            rows: 60
        }]
    );
    assert_eq!(r.held_by_pending.len(), 1);
    assert_eq!(r.held_by_pending[0].result_id, a.id());
    assert_eq!(r.held_by_pending[0].pending, 1);
    assert!(reg.get(c.id()).is_none());
    assert!(reg.get(a.id()).is_some() && reg.get(b.id()).is_some() && reg.get(d.id()).is_some());

    // explicit eviction of a pending holder needs the discard confirmation
    assert_eq!(
        reg.evict(a.id(), false).unwrap_err().kind,
        ErrorKind::Refused
    );
    assert_eq!(reg.evict(a.id(), true).unwrap().unwrap().rows, 60);

    // another owner is unaffected; release_owner drops everything of one owner
    reg.begin_run("tab-2");
    let e = filled(500);
    reg.register("tab-2", e.clone());
    assert_eq!(
        reg.results_of(tab),
        vec![b.id().to_string(), d.id().to_string()]
    );
    assert_eq!(reg.release_owner(tab).len(), 2);
    assert!(reg.get(e.id()).is_some());
    assert!(reg.release(e.id()).is_some());
    assert_eq!(reg.require(e.id()).unwrap_err().kind, ErrorKind::Refused);
}

// ---------------------------------------------------------------------------------------------
// Integration: executor → BufferSink → ResultSet
// ---------------------------------------------------------------------------------------------

fn manager(pg: &PgFixture) -> (ConnectionManager, ConnectionId) {
    let entry = ConnectionEntry {
        id: ConnectionId::new(),
        name: "results".into(),
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
    (manager, id)
}

/// Forwards to a [`BufferSink`] and records the largest batch it saw.
struct Probe<S> {
    inner: S,
    max_batch: usize,
    batches: usize,
}

impl<S: ResultSink> ResultSink for Probe<S> {
    fn started(&mut self, i: usize, s: &StatementRange) {
        self.inner.started(i, s)
    }
    fn columns(&mut self, i: usize, c: &[ColumnMeta]) {
        self.inner.columns(i, c)
    }
    fn rows(&mut self, i: usize, r: RowBatch<'_>) {
        self.max_batch = self.max_batch.max(r.len());
        self.batches += 1;
        self.inner.rows(i, r)
    }
    fn notice(&mut self, i: Option<usize>, n: &Notice) {
        self.inner.notice(i, n)
    }
    fn completed(&mut self, i: usize, d: &StatementDone) {
        self.inner.completed(i, d)
    }
    fn failed(&mut self, i: usize, e: &SqlError) {
        self.inner.failed(i, e)
    }
    fn cancelled(&mut self, i: usize) {
        self.inner.cancelled(i)
    }
    fn txn_changed(&mut self, s: TxnStatus) {
        self.inner.txn_changed(s)
    }
}

#[tokio::test]
async fn streams_200k_rows_into_a_buffer() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let (manager, id) = manager(&pg);
    let session = QuerySession::new(manager.open(&id, PG_DB, Purpose::EditorTab).await.unwrap());
    let registry = Arc::new(ResultRegistry::default());
    let events: Arc<Mutex<Vec<BufferEvent>>> = Arc::default();
    let ev = events.clone();
    let sink = BufferSink::new(
        registry.clone(),
        "tab",
        BufferOptions::default(),
        move |e| ev.lock().unwrap().push(e),
    );
    let mut probe = Probe {
        inner: sink,
        max_batch: 0,
        batches: 0,
    };
    let sql = "select g as n, case when g % 3 = 0 then null when g % 3 = 1 then '' else 'v' || g end as s, \
                      g % 2 = 0 as even, json_build_object('g', g) as j, 'é' || g::text as m \
               from generate_series(1, 200000) g;\n\
               select 1 as one;\n\
               do $$ begin raise notice 'hi'; end $$;";
    let opts = RunOptions {
        batch_rows: 1000,
        ..RunOptions::default()
    };
    let summary = session.run_text(sql, &opts, &mut probe).await.unwrap();
    probe.inner.finish(&summary);
    assert_eq!(summary.outcome, RunOutcome::Completed);
    assert!(
        probe.max_batch <= 1000,
        "batches are bounded: {}",
        probe.max_batch
    );
    assert!(probe.batches >= 200);

    let ids = probe.inner.results().to_vec();
    assert_eq!(ids.len(), 2);
    let set = registry.require(&ids[0]).unwrap();
    assert_eq!(set.row_count(), 200_000);
    assert_eq!(set.state(), ResultState::Complete);
    assert!(!set.truncated());
    assert_eq!(set.columns()[0].type_name.as_deref(), Some("int4"));

    let c = decode(&set.encode_window(199_990, 256));
    assert_eq!((c.start, c.count), (199_990, 10));
    assert_eq!(c.cells[0][0].as_deref(), Some("199991"));
    // g=199991: 199991 % 3 = 2 → 'v199991'; g=199992 → NULL; g=199993 → ''
    assert_eq!(c.cells[1][0].as_deref(), Some("v199991"));
    assert_eq!(c.cells[1][1], None);
    assert_eq!(c.cells[1][2].as_deref(), Some(""));
    assert_eq!(c.cells[2][0].as_deref(), Some("f"));
    assert_eq!(c.cells[4][0].as_deref(), Some("é199991"));

    let ev = events.lock().unwrap().clone();
    let rows_events = ev
        .iter()
        .filter(|e| matches!(e, BufferEvent::Rows { .. }))
        .count();
    assert!((2..200).contains(&rows_events), "throttled: {rows_events}");
    assert!(ev.iter().any(|e| matches!(
        e,
        BufferEvent::StatementDone {
            index: 0,
            row_count: Some(200_000),
            truncated: false,
            ..
        }
    )));
    assert!(ev
        .iter()
        .any(|e| matches!(e, BufferEvent::Notice { index: Some(2), .. })));
    assert!(matches!(
        ev.last(),
        Some(BufferEvent::Finished {
            statements: 3,
            errors: 0,
            cancelled: false,
            ..
        })
    ));
    let json = serde_json::to_value(&ev[0]).unwrap();
    assert_eq!(json["type"], "statement_started");

    // JSON export of a real result: typed numbers/bools/json
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("r.json");
    let r = set.export_json(&p, None).unwrap();
    assert_eq!(r.rows, 200_000);
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(
        v[1],
        serde_json::json!({"n": 2, "s": "v2", "even": true, "j": {"g": 2}, "m": "é2"})
    );
    assert_eq!(v[2]["s"], serde_json::Value::Null);

    // a small cap truncates while the statement still drains; the old run is evictable
    let small = Arc::new(ResultRegistry::new(RegistryOptions {
        owner_row_budget: 10_000,
        owner_byte_budget: usize::MAX,
    }));
    for _ in 0..2 {
        let events: Arc<Mutex<Vec<BufferEvent>>> = Arc::default();
        let ev = events.clone();
        let mut sink = BufferSink::new(
            small.clone(),
            "tab",
            BufferOptions {
                row_cap: 5_000,
                ..BufferOptions::default()
            },
            move |e| ev.lock().unwrap().push(e),
        );
        let summary = session
            .run_text(
                "select g from generate_series(1, 50000) g",
                &opts,
                &mut sink,
            )
            .await
            .unwrap();
        sink.finish(&summary);
        let set = small.require(&sink.results()[0]).unwrap();
        assert_eq!(set.row_count(), 5_000);
        assert_eq!(set.rows_seen(), 50_000);
        assert!(set.truncated());
        let ev = events.lock().unwrap();
        assert!(ev.iter().any(|e| matches!(
            e,
            BufferEvent::StatementDone {
                rows_affected: 50_000,
                row_count: Some(5_000),
                truncated: true,
                ..
            }
        )));
    }
    assert_eq!(small.results_of("tab").len(), 2, "within budget: both kept");
}

#[tokio::test]
async fn cancelled_and_failed_streams_keep_partial_rows() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let (manager, id) = manager(&pg);
    let session = QuerySession::new(manager.open(&id, PG_DB, Purpose::EditorTab).await.unwrap());
    let registry = Arc::new(ResultRegistry::default());
    let mut sink = BufferSink::new(registry.clone(), "tab", BufferOptions::default(), |_| {});
    // rows stream, then the statement errors mid-way (division by zero at g = 3000)
    let summary = session
        .run_text(
            "select 1 / (3000 - g) as x from generate_series(1, 5000) g",
            &RunOptions {
                batch_rows: 100,
                ..RunOptions::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
    sink.finish(&summary);
    let set = registry.require(&sink.results()[0]).unwrap();
    assert_eq!(set.state(), ResultState::Interrupted);
    assert!(set.is_partial());
    let dir = tempfile::tempdir().unwrap();
    let r = set
        .export_csv(&dir.path().join("p.csv"), &CsvOptions::default(), None)
        .unwrap();
    assert!(r.partial);
    assert_eq!(r.rows, set.row_count() as u64);
}

// ---------------------------------------------------------------------------------------------
// Retention limits (bytes, not just rows), copy limit, CSV formula escaping
// ---------------------------------------------------------------------------------------------

#[test]
fn byte_cap_truncates_a_wide_result_before_the_row_cap() {
    // 40-byte cells, a 400-byte ceiling: the row cap (1000) would never fire.
    let set = ResultSet::with_limits(vec![meta("s", None, None)], 1_000, 400);
    let cell = "x".repeat(40);
    let mut kept = 0;
    for _ in 0..50 {
        if set.append_row(&[Some(&cell)]) {
            kept += 1;
        }
    }
    set.finish(ResultState::Complete);
    assert!(set.truncated(), "byte cap must flag the result truncated");
    assert_eq!(set.row_count(), kept);
    assert!(kept < 50, "kept {kept} rows past the byte cap");
    // 40 bytes of text + 4 bytes of offset per row fits ten rows in 400 bytes at most.
    assert!(kept <= 10, "kept {kept} rows in 400 bytes");
    assert!(set.approx_bytes() <= 400);
    assert!(
        set.rows_seen() == 50,
        "dropped rows are still counted as seen"
    );
}

#[test]
fn registry_evicts_on_the_byte_budget_too() {
    let reg = ResultRegistry::new(RegistryOptions {
        owner_row_budget: usize::MAX, // rows are not the constraint here
        owner_byte_budget: 2_000,
    });
    let tab = "tab-bytes";
    reg.begin_run(tab);
    let old = filled(200); // ~2 kB of offsets alone
    reg.register(tab, old.clone());
    reg.begin_run(tab);
    let cur = filled(200);
    reg.register(tab, cur.clone());
    let r = reg.enforce_budget(tab);
    assert_eq!(r.budget_bytes, 2_000);
    assert_eq!(r.evicted.len(), 1, "{r:?}");
    assert_eq!(r.evicted[0].result_id, old.id());
    assert!(r.retained_bytes <= 2_000, "{r:?}");
    assert!(reg.get(cur.id()).is_some());
}

#[test]
fn copy_is_refused_instead_of_building_an_unbounded_string() {
    let set = ResultSet::new(vec![meta("s", None, None)], 1_000);
    let cell = "y".repeat(100);
    for _ in 0..100 {
        set.append_row(&[Some(&cell)]);
    }
    set.finish(ResultState::Complete);

    let full = set.copy_tsv(0, 99, 0, 0, false);
    assert_eq!(full.lines().count(), 100);

    let e = set.copy_tsv_limited(0, 99, 0, 0, false, 1_024).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Refused);
    assert!(e.hint.unwrap_or_default().contains("export"));

    // a selection that fits still copies
    let small = set.copy_tsv_limited(0, 4, 0, 0, false, 1_024).unwrap();
    assert_eq!(small.lines().count(), 5);
}

/// A spreadsheet evaluates a cell that starts with `=`, `+`, `-` or `@`; exports prefix those
/// with `'` unless the caller opted out (CWE-1236).
#[test]
fn csv_export_neutralises_formula_cells() {
    let dir = tempfile::tempdir().unwrap();
    let set = ResultSet::new(
        vec![
            meta("v", Some(25), Some("text")),
            meta("n", Some(25), Some("text")),
        ],
        DEFAULT_ROW_CAP,
    );
    for (formula, plain) in [
        ("=1+1", "ok"),
        ("+SUM(A1)", "ok"),
        ("-1", "ok"),
        ("@cmd", "ok"),
        ("\tx", "ok"),
    ] {
        set.append_row(&[Some(formula), Some(plain)]);
    }
    set.finish(ResultState::Complete);

    let safe = dir.path().join("safe.csv");
    set.export_csv(
        &safe,
        &CsvOptions {
            header: false,
            null_literal: None,
            escape_formulas: true,
        },
        None,
    )
    .unwrap();
    let text = std::fs::read_to_string(&safe).unwrap();
    assert_eq!(text.lines().next(), Some("\"'=1+1\",ok"));
    assert!(text.contains("\"'+SUM(A1)\",ok"));
    assert!(text.contains("\"'-1\",ok"));
    assert!(text.contains("\"'@cmd\",ok"));
    assert!(text.contains("\"'\tx\",ok"));

    let raw = dir.path().join("raw.csv");
    set.export_csv(
        &raw,
        &CsvOptions {
            header: false,
            null_literal: None,
            escape_formulas: false,
        },
        None,
    )
    .unwrap();
    assert!(std::fs::read_to_string(&raw)
        .unwrap()
        .starts_with("=1+1,ok"));
}

/// A NULL literal containing a delimiter must not break the row's column count.
#[test]
fn csv_null_literal_is_quoted_when_it_needs_to_be() {
    let dir = tempfile::tempdir().unwrap();
    let set = ResultSet::new(vec![meta("a", None, None)], 10);
    set.append_row(&[None]);
    set.finish(ResultState::Complete);
    let p = dir.path().join("n.csv");
    set.export_csv(
        &p,
        &CsvOptions {
            header: false,
            null_literal: Some("<null,empty>".into()),
            escape_formulas: false,
        },
        None,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&p).unwrap().trim_end(),
        "\"<null,empty>\""
    );
}

#[test]
fn a_lowered_budget_applies_at_the_next_enforcement() {
    // Settings change the budgets on a live registry; no restart.
    let reg = ResultRegistry::new(RegistryOptions {
        owner_row_budget: 1000,
        owner_byte_budget: usize::MAX,
    });
    let tab = "tab-1";
    reg.begin_run(tab);
    let old = filled(60);
    reg.register(tab, old.clone());
    reg.begin_run(tab);
    reg.register(tab, filled(60));
    assert!(reg.enforce_budget(tab).evicted.is_empty());

    reg.set_options(RegistryOptions {
        owner_row_budget: 100,
        owner_byte_budget: usize::MAX,
    });
    assert_eq!(reg.options().owner_row_budget, 100);
    let r = reg.enforce_budget(tab);
    assert_eq!(r.evicted.len(), 1);
    assert!(reg.get(old.id()).is_none());
}
