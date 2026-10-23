//! Per-run temporary pgpass files — the single sanctioned transient on-disk form of a
//! credential (R20).
//!
//! Lifecycle: one randomly-named file per child run, inside a dedicated app-owned directory
//! ([`PgPassDir`]), created atomically with owner-only permissions (POSIX `O_CREAT|O_EXCL`
//! with mode 0600 — never create-then-chmod; libpq also silently ignores a looser file), and
//! deleted by [`PgPassFile`]'s `Drop` on every exit path. A crash can still orphan a file, so
//! the app sweeps the directory at startup ([`sweep_stale_pgpass`]). File names embed the
//! owning process id, so a second running pgWhy's in-flight files are never swept.
//!
//! The same directory also holds the short-lived system-roots PEM handed to a child as
//! `PGSSLROOTCERT` ([`PgPassDir::create_root_cert`]) — same creation rules, same cleanup.
//!
//! Entries are keyed exactly by what the child presents — the REAL hostname (libpq matches
//! the `host` parameter, not `hostaddr`), the port it dials (the tunnel port when tunneled),
//! database and user — with every field escaped so none can act as a `*` wildcard.
//!
//! Windows: files are created inside the per-user temp directory (`%LOCALAPPDATA%\Temp`),
//! whose inherited ACL already grants access only to the user, SYSTEM and Administrators;
//! no explicit ACL is applied (documented best-effort per the plan).

use crate::endpoint::{ToolEndpoint, ToolPassword};
use pgwhy_errors::{AppError, AppResult, ErrorKind};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const FILE_PREFIX: &str = "pgwhy-run-";
const FILE_SUFFIX: &str = ".pgpass";
const CERT_SUFFIX: &str = ".rootcert.pem";
/// A file this old is stale even if its (possibly reused) owner pid is alive, and is the only
/// staleness test where liveness cannot be probed (non-unix, or an unparsable name).
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The dedicated directory holding per-run pgpass files.
#[derive(Debug, Clone)]
pub struct PgPassDir {
    path: PathBuf,
}

impl PgPassDir {
    /// Use (creating if needed, mode 0700) `path` as the pgpass directory. On POSIX an
    /// existing directory must be a real directory (not a symlink), owned by this user, and
    /// not accessible to group/others — otherwise it is refused rather than trusted.
    pub fn new(path: impl Into<PathBuf>) -> AppResult<Self> {
        let path = path.into();
        create_private_dir(&path)?;
        verify_private_dir(&path)?;
        Ok(Self { path })
    }

    /// `<system temp>/pgwhy-pgpass` (per-user `$TMPDIR` on macOS, `%TEMP%` on Windows).
    /// Prefer an app-cache subdirectory when the caller has one.
    pub fn default_location() -> PathBuf {
        std::env::temp_dir().join(user_scoped_name())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write a single-entry pgpass file for `endpoint` + `password`.
    pub fn create(
        &self,
        endpoint: &ToolEndpoint,
        password: &ToolPassword,
    ) -> AppResult<PgPassFile> {
        let line = entry_line(endpoint, password)?;
        self.create_file(FILE_SUFFIX, line.as_bytes(), "credentials")
    }

    /// Write `pem` (CA certificates) to a private temp file for the child's `PGSSLROOTCERT`.
    pub fn create_root_cert(&self, pem: &str) -> AppResult<PgPassFile> {
        self.create_file(CERT_SUFFIX, pem.as_bytes(), "certificate")
    }

    fn create_file(&self, suffix: &str, contents: &[u8], what: &str) -> AppResult<PgPassFile> {
        let name = format!(
            "{FILE_PREFIX}{}-{}{suffix}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let path = self.path.join(name);
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
            // Never follow a planted symlink.
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = opts.open(&path).map_err(|e| {
            AppError::new(
                ErrorKind::Disk,
                format!("Could not create the temporary {what} file for pg_dump/pg_restore"),
            )
            .with_detail(format!("{}: {e}", path.display()))
        })?;
        // The guard exists before the write so a failed write still deletes the file.
        let guard = PgPassFile { path };
        file.write_all(contents)
            .and_then(|_| file.sync_all())
            .map_err(|e| {
                AppError::new(
                    ErrorKind::Disk,
                    format!("Could not write the temporary {what} file"),
                )
                .with_detail(e.to_string())
            })?;
        Ok(guard)
    }
}

#[cfg(unix)]
fn user_scoped_name() -> String {
    // SAFETY: geteuid has no preconditions.
    format!("pgwhy-pgpass-{}", unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn user_scoped_name() -> String {
    "pgwhy-pgpass".into()
}

fn create_private_dir(path: &Path) -> AppResult<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| {
        AppError::new(
            ErrorKind::Disk,
            "Could not create pgWhy's temporary credentials folder",
        )
        .with_detail(format!("{}: {e}", path.display()))
    })
}

fn verify_private_dir(path: &Path) -> AppResult<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        AppError::new(
            ErrorKind::Disk,
            "pgWhy's temporary credentials folder is unusable",
        )
        .with_detail(format!("{}: {e}", path.display()))
    })?;
    let unsafe_dir = |why: &str| {
        AppError::new(
            ErrorKind::Config,
            "pgWhy's temporary credentials folder is not private, so it was not used",
        )
        .with_detail(format!("{}: {why}", path.display()))
        .with_hint("Delete the folder (pgWhy recreates it) or choose another location")
    };
    if !meta.is_dir() {
        return Err(unsafe_dir("not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(unsafe_dir("owned by another user"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(unsafe_dir("accessible to other users"));
        }
    }
    Ok(())
}

/// A live per-run pgpass file; deleted on drop. Point the child's `PGPASSFILE` at
/// [`PgPassFile::path`].
#[derive(Debug)]
pub struct PgPassFile {
    path: PathBuf,
}

impl PgPassFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Delete now, reporting failure (Drop also deletes, silently).
    pub fn delete(self) -> AppResult<()> {
        let res = remove_if_present(&self.path);
        std::mem::forget(self);
        res.map_err(|e| {
            AppError::new(
                ErrorKind::Disk,
                "Could not delete the temporary credentials file",
            )
            .with_detail(e.to_string())
        })
    }
}

impl Drop for PgPassFile {
    fn drop(&mut self) {
        if let Err(e) = remove_if_present(&self.path) {
            tracing::warn!(path = %self.path.display(), error = %e, "failed to delete temp pgpass");
        }
    }
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The pgpass line for an endpoint: `host:port:dbname:user:password\n`.
fn entry_line(endpoint: &ToolEndpoint, password: &ToolPassword) -> AppResult<String> {
    let pw = password.expose();
    if pw.contains(['\n', '\r', '\0']) {
        return Err(AppError::new(
            ErrorKind::Credentials,
            "The password contains a line break, which pg_dump/pg_restore cannot accept",
        ));
    }
    // libpq matches pgpass against "localhost" when host is a socket directory.
    let host = if endpoint.host.starts_with('/') {
        "localhost"
    } else {
        endpoint.host.as_str()
    };
    Ok(format!(
        "{}:{}:{}:{}:{}\n",
        escape_field(host),
        endpoint.port,
        escape_field(&endpoint.dbname),
        escape_field(&endpoint.user),
        escape_field(pw)
    ))
}

/// Escape `\`, `:` and `*` with a backslash. libpq only treats a field as a wildcard when it
/// is a bare unescaped `*`; an escaped character always compares literally.
fn escape_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | ':' | '*') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Remove leftover per-run files (pgpass and root-cert) from a crashed or killed prior run.
/// Call once at app startup. Files whose owning process is still alive (another pgWhy
/// instance mid-backup) or that are younger than a day with an unknown owner are kept.
/// Returns how many were removed; unrelated files are left.
pub fn sweep_stale_pgpass(dir: &Path) -> AppResult<usize> {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(AppError::new(
                ErrorKind::Disk,
                "Could not scan the temporary credentials folder",
            )
            .with_detail(e.to_string()))
        }
    };
    let mut removed = 0;
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(FILE_PREFIX)
            && (name.ends_with(FILE_SUFFIX) || name.ends_with(CERT_SUFFIX))
            && is_stale(&name, &entry)
        {
            match remove_if_present(&entry.path()) {
                Ok(()) => removed += 1,
                Err(e) => {
                    tracing::warn!(path = %entry.path().display(), error = %e, "pgpass sweep failed")
                }
            }
        }
    }
    Ok(removed)
}

/// `pgwhy-run-<pid>-<random>.<ext>`: stale when the owner is dead, or the file is very old.
fn is_stale(name: &str, entry: &fs::DirEntry) -> bool {
    let owner = name
        .strip_prefix(FILE_PREFIX)
        .and_then(|r| r.split_once('-'))
        .and_then(|(pid, _)| pid.parse::<u32>().ok());
    if owner.is_some_and(|pid| !process_alive(pid)) {
        return true;
    }
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= STALE_AFTER)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes for existence. EPERM means it exists (another user's).
    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// No cheap liveness probe: never treat the owner as dead; the age test decides.
#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep() -> ToolEndpoint {
        ToolEndpoint {
            host: "db.example.com".into(),
            hostaddr: Some("127.0.0.1".parse().unwrap()),
            port: 61000,
            user: "u:*".into(),
            dbname: "*".into(),
            sslmode: "prefer".into(),
            sslrootcert: None,
        }
    }

    #[test]
    fn entry_keyed_by_real_host_and_tunnel_port_without_wildcards() {
        let line = entry_line(&ep(), &ToolPassword::new(r"p:a\ss*")).unwrap();
        assert_eq!(line, "db.example.com:61000:\\*:u\\:\\*:p\\:a\\\\ss\\*\n");
        for field in ["db.example.com", "61000"] {
            assert!(line.contains(field));
        }
        // no field is a bare '*'
        assert!(!line.split(':').any(|f| f == "*"));
    }

    #[test]
    fn password_with_newline_is_refused() {
        let err = entry_line(&ep(), &ToolPassword::new("a\nb")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Credentials);
        assert!(!err.message.contains("a\nb"));
    }
}
