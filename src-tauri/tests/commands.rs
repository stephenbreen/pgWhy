//! Command-layer integration: the real command handlers over IPC (tauri mock runtime) against
//! a containerized Postgres — proves the wiring between UI contracts and the core crates.

mod harness;

use pgwhy_app::state::AppState;
use pgwhy_core::secret::MemoryStore;
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::test::{
    get_ipc_response, mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY,
};
use tauri::webview::InvokeRequest;
use tauri::WebviewWindow;

struct App {
    _app: tauri::App<MockRuntime>,
    webview: WebviewWindow<MockRuntime>,
    _dirs: tempfile::TempDir,
}

fn app() -> App {
    let dirs = tempfile::tempdir().unwrap();
    let state = AppState::with_secrets(
        dirs.path().join("config"),
        dirs.path().join("data"),
        Arc::new(MemoryStore::default()),
    )
    .expect("state");
    let app = pgwhy_app::configure(mock_builder())
        .manage(state)
        .build(mock_context(noop_assets()))
        .expect("app");
    let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .expect("webview");
    App {
        _app: app,
        webview,
        _dirs: dirs,
    }
}

fn invoke(app: &App, cmd: &str, args: Value) -> Result<Value, Value> {
    invoke_on(&app.webview, cmd, args)
}

fn invoke_on(webview: &WebviewWindow<MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
    get_ipc_response(
        webview,
        InvokeRequest {
            cmd: cmd.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            // the origin the app is served from: wry serves custom protocols over http on Windows
            url: if cfg!(windows) {
                "http://tauri.localhost"
            } else {
                "tauri://localhost"
            }
            .parse()
            .unwrap(),
            body: tauri::ipc::InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    )
    .map(|r| r.deserialize::<Value>().unwrap())
}

fn entry_json(pg: &harness::PgFixture) -> Value {
    json!({
        "id": "c-test", "name": "Container", "host": pg.host, "port": pg.port,
        "database": harness::PG_DB, "user": harness::PG_USER, "maintenance_db": "postgres",
        "sslmode": "disable", "env_tag": "local", "password_saved": false
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_explorer_and_script_as_round_trip_over_ipc() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    {
        let (client, conn) = tokio_postgres::connect(&pg.conninfo(), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        client
            .batch_execute("create table users (id int primary key, email text not null unique, nick text); insert into users values (1,'a@x',null)")
            .await
            .unwrap();
    }
    let app = app();

    let saved = invoke(
        &app,
        "save_connection",
        json!({ "entry": entry_json(&pg), "password": {"action":"set","value":harness::PG_PASSWORD,"persist":true}, "sshSecret": {"action":"unchanged"} }),
    )
    .expect("save");
    assert_eq!(saved["entry"]["password_saved"], true);
    assert!(
        !saved.to_string().contains(harness::PG_PASSWORD),
        "no response carries a stored credential"
    );
    let list = invoke(&app, "list_connections", json!({})).unwrap();
    assert!(!list.to_string().contains(harness::PG_PASSWORD));

    let info = invoke(
        &app,
        "connect",
        json!({ "id": "c-test", "password": null, "sshSecret": null }),
    )
    .expect("connect");
    assert_eq!(info["database"], harness::PG_DB);

    let dbs = invoke(
        &app,
        "explorer_databases",
        json!({ "connectionId": "c-test" }),
    )
    .unwrap();
    assert!(dbs
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["name"] == harness::PG_DB));
    let contents = invoke(
        &app,
        "explorer_schema",
        json!({ "connectionId": "c-test", "database": harness::PG_DB, "schema": "public" }),
    )
    .unwrap();
    let users = contents["relations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "users")
        .unwrap()
        .clone();
    assert_eq!(users["columns"][1]["type"], "text");

    let details = invoke(
        &app,
        "explorer_details",
        json!({ "connectionId": "c-test", "database": harness::PG_DB, "schema": "public", "name": "users", "refresh": false }),
    )
    .unwrap();
    assert_eq!(details["edit_key"], json!(["id"]));
    assert!(details["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["name"] == "users_email_key"));

    let select = invoke(
        &app,
        "explorer_script_as",
        json!({ "connectionId": "c-test", "database": harness::PG_DB, "schema": "public", "name": "users", "kind": "select" }),
    )
    .unwrap();
    assert!(select.as_str().unwrap().contains("FROM public.users"));

    // CREATE via the real pg_dump when matching client tools are installed
    match invoke(
        &app,
        "explorer_script_as",
        json!({ "connectionId": "c-test", "database": harness::PG_DB, "schema": "public", "name": "users", "kind": "create" }),
    ) {
        Ok(ddl) => assert!(
            ddl.as_str().unwrap().contains("CREATE TABLE public.users"),
            "{ddl}"
        ),
        Err(e) => {
            assert!(
                matches!(e["kind"].as_str(), Some("tool_missing" | "tool_version")),
                "{e}"
            );
            eprintln!(
                "NOTICE: pg_dump unavailable for CREATE script test: {}",
                e["message"]
            );
        }
    }

    let completion = invoke(
        &app,
        "completion_schema",
        json!({ "connectionId": "c-test", "database": harness::PG_DB }),
    )
    .unwrap();
    assert_eq!(
        completion["public"]["users"],
        json!(["id", "email", "nick"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_run_results_and_grid_edit_round_trip_over_ipc() {
    use tauri::Manager;
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    {
        let (client, conn) = tokio_postgres::connect(&pg.conninfo(), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        client
            .batch_execute("create table people (id int primary key, name text); insert into people select g, 'p' || g from generate_series(1,250) g")
            .await
            .unwrap();
    }
    let app = app();
    invoke(
        &app,
        "save_connection",
        json!({ "entry": entry_json(&pg), "password": {"action":"set","value":harness::PG_PASSWORD,"persist":true}, "sshSecret": {"action":"unchanged"} }),
    )
    .unwrap();

    // editor: run two statements, the first opens a transaction
    invoke(
        &app,
        "editor_open",
        json!({ "tabId": "t1", "connectionId": "c-test", "database": harness::PG_DB }),
    )
    .unwrap();
    let sql = "begin; select id, name from people order by id;";
    invoke(
        &app,
        "editor_run",
        json!({ "tabId": "t1", "sql": sql, "from": 0, "to": sql.len(), "policy": "stop", "events": "__CHANNEL__:900" }),
    )
    .expect("run");
    let state = app.webview.app_handle().state::<AppState>();
    let ids = state.results.results_of("t1");
    assert_eq!(ids.len(), 1, "one result set for the SELECT");
    let tsv = invoke(
        &app,
        "result_copy",
        json!({ "resultId": ids[0], "r0": 0, "r1": 1, "c0": 0, "c1": 1 }),
    )
    .unwrap();
    assert_eq!(tsv.as_str().unwrap(), "1\tp1\n2\tp2\n");
    assert_eq!(
        invoke(&app, "editor_txn_state", json!({ "tabId": "t1" })).unwrap(),
        "in_transaction"
    );
    // closing with an open transaction needs an explicit choice (R19)
    let refused = invoke(&app, "editor_close", json!({ "tabId": "t1", "txn": null })).unwrap_err();
    assert_eq!(refused["kind"], "refused");
    invoke(
        &app,
        "editor_close",
        json!({ "tabId": "t1", "txn": "rollback" }),
    )
    .unwrap();

    // grid: top-N, edit, preview, commit
    let view = invoke(
        &app,
        "grid_open",
        json!({ "gridId": "g1", "connectionId": "c-test", "database": harness::PG_DB, "schema": "public", "name": "people",
                "n": 100, "criteria": [], "sort": [{"column": "id", "dir": "desc"}] }),
    )
    .expect("grid_open");
    assert_eq!(view["row_count"], 100);
    assert_eq!(view["editable"], true);
    let first = invoke(
        &app,
        "result_copy",
        json!({ "resultId": view["result_id"], "r0": 0, "r1": 0, "c0": 0, "c1": 1 }),
    )
    .unwrap();
    assert_eq!(first.as_str().unwrap(), "250\tp250\n", "server-side sort");
    let p = invoke(
        &app,
        "grid_edit_cell",
        json!({ "gridId": "g1", "row": 0, "col": 1, "value": "renamed" }),
    )
    .unwrap();
    assert_eq!(p["pending"], 1);
    let preview = invoke(&app, "grid_preview", json!({ "gridId": "g1" })).unwrap();
    assert_eq!(preview.as_array().unwrap().len(), 1);
    let report = invoke(&app, "grid_commit", json!({ "gridId": "g1" })).expect("commit");
    assert_eq!(report["updated"], 1);

    // without an explicit sort, top-N is in key order — an updated row doesn't drop out
    let unsorted = invoke(
        &app,
        "grid_requery",
        json!({ "gridId": "g1", "criteria": [], "sort": [], "n": null }),
    )
    .unwrap();
    assert!(
        unsorted["sql"].as_str().unwrap().contains("ORDER BY"),
        "{}",
        unsorted["sql"]
    );
    let last = invoke(
        &app,
        "result_copy",
        json!({ "resultId": unsorted["result_id"], "r0": 99, "r1": 99, "c0": 0, "c1": 1 }),
    )
    .unwrap();
    assert_eq!(last.as_str().unwrap(), "100\tp100\n");

    let filtered = invoke(
        &app,
        "grid_requery",
        json!({ "gridId": "g1", "criteria": [{"column": "name", "op": "=", "value": "renamed"}], "sort": [], "n": null }),
    )
    .unwrap();
    assert_eq!(filtered["row_count"], 1);
    invoke(&app, "grid_close", json!({ "gridId": "g1" })).unwrap();
}

/// Closing a tab while its query runs cancels the query and frees what it produced: nothing
/// keeps streaming rows under an owner that no longer exists.
#[tokio::test(flavor = "multi_thread")]
async fn closing_a_tab_mid_run_cancels_the_query_and_releases_its_results() {
    use tauri::Manager;
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let app = app();
    invoke(
        &app,
        "save_connection",
        json!({ "entry": entry_json(&pg), "password": {"action":"set","value":harness::PG_PASSWORD,"persist":true}, "sshSecret": {"action":"unchanged"} }),
    )
    .unwrap();
    invoke(
        &app,
        "editor_open",
        json!({ "tabId": "t1", "connectionId": "c-test", "database": harness::PG_DB }),
    )
    .unwrap();

    // a first result set, then a statement that would run for a minute
    let sql = "select g from generate_series(1, 5000) g; select pg_sleep(60);";
    let webview = app.webview.clone();
    let run = std::thread::spawn(move || {
        invoke_on(
            &webview,
            "editor_run",
            json!({ "tabId": "t1", "sql": sql, "from": 0, "to": sql.len(), "policy": "stop", "events": "__CHANNEL__:901" }),
        )
    });
    let state = app.webview.app_handle().state::<AppState>();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while state.results.results_of("t1").is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the first result never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let started = std::time::Instant::now();
    invoke(&app, "editor_close", json!({ "tabId": "t1", "txn": null })).expect("close");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "close waited out the query instead of cancelling it"
    );
    let _ = run.join().expect("run thread");
    assert!(
        state.results.results_of("t1").is_empty(),
        "results of a closed tab are released"
    );

    let (client, conn) = tokio_postgres::connect(&pg.conninfo(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let sleeping: i64 = client
        .query_one(
            "select count(*) from pg_stat_activity where query like 'select pg_sleep(60)%' and pid <> pg_backend_pid()",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(sleeping, 0, "the server-side query was cancelled");
}

/// F1 through the command layer: save a workflow (with a library post-restore script),
/// preflight, confirm, run, and find the refreshed target in place.
#[tokio::test(flavor = "multi_thread")]
async fn refresh_workflow_round_trip_over_ipc() {
    use tauri::Manager;
    let Some(remote) = harness::pg(16).await else {
        return;
    };
    let Some(local) = harness::pg(16).await else {
        return;
    };
    {
        let (client, conn) = tokio_postgres::connect(&remote.conninfo(), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        client
            .batch_execute("create table customers (id int primary key, email text); insert into customers select g, 'c' || g || '@x' from generate_series(1, 300) g")
            .await
            .unwrap();
        let (client, conn) = tokio_postgres::connect(&local.conninfo(), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        client
            .batch_execute("create table stale (x int)")
            .await
            .unwrap();
    }
    let app = app();
    let mut remote_entry = entry_json(&remote);
    remote_entry["id"] = json!("c-remote");
    remote_entry["name"] = json!("Remote");
    remote_entry["env_tag"] = json!("production");
    let mut local_entry = entry_json(&local);
    local_entry["id"] = json!("c-local");
    local_entry["name"] = json!("Local");
    for e in [&remote_entry, &local_entry] {
        invoke(
            &app,
            "save_connection",
            json!({ "entry": e, "password": {"action":"set","value":harness::PG_PASSWORD,"persist":true}, "sshSecret": {"action":"unchanged"} }),
        )
        .unwrap();
    }
    let script = invoke(&app, "script_save", json!({ "path": "post/anonymize", "content": "update customers set email = 'user' || id || '@example.test';" })).unwrap();
    let script_id = script["info"]["id"].as_str().unwrap().to_string();

    // a Production target is refused at save time (AE7)
    let bad = json!({
        "id": "w-bad", "name": "bad", "source": {"connection": "c-local", "database": harness::PG_DB},
        "target": {"connection": "c-remote", "database": harness::PG_DB}, "options": {}, "has_succeeded": false
    });
    assert_eq!(
        invoke(&app, "workflow_save", json!({ "workflow": bad })).unwrap_err()["kind"],
        "refused"
    );

    let wf = json!({
        "id": "w1", "name": "Refresh local", "source": {"connection": "c-remote", "database": harness::PG_DB},
        "target": {"connection": "c-local", "database": harness::PG_DB}, "post_restore_script": script_id,
        "options": {}, "has_succeeded": false
    });
    invoke(&app, "workflow_save", json!({ "workflow": wf })).expect("save workflow");

    let report = invoke(&app, "workflow_preflight", json!({ "workflowId": "w1" })).unwrap();
    if report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == "tools" && c["status"] == "fail")
    {
        eprintln!("NOTICE: pg client tools unavailable — skipping the refresh run");
        return;
    }
    assert!(report["refused"].is_null(), "{report:#}");
    let ticket = report["ticket"].as_str().expect("ticket").to_string();
    let fingerprint = report["confirmation"]["fingerprint"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(report["confirmation"]["target_db"], harness::PG_DB);

    let run_id = invoke(
        &app,
        "workflow_start",
        json!({ "ticket": ticket, "fingerprint": fingerprint, "remember": false }),
    )
    .expect("start");
    let state = app.webview.app_handle().state::<AppState>();
    let record = state
        .engine()
        .wait(&pgwhy_workflow::RunId(run_id.as_str().unwrap().to_string()))
        .await
        .expect("run");
    assert_eq!(
        record.status,
        pgwhy_workflow::RunStatus::Succeeded,
        "{record:#?}"
    );

    let (client, conn) = tokio_postgres::connect(&local.conninfo(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let n: i64 = client
        .query_one("select count(*) from customers", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 300);
    let email: String = client
        .query_one("select email from customers where id = 7", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(email, "user7@example.test", "post-restore script ran");
    assert!(
        client
            .query_one("select 1 from pg_class where relname = 'stale'", &[])
            .await
            .is_err(),
        "old contents replaced"
    );

    let runs = invoke(&app, "run_list", json!({ "workflowId": "w1" })).unwrap();
    assert_eq!(runs.as_array().unwrap().len(), 1);
    assert!(
        !runs.to_string().contains(harness::PG_PASSWORD),
        "history never holds credentials"
    );
}

fn offline_entry(id: &str, tag: &str) -> Value {
    json!({
        "id": id, "name": id, "host": "127.0.0.1", "port": 1, "database": "app", "user": "u",
        "maintenance_db": "postgres", "sslmode": "disable", "env_tag": tag, "password_saved": false
    })
}

fn save_offline(app: &App, id: &str, tag: &str) {
    invoke(
        app,
        "save_connection",
        json!({ "entry": offline_entry(id, tag), "password": {"action":"unchanged"}, "sshSecret": {"action":"unchanged"} }),
    )
    .expect("save");
}

fn count_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    if e.path().is_dir() {
                        count_files(&e.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// The destructive gates run before any side effect: a refused restore/drop/rename never dumps
/// the target to disk (the safety backup) and never touches the server (these connections point
/// at a closed port, so reaching the server would be a different error).
#[test]
fn destructive_commands_are_refused_before_any_side_effect() {
    let app = app();
    save_offline(&app, "prod", "production");
    save_offline(&app, "dev", "dev");
    let backups = app._dirs.path().join("data").join("backups");
    let restore = |conn: &str, mode: &str, typed: Option<&str>| {
        invoke(
            &app,
            "restore_start",
            json!({
                "connectionId": conn,
                "request": {
                    "path": "/nonexistent/x.dump", "database": "app", "mode": mode,
                    "typed_name": typed, "backup_first": true
                },
                "events": "__CHANNEL__:901"
            }),
        )
        .unwrap_err()
    };

    for mode in ["on_top", "replace", "new_database"] {
        let e = restore("prod", mode, Some("app"));
        assert_eq!(e["kind"], "refused", "{mode}: {e}");
        assert!(e.to_string().contains("Production"), "{e}");
    }
    // a Dev target still needs the exact name
    for typed in [None, Some("App"), Some("")] {
        let e = restore("dev", "on_top", typed);
        assert_eq!(e["kind"], "refused", "{e}");
        assert!(e.to_string().contains("app"), "{e}");
    }

    let e = invoke(
        &app,
        "db_drop",
        json!({ "connectionId": "prod", "database": "app", "typedName": "app", "backupFirst": true }),
    )
    .unwrap_err();
    assert_eq!(e["kind"], "refused");
    let e = invoke(
        &app,
        "db_drop",
        json!({ "connectionId": "dev", "database": "app", "typedName": "nope", "backupFirst": true }),
    )
    .unwrap_err();
    assert_eq!(e["kind"], "refused");
    let e = invoke(
        &app,
        "db_rename",
        json!({ "connectionId": "prod", "database": "app", "to": "app2" }),
    )
    .unwrap_err();
    assert_eq!(e["kind"], "refused");

    assert_eq!(count_files(&backups), 0, "no backup file was written");
}

/// Paths from the webview must have come from a backend dialog (or be inside the library /
/// backup folders); the folders that pick binaries and write locations likewise.
#[test]
fn unpicked_paths_and_folders_are_refused() {
    use tauri::Manager;
    let app = app();
    let outside = app._dirs.path().join("elsewhere.sql");
    std::fs::create_dir_all(app._dirs.path()).unwrap();
    std::fs::write(&outside, "select 1").unwrap();
    let p = outside.display().to_string();

    for (cmd, args) in [
        ("file_read", json!({ "path": p })),
        ("file_write", json!({ "path": p, "content": "x" })),
        ("restore_inspect", json!({ "path": p })),
        ("reveal_path", json!({ "path": p })),
    ] {
        let e = invoke(&app, cmd, args).unwrap_err();
        assert_eq!(e["kind"], "refused", "{cmd}: {e}");
    }
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "select 1");

    // inside the library folder is fine
    invoke(
        &app,
        "script_save",
        json!({ "path": "a.sql", "content": "select 2" }),
    )
    .unwrap();
    let lib_file = app._dirs.path().join("data").join("scripts").join("a.sql");
    let read = invoke(
        &app,
        "file_read",
        json!({ "path": lib_file.display().to_string() }),
    )
    .unwrap();
    assert_eq!(read["content"], "select 2");

    // a granted file is accepted
    let state = app.webview.app_handle().state::<AppState>();
    state.grants.grant_file(&outside);
    invoke(&app, "file_read", json!({ "path": p })).expect("picked file");

    let mut settings =
        invoke(&app, "settings_get", json!({ "rescanTools": false })).unwrap()["settings"].clone();
    let tools = app._dirs.path().join("fake-bin");
    std::fs::create_dir_all(&tools).unwrap();
    settings["pg_tools_dirs"] = json!([tools.display().to_string()]);
    let e = invoke(
        &app,
        "settings_set",
        json!({ "settings": settings.clone() }),
    )
    .unwrap_err();
    assert_eq!(e["kind"], "refused", "{e}");
    let mut s2 = settings.clone();
    s2["pg_tools_dirs"] = json!([]);
    s2["backup_dir"] = json!(tools.display().to_string());
    assert_eq!(
        invoke(&app, "settings_set", json!({ "settings": s2.clone() })).unwrap_err()["kind"],
        "refused"
    );
    state.grants.grant_dir(&tools);
    invoke(&app, "settings_set", json!({ "settings": s2 })).expect("picked folder");
}
