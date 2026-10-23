//! A scripted, in-memory [`RefreshOps`] for state-machine tests: one fake target server
//! (databases with comments and sessions, prepared transactions) plus injectable failures
//! and hangs at every op.

#![allow(dead_code)]

use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag};
use pgwhy_core::conn::BoxFuture;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_workflow::history::{ScriptRecord, SessionInfo};
use pgwhy_workflow::ops::*;
use pgwhy_workflow::staging::LocaleSpec;
use pgwhy_workflow::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Default)]
pub struct FakeDb {
    pub comment: Option<String>,
    /// (pid, application_name, sticky — survives terminate, like an auto-reconnecting client)
    pub sessions: Vec<(i32, String, bool)>,
}

#[derive(Debug, Default)]
pub struct FakeServer {
    pub dbs: BTreeMap<String, FakeDb>,
    pub prepared: Vec<PreparedXact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Point {
    Dump,
    Inspect,
    Restore,
    Script,
    Verify,
    Maintenance,
    CreateDb,
    SetComment,
    /// The nth rename call (1-based).
    Rename(u32),
    DropDb,
    Probe,
}

/// The fake's shared state (the ops and every maintenance connection see the same server).
#[derive(Default)]
pub struct FakeState {
    pub server: Arc<Mutex<FakeServer>>,
    fails: Mutex<HashMap<Point, AppError>>,
    hangs: Mutex<HashSet<Point>>,
    pub calls: Mutex<Vec<String>>,
    renames: AtomicU32,
    pub quiesced: Mutex<Vec<Vec<String>>>,
    pub quiesce_active: Arc<AtomicI32>,
    pub verdict: Mutex<Option<BackupVerdict>>,
    pub source_down: AtomicBool,
    pub maint_down: AtomicBool,
    /// Fail every drop of this database (cleanup-required paths).
    pub undroppable: Mutex<Option<String>>,
    /// Renames (1-based) that commit on the server but whose reply is lost: the call returns a
    /// connection error after the database was already renamed.
    pub lose_rename_reply: Mutex<HashSet<u32>>,
}

/// The fake ops; derefs to its [`FakeState`].
/// A `pg_restore` the fake accepts (the fake never executes it).
pub fn fake_tool() -> pgwhy_pgtools::DiscoveredTool {
    pgwhy_pgtools::DiscoveredTool {
        kind: pgwhy_pgtools::ToolKind::PgRestore,
        path: "/usr/local/bin/pg_restore".into(),
        version: pgwhy_pgtools::PgVersion::parse("16.15").unwrap(),
        source: pgwhy_pgtools::discover::DiscoverySource::Path,
    }
}

/// A target entry for the standalone-restore paths (the fake ignores it).
pub fn fake_entry() -> ConnectionEntry {
    entry("local", 5001, EnvTag::Local)
}

pub struct FakeOps(pub Arc<FakeState>);

impl std::ops::Deref for FakeOps {
    type Target = FakeState;
    fn deref(&self) -> &FakeState {
        &self.0
    }
}

impl FakeOps {
    pub fn new() -> Arc<Self> {
        let f = FakeState::default();
        {
            let mut s = f.server.lock().unwrap();
            s.dbs.insert("postgres".into(), FakeDb::default());
            s.dbs.insert(
                "app".into(),
                FakeDb {
                    comment: Some("my app".into()),
                    sessions: vec![(4242, "psql".into(), false)],
                },
            );
        }
        Arc::new(FakeOps(Arc::new(f)))
    }
}

impl FakeState {
    /// One-shot failure at `p`.
    pub fn fail(&self, p: Point, e: AppError) {
        self.fails.lock().unwrap().insert(p, e);
    }

    /// `p` never completes (until cancelled, for cancellable ops).
    pub fn hang(&self, p: Point) {
        self.hangs.lock().unwrap().insert(p);
    }

    pub fn unhang(&self, p: Point) {
        self.hangs.lock().unwrap().remove(&p);
    }

    fn call(&self, s: impl Into<String>) {
        self.calls.lock().unwrap().push(s.into());
    }

    pub fn calls_matching(&self, prefix: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    fn take_fail(&self, p: Point) -> AppResult<()> {
        match self.fails.lock().unwrap().remove(&p) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    async fn gate(&self, p: Point, cancel: Option<&RunCancel>) -> AppResult<()> {
        if self.hangs.lock().unwrap().contains(&p) {
            match cancel {
                Some(c) => {
                    c.cancelled().await;
                    return Err(AppError::new(
                        ErrorKind::Cancelled,
                        format!("{p:?} was cancelled"),
                    ));
                }
                None => std::future::pending::<()>().await,
            }
        }
        self.take_fail(p)
    }

    pub fn db_names(&self) -> Vec<String> {
        self.server.lock().unwrap().dbs.keys().cloned().collect()
    }

    pub fn has_db(&self, name: &str) -> bool {
        self.server.lock().unwrap().dbs.contains_key(name)
    }

    pub fn comment(&self, name: &str) -> Option<String> {
        self.server
            .lock()
            .unwrap()
            .dbs
            .get(name)
            .and_then(|d| d.comment.clone())
    }

    pub fn add_session(&self, db: &str, pid: i32, app: &str, sticky: bool) {
        self.server
            .lock()
            .unwrap()
            .dbs
            .get_mut(db)
            .unwrap()
            .sessions
            .push((pid, app.into(), sticky));
    }
}

struct Token(Arc<AtomicI32>);
impl Drop for Token {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RefreshOps for FakeOps {
    fn dump<'a>(
        &'a self,
        _plan: &'a RunPlan,
        out: &'a Path,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<DumpDone>> {
        Box::pin(async move {
            self.call("dump");
            ctx.output("pg_dump: dumping contents of table \"public.t\"");
            self.gate(Point::Dump, Some(&ctx.cancel)).await?;
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            std::fs::write(out, b"fake dump").unwrap();
            std::fs::write(log, b"log").unwrap();
            Ok(DumpDone {
                path: out.to_path_buf(),
                size_bytes: 9,
                dumped_by: "16.15".into(),
                tool: ToolStepDone {
                    exit_code: Some(0),
                    output_tail: vec!["dumped".into()],
                    log_path: Some(log.to_path_buf()),
                    objects_done: 1,
                },
            })
        })
    }

    fn inspect_backup<'a>(
        &'a self,
        _plan: &'a RunPlan,
        path: &'a Path,
        _ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<BackupVerdict>> {
        Box::pin(async move {
            self.call("inspect");
            self.take_fail(Point::Inspect)?;
            if let Some(v) = self.verdict.lock().unwrap().clone() {
                return Ok(v);
            }
            Ok(BackupVerdict {
                reusable: path.exists(),
                reason: (!path.exists()).then(|| "missing".into()),
                cause: None,
                toc_items: Some(3),
                dumped_by: Some("16.15".into()),
            })
        })
    }

    fn restore<'a>(
        &'a self,
        _plan: &'a RunPlan,
        archive: &'a Path,
        into_db: &'a str,
        _total: Option<u64>,
        log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>> {
        Box::pin(async move {
            // reuse the standalone implementation with a placeholder tool/credential
            let tool = fake_tool();
            self.restore_archive(
                &fake_entry(),
                &tool,
                None,
                archive,
                into_db,
                &pgwhy_pgtools::RestoreOptions::default(),
                log,
                ctx,
            )
            .await
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn restore_archive<'a>(
        &'a self,
        _target: &'a ConnectionEntry,
        _tool: &'a pgwhy_pgtools::DiscoveredTool,
        _password: Option<&'a pgwhy_pgtools::ToolPassword>,
        _archive: &'a Path,
        into_db: &'a str,
        _opts: &'a pgwhy_pgtools::RestoreOptions,
        _log: &'a Path,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ToolStepDone>> {
        Box::pin(async move {
            self.call(format!("restore {into_db}"));
            if !self.has_db(into_db) {
                return Err(AppError::new(
                    ErrorKind::Config,
                    format!("database {into_db} does not exist"),
                ));
            }
            ctx.emit(OpEvent::Progress(StepProgress {
                objects_done: 1,
                objects_total: Some(3),
                ..Default::default()
            }));
            self.gate(Point::Restore, Some(&ctx.cancel)).await?;
            Ok(ToolStepDone {
                exit_code: Some(0),
                output_tail: vec!["restored".into()],
                log_path: None,
                objects_done: 3,
            })
        })
    }

    fn run_script<'a>(
        &'a self,
        _plan: &'a RunPlan,
        db: &'a str,
        script: &'a ScriptRecord,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<ScriptDone>> {
        Box::pin(async move {
            self.call(format!("script {db} {}", script.sha256));
            ctx.output("statement 1: UPDATE");
            self.gate(Point::Script, Some(&ctx.cancel)).await?;
            Ok(ScriptDone {
                statements: 1,
                output_tail: vec!["statement 1: UPDATE (1 rows)".into()],
            })
        })
    }

    fn verify<'a>(
        &'a self,
        _plan: &'a RunPlan,
        db: &'a str,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>> {
        Box::pin(async move { self.verify_database(&fake_entry(), db, None, ctx).await })
    }

    fn verify_database<'a>(
        &'a self,
        _target: &'a ConnectionEntry,
        db: &'a str,
        _sanity_query: Option<&'a str>,
        ctx: &'a OpCtx,
    ) -> BoxFuture<'a, AppResult<String>> {
        Box::pin(async move {
            self.call(format!("verify {db}"));
            self.gate(Point::Verify, Some(&ctx.cancel)).await?;
            Ok(format!("{db} has 1 user table(s)"))
        })
    }

    fn maintenance<'a>(
        &'a self,
        _target: &'a ConnectionId,
    ) -> BoxFuture<'a, AppResult<Box<dyn MaintenanceOps>>> {
        Box::pin(async move {
            if self.maint_down.load(Ordering::SeqCst) {
                return Err(AppError::new(
                    ErrorKind::Network,
                    "could not connect to the target server",
                ));
            }
            self.take_fail(Point::Maintenance)?;
            Ok(Box::new(FakeMaint { s: self.0.clone() }) as Box<dyn MaintenanceOps>)
        })
    }

    fn probe<'a>(&'a self, _conn: &'a ConnectionId, _db: &'a str) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if self.source_down.load(Ordering::SeqCst) {
                return Err(AppError::new(
                    ErrorKind::Network,
                    "the source is unreachable",
                ));
            }
            self.take_fail(Point::Probe)
        })
    }

    fn quiesce<'a>(
        &'a self,
        _target: &'a ConnectionId,
        dbs: Vec<String>,
    ) -> BoxFuture<'a, AppResult<QuiesceToken>> {
        Box::pin(async move {
            self.quiesced.lock().unwrap().push(dbs);
            self.quiesce_active.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Token(self.quiesce_active.clone())) as QuiesceToken)
        })
    }
}

/// Maintenance over the fake server.
struct FakeMaint {
    s: Arc<FakeState>,
}

impl FakeMaint {
    fn ops(&self) -> &FakeState {
        &self.s
    }
}

impl MaintenanceOps for FakeMaint {
    fn server_major(&self) -> u32 {
        16
    }

    fn is_closed(&self) -> bool {
        self.ops().maint_down.load(Ordering::SeqCst)
    }

    fn databases(&self) -> BoxFuture<'_, AppResult<Vec<DbFact>>> {
        Box::pin(async move {
            if self.is_closed() {
                return Err(AppError::new(ErrorKind::Disconnected, "closed"));
            }
            let s = self.ops().server.lock().unwrap();
            Ok(s.dbs
                .iter()
                .map(|(n, d)| DbFact {
                    name: n.clone(),
                    comment: d.comment.clone(),
                    owner: "pgwhy".into(),
                })
                .collect())
        })
    }

    fn create_database<'a>(
        &'a self,
        name: &'a str,
        _locale: Option<&'a LocaleSpec>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let o = self.ops();
            o.call(format!("create {name}"));
            o.gate(Point::CreateDb, None).await?;
            let mut s = o.server.lock().unwrap();
            if s.dbs.contains_key(name) {
                return Err(AppError::new(
                    ErrorKind::Sql,
                    format!("database \"{name}\" already exists"),
                ));
            }
            s.dbs.insert(name.to_string(), FakeDb::default());
            Ok(())
        })
    }

    fn set_comment<'a>(
        &'a self,
        db: &'a str,
        comment: Option<&'a str>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let o = self.ops();
            o.call(format!("comment {db}"));
            o.gate(Point::SetComment, None).await?;
            let mut s = o.server.lock().unwrap();
            let d = s
                .dbs
                .get_mut(db)
                .ok_or_else(|| AppError::new(ErrorKind::Sql, "no such database"))?;
            d.comment = comment.map(str::to_string);
            Ok(())
        })
    }

    fn rename_database<'a>(&'a self, from: &'a str, to: &'a str) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let o = self.ops();
            let n = o.renames.fetch_add(1, Ordering::SeqCst) + 1;
            o.call(format!("rename {from} {to}"));
            o.gate(Point::Rename(n), None).await?;
            let mut s = o.server.lock().unwrap();
            if s.dbs.contains_key(to) {
                return Err(AppError::new(
                    ErrorKind::Sql,
                    format!("database \"{to}\" already exists"),
                ));
            }
            let d = s
                .dbs
                .get(from)
                .ok_or_else(|| AppError::new(ErrorKind::Sql, "no such database"))?;
            if !d.sessions.is_empty() {
                return Err(object_in_use_error(from));
            }
            let d = s.dbs.remove(from).unwrap();
            s.dbs.insert(to.to_string(), d);
            if o.lose_rename_reply.lock().unwrap().remove(&n) {
                return Err(AppError::new(
                    ErrorKind::Disconnected,
                    "connection closed before the rename was acknowledged",
                ));
            }
            Ok(())
        })
    }

    fn drop_database<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let o = self.ops();
            o.call(format!("drop {name}"));
            o.gate(Point::DropDb, None).await?;
            if o.undroppable.lock().unwrap().as_deref() == Some(name) {
                return Err(AppError::new(
                    ErrorKind::Permission,
                    format!("must be owner of database {name}"),
                ));
            }
            let mut s = o.server.lock().unwrap();
            if s.dbs.get(name).is_some_and(|d| !d.sessions.is_empty()) {
                return Err(object_in_use_error(name));
            }
            s.dbs.remove(name);
            Ok(())
        })
    }

    fn prepared_xacts<'a>(&'a self, db: &'a str) -> BoxFuture<'a, AppResult<Vec<PreparedXact>>> {
        Box::pin(async move {
            Ok(self
                .ops()
                .server
                .lock()
                .unwrap()
                .prepared
                .iter()
                .filter(|p| p.database == db)
                .cloned()
                .collect())
        })
    }

    fn sessions<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<Vec<SessionInfo>>> {
        Box::pin(async move {
            let s = self.ops().server.lock().unwrap();
            let mut out = Vec::new();
            for db in dbs {
                if let Some(d) = s.dbs.get(db) {
                    for (pid, app, _) in &d.sessions {
                        out.push(SessionInfo {
                            pid: *pid,
                            database: db.clone(),
                            user: Some("app".into()),
                            application_name: Some(app.clone()),
                            client_addr: None,
                            state: Some("idle".into()),
                        });
                    }
                }
            }
            Ok(out)
        })
    }

    fn terminate<'a>(&'a self, dbs: &'a [String]) -> BoxFuture<'a, AppResult<u32>> {
        Box::pin(async move {
            let mut s = self.ops().server.lock().unwrap();
            let mut n = 0;
            for db in dbs {
                if let Some(d) = s.dbs.get_mut(db) {
                    let before = d.sessions.len();
                    d.sessions.retain(|(_, _, sticky)| *sticky);
                    n += (before - d.sessions.len()) as u32;
                }
            }
            Ok(n)
        })
    }
}

pub fn entry(name: &str, port: u16, tag: EnvTag) -> ConnectionEntry {
    let mut e: ConnectionEntry = serde_json::from_value(serde_json::json!({
        "name": name, "host": "localhost", "port": port, "database": "app", "user": "pgwhy"
    }))
    .unwrap();
    e.env_tag = tag;
    e
}

pub fn workflow(
    source: &ConnectionEntry,
    target: &ConnectionEntry,
    strategy: ReplaceStrategy,
) -> RefreshWorkflow {
    RefreshWorkflow {
        id: WorkflowId("wf-1".into()),
        name: "Refresh app".into(),
        source: DbRef {
            connection: source.id.clone(),
            database: "app".into(),
        },
        target: DbRef {
            connection: target.id.clone(),
            database: "app".into(),
        },
        post_restore_script: None,
        options: RefreshOptions {
            strategy,
            ..RefreshOptions::default()
        },
        remembered_confirmation: None,
        has_succeeded: false,
    }
}

pub struct Harness {
    pub fake: Arc<FakeOps>,
    pub engine: Engine,
    pub events: Arc<Mutex<Vec<RunEvent>>>,
    pub dir: tempfile::TempDir,
    pub source: ConnectionEntry,
    pub target: ConnectionEntry,
}

impl Harness {
    pub fn new() -> Self {
        let fake = FakeOps::new();
        let dir = tempfile::tempdir().unwrap();
        let mut config = EngineConfig::new(dir.path().join("runs"), dir.path().join("backups"));
        config.session_drain_timeout = Duration::from_millis(400);
        let engine = Engine::with_ops(config, fake.clone(), None);
        Self {
            fake,
            engine,
            events: Arc::default(),
            dir,
            source: entry("remote", 5000, EnvTag::Production),
            target: entry("local", 5001, EnvTag::Local),
        }
    }

    pub fn plan(&self, strategy: ReplaceStrategy) -> RunPlan {
        let wf = workflow(&self.source, &self.target, strategy);
        let mut plan = RunPlan::new(
            wf,
            self.source.clone(),
            self.target.clone(),
            self.dir.path().join("backups").join("wf-1"),
            SystemTime::now(),
        );
        plan.create_mode = !self.fake.has_db("app");
        plan
    }

    pub fn with_script(plan: &mut RunPlan, mode: ScriptMode) {
        plan.workflow.post_restore_script = Some("script-1".into());
        plan.workflow.options.script_mode = mode;
        plan.script = Some(ScriptRecord {
            id: "script-1".into(),
            name: "anonymize".into(),
            mode,
            sha256: "abc123".into(),
            content: "update users set email = null;".into(),
        });
    }

    pub fn sink(&self) -> EventSink {
        let ev = self.events.clone();
        Arc::new(move |e| ev.lock().unwrap().push(e))
    }

    pub async fn run(&self, plan: RunPlan) -> RunRecord {
        let id = self.engine.start_plan(plan, self.sink()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), self.engine.wait(&id))
            .await
            .expect("run finished")
            .unwrap()
    }

    pub fn event_kinds(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

pub fn kinds(rec: &RunRecord) -> Vec<ActionKind> {
    rec.actions.iter().map(|a| a.kind).collect()
}

pub fn enabled_kinds(rec: &RunRecord) -> Vec<ActionKind> {
    rec.actions
        .iter()
        .filter(|a| a.enabled)
        .map(|a| a.kind)
        .collect()
}

pub fn retry_offer(rec: &RunRecord, from: StepId) -> Option<&ActionOffer> {
    rec.actions
        .iter()
        .find(|a| a.retry.as_ref().is_some_and(|r| r.from == from))
}
