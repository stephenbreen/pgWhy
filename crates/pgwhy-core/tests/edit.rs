//! Top-N grid + edit commit engine integration tests against containerized Postgres.

mod harness;

use harness::{PgFixture, PG_DB, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{ConnectionManager, ManagerOptions, NoTunnels, PgConn, Purpose};
use pgwhy_core::introspect::Introspector;
use pgwhy_core::query::*;
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use pgwhy_core::{ErrorClass, ErrorKind};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SEED: &str = r#"
create table public.items (
    id int primary key,
    name text,
    qty int not null default 0,
    price numeric(10,2),
    active boolean not null default true,
    note varchar(40) default 'none',
    created timestamptz not null default now()
);
insert into public.items (id, name, qty, price, active)
select g, 'item ' || g, g * 10, g * 1.5, g % 2 = 0 from generate_series(1, 150) g;
update public.items set name = 'it''s 50% off; really' where id = 7;

create table public.lines (
    order_id int not null,
    line_no int not null,
    sku text not null,
    primary key (order_id, line_no)
);
insert into public.lines values (1, 1, 'a'), (1, 2, 'b'), (2, 1, 'c');

create table public.tags (code text not null unique, label text);
insert into public.tags values ('x', 'ex'), ('y', 'why');

create table public.loose (a int, b text);
insert into public.loose values (1, 'one');

create schema "My Schema";
create table "My Schema"."we""ird; table" (
    "select" int primary key,
    "with space" text,
    "a""b" text default 'dflt',
    "Mixed" int
);
insert into "My Schema"."we""ird; table" values (1, 'one', 'q', 10), (2, 'two', 'r', 20);
"#;

struct Env {
    manager: ConnectionManager,
    id: ConnectionId,
    intro: Introspector,
}

impl Env {
    async fn new(pg: &PgFixture) -> Self {
        let entry = ConnectionEntry {
            id: ConnectionId::new(),
            name: "edit".into(),
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
        let env = Self {
            intro: Introspector::new(manager.clone()),
            manager,
            id,
        };
        env.conn().await.client.batch_execute(SEED).await.unwrap();
        env
    }

    async fn conn(&self) -> PgConn {
        self.manager
            .open(&self.id, PG_DB, Purpose::Grid)
            .await
            .unwrap()
    }

    async fn request(&self, schema: &str, name: &str, limit: u64) -> TopNRequest {
        let rel = self
            .intro
            .relation(&self.id, PG_DB, schema, name)
            .await
            .unwrap()
            .expect("relation exists");
        let prov = self
            .intro
            .edit_provenance(&self.id, PG_DB, schema, name)
            .await
            .unwrap()
            .unwrap();
        TopNRequest::for_relation(&rel, &prov, limit)
    }
}

async fn load(conn: &PgConn, req: &TopNRequest) -> (TopNQuery, Arc<ResultSet>) {
    let q = build_topn(req).unwrap();
    let set = Arc::new(q.new_result_set(DEFAULT_ROW_CAP));
    let out = run_topn(&conn.client, &q, &set).await.unwrap();
    assert!(!out.cancelled);
    (q, set)
}

fn col(set: &ResultSet, name: &str) -> usize {
    set.columns().iter().position(|c| c.name == name).unwrap()
}

/// Grid row whose `key` column shows `value`.
fn row_of(set: &ResultSet, key: &str, value: &str) -> usize {
    let c = col(set, key);
    (0..set.row_count())
        .find(|&r| set.cell(r, c).as_deref() == Some(value))
        .unwrap()
}

async fn scalar(conn: &PgConn, sql: &str) -> Option<String> {
    let rows = conn.client.simple_query(sql).await.unwrap();
    rows.iter().find_map(|m| match m {
        tokio_postgres::SimpleQueryMessage::Row(r) => Some(r.get(0).map(str::to_string)),
        _ => None,
    })?
}

#[tokio::test]
async fn top_n_edit_commit_end_to_end() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg).await;
    let grid = env.conn().await;
    let check = env.conn().await;
    let opts = CommitOptions::default();

    // ---- top-100 of a PK table, two cell edits + one delete, atomic commit
    let mut req = env.request("public", "items", DEFAULT_TOP_N).await;
    req.sort = vec![SortKey {
        column: "id".into(),
        descending: false,
    }];
    let (q, set) = load(&grid, &req).await;
    assert!(
        q.display_sql
            .ends_with("ORDER BY public.items.id ASC LIMIT 100"),
        "{}",
        q.display_sql
    );
    assert_eq!(set.row_count(), 100);
    assert_eq!(q.key_columns, vec!["id".to_string()]);
    let target = set.edit_target().expect("editable");
    assert_eq!(
        set.columns()[col(&set, "price")].type_name.as_deref(),
        Some("numeric(10,2)")
    );
    assert_eq!(
        set.cell(1, col(&set, "active")).as_deref(),
        Some("t"),
        "booleans read as t/f"
    );

    let (name, qty) = (col(&set, "name"), col(&set, "qty"));
    set.set_cell(row_of(&set, "id", "3"), name, Some("renamed".into()))
        .unwrap();
    set.set_cell(row_of(&set, "id", "4"), qty, Some("999".into()))
        .unwrap();
    set.delete_row(row_of(&set, "id", "5")).unwrap();
    assert_eq!(set.pending_counts().total_changes, 3);
    let plan = plan_edits(&set, &target, &opts).unwrap();
    let preview = plan.preview();
    assert_eq!(preview.len(), 3, "{preview:#?}");
    assert!(preview[0].starts_with("DELETE FROM public.items WHERE id = '5'::integer"));
    assert!(preview[1]
        .starts_with("UPDATE public.items SET name = 'renamed'::text WHERE id = '3'::integer"));
    assert!(preview[1].contains("name::text IS NOT DISTINCT FROM 'item 3'"));
    // executed text carries no values
    for st in &plan.statements {
        assert!(
            !st.sql.contains("renamed") && !st.sql.contains("'5'"),
            "{}",
            st.sql
        );
        assert!(st.sql.contains("$1::text"));
    }

    // cancel = discard: nothing reaches the server
    set.discard_pending().unwrap();
    assert!(plan_edits(&set, &target, &opts).unwrap().is_empty());
    set.set_cell(row_of(&set, "id", "3"), name, Some("renamed".into()))
        .unwrap();
    set.set_cell(row_of(&set, "id", "4"), qty, Some("999".into()))
        .unwrap();
    set.delete_row(row_of(&set, "id", "5")).unwrap();

    let report = commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap();
    assert_eq!((report.updated, report.deleted, report.inserted), (2, 1, 0));
    assert!(!set.has_pending());
    assert_eq!(
        scalar(&check, "select name from items where id = 3")
            .await
            .as_deref(),
        Some("renamed")
    );
    assert_eq!(
        scalar(&check, "select qty::text from items where id = 4")
            .await
            .as_deref(),
        Some("999")
    );
    assert_eq!(
        scalar(&check, "select count(*)::text from items where id = 5")
            .await
            .as_deref(),
        Some("0")
    );

    // ---- server-side criteria: values are parameters, metacharacters inert
    let mut req = env.request("public", "items", 1000).await;
    req.criteria = vec![Criterion {
        column: "name".into(),
        op: CriteriaOp::Eq,
        value: Some("x'; drop table items; --".into()),
    }];
    let (q, set2) = load(&grid, &req).await;
    assert_eq!(set2.row_count(), 0);
    assert!(q
        .display_sql
        .contains("name = 'x''; drop table items; --'::text"));
    assert!(!q.sql.contains("drop table"));
    req.criteria = vec![Criterion {
        column: "name".into(),
        op: CriteriaOp::Eq,
        value: Some("it's 50% off; really".into()),
    }];
    let (_, set2) = load(&grid, &req).await;
    assert_eq!(set2.row_count(), 1);
    req.criteria = vec![
        Criterion {
            column: "name".into(),
            op: CriteriaOp::Like,
            value: Some("%50\\% off;%".into()),
        },
        Criterion {
            column: "qty".into(),
            op: CriteriaOp::Ge,
            value: Some("70".into()),
        },
        Criterion {
            column: "note".into(),
            op: CriteriaOp::IsNotNull,
            value: None,
        },
    ];
    let (_, set2) = load(&grid, &req).await;
    assert_eq!(set2.row_count(), 1);
    assert_eq!(set2.cell(0, col(&set2, "id")).as_deref(), Some("7"));
    // server-side sort over the whole table, not just the loaded rows
    req.criteria.clear();
    req.sort = vec![SortKey {
        column: "qty".into(),
        descending: true,
    }];
    req.limit = 1;
    let (_, set2) = load(&grid, &req).await;
    assert_eq!(set2.cell(0, col(&set2, "id")).as_deref(), Some("150"));
    // a value that does not parse as the column's type is a classified SQL error
    req.criteria = vec![Criterion {
        column: "qty".into(),
        op: CriteriaOp::Eq,
        value: Some("1; select 1".into()),
    }];
    let q = build_topn(&req).unwrap();
    let s = q.new_result_set(10);
    let e = run_topn(&grid.client, &q, &s).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Sql);
    assert_eq!(s.state(), ResultState::Interrupted);
    assert_eq!(
        scalar(&check, "select count(*)::text from items")
            .await
            .as_deref(),
        Some("149")
    );

    // ---- INSERT with only some columns: the rest take their defaults; NULL vs ''
    let req = env.request("public", "items", 5).await;
    let (_, set) = load(&grid, &req).await;
    let target = set.edit_target().unwrap();
    let r = set.insert_row().unwrap();
    set.set_cell(r, col(&set, "id"), Some("1000".into()))
        .unwrap();
    set.set_cell(r, col(&set, "name"), Some(String::new()))
        .unwrap();
    set.set_cell(r, col(&set, "price"), None).unwrap();
    let r2 = set.insert_row().unwrap();
    set.set_cell(r2, col(&set, "id"), Some("1001".into()))
        .unwrap();
    set.set_cell(r2, col(&set, "note"), Some("a'b\"; c\\d".into()))
        .unwrap();
    let preview = plan_edits(&set, &target, &opts).unwrap().preview();
    assert_eq!(
        preview[0],
        "INSERT INTO public.items (id, name, price) VALUES ('1000'::integer, ''::text, NULL);"
    );
    commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap();
    let row = check
        .client
        .query_one(
            "select name, price::text, qty, active, note, created is not null from items where id = 1000",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, Option<String>>(0).as_deref(),
        Some(""),
        "empty string stays empty"
    );
    assert_eq!(row.get::<_, Option<String>>(1), None, "NULL stays NULL");
    assert_eq!(row.get::<_, i32>(2), 0);
    assert!(row.get::<_, bool>(3));
    assert_eq!(row.get::<_, Option<String>>(4).as_deref(), Some("none"));
    assert!(row.get::<_, bool>(5));
    assert_eq!(
        scalar(&check, "select note from items where id = 1001")
            .await
            .as_deref(),
        Some("a'b\"; c\\d"),
        "payloads are stored inertly"
    );

    // ---- NULL ↔ '' updates round-trip; bool edits
    let mut req = env.request("public", "items", 10).await;
    req.criteria = vec![Criterion {
        column: "id".into(),
        op: CriteriaOp::Ge,
        value: Some("1000".into()),
    }];
    req.sort = vec![SortKey {
        column: "id".into(),
        descending: false,
    }];
    let (_, set) = load(&grid, &req).await;
    assert_eq!(set.cell(0, col(&set, "name")).as_deref(), Some(""));
    assert_eq!(set.cell(0, col(&set, "price")), None);
    set.set_cell(0, col(&set, "name"), None).unwrap();
    set.set_cell(0, col(&set, "price"), Some("1.25".into()))
        .unwrap();
    set.set_cell(0, col(&set, "active"), Some("f".into()))
        .unwrap();
    set.set_cell(1, col(&set, "name"), Some(String::new()))
        .unwrap();
    commit_edits(&grid.client, &set, &set.edit_target().unwrap(), &opts)
        .await
        .unwrap();
    let (_, set) = load(&grid, &req).await;
    assert_eq!(set.cell(0, col(&set, "name")), None);
    assert_eq!(set.cell(0, col(&set, "price")).as_deref(), Some("1.25"));
    assert_eq!(set.cell(0, col(&set, "active")).as_deref(), Some("f"));
    assert_eq!(set.cell(1, col(&set, "name")).as_deref(), Some(""));
}

#[tokio::test]
async fn keys_identifiers_and_read_only() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg).await;
    let grid = env.conn().await;
    let check = env.conn().await;
    let opts = CommitOptions::default();

    // composite PK
    let (_, set) = load(&grid, &env.request("public", "lines", 100).await).await;
    let target = set.edit_target().unwrap();
    assert_eq!(target.key_names(), vec!["order_id", "line_no"]);
    let r = (0..set.row_count())
        .find(|&r| set.cell(r, 0).as_deref() == Some("1") && set.cell(r, 1).as_deref() == Some("2"))
        .unwrap();
    set.set_cell(r, 2, Some("B".into())).unwrap();
    // changing a key column is fine: the WHERE uses the original key
    let r3 = (0..set.row_count())
        .find(|&r| set.cell(r, 0).as_deref() == Some("2"))
        .unwrap();
    set.set_cell(r3, 1, Some("9".into())).unwrap();
    let preview = plan_edits(&set, &target, &opts).unwrap().preview();
    assert!(preview
        .iter()
        .any(|p| p.contains("WHERE order_id = '1'::integer AND line_no = '2'::integer")));
    commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap();
    assert_eq!(
        scalar(&check, "select string_agg(order_id || '/' || line_no || '/' || sku, ',' order by order_id, line_no) from lines").await.as_deref(),
        Some("1/1/a,1/2/B,2/9/c")
    );

    // no PK, unique NOT NULL key → editable by that key
    let (q, set) = load(&grid, &env.request("public", "tags", 100).await).await;
    assert_eq!(q.key_columns, vec!["code".to_string()]);
    let target = set.edit_target().unwrap();
    set.set_cell(row_of(&set, "code", "y"), 1, Some("WHY".into()))
        .unwrap();
    commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap();
    assert_eq!(
        scalar(&check, "select label from tags where code = 'y'")
            .await
            .as_deref(),
        Some("WHY")
    );

    // neither → read-only with a reason
    let (q, set) = load(&grid, &env.request("public", "loose", 100).await).await;
    assert!(q.read_only_reason.is_some());
    assert!(set.edit_target().is_none());
    assert!(!set.info().editable);

    // adversarial identifiers across select list, criteria, ORDER BY and edit SQL
    let mut req = env.request("My Schema", "we\"ird; table", 100).await;
    req.criteria = vec![Criterion {
        column: "with space".into(),
        op: CriteriaOp::ILike,
        value: Some("T%".into()),
    }];
    req.sort = vec![SortKey {
        column: "a\"b".into(),
        descending: true,
    }];
    let (q, set) = load(&grid, &req).await;
    assert!(
        q.sql.contains(r#"FROM "My Schema"."we""ird; table""#),
        "{}",
        q.sql
    );
    assert!(q
        .sql
        .contains(r#"ORDER BY "My Schema"."we""ird; table"."a""b" DESC"#));
    assert_eq!(set.row_count(), 1);
    assert_eq!(set.cell(0, col(&set, "select")).as_deref(), Some("2"));
    let target = set.edit_target().unwrap();
    assert_eq!(target.key_names(), vec!["select"]);
    set.set_cell(0, col(&set, "Mixed"), Some("21".into()))
        .unwrap();
    set.set_cell(
        0,
        col(&set, "a\"b"),
        Some("x\"); drop table lines; --".into()),
    )
    .unwrap();
    let r = set.insert_row().unwrap();
    set.set_cell(r, col(&set, "select"), Some("3".into()))
        .unwrap();
    set.set_cell(r, col(&set, "with space"), Some("three".into()))
        .unwrap();
    commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap();
    let row = check
        .client
        .query_one(
            r#"select "Mixed", "a""b" from "My Schema"."we""ird; table" where "select" = 2"#,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 21);
    assert_eq!(row.get::<_, String>(1), "x\"); drop table lines; --");
    assert_eq!(
        scalar(
            &check,
            r#"select "a""b" from "My Schema"."we""ird; table" where "select" = 3"#
        )
        .await
        .as_deref(),
        Some("dflt")
    );
    assert_eq!(
        scalar(&check, "select count(*)::text from lines")
            .await
            .as_deref(),
        Some("3")
    );
}

#[tokio::test]
async fn failed_commits_roll_back_and_keep_pending_edits() {
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let env = Env::new(&pg).await;
    let grid = env.conn().await;
    let other = env.conn().await;
    let mut req = env.request("public", "items", 20).await;
    req.sort = vec![SortKey {
        column: "id".into(),
        descending: false,
    }];
    let (_, set) = load(&grid, &req).await;
    let target = set.edit_target().unwrap();
    let opts = CommitOptions::default();
    let name = col(&set, "name");

    // concurrent external update of a target row (the column being edited)
    set.set_cell(row_of(&set, "id", "1"), name, Some("mine 1".into()))
        .unwrap();
    set.set_cell(row_of(&set, "id", "2"), name, Some("mine 2".into()))
        .unwrap();
    set.delete_row(row_of(&set, "id", "6")).unwrap();
    other
        .client
        .batch_execute("update items set name = 'theirs' where id = 2")
        .await
        .unwrap();
    let e = commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap_err();
    assert!(e.message.contains("id = 2"), "{}", e.message);
    assert!(e.message.contains("nothing was committed"));
    let rows = &e.data.as_ref().unwrap()["rows"];
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["affected"], 0);
    assert_eq!(rows[0]["key"][0]["value"], "2");
    assert_eq!(
        set.pending_counts().total_changes,
        3,
        "pending edits preserved"
    );
    assert_eq!(
        scalar(&other, "select name from items where id = 1")
            .await
            .as_deref(),
        Some("item 1"),
        "rolled back"
    );
    assert_eq!(
        scalar(&other, "select count(*)::text from items where id = 6")
            .await
            .as_deref(),
        Some("1")
    );

    // concurrent external delete is named the same way
    other
        .client
        .batch_execute("delete from items where id = 6")
        .await
        .unwrap();
    set.set_cell(row_of(&set, "id", "2"), name, None).unwrap();
    set.clear_cell(row_of(&set, "id", "2"), name).unwrap();
    let e = commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap_err();
    assert!(e.message.contains("id = 6"), "{}", e.message);
    assert_eq!(set.pending_counts().total_changes, 2);
    set.undelete_row(row_of(&set, "id", "6")).unwrap();

    // lock_timeout: another session holds the row lock → fails fast, pending kept
    other
        .client
        .batch_execute("begin; select * from items where id = 1 for update")
        .await
        .unwrap();
    let fast = CommitOptions {
        lock_timeout_ms: 300,
        ..CommitOptions::default()
    };
    let t = Instant::now();
    let e = commit_edits(&grid.client, &set, &target, &fast)
        .await
        .unwrap_err();
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "fails fast: {:?}",
        t.elapsed()
    );
    assert!(
        e.message.contains("locked by another session"),
        "{}",
        e.message
    );
    assert_eq!(e.class, ErrorClass::Retriable);
    assert_eq!(e.data.as_ref().unwrap()["sqlstate"], "55P03");
    assert_eq!(set.pending_counts().total_changes, 1);
    other.client.batch_execute("rollback").await.unwrap();

    // a DB error (constraint) rolls back too and names the row
    let r = set.insert_row().unwrap();
    set.set_cell(r, col(&set, "id"), Some("3".into())).unwrap(); // duplicate key
    let e = commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Sql);
    assert!(e.message.contains("new row"), "{}", e.message);
    assert_eq!(set.pending_counts().total_changes, 2);
    assert_eq!(
        scalar(&other, "select name from items where id = 1")
            .await
            .as_deref(),
        Some("item 1")
    );
    // the grid connection is usable afterwards (no transaction left open)
    assert_eq!(
        scalar(&grid, "select (now() = statement_timestamp())::text")
            .await
            .as_deref(),
        Some("true")
    );

    // fix and retry: commits
    set.delete_row(r).unwrap();
    commit_edits(&grid.client, &set, &target, &opts)
        .await
        .unwrap();
    assert_eq!(
        scalar(&other, "select name from items where id = 1")
            .await
            .as_deref(),
        Some("mine 1")
    );
    assert!(!set.has_pending());
}
