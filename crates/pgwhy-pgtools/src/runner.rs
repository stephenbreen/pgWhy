//! The child runner: spawn pg_dump/pg_restore directly (no shell), credentials only via a
//! per-run temp `PGPASSFILE`, stream `--verbose` stderr into progress, cancel by killing the
//! child's process group, and classify failures into the AppError taxonomy (ER-3).
//!
//! Environment hygiene: every inherited `PG*` variable is removed so a stray `PGPASSWORD`,
//! `PGSERVICE` or `PGHOST` in the user's shell can't redirect or authenticate a child; the
//! child sees exactly `PGPASSFILE`, `PGSSLMODE`, `PGSSLROOTCERT` (when set), `PGAPPNAME` and
//! `PGCONNECT_TIMEOUT`. `LC_MESSAGES=C` keeps stderr in the English grammar the parsers pin.

use crate::discover::{DiscoveredTool, ToolKind};
use crate::endpoint::{ToolEndpoint, ToolPassword};
use crate::pgpass::PgPassDir;
use crate::progress::{LineEvent, ProgressEvent, ProgressPhase, ProgressTracker};
use crate::sysroots::system_roots_pem;
use pgwhy_errors::{AppError, AppResult, ErrorClass, ErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

/// Lines of stderr kept in memory for reports; the full output goes to the log file.
const TAIL_LINES: usize = 200;
/// How often bytes-written/elapsed are re-emitted while a child runs.
const TICK: Duration = Duration::from_millis(500);
/// Minimum spacing of line-driven progress events (big schemas print thousands of lines).
const LINE_EMIT_MIN: Duration = Duration::from_millis(100);
/// SIGTERM → SIGKILL escalation delay on cancel.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// After the child exits, how long to keep draining stderr held open by grandchildren.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// libpq connect timeout for children (seconds).
const CONNECT_TIMEOUT_SECS: &str = "20";

/// Cooperative cancellation for a run; clone freely, cancel from any thread.
#[derive(Debug, Clone)]
pub struct CancelHandle {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancelHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelHandle {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
        }
    }

    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.tx.subscribe()
    }
}

/// Per-run environment shared by all children of one operation.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub pgpass_dir: PgPassDir,
    pub cancel: CancelHandle,
}

impl RunContext {
    pub fn new(pgpass_dir: PgPassDir) -> Self {
        Self {
            pgpass_dir,
            cancel: CancelHandle::new(),
        }
    }
}

/// Why a child failed — drives retry decisions (e.g. `ArchiveCorrupt` marks a backup
/// non-reusable; `Network` is retriable once the tunnel is back).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCause {
    Auth,
    Network,
    Tls,
    Disk,
    /// pg_dump older than the server, or pg_restore older than the archive.
    VersionMismatch,
    /// The archive is unreadable: truncated, corrupt, not an archive, or missing.
    ArchiveCorrupt,
    /// The named database / table / schema doesn't exist.
    MissingObject,
    Permission,
    /// pg_restore statements failed for specific TOC items (see `failed_items`).
    ObjectErrors,
    /// The child died from a signal it wasn't sent by us.
    Killed,
    Cancelled,
    Other,
}

impl FailureCause {
    /// Read the cause back out of an AppError produced by this crate.
    pub fn from_error(err: &AppError) -> Option<FailureCause> {
        let v = err.data.as_ref()?.get("cause")?.clone();
        serde_json::from_value(v).ok()
    }
}

/// A TOC item pg_restore failed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedItem {
    /// The object (`INDEX public.t1_v_idx`, or the `from TOC entry` text).
    pub object: Option<String>,
    /// The server error (`ERROR:  relation "x" already exists`).
    pub error: String,
    /// The failing statement, when pg_restore printed it (truncated).
    pub command: Option<String>,
}

/// The record of one child run (also embedded in failure AppErrors as `data`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRun {
    pub tool: ToolKind,
    pub tool_path: PathBuf,
    pub tool_version: String,
    /// argv as passed (never contains credential material — asserted in tests).
    pub args: Vec<String>,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    pub stderr_tail: Vec<String>,
    pub log_path: Option<PathBuf>,
    pub failed_items: Vec<FailedItem>,
    /// `pg_restore: warning: errors ignored on restore: N`
    pub errors_ignored: Option<u32>,
    pub objects_done: u64,
    #[serde(skip)]
    pub stdout: Option<Vec<u8>>,
}

impl ToolRun {
    /// `Ok(self)` when the child succeeded, else the classified error (with this run's
    /// record in `data`).
    pub fn into_result(self) -> AppResult<ToolRun> {
        if self.success {
            return Ok(self);
        }
        Err(classify_failure(&self))
    }
}

/// Everything needed to launch one child. `args` must already contain the connection
/// argument (see [`dbname_arg`]) in the right position.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub tool: DiscoveredTool,
    pub args: Vec<OsString>,
    /// Provides sslmode/sslrootcert env and the pgpass key; `None` for offline commands
    /// (`pg_restore --list`).
    pub endpoint: Option<ToolEndpoint>,
    pub password: Option<ToolPassword>,
    pub capture_stdout: bool,
    /// File (or directory) whose size is reported as bytes written.
    pub watch_path: Option<PathBuf>,
    /// Full stderr is appended here when set.
    pub log_path: Option<PathBuf>,
    pub total_objects: Option<u64>,
    pub total_is_estimate: bool,
}

/// `--dbname=<conninfo>`: one argv element, so nothing in the conninfo can be read as a flag.
pub fn dbname_arg(endpoint: &ToolEndpoint) -> OsString {
    format!("--dbname={}", endpoint.conninfo()).into()
}

impl ChildSpec {
    pub fn new(tool: &DiscoveredTool, args: Vec<OsString>) -> Self {
        Self {
            tool: tool.clone(),
            args,
            endpoint: None,
            password: None,
            capture_stdout: false,
            watch_path: None,
            log_path: None,
            total_objects: None,
            total_is_estimate: false,
        }
    }

    /// Build the (unspawned) command: direct exec, scrubbed environment, no credential in
    /// argv or env — only the path of the pgpass file.
    pub fn build(&self, pgpass: Option<&Path>) -> Command {
        self.build_with(pgpass, None)
    }

    /// [`build`](Self::build) plus a generated root-certificate bundle, used for
    /// `PGSSLROOTCERT` only when the endpoint configures none of its own.
    pub fn build_with(&self, pgpass: Option<&Path>, root_cert: Option<&Path>) -> Command {
        let mut cmd = Command::new(&self.tool.path);
        cmd.args(&self.args);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PG") {
                cmd.env_remove(&key);
            }
        }
        cmd.env_remove("LC_ALL").env_remove("LANGUAGE");
        cmd.env("LC_MESSAGES", "C");
        cmd.env("PGAPPNAME", "pgWhy");
        cmd.env("PGCONNECT_TIMEOUT", CONNECT_TIMEOUT_SECS);
        if let Some(ep) = &self.endpoint {
            cmd.env("PGSSLMODE", &ep.sslmode);
            if let Some(root) = ep.sslrootcert.as_deref().or(root_cert) {
                cmd.env("PGSSLROOTCERT", root);
            }
        }
        if let Some(p) = pgpass {
            cmd.env("PGPASSFILE", p);
        }
        cmd.stdin(Stdio::null())
            .stdout(if self.capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            cmd.process_group(0);
            // Owner-only output: a dump file (or directory-format tree) holds row data.
            // SAFETY: umask is async-signal-safe and the closure touches no other state.
            unsafe {
                cmd.pre_exec(|| {
                    libc::umask(0o077);
                    Ok(())
                });
            }
        }
        cmd
    }
}

/// Run a child to completion, emitting progress. `Err` only when the child could not be
/// set up or started; a failing child is `Ok` with `success == false` (see
/// [`ToolRun::into_result`]). The pgpass file is deleted on every path out.
pub async fn run_child(
    spec: &ChildSpec,
    ctx: &RunContext,
    on_progress: &mut (dyn FnMut(ProgressEvent) + Send),
) -> AppResult<ToolRun> {
    if ctx.cancel.is_cancelled() {
        return Err(cancelled_error(spec.tool.kind));
    }
    if let Some(ep) = &spec.endpoint {
        ep.validate()?;
    }
    let pgpass = match (&spec.endpoint, &spec.password) {
        (Some(ep), Some(pw)) => Some(ctx.pgpass_dir.create(ep, pw)?),
        _ => None,
    };
    // The in-app client falls back to the OS roots when no root cert is configured; libpq
    // would instead look for ~/.postgresql/root.crt, so hand the child the same roots.
    let root_cert = match &spec.endpoint {
        Some(ep)
            if ep.sslrootcert.is_none()
                && matches!(ep.sslmode.as_str(), "verify-ca" | "verify-full") =>
        {
            Some(ctx.pgpass_dir.create_root_cert(&system_roots_pem()?)?)
        }
        _ => None,
    };
    let mut cmd = spec.build_with(
        pgpass.as_ref().map(|p| p.path()),
        root_cert.as_ref().map(|p| p.path()),
    );
    let kind = spec.tool.kind;
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        AppError::new(
            ErrorKind::ToolMissing,
            format!("Could not start {kind} ({})", spec.tool.path.display()),
        )
        .with_detail(e.to_string())
        .with_hint(
            "Rescan PostgreSQL tools in Settings; the binary may have been removed or upgraded",
        )
    })?;
    let pid = child.id();
    // Declared after `child` so it drops first: if this future is dropped mid-run (task
    // aborted), the whole group — parallel workers included — is signalled, not just the
    // leader that `kill_on_drop` reaches. On a normal exit it sweeps up any survivors.
    let _group = GroupReaper(pid);

    let stdout_task = child.stdout.take().map(|mut out| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf).await;
            buf
        })
    });
    let Some(stderr) = child.stderr.take() else {
        return Err(AppError::internal("child stderr was not piped"));
    };
    let mut stderr = BufReader::new(stderr).split(b'\n');

    let mut log = match &spec.log_path {
        Some(p) => match create_log(p).await {
            Ok(f) => Some(tokio::io::BufWriter::new(f)),
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "could not open tool log");
                None
            }
        },
        None => None,
    };

    let mut tracker = ProgressTracker::new(kind, spec.total_objects, spec.total_is_estimate);
    let mut tail: VecDeque<String> = VecDeque::with_capacity(TAIL_LINES);
    let mut failures = FailureCollector::default();
    let mut cancel_rx = ctx.cancel.subscribe();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_emit = Instant::now() - LINE_EMIT_MIN;

    let mut stderr_open = true;
    let mut status: Option<std::io::Result<std::process::ExitStatus>> = None;
    let mut cancelled = false;
    let mut cancel_live = true;
    let mut kill_requested = false;
    let mut term_sent_at: Option<Instant> = None;
    let mut killed_hard = false;
    let mut drain_deadline: Option<tokio::time::Instant> = None;
    // A cancel that landed between the entry check and subscribe() is already "seen".
    if *cancel_rx.borrow() {
        cancelled = true;
        kill_requested = true;
    }

    on_progress(tracker.snapshot());
    loop {
        if stderr_open && status.is_some() && drain_deadline.is_none() {
            drain_deadline = Some(tokio::time::Instant::now() + DRAIN_GRACE);
        }
        tokio::select! {
            seg = stderr.next_segment(), if stderr_open => match seg {
                Ok(Some(bytes)) => {
                    let line = String::from_utf8_lossy(&bytes).trim_end_matches('\r').to_string();
                    if let Some(w) = log.as_mut() {
                        let _ = w.write_all(line.as_bytes()).await;
                        let _ = w.write_all(b"\n").await;
                    }
                    let ev = tracker.feed(&line);
                    failures.observe(&line, &ev, tracker.current());
                    if tail.len() == TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                    if last_emit.elapsed() >= LINE_EMIT_MIN {
                        last_emit = Instant::now();
                        tracker.set_bytes(watched_size(spec.watch_path.as_deref()));
                        on_progress(tracker.snapshot());
                    }
                }
                _ => stderr_open = false,
            },
            st = child.wait(), if status.is_none() => status = Some(st),
            _ = tick.tick() => {
                tracker.set_bytes(watched_size(spec.watch_path.as_deref()));
                on_progress(tracker.snapshot());
            }
            changed = cancel_rx.changed(), if cancel_live && !cancelled => {
                if changed.is_err() {
                    cancel_live = false;
                } else if *cancel_rx.borrow() {
                    cancelled = true;
                    kill_requested = true;
                }
            }
            _ = sleep_until_opt(drain_deadline), if drain_deadline.is_some() => stderr_open = false,
        }
        if status.is_some() && !stderr_open {
            break;
        }
        if status.is_none() {
            if kill_requested {
                kill_requested = false;
                term_sent_at = Some(Instant::now());
                terminate(&mut child, pid, false);
            } else if let Some(at) = term_sent_at {
                if !killed_hard && at.elapsed() >= KILL_GRACE {
                    killed_hard = true;
                    terminate(&mut child, pid, true);
                }
            }
        }
    }
    if let Some(mut w) = log {
        let _ = w.flush().await;
    }
    if let Some(p) = pgpass {
        if let Err(e) = p.delete() {
            tracing::warn!(error = %e, "temp pgpass delete failed");
        }
    }
    if let Some(p) = root_cert {
        if let Err(e) = p.delete() {
            tracing::warn!(error = %e, "temp root cert delete failed");
        }
    }
    let stdout = match stdout_task {
        Some(t) => t.await.ok(),
        None => None,
    };

    tracker.set_bytes(watched_size(spec.watch_path.as_deref()));
    tracker.set_phase(ProgressPhase::Finishing);
    on_progress(tracker.snapshot());

    let status = status.and_then(|s| s.ok());
    let exit_code = status.and_then(|s| s.code());
    let success = !cancelled && status.is_some_and(|s| s.success());
    Ok(ToolRun {
        tool: kind,
        tool_path: spec.tool.path.clone(),
        tool_version: spec.tool.version.raw.clone(),
        args: spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        exit_code,
        success,
        cancelled,
        duration_ms: started.elapsed().as_millis() as u64,
        stderr_tail: tail.into_iter().collect(),
        log_path: spec.log_path.clone(),
        failed_items: failures.items,
        errors_ignored: failures.errors_ignored,
        objects_done: tracker.objects_done(),
        stdout,
    })
}

/// Truncate-create the tool log; owner-only on unix since it can echo row data.
async fn create_log(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let file = opts.open(path).await?;
    // `mode` only applies to a newly created file; tighten a pre-existing one too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    Ok(file)
}

/// Signals the child's process group on drop if anything in it is still alive: SIGTERM now,
/// SIGKILL after [`KILL_GRACE`] (mirrors the cooperative cancel path, but from `Drop`, which
/// cannot await). A no-op once the group is empty.
struct GroupReaper(Option<u32>);

impl Drop for GroupReaper {
    #[cfg(unix)]
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        let pgid = pid as libc::pid_t;
        // SAFETY: plain syscalls; the group id is the child's pid (process_group(0)).
        unsafe {
            if libc::kill(-pgid, 0) != 0 {
                return;
            }
            libc::kill(-pgid, libc::SIGTERM);
        }
        std::thread::spawn(move || {
            std::thread::sleep(KILL_GRACE);
            // SAFETY: as above; ESRCH if the group is already gone.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        });
    }

    #[cfg(not(unix))]
    fn drop(&mut self) {
        // Windows: `kill_on_drop` terminates the leader; there is no group to signal.
        let _ = self.0;
    }
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn watched_size(path: Option<&Path>) -> Option<u64> {
    let path = path?;
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_dir() {
        // Directory-format dumps: sum the files pg_dump has written so far.
        let total = std::fs::read_dir(path)
            .ok()?
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum();
        Some(total)
    } else {
        Some(meta.len())
    }
}

/// Signal the child's whole process tree (parallel pg_dump/pg_restore workers included).
fn terminate(child: &mut tokio::process::Child, pid: Option<u32>, hard: bool) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let sig = if hard { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: plain syscall; the group id is the child's pid (process_group(0)).
        unsafe {
            libc::kill(-(pid as libc::pid_t), sig);
        }
        return;
    }
    // Windows has no process groups to signal, and `Child::start_kill` only terminates the
    // direct child: `pg_restore -j N` workers would survive and keep writing into the archive
    // target. `taskkill /T` walks the tree; `/F` is the only option there (a console control
    // event needs a console this GUI process does not have).
    #[cfg(windows)]
    if let Some(pid) = pid {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        if cmd.status().is_ok_and(|s| s.success()) {
            return;
        }
        tracing::warn!(
            pid,
            hard,
            "taskkill could not terminate the tool's process tree"
        );
    }
    let _ = (pid, hard);
    let _ = child.start_kill();
}

/// Tracks restore item failures as stderr streams by.
#[derive(Debug, Default)]
struct FailureCollector {
    items: Vec<FailedItem>,
    pending_toc: Option<String>,
    awaiting_command: bool,
    errors_ignored: Option<u32>,
}

impl FailureCollector {
    fn observe(&mut self, line: &str, ev: &LineEvent, current: Option<&str>) {
        if let Some(rest) = line.split_once(": from TOC entry ").map(|(_, r)| r) {
            self.pending_toc = Some(format!("TOC entry {rest}"));
            return;
        }
        if let Some(cmd) = line.strip_prefix("Command was: ") {
            if self.awaiting_command {
                if let Some(last) = self.items.last_mut() {
                    last.command = Some(truncate(cmd, 500));
                }
            }
            self.awaiting_command = false;
            return;
        }
        if let LineEvent::Warning(w) = ev {
            if let Some(n) = w.strip_prefix("errors ignored on restore: ") {
                self.errors_ignored = n.trim().parse().ok();
            }
        }
        if let LineEvent::Error(e) = ev {
            // Item failures: "could not execute query: ERROR: ...", "COPY failed for table
            // ...", or any error directly preceded by "from TOC entry ...".
            let query = e.strip_prefix("could not execute query: ");
            if query.is_some() || e.starts_with("COPY failed") || self.pending_toc.is_some() {
                self.items.push(FailedItem {
                    object: self
                        .pending_toc
                        .take()
                        .or_else(|| current.map(str::to_string)),
                    error: query.unwrap_or(e).trim().to_string(),
                    command: None,
                });
                self.awaiting_command = query.is_some();
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn cancelled_error(kind: ToolKind) -> AppError {
    AppError::new(ErrorKind::Cancelled, format!("{kind} was cancelled"))
        .with_data(json!({ "tool": kind.name(), "cause": FailureCause::Cancelled }))
}

/// Classify a failed run's stderr into a cause and a user-facing AppError.
pub fn classify(run: &ToolRun) -> FailureCause {
    if run.cancelled {
        return FailureCause::Cancelled;
    }
    let text = run.stderr_tail.join("\n").to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));

    if has(&["server version mismatch", "unsupported version ("]) {
        return FailureCause::VersionMismatch;
    }
    let connect_failed = has(&[
        "connection to server",
        "connection to database",
        "could not connect to",
        "could not translate host name",
    ]);
    if connect_failed {
        if has(&[
            "password authentication failed",
            "no password supplied",
            "authentication failed",
            "fe_sendauth",
            "no pg_hba.conf entry",
            "scram",
        ]) || (has(&["role \""]) && has(&["does not exist"]))
        {
            return FailureCause::Auth;
        }
        if has(&["database \""]) && has(&["does not exist"]) {
            return FailureCause::MissingObject;
        }
        if has(&["permission denied for database"]) {
            return FailureCause::Permission;
        }
        if has(&[
            "certificate",
            "ssl error",
            "server does not support ssl",
            "tls",
            "sslmode",
        ]) {
            return FailureCause::Tls;
        }
        return FailureCause::Network;
    }
    if has(&[
        "server closed the connection unexpectedly",
        "connection to server was lost",
        "could not receive data from server",
        "could not send data to server",
        "ssl syscall error",
        "connection reset",
        "broken pipe",
    ]) {
        return FailureCause::Network;
    }
    if has(&[
        "no space left on device",
        "disk quota exceeded",
        "could not write to output file",
        "could not open output file",
        "could not close output file",
        "could not create directory",
        "file too large",
    ]) {
        return FailureCause::Disk;
    }
    if run.tool == ToolKind::PgRestore
        && has(&[
            "could not read from input file",
            "does not appear to be a valid archive",
            "input file is too short",
            "did not find magic string",
            "unexpected end of file",
            "could not uncompress",
            "could not decompress",
            "could not find block id",
            "unexpected block id",
            "corrupt",
            "error during file seek",
            "could not open input file",
            "could not read input file",
        ])
    {
        return FailureCause::ArchiveCorrupt;
    }
    if has(&[
        "no matching tables were found",
        "no matching schemas were found",
    ]) {
        return FailureCause::MissingObject;
    }
    if run.tool == ToolKind::PgRestore
        && (!run.failed_items.is_empty() || run.errors_ignored.is_some())
    {
        return FailureCause::ObjectErrors;
    }
    if has(&["permission denied"]) {
        return FailureCause::Permission;
    }
    if run.exit_code.is_none() {
        return FailureCause::Killed;
    }
    FailureCause::Other
}

/// The first `error:` line's text, for the message.
fn first_error(run: &ToolRun) -> Option<String> {
    run.stderr_tail.iter().find_map(|l| {
        l.split_once(": error: ")
            .map(|(_, e)| truncate(e.trim(), 300))
    })
}

/// Build the classified AppError for a failed run. `data` carries the cause, exit code,
/// stderr tail, log path and failed TOC items for the run report.
pub fn classify_failure(run: &ToolRun) -> AppError {
    let cause = classify(run);
    let tool = run.tool;
    let summary = first_error(run).unwrap_or_else(|| match run.exit_code {
        Some(c) => format!("exited with code {c}"),
        None => "was terminated".into(),
    });
    let (kind, class, message, hint): (ErrorKind, Option<ErrorClass>, String, Option<String>) =
        match cause {
            FailureCause::Cancelled => (
                ErrorKind::Cancelled,
                None,
                format!("{tool} was cancelled"),
                None,
            ),
            FailureCause::Auth => (
                ErrorKind::Auth,
                None,
                format!("{tool} could not log in: {summary}"),
                Some("Check the connection's user and password".into()),
            ),
            FailureCause::Network => (
                ErrorKind::Network,
                None,
                format!("{tool} lost or could not reach the server: {summary}"),
                Some("Check the server (and SSH tunnel, if any) is reachable, then retry".into()),
            ),
            FailureCause::Tls => (
                ErrorKind::Tls,
                None,
                format!("{tool} could not establish a secure connection: {summary}"),
                Some("Check the connection's SSL mode and root certificate".into()),
            ),
            FailureCause::Disk => (
                ErrorKind::Disk,
                None,
                format!("{tool} could not write its output: {summary}"),
                Some("Free disk space or choose another backup folder".into()),
            ),
            FailureCause::VersionMismatch => (
                ErrorKind::ToolVersion,
                None,
                format!("{tool} is too old for this server or backup: {summary}"),
                Some(format!(
                    "Install newer PostgreSQL client tools so pgWhy can select a matching {tool}"
                )),
            ),
            FailureCause::ArchiveCorrupt => (
                ErrorKind::ToolFailed,
                None,
                format!("The backup file is unreadable or truncated: {summary}"),
                Some("Take a new backup; this file cannot be reused".into()),
            ),
            FailureCause::MissingObject => (
                ErrorKind::Config,
                None,
                format!("{tool} failed: {summary}"),
                Some("Check the database/table still exists".into()),
            ),
            FailureCause::Permission => (
                ErrorKind::Permission,
                None,
                format!("{tool} lacks a required privilege: {summary}"),
                Some("Connect as a role with the needed privileges".into()),
            ),
            FailureCause::ObjectErrors => {
                let n = run
                    .failed_items
                    .len()
                    .max(run.errors_ignored.unwrap_or(0) as usize);
                let first = run
                    .failed_items
                    .first()
                    .map(|i| match &i.object {
                        Some(o) => format!("{o}: {}", i.error),
                        None => i.error.clone(),
                    })
                    .unwrap_or(summary.clone());
                (
                    ErrorKind::ToolFailed,
                    None,
                    format!("pg_restore failed on {n} item(s); first: {first}"),
                    None,
                )
            }
            FailureCause::Killed => (
                ErrorKind::ToolFailed,
                Some(ErrorClass::Retriable),
                format!("{tool} was terminated unexpectedly"),
                None,
            ),
            FailureCause::Other => (
                ErrorKind::ToolFailed,
                None,
                format!("{tool} failed: {summary}"),
                None,
            ),
        };
    let mut err = AppError::new(kind, message)
        .with_detail(run.stderr_tail.join("\n"))
        .with_data(json!({
            "tool": tool.name(),
            "cause": cause,
            "exit_code": run.exit_code,
            "log_path": run.log_path,
            "failed_items": run.failed_items,
            "errors_ignored": run.errors_ignored,
            "stderr_tail": run.stderr_tail,
        }));
    if let Some(c) = class {
        err = err.with_class(c);
    }
    if let Some(h) = hint {
        err = err.with_hint(h);
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(tool: ToolKind, lines: &[&str], code: Option<i32>) -> ToolRun {
        let mut fc = FailureCollector::default();
        for l in lines {
            let ev = crate::progress::parse_line(tool, l);
            fc.observe(l, &ev, Some("INDEX public.t1_v_idx"));
        }
        ToolRun {
            tool,
            tool_path: "/x".into(),
            tool_version: "16.15".into(),
            args: vec![],
            exit_code: code,
            success: false,
            cancelled: false,
            duration_ms: 0,
            stderr_tail: lines.iter().map(|s| s.to_string()).collect(),
            log_path: None,
            failed_items: fc.items,
            errors_ignored: fc.errors_ignored,
            objects_done: 0,
            stdout: None,
        }
    }

    #[test]
    fn classifies_real_stderr_shapes() {
        let cases: &[(ToolKind, &str, FailureCause, ErrorKind)] = &[
            (
                ToolKind::PgDump,
                r#"pg_dump: error: connection to server at "127.0.0.1", port 55432 failed: FATAL:  password authentication failed for user "postgres""#,
                FailureCause::Auth,
                ErrorKind::Auth,
            ),
            (
                ToolKind::PgDump,
                r#"pg_dump: error: connection to server at "127.0.0.1", port 55499 failed: Connection refused"#,
                FailureCause::Network,
                ErrorKind::Network,
            ),
            (
                ToolKind::PgDump,
                r#"pg_dump: error: connection to server at "127.0.0.1", port 55432 failed: FATAL:  database "nodb" does not exist"#,
                FailureCause::MissingObject,
                ErrorKind::Config,
            ),
            (
                ToolKind::PgDump,
                "pg_dump: error: aborting because of server version mismatch",
                FailureCause::VersionMismatch,
                ErrorKind::ToolVersion,
            ),
            (
                ToolKind::PgDump,
                r#"pg_dump: error: could not open output file "/nonexistentdir/x.dump": No such file or directory"#,
                FailureCause::Disk,
                ErrorKind::Disk,
            ),
            (
                ToolKind::PgRestore,
                "pg_restore: error: could not read from input file: end of file",
                FailureCause::ArchiveCorrupt,
                ErrorKind::ToolFailed,
            ),
            (
                ToolKind::PgRestore,
                "pg_restore: error: input file does not appear to be a valid archive (too short?)",
                FailureCause::ArchiveCorrupt,
                ErrorKind::ToolFailed,
            ),
            (
                ToolKind::PgRestore,
                "pg_restore: error: unsupported version (1.16) in file header",
                FailureCause::VersionMismatch,
                ErrorKind::ToolVersion,
            ),
            (
                ToolKind::PgDump,
                "pg_dump: error: no matching tables were found",
                FailureCause::MissingObject,
                ErrorKind::Config,
            ),
            (
                ToolKind::PgDump,
                "pg_dump: error: query failed: server closed the connection unexpectedly",
                FailureCause::Network,
                ErrorKind::Network,
            ),
        ];
        for (tool, line, cause, kind) in cases {
            let r = run(*tool, &[line], Some(1));
            assert_eq!(classify(&r), *cause, "{line}");
            let e = classify_failure(&r);
            assert_eq!(e.kind, *kind, "{line}");
            assert_eq!(FailureCause::from_error(&e), Some(*cause));
        }
    }

    #[test]
    fn restore_object_errors_name_the_object() {
        let r = run(
            ToolKind::PgRestore,
            &[
                r#"pg_restore: creating INDEX "public.t1_v_idx""#,
                r#"pg_restore: error: could not execute query: ERROR:  relation "t1_v_idx" already exists"#,
                "Command was: CREATE INDEX t1_v_idx ON public.t1 USING btree (v);",
                "pg_restore: warning: errors ignored on restore: 1",
            ],
            Some(1),
        );
        assert_eq!(classify(&r), FailureCause::ObjectErrors);
        assert_eq!(r.errors_ignored, Some(1));
        let item = &r.failed_items[0];
        assert_eq!(item.object.as_deref(), Some("INDEX public.t1_v_idx"));
        assert!(item.command.as_deref().unwrap().starts_with("CREATE INDEX"));
        let e = classify_failure(&r);
        assert!(e.message.contains("INDEX public.t1_v_idx"), "{}", e.message);
    }

    #[test]
    fn real_continue_on_error_output_names_toc_entries() {
        let r = run(
            ToolKind::PgRestore,
            &[
                "pg_restore: while PROCESSING TOC:",
                "pg_restore: from TOC entry 6; 2615 16392 SCHEMA s2 postgres",
                r#"pg_restore: error: could not execute query: ERROR:  schema "s2" already exists"#,
                "Command was: CREATE SCHEMA s2;",
                "",
                "pg_restore: from TOC entry 3420; 0 16384 TABLE DATA t1 postgres",
                r#"pg_restore: error: COPY failed for table "t1": ERROR:  duplicate key value violates unique constraint "t1_pkey""#,
                "DETAIL:  Key (id)=(1) already exists.",
                "pg_restore: warning: errors ignored on restore: 2",
            ],
            Some(1),
        );
        assert_eq!(r.failed_items.len(), 2);
        assert_eq!(
            r.failed_items[0].object.as_deref(),
            Some("TOC entry 6; 2615 16392 SCHEMA s2 postgres")
        );
        assert_eq!(
            r.failed_items[0].command.as_deref(),
            Some("CREATE SCHEMA s2;")
        );
        assert!(r.failed_items[1].error.starts_with("COPY failed"));
        assert_eq!(r.failed_items[1].command, None);
        assert_eq!(classify(&r), FailureCause::ObjectErrors);
    }

    #[test]
    fn signal_death_is_retriable() {
        let r = run(ToolKind::PgDump, &[], None);
        let e = classify_failure(&r);
        assert_eq!(classify(&r), FailureCause::Killed);
        assert!(e.is_retriable());
    }
}
