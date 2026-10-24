//! Recently used scripts.
//!
//! Tracked automatically on open/save/run, most recent first, capped at
//! [`MAX_RECENT_SCRIPTS`]. Each entry remembers the connection and database it last *ran*
//! against so reopening restores that binding (HG-2). Entries can be loose files anywhere on
//! disk or library scripts; library scripts are matched by [`ScriptId`] so a rename or move
//! inside the library does not orphan the recent.
//!
//! Persisted in the app config directory (not the library), so absolute paths are fine here.

use super::library::{Library, ScriptId};
use super::now_ms;
use crate::config::{ConfigStore, ConnectionId};
use crate::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Config file (under [`ConfigStore::dir`]) holding the recents list.
pub const RECENTS_FILE_NAME: &str = "script-recents.json";

/// How many recents are kept.
pub const MAX_RECENT_SCRIPTS: usize = 30;

/// What the user last did with a recent script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecentAction {
    Open,
    Save,
    Run,
}

/// One remembered script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentScript {
    /// Absolute path as last used. For library scripts this may be stale after an external
    /// move; [`Recents::status`] resolves the current path through the library.
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_id: Option<ScriptId>,
    /// Connection the script last ran against (only runs set it; open/save keep it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<ConnectionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Ms since the Unix epoch.
    pub last_used_ms: u64,
    pub last_action: RecentAction,
}

/// A use to record (the argument to [`Recents::record`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentUse {
    /// Absolute path of the script file.
    pub path: PathBuf,
    #[serde(default)]
    pub library_id: Option<ScriptId>,
    pub action: RecentAction,
    /// Set for runs: the connection + database that received the script.
    #[serde(default)]
    pub connection_id: Option<ConnectionId>,
    #[serde(default)]
    pub database: Option<String>,
}

/// Availability of a recent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecentState {
    Available,
    /// Nothing at the resolved path: the UI offers "remove from recents" and "browse".
    NotFound,
}

/// A recent with its resolved location and availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentStatus {
    pub entry: RecentScript,
    /// Where the script is now: the library's current path for its id when known,
    /// otherwise the recorded path.
    pub resolved_path: PathBuf,
    pub state: RecentState,
}

/// The recents list, most recent first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recents {
    #[serde(default)]
    entries: Vec<RecentScript>,
}

impl Recents {
    /// Load from the config directory; a missing file is an empty list.
    pub fn load(store: &ConfigStore) -> AppResult<Self> {
        let mut r: Recents = store.load_json(RECENTS_FILE_NAME)?;
        r.normalize();
        Ok(r)
    }

    pub fn save(&self, store: &ConfigStore) -> AppResult<()> {
        store.save_json(RECENTS_FILE_NAME, self)
    }

    /// Most recent first.
    pub fn entries(&self) -> &[RecentScript] {
        &self.entries
    }

    /// Record a use now.
    pub fn record(&mut self, used: RecentUse) -> AppResult<()> {
        self.record_at(used, now_ms())
    }

    /// Record a use at an explicit time (ms since epoch). The entry is matched by library id
    /// when given, else by path; it moves to the front and the list is capped.
    pub fn record_at(&mut self, used: RecentUse, at_ms: u64) -> AppResult<()> {
        if !used.path.is_absolute() {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("recent script path {} is not absolute", used.path.display()),
            ));
        }
        let mut previous: Option<RecentScript> = None;
        // Remove every entry that is the same script by id or by path (a loose file later
        // saved into the library, or a library script now at a path an old entry used).
        self.entries.retain(|e| {
            let same_id = used.library_id.is_some() && e.library_id == used.library_id;
            let same_path = e.path == used.path;
            if same_id || same_path {
                if previous.is_none() || same_id {
                    previous = Some(e.clone());
                }
                false
            } else {
                true
            }
        });
        let (mut connection_id, mut database) = previous
            .map(|p| (p.connection_id, p.database))
            .unwrap_or((None, None));
        if used.connection_id.is_some() {
            connection_id = used.connection_id;
            database = used.database;
        }
        self.entries.insert(
            0,
            RecentScript {
                path: used.path,
                library_id: used.library_id,
                connection_id,
                database,
                last_used_ms: at_ms,
                last_action: used.action,
            },
        );
        self.normalize();
        Ok(())
    }

    /// Remove the recent whose recorded path is `path`. Returns whether one was removed.
    pub fn remove(&mut self, path: &Path) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        self.entries.len() != before
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Resolve each recent's current location and availability. With a library, recents
    /// that carry its script ids follow renames/moves that a rescan picked up.
    pub fn status(&self, library: Option<&Library>) -> Vec<RecentStatus> {
        self.entries
            .iter()
            .map(|e| {
                let resolved_path = self
                    .library_path(e, library)
                    .unwrap_or_else(|| e.path.clone());
                let state = if resolved_path.is_file() {
                    RecentState::Available
                } else {
                    RecentState::NotFound
                };
                RecentStatus {
                    entry: e.clone(),
                    resolved_path,
                    state,
                }
            })
            .collect()
    }

    /// Rewrite recorded paths of library recents to the library's current paths (after a
    /// rescan detected moves). Returns whether anything changed, i.e. whether to save.
    pub fn sync_with_library(&mut self, library: &Library) -> bool {
        let mut changed = false;
        for i in 0..self.entries.len() {
            if let Some(p) = self.library_path(&self.entries[i], Some(library)) {
                if p != self.entries[i].path {
                    self.entries[i].path = p;
                    changed = true;
                }
            }
        }
        if changed {
            self.normalize();
        }
        changed
    }

    fn library_path(&self, e: &RecentScript, library: Option<&Library>) -> Option<PathBuf> {
        let (lib, id) = (library?, e.library_id.as_ref()?);
        lib.get(id).filter(|s| !s.missing)?;
        lib.absolute_path(id)
    }

    /// Most recent first (stable for equal times), unique paths, capped.
    fn normalize(&mut self) {
        self.entries
            .sort_by_key(|e| std::cmp::Reverse(e.last_used_ms));
        let mut seen = std::collections::HashSet::new();
        self.entries.retain(|e| seen.insert(e.path.clone()));
        self.entries.truncate(MAX_RECENT_SCRIPTS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/x/a.sql` has no drive letter, so it is not absolute on Windows.
    fn abs(path: &str) -> PathBuf {
        match path.strip_prefix('/') {
            Some(rest) if cfg!(windows) => PathBuf::from(format!("C:\\{rest}")),
            _ => PathBuf::from(path),
        }
    }

    fn use_(path: &str, action: RecentAction) -> RecentUse {
        RecentUse {
            path: abs(path),
            library_id: None,
            action,
            connection_id: None,
            database: None,
        }
    }

    #[test]
    fn relative_paths_are_rejected() {
        let mut r = Recents::default();
        assert!(r.record(use_("rel.sql", RecentAction::Open)).is_err());
    }

    #[test]
    fn open_after_run_keeps_connection() {
        let mut r = Recents::default();
        let mut run = use_("/x/a.sql", RecentAction::Run);
        run.connection_id = Some(ConnectionId("local".into()));
        run.database = Some("app".into());
        r.record_at(run, 1).unwrap();
        r.record_at(use_("/x/a.sql", RecentAction::Open), 2)
            .unwrap();
        let e = &r.entries()[0];
        assert_eq!(e.connection_id, Some(ConnectionId("local".into())));
        assert_eq!(e.database.as_deref(), Some("app"));
        assert_eq!(e.last_action, RecentAction::Open);
        assert_eq!(r.entries().len(), 1);
    }

    #[test]
    fn library_id_merges_with_loose_path_entry() {
        let mut r = Recents::default();
        r.record_at(use_("/lib/a.sql", RecentAction::Open), 1)
            .unwrap();
        let mut saved = use_("/lib/b.sql", RecentAction::Save);
        saved.library_id = Some(ScriptId("s1".into()));
        r.record_at(saved.clone(), 2).unwrap();
        // Same id at a new path replaces the old entry rather than duplicating it.
        saved.path = abs("/lib/moved/b.sql");
        r.record_at(saved, 3).unwrap();
        let paths: Vec<_> = r.entries().iter().map(|e| e.path.clone()).collect();
        assert_eq!(paths, [abs("/lib/moved/b.sql"), abs("/lib/a.sql")]);
    }
}
