//! The operations callers use: [`backup`] (pg_dump to a file), [`restore`] (pg_restore into
//! a database, typically staging), and [`dump_schema_ddl`] (one table's CREATE DDL for
//! Script-as). Each takes an already-gated tool (see [`crate::gates`]), a resolved endpoint,
//! optional password (→ per-run pgpass), a [`RunContext`], and a progress callback.

use crate::discover::{DiscoveredTool, PgVersion, ToolKind};
use crate::endpoint::{table_pattern, ToolEndpoint, ToolPassword};
use crate::progress::ProgressEvent;
use crate::runner::{dbname_arg, run_child, ChildSpec, FailureCause, RunContext, ToolRun};
use pgwhy_errors::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DumpFormat {
    /// `-Fc`: single compressed file, restorable selectively and in parallel.
    #[default]
    Custom,
    /// `-Fd`: a directory; the only format pg_dump can write with `--jobs`.
    Directory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupOptions {
    pub format: DumpFormat,
    /// psql patterns for `--exclude-table-data`; build each with [`table_pattern`] so it
    /// matches exactly one table.
    pub exclude_table_data: Vec<String>,
    /// Parallel dump workers (Directory format only).
    pub jobs: Option<u32>,
    /// Catalog pre-count of tables whose data will be dumped (minus exclusions) — the "~Y"
    /// of the "~X of ~Y" readout.
    pub estimated_objects: Option<u64>,
    /// psql patterns for `--schema`; build each with
    /// [`schema_pattern`](crate::endpoint::schema_pattern). Never combined with `tables`.
    #[serde(default)]
    pub schemas: Vec<String>,
    /// psql patterns for `--table`; build each with [`table_pattern`]. Never combined with
    /// `schemas`.
    #[serde(default)]
    pub tables: Vec<String>,
    /// Structure only, no data.
    #[serde(default)]
    pub schema_only: bool,
    /// Data only, no schema.
    #[serde(default)]
    pub data_only: bool,
    /// Full tool output is written here.
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupOutcome {
    pub path: PathBuf,
    pub size_bytes: u64,
    /// Version of the pg_dump that wrote the archive (gates the later pg_restore).
    pub dumped_by: PgVersion,
    pub run: ToolRun,
}

/// Back up `endpoint.dbname` to `out_path`. pg_dump writes to `<out_path>.partial`, which is
/// renamed into place only on success and deleted on failure or cancel — a failed dump never
/// leaves a file that looks like a backup (retry matrix: "dump failed → no backup").
pub async fn backup(
    pg_dump: &DiscoveredTool,
    endpoint: &ToolEndpoint,
    password: Option<&ToolPassword>,
    out_path: &Path,
    opts: &BackupOptions,
    ctx: &RunContext,
    mut on_progress: impl FnMut(ProgressEvent) + Send,
) -> AppResult<BackupOutcome> {
    expect_kind(pg_dump, ToolKind::PgDump)?;
    if opts.jobs.is_some_and(|j| j > 1) && opts.format != DumpFormat::Directory {
        return Err(AppError::new(
            ErrorKind::Config,
            "Parallel backup (jobs > 1) requires the directory backup format",
        ));
    }
    for p in &opts.exclude_table_data {
        check_arg_value(p, "exclude-table-data pattern")?;
    }
    if opts.schema_only && opts.data_only {
        return Err(AppError::new(
            ErrorKind::Config,
            "a backup cannot be both schema-only and data-only",
        ));
    }
    if !opts.schemas.is_empty() && !opts.tables.is_empty() {
        return Err(AppError::new(
            ErrorKind::Config,
            "choose schemas or tables for a partial backup, not both",
        ));
    }
    for p in opts.schemas.iter().chain(&opts.tables) {
        check_arg_value(p, "backup pattern")?;
    }
    if out_path.exists() {
        return Err(AppError::new(
            ErrorKind::Config,
            format!("A backup already exists at {}", out_path.display()),
        )
        .with_hint("Choose a new file name; pgWhy never overwrites an existing backup"));
    }
    if let Some(parent) = out_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_backup_dir(parent).map_err(|e| {
            AppError::new(ErrorKind::Disk, "Could not create the backup folder")
                .with_detail(format!("{}: {e}", parent.display()))
        })?;
    }
    // Claim the partial path atomically; only something this run created is ever removed. A
    // leftover from an interrupted run (not ours, so never deleted) gets a fresh name beside it
    // rather than blocking the backup.
    let partial_path = claim_unique_partial(out_path, opts.format).map_err(|(path, e)| {
        AppError::new(ErrorKind::Disk, "Could not create the partial backup")
            .with_detail(format!("{}: {e}", path.display()))
    })?;
    let partial = PartialGuard::new(partial_path);

    let mut file_arg = OsString::from("--file=");
    file_arg.push(&partial.path);
    let mut args: Vec<OsString> = vec![
        "--verbose".into(),
        match opts.format {
            DumpFormat::Custom => "--format=custom".into(),
            DumpFormat::Directory => "--format=directory".into(),
        },
        file_arg,
    ];
    if let Some(j) = opts.jobs.filter(|&j| j > 1) {
        args.push(format!("--jobs={j}").into());
    }
    for p in &opts.exclude_table_data {
        args.push(format!("--exclude-table-data={p}").into());
    }
    if opts.schema_only {
        args.push("--schema-only".into());
    }
    if opts.data_only {
        args.push("--data-only".into());
    }
    for p in &opts.schemas {
        args.push(format!("--schema={p}").into());
    }
    for p in &opts.tables {
        args.push(format!("--table={p}").into());
    }
    args.push(dbname_arg(endpoint));

    let mut spec = ChildSpec::new(pg_dump, args);
    spec.endpoint = Some(endpoint.clone());
    spec.password = password.cloned();
    spec.watch_path = Some(partial.path.clone());
    spec.log_path = opts.log_path.clone();
    spec.total_objects = opts.estimated_objects;
    spec.total_is_estimate = true;

    let run = run_child(&spec, ctx, &mut on_progress)
        .await?
        .into_result()?;
    place_no_clobber(&partial.path, out_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            AppError::new(
                ErrorKind::Config,
                format!("A backup already exists at {}", out_path.display()),
            )
            .with_hint("Choose a new file name; pgWhy never overwrites an existing backup")
        } else {
            AppError::new(
                ErrorKind::Disk,
                "Could not move the finished backup into place",
            )
            .with_detail(format!("{}: {e}", out_path.display()))
        }
    })?;
    partial.disarm();
    let size_bytes = path_size(out_path);
    Ok(BackupOutcome {
        path: out_path.to_path_buf(),
        size_bytes,
        dumped_by: pg_dump.version.clone(),
        run,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPolicy {
    /// `--exit-on-error`: stop at the first failing item (the default).
    #[default]
    ExitOnError,
    /// Keep going; any failed item still makes the run a failure, with the items named.
    ContinueOnError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOptions {
    pub no_owner: bool,
    pub no_privileges: bool,
    pub jobs: Option<u32>,
    pub error_policy: ErrorPolicy,
    /// Restore progress total — [`crate::Toc::restorable_count`].
    pub total_items: Option<u64>,
    pub log_path: Option<PathBuf>,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            no_owner: true,
            no_privileges: true,
            jobs: None,
            error_policy: ErrorPolicy::ExitOnError,
            total_items: None,
            log_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreOutcome {
    pub run: ToolRun,
}

/// Restore `archive` into `endpoint.dbname` (which must already exist — staging is created by
/// the workflow layer). Success means exit 0 *and* no failed items; under continue-on-error,
/// failed items are a failure naming the objects (`data.failed_items`). An unreadable archive
/// fails with cause `archive_corrupt` (backup non-reusable).
pub async fn restore(
    pg_restore: &DiscoveredTool,
    endpoint: &ToolEndpoint,
    password: Option<&ToolPassword>,
    archive: &Path,
    opts: &RestoreOptions,
    ctx: &RunContext,
    mut on_progress: impl FnMut(ProgressEvent) + Send,
) -> AppResult<RestoreOutcome> {
    expect_kind(pg_restore, ToolKind::PgRestore)?;
    if !archive.exists() {
        return Err(AppError::new(
            ErrorKind::ToolFailed,
            format!("The backup file {} no longer exists", archive.display()),
        )
        .with_hint("Take a new backup")
        .with_data(json!({ "tool": "pg_restore", "cause": FailureCause::ArchiveCorrupt })));
    }
    let mut args: Vec<OsString> = vec!["--verbose".into()];
    if opts.no_owner {
        args.push("--no-owner".into());
    }
    if opts.no_privileges {
        args.push("--no-privileges".into());
    }
    if opts.error_policy == ErrorPolicy::ExitOnError {
        args.push("--exit-on-error".into());
    }
    if let Some(j) = opts.jobs.filter(|&j| j > 1) {
        args.push(format!("--jobs={j}").into());
    }
    args.push(dbname_arg(endpoint));
    args.push("--".into());
    args.push(archive.as_os_str().to_owned());

    let mut spec = ChildSpec::new(pg_restore, args);
    spec.endpoint = Some(endpoint.clone());
    spec.password = password.cloned();
    spec.log_path = opts.log_path.clone();
    spec.total_objects = opts.total_items;
    spec.total_is_estimate = false;

    let mut run = run_child(&spec, ctx, &mut on_progress).await?;
    if run.success && (!run.failed_items.is_empty() || run.errors_ignored.is_some_and(|n| n > 0)) {
        run.success = false;
    }
    Ok(RestoreOutcome {
        run: run.into_result()?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DdlOptions {
    pub no_owner: bool,
    pub no_privileges: bool,
    /// Keep pg_dump's session preamble (`SET ...`, `\restrict`, header comments).
    pub keep_preamble: bool,
}

impl Default for DdlOptions {
    fn default() -> Self {
        Self {
            no_owner: true,
            no_privileges: true,
            keep_preamble: false,
        }
    }
}

/// CREATE DDL for exactly one table via `pg_dump --schema-only --table='"schema"."table"'`
/// (pattern-safe: quoted parts make every psql pattern metacharacter literal).
pub async fn dump_schema_ddl(
    pg_dump: &DiscoveredTool,
    endpoint: &ToolEndpoint,
    password: Option<&ToolPassword>,
    schema: &str,
    table: &str,
    opts: &DdlOptions,
    ctx: &RunContext,
) -> AppResult<String> {
    expect_kind(pg_dump, ToolKind::PgDump)?;
    check_arg_value(schema, "schema name")?;
    check_arg_value(table, "table name")?;
    let mut args: Vec<OsString> = vec![
        "--schema-only".into(),
        "--format=plain".into(),
        format!("--table={}", table_pattern(schema, table)).into(),
    ];
    if opts.no_owner {
        args.push("--no-owner".into());
    }
    if opts.no_privileges {
        args.push("--no-privileges".into());
    }
    args.push(dbname_arg(endpoint));
    let mut spec = ChildSpec::new(pg_dump, args);
    spec.endpoint = Some(endpoint.clone());
    spec.password = password.cloned();
    spec.capture_stdout = true;
    let run = run_child(&spec, ctx, &mut |_| {}).await?.into_result()?;
    let raw = String::from_utf8_lossy(run.stdout.as_deref().unwrap_or_default()).into_owned();
    Ok(if opts.keep_preamble {
        raw
    } else {
        ddl_body(&raw)
    })
}

/// Strip pg_dump's session preamble and banner comments from a plain schema dump, leaving
/// the DDL statements: `SET`/`set_config` lines, `\restrict`/`\unrestrict` meta-commands
/// (pg_dump 13.22+/…/17.6+ emit them; they'd break the SQL editor), bare `--` and `-- Name:`
/// / `-- TOC entry` comments, and banner lines. Runs of blank lines collapse to one.
pub fn ddl_body(raw: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in raw.lines() {
        let t = line.trim_end();
        let drop = t.starts_with("SET ")
            || t.starts_with("SELECT pg_catalog.set_config(")
            || t.starts_with("\\restrict")
            || t.starts_with("\\unrestrict")
            || t == "--"
            || t.starts_with("-- Name: ")
            || t.starts_with("-- TOC entry ")
            || t.starts_with("-- Dependencies: ")
            || t.starts_with("-- PostgreSQL database dump")
            || t.starts_with("-- Dumped from database version")
            || t.starts_with("-- Dumped by pg_dump version")
            || t.starts_with("-- Started on")
            || t.starts_with("-- Completed on");
        if drop {
            continue;
        }
        if t.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(t);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn expect_kind(tool: &DiscoveredTool, kind: ToolKind) -> AppResult<()> {
    if tool.kind != kind {
        return Err(AppError::internal(format!(
            "expected {kind}, got {}",
            tool.kind
        )));
    }
    Ok(())
}

fn check_arg_value(v: &str, what: &str) -> AppResult<()> {
    if v.contains(['\0', '\n', '\r']) {
        return Err(AppError::new(
            ErrorKind::Config,
            format!("The {what} contains a line break or NUL character"),
        ));
    }
    Ok(())
}

/// Create the backup folder, owner-only (0700) when we create it; an existing folder is
/// left as the user made it.
fn create_backup_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Atomically create the (empty) partial target so a concurrent creator cannot be
/// overwritten by pg_dump; pg_dump accepts an empty file or an empty directory.
fn claim_partial(path: &Path, format: DumpFormat) -> std::io::Result<()> {
    match format {
        DumpFormat::Custom => {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            opts.open(path).map(drop)
        }
        DumpFormat::Directory => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)
        }
    }
}

/// Move the finished partial to `to`, failing with `AlreadyExists` instead of replacing
/// something that appeared since the up-front check.
fn place_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        // Claim `to` with an atomic create_dir. unix rename() may replace an empty directory
        // (ours); Windows rename refuses an existing directory, so it is already no-clobber.
        #[cfg(unix)]
        std::fs::create_dir(to)?;
        let res = std::fs::rename(from, to);
        #[cfg(unix)]
        if res.is_err() {
            let _ = std::fs::remove_dir(to);
        }
        return res;
    }
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            let _ = std::fs::remove_file(from);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // No hard links here (FAT, some network shares): re-check, then rename.
        Err(_) => {
            if to.exists() {
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            std::fs::rename(from, to)
        }
    }
}

fn partial_path(out: &Path) -> PathBuf {
    partial_path_tagged(out, None)
}

/// `<out>.partial`, or `<out>.<tag>.partial` when the plain name is taken.
fn partial_path_tagged(out: &Path, tag: Option<&str>) -> PathBuf {
    let mut name = out
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    if let Some(tag) = tag {
        name.push(".");
        name.push(tag);
    }
    name.push(".partial");
    out.with_file_name(name)
}

/// Claim `<out>.partial`, falling back to a uniquely tagged name when something (a leftover
/// from an interrupted backup) already holds it. Returns the claimed path.
fn claim_unique_partial(
    out: &Path,
    format: DumpFormat,
) -> Result<PathBuf, (PathBuf, std::io::Error)> {
    let mut path = partial_path(out);
    for _ in 0..4 {
        match claim_partial(&path, format) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let tag = uuid::Uuid::new_v4().simple().to_string();
                path = partial_path_tagged(out, Some(&tag[..8]));
            }
            Err(e) => return Err((path, e)),
        }
    }
    Err((path, std::io::ErrorKind::AlreadyExists.into()))
}

fn path_size(p: &Path) -> u64 {
    match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Deletes the in-progress dump on every exit path unless disarmed after the final rename.
struct PartialGuard {
    path: PathBuf,
    armed: bool,
}

impl PartialGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn remove(&self) {
        let _ = if self.path.is_dir() {
            std::fs::remove_dir_all(&self.path)
        } else {
            std::fs::remove_file(&self.path)
        };
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PartialGuard {
    fn drop(&mut self) {
        if self.armed {
            self.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_body_strips_preamble() {
        let raw = "--\n-- PostgreSQL database dump\n--\n\n\\restrict abc123\n\n-- Dumped from database version 16.11\n-- Dumped by pg_dump version 16.15\n\nSET statement_timeout = 0;\nSELECT pg_catalog.set_config('search_path', '', false);\n\n--\n-- Name: t1; Type: TABLE; Schema: public; Owner: -\n--\n\nCREATE TABLE public.t1 (\n    id integer NOT NULL\n);\n\n\n\nALTER TABLE ONLY public.t1\n    ADD CONSTRAINT t1_pkey PRIMARY KEY (id);\n\n--\n-- PostgreSQL database dump complete\n--\n\n\\unrestrict abc123\n\n";
        assert_eq!(
            ddl_body(raw),
            "CREATE TABLE public.t1 (\n    id integer NOT NULL\n);\n\nALTER TABLE ONLY public.t1\n    ADD CONSTRAINT t1_pkey PRIMARY KEY (id);\n"
        );
    }

    #[test]
    fn partial_name() {
        assert_eq!(
            partial_path(Path::new("/b/x.dump")),
            PathBuf::from("/b/x.dump.partial")
        );
        assert_eq!(
            partial_path_tagged(Path::new("/b/x.dump"), Some("1a2b3c4d")),
            PathBuf::from("/b/x.dump.1a2b3c4d.partial")
        );
    }
}
