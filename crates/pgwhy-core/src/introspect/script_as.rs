//! Script-as (R5): SELECT / INSERT / UPDATE / DELETE templates generated from the cached
//! model. CREATE DDL is not generated here — it comes from `pg_dump -s -t` (version-matched,
//! correct for every object feature) via pgwhy-pgtools.
//!
//! Every identifier goes through [`quote_ident`]/[`qualified`]; the templates contain no
//! values, only `NULL` placeholders annotated with the column type for the user to fill in.

use super::model::{RelationDetails, RelationKind};
use super::quote::{qualified, quote_ident};
use crate::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    Select,
    Insert,
    Update,
    Delete,
    Create,
}

/// Why a Script-as item is unavailable for this object, or `None` when it applies.
pub fn unavailable_reason(details: &RelationDetails, kind: ScriptKind) -> Option<String> {
    let rel = &details.relation;
    let dml = matches!(
        kind,
        ScriptKind::Insert | ScriptKind::Update | ScriptKind::Delete
    );
    if rel.kind == RelationKind::Sequence
        && kind != ScriptKind::Create
        && kind != ScriptKind::Select
    {
        return Some("sequences have no rows to change".into());
    }
    if rel.kind == RelationKind::MaterializedView && dml {
        return Some("materialized views can't be changed directly — REFRESH it instead".into());
    }
    if let (true, Some(u)) = (dml, &details.updatability) {
        let ok = match kind {
            ScriptKind::Insert => u.insertable,
            ScriptKind::Update => u.updatable,
            _ => u.deletable,
        };
        if !ok {
            return Some(format!(
                "this {} isn't {}",
                kind_noun(rel.kind),
                match kind {
                    ScriptKind::Insert => "insertable",
                    ScriptKind::Update => "updatable",
                    _ => "deletable",
                }
            ));
        }
    }
    if matches!(kind, ScriptKind::Update | ScriptKind::Delete)
        && key_columns(details).is_empty()
        && details.updatability.is_none()
    {
        return Some("no primary key or unique NOT NULL key to target a single row".into());
    }
    None
}

fn kind_noun(k: RelationKind) -> &'static str {
    match k {
        RelationKind::View => "view",
        RelationKind::ForeignTable => "foreign table",
        _ => "relation",
    }
}

fn key_columns(details: &RelationDetails) -> Vec<String> {
    details
        .provenance
        .key
        .as_ref()
        .map(|k| k.columns().to_vec())
        .unwrap_or_default()
}

const INDENT: &str = "    ";

/// Generate a DML template. `Create` is refused here (use pg_dump).
pub fn script_as(details: &RelationDetails, kind: ScriptKind) -> AppResult<String> {
    if kind == ScriptKind::Create {
        return Err(AppError::internal(
            "CREATE scripts come from pg_dump, not the template generator",
        ));
    }
    if let Some(reason) = unavailable_reason(details, kind) {
        return Err(AppError::new(ErrorKind::Refused, reason));
    }
    let rel = &details.relation;
    let table = qualified(&rel.schema, &rel.name);
    let type_of = |name: &str| {
        rel.column(name)
            .map(|c| c.data_type.as_str())
            .unwrap_or("?")
    };
    let placeholder = |name: &str| format!("NULL /* {} */", type_of(name).replace("*/", "* /"));
    let keys = key_columns(details);
    let where_keys = || {
        if keys.is_empty() {
            "WHERE <condition>".to_string()
        } else {
            let conds: Vec<String> = keys
                .iter()
                .map(|k| format!("{} = {}", quote_ident(k), placeholder(k)))
                .collect();
            format!("WHERE {}", conds.join(&format!("\n{INDENT}AND ")))
        }
    };
    let sql = match kind {
        ScriptKind::Select => {
            let cols: Vec<String> = rel
                .columns
                .iter()
                .map(|c| format!("{INDENT}{}", quote_ident(&c.name)))
                .collect();
            let cols = if cols.is_empty() {
                format!("{INDENT}*")
            } else {
                cols.join(",\n")
            };
            format!("SELECT\n{cols}\nFROM {table}\nLIMIT 1000;\n")
        }
        ScriptKind::Insert => {
            let writable: Vec<_> = rel.columns.iter().filter(|c| c.is_writable()).collect();
            let names: Vec<String> = writable
                .iter()
                .map(|c| format!("{INDENT}{}", quote_ident(&c.name)))
                .collect();
            let values: Vec<String> = writable
                .iter()
                .map(|c| {
                    let default = if c.default.is_some() || c.identity.is_some() {
                        "DEFAULT"
                    } else {
                        "NULL"
                    };
                    format!(
                        "{INDENT}{default} /* {} {} */",
                        c.name.replace("*/", "* /"),
                        c.data_type.replace("*/", "* /")
                    )
                })
                .collect();
            format!(
                "INSERT INTO {table} (\n{}\n) VALUES (\n{}\n);\n",
                names.join(",\n"),
                values.join(",\n")
            )
        }
        ScriptKind::Update => {
            let sets: Vec<String> = rel
                .columns
                .iter()
                .filter(|c| c.is_writable() && !keys.contains(&c.name))
                .map(|c| {
                    format!(
                        "{INDENT}{} = {}",
                        quote_ident(&c.name),
                        placeholder(&c.name)
                    )
                })
                .collect();
            format!(
                "UPDATE {table}\nSET\n{}\n{};\n",
                sets.join(",\n"),
                where_keys()
            )
        }
        ScriptKind::Delete => format!("DELETE FROM {table}\n{};\n", where_keys()),
        ScriptKind::Create => unreachable!(),
    };
    Ok(sql)
}
