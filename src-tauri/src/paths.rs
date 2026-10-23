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
    files: Mutex<HashSet<PathBuf>>,
    dirs: Mutex<HashSet<PathBuf>>,
}

/// Resolve symlinks and `..` where the path exists; for a path to be created (a save target),
/// resolve its parent and keep the file name.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => parent
            .canonicalize()
            .map(|p| p.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

impl PathGrants {
    pub fn grant_file(&self, path: &Path) {
        self.files
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

    fn has_file(&self, norm: &Path) -> bool {
        self.files
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

    /// Accept `path` for a file command, or refuse it. `reveal` also admits the app's own data
    /// folder (run logs), which may be shown in the file manager but never read or written.
    pub fn check_path(&self, path: &Path, reveal: bool) -> AppResult<PathBuf> {
        let norm = normalize(path);
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
    fn grants_match_normalized_paths() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.sql");
        std::fs::write(&f, "").unwrap();
        let g = PathGrants::default();
        assert!(!g.has_file(&normalize(&f)));
        g.grant_file(&f);
        assert!(g.has_file(&normalize(&dir.path().join(".").join("x.sql"))));
    }
}
