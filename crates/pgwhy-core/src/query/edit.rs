//! Editable-grid commit engine (U13 / R16 / AE4).
//!
//! A grid's pending edits (held in its [`ResultSet`]) become one statement per changed row:
//!
//! - `DELETE FROM t WHERE k = CAST($n::text AS ktype) …`
//! - `UPDATE t SET c = CAST($n::text AS ctype), … WHERE <key> [AND <original values>]`
//! - `INSERT INTO t (only the columns the user set) VALUES (…)` — untouched columns take their
//!   defaults (`DEFAULT VALUES` when none were set)
//!
//! Keys use the row's *original* values from the buffer. With
//! [`CommitOptions::verify_original_values`] (the default) an `UPDATE` also requires each
//! column it changes to still hold the value the grid loaded (compared as the same text the
//! grid shows), so a concurrent change to that row is detected instead of overwritten.
//!
//! The preview is display text only. The commit executes the parameterized statements in one
//! transaction with `SET LOCAL lock_timeout`, requires every statement to affect exactly one
//! row, and on any mismatch or error rolls back the whole set, names the rows, and leaves the
//! pending edits untouched. Only a successful commit clears them.

use super::results::{PendingChange, ResultSet};
use super::topn::{params_refs, safe_type_name, text_expr, SqlBuilder};
use crate::conn::classify as classify_pg;
use crate::introspect::{qualified, quote_ident};
use crate::{AppError, AppResult, ErrorClass, ErrorKind};
use std::time::Instant;

/// One result column as the edit layer sees it (aligned with the result's columns).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditColumn {
    pub name: String,
    /// `format_type` text — the cast target for values of this column.
    pub data_type: String,
    pub type_oid: u32,
    pub writable: bool,
}

/// The table a grid edits and how its rows are keyed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditTarget {
    pub schema: String,
    pub relation: String,
    /// One per result column, in result order.
    pub columns: Vec<EditColumn>,
    /// Indices into `columns` of the edit key (primary key or unique NOT NULL key).
    pub key: Vec<usize>,
}

impl EditTarget {
    /// Resolve `key_columns` against the result columns. Fails (read-only, with the reason)
    /// when a key column is missing from the result or has a type that cannot be cast safely.
    pub fn new(
        schema: &str,
        relation: &str,
        columns: Vec<EditColumn>,
        key_columns: &[String],
    ) -> AppResult<Self> {
        if key_columns.is_empty() {
            return Err(AppError::new(
                ErrorKind::Refused,
                "the table has no edit key",
            ));
        }
        let mut key = Vec::with_capacity(key_columns.len());
        for k in key_columns {
            let Some(i) = columns.iter().position(|c| &c.name == k) else {
                return Err(AppError::new(
                    ErrorKind::Refused,
                    format!("key column {k} is not in the results"),
                ));
            };
            if safe_type_name(&columns[i].data_type).is_none() {
                return Err(AppError::new(
                    ErrorKind::Refused,
                    format!("key column {k} has a type pgWhy cannot bind safely"),
                ));
            }
            key.push(i);
        }
        Ok(Self {
            schema: schema.to_string(),
            relation: relation.to_string(),
            columns,
            key,
        })
    }

    pub fn key_names(&self) -> Vec<&str> {
        self.key
            .iter()
            .map(|&i| self.columns[i].name.as_str())
            .collect()
    }

    /// Whether column `col` can be edited (exists and is not generated/identity-ALWAYS).
    pub fn is_writable(&self, col: usize) -> bool {
        self.columns.get(col).is_some_and(|c| c.writable)
    }

    fn column(&self, col: usize) -> AppResult<&EditColumn> {
        self.columns.get(col).ok_or_else(|| {
            AppError::internal(format!("pending edit refers to unknown column {col}"))
        })
    }

    fn cast_type(&self, col: usize) -> AppResult<&str> {
        let c = self.column(col)?;
        safe_type_name(&c.data_type).ok_or_else(|| {
            AppError::new(
                ErrorKind::Refused,
                format!(
                    "column {} has a type pgWhy cannot bind safely: {}",
                    c.name, c.data_type
                ),
            )
        })
    }

    fn writable_column(&self, col: usize) -> AppResult<&EditColumn> {
        let c = self.column(col)?;
        if !c.writable {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("column {} is generated and cannot be edited", c.name),
            ));
        }
        Ok(c)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditKind {
    Delete,
    Update,
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyValue {
    pub column: String,
    pub value: Option<String>,
}

/// One generated statement.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditStatement {
    pub kind: EditKind,
    /// Grid row index.
    pub row: usize,
    /// The row's original key values (empty for inserts).
    pub key: Vec<KeyValue>,
    /// Executed: `$n` text parameters only.
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// The preview text (literals quoted for display). Never executed.
    pub display: String,
}

impl EditStatement {
    /// `id = 3, line_no = 2`, or `new row 7` for inserts — how errors name the row.
    pub fn describe_row(&self) -> String {
        if self.kind == EditKind::Insert {
            return format!("new row {}", self.row + 1);
        }
        self.key
            .iter()
            .map(|k| format!("{} = {}", k.column, k.value.as_deref().unwrap_or("NULL")))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditPlan {
    pub statements: Vec<EditStatement>,
    /// The pending-edit version the plan was made from.
    #[serde(skip)]
    version: u64,
}

impl EditPlan {
    /// The preview lines, one statement each, terminated with `;`.
    pub fn preview(&self) -> Vec<String> {
        self.statements
            .iter()
            .map(|s| format!("{};", s.display))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitOptions {
    /// `SET LOCAL lock_timeout` for the commit transaction (EC-21); `0` disables it.
    pub lock_timeout_ms: u64,
    /// UPDATEs also require the changed columns to still hold their loaded values.
    pub verify_original_values: bool,
}

impl Default for CommitOptions {
    fn default() -> Self {
        Self {
            lock_timeout_ms: 5_000,
            verify_original_values: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommitReport {
    pub updated: usize,
    pub inserted: usize,
    pub deleted: usize,
    pub statements: usize,
    pub duration_ms: f64,
}

/// A row whose statement did not affect exactly one row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RowMismatch {
    pub row: usize,
    pub kind: EditKind,
    pub key: Vec<KeyValue>,
    pub affected: u64,
    pub description: String,
}

fn key_where(
    b: &mut SqlBuilder,
    target: &EditTarget,
    original: &[Option<String>],
) -> AppResult<Vec<KeyValue>> {
    let mut key = Vec::with_capacity(target.key.len());
    for (n, &k) in target.key.iter().enumerate() {
        let col = target.column(k)?;
        let value = original.get(k).cloned().flatten();
        if value.is_none() {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!(
                    "a row has NULL in key column {}; it cannot be targeted",
                    col.name
                ),
            ));
        }
        b.push(if n == 0 { " WHERE " } else { " AND " });
        b.push(&quote_ident(&col.name))
            .push(" = ")
            .typed(value.as_deref(), target.cast_type(k)?);
        key.push(KeyValue {
            column: col.name.clone(),
            value,
        });
    }
    Ok(key)
}

/// Generate the statements for `set`'s pending edits against `target`.
pub fn plan_edits(
    set: &ResultSet,
    target: &EditTarget,
    opts: &CommitOptions,
) -> AppResult<EditPlan> {
    let (version, changes) = set.pending_snapshot();
    let table = qualified(&target.schema, &target.relation);
    let mut statements = Vec::with_capacity(changes.len());
    for change in &changes {
        let mut b = SqlBuilder::default();
        let (kind, row, key) = match change {
            PendingChange::Delete { row, original } => {
                b.push("DELETE FROM ").push(&table);
                let key = key_where(&mut b, target, original)?;
                (EditKind::Delete, *row, key)
            }
            PendingChange::Update {
                row,
                original,
                values,
            } => {
                b.push("UPDATE ").push(&table).push(" SET ");
                for (n, (col, v)) in values.iter().enumerate() {
                    let c = target.writable_column(*col)?;
                    if n > 0 {
                        b.push(", ");
                    }
                    b.push(&quote_ident(&c.name))
                        .push(" = ")
                        .typed(v.as_deref(), target.cast_type(*col)?);
                }
                let key = key_where(&mut b, target, original)?;
                if opts.verify_original_values {
                    for (col, _) in values {
                        if target.key.contains(col) {
                            continue; // the key comparison already pins it
                        }
                        let c = target.column(*col)?;
                        let ident = quote_ident(&c.name);
                        b.push(" AND ")
                            .push(&text_expr(&ident, c.type_oid))
                            .push(" IS NOT DISTINCT FROM ")
                            .param(original.get(*col).cloned().flatten().as_deref());
                    }
                }
                (EditKind::Update, *row, key)
            }
            PendingChange::Insert { row, values } => {
                b.push("INSERT INTO ").push(&table);
                if values.is_empty() {
                    b.push(" DEFAULT VALUES");
                } else {
                    b.push(" (");
                    for (n, (col, _)) in values.iter().enumerate() {
                        let c = target.writable_column(*col)?;
                        if n > 0 {
                            b.push(", ");
                        }
                        b.push(&quote_ident(&c.name));
                    }
                    b.push(") VALUES (");
                    for (n, (col, v)) in values.iter().enumerate() {
                        if n > 0 {
                            b.push(", ");
                        }
                        b.typed(v.as_deref(), target.cast_type(*col)?);
                    }
                    b.push(")");
                }
                (EditKind::Insert, *row, Vec::new())
            }
        };
        statements.push(EditStatement {
            kind,
            row,
            key,
            sql: b.sql,
            params: b.params,
            display: b.display,
        });
    }
    Ok(EditPlan {
        statements,
        version,
    })
}

async fn rollback(client: &tokio_postgres::Client) {
    if let Err(e) = client.batch_execute("ROLLBACK").await {
        tracing::warn!(error = %e, "rollback after a failed grid commit failed");
    }
}

fn statement_error(
    e: &tokio_postgres::Error,
    st: &EditStatement,
    opts: &CommitOptions,
) -> AppError {
    let row = st.describe_row();
    let data = serde_json::json!({
        "rows": [{ "row": st.row, "kind": st.kind, "key": st.key }],
        "sqlstate": e.as_db_error().map(|d| d.code().code().to_string()),
    });
    let mut app = classify_pg(e, "commit the grid changes");
    if e.as_db_error().is_some_and(|d| d.code().code() == "55P03") {
        app.message = format!(
            "row {row} is locked by another session; gave up after {} ms and nothing was committed",
            opts.lock_timeout_ms
        );
        app.class = ErrorClass::Retriable;
        app.hint = Some("retry once the other transaction finishes".into());
    } else {
        app.message = format!(
            "could not apply the change to row {row}: {}; nothing was committed",
            app.message
        );
    }
    app.with_data(data)
}

/// Execute `set`'s pending edits against `target` in one transaction on the grid's dedicated
/// connection (see the module docs). Edits are frozen while this runs. On success the pending
/// edits are cleared (re-query the grid to show committed state); on any failure they are
/// kept and the error names the offending rows (`data.rows`).
///
/// Do not drop the future mid-way — it would leave the transaction open; cancel through the
/// connection's cancel token instead (the error path rolls back).
pub async fn commit_edits(
    client: &tokio_postgres::Client,
    set: &ResultSet,
    target: &EditTarget,
    opts: &CommitOptions,
) -> AppResult<CommitReport> {
    let started = Instant::now();
    let _guard = set.begin_commit()?;
    let plan = plan_edits(set, target, opts)?;
    let mut report = CommitReport {
        updated: 0,
        inserted: 0,
        deleted: 0,
        statements: plan.statements.len(),
        duration_ms: 0.0,
    };
    if plan.is_empty() {
        return Ok(report);
    }
    client
        .batch_execute("BEGIN")
        .await
        .map_err(|e| classify_pg(&e, "start the commit transaction"))?;
    if opts.lock_timeout_ms > 0 {
        let set_timeout = format!("SET LOCAL lock_timeout = '{}ms'", opts.lock_timeout_ms);
        if let Err(e) = client.batch_execute(&set_timeout).await {
            rollback(client).await;
            return Err(classify_pg(&e, "set the commit lock timeout"));
        }
    }
    let mut mismatches = Vec::new();
    for st in &plan.statements {
        match client
            .execute(st.sql.as_str(), &params_refs(&st.params))
            .await
        {
            Ok(1) => match st.kind {
                EditKind::Update => report.updated += 1,
                EditKind::Insert => report.inserted += 1,
                EditKind::Delete => report.deleted += 1,
            },
            Ok(n) => mismatches.push(RowMismatch {
                row: st.row,
                kind: st.kind,
                key: st.key.clone(),
                affected: n,
                description: st.describe_row(),
            }),
            Err(e) => {
                rollback(client).await;
                return Err(statement_error(&e, st, opts));
            }
        }
    }
    if !mismatches.is_empty() {
        rollback(client).await;
        let names = mismatches
            .iter()
            .map(|m| m.description.clone())
            .collect::<Vec<_>>()
            .join("; ");
        let message = if mismatches.len() == 1 {
            format!("row {names} was changed or deleted by someone else; nothing was committed")
        } else {
            format!(
                "{} rows were changed or deleted by someone else ({names}); nothing was committed",
                mismatches.len()
            )
        };
        return Err(AppError::new(ErrorKind::Sql, message)
            .with_class(ErrorClass::FixThenRetry)
            .with_hint("re-query to see the current rows; your pending edits are kept")
            .with_data(serde_json::json!({ "rows": mismatches })));
    }
    if let Err(e) = client.batch_execute("COMMIT").await {
        rollback(client).await;
        let mut app = classify_pg(&e, "commit the grid changes");
        app.message = format!("the commit failed: {}; nothing was committed", app.message);
        return Err(app);
    }
    set.clear_committed(plan.version);
    report.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}
