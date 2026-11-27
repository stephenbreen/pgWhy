//! Deterministic SQL compiler for Workflows v2 visual step blocks and `{{param}}` substitution.
//!
//! Every visual step compiles cleanly to inspectable PostgreSQL SQL using
//! [`pgwhy_core::introspect::quote_ident_always`] and strict literal quoting/validation, so:
//! - users can toggle **Preview SQL** on any visual step card;
//! - users can click **Convert to SQL** on any visual block while preserving `{{param}}` bindings;
//! - the engine executes the exact same compiled SQL (with secrets bound in memory only).

use crate::pipeline::*;
use crate::real::ScriptSource;
use pgwhy_core::introspect::quote_ident_always;
use pgwhy_core::secret::Secret;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};

/// How `{{param}}` references are rendered during compilation.
pub enum CompileMode<'a> {
    /// Preserve `{{param_name}}` placeholders verbatim (used by "Convert to SQL").
    Template,
    /// Substitute non-secret values/defaults and mask secrets as `'••••••••'` (used by "Preview SQL"
    /// and safe history snapshots — never reveals secret values).
    Preview { values: &'a HashMap<String, String> },
    /// Substitute actual bound values for execution (kept in memory only; never logged or persisted).
    Execute {
        values: &'a HashMap<String, String>,
        secrets: &'a HashMap<String, Secret>,
    },
}

/// The compiled SQL and static analysis metadata for one workflow step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledStep {
    pub step_id: String,
    pub step_name: String,
    pub sql: String,
    pub no_transaction: bool,
    pub is_verify: bool,
    /// Non-fatal static warnings (e.g., `DELETE` without `WHERE`, `{{param}}` inside quotes).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Fatal static problems (e.g., psql meta-commands, explicit `BEGIN`/`COMMIT`, `VACUUM` in TX).
    #[serde(default)]
    pub problems: Vec<String>,
    /// Declared parameter names referenced by this step.
    #[serde(default)]
    pub referenced_params: Vec<String>,
}

/// Qualified `"schema"."table"` identifier, refusing empty names.
fn qualified_table(schema: &str, table: &str) -> AppResult<String> {
    let s = schema.trim();
    let t = table.trim();
    if t.is_empty() {
        return Err(AppError::new(
            ErrorKind::Config,
            "a step is missing its table name",
        ));
    }
    if s.is_empty() {
        Ok(quote_ident_always(t))
    } else {
        Ok(format!(
            "{}.{}",
            quote_ident_always(s),
            quote_ident_always(t)
        ))
    }
}

/// Quote a PostgreSQL string literal (`standard_conforming_strings = on`), rejecting NUL bytes.
pub fn quote_sql_literal(value: &str) -> AppResult<String> {
    if value.contains('\0') {
        return Err(AppError::new(
            ErrorKind::Config,
            "SQL text values cannot contain NUL (\\0) bytes",
        ));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

struct CompilerCtx<'a> {
    params: HashMap<&'a str, &'a WorkflowParam>,
    mode: &'a CompileMode<'a>,
    referenced: BTreeSet<String>,
    warnings: Vec<String>,
}

impl<'a> CompilerCtx<'a> {
    fn new(params: &'a [WorkflowParam], mode: &'a CompileMode<'a>) -> Self {
        Self {
            params: params.iter().map(|p| (p.name.as_str(), p)).collect(),
            mode,
            referenced: BTreeSet::new(),
            warnings: Vec::new(),
        }
    }

    fn resolve_param_sql(&mut self, name: &str) -> AppResult<String> {
        let Some(decl) = self.params.get(name).copied() else {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("unknown workflow parameter \"{{{{{name}}}}}\""),
            )
            .with_hint(format!(
                "declare parameter \"{name}\" in the workflow's Parameters section"
            )));
        };
        self.referenced.insert(name.to_string());
        match self.mode {
            CompileMode::Template => Ok(format!("{{{{{name}}}}}")),
            CompileMode::Preview { values } => {
                if decl.kind == ParamType::Secret {
                    return Ok("'••••••••'".into());
                }
                let val = values
                    .get(name)
                    .map(String::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| decl.default.as_deref().filter(|s| !s.is_empty()));
                match val {
                    Some(v) => format_typed_param(decl, v),
                    None => match decl.kind {
                        ParamType::Number => Ok(format!("0 /* {{{{{name}}}}} */")),
                        ParamType::Text | ParamType::FilePath | ParamType::Secret => {
                            Ok(format!("'{{{{{name}}}}}'"))
                        }
                    },
                }
            }
            CompileMode::Execute { values, secrets } => {
                if decl.kind == ParamType::Secret {
                    match secrets.get(name) {
                        Some(s) if !s.expose().is_empty() => quote_sql_literal(s.expose()),
                        _ if !decl.required => Ok("NULL".into()),
                        _ => Err(AppError::new(
                            ErrorKind::Refused,
                            format!("missing required secret parameter \"{name}\""),
                        )),
                    }
                } else {
                    let val = values
                        .get(name)
                        .map(String::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| decl.default.as_deref().filter(|s| !s.is_empty()));
                    match val {
                        Some(v) => format_typed_param(decl, v),
                        None if !decl.required => Ok("NULL".into()),
                        None => Err(AppError::new(
                            ErrorKind::Refused,
                            format!("missing required parameter \"{name}\""),
                        )),
                    }
                }
            }
        }
    }

    /// Substitute `{{param}}` references inside a raw SQL fragment or expression, respecting SQL
    /// quotes and comments so string literals and comments are never corrupted.
    fn substitute_sql(&mut self, sql: &str) -> AppResult<String> {
        let bytes = sql.as_bytes();
        let len = bytes.len();
        let mut out = String::with_capacity(len);
        let mut i = 0;

        while i < len {
            // Line comment `-- ... \n`
            if bytes[i] == b'-' && i + 1 < len && bytes[i + 1] == b'-' {
                let start = i;
                i += 2;
                while i < len && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push_str(&sql[start..i]);
                continue;
            }
            // Nested block comment `/* ... */`
            if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
                let start = i;
                i += 2;
                let mut depth = 1usize;
                while i < len && depth > 0 {
                    if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                out.push_str(&sql[start..i]);
                continue;
            }
            // Single-quoted string `'...'` (with `''` escapes)
            if bytes[i] == b'\'' {
                let start = i;
                i += 1;
                while i < len {
                    if bytes[i] == b'\'' {
                        if i + 1 < len && bytes[i + 1] == b'\'' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                let literal = &sql[start..i];
                for p in self.params.keys() {
                    let needle = format!("{{{{{p}}}}}");
                    if literal.contains(&needle) {
                        let msg = format!(
                            "parameter {needle} appears inside single quotes; write {needle} unquoted so pgWhy quotes it safely"
                        );
                        if !self.warnings.contains(&msg) {
                            self.warnings.push(msg);
                        }
                    }
                }
                out.push_str(literal);
                continue;
            }
            // Double-quoted identifier `"..."`
            if bytes[i] == b'"' {
                let start = i;
                i += 1;
                while i < len {
                    if bytes[i] == b'"' {
                        if i + 1 < len && bytes[i + 1] == b'"' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                out.push_str(&sql[start..i]);
                continue;
            }
            // Dollar-quoted body `$tag$...$tag$`
            if bytes[i] == b'$' {
                if let Some(tag_end) = dollar_tag_end(bytes, i) {
                    let tag = &sql[i..=tag_end];
                    let body_start = tag_end + 1;
                    if let Some(rel) = sql[body_start..].find(tag) {
                        let end = body_start + rel + tag.len();
                        out.push_str(&sql[i..end]);
                        i = end;
                        continue;
                    } else {
                        out.push_str(&sql[i..]);
                        break;
                    }
                }
            }
            // Parameter placeholder `{{name}}`
            if bytes[i] == b'{' && i + 1 < len && bytes[i + 1] == b'{' {
                if let Some(rel_end) = sql[i + 2..].find("}}") {
                    let candidate = sql[i + 2..i + 2 + rel_end].trim();
                    if is_valid_param_name(candidate) {
                        let replacement = self.resolve_param_sql(candidate)?;
                        out.push_str(&replacement);
                        i = i + 2 + rel_end + 2;
                        continue;
                    }
                }
            }
            let ch = sql[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        Ok(out)
    }

    fn compile_cell(&mut self, cell: &CellValue) -> AppResult<String> {
        match cell {
            CellValue::Literal { value } => quote_sql_literal(value),
            CellValue::Number { value } => {
                let trimmed = value.trim();
                if !is_valid_number_literal(trimmed) {
                    return Err(AppError::new(
                        ErrorKind::Config,
                        format!("\"{value}\" is not a valid numeric literal"),
                    ));
                }
                Ok(trimmed.to_string())
            }
            CellValue::Bool { value } => Ok(if *value { "TRUE" } else { "FALSE" }.into()),
            CellValue::Null => Ok("NULL".into()),
            CellValue::Default => Ok("DEFAULT".into()),
            CellValue::Param { name } => self.resolve_param_sql(name),
            CellValue::Expr { sql } => {
                let trimmed = sql.trim();
                if trimmed.is_empty() {
                    return Err(AppError::new(
                        ErrorKind::Config,
                        "SQL expression cell cannot be empty",
                    ));
                }
                self.substitute_sql(trimmed)
            }
            CellValue::List { items } => {
                if items.is_empty() {
                    return Err(AppError::new(
                        ErrorKind::Config,
                        "IN (...) list must contain at least one value",
                    ));
                }
                let mut compiled = Vec::with_capacity(items.len());
                for item in items {
                    compiled.push(self.compile_cell(item)?);
                }
                Ok(format!("({})", compiled.join(", ")))
            }
        }
    }

    fn compile_where(
        &mut self,
        conditions: &[Predicate],
        combinator: LogicalOp,
    ) -> AppResult<Option<String>> {
        if conditions.is_empty() {
            return Ok(None);
        }
        let joiner = match combinator {
            LogicalOp::And => " AND ",
            LogicalOp::Or => " OR ",
        };
        let mut parts = Vec::with_capacity(conditions.len());
        for cond in conditions {
            let col = cond.column.trim();
            if col.is_empty() {
                return Err(AppError::new(
                    ErrorKind::Config,
                    "a WHERE condition is missing its column name",
                ));
            }
            let qcol = quote_ident_always(col);
            let expr = match cond.op {
                FilterOp::IsNull => format!("{qcol} IS NULL"),
                FilterOp::IsNotNull => format!("{qcol} IS NOT NULL"),
                FilterOp::In | FilterOp::NotIn => {
                    let kw = if cond.op == FilterOp::In {
                        "IN"
                    } else {
                        "NOT IN"
                    };
                    let val = cond.value.as_ref().ok_or_else(|| {
                        AppError::new(
                            ErrorKind::Config,
                            format!("condition on \"{col}\" needs a list of values"),
                        )
                    })?;
                    let rhs = match val {
                        CellValue::List { .. } => self.compile_cell(val)?,
                        other => format!("({})", self.compile_cell(other)?),
                    };
                    format!("{qcol} {kw} {rhs}")
                }
                op => {
                    let sym = match op {
                        FilterOp::Eq => "=",
                        FilterOp::Neq => "!=",
                        FilterOp::Lt => "<",
                        FilterOp::Lte => "<=",
                        FilterOp::Gt => ">",
                        FilterOp::Gte => ">=",
                        FilterOp::Like => "LIKE",
                        FilterOp::ILike => "ILIKE",
                        FilterOp::NotLike => "NOT LIKE",
                        FilterOp::NotILike => "NOT ILIKE",
                        FilterOp::In | FilterOp::NotIn | FilterOp::IsNull | FilterOp::IsNotNull => {
                            unreachable!()
                        }
                    };
                    let val = cond.value.as_ref().ok_or_else(|| {
                        AppError::new(
                            ErrorKind::Config,
                            format!("condition on \"{col}\" needs a value"),
                        )
                    })?;
                    format!("{qcol} {sym} {}", self.compile_cell(val)?)
                }
            };
            parts.push(expr);
        }
        Ok(Some(parts.join(joiner)))
    }
}

fn dollar_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start >= bytes.len() || bytes[start] != b'$' {
        return None;
    }
    let mut j = start + 1;
    if j < bytes.len() && bytes[j] == b'$' {
        return Some(j);
    }
    if j >= bytes.len() || !(bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
        return None;
    }
    j += 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'$' {
        Some(j)
    } else {
        None
    }
}

fn format_typed_param(decl: &WorkflowParam, value: &str) -> AppResult<String> {
    match decl.kind {
        ParamType::Number => {
            let t = value.trim();
            if !is_valid_number_literal(t) {
                return Err(AppError::new(
                    ErrorKind::Config,
                    format!(
                        "value \"{value}\" for number parameter \"{}\" is not a valid number",
                        decl.name
                    ),
                ));
            }
            Ok(t.to_string())
        }
        ParamType::Text | ParamType::FilePath | ParamType::Secret => quote_sql_literal(value),
    }
}

/// Compile a single [`WorkflowStep`] into inspectable SQL plus static warnings/problems.
pub fn compile_step(
    step: &WorkflowStep,
    params: &[WorkflowParam],
    mode: &CompileMode<'_>,
    scripts: Option<&dyn ScriptSource>,
) -> AppResult<CompiledStep> {
    let mut ctx = CompilerCtx::new(params, mode);

    let sql = match &step.kind {
        StepKind::DeleteRows {
            schema,
            table,
            mode: del_mode,
        } => {
            let qt = qualified_table(schema, table)?;
            match del_mode {
                DeleteMode::DeleteWhere {
                    conditions,
                    combinator,
                } => match ctx.compile_where(conditions, *combinator)? {
                    Some(where_sql) => format!("DELETE FROM {qt}\nWHERE {where_sql};"),
                    None => {
                        ctx.warnings
                            .push(format!("Deletes every row in {qt} (no WHERE filter)."));
                        format!("DELETE FROM {qt};")
                    }
                },
                DeleteMode::Truncate {
                    restart_identity,
                    cascade,
                } => {
                    let mut sql = format!("TRUNCATE TABLE {qt}");
                    if *restart_identity {
                        sql.push_str(" RESTART IDENTITY");
                    }
                    if *cascade {
                        sql.push_str(" CASCADE");
                    }
                    sql.push(';');
                    sql
                }
            }
        }

        StepKind::InsertRows {
            schema,
            table,
            columns,
            rows,
            on_conflict,
        } => {
            let qt = qualified_table(schema, table)?;
            if columns.is_empty() {
                return Err(AppError::new(
                    ErrorKind::Config,
                    format!("insert step \"{}\" needs at least one column", step.name),
                ));
            }
            if rows.is_empty() {
                return Err(AppError::new(
                    ErrorKind::Config,
                    format!("insert step \"{}\" needs at least one row", step.name),
                ));
            }
            let qcols: Vec<String> = columns
                .iter()
                .map(|c| {
                    let t = c.trim();
                    if t.is_empty() {
                        Err(AppError::new(
                            ErrorKind::Config,
                            format!("insert step \"{}\" has an empty column name", step.name),
                        ))
                    } else {
                        Ok(quote_ident_always(t))
                    }
                })
                .collect::<AppResult<Vec<_>>>()?;

            let mut row_sqls = Vec::with_capacity(rows.len());
            for (idx, row) in rows.iter().enumerate() {
                if row.len() != columns.len() {
                    return Err(AppError::new(
                        ErrorKind::Config,
                        format!(
                            "insert step \"{}\" row {} has {} value(s), expected {}",
                            step.name,
                            idx + 1,
                            row.len(),
                            columns.len()
                        ),
                    ));
                }
                let mut vals = Vec::with_capacity(row.len());
                for cell in row {
                    vals.push(ctx.compile_cell(cell)?);
                }
                row_sqls.push(format!("  ({})", vals.join(", ")));
            }

            let mut sql = format!(
                "INSERT INTO {qt} ({})\nVALUES\n{}",
                qcols.join(", "),
                row_sqls.join(",\n")
            );
            match on_conflict {
                ConflictPolicy::Error => {}
                ConflictPolicy::DoNothing { columns: conf } => {
                    if conf.is_empty() {
                        sql.push_str("\nON CONFLICT DO NOTHING");
                    } else {
                        let qc: Vec<String> =
                            conf.iter().map(|c| quote_ident_always(c.trim())).collect();
                        sql.push_str(&format!("\nON CONFLICT ({}) DO NOTHING", qc.join(", ")));
                    }
                }
                ConflictPolicy::DoUpdate {
                    columns: conf,
                    update_columns,
                } => {
                    if conf.is_empty() {
                        return Err(AppError::new(
                            ErrorKind::Config,
                            format!(
                                "insert step \"{}\" uses ON CONFLICT DO UPDATE without conflict columns",
                                step.name
                            ),
                        ));
                    }
                    let qc: Vec<String> =
                        conf.iter().map(|c| quote_ident_always(c.trim())).collect();
                    let targets: Vec<&str> = if update_columns.is_empty() {
                        columns
                            .iter()
                            .map(|c| c.trim())
                            .filter(|c| !conf.iter().any(|cc| cc.trim() == *c))
                            .collect()
                    } else {
                        update_columns.iter().map(|c| c.trim()).collect()
                    };
                    if targets.is_empty() {
                        sql.push_str(&format!("\nON CONFLICT ({}) DO NOTHING", qc.join(", ")));
                    } else {
                        let sets: Vec<String> = targets
                            .into_iter()
                            .map(|c| {
                                let qi = quote_ident_always(c);
                                format!("{qi} = EXCLUDED.{qi}")
                            })
                            .collect();
                        sql.push_str(&format!(
                            "\nON CONFLICT ({}) DO UPDATE SET {}",
                            qc.join(", "),
                            sets.join(", ")
                        ));
                    }
                }
            }
            sql.push(';');
            sql
        }

        StepKind::UpdateMask {
            schema,
            table,
            conditions,
            combinator,
            assignments,
        } => {
            let qt = qualified_table(schema, table)?;
            if assignments.is_empty() {
                return Err(AppError::new(
                    ErrorKind::Config,
                    format!(
                        "update/mask step \"{}\" needs at least one column transform",
                        step.name
                    ),
                ));
            }
            let mut sets = Vec::with_capacity(assignments.len());
            for a in assignments {
                let col = a.column.trim();
                if col.is_empty() {
                    return Err(AppError::new(
                        ErrorKind::Config,
                        format!(
                            "update/mask step \"{}\" has an empty column name",
                            step.name
                        ),
                    ));
                }
                let qcol = quote_ident_always(col);
                let rhs = match &a.transform {
                    MaskTransform::SyntheticEmail { domain, id_column } => {
                        let d = domain.trim();
                        let d = if d.is_empty() { "example.test" } else { d };
                        let suffix = quote_sql_literal(&format!("@{d}"))?;
                        match id_column
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            Some(id_col) => {
                                format!(
                                    "'user_' || {}::text || {suffix}",
                                    quote_ident_always(id_col)
                                )
                            }
                            None => {
                                format!("'user_' || substr(md5(ctid::text), 1, 12) || {suffix}")
                            }
                        }
                    }
                    MaskTransform::FixedValue { value } => ctx.compile_cell(value)?,
                    MaskTransform::SetNull => "NULL".into(),
                    MaskTransform::Sha256Hash { salt } => match salt {
                        Some(s) => {
                            let salt_sql = ctx.compile_cell(s)?;
                            format!(
                                "encode(sha256(convert_to(COALESCE({qcol}::text, '') || ({salt_sql})::text, 'UTF8')), 'hex')"
                            )
                        }
                        None => {
                            format!(
                                "encode(sha256(convert_to(COALESCE({qcol}::text, ''), 'UTF8')), 'hex')"
                            )
                        }
                    },
                    MaskTransform::CustomExpr { sql: expr } => {
                        let trimmed = expr.trim();
                        if trimmed.is_empty() {
                            return Err(AppError::new(
                                ErrorKind::Config,
                                format!("custom mask expression for column \"{col}\" is empty"),
                            ));
                        }
                        ctx.substitute_sql(trimmed)?
                    }
                };
                sets.push(format!("  {qcol} = {rhs}"));
            }
            let mut sql = format!("UPDATE {qt}\nSET\n{}", sets.join(",\n"));
            if let Some(where_sql) = ctx.compile_where(conditions, *combinator)? {
                sql.push_str(&format!("\nWHERE {where_sql}"));
            }
            sql.push(';');
            sql
        }

        StepKind::Sql { body } => {
            let raw = match body {
                SqlStepBody::Inline { sql } => sql.clone(),
                SqlStepBody::Library { script_id } => {
                    if let Some(src) = scripts {
                        src.load(script_id)?.content
                    } else {
                        format!("-- Linked library script: {script_id}")
                    }
                }
            };
            ctx.substitute_sql(&raw)?
        }

        StepKind::Verify { assertion } => match assertion {
            VerifyAssertion::RowCount {
                schema,
                table,
                conditions,
                combinator,
                ..
            }
            | VerifyAssertion::ZeroRows {
                schema,
                table,
                conditions,
                combinator,
            } => {
                let qt = qualified_table(schema, table)?;
                let mut sql = format!("SELECT count(*) FROM {qt}");
                if let Some(where_sql) = ctx.compile_where(conditions, *combinator)? {
                    sql.push_str(&format!("\nWHERE {where_sql}"));
                }
                sql.push(';');
                sql
            }
            VerifyAssertion::CustomSql { sql, .. } => {
                let trimmed = sql.trim();
                if trimmed.is_empty() {
                    return Err(AppError::new(
                        ErrorKind::Config,
                        format!("verify step \"{}\" has empty SQL", step.name),
                    ));
                }
                ctx.substitute_sql(trimmed)?
            }
        },
    };

    // Static SQL safety checks (skip placeholder-only unresolved library comment).
    let mut problems = Vec::new();
    let check_sql = if matches!(mode, CompileMode::Template) {
        // Replace remaining {{param}} with a safe dummy literal so query::analyze parses cleanly.
        replace_template_placeholders_for_analysis(&sql, params)
    } else {
        sql.clone()
    };

    if pgwhy_core::scripts::has_meta_commands(&check_sql) {
        let cmds: Vec<String> = pgwhy_core::scripts::scan_meta_commands(&check_sql)
            .into_iter()
            .map(|m| format!("{} (line {})", m.command, m.line))
            .collect();
        problems.push(format!(
            "contains psql meta-commands, which pgWhy can't run: {}",
            cmds.join(", ")
        ));
    }
    let analysis = pgwhy_core::query::analyze(&check_sql);
    if !analysis.txn_control.is_empty() {
        problems.push(
            "contains explicit transaction control (BEGIN/COMMIT/ROLLBACK/SAVEPOINT); pgWhy manages the transaction".into(),
        );
    }
    if !analysis.copy_client.is_empty() {
        problems.push("uses COPY FROM STDIN / TO STDOUT, which needs psql".into());
    }
    if !analysis.unterminated.is_empty() {
        problems.push("ends inside an unterminated string, comment, or dollar-quoted body".into());
    }
    if !step.no_transaction && !step.is_verify() && analysis.needs_no_transaction_mode() {
        let reasons: Vec<String> = analysis
            .needs_no_transaction
            .iter()
            .map(|(_, why)| why.clone())
            .collect();
        problems.push(format!(
            "has statements that can't run inside a transaction ({}); enable \"Run without transaction\" on this step",
            reasons.join(", ")
        ));
    }

    Ok(CompiledStep {
        step_id: step.id.clone(),
        step_name: step.name.clone(),
        sql,
        no_transaction: step.no_transaction,
        is_verify: step.is_verify(),
        warnings: ctx.warnings,
        problems,
        referenced_params: ctx.referenced.into_iter().collect(),
    })
}

fn replace_template_placeholders_for_analysis(sql: &str, params: &[WorkflowParam]) -> String {
    let mut out = sql.to_string();
    for p in params {
        let needle = format!("{{{{{}}}}}", p.name);
        let dummy = match p.kind {
            ParamType::Number => "0",
            ParamType::Text | ParamType::FilePath | ParamType::Secret => "'__param__'",
        };
        out = out.replace(&needle, dummy);
    }
    out
}

/// Convert any visual step into an editable inline [`StepKind::Sql`] step, preserving `{{param}}`
/// references.
pub fn convert_step_to_sql(
    step: &WorkflowStep,
    params: &[WorkflowParam],
    scripts: Option<&dyn ScriptSource>,
) -> AppResult<WorkflowStep> {
    let compiled = compile_step(step, params, &CompileMode::Template, scripts)?;
    Ok(WorkflowStep {
        id: step.id.clone(),
        name: step.name.clone(),
        enabled: step.enabled,
        no_transaction: step.no_transaction,
        kind: StepKind::Sql {
            body: SqlStepBody::Inline { sql: compiled.sql },
        },
    })
}

/// Compile every enabled step of a workflow in order.
pub fn compile_steps(
    steps: &[WorkflowStep],
    params: &[WorkflowParam],
    mode: &CompileMode<'_>,
    scripts: Option<&dyn ScriptSource>,
) -> AppResult<Vec<CompiledStep>> {
    let mut out = Vec::new();
    for step in steps {
        if !step.enabled {
            continue;
        }
        out.push(compile_step(step, params, mode, scripts)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Vec<WorkflowParam> {
        vec![
            WorkflowParam {
                name: "admin_email".into(),
                label: None,
                description: None,
                kind: ParamType::Text,
                required: true,
                default: Some("admin@test.local".into()),
            },
            WorkflowParam {
                name: "min_age".into(),
                label: None,
                description: None,
                kind: ParamType::Number,
                required: true,
                default: Some("18".into()),
            },
            WorkflowParam {
                name: "test_password".into(),
                label: None,
                description: None,
                kind: ParamType::Secret,
                required: true,
                default: None,
            },
        ]
    }

    #[test]
    fn compiles_delete_where_and_truncate() {
        let del = WorkflowStep {
            id: "s1".into(),
            name: "Delete users".into(),
            enabled: true,
            no_transaction: false,
            kind: StepKind::DeleteRows {
                schema: "public".into(),
                table: "users".into(),
                mode: DeleteMode::DeleteWhere {
                    conditions: vec![
                        Predicate {
                            column: "email".into(),
                            op: FilterOp::Neq,
                            value: Some(CellValue::Param {
                                name: "admin_email".into(),
                            }),
                        },
                        Predicate {
                            column: "deleted_at".into(),
                            op: FilterOp::IsNotNull,
                            value: None,
                        },
                    ],
                    combinator: LogicalOp::Or,
                },
            },
        };
        let empty = HashMap::new();
        let compiled = compile_step(
            &del,
            &params(),
            &CompileMode::Preview { values: &empty },
            None,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "DELETE FROM \"public\".\"users\"\nWHERE \"email\" != 'admin@test.local' OR \"deleted_at\" IS NOT NULL;"
        );
        assert!(compiled.problems.is_empty());

        let trunc = WorkflowStep {
            id: "s2".into(),
            name: "Truncate".into(),
            enabled: true,
            no_transaction: false,
            kind: StepKind::DeleteRows {
                schema: "public".into(),
                table: "sessions".into(),
                mode: DeleteMode::Truncate {
                    restart_identity: true,
                    cascade: true,
                },
            },
        };
        let compiled = compile_step(
            &trunc,
            &params(),
            &CompileMode::Preview { values: &empty },
            None,
        )
        .unwrap();
        assert_eq!(
            compiled.sql,
            "TRUNCATE TABLE \"public\".\"sessions\" RESTART IDENTITY CASCADE;"
        );
    }

    #[test]
    fn compiles_insert_rows_with_conflict_and_masks_secret_in_preview() {
        let ins = WorkflowStep {
            id: "s3".into(),
            name: "Seed users".into(),
            enabled: true,
            no_transaction: false,
            kind: StepKind::InsertRows {
                schema: "public".into(),
                table: "users".into(),
                columns: vec!["email".into(), "password_hash".into(), "is_admin".into()],
                rows: vec![vec![
                    CellValue::Param {
                        name: "admin_email".into(),
                    },
                    CellValue::Expr {
                        sql: "encode(sha256(convert_to({{test_password}}, 'UTF8')), 'hex')".into(),
                    },
                    CellValue::Bool { value: true },
                ]],
                on_conflict: ConflictPolicy::DoUpdate {
                    columns: vec!["email".into()],
                    update_columns: Vec::new(),
                },
            },
        };
        let empty = HashMap::new();
        let preview = compile_step(
            &ins,
            &params(),
            &CompileMode::Preview { values: &empty },
            None,
        )
        .unwrap();
        assert!(preview.sql.contains("'••••••••'"), "{}", preview.sql);
        assert!(
            preview
                .sql
                .contains("ON CONFLICT (\"email\") DO UPDATE SET \"password_hash\" = EXCLUDED.\"password_hash\", \"is_admin\" = EXCLUDED.\"is_admin\";"),
            "{}",
            preview.sql
        );

        let mut secrets = HashMap::new();
        secrets.insert("test_password".into(), Secret::new("s3cr'et"));
        let exec = compile_step(
            &ins,
            &params(),
            &CompileMode::Execute {
                values: &empty,
                secrets: &secrets,
            },
            None,
        )
        .unwrap();
        assert!(exec.sql.contains("'s3cr''et'"), "{}", exec.sql);
        assert!(!exec.sql.contains("••••••••"));
    }

    #[test]
    fn compiles_update_mask_and_rejects_sql_injection_in_numbers() {
        let mask = WorkflowStep {
            id: "s4".into(),
            name: "Mask".into(),
            enabled: true,
            no_transaction: false,
            kind: StepKind::UpdateMask {
                schema: "public".into(),
                table: "users".into(),
                conditions: vec![Predicate {
                    column: "age".into(),
                    op: FilterOp::Gte,
                    value: Some(CellValue::Param {
                        name: "min_age".into(),
                    }),
                }],
                combinator: LogicalOp::And,
                assignments: vec![
                    MaskAssignment {
                        column: "email".into(),
                        transform: MaskTransform::SyntheticEmail {
                            domain: "test.local".into(),
                            id_column: Some("id".into()),
                        },
                    },
                    MaskAssignment {
                        column: "ssn".into(),
                        transform: MaskTransform::Sha256Hash { salt: None },
                    },
                    MaskAssignment {
                        column: "phone".into(),
                        transform: MaskTransform::SetNull,
                    },
                ],
            },
        };
        let empty = HashMap::new();
        let compiled = compile_step(
            &mask,
            &params(),
            &CompileMode::Preview { values: &empty },
            None,
        )
        .unwrap();
        assert!(compiled
            .sql
            .contains("\"email\" = 'user_' || \"id\"::text || '@test.local'"));
        assert!(compiled
            .sql
            .contains("encode(sha256(convert_to(COALESCE(\"ssn\"::text, ''), 'UTF8')), 'hex')"));
        assert!(compiled.sql.contains("\"phone\" = NULL"));
        assert!(compiled.sql.contains("WHERE \"age\" >= 18;"));

        let mut bad_vals = HashMap::new();
        bad_vals.insert("min_age".into(), "18; DROP TABLE users".into());
        let err = compile_step(
            &mask,
            &params(),
            &CompileMode::Preview { values: &bad_vals },
            None,
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config);
    }

    #[test]
    fn convert_to_sql_preserves_parameter_placeholders() {
        let tpl = starter_templates();
        let wf = &tpl[0].workflow;
        let converted = convert_step_to_sql(&wf.steps[1], &wf.parameters, None).unwrap();
        match converted.kind {
            StepKind::Sql {
                body: SqlStepBody::Inline { sql },
            } => {
                assert!(sql.contains("{{test_password}}"), "{sql}");
                assert!(sql.contains("INSERT INTO \"public\".\"users\""), "{sql}");
            }
            other => panic!("expected inline sql, got {other:?}"),
        }
    }

    #[test]
    fn sql_substitution_ignores_comments_and_warns_inside_quotes() {
        let step = WorkflowStep {
            id: "s5".into(),
            name: "Custom SQL".into(),
            enabled: true,
            no_transaction: false,
            kind: StepKind::Sql {
                body: SqlStepBody::Inline {
                    sql:
                        "-- {{admin_email}} in comment\nSELECT '{{admin_email}}', {{admin_email}};"
                            .into(),
                },
            },
        };
        let empty = HashMap::new();
        let compiled = compile_step(
            &step,
            &params(),
            &CompileMode::Preview { values: &empty },
            None,
        )
        .unwrap();
        assert!(compiled.sql.starts_with("-- {{admin_email}} in comment\n"));
        assert!(compiled
            .sql
            .contains("SELECT '{{admin_email}}', 'admin@test.local';"));
        assert_eq!(compiled.warnings.len(), 1);
    }
}
