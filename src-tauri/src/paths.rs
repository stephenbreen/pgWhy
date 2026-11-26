//! Which filesystem paths the webview may name in a command.
//!
//! The webview is not trusted with arbitrary paths: a compromised page could otherwise read or
//! overwrite any file the user can, or point the pg tools at a directory of its choosing. A path
//! is acceptable when it came from a native dialog the *backend* showed (recorded here), lies
//! inside the script library or backup folder, or is in the recents list.

use crate::state::AppState;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Paths and folders the user picked in a backend-shown dialog during this session.
#[derive(Default)]
pub struct PathGrants {
    /// Files granted for read and write (open/save dialogs for scripts, exports, Save-As backups).
    files: Mutex<HashSet<PathBuf>>,
    /// Files granted for reading only (e.g. restore archive picker).
    read_files: Mutex<HashSet<PathBuf>>,
    dirs: Mutex<HashSet<PathBuf>>,
}

/// Resolve symlinks and `..` where the path (or any ancestor) exists, and lexically collapse
/// `.` / `..` on any non-existent suffix so `..` can never survive into a `starts_with` check.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    // Walk up until we find the deepest existing ancestor we can canonicalize, keeping the tail.
    let mut existing = abs.as_path();
    let mut tail: Vec<Component<'_>> = Vec::new();
    let base = loop {
        if let Ok(canon) = existing.canonicalize() {
            break canon;
        }
        match existing.components().next_back() {
            Some(comp) => {
                tail.push(comp);
                match existing.parent() {
                    Some(parent) if parent != existing => existing = parent,
                    _ => break PathBuf::new(),
                }
            }
            None => break PathBuf::new(),
        }
    };
    let mut out = base;
    for comp in tail.into_iter().rev() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::Prefix(p) => out.push(p.as_os_str()),
        }
    }
    // If collapsing `..` landed on an existing path, resolve any symlinks at the destination.
    out.canonicalize().unwrap_or(out)
}

impl PathGrants {
    /// Grant read + write access to a file chosen in a backend dialog.
    pub fn grant_file(&self, path: &Path) {
        self.files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(normalize(path));
    }

    /// Grant read-only access to a file chosen in a backend dialog (e.g. restore archive).
    pub fn grant_read_file(&self, path: &Path) {
        self.read_files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(normalize(path));
    }

    pub fn grant_dir(&self, path: &Path) {
        self.dirs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(normalize(path));
    }

    fn has_write_file(&self, norm: &Path) -> bool {
        self.files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(norm)
    }

    fn has_file(&self, norm: &Path) -> bool {
        self.has_write_file(norm)
            || self
                .read_files
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(norm)
    }

    pub fn has_dir(&self, path: &Path) -> bool {
        self.dirs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&normalize(path))
    }
}

fn refused(path: &Path) -> AppError {
    AppError::new(
        ErrorKind::Refused,
        format!("{} was not chosen in a file dialog", path.display()),
    )
    .with_hint(
        "open or save the file through pgWhy's file dialog, or use a file in the script library",
    )
}

impl AppState {
    pub fn backup_dir(&self) -> PathBuf {
        self.settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .backup_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("backups"))
    }

    /// Accept `path` for a read or reveal command, or refuse it. `reveal` also admits the app's
    /// own data folder (run logs) excluding the temporary `pgpass` credential folder.
    pub fn check_path(&self, path: &Path, reveal: bool) -> AppResult<PathBuf> {
        let norm = normalize(path);
        if norm.starts_with(normalize(&self.data_dir.join("pgpass"))) {
            return Err(refused(path));
        }
        let mut roots = vec![self.library_dir(), self.backup_dir()];
        if reveal {
            roots.push(self.data_dir.clone());
        }
        let ok = self.grants.has_file(&norm)
            || roots.iter().any(|r| norm.starts_with(normalize(r)))
            || self
                .script_recents
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entries()
                .iter()
                .any(|r| normalize(&r.path) == norm)
            || self.history_owns(&norm);
        if ok {
            Ok(norm)
        } else {
            Err(refused(path))
        }
    }

    /// Accept `path` for writing (script saves, result exports), or refuse it. Stricter than
    /// [`check_path`](Self::check_path): excludes `backup_dir`, history-owned backups, and
    /// read-only dialog picks (such as restore archives).
    pub fn check_write_path(&self, path: &Path) -> AppResult<PathBuf> {
        let norm = normalize(path);
        if norm.starts_with(normalize(&self.data_dir.join("pgpass"))) {
            return Err(refused(path));
        }
        let ok = self.grants.has_write_file(&norm)
            || norm.starts_with(normalize(&self.library_dir()))
            || self
                .script_recents
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entries()
                .iter()
                .any(|r| normalize(&r.path) == norm);
        if ok {
            Ok(norm)
        } else {
            Err(refused(path))
        }
    }

    /// Accept `path` as a new backup file chosen with "Save as…", or refuse it. Stricter than
    /// [`check_path`](Self::check_path): only a save dialog can name a place to write a database
    /// dump. The app's own backup folder is refused because retention deletes old backups there,
    /// and a file the user placed by hand must never be one of them.
    pub fn check_save_as_path(&self, path: &Path) -> AppResult<PathBuf> {
        let norm = normalize(path);
        if !self.grants.has_write_file(&norm) {
            return Err(refused(path));
        }
        if norm.starts_with(normalize(&self.backup_dir())) {
            return Err(AppError::new(
                ErrorKind::Config,
                "Save as can't use pgWhy's backup folder, where old backups are removed automatically",
            )
            .with_hint("Choose another folder, or use the default location"));
        }
        if norm.exists() {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("A backup already exists at {}", norm.display()),
            )
            .with_hint("Choose a new file name; pgWhy never overwrites an existing backup"));
        }
        Ok(norm)
    }

    /// True when a history record owns the backup at `norm`: the app wrote both, so "Restore
    /// backup…" and "Reveal" keep working after the backup folder moves. Only consulted once the
    /// cheap checks have failed, because it reads the history folder.
    fn history_owns(&self, norm: &Path) -> bool {
        self.engine()
            .history()
            .list_entries()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e.backup.as_ref())
                    .any(|b| normalize(&b.path) == norm)
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotdot_cannot_escape_a_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(dir.path().join("secret"), "x").unwrap();
        let sneaky = root.join("..").join("secret");
        assert!(!normalize(&sneaky).starts_with(normalize(&root)));
        assert!(normalize(&root.join("a.sql")).starts_with(normalize(&root)));
    }

    #[test]
    fn dotdot_through_nonexistent_subdir_cannot_escape_a_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        std::fs::create_dir_all(&root).unwrap();
        let sneaky = root
            .join("nonexistent")
            .join("..")
            .join("..")
            .join("escaped.sql");
        let norm = normalize(&sneaky);
        assert!(
            !norm.starts_with(normalize(&root)),
            "expected {} not to be inside {}",
            norm.display(),
            root.display()
        );
        assert_eq!(norm, normalize(&dir.path().join("escaped.sql")));
    }

    #[test]
    fn grants_match_normalized_paths() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.sql");
        std::fs::write(&f, "").unwrap();
        let g = PathGrants::default();
        assert!(!g.has_file(&normalize(&f)));
        g.grant_file(&f);
        assert!(g.has_file(&normalize(&dir.path().join(".").join("x.sql"))));
        assert!(g.has_write_file(&normalize(&f)));
    }

    #[test]
    fn read_only_grant_does_not_allow_write() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("archive.dump");
        std::fs::write(&f, "").unwrap();
        let g = PathGrants::default();
        g.grant_read_file(&f);
        assert!(g.has_file(&normalize(&f)));
        assert!(!g.has_write_file(&normalize(&f)));
    }
}
