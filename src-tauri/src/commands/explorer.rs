//! Object explorer commands (R3, R4, AE1) over the introspection cache (U8). DTOs are shaped
//! for the tree (ui/src/lib/ipc/explorer.ts): flat, display-ready, names unquoted.

use crate::state::AppState;
use pgwhy_core::config::ConnectionId;
use pgwhy_core::introspect::{
    ColumnInfo, ConstraintInfo, FunctionInfo, IndexInfo, ObjectKind, RelationDetails, RelationInfo,
    RelationKind, RelationStats,
};
use pgwhy_core::AppResult;
use serde::Serialize;
use tauri::State;

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseNode {
    pub name: String,
    pub is_template: bool,
    pub allow_connections: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaNode {
    pub name: String,
    pub system: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnNode {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub in_primary_key: bool,
    pub identity: Option<String>,
    pub generated: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelationNode {
    pub schema: String,
    pub name: String,
    pub kind: &'static str,
    pub columns: Vec<ColumnNode>,
    pub partition_of: Option<String>,
    pub row_estimate: Option<i64>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutineNode {
    pub schema: String,
    pub name: String,
    pub kind: &'static str,
    pub identity_args: String,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaContents {
    pub relations: Vec<RelationNode>,
    pub routines: Vec<RoutineNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub schema: String,
    pub name: String,
    pub kind: &'static str,
    pub identity_args: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetailsDto {
    pub relation: RelationNode,
    pub constraints: Vec<ConstraintDto>,
    pub indexes: Vec<IndexDto>,
    pub stats: StatsDto,
    pub edit_key: Vec<String>,
    pub read_only_reason: Option<String>,
    pub view_definition: Option<String>,
    /// full model for anything the panel shows beyond the DTO (sequence, updatability, FKs)
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConstraintDto {
    pub name: String,
    pub kind: pgwhy_core::introspect::ConstraintKind,
    pub columns: Vec<String>,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexDto {
    pub name: String,
    pub definition: String,
    pub unique: bool,
    pub primary: bool,
    pub valid: bool,
    pub size_bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsDto {
    pub row_estimate: Option<i64>,
    pub live_rows: Option<i64>,
    pub dead_rows: Option<i64>,
    pub total_bytes: Option<i64>,
    pub table_bytes: Option<i64>,
    pub indexes_bytes: Option<i64>,
    pub last_analyze: Option<String>,
    pub last_autoanalyze: Option<String>,
    pub last_vacuum: Option<String>,
    pub last_autovacuum: Option<String>,
}

pub(crate) fn rel_kind(k: RelationKind) -> &'static str {
    match k {
        RelationKind::Table => "table",
        RelationKind::PartitionedTable => "partitioned_table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "matview",
        RelationKind::ForeignTable => "foreign_table",
        RelationKind::Sequence => "sequence",
    }
}

fn object_kind(k: ObjectKind) -> &'static str {
    match k {
        ObjectKind::Table => "table",
        ObjectKind::PartitionedTable => "partitioned_table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "matview",
        ObjectKind::ForeignTable => "foreign_table",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Function => "function",
        ObjectKind::Procedure => "procedure",
        ObjectKind::Aggregate => "aggregate",
        ObjectKind::Window => "window",
    }
}

fn column(c: &ColumnInfo, pk: &[String]) -> ColumnNode {
    ColumnNode {
        name: c.name.clone(),
        data_type: c.data_type.clone(),
        not_null: c.not_null,
        default: c.default.clone(),
        in_primary_key: pk.contains(&c.name),
        identity: c.identity.map(|i| format!("{i:?}").to_lowercase()),
        generated: c.generated.map(|g| format!("{g:?}").to_lowercase()),
    }
}

pub(crate) fn relation(r: &RelationInfo, pk: &[String], row_estimate: Option<i64>) -> RelationNode {
    RelationNode {
        schema: r.schema.clone(),
        name: r.name.clone(),
        kind: rel_kind(r.kind),
        columns: r.columns.iter().map(|c| column(c, pk)).collect(),
        partition_of: r
            .partition_of
            .as_ref()
            .map(|p| format!("{}.{}", p.schema, p.name)),
        row_estimate,
        comment: r.comment.clone(),
    }
}

fn routine(f: &FunctionInfo) -> RoutineNode {
    RoutineNode {
        schema: f.schema.clone(),
        name: f.name.clone(),
        kind: object_kind(f.kind.into()),
        identity_args: f.identity_args.clone(),
        result: f.result.clone(),
    }
}

fn constraint(c: &ConstraintInfo) -> ConstraintDto {
    ConstraintDto {
        name: c.name.clone(),
        kind: c.kind,
        columns: c.columns.clone(),
        definition: c.definition.clone(),
    }
}

fn index(i: &IndexInfo) -> IndexDto {
    IndexDto {
        name: i.name.clone(),
        definition: i.definition.clone(),
        unique: i.is_unique,
        primary: i.is_primary,
        valid: i.is_valid,
        size_bytes: i.size_bytes,
    }
}

fn stats(s: &RelationStats) -> StatsDto {
    StatsDto {
        row_estimate: s.estimated_rows,
        live_rows: s.live_tuples,
        dead_rows: s.dead_tuples,
        total_bytes: s.total_bytes,
        table_bytes: s.table_bytes,
        indexes_bytes: s.indexes_bytes,
        last_analyze: s.last_analyze.clone(),
        last_autoanalyze: s.last_autoanalyze.clone(),
        last_vacuum: s.last_vacuum.clone(),
        last_autovacuum: s.last_autovacuum.clone(),
    }
}

pub(crate) fn details(d: &RelationDetails) -> DetailsDto {
    let pk = d
        .primary_key()
        .map(|c| c.columns.clone())
        .unwrap_or_default();
    DetailsDto {
        relation: relation(&d.relation, &pk, d.stats.estimated_rows),
        constraints: d.constraints.iter().map(constraint).collect(),
        indexes: d.indexes.iter().map(index).collect(),
        stats: stats(&d.stats),
        edit_key: d
            .provenance
            .key
            .as_ref()
            .map(|k| k.columns().to_vec())
            .unwrap_or_default(),
        read_only_reason: d.provenance.read_only_reason.clone(),
        view_definition: d.view_definition.clone(),
        raw: serde_json::to_value(d).unwrap_or_default(),
    }
}

#[tauri::command]
pub async fn explorer_databases(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
) -> AppResult<Vec<DatabaseNode>> {
    let dbs = state.introspector.databases(&connection_id).await?;
    Ok(dbs
        .iter()
        .map(|d| DatabaseNode {
            name: d.name.clone(),
            is_template: d.is_template,
            allow_connections: d.allow_connections,
            comment: d.comment.clone(),
        })
        .collect())
}

#[tauri::command]
pub async fn explorer_schemas(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
) -> AppResult<Vec<SchemaNode>> {
    let schemas = state
        .introspector
        .schemas(&connection_id, &database)
        .await?;
    Ok(schemas
        .iter()
        .map(|s| SchemaNode {
            name: s.name.clone(),
            system: s.is_system,
        })
        .collect())
}

#[tauri::command]
pub async fn explorer_schema(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
) -> AppResult<SchemaContents> {
    let (contents, functions) = tokio::join!(
        state
            .introspector
            .schema(&connection_id, &database, &schema),
        state
            .introspector
            .functions(&connection_id, &database, &schema),
    );
    let contents = contents?;
    let functions = functions?;
    Ok(SchemaContents {
        relations: contents
            .relations
            .iter()
            .map(|r| relation(r, &[], None))
            .collect(),
        routines: functions.iter().map(routine).collect(),
    })
}

#[tauri::command]
pub async fn explorer_search(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
    pattern: String,
) -> AppResult<Vec<SearchHit>> {
    let hits = state
        .introspector
        .search_objects(&connection_id, &database, &pattern, 50)
        .await?;
    Ok(hits
        .into_iter()
        .map(|h| SearchHit {
            schema: h.schema,
            name: h.name,
            kind: object_kind(h.kind),
            identity_args: h.identity_args,
        })
        .collect())
}

/// Serve-cached details; `refresh` re-reads (and refreshes the schema's cached relation).
/// `None` = the object no longer exists.
#[tauri::command]
pub async fn explorer_details(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
    name: String,
    refresh: bool,
) -> AppResult<Option<DetailsDto>> {
    let d = if refresh {
        state
            .introspector
            .refresh_relation(&connection_id, &database, &schema, &name)
            .await?
    } else {
        state
            .introspector
            .relation_details(&connection_id, &database, &schema, &name)
            .await?
    };
    Ok(d.map(|d| details(&d)))
}

#[tauri::command]
pub async fn explorer_refresh(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: Option<String>,
    schema: Option<String>,
) -> AppResult<()> {
    match (database, schema) {
        (Some(db), Some(schema)) => {
            state
                .introspector
                .refresh_schema(&connection_id, &db, &schema)
                .await?;
        }
        (Some(db), None) => state.introspector.invalidate_db(&connection_id, &db),
        _ => state.introspector.invalidate_server(&connection_id),
    }
    Ok(())
}

const SEP: char = '\u{1f}';

#[tauri::command]
pub fn tree_state_get(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
) -> AppResult<Vec<String>> {
    Ok(state
        .expansion
        .get(&connection_id)?
        .unwrap_or_default()
        .into_iter()
        .map(|segs| segs.join(&SEP.to_string()))
        .collect())
}

#[tauri::command]
pub fn tree_state_set(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    expanded: Vec<String>,
) -> AppResult<()> {
    let paths = expanded
        .into_iter()
        .map(|p| p.split(SEP).map(str::to_string).collect())
        .collect();
    state.expansion.set(&connection_id, paths)
}

/// Script-as (R5): DML templates from the cache; CREATE via a version-matched `pg_dump -s`.
#[tauri::command]
pub async fn explorer_script_as(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
    name: String,
    kind: pgwhy_core::introspect::ScriptKind,
) -> AppResult<String> {
    use pgwhy_core::introspect::ScriptKind;
    let details = state
        .introspector
        .relation_details(&connection_id, &database, &schema, &name)
        .await?
        .ok_or_else(|| {
            pgwhy_core::AppError::new(
                pgwhy_core::ErrorKind::Config,
                "that object no longer exists",
            )
        })?;
    if kind != ScriptKind::Create {
        return pgwhy_core::introspect::script_as(&details, kind);
    }
    let entry = state.manager.entry(&connection_id)?;
    let conn = state
        .introspector
        .manager()
        .introspection(&connection_id, &database)
        .await?;
    let server_major = conn.info.server_major();
    let registry = state.tool_registry(false).await;
    let pg_dump = pgwhy_pgtools::require_pg_dump(&registry, server_major)?;
    let (endpoint, password, _lease) = state.tool_endpoint(&entry, &database).await?;
    let ctx = state.run_context()?;
    pgwhy_pgtools::ops::dump_schema_ddl(
        &pg_dump,
        &endpoint,
        password.as_ref(),
        &schema,
        &name,
        &pgwhy_pgtools::ops::DdlOptions::default(),
        &ctx,
    )
    .await
}

/// Which Script-as items apply to an object, and why the others don't.
#[tauri::command]
pub async fn explorer_script_options(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
    schema: String,
    name: String,
) -> AppResult<Vec<(pgwhy_core::introspect::ScriptKind, Option<String>)>> {
    use pgwhy_core::introspect::{unavailable_reason, ScriptKind};
    let Some(d) = state
        .introspector
        .relation_details(&connection_id, &database, &schema, &name)
        .await?
    else {
        return Ok(Vec::new());
    };
    Ok([
        ScriptKind::Select,
        ScriptKind::Insert,
        ScriptKind::Update,
        ScriptKind::Delete,
        ScriptKind::Create,
    ]
    .into_iter()
    .map(|k| {
        (
            k,
            if k == ScriptKind::Create {
                None
            } else {
                unavailable_reason(&d, k)
            },
        )
    })
    .collect())
}

/// Completion namespace for the editor (R7): `{ schema: { relation: [columns] } }` from the
/// already-loaded catalog only — never a round trip on keystrokes.
#[tauri::command]
pub fn completion_schema(
    state: State<'_, AppState>,
    connection_id: ConnectionId,
    database: String,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, Vec<String>>> {
    state
        .introspector
        .cached_contents(&connection_id, &database)
        .iter()
        .map(|sc| {
            let rels = sc
                .relations
                .iter()
                .map(|r| {
                    (
                        r.name.clone(),
                        r.columns.iter().map(|c| c.name.clone()).collect(),
                    )
                })
                .collect();
            (sc.schema.clone(), rels)
        })
        .collect()
}
