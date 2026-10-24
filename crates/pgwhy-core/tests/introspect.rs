//! Introspection service integration tests against containerized Postgres.

mod harness;

use harness::{PgFixture, PG_DB, PG_PASSWORD, PG_USER};
use pgwhy_core::config::{ConfigStore, ConnectionEntry, ConnectionId, EnvTag, SslMode};
use pgwhy_core::conn::{ConnectionManager, ManagerOptions, NoTunnels, Purpose};
use pgwhy_core::introspect::quote::is_reserved_keyword;
use pgwhy_core::introspect::*;
use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
use std::sync::Arc;
use std::time::Duration;

const SEED: &str = r#"
create schema app;

create table app.customers (
    id bigint generated always as identity primary key,
    email varchar(200) not null,
    name text default 'anon',
    balance numeric(10,2) not null default 0,
    email_upper text generated always as (upper(email)) stored
);
-- partial unique: must never count as an edit key
create unique index customers_email_partial on app.customers (email) where balance > 0;
create index customers_name_idx on app.customers (name);
comment on table app.customers is 'people who pay';

create table app.order_lines (
    order_id int not null,
    line_no int not null,
    customer_id bigint references app.customers (id) on delete cascade,
    qty int check (qty > 0),
    primary key (order_id, line_no)
);

-- no PK, but a unique NOT NULL key
create table app.tags (
    code text not null,
    label text,
    constraint tags_code_key unique (code)
);
create unique index tags_label_key on app.tags (label);

-- no usable key at all: only a partial unique index on a NOT NULL column
create table app.events_log (msg text not null, at timestamptz);
create unique index events_log_msg_partial on app.events_log (msg) where at is not null;

create table app.measurements (id int not null, at date not null, v float8)
    partition by range (at);
create table app.measurements_2024 partition of app.measurements
    for values from ('2024-01-01') to ('2025-01-01');
create table app.measurements_2025 partition of app.measurements
    for values from ('2025-01-01') to ('2026-01-01');
alter table app.measurements add primary key (id, at);

create view app.customer_emails as select id, email from app.customers;
create materialized view app.customer_counts as select count(*) as n from app.customers;
create sequence app.ticket_seq start 100 increment 5;

create table app.never_analyzed (x int);

create table app."Weird ""Name""" ("select" int primary key, "ünï" text, "100%" int);
create table app."select" (id int);

create function app.add(a int, b int default 1) returns int
    language sql immutable as 'select a + b';
create procedure app.clear_tags() language sql as 'delete from app.tags';

insert into app.customers (email) values ('a@x'), ('b@x'), ('c@x');
insert into app.never_analyzed select generate_series(1, 7);
insert into app.measurements values (1, '2024-02-01', 1), (2, '2024-03-01', 2),
    (3, '2025-02-01', 3), (4, '2025-03-01', 4), (5, '2025-04-01', 5);
analyze app.customers;
"#;

fn entry(pg: &PgFixture) -> ConnectionEntry {
    ConnectionEntry {
        id: ConnectionId::new(),
        name: "introspect".into(),
        group: None,
        host: pg.host.clone(),
        port: pg.port,
        database: PG_DB.into(),
        user: PG_USER.into(),
        maintenance_db: "postgres".into(),
        sslmode: SslMode::Disable,
        ssl_root_cert: None,
        tunnel: None,
        env_tag: EnvTag::Local,
        password_saved: true,
    }
}

fn manager(e: &ConnectionEntry) -> ConnectionManager {
    let vault = Arc::new(Vault::new(Arc::new(MemoryStore::default())));
    vault
        .store(
            &SecretKey::new(&e.id, SecretKind::DbPassword),
            Secret::new(PG_PASSWORD),
            true,
        )
        .unwrap();
    let m = ConnectionManager::new(vault, Arc::new(NoTunnels), ManagerOptions::default());
    m.set_entries([e.clone()]);
    m
}

async fn admin(pg: &PgFixture, db: &str) -> tokio_postgres::Client {
    let conninfo = format!(
        "host={} port={} user={PG_USER} password={PG_PASSWORD} dbname={db}",
        pg.host, pg.port
    );
    let (client, conn) = tokio_postgres::connect(&conninfo, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
}

struct Seeded {
    pg: PgFixture,
    entry: ConnectionEntry,
    intro: Introspector,
}

/// Start `major`, seed it on a connection that is then closed (so its table stats are
/// flushed at backend exit), and build an introspector over a fresh manager.
async fn seeded(major: u32) -> Option<Seeded> {
    let pg = harness::pg(major).await?;
    {
        let c = admin(&pg, PG_DB).await;
        c.batch_execute(SEED).await.unwrap();
    }
    let entry = entry(&pg);
    let intro = Introspector::new(manager(&entry));
    Some(Seeded { pg, entry, intro })
}

fn col(name: &str, ordinal: i16, ty: &str, not_null: bool) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        ordinal,
        data_type: ty.into(),
        type_oid: 0,
        not_null,
        default: None,
        identity: None,
        generated: None,
        generation_expr: None,
        comment: None,
    }
}

/// Columns with type oids blanked (oids are server-assigned for user types; builtins are
/// stable but asserting them adds nothing here).
fn cols(r: &RelationInfo) -> Vec<ColumnInfo> {
    r.columns
        .iter()
        .cloned()
        .map(|mut c| {
            assert_ne!(c.type_oid, 0, "{} has a type oid", c.name);
            c.type_oid = 0;
            c
        })
        .collect()
}

fn names(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

async fn details(s: &Seeded, name: &str) -> Arc<RelationDetails> {
    s.intro
        .relation_details(&s.entry.id, PG_DB, "app", name)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{name} exists"))
}

/// The whole seeded model, asserted exactly; shared by the PG 13/16/18 runs.
async fn assert_seeded_model(s: &Seeded) {
    let id = &s.entry.id;

    // ---- databases (maintenance connection) and schemas
    let dbs = s.intro.databases(id).await.unwrap();
    let test_db = dbs
        .iter()
        .find(|d| d.name == PG_DB)
        .expect("test db listed");
    assert_eq!(test_db.owner, PG_USER);
    assert_eq!(test_db.encoding, "UTF8");
    assert!(test_db.allow_connections && !test_db.is_template);
    assert!(dbs.iter().any(|d| d.name == "template0" && d.is_template));
    assert_eq!(
        test_db.locale_provider.is_some(),
        s.pg.major >= 15,
        "{test_db:?}"
    );

    let schemas = s.intro.schemas(id, PG_DB).await.unwrap();
    let schema_names: Vec<_> = schemas
        .iter()
        .map(|s| (s.name.as_str(), s.is_system))
        .collect();
    assert_eq!(
        schema_names,
        [
            ("app", false),
            ("public", false),
            ("information_schema", true),
            ("pg_catalog", true)
        ]
    );

    // ---- one schema expansion: exactly one round trip
    let before = s.intro.round_trips();
    let app = s.intro.schema(id, PG_DB, "app").await.unwrap();
    assert_eq!(
        s.intro.round_trips() - before,
        1,
        "one round trip per schema"
    );
    let again = s.intro.schema(id, PG_DB, "app").await.unwrap();
    assert_eq!(
        s.intro.round_trips() - before,
        1,
        "second expansion is served cached"
    );
    assert!(Arc::ptr_eq(&app, &again));

    let rels: Vec<_> = app
        .relations
        .iter()
        .map(|r| (r.name.as_str(), r.kind))
        .collect();
    use RelationKind::*;
    assert_eq!(
        rels,
        [
            ("Weird \"Name\"", Table),
            ("customer_counts", MaterializedView),
            ("customer_emails", View),
            ("customers", Table),
            ("customers_id_seq", Sequence),
            ("events_log", Table),
            ("measurements", PartitionedTable),
            ("measurements_2024", Table),
            ("measurements_2025", Table),
            ("never_analyzed", Table),
            ("order_lines", Table),
            ("select", Table),
            ("tags", Table),
            ("ticket_seq", Sequence),
        ]
    );

    let customers = app.relation("customers").unwrap();
    assert_eq!(customers.owner, PG_USER);
    assert_eq!(customers.comment.as_deref(), Some("people who pay"));
    assert_eq!(
        cols(customers),
        vec![
            ColumnInfo {
                identity: Some(Identity::Always),
                ..col("id", 1, "bigint", true)
            },
            col("email", 2, "character varying(200)", true),
            ColumnInfo {
                default: Some("'anon'::text".into()),
                ..col("name", 3, "text", false)
            },
            ColumnInfo {
                default: Some("0".into()),
                ..col("balance", 4, "numeric(10,2)", true)
            },
            ColumnInfo {
                generated: Some(Generated::Stored),
                generation_expr: Some("upper((email)::text)".into()),
                ..col("email_upper", 5, "text", false)
            },
        ]
    );
    assert!(!customers.columns[0].is_writable() && !customers.columns[4].is_writable());
    assert!(customers.columns[1].is_writable());

    let weird = app.relation("Weird \"Name\"").unwrap();
    assert_eq!(
        cols(weird),
        vec![
            col("select", 1, "integer", true),
            col("ünï", 2, "text", false),
            col("100%", 3, "integer", false),
        ]
    );

    // partitioned table children grouped under the parent
    let m = app.relation("measurements").unwrap();
    assert_eq!(m.partition_key.as_deref(), Some("RANGE (at)"));
    assert_eq!(
        m.partitions,
        [
            QualifiedName::new("app", "measurements_2024"),
            QualifiedName::new("app", "measurements_2025")
        ]
    );
    assert_eq!(m.partition_of, None);
    let m24 = app.relation("measurements_2024").unwrap();
    assert_eq!(
        m24.partition_of,
        Some(QualifiedName::new("app", "measurements"))
    );
    assert_eq!(
        m24.partition_bound.as_deref(),
        Some("FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')")
    );
    assert!(m24.is_partition() && !customers.is_partition());

    assert_eq!(
        cols(app.relation("customer_emails").unwrap()),
        vec![
            col("id", 1, "bigint", false),
            col("email", 2, "character varying(200)", false)
        ]
    );
    assert!(app.relation("ticket_seq").unwrap().columns.is_empty());

    // ---- details: constraints, indexes, stats, provenance
    let ol = details(s, "order_lines").await;
    let cons: Vec<_> = ol
        .constraints
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.kind,
                names(&c.columns),
                c.definition.as_str(),
            )
        })
        .collect();
    assert_eq!(
        cons,
        [
            (
                "order_lines_pkey",
                ConstraintKind::PrimaryKey,
                vec!["order_id", "line_no"],
                "PRIMARY KEY (order_id, line_no)"
            ),
            (
                "order_lines_customer_id_fkey",
                ConstraintKind::ForeignKey,
                vec!["customer_id"],
                "FOREIGN KEY (customer_id) REFERENCES app.customers(id) ON DELETE CASCADE"
            ),
            (
                "order_lines_qty_check",
                ConstraintKind::Check,
                vec!["qty"],
                "CHECK (qty > 0)"
            ),
        ]
    );
    assert_eq!(
        ol.constraints[1].foreign_key,
        Some(ForeignKeyTarget {
            table: QualifiedName::new("app", "customers"),
            columns: vec!["id".into()],
            on_update: FkAction::NoAction,
            on_delete: FkAction::Cascade,
        })
    );
    assert_eq!(ol.constraints[0].index.as_deref(), Some("order_lines_pkey"));
    assert_eq!(ol.constraints[1].index, None);
    assert!(ol.constraints.iter().all(|c| c.validated && !c.deferrable));
    assert_eq!(
        ol.provenance,
        EditProvenance {
            key: Some(EditKey::PrimaryKey {
                constraint: Some("order_lines_pkey".into()),
                columns: vec!["order_id".into(), "line_no".into()]
            }),
            read_only_reason: None
        }
    );
    assert_eq!(ol.primary_key().unwrap().name, "order_lines_pkey");

    let c = details(s, "customers").await;
    let idx: Vec<_> = c
        .indexes
        .iter()
        .map(|i| {
            (
                i.name.as_str(),
                i.definition.as_str(),
                i.is_unique,
                i.is_primary,
                i.predicate.as_deref(),
                names(&i.columns),
            )
        })
        .collect();
    assert_eq!(
        idx,
        [
            (
                "customers_pkey",
                "CREATE UNIQUE INDEX customers_pkey ON app.customers USING btree (id)",
                true,
                true,
                None,
                vec!["id"]
            ),
            (
                "customers_email_partial",
                "CREATE UNIQUE INDEX customers_email_partial ON app.customers USING btree (email) WHERE (balance > (0)::numeric)",
                true,
                false,
                Some("(balance > (0)::numeric)"),
                vec!["email"]
            ),
            (
                "customers_name_idx",
                "CREATE INDEX customers_name_idx ON app.customers USING btree (name)",
                false,
                false,
                None,
                vec!["name"]
            ),
        ]
    );
    assert!(c.indexes.iter().all(|i| i.is_valid && i.method == "btree"));
    assert_eq!(c.indexes[0].constraint.as_deref(), Some("customers_pkey"));
    assert_eq!(c.indexes[1].constraint, None);
    // analyzed: reltuples is authoritative
    assert_eq!(c.stats.estimated_rows, Some(3));
    assert_eq!(c.stats.estimate_source, Some(RowEstimateSource::Reltuples));
    assert!(c.stats.total_bytes.unwrap() > 0 && c.stats.indexes_bytes.unwrap() > 0);
    assert_eq!(c.updatability, None);

    // no PK: the unique NOT NULL key (not the nullable unique index on label)
    let tags = details(s, "tags").await;
    assert_eq!(
        tags.provenance.key,
        Some(EditKey::UniqueKey {
            index: "tags_code_key".into(),
            constraint: Some("tags_code_key".into()),
            columns: vec!["code".into()]
        })
    );
    let label = tags
        .indexes
        .iter()
        .find(|i| i.name == "tags_label_key")
        .unwrap();
    assert!(label.is_unique && !label.key_columns_not_null);

    // only a partial unique index: read-only with a reason
    let ev = details(s, "events_log").await;
    assert_eq!(ev.provenance.key, None);
    assert!(ev.provenance.read_only_reason.is_some());
    assert!(ev.indexes[0].is_partial() && ev.indexes[0].key_columns_not_null);

    let weird = details(s, "Weird \"Name\"").await;
    assert_eq!(
        weird.provenance.key.as_ref().map(EditKey::columns),
        Some(&["select".to_string()][..])
    );

    let mp = details(s, "measurements").await;
    assert_eq!(
        mp.provenance.key.as_ref().map(EditKey::columns),
        Some(&["id".to_string(), "at".to_string()][..])
    );

    let view = details(s, "customer_emails").await;
    assert_eq!(view.provenance.key, None);
    assert_eq!(
        view.updatability,
        Some(Updatability {
            insertable: true,
            updatable: true,
            deletable: true
        })
    );
    assert!(view
        .view_definition
        .as_deref()
        .is_some_and(|d| d.contains("FROM app.customers") || d.contains("FROM customers")));
    assert_eq!(view.stats.estimated_rows, None);

    let seq = details(s, "ticket_seq").await;
    assert_eq!(
        seq.sequence,
        Some(SequenceInfo {
            data_type: "bigint".into(),
            start: 100,
            increment: 5,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
            last_value: None,
            owned_by: None,
        })
    );

    let idseq = details(s, "customers_id_seq").await;
    assert_eq!(
        idseq.sequence.as_ref().and_then(|q| q.owned_by.clone()),
        Some(OwnedBy {
            table: QualifiedName::new("app", "customers"),
            column: "id".into()
        })
    );

    // never analyzed (reltuples −1 on 14+, 0 on 13) → live tuples; partitioned → leaf sum.
    // Stats reach the cumulative system asynchronously, so poll briefly.
    let mut last = None;
    for _ in 0..40 {
        let na = s
            .intro
            .refresh_relation(id, PG_DB, "app", "never_analyzed")
            .await
            .unwrap()
            .unwrap();
        let mp = s
            .intro
            .refresh_relation(id, PG_DB, "app", "measurements")
            .await
            .unwrap()
            .unwrap();
        let cu = s
            .intro
            .refresh_relation(id, PG_DB, "app", "customers")
            .await
            .unwrap()
            .unwrap();
        // the PG13/14 stats collector reports ANALYZE asynchronously too
        let analyzed = cu
            .stats
            .last_analyze
            .as_deref()
            .is_some_and(|t| t.len() == 20 && t.ends_with('Z'));
        let got = (
            analyzed,
            na.stats.estimated_rows,
            na.stats.estimate_source,
            mp.stats.estimated_rows,
            mp.stats.estimate_source,
        );
        if analyzed && got.1 == Some(7) && got.3 == Some(5) {
            assert!(
                na.stats.reltuples <= 0.0,
                "never analyzed: {}",
                na.stats.reltuples
            );
            last = Some(got);
            break;
        }
        last = Some(got);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        last,
        Some((
            true,
            Some(7),
            Some(RowEstimateSource::LiveTuples),
            Some(5),
            Some(RowEstimateSource::Partitions)
        ))
    );

    // ---- functions (separate lazy pull)
    let fns = s.intro.functions(id, PG_DB, "app").await.unwrap();
    let f: Vec<_> = fns
        .iter()
        .map(|f| {
            (
                f.name.as_str(),
                f.kind,
                f.identity_args.as_str(),
                f.arguments.as_str(),
                f.result.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        f,
        [
            (
                "add",
                FunctionKind::Function,
                "a integer, b integer",
                "a integer, b integer DEFAULT 1",
                Some("integer")
            ),
            ("clear_tags", FunctionKind::Procedure, "", "", None),
        ]
    );
    assert_eq!(fns[0].volatility, Volatility::Immutable);
    assert_eq!(fns[0].language, "sql");

    // ---- quoting helper produces SQL the server accepts for every odd name
    let conn = s.intro.manager().introspection(id, PG_DB).await.unwrap();
    for r in &app.relations {
        if r.kind == Sequence {
            continue;
        }
        let list: Vec<_> = r.columns.iter().map(|c| quote_ident(&c.name)).collect();
        let sql = format!(
            "select {} from {} limit 0",
            list.join(", "),
            qualified(&r.schema, &r.name)
        );
        conn.client
            .simple_query(&sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[tokio::test]
async fn model_matches_seed_pg16() {
    let Some(s) = seeded(16).await else { return };
    assert_seeded_model(&s).await;
}

#[tokio::test]
async fn model_matches_seed_pg13() {
    let Some(s) = seeded(13).await else { return };
    assert_seeded_model(&s).await;
}

#[tokio::test]
async fn model_matches_seed_pg18() {
    let Some(s) = seeded(18).await else { return };
    assert_seeded_model(&s).await;
}

#[tokio::test]
async fn cache_invalidation_lazy_databases_and_search() {
    let Some(s) = seeded(16).await else { return };
    let id = &s.entry.id;
    let a = admin(&s.pg, PG_DB).await;

    // serve-cached: external DDL is not visible until invalidated
    let app = s.intro.schema(id, PG_DB, "app").await.unwrap();
    a.batch_execute("create table app.added_later (x int)")
        .await
        .unwrap();
    let before = s.intro.round_trips();
    let cached = s.intro.schema(id, PG_DB, "app").await.unwrap();
    assert!(Arc::ptr_eq(&app, &cached) && cached.relation("added_later").is_none());
    assert_eq!(s.intro.round_trips(), before);
    assert!(s.intro.cached_schema(id, PG_DB, "app").is_some());

    s.intro.invalidate_db(id, PG_DB);
    assert!(s.intro.cached_schema(id, PG_DB, "app").is_none());
    let fresh = s.intro.schema(id, PG_DB, "app").await.unwrap();
    assert!(fresh.relation("added_later").is_some());
    assert_eq!(s.intro.round_trips(), before + 1);

    // refresh_schema re-pulls; refresh_relation patches the cached contents
    a.batch_execute("alter table app.added_later add column y text not null default 'q'")
        .await
        .unwrap();
    let d = s
        .intro
        .refresh_relation(id, PG_DB, "app", "added_later")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d.relation.columns.len(), 2);
    let patched = s.intro.cached_schema(id, PG_DB, "app").unwrap();
    assert_eq!(patched.relation("added_later").unwrap().columns.len(), 2);

    // a refetch miss is "object no longer exists", and drops it from the cache
    a.batch_execute("drop table app.added_later").await.unwrap();
    assert!(s
        .intro
        .refresh_relation(id, PG_DB, "app", "added_later")
        .await
        .unwrap()
        .is_none());
    assert!(s
        .intro
        .cached_schema(id, PG_DB, "app")
        .unwrap()
        .relation("added_later")
        .is_none());
    let r = s.intro.refresh_schema(id, PG_DB, "app").await.unwrap();
    assert!(r.relation("added_later").is_none() && r.relation("customers").is_some());

    // lazy second database: its own introspection connection
    let dbs = s.intro.databases(id).await.unwrap();
    assert!(!dbs.iter().any(|d| d.name == "otherdb"));
    {
        let pg_admin = admin(&s.pg, "postgres").await;
        pg_admin
            .batch_execute("create database otherdb")
            .await
            .unwrap();
    }
    let dbs = s.intro.databases(id).await.unwrap();
    assert!(!dbs.iter().any(|d| d.name == "otherdb"), "served cached");
    let dbs = s.intro.refresh_databases(id).await.unwrap();
    assert!(dbs.iter().any(|d| d.name == "otherdb"));
    assert!(s.intro.database_size(id, "otherdb").await.unwrap().unwrap() > 0);
    {
        let o = admin(&s.pg, "otherdb").await;
        o.batch_execute("create table public.thing (id int primary key)")
            .await
            .unwrap();
    }
    let other = s.intro.schema(id, "otherdb", "public").await.unwrap();
    assert_eq!(other.relations.len(), 1);
    assert_eq!(other.relations[0].name, "thing");
    let live = s.intro.manager().live_connections();
    // listing databases uses a short-lived maintenance connection: the maintenance database
    // isn't held open (the tree shows it as not connected until the user opens it)
    assert!(
        !live.iter().any(|c| c.database == "postgres"),
        "no lingering connection to the maintenance database: {live:?}"
    );
    for db in [PG_DB, "otherdb"] {
        assert_eq!(
            live.iter()
                .filter(|c| c.database == db && c.purpose == Purpose::Introspection)
                .count(),
            1,
            "one shared introspection connection for {db}: {live:?}"
        );
    }

    // invalidate_server drops every database and the database list
    let before = s.intro.round_trips();
    s.intro.invalidate_server(id);
    assert!(s.intro.cached_schema(id, "otherdb", "public").is_none());
    assert!(s.intro.cached_contents(id, PG_DB).is_empty());
    s.intro.databases(id).await.unwrap();
    assert_eq!(s.intro.round_trips(), before + 1);

    // ---- go-to-object: catalog query while not fully cached
    let hits = s
        .intro
        .search_objects(id, PG_DB, "CUSTOM", 10)
        .await
        .unwrap();
    let got: Vec<_> = hits.iter().map(|h| (h.name.as_str(), h.kind)).collect();
    assert_eq!(
        got,
        [
            ("customers", ObjectKind::Table),
            ("customer_counts", ObjectKind::MaterializedView),
            ("customer_emails", ObjectKind::View),
            ("customers_id_seq", ObjectKind::Sequence),
        ]
    );
    let add = s.intro.search_objects(id, PG_DB, "add", 10).await.unwrap();
    assert_eq!(add[0].name, "add");
    assert_eq!(add[0].kind, ObjectKind::Function);
    assert_eq!(
        add[0].identity_args.as_deref(),
        Some("a integer, b integer")
    );
    // LIKE metacharacters are literal
    assert!(s
        .intro
        .search_objects(id, PG_DB, "%", 10)
        .await
        .unwrap()
        .is_empty());
    // `_` would be a single-char wildcard matching "customers"
    assert!(s
        .intro
        .search_objects(id, PG_DB, "cu_tomers", 10)
        .await
        .unwrap()
        .is_empty());
    let weird = s
        .intro
        .search_objects(id, PG_DB, "\"name\"", 10)
        .await
        .unwrap();
    assert_eq!(weird.len(), 1);
    assert_eq!(weird[0].name, "Weird \"Name\"");
    assert_eq!(
        s.intro
            .search_objects(id, PG_DB, "e", 2)
            .await
            .unwrap()
            .len(),
        2,
        "limit honoured"
    );

    // fully cached: answered without a round trip, same results
    for sc in s.intro.schemas(id, PG_DB).await.unwrap().iter() {
        if !sc.is_system {
            s.intro.schema(id, PG_DB, &sc.name).await.unwrap();
            s.intro.functions(id, PG_DB, &sc.name).await.unwrap();
        }
    }
    let before = s.intro.round_trips();
    let cached_hits = s
        .intro
        .search_objects(id, PG_DB, "CUSTOM", 10)
        .await
        .unwrap();
    assert_eq!(cached_hits, hits);
    let cached_add = s.intro.search_objects(id, PG_DB, "add", 10).await.unwrap();
    assert_eq!(cached_add, add);
    assert!(s
        .intro
        .search_objects(id, PG_DB, "%", 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(s.intro.round_trips(), before);
}

#[tokio::test]
async fn quoting_matches_the_server_and_keywords_are_covered() {
    for major in [13, 18] {
        let Some(pg) = harness::pg(major).await else {
            return;
        };
        let c = admin(&pg, PG_DB).await;
        let words = c
            .query(
                "select word from pg_get_keywords() where catcode <> 'U'",
                &[],
            )
            .await
            .unwrap();
        for w in &words {
            let w: String = w.get(0);
            assert!(
                is_reserved_keyword(&w),
                "PG{major} keyword {w} not in the list"
            );
        }
        let names = [
            "users",
            "Users",
            "select",
            "user",
            "year",
            "with space",
            "Weird \"Name\"",
            "x\"; drop table y; --",
            "100%",
            "a*b",
            "ünïcödé",
            "1abc",
            "a$b",
            "_ok",
            "",
        ];
        for n in names {
            let server: String = c
                .query_one("select quote_ident($1)", &[&n])
                .await
                .unwrap()
                .get(0);
            assert_eq!(quote_ident(n), server, "PG{major} quote_ident({n:?})");
        }
    }
}

#[test]
fn expansion_state_round_trips_through_the_config_dir() {
    let dir = tempfile::tempdir().unwrap();
    let e: ConnectionEntry = serde_json::from_value(
        serde_json::json!({"name": "x", "host": "localhost", "database": "app", "user": "me"}),
    )
    .unwrap();
    let store = ExpansionStore::new(ConfigStore::new(dir.path()));
    assert_eq!(
        store.get_or_initial(&e).unwrap(),
        vec![
            vec!["db:app".to_string()],
            vec!["db:app".to_string(), "schema:public".to_string()]
        ]
    );
    let paths = vec![
        vec!["db:app".to_string()],
        vec![
            "db:app".to_string(),
            "schema:public".to_string(),
            "table:users".to_string(),
        ],
        vec!["db:other".to_string(), "schema:we:ird".to_string()],
    ];
    store.set(&e.id, paths.clone()).unwrap();
    let other = ConnectionId::new();
    store.set(&other, vec![vec!["db:x".to_string()]]).unwrap();

    // a fresh store (app restart) reads the same state back from tree_state.json
    let reopened = ExpansionStore::new(ConfigStore::new(dir.path()));
    assert_eq!(reopened.get_or_initial(&e).unwrap(), paths);
    assert_eq!(
        reopened.get(&other).unwrap(),
        Some(vec![vec!["db:x".to_string()]])
    );
    assert!(dir.path().join(TREE_STATE_FILE).exists());
}

#[tokio::test]
async fn script_as_templates_quote_identifiers_and_target_keys() {
    use pgwhy_core::introspect::{script_as, ScriptKind};
    let Some(pg) = harness::pg(16).await else {
        return;
    };
    let (client, conn) = tokio_postgres::connect(&pg.conninfo(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
        .batch_execute(
            r#"create schema "Odd Schema";
               create table "Odd Schema"."order" (
                 "id" int generated always as identity,
                 "line no" int not null,
                 "select" text default 'x',
                 primary key (id, "line no"));
               create table nokey (a int, b text);
               create view v_ro as select 1 as one;"#,
        )
        .await
        .unwrap();
    let entry = script_entry(&pg);
    let m = script_manager(&entry);
    let intro = pgwhy_core::introspect::Introspector::new(m);
    let d = intro
        .relation_details(&entry.id, harness::PG_DB, "Odd Schema", "order")
        .await
        .unwrap()
        .unwrap();

    let select = script_as(&d, ScriptKind::Select).unwrap();
    assert_eq!(select, "SELECT\n    id,\n    \"line no\",\n    \"select\"\nFROM \"Odd Schema\".\"order\"\nLIMIT 1000;\n");
    let insert = script_as(&d, ScriptKind::Insert).unwrap();
    assert!(
        !insert.contains("    id,"),
        "identity ALWAYS column is not writable: {insert}"
    );
    assert!(insert.contains("\"select\""), "{insert}");
    assert!(insert.contains("DEFAULT /* select text */"), "{insert}");
    let update = script_as(&d, ScriptKind::Update).unwrap();
    assert!(
        update.contains("WHERE id = NULL /* integer */\n    AND \"line no\" = NULL /* integer */;"),
        "{update}"
    );
    assert!(
        !update.contains("SET\n    id ="),
        "key columns are not SET: {update}"
    );

    let nk = intro
        .relation_details(&entry.id, harness::PG_DB, "public", "nokey")
        .await
        .unwrap()
        .unwrap();
    assert!(script_as(&nk, ScriptKind::Select).is_ok());
    assert!(script_as(&nk, ScriptKind::Update)
        .unwrap_err()
        .message
        .contains("no primary key"));

    let v = intro
        .relation_details(&entry.id, harness::PG_DB, "public", "v_ro")
        .await
        .unwrap()
        .unwrap();
    assert!(script_as(&v, ScriptKind::Insert)
        .unwrap_err()
        .message
        .contains("isn't insertable"));
}

fn script_entry(pg: &harness::PgFixture) -> pgwhy_core::config::ConnectionEntry {
    serde_json::from_value(serde_json::json!({
        "name": "t", "host": pg.host, "port": pg.port, "database": harness::PG_DB,
        "user": harness::PG_USER, "sslmode": "disable", "env_tag": "local"
    }))
    .unwrap()
}

fn script_manager(
    entry: &pgwhy_core::config::ConnectionEntry,
) -> pgwhy_core::conn::ConnectionManager {
    use pgwhy_core::secret::{MemoryStore, Secret, SecretKey, SecretKind, Vault};
    let vault = std::sync::Arc::new(Vault::new(std::sync::Arc::new(MemoryStore::default())));
    vault
        .store(
            &SecretKey::new(&entry.id, SecretKind::DbPassword),
            Secret::new(harness::PG_PASSWORD),
            true,
        )
        .unwrap();
    let m = pgwhy_core::conn::ConnectionManager::new(
        vault,
        std::sync::Arc::new(pgwhy_core::conn::NoTunnels),
        Default::default(),
    );
    m.set_entries([entry.clone()]);
    m
}
