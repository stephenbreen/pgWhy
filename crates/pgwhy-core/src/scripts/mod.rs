//! Script workbench core: library index with stable ids, recents, open-buffer file
//! states, psql meta-command scan.
//!
//! - [`Library`]: a directory tree of plain `.sql` files plus a sidecar index
//!   ([`INDEX_FILE_NAME`]) that gives each script a stable [`ScriptId`] which survives
//!   renames, moves and git operations. Workflows reference scripts by that id.
//! - [`Recents`]: auto-tracked recently used scripts (library or loose files) with the
//!   connection + database each last ran against, persisted via [`crate::config::ConfigStore`].
//! - [`check_file`]: change detection for open editor buffers.
//! - [`scan_meta_commands`]: detection of psql backslash commands outside string,
//!   identifier, dollar-quote and comment context.
//!
//! Nothing here touches a database.

mod file_state;
mod library;
mod meta;
mod recents;

pub use file_state::{
    check_file, fingerprint_file, read_script_file, write_script_file, FileCheck, FileFingerprint,
    FileState, ScriptFile,
};
pub use library::{Library, RescanReport, ScriptId, ScriptInfo, ScriptMove, INDEX_FILE_NAME};
pub use meta::{has_meta_commands, scan_meta_commands, MetaCommand, MetaPosition};
pub use recents::{
    RecentAction, RecentScript, RecentState, RecentStatus, RecentUse, Recents, MAX_RECENT_SCRIPTS,
    RECENTS_FILE_NAME,
};

use crate::{AppError, AppResult, ErrorKind};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Lower-case hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        // Writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Milliseconds since the Unix epoch; `0` if the clock is before 1970.
pub(crate) fn now_ms() -> u64 {
    system_time_ms(SystemTime::now()).unwrap_or(0)
}

pub(crate) fn system_time_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
}

/// Map a filesystem error to an [`AppError`]. Paths are not secrets, so they are named.
pub(crate) fn fs_error(path: &Path, action: &str, e: std::io::Error) -> AppError {
    let kind = if e.kind() == std::io::ErrorKind::StorageFull {
        ErrorKind::Disk
    } else {
        ErrorKind::Config
    };
    AppError::new(kind, format!("could not {action} {}", path.display())).with_detail(e.to_string())
}

/// Atomically replace `path` with `bytes`, keeping the previous file's permissions.
///
/// The temp-file-and-rename write creates a fresh inode with owner-only permissions; users'
/// `.sql` files often live in shared repos, so the original mode (or a conventional 0644 for
/// new files) is restored afterwards.
pub(crate) fn write_file_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let previous = std::fs::metadata(path).ok().map(|m| m.permissions());
    crate::config::write_atomic(path, bytes)?;
    let perms = match previous {
        Some(p) => Some(p),
        None => default_new_file_permissions(),
    };
    if let Some(perms) = perms {
        // Best effort: the content is already safely written.
        if let Err(e) = std::fs::set_permissions(path, perms) {
            tracing::warn!(path = %path.display(), error = %e, "could not restore file permissions");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn default_new_file_permissions() -> Option<std::fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    Some(std::fs::Permissions::from_mode(0o644))
}

#[cfg(not(unix))]
fn default_new_file_permissions() -> Option<std::fs::Permissions> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
