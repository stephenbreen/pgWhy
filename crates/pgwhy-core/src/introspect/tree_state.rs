//! Persisted tree expansion state (R3: "on connect the tree restores its last expansion
//! state"). Paths are stored *by name*, not oid, so they survive a Refresh swap that recreates
//! every object: `["db:app", "schema:public", "table:users"]`.

use crate::config::{ConfigStore, ConnectionEntry, ConnectionId};
use crate::AppResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The file under the config directory.
pub const TREE_STATE_FILE: &str = "tree_state.json";

/// Upper bound per connection so a long-lived install can't grow the file without limit.
const MAX_PATHS_PER_CONNECTION: usize = 2000;

/// Root-to-node segments, each `kind:name` (see [`node_segment`]).
pub type NodePath = Vec<String>;

/// One path segment, e.g. `node_segment("schema", "public")` → `"schema:public"`. Names may
/// contain `:`; split on the *first* colon to recover the kind.
pub fn node_segment(kind: &str, name: &str) -> String {
    format!("{kind}:{name}")
}

/// Split a segment back into `(kind, name)`.
pub fn parse_segment(segment: &str) -> Option<(&str, &str)> {
    segment.split_once(':')
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeStateFile {
    #[serde(default)]
    pub connections: BTreeMap<ConnectionId, Vec<NodePath>>,
}

/// Get/set expanded paths per connection; serializes load-modify-save cycles.
pub struct ExpansionStore {
    store: ConfigStore,
    lock: Mutex<()>,
}

impl ExpansionStore {
    pub fn new(store: ConfigStore) -> Self {
        Self {
            store,
            lock: Mutex::new(()),
        }
    }

    /// The stored paths, or `None` if this connection has never stored any.
    pub fn get(&self, id: &ConnectionId) -> AppResult<Option<Vec<NodePath>>> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let file: TreeStateFile = self.store.load_json(TREE_STATE_FILE)?;
        Ok(file.connections.get(id).cloned())
    }

    /// The stored paths, or the first-connect default: the entry's database and its `public`
    /// schema expanded.
    pub fn get_or_initial(&self, entry: &ConnectionEntry) -> AppResult<Vec<NodePath>> {
        Ok(self
            .get(&entry.id)?
            .unwrap_or_else(|| initial_expansion(&entry.database)))
    }

    /// Replace the stored paths for one connection (deduplicated, order kept, capped).
    pub fn set(&self, id: &ConnectionId, paths: Vec<NodePath>) -> AppResult<()> {
        let mut seen = std::collections::HashSet::new();
        let mut paths: Vec<NodePath> = paths
            .into_iter()
            .filter(|p| !p.is_empty() && seen.insert(p.clone()))
            .collect();
        paths.truncate(MAX_PATHS_PER_CONNECTION);
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file: TreeStateFile = self.store.load_json(TREE_STATE_FILE)?;
        file.connections.insert(id.clone(), paths);
        self.store.save_json(TREE_STATE_FILE, &file)
    }

    /// Drop a connection's state (entry deleted).
    pub fn forget(&self, id: &ConnectionId) -> AppResult<()> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file: TreeStateFile = self.store.load_json(TREE_STATE_FILE)?;
        if file.connections.remove(id).is_some() {
            self.store.save_json(TREE_STATE_FILE, &file)?;
        }
        Ok(())
    }
}

/// First-connect expansion: the default database and its `public` schema.
pub fn initial_expansion(database: &str) -> Vec<NodePath> {
    let db = node_segment("db", database);
    vec![vec![db.clone()], vec![db, node_segment("schema", "public")]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_round_trip_names_with_colons() {
        let s = node_segment("table", "a:b");
        assert_eq!(parse_segment(&s), Some(("table", "a:b")));
    }

    #[test]
    fn set_dedupes_and_forget_removes() {
        let dir = tempfile::tempdir().unwrap();
        let es = ExpansionStore::new(ConfigStore::new(dir.path()));
        let id = ConnectionId::new();
        assert_eq!(es.get(&id).unwrap(), None);
        let p = vec!["db:app".to_string()];
        es.set(&id, vec![p.clone(), p.clone(), vec![]]).unwrap();
        assert_eq!(es.get(&id).unwrap(), Some(vec![p]));
        es.forget(&id).unwrap();
        assert_eq!(es.get(&id).unwrap(), None);
    }
}
