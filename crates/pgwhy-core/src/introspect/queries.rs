//! The catalog queries. pg_catalog only (no `information_schema`: it is slow on big catalogs
//! and hides objects the role can't use), every query scoped by schema or relation name, and
//! stats views referenced by column name so shifting column sets across majors don't matter.
//!
//! Every query goes through [`Catalog::run`], which uses `query_typed` (Parse/Bind/Execute in
//! one flight — no separate prepare round trip) and counts round trips so tests can hold the
//! "one round trip per schema expansion" budget.

use super::model::*;
use crate::conn::{classify, PgConn};
use crate::AppResult;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::Row;

/// RFC 3339 UTC rendering of a timestamptz, done server-side so no date crate is needed.
fn ts(column: &str) -> String {
    format!("to_char({column} at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
}

/// Schemas the tree never shows: toast and per-session temp schemas.
const HIDDEN_SCHEMAS: &str = "n.nspname !~ '^pg_toast' and n.nspname !~ '^pg_temp_'";

/// Query runner over one introspection connection, counting round trips.
pub(crate) struct Catalog<'a> {
    pub conn: &'a PgConn,
    pub counter: &'a AtomicU64,
}

impl Catalog<'_> {
    async fn run(
        &self,
        sql: &str,
        params: &[(&(dyn ToSql + Sync), Type)],
        context: &str,
    ) -> AppResult<Vec<Row>> {
        self.counter.fetch_add(1, Ordering::Relaxed);
        self.conn.touch();
        self.conn
            .client
            .query_typed(sql, params)
            .await
            .map_err(|e| classify(&e, context))
    }

    fn server_version(&self) -> i32 {
        self.conn.info.server_version_num
    }

    // ------------------------------------------------------------------ databases

    pub async fn databases(&self) -> AppResult<Vec<DatabaseInfo>> {
        let provider = if self.server_version() >= 150000 {
            "d.datlocprovider::text"
        } else {
            "null::text"
        };
        let sql = format!(
            "select d.datname::text, pg_get_userbyid(d.datdba)::text, \
                    pg_encoding_to_char(d.encoding)::text, d.datcollate::text, d.datctype::text, \
                    {provider}, d.datallowconn, d.datistemplate, \
                    shobj_description(d.oid, 'pg_database') \
             from pg_database d order by d.datname"
        );
        let rows = self.run(&sql, &[], "list databases").await?;
        Ok(rows
            .iter()
            .map(|r| DatabaseInfo {
                name: r.get(0),
                owner: r.get(1),
                encoding: r.get(2),
                collate: r.get(3),
                ctype: r.get(4),
                locale_provider: r.get(5),
                allow_connections: r.get(6),
                is_template: r.get(7),
                comment: r.get(8),
            })
            .collect())
    }

    /// `pg_database_size` walks the database directory, so it is on demand only.
    pub async fn database_size(&self, database: &str) -> AppResult<Option<i64>> {
        let rows = self
            .run(
                "select case when has_database_privilege(d.oid, 'CONNECT') \
                        then pg_database_size(d.oid) end \
                 from pg_database d where d.datname = $1",
                &[(&database, Type::TEXT)],
                "read the database size",
            )
            .await?;
        Ok(rows.first().and_then(|r| r.get(0)))
    }

    // ------------------------------------------------------------------ schemas

    pub async fn schemas(&self) -> AppResult<Vec<SchemaInfo>> {
        let sql = format!(
            "select n.nspname::text, pg_get_userbyid(n.nspowner)::text, \
                    n.nspname in ('pg_catalog', 'information_schema'), \
                    obj_description(n.oid, 'pg_namespace') \
             from pg_namespace n where {HIDDEN_SCHEMAS} \
             order by n.nspname in ('pg_catalog', 'information_schema'), n.nspname"
        );
        let rows = self.run(&sql, &[], "list schemas").await?;
        Ok(rows
            .iter()
            .map(|r| SchemaInfo {
                name: r.get(0),
                owner: r.get(1),
                is_system: r.get(2),
                comment: r.get(3),
            })
            .collect())
    }

    // ------------------------------------------------------------------ relations + columns

    /// All relations of `schema` (or just `only`) with their columns, in one round trip.
    pub async fn relations(
        &self,
        schema: &str,
        only: Option<&str>,
    ) -> AppResult<Vec<RelationInfo>> {
        const SQL: &str = "\
select c.oid, c.relname::text, c.relkind::text, pg_get_userbyid(c.relowner)::text,
       obj_description(c.oid, 'pg_class'),
       par.schema, par.name,
       case when c.relispartition then pg_get_expr(c.relpartbound, c.oid) end,
       case when c.relkind = 'p' then pg_get_partkeydef(c.oid) end,
       coalesce(ch.schemas, '{}'), coalesce(ch.names, '{}'),
       coalesce(col.names, '{}'), coalesce(col.nums, '{}'), coalesce(col.types, '{}'),
       coalesce(col.typoids, '{}'), coalesce(col.notnull, '{}'), coalesce(col.defaults, '{}'),
       coalesce(col.identity, '{}'), coalesce(col.generated, '{}'), coalesce(col.comments, '{}')
from pg_class c
join pg_namespace n on n.oid = c.relnamespace
left join lateral (
    select pn.nspname::text as schema, pc.relname::text as name
    from pg_inherits i
    join pg_class pc on pc.oid = i.inhparent
    join pg_namespace pn on pn.oid = pc.relnamespace
    where c.relispartition and i.inhrelid = c.oid
    limit 1
) par on true
left join lateral (
    select array_agg(cn.nspname::text order by cn.nspname, cc.relname) as schemas,
           array_agg(cc.relname::text order by cn.nspname, cc.relname) as names
    from pg_inherits i
    join pg_class cc on cc.oid = i.inhrelid
    join pg_namespace cn on cn.oid = cc.relnamespace
    where c.relkind = 'p' and i.inhparent = c.oid
) ch on true
left join lateral (
    select array_agg(a.attname::text order by a.attnum) as names,
           array_agg(a.attnum order by a.attnum) as nums,
           array_agg(format_type(a.atttypid, a.atttypmod) order by a.attnum) as types,
           array_agg(a.atttypid order by a.attnum) as typoids,
           array_agg(a.attnotnull order by a.attnum) as notnull,
           array_agg(pg_get_expr(d.adbin, d.adrelid) order by a.attnum) as defaults,
           array_agg(a.attidentity::text order by a.attnum) as identity,
           array_agg(a.attgenerated::text order by a.attnum) as generated,
           array_agg(col_description(a.attrelid, a.attnum) order by a.attnum) as comments
    from pg_attribute a
    left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
    where c.relkind <> 'S' and a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
) col on true
where n.nspname = $1
  and ($2::text is null or c.relname = $2)
  and c.relkind in ('r', 'p', 'v', 'm', 'f', 'S')
order by c.relname";
        let rows = self
            .run(
                SQL,
                &[(&schema, Type::TEXT), (&only, Type::TEXT)],
                "load the schema's tables and columns",
            )
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let Some(kind) = RelationKind::from_relkind(r.get::<_, &str>(2)) else {
                continue;
            };
            let par_schema: Option<String> = r.get(5);
            let par_name: Option<String> = r.get(6);
            let ch_schemas: Vec<String> = r.get(9);
            let ch_names: Vec<String> = r.get(10);
            let names: Vec<String> = r.get(11);
            let nums: Vec<i16> = r.get(12);
            let types: Vec<String> = r.get(13);
            let typoids: Vec<u32> = r.get(14);
            let notnull: Vec<bool> = r.get(15);
            let defaults: Vec<Option<String>> = r.get(16);
            let identity: Vec<String> = r.get(17);
            let generated: Vec<String> = r.get(18);
            let comments: Vec<Option<String>> = r.get(19);
            let columns = names
                .into_iter()
                .enumerate()
                .map(|(i, name)| {
                    let mut default = defaults.get(i).cloned().flatten();
                    let generated = match generated.get(i).map(String::as_str) {
                        Some("s") => Some(Generated::Stored),
                        Some("v") => Some(Generated::Virtual),
                        _ => None,
                    };
                    let generation_expr = if generated.is_some() {
                        default.take()
                    } else {
                        None
                    };
                    ColumnInfo {
                        name,
                        ordinal: nums.get(i).copied().unwrap_or_default(),
                        data_type: types.get(i).cloned().unwrap_or_default(),
                        type_oid: typoids.get(i).copied().unwrap_or_default(),
                        not_null: notnull.get(i).copied().unwrap_or_default(),
                        default,
                        identity: match identity.get(i).map(String::as_str) {
                            Some("a") => Some(Identity::Always),
                            Some("d") => Some(Identity::ByDefault),
                            _ => None,
                        },
                        generated,
                        generation_expr,
                        comment: comments.get(i).cloned().flatten(),
                    }
                })
                .collect();
            out.push(RelationInfo {
                oid: r.get(0),
                schema: schema.to_string(),
                name: r.get(1),
                kind,
                owner: r.get(3),
                comment: r.get(4),
                columns,
                partition_of: par_schema
                    .zip(par_name)
                    .map(|(s, n)| QualifiedName::new(s, n)),
                partition_bound: r.get(7),
                partition_key: r.get(8),
                partitions: ch_schemas
                    .into_iter()
                    .zip(ch_names)
                    .map(|(s, n)| QualifiedName::new(s, n))
                    .collect(),
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ functions

    pub async fn functions(&self, schema: &str) -> AppResult<Vec<FunctionInfo>> {
        const SQL: &str = "\
select p.oid, p.proname::text, p.prokind::text,
       pg_get_function_identity_arguments(p.oid), pg_get_function_arguments(p.oid),
       case when p.prokind <> 'p' then pg_get_function_result(p.oid) end,
       l.lanname::text, p.provolatile::text, p.prosecdef, p.proretset,
       pg_get_userbyid(p.proowner)::text, obj_description(p.oid, 'pg_proc')
from pg_proc p
join pg_namespace n on n.oid = p.pronamespace
join pg_language l on l.oid = p.prolang
where n.nspname = $1
order by p.proname, 4";
        let rows = self
            .run(SQL, &[(&schema, Type::TEXT)], "load the schema's functions")
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(FunctionInfo {
                    oid: r.get(0),
                    schema: schema.to_string(),
                    name: r.get(1),
                    kind: FunctionKind::from_prokind(r.get::<_, &str>(2))?,
                    identity_args: r.get(3),
                    arguments: r.get(4),
                    result: r.get(5),
                    language: r.get(6),
                    volatility: match r.get::<_, &str>(7) {
                        "i" => Volatility::Immutable,
                        "s" => Volatility::Stable,
                        _ => Volatility::Volatile,
                    },
                    security_definer: r.get(8),
                    returns_set: r.get(9),
                    owner: r.get(10),
                    comment: r.get(11),
                })
            })
            .collect())
    }

    // ------------------------------------------------------------------ per-relation details

    pub async fn constraints(&self, schema: &str, name: &str) -> AppResult<Vec<ConstraintInfo>> {
        const SQL: &str = "\
select co.conname::text, co.contype::text,
       array(select a.attname::text
             from unnest(co.conkey) with ordinality k(attnum, ord)
             join pg_attribute a on a.attrelid = co.conrelid and a.attnum = k.attnum
             order by k.ord),
       pg_get_constraintdef(co.oid, true),
       fn.nspname::text, fc.relname::text,
       array(select a.attname::text
             from unnest(co.confkey) with ordinality k(attnum, ord)
             join pg_attribute a on a.attrelid = co.confrelid and a.attnum = k.attnum
             order by k.ord),
       co.confupdtype::text, co.confdeltype::text,
       co.condeferrable, co.condeferred, co.convalidated,
       (select ic.relname::text from pg_class ic where ic.oid = co.conindid)
from pg_constraint co
join pg_class c on c.oid = co.conrelid
join pg_namespace n on n.oid = c.relnamespace
left join pg_class fc on fc.oid = co.confrelid
left join pg_namespace fn on fn.oid = fc.relnamespace
where n.nspname = $1 and c.relname = $2 and co.contype in ('p', 'f', 'u', 'c', 'x')
order by array_position(array['p', 'f', 'u', 'c', 'x'], co.contype::text), co.conname";
        let rows = self
            .run(
                SQL,
                &[(&schema, Type::TEXT), (&name, Type::TEXT)],
                "load constraints",
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let kind = ConstraintKind::from_contype(r.get::<_, &str>(1))?;
                let foreign_key = if kind == ConstraintKind::ForeignKey {
                    let fs: Option<String> = r.get(4);
                    let ft: Option<String> = r.get(5);
                    Some(ForeignKeyTarget {
                        table: QualifiedName::new(fs.unwrap_or_default(), ft.unwrap_or_default()),
                        columns: r.get(6),
                        on_update: FkAction::from_code(r.get::<_, &str>(7))
                            .unwrap_or(FkAction::NoAction),
                        on_delete: FkAction::from_code(r.get::<_, &str>(8))
                            .unwrap_or(FkAction::NoAction),
                    })
                } else {
                    None
                };
                let index: Option<String> = r.get(12);
                Some(ConstraintInfo {
                    name: r.get(0),
                    kind,
                    columns: r.get(2),
                    definition: r.get(3),
                    foreign_key,
                    deferrable: r.get(9),
                    initially_deferred: r.get(10),
                    validated: r.get(11),
                    // FKs record the *referenced* side's index in conindid
                    index: index.filter(|_| kind != ConstraintKind::ForeignKey),
                })
            })
            .collect())
    }

    pub async fn indexes(&self, schema: &str, name: &str) -> AppResult<Vec<IndexInfo>> {
        const SQL: &str = "\
select ic.relname::text, pg_get_indexdef(i.indexrelid), am.amname::text,
       i.indisunique, i.indisprimary, i.indisvalid,
       pg_get_expr(i.indpred, i.indrelid),
       array(select coalesce(a.attname::text, pg_get_indexdef(i.indexrelid, k.ord::int, true))
             from unnest(i.indkey::int2[]) with ordinality k(attnum, ord)
             left join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
                                     and k.attnum > 0
             where k.ord <= i.indnkeyatts
             order by k.ord),
       array(select coalesce(a.attname::text, pg_get_indexdef(i.indexrelid, k.ord::int, true))
             from unnest(i.indkey::int2[]) with ordinality k(attnum, ord)
             left join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
                                     and k.attnum > 0
             where k.ord > i.indnkeyatts
             order by k.ord),
       i.indexprs is not null
         or exists (select 1 from unnest(i.indkey::int2[]) with ordinality k(attnum, ord)
                    where k.ord <= i.indnkeyatts and k.attnum = 0),
       coalesce((select bool_and(a.attnotnull)
                 from unnest(i.indkey::int2[]) with ordinality k(attnum, ord)
                 join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
                 where k.ord <= i.indnkeyatts and k.attnum > 0), false),
       (select co.conname::text from pg_constraint co
        where co.conindid = i.indexrelid and co.conrelid = i.indrelid
          and co.contype in ('p', 'u', 'x')
        limit 1),
       pg_relation_size(i.indexrelid)
from pg_index i
join pg_class ic on ic.oid = i.indexrelid
join pg_am am on am.oid = ic.relam
join pg_class c on c.oid = i.indrelid
join pg_namespace n on n.oid = c.relnamespace
where n.nspname = $1 and c.relname = $2
order by i.indisprimary desc, ic.relname";
        let rows = self
            .run(
                SQL,
                &[(&schema, Type::TEXT), (&name, Type::TEXT)],
                "load indexes",
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| IndexInfo {
                name: r.get(0),
                definition: r.get(1),
                method: r.get(2),
                is_unique: r.get(3),
                is_primary: r.get(4),
                is_valid: r.get(5),
                predicate: r.get(6),
                columns: r.get(7),
                include_columns: r.get(8),
                has_expressions: r.get(9),
                key_columns_not_null: r.get(10),
                constraint: r.get(11),
                size_bytes: r.get(12),
            })
            .collect())
    }

    /// Stats, sizes, and kind-specific facts (view definition, updatability, sequence).
    pub async fn facts(&self, schema: &str, name: &str) -> AppResult<Option<RelationFacts>> {
        let sql = format!(
            "\
select c.relkind::text, c.reltuples::float8, s.n_live_tup, s.n_dead_tup,
       {}, {}, {}, {},
       case when c.relkind in ('r', 'm') then pg_total_relation_size(c.oid)
            when c.relkind = 'p' then (select coalesce(sum(pg_total_relation_size(t.relid)), 0)::int8
                                       from pg_partition_tree(c.oid) t where t.isleaf) end,
       case when c.relkind in ('r', 'm') then pg_table_size(c.oid)
            when c.relkind = 'p' then (select coalesce(sum(pg_table_size(t.relid)), 0)::int8
                                       from pg_partition_tree(c.oid) t where t.isleaf) end,
       case when c.relkind in ('r', 'm') then pg_indexes_size(c.oid)
            when c.relkind = 'p' then (select coalesce(sum(pg_indexes_size(t.relid)), 0)::int8
                                       from pg_partition_tree(c.oid) t where t.isleaf) end,
       case when c.relkind = 'p' then
            (select coalesce(sum(case when lc.reltuples > 0 then lc.reltuples::int8
                                      else coalesce(ls.n_live_tup, 0) end), 0)::int8
             from pg_partition_tree(c.oid) t
             join pg_class lc on lc.oid = t.relid
             left join pg_stat_all_tables ls on ls.relid = t.relid
             where t.isleaf) end,
       case when c.relkind in ('v', 'm') then pg_get_viewdef(c.oid, true) end,
       case when c.relkind in ('v', 'f') then pg_relation_is_updatable(c.oid, false) end,
       format_type(sq.seqtypid, null), sq.seqstart, sq.seqincrement, sq.seqmin, sq.seqmax,
       sq.seqcache, sq.seqcycle,
       case when c.relkind = 'S' and has_sequence_privilege(c.oid, 'SELECT,USAGE')
            then pg_sequence_last_value(c.oid) end,
       ob.schema, ob.tbl, ob.col
from pg_class c
join pg_namespace n on n.oid = c.relnamespace
left join pg_stat_all_tables s on s.relid = c.oid
left join pg_sequence sq on sq.seqrelid = c.oid
left join lateral (
    select tn.nspname::text as schema, tc.relname::text as tbl, a.attname::text as col
    from pg_depend d
    join pg_class tc on tc.oid = d.refobjid
    join pg_namespace tn on tn.oid = tc.relnamespace
    join pg_attribute a on a.attrelid = d.refobjid and a.attnum = d.refobjsubid
    where c.relkind = 'S' and d.classid = 'pg_class'::regclass and d.objid = c.oid
      and d.refclassid = 'pg_class'::regclass and d.deptype in ('a', 'i')
    limit 1
) ob on true
where n.nspname = $1 and c.relname = $2 and c.relkind in ('r', 'p', 'v', 'm', 'f', 'S')",
            ts("s.last_analyze"),
            ts("s.last_autoanalyze"),
            ts("s.last_vacuum"),
            ts("s.last_autovacuum"),
        );
        let rows = self
            .run(
                &sql,
                &[(&schema, Type::TEXT), (&name, Type::TEXT)],
                "load table statistics",
            )
            .await?;
        let Some(r) = rows.first() else {
            return Ok(None);
        };
        let Some(kind) = RelationKind::from_relkind(r.get::<_, &str>(0)) else {
            return Ok(None);
        };
        let reltuples: f64 = r.get(1);
        let live_tuples: Option<i64> = r.get(2);
        let partition_rows: Option<i64> = r.get(11);
        let (estimated_rows, estimate_source) =
            row_estimate(kind, reltuples, live_tuples, partition_rows);
        let stats = RelationStats {
            estimated_rows,
            estimate_source,
            reltuples,
            live_tuples,
            dead_tuples: r.get(3),
            last_analyze: r.get(4),
            last_autoanalyze: r.get(5),
            last_vacuum: r.get(6),
            last_autovacuum: r.get(7),
            total_bytes: r.get(8),
            table_bytes: r.get(9),
            indexes_bytes: r.get(10),
        };
        let updatability = r.get::<_, Option<i32>>(13).map(|bits| Updatability {
            // bit (1 << CmdType): UPDATE = 2, INSERT = 3, DELETE = 4
            updatable: bits & (1 << 2) != 0,
            insertable: bits & (1 << 3) != 0,
            deletable: bits & (1 << 4) != 0,
        });
        let sequence = match r.get::<_, Option<String>>(14) {
            Some(data_type) if kind == RelationKind::Sequence => {
                let ob_schema: Option<String> = r.get(22);
                let ob_table: Option<String> = r.get(23);
                let ob_col: Option<String> = r.get(24);
                Some(SequenceInfo {
                    data_type,
                    start: r.get(15),
                    increment: r.get(16),
                    min: r.get(17),
                    max: r.get(18),
                    cache: r.get(19),
                    cycle: r.get(20),
                    last_value: r.get(21),
                    owned_by: match (ob_schema, ob_table, ob_col) {
                        (Some(s), Some(t), Some(c)) => Some(OwnedBy {
                            table: QualifiedName::new(s, t),
                            column: c,
                        }),
                        _ => None,
                    },
                })
            }
            _ => None,
        };
        Ok(Some(RelationFacts {
            stats,
            view_definition: r.get(12),
            updatability,
            sequence,
        }))
    }

    // ------------------------------------------------------------------ search

    /// Name-only search over the whole database (relations + routines), excluding system
    /// schemas. Exact matches first, then prefix matches, then shorter names.
    pub async fn search(&self, pattern: &str, limit: i64) -> AppResult<Vec<SearchHit>> {
        const SQL: &str = "\
select schema, name, kind, args from (
    select n.nspname::text as schema, c.relname::text as name, c.relkind::text as kind,
           null::text as args
    from pg_class c join pg_namespace n on n.oid = c.relnamespace
    where c.relkind in ('r', 'p', 'v', 'm', 'f', 'S') and c.relname ilike $1
      and n.nspname not in ('pg_catalog', 'information_schema')
      and n.nspname !~ '^pg_toast' and n.nspname !~ '^pg_temp_'
    union all
    select n.nspname::text, p.proname::text, 'fn:' || p.prokind::text,
           pg_get_function_identity_arguments(p.oid)
    from pg_proc p join pg_namespace n on n.oid = p.pronamespace
    where p.proname ilike $1
      and n.nspname not in ('pg_catalog', 'information_schema')
      and n.nspname !~ '^pg_toast' and n.nspname !~ '^pg_temp_'
) s
order by lower(name) = lower($2) desc, name ilike $3 desc, length(name), name, schema
limit $4";
        let escaped = super::quote::escape_like(pattern);
        let contains = format!("%{escaped}%");
        let prefix = format!("{escaped}%");
        let rows = self
            .run(
                SQL,
                &[
                    (&contains, Type::TEXT),
                    (&pattern, Type::TEXT),
                    (&prefix, Type::TEXT),
                    (&limit, Type::INT8),
                ],
                "search the database",
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let kind: &str = r.get(2);
                let kind = match kind.strip_prefix("fn:") {
                    Some(k) => FunctionKind::from_prokind(k)?.into(),
                    None => RelationKind::from_relkind(kind)?.into(),
                };
                Some(SearchHit {
                    schema: r.get(0),
                    name: r.get(1),
                    kind,
                    identity_args: r.get(3),
                })
            })
            .collect())
    }
}

/// The non-column per-relation facts, assembled into [`RelationDetails`] by the cache.
pub(crate) struct RelationFacts {
    pub stats: RelationStats,
    pub view_definition: Option<String>,
    pub updatability: Option<Updatability>,
    pub sequence: Option<SequenceInfo>,
}

/// Approximate row count with its provenance. `reltuples` is −1 for never-analyzed tables on
/// PG14+ and 0 on PG13 (where 0 is ambiguous with "empty"), so anything ≤ 0 falls back to the
/// stats collector's live-tuple count.
pub(crate) fn row_estimate(
    kind: RelationKind,
    reltuples: f64,
    live_tuples: Option<i64>,
    partition_rows: Option<i64>,
) -> (Option<i64>, Option<RowEstimateSource>) {
    match kind {
        RelationKind::PartitionedTable => (
            partition_rows,
            partition_rows.map(|_| RowEstimateSource::Partitions),
        ),
        RelationKind::Table | RelationKind::MaterializedView => {
            if reltuples > 0.0 {
                (
                    Some(reltuples.round() as i64),
                    Some(RowEstimateSource::Reltuples),
                )
            } else {
                (
                    live_tuples,
                    live_tuples.map(|_| RowEstimateSource::LiveTuples),
                )
            }
        }
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_estimate_falls_back_when_never_analyzed() {
        use RowEstimateSource::*;
        let t = RelationKind::Table;
        assert_eq!(
            row_estimate(t, 1234.4, Some(1), None),
            (Some(1234), Some(Reltuples))
        );
        assert_eq!(
            row_estimate(t, -1.0, Some(7), None),
            (Some(7), Some(LiveTuples))
        );
        assert_eq!(
            row_estimate(t, 0.0, Some(7), None),
            (Some(7), Some(LiveTuples))
        );
        assert_eq!(row_estimate(t, -1.0, None, None), (None, None));
        assert_eq!(
            row_estimate(RelationKind::PartitionedTable, -1.0, None, Some(40)),
            (Some(40), Some(Partitions))
        );
        assert_eq!(
            row_estimate(RelationKind::View, 10.0, Some(1), None),
            (None, None)
        );
    }
}
