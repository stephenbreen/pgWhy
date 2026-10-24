//! Preflight (SM-2): the ordered gate sequence that resolves everything before the single
//! confirmation, so a run never prompts mid-way. Nothing destructive happens here.
//!
//! Order: run lock (per target server) → R18 tag re-check against the *current* entries →
//! tools → source reachable (+ live catalog facts) → version gates → maintenance DB +
//! privileges → maintenance ≠ target → prepared transactions → disk space → staging/`_old`
//! names (marker-verified) → encoding/locale representability → post-restore script (R22) →
//! credentials (EC-10). Version gates follow the source check because they need its major.
//!
//! Every gate produces a [`PreflightCheck`]; a failing one stops the run before the
//! confirmation (`refused`), and fixable findings carry a [`PreflightFix`].

use crate::definition::{self, RefreshWorkflow, ReplaceStrategy, ScriptMode, WorkflowId};
use crate::engine::Inner;
use crate::history::{Inventory, RetryOffer, RunRecord, ScriptRecord, StepId};
use crate::lock::RunLockGuard;
use crate::ops::{OpCtx, PlannedTools, Resume, RunCancel, RunPlan};
use crate::real::{RealDeps, RealMaintenance};
use crate::staging::{LocaleProvider, LocaleSpec, Marker, MarkerRole};
use pgwhy_core::config::{ConnectionEntry, ConnectionId, EnvTag, TunnelAuth};
use pgwhy_core::conn::{PgConn, Purpose};
use pgwhy_core::secret::{CredentialSource, SecretKey, SecretKind};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use pgwhy_pgtools::{PgVersion, ToolKind, ToolPassword};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    /// Not applicable, or not checkable because an earlier gate failed.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightFixKind {
    /// Drop a pgWhy-marked leftover occupying the staging or `_old` name.
    DropLeftover,
    /// `ROLLBACK PREPARED` a prepared transaction on the target.
    RollbackPrepared,
}

/// A one-click fix the UI can offer next to a failed check ([`crate::Engine::apply_preflight_fix`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightFix {
    pub kind: PreflightFixKind,
    pub database: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightCheck {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixes: Vec<PreflightFix>,
}

impl PreflightCheck {
    fn new(
        id: &str,
        label: &str,
        status: CheckStatus,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            status,
            message: message.into(),
            hint,
            fixes: Vec::new(),
        }
    }
    pub(crate) fn ok(id: &str, label: &str, message: impl Into<String>) -> Self {
        Self::new(id, label, CheckStatus::Ok, message, None)
    }
    pub(crate) fn warn(
        id: &str,
        label: &str,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::new(id, label, CheckStatus::Warn, message, hint)
    }
    pub(crate) fn fail(
        id: &str,
        label: &str,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self::new(id, label, CheckStatus::Fail, message, hint)
    }
    fn from_error(id: &str, label: &str, e: &AppError) -> Self {
        Self::fail(id, label, e.message.clone(), e.hint.clone())
    }
    pub(crate) fn skipped(id: &str, label: &str, message: impl Into<String>) -> Self {
        Self::new(id, label, CheckStatus::Skipped, message, None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretNeed {
    DbPassword,
    SshKeyPassphrase,
    SshPassword,
}

/// A credential the UI must prompt for (then save to the vault and re-run preflight).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingSecret {
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub kind: SecretNeed,
    pub reason: String,
}

/// What the single confirmation shows (R14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub target_db: String,
    /// "Local PG (localhost:5432)".
    pub target_server_label: String,
    pub target_tag: EnvTag,
    pub strategy: ReplaceStrategy,
    /// The target doesn't exist yet and will be created.
    pub create_mode: bool,
    pub source_label: String,
    pub source_db: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging_db: Option<String>,
    pub warnings: Vec<String>,
    /// A remembered confirmation applies (the UI may skip the dialog).
    pub remembered: bool,
    /// "Remember" may be offered (Local target, after a first successful run).
    pub can_remember: bool,
    /// Echo this back in [`crate::StartRequest`].
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreflightReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<WorkflowId>,
    /// Pass to [`crate::Engine::start`] after the user confirms. Present iff nothing failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    pub checks: Vec<PreflightCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<Confirmation>,
    #[serde(default)]
    pub missing_secrets: Vec<MissingSecret>,
    /// The first failing gate's error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<AppError>,
    /// For a retry: what it will reuse and where it begins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryOffer>,
}

impl PreflightReport {
    pub(crate) fn refused_early(wf: &RefreshWorkflow, e: AppError) -> Self {
        let mut r = Self::refused_bare(e);
        r.workflow_id = Some(wf.id.clone());
        r
    }

    pub(crate) fn refused_bare(e: AppError) -> Self {
        Self {
            workflow_id: None,
            ticket: None,
            checks: Vec::new(),
            confirmation: None,
            missing_secrets: Vec::new(),
            refused: Some(e),
            retry: None,
        }
    }

    pub fn check(&self, id: &str) -> Option<&PreflightCheck> {
        self.checks.iter().find(|c| c.id == id)
    }
}

/// A retry's inputs (validated by [`crate::retry::plan_retry`]).
pub(crate) struct RetryContext {
    pub record: RunRecord,
    pub offer: RetryOffer,
    pub inventory: Inventory,
}

struct SourceFacts {
    version_num: i32,
    version: String,
    size: i64,
    locale: LocaleSpec,
    tables: i64,
}

async fn source_facts(conn: &PgConn) -> AppResult<SourceFacts> {
    let c = &conn.client;
    let ctx = |e: tokio_postgres::Error| pgwhy_core::conn::classify(&e, "read the source catalog");
    let row = c
        .query_one(
            "select current_setting('server_version_num')::int4, current_setting('server_version'), \
             pg_database_size(current_database())",
            &[],
        )
        .await
        .map_err(ctx)?;
    let version_num: i32 = row.get(0);
    let l = c
        .query_one(
            &crate::staging::locale_sql(version_num, "datname = current_database()"),
            &[],
        )
        .await
        .map_err(ctx)?;
    let locale = crate::staging::locale_from_row(&l);
    let tables: i64 = c
        .query_one(
            "select count(*) from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
             where c.relkind in ('r','m') and n.nspname not in ('pg_catalog','information_schema') \
             and n.nspname not like 'pg\\_toast%'",
            &[],
        )
        .await
        .map_err(ctx)?
        .get(0);
    Ok(SourceFacts {
        version_num,
        version: row.get(1),
        size: row.get(2),
        locale,
        tables,
    })
}

struct TargetFacts {
    version_num: i32,
    version: String,
    superuser: bool,
    createdb: bool,
    can_signal: bool,
    owns_target: Option<bool>,
    target_size: Option<i64>,
    data_directory: Option<String>,
}

async fn target_facts(conn: &PgConn, target_db: &str) -> AppResult<TargetFacts> {
    let c = &conn.client;
    let ctx = |e: tokio_postgres::Error| {
        pgwhy_core::conn::classify(&e, "read the target server's catalog")
    };
    let r = c
        .query_one(
            "select current_setting('server_version_num')::int4, current_setting('server_version'), \
             r.rolsuper, r.rolcreatedb, pg_has_role(current_user, 'pg_signal_backend', 'MEMBER') \
             from pg_catalog.pg_roles r where r.rolname = current_user",
            &[],
        )
        .await
        .map_err(ctx)?;
    let t = c
        .query_opt(
            "select pg_has_role(current_user, d.datdba, 'MEMBER'), pg_database_size(d.oid) \
             from pg_catalog.pg_database d where d.datname = $1",
            &[&target_db],
        )
        .await
        .map_err(ctx)?;
    let data_directory = c
        .query_one("select current_setting('data_directory', true)", &[])
        .await
        .ok()
        .and_then(|r| r.get::<_, Option<String>>(0));
    Ok(TargetFacts {
        version_num: r.get(0),
        version: r.get(1),
        superuser: r.get(2),
        createdb: r.get(3),
        can_signal: r.get(4),
        owns_target: t.as_ref().map(|t| t.get(0)),
        target_size: t.as_ref().map(|t| t.get(1)),
        data_directory,
    })
}

/// Can `locale` be created on the target? `Err(gap)` names exactly what is missing.
async fn locale_gap(
    conn: &PgConn,
    target_major: u32,
    spec: &LocaleSpec,
) -> AppResult<Option<String>> {
    let c = &conn.client;
    let ctx = |e: tokio_postgres::Error| {
        pgwhy_core::conn::classify(&e, "check locales on the target server")
    };
    match spec.provider {
        LocaleProvider::Icu if target_major < 15 => {
            return Ok(Some(format!(
                "the source uses the ICU locale provider ({}), which PostgreSQL {target_major} can't use for databases (needs 15+)",
                spec.locale.as_deref().unwrap_or("?")
            )))
        }
        LocaleProvider::Icu => {
            let has: bool = c
                .query_one("select exists(select 1 from pg_catalog.pg_collation where collprovider = 'i')", &[])
                .await
                .map_err(ctx)?
                .get(0);
            if !has {
                return Ok(Some("the source uses ICU locales but the target server was built without ICU".into()));
            }
        }
        LocaleProvider::Builtin if target_major < 17 => {
            return Ok(Some(format!(
                "the source uses the builtin locale provider, which needs PostgreSQL 17+ (target is {target_major})"
            )))
        }
        _ => {}
    }
    for (what, name) in [("collation", &spec.collate), ("ctype", &spec.ctype)] {
        if matches!(name.as_str(), "C" | "POSIX") {
            continue;
        }
        let found: bool = c
            .query_one(
                "select exists(select 1 from pg_catalog.pg_database where datcollate = $1 or datctype = $1) \
                 or exists(select 1 from pg_catalog.pg_collation where collprovider = 'c' and (collcollate = $1 or collctype = $1))",
                &[name],
            )
            .await
            .map_err(ctx)?
            .get(0);
        if !found {
            return Ok(Some(format!(
                "the source's {what} locale \"{name}\" is not installed on the target server"
            )));
        }
    }
    Ok(None)
}

#[cfg(unix)]
fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes into the zeroed struct we own; `c` is a valid C string.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_space(_path: &Path) -> Option<u64> {
    None
}

fn human(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

fn is_local_host(entry: &ConnectionEntry) -> bool {
    entry.tunnel.is_none()
        && matches!(
            entry.host.to_ascii_lowercase().as_str(),
            "localhost" | "127.0.0.1" | "::1"
        )
}

/// Resolve a DB password; returns the child copy, its source, and whether it is missing
/// although one is recorded as saved.
fn resolve_db_password(
    deps: &RealDeps,
    entry: &ConnectionEntry,
) -> AppResult<(Option<ToolPassword>, CredentialSource)> {
    let (secret, source) = deps
        .manager
        .vault()
        .resolve(&SecretKey::new(&entry.id, SecretKind::DbPassword))?;
    Ok((secret.map(|s| ToolPassword::new(s.expose())), source))
}

fn tunnel_needs(deps: &RealDeps, entry: &ConnectionEntry, out: &mut Vec<MissingSecret>) {
    let Some(t) = &entry.tunnel else { return };
    let (kind, needed, label) = match &t.auth {
        TunnelAuth::Key {
            passphrase_saved, ..
        } => (
            SecretKind::SshKeyPassphrase,
            *passphrase_saved,
            SecretNeed::SshKeyPassphrase,
        ),
        TunnelAuth::Password { .. } => (SecretKind::SshPassword, true, SecretNeed::SshPassword),
    };
    if !needed {
        return;
    }
    let present = deps
        .manager
        .vault()
        .resolve(&SecretKey::new(&entry.id, kind))
        .map(|(s, _)| s.is_some())
        .unwrap_or(false);
    if !present {
        out.push(MissingSecret {
            connection_id: entry.id.clone(),
            connection_name: entry.name.clone(),
            kind: label,
            reason: "the SSH tunnel needs it to start".into(),
        });
    }
}

fn need_password(entry: &ConnectionEntry, reason: &str, out: &mut Vec<MissingSecret>) {
    if !out
        .iter()
        .any(|m| m.connection_id == entry.id && m.kind == SecretNeed::DbPassword)
    {
        out.push(MissingSecret {
            connection_id: entry.id.clone(),
            connection_name: entry.name.clone(),
            kind: SecretNeed::DbPassword,
            reason: reason.into(),
        });
    }
}

fn auth_like(e: &AppError) -> bool {
    matches!(e.kind, ErrorKind::Auth | ErrorKind::Credentials)
}

type Parked = Option<(RunPlan, RunLockGuard, bool)>;

/// The gate sequence. Returns the report and, when every gate passed, the plan + lock to park.
pub(crate) async fn run(
    deps: &RealDeps,
    inner: &Inner,
    wf: &RefreshWorkflow,
    caller_warnings: Vec<String>,
    retry: Option<RetryContext>,
) -> (PreflightReport, Parked) {
    let mut report = PreflightReport {
        workflow_id: Some(wf.id.clone()),
        ticket: None,
        checks: Vec::new(),
        confirmation: None,
        missing_secrets: Vec::new(),
        refused: None,
        retry: retry.as_ref().map(|r| r.offer.clone()),
    };
    let refuse = |mut report: PreflightReport, e: AppError| -> (PreflightReport, Parked) {
        report.refused.get_or_insert(e);
        (report, None)
    };
    let (source, target) = match (
        deps.manager.entry(&wf.source.connection),
        deps.manager.entry(&wf.target.connection),
    ) {
        (Ok(s), Ok(t)) => (s, t),
        (Err(e), _) | (_, Err(e)) => {
            let e = AppError::new(
                ErrorKind::Config,
                format!(
                    "the workflow refers to a connection that no longer exists ({})",
                    e.message
                ),
            );
            report
                .checks
                .push(PreflightCheck::from_error("connections", "Connections", &e));
            return refuse(report, e);
        }
    };
    let target_db = wf.target.database.clone();
    let strategy = wf.options.strategy;
    let begins_at = retry
        .as_ref()
        .map(|r| r.offer.begins_at)
        .unwrap_or(StepId::Dump);
    let redump = begins_at == StepId::Dump;
    let mut warnings = caller_warnings;

    // 1. run lock
    let lock = match inner
        .locks
        .try_acquire(&target.server_key(), format!("the Refresh \"{}\"", wf.name))
    {
        Ok(g) => {
            report.checks.push(PreflightCheck::ok(
                "run_lock",
                "Run lock",
                format!("no other run is using {}", target.server_key()),
            ));
            g
        }
        Err(e) => {
            report
                .checks
                .push(PreflightCheck::from_error("run_lock", "Run lock", &e));
            return refuse(report, e);
        }
    };

    // 2. R18 re-check against the current entries (workflow files and tags are editable
    // outside the app).
    match definition::validate(wf, &source, &target) {
        Ok(()) => report.checks.push(PreflightCheck::ok(
            "environment_tag",
            "Target environment tag",
            format!("\"{}\" is tagged {}", target.name, target.env_tag.label()),
        )),
        Err(e) => {
            report.checks.push(PreflightCheck::from_error(
                "environment_tag",
                "Target environment tag",
                &e,
            ));
            return refuse(report, e);
        }
    }

    // 3. tools
    let registry = pgwhy_pgtools::discover(&deps.discovery);
    let tools_found = registry.highest(ToolKind::PgDump).is_some()
        && registry.highest(ToolKind::PgRestore).is_some();
    if tools_found {
        let d = registry
            .highest(ToolKind::PgDump)
            .map(|t| t.version.raw.clone())
            .unwrap_or_default();
        let r = registry
            .highest(ToolKind::PgRestore)
            .map(|t| t.version.raw.clone())
            .unwrap_or_default();
        report.checks.push(PreflightCheck::ok(
            "tools",
            "PostgreSQL client tools",
            format!("found pg_dump {d} and pg_restore {r}"),
        ));
    } else {
        let kind = if registry.highest(ToolKind::PgDump).is_none() {
            ToolKind::PgDump
        } else {
            ToolKind::PgRestore
        };
        let e = match kind {
            ToolKind::PgDump => pgwhy_pgtools::require_pg_dump(&registry, 0).err(),
            ToolKind::PgRestore => pgwhy_pgtools::require_pg_restore(
                &registry,
                &PgVersion {
                    major: 0,
                    minor: 0,
                    raw: "0".into(),
                },
            )
            .err(),
        }
        .unwrap_or_else(|| AppError::new(ErrorKind::ToolMissing, "pg client tools were not found"));
        report.checks.push(PreflightCheck::from_error(
            "tools",
            "PostgreSQL client tools",
            &e,
        ));
    }

    // credentials (resolved now, reported last)
    let mut missing = Vec::new();
    let (source_pw, source_cred) = match resolve_db_password(deps, &source) {
        Ok(v) => v,
        Err(e) => return refuse(report, e),
    };
    let (target_pw, target_cred) = match resolve_db_password(deps, &target) {
        Ok(v) => v,
        Err(e) => return refuse(report, e),
    };
    if redump {
        if source.password_saved && source_pw.is_none() {
            need_password(
                &source,
                "its saved password was not found in the keychain",
                &mut missing,
            );
        }
        tunnel_needs(deps, &source, &mut missing);
    }
    if target.password_saved && target_pw.is_none() {
        need_password(
            &target,
            "its saved password was not found in the keychain",
            &mut missing,
        );
    }
    tunnel_needs(deps, &target, &mut missing);

    // 4. source reachable + live catalog facts
    let mut src: Option<SourceFacts> = None;
    if redump {
        match deps
            .manager
            .open(&source.id, &wf.source.database, Purpose::Workflow)
            .await
        {
            Ok(conn) => match source_facts(&conn).await {
                Ok(f) => {
                    report.checks.push(PreflightCheck::ok(
                        "source",
                        "Source database",
                        format!(
                            "{} on {} answers (PostgreSQL {}, {})",
                            wf.source.database,
                            source.name,
                            f.version,
                            human(f.size.max(0) as u64)
                        ),
                    ));
                    src = Some(f);
                }
                Err(e) => {
                    report
                        .checks
                        .push(PreflightCheck::from_error("source", "Source database", &e))
                }
            },
            Err(e) => {
                if auth_like(&e) && source_pw.is_none() {
                    need_password(&source, "the server asked for a password", &mut missing);
                }
                report
                    .checks
                    .push(PreflightCheck::from_error("source", "Source database", &e));
            }
        }
    } else {
        let from = retry
            .as_ref()
            .map(|r| r.record.run_id.to_string())
            .unwrap_or_default();
        report.checks.push(PreflightCheck::skipped(
            "source",
            "Source database",
            format!("not needed: the backup from run {from} is reused"),
        ));
    }

    // maintenance connection (facts used by several gates)
    let maint_conn = deps
        .manager
        .open(&target.id, &target.maintenance_db, Purpose::Maintenance)
        .await;
    let tgt: Option<TargetFacts>;
    let maint: Option<RealMaintenance>;
    let maint_err: Option<AppError>;
    match maint_conn {
        Ok(conn) => match target_facts(&conn, &target_db).await {
            Ok(f) => {
                tgt = Some(f);
                maint = Some(RealMaintenance::new(conn));
                maint_err = None;
            }
            Err(e) => {
                tgt = None;
                maint = None;
                maint_err = Some(e);
            }
        },
        Err(e) => {
            if auth_like(&e) && target_pw.is_none() {
                need_password(&target, "the server asked for a password", &mut missing);
            }
            tgt = None;
            maint = None;
            maint_err = Some(e);
        }
    }
    let target_major = tgt.as_ref().map(|t| (t.version_num / 10_000) as u32);
    let dbs = match &maint {
        Some(m) => crate::ops::MaintenanceOps::databases(m)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let target_exists = dbs.iter().any(|d| d.name == target_db);
    let prev = retry.as_ref().map(|r| &r.record);
    let create_mode = match (strategy, prev) {
        (ReplaceStrategy::DropFirst, Some(p)) if begins_at > StepId::DropTarget => p.create_mode,
        _ => !target_exists,
    };

    // 5. version gates
    let mut tools: Option<PlannedTools> = None;
    if tools_found {
        let gate = if redump {
            match &src {
                Some(f) => {
                    let major = PgVersion::major_from_version_num(f.version_num as u32);
                    pgwhy_pgtools::require_pg_dump(&registry, major).and_then(|d| {
                        let r = pgwhy_pgtools::require_pg_restore(&registry, &d.version)?;
                        Ok(PlannedTools {
                            pg_dump: d,
                            pg_restore: r,
                        })
                    })
                }
                None => Err(AppError::new(
                    ErrorKind::Refused,
                    "can't check versions without reaching the source",
                )),
            }
        } else {
            let dumped = prev
                .and_then(|p| p.backup.as_ref())
                .and_then(|b| b.dumped_by.as_deref())
                .and_then(PgVersion::parse);
            match (dumped, registry.highest(ToolKind::PgDump)) {
                (Some(v), Some(d)) => {
                    pgwhy_pgtools::require_pg_restore(&registry, &v).map(|r| PlannedTools {
                        pg_dump: d.clone(),
                        pg_restore: r,
                    })
                }
                _ => Err(AppError::new(
                    ErrorKind::Refused,
                    "the reused backup's pg_dump version is unknown",
                )),
            }
        };
        match gate {
            Ok(t) => {
                report.checks.push(PreflightCheck::ok(
                    "tool_versions",
                    "Tool versions",
                    format!(
                        "pg_dump {} and pg_restore {} selected",
                        t.pg_dump.version.raw, t.pg_restore.version.raw
                    ),
                ));
                tools = Some(t);
            }
            Err(e) if src.is_none() && redump => {
                report.checks.push(PreflightCheck::skipped(
                    "tool_versions",
                    "Tool versions",
                    e.message,
                ));
            }
            Err(e) => report.checks.push(PreflightCheck::from_error(
                "tool_versions",
                "Tool versions",
                &e,
            )),
        }
        if let (Some(f), Some(tm)) = (&src, target_major) {
            if let Some(w) = pgwhy_pgtools::target_version_warning(
                PgVersion::major_from_version_num(f.version_num as u32),
                tm,
            ) {
                warnings.push(w.message.clone());
                report.checks.push(PreflightCheck::warn(
                    "target_version",
                    "Target server version",
                    w.message,
                    w.hint,
                ));
            }
        }
    } else {
        report.checks.push(PreflightCheck::skipped(
            "tool_versions",
            "Tool versions",
            "no client tools found",
        ));
    }

    // 6. maintenance DB + privileges
    match (&tgt, &maint_err) {
        (Some(f), _) => {
            let mut problems = Vec::new();
            if !f.superuser && !f.createdb {
                problems.push(format!(
                    "{} lacks CREATEDB (needed to create the staging database)",
                    target.user
                ));
            }
            if f.owns_target == Some(false) && !f.superuser {
                problems.push(format!(
                    "{} does not own {target_db} (needed to rename and drop it)",
                    target.user
                ));
            }
            if problems.is_empty() {
                report.checks.push(PreflightCheck::ok(
                    "maintenance",
                    "Target server access",
                    format!(
                        "connected to {} as {} (PostgreSQL {})",
                        target.maintenance_db, target.user, f.version
                    ),
                ));
                if !f.superuser && !f.can_signal {
                    report.checks.push(PreflightCheck::warn(
                        "terminate_privilege",
                        "Ending sessions",
                        format!(
                            "{} can only end sessions of roles it belongs to",
                            target.user
                        ),
                        Some("grant pg_signal_backend if other roles connect to the target".into()),
                    ));
                }
            } else {
                report.checks.push(PreflightCheck::fail(
                    "maintenance",
                    "Target server access",
                    problems.join("; "),
                    Some("connect the target as the database owner or a role with CREATEDB".into()),
                ));
            }
        }
        (None, Some(e)) => report.checks.push(PreflightCheck::from_error(
            "maintenance",
            "Target server access",
            e,
        )),
        (None, None) => {}
    }

    // 7. maintenance ≠ target (validate() refused it already; recorded for the report)
    report.checks.push(PreflightCheck::ok(
        "maintenance_not_target",
        "Maintenance database",
        format!(
            "renames run from {}, not from {target_db}",
            target.maintenance_db
        ),
    ));

    // 8. prepared transactions on the target
    if let Some(m) = &maint {
        if target_exists {
            match crate::ops::MaintenanceOps::prepared_xacts(m, &target_db).await {
                Ok(px) if px.is_empty() => report.checks.push(PreflightCheck::ok(
                    "prepared_xacts",
                    "Prepared transactions",
                    format!("none on {target_db}"),
                )),
                Ok(px) => {
                    let gids = px
                        .iter()
                        .map(|p| format!("'{}'", p.gid))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut c = PreflightCheck::fail(
                        "prepared_xacts",
                        "Prepared transactions",
                        format!(
                            "{target_db} has {} prepared transaction(s): {gids}",
                            px.len()
                        ),
                        Some(
                            "commit or roll them back first; pgWhy never discards them blindly"
                                .into(),
                        ),
                    );
                    c.fixes = px
                        .iter()
                        .map(|p| PreflightFix {
                            kind: PreflightFixKind::RollbackPrepared,
                            database: target_db.clone(),
                            gid: Some(p.gid.clone()),
                        })
                        .collect();
                    report.checks.push(c);
                }
                Err(e) => report.checks.push(PreflightCheck::from_error(
                    "prepared_xacts",
                    "Prepared transactions",
                    &e,
                )),
            }
        } else {
            report.checks.push(PreflightCheck::skipped(
                "prepared_xacts",
                "Prepared transactions",
                format!("{target_db} does not exist yet"),
            ));
        }
    }

    // plan skeleton (names) — needed by the name and backup checks
    let backup_dir = inner.config.backup_root.join(wf.id.0.as_str());
    let mut plan = RunPlan::new(
        wf.clone(),
        source.clone(),
        target.clone(),
        backup_dir.clone(),
        SystemTime::now(),
    );
    let adopt = retry.as_ref().is_some_and(|r| r.offer.reuses_work_db);
    if adopt && strategy == ReplaceStrategy::StagingSwap {
        if let Some(s) = prev.and_then(|p| p.staging_db.clone()) {
            plan.staging_db = s;
        }
    }

    // 9. disk space
    if begins_at <= StepId::CreateDatabase {
        let size = src.as_ref().map(|f| f.size.max(0) as u64);
        let mut notes: Vec<(CheckStatus, String, Option<String>)> = Vec::new();
        if let (true, Some(sz)) = (redump, size) {
            match free_space(&backup_dir) {
                Some(free) if free < sz => notes.push((
                    CheckStatus::Warn,
                    format!(
                        "only {} free in the backup folder; the backup may need up to {}",
                        human(free),
                        human(sz)
                    ),
                    Some("free space or choose another backup folder in Settings".into()),
                )),
                Some(free) => notes.push((
                    CheckStatus::Ok,
                    format!("{} free in the backup folder", human(free)),
                    None,
                )),
                None => notes.push((
                    CheckStatus::Warn,
                    "could not check free space in the backup folder".into(),
                    None,
                )),
            }
        }
        let local_dir = tgt
            .as_ref()
            .and_then(|t| t.data_directory.clone())
            .filter(|_| is_local_host(&target))
            .filter(|d| Path::new(d).exists());
        match (
            local_dir,
            size.or_else(|| prev.and_then(|p| p.backup.as_ref()).map(|b| b.size_bytes)),
        ) {
            (Some(dir), Some(sz)) => {
                let need = match strategy {
                    ReplaceStrategy::StagingSwap => sz.saturating_mul(2),
                    ReplaceStrategy::DropFirst => sz.saturating_sub(
                        tgt.as_ref().and_then(|t| t.target_size).unwrap_or(0).max(0) as u64,
                    ),
                };
                match free_space(Path::new(&dir)) {
                    Some(free) if free < need => {
                        let hint =
                            (strategy == ReplaceStrategy::StagingSwap && free >= sz).then(|| {
                                "switch the workflow to drop-first (needs space for one copy only)"
                                    .to_string()
                            });
                        notes.push((
                            CheckStatus::Warn,
                            format!(
                                "only {} free on the target server; this run may need about {}",
                                human(free),
                                human(need)
                            ),
                            hint,
                        ));
                    }
                    Some(free) => notes.push((
                        CheckStatus::Ok,
                        format!("{} free on the target server", human(free)),
                        None,
                    )),
                    None => notes.push((
                        CheckStatus::Warn,
                        "could not check free space on the target server".into(),
                        None,
                    )),
                }
            }
            _ => notes.push((
                CheckStatus::Warn,
                "free space on the target server can't be checked from here".into(),
                None,
            )),
        }
        let status = if notes.iter().any(|n| n.0 == CheckStatus::Warn) {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        };
        let message = notes
            .iter()
            .map(|n| n.1.clone())
            .collect::<Vec<_>>()
            .join("; ");
        let hint = notes.iter().find_map(|n| n.2.clone());
        if status == CheckStatus::Warn {
            for n in notes
                .iter()
                .filter(|n| n.0 == CheckStatus::Warn && n.1.starts_with("only"))
            {
                warnings.push(n.1.clone());
            }
        }
        report.checks.push(PreflightCheck::new(
            "disk_space",
            "Disk space",
            status,
            message,
            hint,
        ));
    } else {
        report.checks.push(PreflightCheck::skipped(
            "disk_space",
            "Disk space",
            "the existing staging database is reused",
        ));
    }

    // 10. staging / _old names (marker-verified)
    if strategy == ReplaceStrategy::StagingSwap && maint.is_some() {
        let mut problems: Vec<String> = Vec::new();
        let mut fixes = Vec::new();
        let mut check_free = |name: &str, role: MarkerRole, problems: &mut Vec<String>| {
            if let Some(d) = dbs.iter().find(|d| d.name == name) {
                match d.comment.as_deref().and_then(Marker::parse) {
                    Some(m) if m.belongs_to(role, &wf.id.0, &target_db) => {
                        problems.push(format!("{name} is a leftover of run {}", m.run_id));
                        fixes.push(PreflightFix {
                            kind: PreflightFixKind::DropLeftover,
                            database: name.to_string(),
                            gid: None,
                        });
                    }
                    _ => problems.push(format!(
                        "{name} already exists and is not pgWhy's — it may be a real database"
                    )),
                }
            }
        };
        if adopt {
            let owner = prev.map(|p| p.run_id.0.clone()).unwrap_or_default();
            match dbs.iter().find(|d| d.name == plan.staging_db) {
                Some(d)
                    if d.comment
                        .as_deref()
                        .and_then(Marker::parse)
                        .is_some_and(|m| m.run_id == owner) => {}
                Some(_) => problems.push(format!(
                    "{} no longer carries run {owner}'s marker",
                    plan.staging_db
                )),
                None => problems.push(format!("{} no longer exists", plan.staging_db)),
            }
        } else {
            // A retry drops the retried run's own staging before creating the new one, so a
            // fresh name that lands on it (a retry within the same second) is not a conflict.
            let dropped_by_retry = prev.is_some_and(|p| {
                p.staging_db.as_deref() == Some(plan.staging_db.as_str())
                    && dbs
                        .iter()
                        .find(|d| d.name == plan.staging_db)
                        .and_then(|d| d.comment.as_deref().and_then(Marker::parse))
                        .is_some_and(|m| m.run_id == p.run_id.0)
            });
            if !dropped_by_retry {
                check_free(&plan.staging_db, MarkerRole::Staging, &mut problems);
            }
        }
        if !create_mode {
            check_free(&plan.old_db, MarkerRole::Old, &mut problems);
        }
        if problems.is_empty() {
            let msg = if create_mode {
                format!("{} is free ({target_db} will be created)", plan.staging_db)
            } else if adopt {
                format!("{} is reused; {} is free", plan.staging_db, plan.old_db)
            } else {
                format!("{} and {} are free", plan.staging_db, plan.old_db)
            };
            report
                .checks
                .push(PreflightCheck::ok("names", "Staging names", msg));
        } else {
            let mut c = PreflightCheck::fail(
                "names",
                "Staging names",
                problems.join("; "),
                Some(
                    "drop pgWhy's leftovers (one click), or rename/drop other databases yourself"
                        .into(),
                ),
            );
            c.fixes = fixes;
            report.checks.push(c);
        }
    }

    // 11. encoding / locale representability
    let locale = if redump {
        src.as_ref().map(|f| f.locale.clone())
    } else {
        prev.and_then(|p| p.locale.clone())
    };
    if begins_at <= StepId::CreateDatabase {
        match (&locale, &maint, target_major) {
            (Some(l), Some(m), Some(tm)) => {
                let conn_check = locale_gap(m.conn(), tm, l).await;
                match conn_check {
                    Ok(None) => {
                        let mut msg = format!("{} / {} ({})", l.encoding, l.collate, l.provider.label());
                        if let Some(loc) = &l.locale {
                            msg.push_str(&format!(" {loc}"));
                        }
                        report.checks.push(PreflightCheck::ok("locale", "Encoding and locale", msg));
                        if l.icu_rules.is_some() && tm < 16 {
                            warnings.push("the source's ICU tailoring rules can't be applied on this target (needs 16+)".into());
                        }
                    }
                    Ok(Some(gap)) => report.checks.push(PreflightCheck::fail(
                        "locale",
                        "Encoding and locale",
                        gap,
                        Some("install the locale on the target machine, or upgrade the target server".into()),
                    )),
                    Err(e) => report.checks.push(PreflightCheck::from_error("locale", "Encoding and locale", &e)),
                }
            }
            _ => report.checks.push(PreflightCheck::skipped(
                "locale",
                "Encoding and locale",
                "needs the source and target servers",
            )),
        }
    } else {
        report.checks.push(PreflightCheck::skipped(
            "locale",
            "Encoding and locale",
            "the existing database is reused",
        ));
    }

    // 12. post-restore script (R22, re-checked against current content)
    let mut script: Option<ScriptRecord> = None;
    match &wf.post_restore_script {
        None => report.checks.push(PreflightCheck::skipped(
            "script",
            "Post-restore script",
            "no post-restore script",
        )),
        Some(_) if begins_at > StepId::PostRestore => {
            script = prev.and_then(|p| p.script.clone());
            report.checks.push(PreflightCheck::skipped(
                "script",
                "Post-restore script",
                "already applied by the retried run",
            ));
        }
        Some(id) => {
            match deps.scripts.load(id) {
                Err(e) => report.checks.push(PreflightCheck::from_error(
                    "script",
                    "Post-restore script",
                    &e,
                )),
                Ok(s) => {
                    let analysis = pgwhy_core::query::analyze(&s.content);
                    let stmt = |i: usize| {
                        pgwhy_core::query::classify(analysis.statements[i].text(&s.content)).command
                    };
                    let problem = if pgwhy_core::scripts::has_meta_commands(&s.content) {
                        Some((
                        format!("\"{}\" contains psql meta-commands (\\c, \\i, \\set, …), which pgWhy doesn't run", s.name),
                        "remove the backslash commands".to_string(),
                    ))
                    } else if let Some(&i) = analysis.txn_control.first() {
                        Some((
                        format!("\"{}\" controls its own transaction ({} at statement {})", s.name, stmt(i), i + 1),
                        "remove BEGIN/COMMIT/ROLLBACK/SAVEPOINT; the script already runs in one transaction (or none)".to_string(),
                    ))
                    } else if let Some(&i) = analysis.copy_client.first() {
                        Some((
                            format!(
                                "\"{}\" uses COPY FROM STDIN/TO STDOUT at statement {}",
                                s.name,
                                i + 1
                            ),
                            "use server-side COPY or INSERTs".to_string(),
                        ))
                    } else if let Some(&i) = analysis.unterminated.first() {
                        Some((format!("\"{}\" ends inside an unterminated string or comment (statement {})", s.name, i + 1), "fix the script".to_string()))
                    } else if wf.options.script_mode == ScriptMode::Transaction
                        && analysis.needs_no_transaction_mode()
                    {
                        let (i, why) = analysis.needs_no_transaction[0].clone();
                        Some((
                            format!(
                                "statement {} of \"{}\" can't run inside a transaction: {why}",
                                i + 1,
                                s.name
                            ),
                            "enable \"run without transaction\" for this workflow's script"
                                .to_string(),
                        ))
                    } else {
                        None
                    };
                    match problem {
                        Some((msg, hint)) => report.checks.push(PreflightCheck::fail(
                            "script",
                            "Post-restore script",
                            msg,
                            Some(hint),
                        )),
                        None => {
                            report.checks.push(PreflightCheck::ok(
                                "script",
                                "Post-restore script",
                                format!(
                                    "\"{}\": {} statement(s), {}",
                                    s.name,
                                    analysis.statements.len(),
                                    match wf.options.script_mode {
                                        ScriptMode::Transaction => "single transaction",
                                        ScriptMode::NoTransaction => "without a transaction",
                                    }
                                ),
                            ));
                            script = Some(ScriptRecord {
                                id: s.id,
                                name: s.name,
                                mode: wf.options.script_mode,
                                sha256: s.sha256,
                                content: s.content,
                            });
                        }
                    }
                }
            }
        }
    }

    // 13. credentials
    if missing.is_empty() {
        report.checks.push(PreflightCheck::ok(
            "auth",
            "Credentials",
            "everything the run needs is available",
        ));
    } else {
        let names = missing
            .iter()
            .map(|m| {
                format!(
                    "{} ({})",
                    m.connection_name,
                    match m.kind {
                        SecretNeed::DbPassword => "database password",
                        SecretNeed::SshKeyPassphrase => "SSH key passphrase",
                        SecretNeed::SshPassword => "SSH password",
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        report.checks.push(PreflightCheck::fail(
            "auth",
            "Credentials",
            format!("credentials needed: {names}"),
            Some("enter them to continue (they can be saved to the keychain)".into()),
        ));
    }
    report.missing_secrets = missing;

    // retry: re-validate the reused backup now (`pg_restore -l`)
    let mut resume: Option<Resume> = None;
    if let Some(r) = &retry {
        let mut backup = None;
        if r.offer.reuses_backup {
            match (&r.record.backup, &tools) {
                (Some(b), Some(t)) => {
                    let mut probe_plan = plan.clone();
                    probe_plan.tools = Some(t.clone());
                    let ctx = OpCtx::new(RunCancel::new(), Arc::new(|_| {}));
                    match inner.ops.inspect_backup(&probe_plan, &b.path, &ctx).await {
                        Ok(v) if v.reusable => {
                            report.checks.push(PreflightCheck::ok(
                                "backup",
                                "Reused backup",
                                format!("{} is readable", b.path.display()),
                            ));
                            let mut b = b.clone();
                            b.from_run = Some(r.record.run_id.clone());
                            b.toc_items = v.toc_items.or(b.toc_items);
                            backup = Some(b);
                        }
                        Ok(v) => report.checks.push(PreflightCheck::fail(
                            "backup",
                            "Reused backup",
                            format!(
                                "the backup can't be reused: {}",
                                v.reason.unwrap_or_default()
                            ),
                            Some("retry with a fresh backup (re-dump)".into()),
                        )),
                        Err(e) => report.checks.push(PreflightCheck::from_error(
                            "backup",
                            "Reused backup",
                            &e,
                        )),
                    }
                }
                _ => report.checks.push(PreflightCheck::fail(
                    "backup",
                    "Reused backup",
                    "the backup can't be checked",
                    None,
                )),
            }
        }
        let mut drop_before_create = Vec::new();
        if !r.offer.reuses_work_db {
            match strategy {
                ReplaceStrategy::StagingSwap => {
                    if let (Some(s), Some(name)) = (&r.inventory.staging, &r.record.staging_db) {
                        if s.exists && s.owned_by_run {
                            drop_before_create.push(name.clone());
                        }
                    }
                }
                ReplaceStrategy::DropFirst if r.offer.begins_at == StepId::CreateDatabase => {
                    if r.inventory.target.exists && r.inventory.target.owned_by_run {
                        drop_before_create.push(target_db.clone());
                    }
                }
                ReplaceStrategy::DropFirst => {}
            }
        }
        resume = Some(Resume {
            retry_of: r.record.run_id.clone(),
            from: r.offer.from,
            begins_at: r.offer.begins_at,
            backup,
            adopt_work_db: r.offer.reuses_work_db,
            work_db_state: r.record.work_db_state,
            drop_before_create,
        });
    }

    if let Some(c) = report.checks.iter().find(|c| c.status == CheckStatus::Fail) {
        let e = AppError::new(ErrorKind::Refused, format!("{}: {}", c.label, c.message));
        let e = match &c.hint {
            Some(h) => e.with_hint(h.clone()),
            None => e,
        };
        return refuse(report, e);
    }

    // confirmation
    let own: Vec<_> = deps
        .manager
        .live_connections()
        .into_iter()
        .filter(|c| {
            c.connection_id == target.id
                && (c.database == target_db || c.database == plan.staging_db)
        })
        .collect();
    if !own.is_empty() {
        warnings.push(format!(
            "pgWhy has {} open connection(s) to {target_db} (explorer, editor tabs, grids); they are closed when the {} — open transactions roll back",
            own.len(),
            if strategy == ReplaceStrategy::DropFirst { "target is dropped" } else { "swap starts" }
        ));
    }
    match (strategy, create_mode) {
        (_, true) => warnings.push(format!("{target_db} does not exist on {} yet; it will be created", target.name)),
        (ReplaceStrategy::StagingSwap, false) => warnings.push(format!(
            "anything opened on {target_db} during the run is closed when the swap starts: open transactions roll back and pending grid edits are discarded"
        )),
        (ReplaceStrategy::DropFirst, false) => warnings.push(format!(
            "drop-first: {target_db} is dropped before the restore — a failed run will leave no local database"
        )),
    }
    if wf.options.no_owner || wf.options.no_privileges {
        warnings.push(format!(
            "restored without ownership/privileges: objects will be owned by {} and other roles lose access unless the post-restore script re-grants",
            target.user
        ));
    }

    let script_hash = script.as_ref().map(|s| s.sha256.clone());
    plan.fingerprint = wf.confirmation_fingerprint(script_hash.as_deref());
    plan.remembered = wf.confirmation_remembered(&target, script_hash.as_deref());
    let can_remember = target.env_tag == EnvTag::Local && wf.has_succeeded;
    plan.create_mode = create_mode;
    plan.tools = tools;
    plan.locale = locale;
    plan.script = script;
    plan.source_password = source_pw;
    plan.target_password = target_pw;
    plan.credentials = crate::history::CredentialsRecord {
        source: source_cred,
        target: target_cred,
    };
    plan.source_version = src
        .as_ref()
        .map(|f| f.version.clone())
        .or_else(|| prev.and_then(|p| p.source.server_version.clone()));
    plan.target_version = tgt.as_ref().map(|t| t.version.clone());
    plan.estimated_objects = src.as_ref().map(|f| {
        (f.tables.max(0) as u64).saturating_sub(wf.options.exclude_table_data.len() as u64)
    });
    plan.resume = resume;
    plan.warnings = warnings.clone();

    report.confirmation = Some(Confirmation {
        target_db: target_db.clone(),
        target_server_label: format!("{} ({}:{})", target.name, target.host, target.port),
        target_tag: target.env_tag,
        strategy,
        create_mode,
        source_label: format!("{} ({}:{})", source.name, source.host, source.port),
        source_db: wf.source.database.clone(),
        staging_db: (strategy == ReplaceStrategy::StagingSwap).then(|| plan.staging_db.clone()),
        warnings,
        remembered: plan.remembered,
        can_remember,
        fingerprint: plan.fingerprint.clone(),
    });
    (report, Some((plan, lock, can_remember)))
}

/// Execute a [`PreflightFix`].
pub(crate) async fn apply_fix(
    deps: &RealDeps,
    ops: &dyn crate::ops::RefreshOps,
    wf: &RefreshWorkflow,
    fix: &PreflightFix,
    timeout: std::time::Duration,
) -> AppResult<()> {
    match fix.kind {
        PreflightFixKind::DropLeftover => {
            let maint = ops.maintenance(&wf.target.connection).await?;
            let dbs = maint.databases().await?;
            let fact = dbs.iter().find(|d| d.name == fix.database).ok_or_else(|| {
                AppError::new(
                    ErrorKind::Refused,
                    format!("{} no longer exists", fix.database),
                )
            })?;
            let marker = fact
                .comment
                .as_deref()
                .and_then(Marker::parse)
                .filter(|m| {
                    m.workflow_id == wf.id.0
                        && m.original_target == wf.target.database
                        && m.role != MarkerRole::WorkTarget
                })
                .ok_or_else(|| {
                    AppError::new(
                        ErrorKind::Refused,
                        format!(
                            "{} is not a pgWhy leftover of this workflow; not dropped",
                            fix.database
                        ),
                    )
                })?;
            let run = crate::history::RunId(marker.run_id.clone());
            crate::swap::drop_marked(&*maint, &fix.database, &run, Some(marker.role), timeout).await
        }
        PreflightFixKind::RollbackPrepared => {
            let gid = fix
                .gid
                .as_deref()
                .ok_or_else(|| AppError::internal("no gid"))?;
            let conn = deps
                .manager
                .open(&wf.target.connection, &fix.database, Purpose::Maintenance)
                .await?;
            conn.client
                .batch_execute(&format!(
                    "ROLLBACK PREPARED {}",
                    crate::staging::quote_literal(gid)
                ))
                .await
                .map_err(|e| pgwhy_core::conn::classify(&e, "roll back the prepared transaction"))
        }
    }
}
