//! Top-N query builder: `SELECT … FROM <table> [WHERE criteria] [ORDER BY …]
//! LIMIT N`, with server-side filtering and sorting so they act on the whole table.
//!
//! Safety rules, shared with the edit SQL in [`super::edit`]:
//! - every identifier goes through [`quote_ident`] / [`qualified`] (catalog names are
//!   attacker-controlled on a hostile remote);
//! - every value is a bind parameter, typed `text` and cast server-side
//!   (`CAST($n::text AS <type>)`), never spliced into the statement;
//! - the cast's type name is the column's `format_type` text, accepted only if it matches a
//!   conservative type-name grammar ([`safe_type_name`]), so even a forged catalog cannot
//!   break out of the cast.
//!
//! **Every column is selected as text** ([`text_expr`]): bind parameters need the extended
//! protocol, whose results tokio-postgres always requests in binary — selecting text lets one
//! code path buffer any type, user-defined ones included, into the same [`ResultSet`] the
//! editor uses. The original type stays in the column metadata. The text is the type's output
//! function except where a dedicated cast differs: `boolean` (selected as `t`/`f`, matching
//! the editor), `inet` (`abbrev()`, which equals the output function) and `character(n)`
//! (`format('%s')`, keeping the padding).

use super::edit::{EditColumn, EditTarget};
use super::executor::ColumnMeta;
use super::results::{ResultSet, ResultState};
use crate::conn::classify as classify_pg;
use crate::introspect::{qualified, quote_ident, ColumnInfo, EditProvenance, RelationInfo};
use crate::{AppError, AppResult, ErrorKind};
use futures_util::StreamExt;
use std::time::Instant;
use tokio_postgres::types::ToSql;

/// Default N.
pub const DEFAULT_TOP_N: u64 = 100;

const BOOL_OID: u32 = 16;
const INET_OID: u32 = 869;
const BPCHAR_OID: u32 = 1042;

// ---------------------------------------------------------------------------------------------
// SQL building blocks (shared with edit.rs)
// ---------------------------------------------------------------------------------------------

/// Builds the executable statement and its display twin in lockstep: SQL text is appended to
/// both, values become `$n` in one and a quoted literal (display only) in the other.
#[derive(Debug, Default)]
pub(crate) struct SqlBuilder {
    pub sql: String,
    pub display: String,
    pub params: Vec<Option<String>>,
}

impl SqlBuilder {
    pub fn push(&mut self, s: &str) -> &mut Self {
        self.sql.push_str(s);
        self.display.push_str(s);
        self
    }

    /// A text parameter: `$n::text` / `'literal'`.
    pub fn param(&mut self, value: Option<&str>) -> &mut Self {
        self.params.push(value.map(str::to_string));
        self.sql.push_str(&format!("${}::text", self.params.len()));
        self.display.push_str(&display_literal(value));
        self
    }

    /// A value of type `ty`: `CAST($n::text AS ty)` / `'literal'::ty`.
    pub fn typed(&mut self, value: Option<&str>, ty: &str) -> &mut Self {
        self.params.push(value.map(str::to_string));
        self.sql
            .push_str(&format!("CAST(${}::text AS {ty})", self.params.len()));
        match value {
            Some(_) => {
                self.display.push_str(&display_literal(value));
                self.display.push_str("::");
                self.display.push_str(ty);
            }
            None => self.display.push_str("NULL"),
        }
        self
    }
}

pub(crate) fn params_refs(params: &[Option<String>]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect()
}

/// `'text'` with quotes doubled (an `E''` literal when it contains a backslash, so the
/// preview reads the same under any `standard_conforming_strings`), or `NULL`.
pub(crate) fn display_literal(value: Option<&str>) -> String {
    let Some(v) = value else {
        return "NULL".into();
    };
    let mut out = String::with_capacity(v.len() + 2);
    if v.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for c in v.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `type_name` when it is a plausible `format_type` result, else `None`.
///
/// Accepted: words (`[A-Za-z_][A-Za-z0-9_$]*`), double-quoted identifiers (with `""`
/// escapes), `.` between names, single spaces, a parenthesized typmod of words/digits/commas
/// (`numeric(10,2)`, `geometry(Point,4326)`), and trailing `[]`s. Anything else — quotes,
/// semicolons, comment markers, operators — is refused, so the text can never end the cast.
pub fn safe_type_name(type_name: &str) -> Option<&str> {
    let b = type_name.as_bytes();
    let mut p = 0;
    let mut words = 0;
    let word_start = |c: u8| c.is_ascii_alphabetic() || c == b'_';
    let word_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let mut arrays = false;
    while p < b.len() {
        let c = b[p];
        if arrays && c != b'[' {
            return None; // only more [] after the first []
        }
        if word_start(c) {
            while p < b.len() && word_char(b[p]) {
                p += 1;
            }
            words += 1;
        } else if c == b'"' {
            p += 1;
            let mut closed = false;
            while p < b.len() {
                if b[p] == b'"' {
                    if b.get(p + 1) == Some(&b'"') {
                        p += 2;
                        continue;
                    }
                    p += 1;
                    closed = true;
                    break;
                }
                if b[p] == 0 {
                    return None;
                }
                p += 1;
            }
            if !closed {
                return None;
            }
            words += 1;
        } else if c == b'.' || c == b' ' {
            if p == 0 || p + 1 == b.len() || b[p + 1] == b' ' || b[p + 1] == b'.' {
                return None;
            }
            p += 1;
        } else if c == b'(' {
            if words == 0 {
                return None;
            }
            p += 1;
            let start = p;
            while p < b.len() && b[p] != b')' {
                let ch = b[p];
                if !(word_char(ch) || ch == b',' || ch == b' ' || ch == b'-') {
                    return None;
                }
                // `-` only as a sign before a digit (negative typmods do not occur, but
                // never allow `--`)
                if ch == b'-' && !b.get(p + 1).is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                p += 1;
            }
            if p == b.len() || p == start {
                return None;
            }
            p += 1;
        } else if c == b'[' {
            if words == 0 || b.get(p + 1) != Some(&b']') {
                return None;
            }
            arrays = true;
            p += 2;
        } else {
            return None;
        }
    }
    (words > 0).then_some(type_name)
}

fn require_type(col: &str, data_type: &str) -> AppResult<String> {
    safe_type_name(data_type)
        .map(str::to_string)
        .ok_or_else(|| {
            AppError::new(
                ErrorKind::Refused,
                format!("column {col} has a type name pgWhy cannot use safely: {data_type}"),
            )
        })
}

/// The SQL expression rendering column `ident` (already quoted) as text the way the grid
/// shows it (see the module docs for the per-type exceptions).
pub fn text_expr(ident: &str, type_oid: u32) -> String {
    match type_oid {
        BOOL_OID => format!("CASE WHEN {ident} THEN 't' WHEN NOT {ident} THEN 'f' END"),
        INET_OID => format!("abbrev({ident})"),
        BPCHAR_OID => format!("CASE WHEN {ident} IS NULL THEN NULL ELSE format('%s', {ident}) END"),
        _ => format!("{ident}::text"),
    }
}

// ---------------------------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------------------------

/// A relation column as the builder needs it (from introspection's [`ColumnInfo`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TopNColumn {
    pub name: String,
    /// `format_type` text, e.g. `character varying(40)`.
    pub data_type: String,
    pub type_oid: u32,
    /// Generated and identity-ALWAYS columns cannot be written.
    #[serde(default = "yes")]
    pub writable: bool,
}

fn yes() -> bool {
    true
}

impl From<&ColumnInfo> for TopNColumn {
    fn from(c: &ColumnInfo) -> Self {
        Self {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            type_oid: c.type_oid,
            writable: c.is_writable(),
        }
    }
}

/// Criteria-row operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CriteriaOp {
    #[serde(rename = "=")]
    Eq,
    #[serde(rename = "<>")]
    Ne,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "LIKE")]
    Like,
    #[serde(rename = "ILIKE")]
    ILike,
    #[serde(rename = "NOT LIKE")]
    NotLike,
    #[serde(rename = "IS NULL")]
    IsNull,
    #[serde(rename = "IS NOT NULL")]
    IsNotNull,
}

impl CriteriaOp {
    pub fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Like => "LIKE",
            Self::ILike => "ILIKE",
            Self::NotLike => "NOT LIKE",
            Self::IsNull => "IS NULL",
            Self::IsNotNull => "IS NOT NULL",
        }
    }

    pub fn takes_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::IsNotNull)
    }

    fn is_pattern(self) -> bool {
        matches!(self, Self::Like | Self::ILike | Self::NotLike)
    }
}

/// One criteria-row entry. Comparison values are cast to the column's type server-side;
/// `LIKE`-family values are patterns matched against the column's text (the user's `%`/`_`
/// are wildcards — use [`crate::introspect::escape_like`] for a literal match).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Criterion {
    pub column: String,
    pub op: CriteriaOp,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TopNRequest {
    pub schema: String,
    pub relation: String,
    /// Every column of the relation, in table order.
    pub columns: Vec<TopNColumn>,
    /// Columns to show, in order; `None` = all. Key columns are added when missing.
    #[serde(default)]
    pub selected: Option<Vec<String>>,
    /// The edit key ([`EditProvenance`]); empty = read-only.
    #[serde(default)]
    pub key_columns: Vec<String>,
    /// Shown when `key_columns` is empty.
    #[serde(default)]
    pub read_only_reason: Option<String>,
    #[serde(default)]
    pub criteria: Vec<Criterion>,
    #[serde(default)]
    pub sort: Vec<SortKey>,
    pub limit: u64,
}

impl TopNRequest {
    /// All columns, keyed by the relation's edit provenance, no criteria/sort.
    pub fn for_relation(rel: &RelationInfo, provenance: &EditProvenance, limit: u64) -> Self {
        Self {
            schema: rel.schema.clone(),
            relation: rel.name.clone(),
            columns: rel.columns.iter().map(TopNColumn::from).collect(),
            selected: None,
            key_columns: provenance
                .key
                .as_ref()
                .map(|k| k.columns().to_vec())
                .unwrap_or_default(),
            read_only_reason: provenance.read_only_reason.clone(),
            criteria: Vec::new(),
            sort: Vec::new(),
            limit,
        }
    }
}

/// One output column of a top-N query.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TopNOutputColumn {
    pub name: String,
    pub data_type: String,
    pub type_oid: u32,
    pub writable: bool,
    pub is_key: bool,
    /// A key column the user did not select: present for editing, hidden by default.
    pub hidden: bool,
}

/// A built top-N statement.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TopNQuery {
    /// Executable SQL with `$n` text parameters.
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// The "generated SQL" preview: same statement with values as quoted literals. Display
    /// only — never executed.
    pub display_sql: String,
    pub columns: Vec<TopNOutputColumn>,
    pub schema: String,
    pub relation: String,
    pub key_columns: Vec<String>,
    /// Why the grid is read-only (no key, or a key type pgWhy cannot bind safely).
    pub read_only_reason: Option<String>,
}

fn unknown_column(name: &str) -> AppError {
    AppError::new(
        ErrorKind::Refused,
        format!("column {name} does not exist on this table"),
    )
    .with_hint("refresh the object explorer; the table may have changed")
}

/// Build the top-N statement. Errors name unknown columns, missing comparison values, and
/// column types that cannot be cast safely.
pub fn build_topn(req: &TopNRequest) -> AppResult<TopNQuery> {
    if req.limit == 0 {
        return Err(AppError::new(ErrorKind::Refused, "N must be at least 1"));
    }
    let find = |name: &str| {
        req.columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| unknown_column(name))
    };
    let mut out: Vec<TopNOutputColumn> = Vec::new();
    let add = |c: &TopNColumn, hidden: bool, out: &mut Vec<TopNOutputColumn>| {
        if !out.iter().any(|o| o.name == c.name) {
            out.push(TopNOutputColumn {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                type_oid: c.type_oid,
                writable: c.writable,
                is_key: req.key_columns.contains(&c.name),
                hidden,
            });
        }
    };
    match &req.selected {
        Some(sel) => {
            for name in sel {
                add(find(name)?, false, &mut out);
            }
        }
        None => {
            for c in &req.columns {
                add(c, false, &mut out);
            }
        }
    }
    let mut read_only_reason = if req.key_columns.is_empty() {
        Some(
            req.read_only_reason
                .clone()
                .unwrap_or_else(|| "the table has no edit key".into()),
        )
    } else {
        None
    };
    for k in &req.key_columns {
        let c = find(k)?;
        add(c, true, &mut out);
        if safe_type_name(&c.data_type).is_none() {
            read_only_reason = Some(format!(
                "key column {} has a type pgWhy cannot bind safely",
                c.name
            ));
        }
    }
    if out.is_empty() {
        return Err(AppError::new(
            ErrorKind::Refused,
            "the table has no columns",
        ));
    }

    let mut b = SqlBuilder::default();
    b.push("SELECT ");
    for (i, c) in out.iter().enumerate() {
        if i > 0 {
            b.push(", ");
        }
        let ident = quote_ident(&c.name);
        b.push(&text_expr(&ident, c.type_oid));
        b.push(" AS ").push(&ident);
    }
    let table = qualified(&req.schema, &req.relation);
    b.push(" FROM ").push(&table);
    for (i, cr) in req.criteria.iter().enumerate() {
        let col = find(&cr.column)?;
        let ident = quote_ident(&col.name);
        b.push(if i == 0 { " WHERE " } else { " AND " });
        if !cr.op.takes_value() {
            b.push(&ident).push(" ").push(cr.op.sql());
            continue;
        }
        let Some(value) = cr.value.as_deref() else {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("the {} filter on {} needs a value", cr.op.sql(), col.name),
            ));
        };
        if cr.op.is_pattern() {
            b.push(&text_expr(&ident, col.type_oid))
                .push(" ")
                .push(cr.op.sql())
                .push(" ")
                .param(Some(value));
        } else {
            let ty = require_type(&col.name, &col.data_type)?;
            b.push(&ident)
                .push(" ")
                .push(cr.op.sql())
                .push(" ")
                .typed(Some(value), &ty);
        }
    }
    for (i, s) in req.sort.iter().enumerate() {
        let col = find(&s.column)?;
        b.push(if i == 0 { " ORDER BY " } else { ", " });
        // table-qualified: a bare name would resolve to the `::text` output alias and sort
        // as text
        b.push(&table)
            .push(".")
            .push(&quote_ident(&col.name))
            .push(if s.descending { " DESC" } else { " ASC" });
    }
    b.push(&format!(" LIMIT {}", req.limit));

    Ok(TopNQuery {
        sql: b.sql,
        params: b.params,
        display_sql: b.display,
        columns: out,
        schema: req.schema.clone(),
        relation: req.relation.clone(),
        key_columns: req.key_columns.clone(),
        read_only_reason,
    })
}

impl TopNQuery {
    /// Result column metadata: the original type (oid + `format_type` text), not `text`.
    pub fn result_columns(&self) -> Vec<ColumnMeta> {
        self.columns
            .iter()
            .map(|c| ColumnMeta {
                name: c.name.clone(),
                type_oid: Some(c.type_oid),
                type_name: Some(c.data_type.clone()),
            })
            .collect()
    }

    /// The edit target for the result, or `None` when read-only.
    pub fn edit_target(&self) -> Option<EditTarget> {
        if self.read_only_reason.is_some() || self.key_columns.is_empty() {
            return None;
        }
        let columns = self
            .columns
            .iter()
            .map(|c| EditColumn {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                type_oid: c.type_oid,
                writable: c.writable,
            })
            .collect();
        EditTarget::new(&self.schema, &self.relation, columns, &self.key_columns).ok()
    }

    /// An empty result set for this query, with the edit target attached when editable.
    pub fn new_result_set(&self, cap: usize) -> ResultSet {
        let set = ResultSet::new(self.result_columns(), cap);
        set.set_edit_target(self.edit_target());
        set
    }
}

/// How a top-N run ended.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TopNOutcome {
    pub rows: usize,
    pub truncated: bool,
    /// Cancelled through the connection's cancel token; the rows so far are kept.
    pub cancelled: bool,
    pub duration_ms: f64,
}

/// Run `query` on the grid's dedicated connection, streaming rows into `set` (from
/// [`TopNQuery::new_result_set`]). Cancel with the connection's cancel token: a server-side
/// cancel ends the run with `cancelled` and the partial rows kept. Other failures mark the
/// set interrupted and return the classified error.
pub async fn run_topn(
    client: &tokio_postgres::Client,
    query: &TopNQuery,
    set: &ResultSet,
) -> AppResult<TopNOutcome> {
    let started = Instant::now();
    let fail = |e: &tokio_postgres::Error, set: &ResultSet| -> AppResult<TopNOutcome> {
        if e.as_db_error()
            .is_some_and(|db| db.code().code() == "57014")
        {
            set.finish(ResultState::Cancelled);
            return Ok(TopNOutcome {
                rows: set.row_count(),
                truncated: set.truncated(),
                cancelled: true,
                duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            });
        }
        set.finish(ResultState::Interrupted);
        Err(classify_pg(e, "load the rows"))
    };
    let stream = match client
        .query_raw(query.sql.as_str(), params_refs(&query.params))
        .await
    {
        Ok(s) => s,
        Err(e) => return fail(&e, set),
    };
    let mut stream = std::pin::pin!(stream);
    let mut chunk: Vec<tokio_postgres::Row> = Vec::with_capacity(256);
    while let Some(next) = stream.next().await {
        match next {
            Ok(row) => {
                chunk.push(row);
                if chunk.len() >= 256 {
                    set.append_source(chunk.as_slice());
                    chunk.clear();
                }
            }
            Err(e) => {
                set.append_source(chunk.as_slice());
                return fail(&e, set);
            }
        }
    }
    set.append_source(chunk.as_slice());
    set.finish(ResultState::Complete);
    Ok(TopNOutcome {
        rows: set.row_count(),
        truncated: set.truncated(),
        cancelled: false,
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_name_grammar() {
        for ok in [
            "integer",
            "character varying(40)",
            "numeric(10,2)",
            "timestamp(3) with time zone",
            "integer[]",
            "text[][]",
            "\"char\"",
            "public.\"My Type\"",
            "\"we\"\"ird\".t",
            "geometry(Point,4326)",
            "interval day to second(3)",
            "bit varying(5)",
        ] {
            assert_eq!(safe_type_name(ok), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "int; drop table x",
            "int)--",
            "text) || (select 1",
            "int --",
            "int/*",
            "\"unterminated",
            "int[1]",
            "int[] x",
            "a..b",
            "(1)",
            "int  4",
            "x'y",
            "numeric(--1)",
            ".int",
        ] {
            assert_eq!(safe_type_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn display_literals() {
        assert_eq!(display_literal(None), "NULL");
        assert_eq!(display_literal(Some("it's")), "'it''s'");
        assert_eq!(display_literal(Some(r"a\b")), r"E'a\\b'");
    }
}
