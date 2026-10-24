//! File states for open editor buffers, and plain read/write of script files that may
//! live outside the library.
//!
//! The editor remembers the [`FileFingerprint`] of what it loaded (or last saved). On focus it
//! asks [`check_file`] what happened on disk since: unchanged → nothing; changed → reload a
//! clean buffer, prompt for a dirty one; deleted → keep the buffer and mark it "deleted on
//! disk".

use super::{fs_error, sha256_hex, system_time_ms, write_file_atomic};
use crate::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What the app last knew about a file's bytes on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// Lower-case hex SHA-256 of the file's bytes. The authority for "did it change".
    pub sha256: String,
    pub size: u64,
    /// Modification time in ms since the Unix epoch, when the platform reports one.
    /// Informational only: checkouts and copies change it without changing content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
}

impl FileFingerprint {
    pub(crate) fn of(bytes: &[u8], meta: Option<&std::fs::Metadata>) -> Self {
        Self {
            sha256: sha256_hex(bytes),
            size: bytes.len() as u64,
            modified_ms: meta
                .and_then(|m| m.modified().ok())
                .and_then(system_time_ms),
        }
    }
}

/// A script file's text together with the fingerprint of exactly those bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptFile {
    pub content: String,
    pub fingerprint: FileFingerprint,
}

/// On-disk state of a file relative to a buffer's loaded fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// Same bytes as loaded (the mtime may still have moved, e.g. `touch` or a checkout).
    Unchanged,
    /// Different bytes on disk: reload a clean buffer, prompt for a dirty one.
    ChangedOnDisk,
    /// Nothing at the path any more: keep the buffer, mark it "deleted on disk".
    DeletedOnDisk,
}

/// Result of [`check_file`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCheck {
    pub state: FileState,
    /// The current fingerprint when the file exists, so the editor can adopt it (a clean
    /// reload, or an `Unchanged` file whose mtime moved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<FileFingerprint>,
}

/// Compare the file at `path` with the fingerprint the editor loaded.
///
/// Always hashes the content rather than trusting size + mtime: buffers are checked on focus,
/// not in a loop, and an mtime-only check misses same-size edits within one timestamp tick.
pub fn check_file(path: &Path, loaded: &FileFingerprint) -> AppResult<FileCheck> {
    let bytes = match read_bytes(path)? {
        Some(b) => b,
        None => {
            return Ok(FileCheck {
                state: FileState::DeletedOnDisk,
                current: None,
            })
        }
    };
    let meta = std::fs::metadata(path).ok();
    let current = FileFingerprint::of(&bytes, meta.as_ref());
    let state = if current.sha256 == loaded.sha256 {
        FileState::Unchanged
    } else {
        FileState::ChangedOnDisk
    };
    Ok(FileCheck {
        state,
        current: Some(current),
    })
}

/// Fingerprint the file at `path` (it must exist).
pub fn fingerprint_file(path: &Path) -> AppResult<FileFingerprint> {
    let bytes = read_bytes(path)?.ok_or_else(|| not_found(path))?;
    let meta = std::fs::metadata(path).ok();
    Ok(FileFingerprint::of(&bytes, meta.as_ref()))
}

/// Read a script file as UTF-8 text with the fingerprint of the bytes read.
pub fn read_script_file(path: &Path) -> AppResult<ScriptFile> {
    let bytes = read_bytes(path)?.ok_or_else(|| not_found(path))?;
    let meta = std::fs::metadata(path).ok();
    let fingerprint = FileFingerprint::of(&bytes, meta.as_ref());
    let content = String::from_utf8(bytes).map_err(|_| {
        AppError::new(
            ErrorKind::Config,
            format!("{} is not UTF-8 text", path.display()),
        )
        .with_hint("pgWhy edits scripts as UTF-8; convert the file's encoding and reopen it")
    })?;
    Ok(ScriptFile {
        content,
        fingerprint,
    })
}

/// Atomically write `content` to `path` (creating parent directories) and return the new
/// fingerprint, which the editor stores as its loaded state.
pub fn write_script_file(path: &Path, content: &str) -> AppResult<FileFingerprint> {
    write_file_atomic(path, content.as_bytes())?;
    let meta = std::fs::metadata(path).ok();
    Ok(FileFingerprint::of(content.as_bytes(), meta.as_ref()))
}

/// `Ok(None)` when nothing (or a directory) is at `path`.
pub(crate) fn read_bytes(path: &Path) -> AppResult<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) if path.is_dir() => Ok(None),
        Err(e) => Err(fs_error(path, "read", e)),
    }
}

pub(crate) fn not_found(path: &Path) -> AppError {
    AppError::new(
        ErrorKind::Config,
        format!("{} was not found", path.display()),
    )
    .with_hint("it may have been moved, renamed or deleted outside pgWhy")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_unchanged_changed_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sql");
        let loaded = write_script_file(&path, "select 1;").unwrap();
        assert_eq!(
            check_file(&path, &loaded).unwrap().state,
            FileState::Unchanged
        );

        // Same size, different bytes: must still be detected.
        std::fs::write(&path, "select 2;").unwrap();
        let check = check_file(&path, &loaded).unwrap();
        assert_eq!(check.state, FileState::ChangedOnDisk);
        assert_ne!(check.current.unwrap().sha256, loaded.sha256);

        std::fs::remove_file(&path).unwrap();
        let check = check_file(&path, &loaded).unwrap();
        assert_eq!(check.state, FileState::DeletedOnDisk);
        assert!(check.current.is_none());
    }

    #[test]
    fn read_returns_fingerprint_of_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/b.sql");
        let written = write_script_file(&path, "select 'é';").unwrap();
        let read = read_script_file(&path).unwrap();
        assert_eq!(read.content, "select 'é';");
        assert_eq!(read.fingerprint.sha256, written.sha256);
        assert_eq!(read.fingerprint.size, "select 'é';".len() as u64);
    }

    #[test]
    fn non_utf8_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.sql");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(read_script_file(&path).unwrap_err().kind, ErrorKind::Config);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.sql");
        write_script_file(&path, "a").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        write_script_file(&path, "b").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o664
        );
    }
}
