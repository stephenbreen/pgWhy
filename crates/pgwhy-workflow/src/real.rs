//! The production [`RefreshOps`]: app-side connections through the [`ConnectionManager`]
//! (maintenance, post-restore, verify, quiesce) and pg_dump/pg_restore children through
//! pgwhy-pgtools. Children reach tunneled servers via a [`TunnelProvider`] lease held for the
//! child's lifetime, as `host=<real> hostaddr=127.0.0.1 port=<lease>` — never a password in
//! argv; credentials travel only through the per-run temp pgpass.

use crate::definition::{ErrorPolicy, ScriptMode};
use crate::history::{ScriptRecord, SessionInfo};
use crate::ops::*;
use crate::staging::{self, ident, quote_literal, LocaleSpec};
use pgwhy_core::config::{ConnectionEntry, ConnectionId};
use pgwhy_core::conn::{
    classify, BoxFuture, ConnectionManager, Endpoint, PgConn, Purpose, TunnelLease, TunnelProvider,
};
use pgwhy_core::query::{QuerySession, ResultSink, SqlError, StatementDone};
use pgwhy_core::scripts::Library;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_pgtools::{
    BackupOptions, DiscoveryOptions, DumpFormat, PgPassDir, ProgressEvent, RestoreOptions,
    RunContext, ToolEndpoint, ToolPassword,
};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A post-restore script loaded by stable id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedScript {
    pub id: String,
    pub name: String,
    pub content: String,
    pub sha256: String,
}

/// Where scripts come from (U14's library, behind whatever lock the app uses).
pub trait ScriptSource: Send + Sync {
    fn load(&self, id: &str) -> AppResult<LoadedScript>;
}

impl<F> ScriptSource for F
where
    F: Fn(&str) -> AppResult<LoadedScript> + Send + Sync,
{
    fn load(&self, id: &str) -> AppResult<LoadedScript> {
        self(id)
    }
}

fn load_from_library(lib: &Library, id: &str) -> AppResult<LoadedScript> {
    let sid = pgwhy_core::scripts::ScriptId(id.to_string());
    let info = lib.get(&sid).ok_or_else(|| {
        AppError::new(
            ErrorKind::Config,
            "the post-restore script is no longer in the script library",
        )
        .with_hint("attach a script again in the workflow editor")
    })?;
    let file = lib.read(&sid)?;
    Ok(LoadedScript {
        id: id.to_string(),
        name: info.name,
        content: file.content,
        sha256: file.fingerprint.sha256,
    })
}

impl ScriptSource for Mutex<Library> {
    fn load(&self, id: &str) -> AppResult<LoadedScript> {
        load_from_library(&self.lock().unwrap_or_else(|p| p.into_inner()), id)
    }
}

impl ScriptSource for std::sync::RwLock<Library> {
    fn load(&self, id: &str) -> AppResult<LoadedScript> {
        load_from_library(&self.read().unwrap_or_else(|p| p.into_inner()), id)
    }
}

/// What the real engine needs from the app.
#[derive(Clone)]
pub struct RealDeps {
    pub manager: ConnectionManager,
    /// For pg_dump/pg_restore children of tunneled entries (the core `TunnelManager`).
    pub tunnels: Arc<dyn TunnelProvider>,
    pub scripts: Arc<dyn ScriptSource>,
    pub discovery: DiscoveryOptions,
    pub pgpass_dir: PgPassDir,
}

/// Production ops.
#[derive(Clone)]
pub struct RealOps {
    deps: Arc<RealDeps>,
}

impl RealOps {
    pub fn new(deps: Arc<RealDeps>) -> Self {
        Self { deps }
    }

    async fn lease(&self, entry: &ConnectionEntry) -> AppResult<Option<TunnelLease>> {
        match entry.tunnel {
            Some(_) => Ok(Some(self.deps.tunnels.acquire(entry).await?)),
            None => Ok(None),
        }
    }

    fn run_context(&self, ctx: &OpCtx) -> RunContext {
        RunContext {
            pgpass_dir: self.deps.pgpass_dir.clone(),
            cancel: ctx.cancel.tools().clone(),
        }
    }
}

/// The child endpoint for `entry`/`db` (tunnel split when leased).
pub fn tool_endpoint(
    entry: &ConnectionEntry,
    lease: Option<&TunnelLease>,
    db: &str,
) -> ToolEndpoint {
    let ep = Endpoint::for_entry(entry, lease);
    ToolEndpoint {
        host: ep.host,
        hostaddr: ep.hostaddr,
        port: ep.port,
        user: entry.user.clone(),
        dbname: db.to_string(),
        sslmode: entry.sslmode.as_libpq().to_string(),
        sslrootcert: entry.ssl_root_cert.clone(),
    }
}

/// `schema.table` → an exact psql pattern; anything already quoted or wildcarded is passed
/// through as the user wrote it.
pub fn exclude_pattern(spec: &str) -> String {
    if spec.contains(['"', '*', '?']) {
        return spec.to_string();
    }
    match spec.split_once('.') {
        Some((s, t)) => pgwhy_pgtools::table_pattern(s, t),
        None => spec.to_string(),
    }
}

fn progress_of(p: &ProgressEvent) -> StepProgress {
    StepProgress {
        phase: Some(format!("{:?}", p.phase).to_lowercase()),
        current_object: p.current_object.clone(),
        objects_done: p.objects_done,
        objects_total: p.objects_total,
        total_is_estimate: p.total_is_estimate,
        bytes_written: p.bytes_written,
        elapsed_ms: p.elapsed_ms,
    }
}

fn forward(ctx: &OpCtx) -> impl FnMut(ProgressEvent) + Send + '_ {
    let mut last: Option<String> = None;
    move |p: ProgressEvent| {
        if let Some(line) = &p.last_line {
            if last.as_ref() != Some(line) {
                ctx.output(line.clone());
                last = Some(line.clone());
            }
        }
        ctx.emit(OpEvent::Progress(progress_of(&p)));
    }
}

fn tail(lines: &[String], n: usize) -> Vec<String> {
    lines[lines.len().saturating_sub(n)..].to_vec()
}

const TAIL: usize = 60;

/// Script messages → run output.
struct ScriptSink<'a> {
    ctx: &'a OpCtx,
    lines: VecDeque<String>,
}

impl ScriptSink<'_> {
    fn push(&mut self, line: String) {
        self.ctx.output(line.clone());
        self.lines.push_back(line);
        while self.lines.len() > TAIL {
            self.lines.pop_front();
        }
    }
}

impl ResultSink for ScriptSink<'_> {
    fn notice(&mut self, _index: Option<usize>, notice: &pgwhy_core::conn::Notice) {
        self.push(format!("{}: {}", notice.severity, notice.message));
    }
    fn completed(&mut self, index: usize, done: &StatementDone) {
        self.push(format!(
            "statement {}: {} ({} rows)",
            index + 1,
            done.command,
            done.rows_affected
        ));
    }
    fn failed(&mut self, index: usize, error: &SqlError) {
        self.push(format!("statement {} failed: {}", index + 1, error.message));
    }
}

const DEFAULT_SANITY: &str = "select count(*) from pg_catalog.pg_class c \
    join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
    where c.relkind in ('r','p') and n.nspname not in ('pg_catalog','information_schema') \
    and n.nspname not like 'pg\\_toast%' and n.nspname not like 'pg\\_temp%'";

impl RefreshOps for RealOps {
    fn dump<'a>(
        &'a self,
        plan: &'a RunPlan,
        out: &'a Path,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<DumpDone>> {
        Box::pin(async move {
            let tools = plan
                .tools
                .as_ref()
                .ok_or_else(|| AppError::internal("no pg_dump selected"))?;
            let lease = self.lease(&plan.source).await?;
            let ep = tool_endpoint(&plan.source, lease.as_ref(), plan.source_db());
            let opts = BackupOptions {
                format: DumpFormat::Custom,
                exclude_table_data: plan
                    .workflow
                    .options
                    .exclude_table_data
                    .iter()
                    .map(|s| exclude_pattern(s))
                    .collect(),
                jobs: None,
                estimated_objects: plan.estimated_objects,
                // The Refresh pipeline dumps the whole source (minus the excluded tables' data).
                schemas: Vec::new(),
                tables: Vec::new(),
                schema_only: false,
                data_only: false,
                log_path: Some(log.to_path_buf()),
            };
            let rc = self.run_context(ctx);
            let outcome = pgwhy_pgtools::backup(
                &tools.pg_dump,
                &ep,
                plan.source_password.as_ref(),
                out,
                &opts,
                &rc,
                forward(ctx),
            )
            .await?;
            drop(lease);
            Ok(DumpDone {
                path: outcome.path,
                size_bytes: outcome.size_bytes,
                dumped_by: outcome.dumped_by.raw.clone(),
                tool: ToolStepDone {
                    exit_code: outcome.run.exit_code,
                    output_tail: tail(&outcome.run.stderr_tail, TAIL),
                    log_path: outcome.run.log_path.clone(),
                    objects_done: outcome.run.objects_done,
                },
            })
        })
    }

    fn inspect_backup<'a>(
        &'a self,
        plan: &'a RunPlan,
        path: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<BackupVerdict>> {
        Box::pin(async move {
            let tools = plan
                .tools
                .as_ref()
                .ok_or_else(|| AppError::internal("no pg_restore selected"))?;
            let rc = self.run_context(ctx);
            let insp = pgwhy_pgtools::inspect_archive(
                &tools.pg_restore,
                path,
                pgwhy_pgtools::ArchiveCheck::TocOnly,
                &rc,
            )
            .await?;
            Ok(BackupVerdict {
                reusable: insp.reusable,
                reason: insp.reason,
                cause: insp.cause,
                toc_items: insp
                    .toc
                    .as_ref()
                    .map(|t| t.restorable_count(plan.workflow.options.no_privileges)),
                dumped_by: insp
                    .toc
                    .as_ref()
                    .and_then(|t| t.header.dumped_by.as_ref())
                    .map(|v| v.raw.clone()),
            })
        })
    }

    fn restore<'a>(
        &'a self,
        plan: &'a RunPlan,
        archive: &'a Path,
        into_db: &'a str,
        total_items: Option<u64>,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>> {
        Box::pin(async move {
            let tools = plan
                .tools
                .as_ref()
                .ok_or_else(|| AppError::internal("no pg_restore selected"))?;
            let o = &plan.workflow.options;
            let opts = RestoreOptions {
                no_owner: o.no_owner,
                no_privileges: o.no_privileges,
                jobs: Some(o.effective_jobs()),
                error_policy: match o.error_policy {
                    ErrorPolicy::StopOnFirst => pgwhy_pgtools::ErrorPolicy::ExitOnError,
                    ErrorPolicy::ContinueThenFail => pgwhy_pgtools::ErrorPolicy::ContinueOnError,
                },
                total_items,
                log_path: Some(log.to_path_buf()),
            };
            self.restore_archive(
                &plan.target,
                &tools.pg_restore,
                plan.target_password.as_ref(),
                archive,
                into_db,
                &opts,
                log,
                ctx,
            )
            .await
        })
    }

    fn restore_archive<'a>(
        &'a self,
        target: &'a ConnectionEntry,
        tool: &'a pgwhy_pgtools::DiscoveredTool,
        password: Option<&'a ToolPassword>,
        archive: &'a Path,
        into_db: &'a str,
        opts: &'a RestoreOptions,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>> {
        Box::pin(async move {
            let lease = self.lease(target).await?;
            let ep = tool_endpoint(target, lease.as_ref(), into_db);
            let opts = RestoreOptions {
                log_path: Some(log.to_path_buf()),
                ..opts.clone()
            };
            let rc = self.run_context(ctx);
            let outcome =
                pgwhy_pgtools::restore(tool, &ep, password, archive, &opts, &rc, forward(ctx))
                    .await?;
            drop(lease);
            Ok(ToolStepDone {
                exit_code: outcome.run.exit_code,
                output_tail: tail(&outcome.run.stderr_tail, TAIL),
                log_path: outcome.run.log_path.clone(),
                objects_done: outcome.run.objects_done,
            })
        })
    }

    fn run_script<'a>(
        &'a self,
        plan: &'a RunPlan,
        db: &'a str,
        script: &'a ScriptRecord,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ScriptDone>> {
        Box::pin(async move {
            let conn = self
                .deps
                .manager
                .open(&plan.target.id, db, Purpose::Workflow)
                .await?;
            let session = QuerySession::new(conn);
            let mode = match script.mode {
                ScriptMode::Transaction => pgwhy_core::query::ScriptMode::SingleTransaction,
                ScriptMode::NoTransaction => pgwhy_core::query::ScriptMode::NoTransaction,
            };
            let mut sink = ScriptSink {
                ctx,
                lines: VecDeque::new(),
            };
            let cancel = session.cancel_handle();
            let report = {
                let fut = session.run_script(&script.content, mode, &mut sink);
                tokio::pin!(fut);
                tokio::select! {
                    r = &mut fut => r,
                    _ = ctx.cancel.cancelled() => {
                        let _ = cancel.cancel().await;
                        fut.await
                    }
                }
            }?;
            let output_tail: Vec<String> = sink.lines.into_iter().collect();
            if ctx.cancel.is_cancelled() && !report.committed {
                return Err(AppError::new(
                    ErrorKind::Cancelled,
                    "the post-restore script was cancelled",
                ));
            }
            if let Some(mut err) = report.error() {
                let mut data = err.data.take().unwrap_or_else(|| serde_json::json!({}));
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("output_tail".into(), serde_json::json!(output_tail));
                }
                return Err(err.with_data(data));
            }
            Ok(ScriptDone {
                statements: report.summary.total,
                output_tail,
            })
        })
    }

    fn verify<'a>(
        &'a self,
        plan: &'a RunPlan,
        db: &'a str,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>> {
        Box::pin(async move {
            self.verify_database(
                &plan.target,
                db,
                plan.workflow.options.sanity_query.as_deref(),
                ctx,
            )
            .await
        })
    }

    fn verify_database<'a>(
        &'a self,
        target: &'a ConnectionEntry,
        db: &'a str,
        sanity_query: Option<&'a str>,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>> {
        Box::pin(async move {
            let conn = self
                .deps
                .manager
                .open(&target.id, db, Purpose::Workflow)
                .await?;
            let result = match sanity_query {
                Some(q) => {
                    ctx.output("running the workflow's sanity query");
                    conn.client
                        .simple_query(q)
                        .await
                        .map(|_| "the sanity query succeeded".to_string())
                        .map_err(|e| {
                            let mut err = classify(&e, "run the sanity query");
                            err.message = format!("verification failed: {}", err.message);
                            err
                        })
                }
                None => {
                    let n: i64 = conn
                        .client
                        .query_one(DEFAULT_SANITY, &[])
                        .await
                        .map_err(|e| classify(&e, "run the verification query"))?
                        .get(0);
                    if n > 0 {
                        Ok(format!("{db} has {n} user table(s)"))
                    } else {
                        Err(AppError::new(ErrorKind::Sql, format!("verification failed: {db} has no user tables"))
                            .with_hint("set a sanity query on the workflow if the source legitimately has no tables"))
                    }
                }
            };
            // Closed before the swap: a verify connection must not hold staging open.
            drop(conn);
            result
        })
    }

    fn maintenance<'a>(
        &'a self,
        target: &'a ConnectionId,
    ) -> BoxFuture<'a, AppResult<Box<dyn MaintenanceOps>>> {
        Box::pin(async move {
            let entry = self.deps.manager.entry(target)?;
            let conn = self
                .deps
                .manager
                .open(target, &entry.maintenance_db, Purpose::Maintenance)
                .await?;
            Ok(Box::new(RealMaintenance { conn }) as Box<dyn MaintenanceOps>)
        })
    }

    fn probe<'a>(&'a self, conn: &'a ConnectionId, db: &'a str) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let c = self.deps.manager.open(conn, db, Purpose::Workflow).await?;
            drop(c);
            Ok(())
        })
    }

    fn quiesce<'a>(
        &'a self,
        target: &'a ConnectionId,
        dbs: Vec<String>,
    ) -> BoxFuture<'a, AppResult<QuiesceToken>> {
        Box::pin(async move {
            let entry = self.deps.manager.entry(target)?;
            let guard = self.deps.manager.quiesce(&entry.server_key(), &dbs).await;
            Ok(Box::new(guard) as QuiesceToken)
        })
    }
}

/// Maintenance statements over one dedicated connection.
pub struct RealMaintenance {
    conn: PgConn,
}

impl RealMaintenance {
    pub fn new(conn: PgConn) -> Self {
        Self { conn }
    }

    pub fn conn(&self) -> &PgConn {
        &self.conn
    }

    async fn exec(&self, sql: String, what: &str) -> AppResult<()> {
        self.conn
            .client
            .batch_execute(&sql)
            .await
            .map_err(|e| sql_err(&e, what))
    }
}

/// Classify and keep the SQLSTATE in `data` (the swap loop keys on 55006).
fn sql_err(e: &tokio_postgres::Error, what: &str) -> AppError {
    let mut err = classify(e, what);
    if let Some(code) = e.code() {
        err = err.with_data(serde_json::json!({ "sqlstate": code.code() }));
        if code.code() == SQLSTATE_OBJECT_IN_USE {
            if let Some(db) = e.as_db_error() {
                err.message = db.message().to_string();
            }
        }
    }
    err
}

impl MaintenanceOps for RealMaintenance {
    fn server_major(&self) -> u32 {
        self.conn.info.server_major()
    }

    fn is_closed(&self) -> bool {
        self.conn.is_closed()
    }

    fn databases(&self) -> BoxFuture<'_, AppResult<Vec<DbFact>>> {
        Box::pin(async move {
            let rows = self
                .conn
                .client
                .query(
                    "select d.datname::text, shobj_description(d.oid, 'pg_database'), pg_get_userbyid(d.datdba)::text \
                     from pg_catalog.pg_database d order by 1",
                    &[],
                )
                .await
                .map_err(|e| sql_err(&e, "list databases"))?;
            Ok(rows
                .iter()
                .map(|r| DbFact {
                    name: r.get(0),
                    comment: r.get(1),
                    owner: r.get(2),
                })
                .collect())
        })
    }

    fn create_database<'a>(
        &'a self,
        name: &'a str,
        locale: Option<&'a LocaleSpec>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let sql = match locale {
                Some(l) => staging::create_database_sql(name, l, self.server_major()),
                None => format!("CREATE DATABASE {} TEMPLATE template0", ident(name)),
            };
            self.exec(sql, &format!("create database {name}")).await
        })
    }

    fn set_comment<'a>(
        &'a self,
        db: &'a str,
        comment: Option<&'a str>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let value = comment.map(quote_literal).unwrap_or_else(|| "NULL".into());
            self.exec(
                format!("COMMENT ON DATABASE {} IS {value}", ident(db)),
                &format!("mark database {db}"),
            )
            .await
        })
    }

    fn rename_database<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.exec(
                format!("ALTER DATABASE {} RENAME TO {}", ident(from), ident(to)),
                &format!("rename database {from} to {to}"),
            )
            .await
        })
    }

    fn drop_database<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.exec(
                format!("DROP DATABASE IF EXISTS {}", ident(name)),
                &format!("drop database {name}"),
            )
            .await
        })
    }

    fn prepared_xacts<'a>(&'a self, db: &'a str) -> BoxFuture<'a, AppResult<Vec<PreparedXact>>> {
        Box::pin(async move {
            let rows = self
                .conn
                .client
                .query(
                    "select gid, database::text, owner::text, prepared::text from pg_catalog.pg_prepared_xacts \
                     where database = $1 order by prepared",
                    &[&db],
                )
                .await
                .map_err(|e| sql_err(&e, "list prepared transactions"))?;
            Ok(rows
                .iter()
                .map(|r| PreparedXact {
                    gid: r.get(0),
                    database: r.get(1),
                    owner: r.get(2),
                    prepared_at: r.get(3),
                })
                .collect())
        })
    }

    fn sessions<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<Vec<SessionInfo>>> {
        Box::pin(async move {
            let dbs: Vec<String> = dbs.to_vec();
            let rows = self
                .conn
                .client
                .query(
                    "select pid, datname::text, usename::text, application_name, client_addr::text, state \
                     from pg_catalog.pg_stat_activity where datname::text = any($1) and pid <> pg_backend_pid() \
                     order by pid",
                    &[&dbs],
                )
                .await
                .map_err(|e| sql_err(&e, "list sessions"))?;
            Ok(rows
                .iter()
                .map(|r| SessionInfo {
                    pid: r.get(0),
                    database: r.get(1),
                    user: r.get(2),
                    application_name: r.get(3),
                    client_addr: r.get(4),
                    state: r.get(5),
                })
                .collect())
        })
    }

    fn terminate<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<u32>> {
        Box::pin(async move {
            let dbs: Vec<String> = dbs.to_vec();
            let n: i64 = self
                .conn
                .client
                .query_one(
                    "select count(*) filter (where pg_terminate_backend(pid)) from pg_catalog.pg_stat_activity \
                     where datname::text = any($1) and pid <> pg_backend_pid()",
                    &[&dbs],
                )
                .await
                .map_err(|e| sql_err(&e, "end sessions"))?
                .get(0);
            Ok(n.max(0) as u32)
        })
    }
}
