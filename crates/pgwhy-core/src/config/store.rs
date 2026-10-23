//! File persistence for config: one JSON file per concern, written atomically
//! (temp file in the same directory + rename) so a crash never leaves a torn file.

use super::{AppSettings, ConnectionEntry, ConnectionId};
use crate::{AppError, AppResult, ErrorKind};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

const CONNECTIONS_FILE: &str = "connections.json";
const SETTINGS_FILE: &str = "settings.json";
const MAX_RECENTS: usize = 10;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsFile {
    #[serde(default)]
    pub connections: Vec<ConnectionEntry>,
    /// Most recently used first.
    #[serde(default)]
    pub recents: Vec<ConnectionId>,
}

impl ConnectionsFile {
    pub fn get(&self, id: &ConnectionId) -> Option<&ConnectionEntry> {
        self.connections.iter().find(|c| &c.id == id)
    }

    /// Insert or replace by id.
    pub fn upsert(&mut self, entry: ConnectionEntry) {
        match self.connections.iter_mut().find(|c| c.id == entry.id) {
            Some(existing) => *existing = entry,
            None => self.connections.push(entry),
        }
    }

    pub fn remove(&mut self, id: &ConnectionId) -> Option<ConnectionEntry> {
        self.recents.retain(|r| r != id);
        let idx = self.connections.iter().position(|c| &c.id == id)?;
        Some(self.connections.remove(idx))
    }

    pub fn touch_recent(&mut self, id: &ConnectionId) {
        self.recents.retain(|r| r != id);
        self.recents.insert(0, id.clone());
        self.recents.truncate(MAX_RECENTS);
    }
}

/// Reads and writes config files under one directory.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    dir: PathBuf,
}

impl ConfigStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn load_connections(&self) -> AppResult<ConnectionsFile> {
        self.load_json(CONNECTIONS_FILE)
    }

    pub fn save_connections(&self, file: &ConnectionsFile) -> AppResult<()> {
        self.save_json(CONNECTIONS_FILE, file)
    }

    pub fn load_settings(&self) -> AppResult<AppSettings> {
        self.load_json(SETTINGS_FILE)
    }

    pub fn save_settings(&self, settings: &AppSettings) -> AppResult<()> {
        self.save_json(SETTINGS_FILE, settings)
    }

    /// Load `name` as JSON; a missing file yields the default value.
    pub fn load_json<T: DeserializeOwned + Default>(&self, name: &str) -> AppResult<T> {
        let path = self.dir.join(name);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                AppError::new(
                    ErrorKind::Config,
                    format!("{} is not valid pgWhy config", path.display()),
                )
                .with_detail(e.to_string())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(io_error(&path, e)),
        }
    }

    pub fn save_json<T: Serialize>(&self, name: &str, value: &T) -> AppResult<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| {
            AppError::internal("could not serialize config").with_detail(e.to_string())
        })?;
        write_atomic(&self.dir.join(name), &bytes)
    }
}

/// Write `bytes` to `path` via a sibling temp file and rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let dir = path
        .parent()
        .ok_or_else(|| AppError::internal("config path has no parent"))?;
    std::fs::create_dir_all(dir).map_err(|e| io_error(dir, e))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| io_error(dir, e))?;
    tmp.write_all(bytes).map_err(|e| io_error(path, e))?;
    tmp.as_file().sync_all().map_err(|e| io_error(path, e))?;
    tmp.persist(path).map_err(|e| io_error(path, e.error))?;
    Ok(())
}

fn io_error(path: &Path, e: std::io::Error) -> AppError {
    let kind = if e.kind() == std::io::ErrorKind::StorageFull {
        ErrorKind::Disk
    } else {
        ErrorKind::Config
    };
    AppError::new(kind, format!("could not access {}", path.display())).with_detail(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvTag;

    fn entry(name: &str) -> ConnectionEntry {
        serde_json::from_value(
            serde_json::json!({"name": name, "host": "localhost", "database": "app", "user": "me"}),
        )
        .unwrap()
    }

    #[test]
    fn round_trips_connections_and_recents() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        assert_eq!(
            store.load_connections().unwrap(),
            ConnectionsFile::default()
        );

        let mut file = ConnectionsFile::default();
        let mut a = entry("a");
        a.env_tag = EnvTag::Local;
        let b = entry("b");
        file.upsert(a.clone());
        file.upsert(b.clone());
        file.touch_recent(&a.id);
        file.touch_recent(&b.id);
        file.touch_recent(&a.id);
        store.save_connections(&file).unwrap();

        let loaded = store.load_connections().unwrap();
        assert_eq!(loaded, file);
        assert_eq!(loaded.recents, vec![a.id.clone(), b.id.clone()]);
    }

    #[test]
    fn rename_keeps_id_and_remove_drops_recent() {
        let mut file = ConnectionsFile::default();
        let mut a = entry("a");
        file.upsert(a.clone());
        file.touch_recent(&a.id);
        a.name = "renamed".into();
        file.upsert(a.clone());
        assert_eq!(file.connections.len(), 1);
        assert_eq!(file.get(&a.id).unwrap().name, "renamed");
        file.remove(&a.id);
        assert!(file.recents.is_empty());
    }

    #[test]
    fn corrupt_file_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONNECTIONS_FILE), b"{not json").unwrap();
        let err = ConfigStore::new(dir.path()).load_connections().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config);
    }
}
