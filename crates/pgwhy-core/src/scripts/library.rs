//! The script library: a directory tree of plain `.sql` files (and `.txt` / `.md` notes
//! kept alongside them) plus a sidecar index.
//!
//! Folders are the organisation; the index ([`INDEX_FILE_NAME`] at the library root) adds what
//! a filesystem cannot: a stable [`ScriptId`] per script (what workflows reference),
//! tags, and the content hash that lets a rescan recognise a script that was renamed or moved
//! outside the app (`git mv`, Finder) and keep its id.
//!
//! The index is meant to be committed alongside the scripts, so it is deterministic (entries
//! sorted by path, tags sorted, pretty JSON), stores only library-relative `/`-separated paths,
//! and is only rewritten when something meaningful changed — a checkout that merely bumps
//! mtimes does not dirty it.
//!
//! Hidden files and directories (names starting with `.`, e.g. `.git`, the index itself, and
//! atomic-write temp files) are never part of the library. Symlinked directories are not
//! followed (cycles); symlinked files are.

use super::file_state::{not_found, read_bytes, FileFingerprint, ScriptFile};
use super::{fs_error, system_time_ms, write_file_atomic};
use crate::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// File name of the sidecar index at the library root.
pub const INDEX_FILE_NAME: &str = ".pgwhy-library.json";

const INDEX_VERSION: u32 = 1;

/// Stable identity of a library script. Survives renames, moves, edits and git round-trips.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScriptId(pub String);

impl ScriptId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for ScriptId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ScriptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A library script as the UI sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptInfo {
    pub id: ScriptId,
    /// Library-relative path with `/` separators, e.g. `reports/daily.sql`.
    pub path: String,
    /// File name without the `.sql` extension (`.txt` / `.md` notes keep theirs).
    pub name: String,
    /// Library-relative folder (`""` for the root).
    pub folder: String,
    /// Sorted, case-insensitively unique.
    pub tags: Vec<String>,
    /// Last-known content fingerprint (as of the last save or rescan).
    pub fingerprint: FileFingerprint,
    /// The file was not found on the last rescan. The entry is kept (workflows may
    /// reference the id) until the user removes it or the file reappears.
    pub missing: bool,
}

/// One script that a rescan found at a new path and re-associated by content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptMove {
    pub id: ScriptId,
    pub from: String,
    pub to: String,
}

/// What a [`Library::rescan`] reconciled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescanReport {
    /// New files, entered untagged.
    pub added: Vec<ScriptId>,
    /// Renamed/moved outside the app; same id, new path.
    pub moved: Vec<ScriptMove>,
    /// Content edited outside the app.
    pub changed: Vec<ScriptId>,
    /// Newly gone from disk (now marked missing).
    pub missing: Vec<ScriptId>,
    /// Previously missing, found again (same path, or same content at a new path).
    pub restored: Vec<ScriptId>,
}

impl RescanReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.moved.is_empty()
            && self.changed.is_empty()
            && self.missing.is_empty()
            && self.restored.is_empty()
    }
}

/// On-disk index shape. Field order and sorting are part of the git-friendliness contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    #[serde(default)]
    scripts: Vec<IndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexEntry {
    id: ScriptId,
    path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    sha256: String,
    size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modified_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    missing: bool,
}

impl IndexEntry {
    fn fingerprint(&self) -> FileFingerprint {
        FileFingerprint {
            sha256: self.sha256.clone(),
            size: self.size,
            modified_ms: self.modified_ms,
        }
    }

    fn set_fingerprint(&mut self, fp: FileFingerprint) {
        self.sha256 = fp.sha256;
        self.size = fp.size;
        self.modified_ms = fp.modified_ms;
    }

    fn info(&self) -> ScriptInfo {
        let (folder, file) = match self.path.rsplit_once('/') {
            Some((folder, file)) => (folder.to_string(), file),
            None => (String::new(), self.path.as_str()),
        };
        let name = strip_sql_ext(file).to_string();
        ScriptInfo {
            id: self.id.clone(),
            path: self.path.clone(),
            name,
            folder,
            tags: self.tags.clone(),
            fingerprint: self.fingerprint(),
            missing: self.missing,
        }
    }
}

/// An opened script library. Not internally synchronised: the command layer holds it behind
/// a lock. Every mutating call persists the index before returning.
#[derive(Debug)]
pub struct Library {
    root: PathBuf,
    entries: Vec<IndexEntry>,
}

impl Library {
    /// Open the library rooted at the existing directory `root`, loading its index (an absent
    /// index means an empty one) and rescanning so the listing reflects the disk.
    pub fn open(root: impl AsRef<Path>) -> AppResult<Self> {
        let root = root.as_ref();
        let meta = std::fs::metadata(root).map_err(|e| fs_error(root, "open library", e))?;
        if !meta.is_dir() {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("{} is not a folder", root.display()),
            )
            .with_hint("choose a folder to use as the script library"));
        }
        let root = std::fs::canonicalize(root).map_err(|e| fs_error(root, "open library", e))?;
        let entries = load_index(&root.join(INDEX_FILE_NAME))?;
        let mut lib = Self { root, entries };
        lib.rescan()?;
        Ok(lib)
    }

    /// Create the library folder (and parents) if needed, then [`Library::open`] it.
    pub fn create(root: impl AsRef<Path>) -> AppResult<Self> {
        let root = root.as_ref();
        std::fs::create_dir_all(root).map_err(|e| fs_error(root, "create library", e))?;
        Self::open(root)
    }

    /// Canonical absolute library root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE_NAME)
    }

    /// All scripts (including missing ones), sorted by path.
    pub fn list(&self) -> Vec<ScriptInfo> {
        self.entries.iter().map(IndexEntry::info).collect()
    }

    /// Case-insensitive substring search over path (so folder and name) and tags. The query
    /// is split on whitespace and every term must match; an empty query lists everything.
    pub fn search(&self, query: &str) -> Vec<ScriptInfo> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        self.entries
            .iter()
            .filter(|e| {
                let path = e.path.to_lowercase();
                let tags: Vec<String> = e.tags.iter().map(|t| t.to_lowercase()).collect();
                terms.iter().all(|t| {
                    path.contains(t.as_str()) || tags.iter().any(|tag| tag.contains(t.as_str()))
                })
            })
            .map(IndexEntry::info)
            .collect()
    }

    /// Every distinct tag in use, sorted case-insensitively.
    pub fn all_tags(&self) -> Vec<String> {
        let mut seen = BTreeMap::new();
        for tag in self.entries.iter().flat_map(|e| &e.tags) {
            seen.entry(tag.to_lowercase())
                .or_insert_with(|| tag.clone());
        }
        seen.into_values().collect()
    }

    pub fn get(&self, id: &ScriptId) -> Option<ScriptInfo> {
        self.entry(id).map(IndexEntry::info)
    }

    /// Absolute path of a script (whether or not it currently exists).
    pub fn absolute_path(&self, id: &ScriptId) -> Option<PathBuf> {
        self.entry(id).map(|e| self.abs(&e.path))
    }

    /// The id of the script at `path` (absolute, or relative to the library root), if the
    /// path lies inside the library and is indexed. Lets recents and open buffers link a file
    /// opened from disk to its library identity.
    pub fn id_for_path(&self, path: &Path) -> Option<ScriptId> {
        let rel = if path.is_absolute() {
            let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            abs.strip_prefix(&self.root).ok()?.to_path_buf()
        } else {
            path.to_path_buf()
        };
        let key = rel_key(&rel).ok()?;
        self.entries
            .iter()
            .find(|e| e.path == key)
            .map(|e| e.id.clone())
    }

    /// Read a script's text and the fingerprint of the bytes read (the editor's loaded state).
    pub fn read(&self, id: &ScriptId) -> AppResult<ScriptFile> {
        let entry = self.entry(id).ok_or_else(|| unknown_id(id))?;
        super::read_script_file(&self.abs(&entry.path))
    }

    /// Create or overwrite the script at library-relative `path` (`.sql` is appended unless
    /// it already names a library file type). An already-indexed path keeps its id; otherwise a new id is minted.
    pub fn save(&mut self, path: impl AsRef<Path>, content: &str) -> AppResult<ScriptInfo> {
        let key = rel_key(&with_sql_ext(path.as_ref()))?;
        let abs = self.abs(&key);
        let fp = super::write_script_file(&abs, content)?;
        let idx = match self.entries.iter().position(|e| e.path == key) {
            Some(i) => i,
            None => {
                self.entries.push(IndexEntry {
                    id: ScriptId::new(),
                    path: key,
                    tags: Vec::new(),
                    sha256: String::new(),
                    size: 0,
                    modified_ms: None,
                    missing: false,
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[idx];
        entry.set_fingerprint(fp);
        entry.missing = false;
        let id = entry.id.clone();
        self.persist()?;
        self.get(&id).ok_or_else(|| unknown_id(&id))
    }

    /// Overwrite an existing script by id (wherever it currently lives).
    pub fn write(&mut self, id: &ScriptId, content: &str) -> AppResult<ScriptInfo> {
        let path = self.entry(id).ok_or_else(|| unknown_id(id))?.path.clone();
        self.save(path, content)
    }

    /// Rename and/or move a script to library-relative `new_path`, keeping its id. Refuses to
    /// overwrite a different existing file.
    pub fn rename(&mut self, id: &ScriptId, new_path: impl AsRef<Path>) -> AppResult<ScriptInfo> {
        let new_key = rel_key(&with_sql_ext(new_path.as_ref()))?;
        let entry = self.entry(id).ok_or_else(|| unknown_id(id))?;
        if entry.path == new_key {
            return Ok(entry.info());
        }
        let from = self.abs(&entry.path);
        let to = self.abs(&new_key);
        if entry.missing || !from.is_file() {
            return Err(not_found(&from));
        }
        if self
            .entries
            .iter()
            .any(|e| e.path == new_key && &e.id != id)
            || (to.exists() && !same_file(&from, &to))
        {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("{new_key} already exists in the library"),
            )
            .with_hint("choose a different name, or delete the other script first"));
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| fs_error(parent, "create folder", e))?;
        }
        std::fs::rename(&from, &to).map_err(|e| fs_error(&from, "rename", e))?;
        let meta = std::fs::metadata(&to).ok();
        let entry = self.entry_mut(id).ok_or_else(|| unknown_id(id))?;
        entry.path = new_key;
        entry.modified_ms = meta
            .and_then(|m| m.modified().ok())
            .and_then(system_time_ms);
        let info = entry.info();
        self.persist()?;
        Ok(info)
    }

    /// Delete a script's file (if still present) and drop its entry. Also the way to remove a
    /// missing entry the user no longer wants.
    pub fn delete(&mut self, id: &ScriptId) -> AppResult<()> {
        let idx = self
            .entries
            .iter()
            .position(|e| &e.id == id)
            .ok_or_else(|| unknown_id(id))?;
        let abs = self.abs(&self.entries[idx].path);
        match std::fs::remove_file(&abs) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(fs_error(&abs, "delete", e)),
        }
        self.entries.remove(idx);
        self.persist()
    }

    /// Replace a script's tags. Tags are trimmed, empties dropped, de-duplicated
    /// case-insensitively (first spelling wins) and sorted.
    pub fn set_tags(&mut self, id: &ScriptId, tags: &[String]) -> AppResult<ScriptInfo> {
        let tags = normalize_tags(tags);
        let entry = self.entry_mut(id).ok_or_else(|| unknown_id(id))?;
        entry.tags = tags;
        let info = entry.info();
        self.persist()?;
        Ok(info)
    }

    /// Reconcile the index with the disk (called on window focus):
    ///
    /// 1. indexed paths still on disk keep their id; content edits update the hash;
    /// 2. a new file whose content hash equals an entry whose path vanished takes over that
    ///    entry's id (external rename/move, `git mv`); empty files are never matched this way,
    ///    since every empty file has the same hash;
    /// 3. other new files become new, untagged entries;
    /// 4. entries still unaccounted for are marked missing, not dropped.
    ///
    /// The index file is rewritten only if one of those changed something.
    pub fn rescan(&mut self) -> AppResult<RescanReport> {
        let mut disk: BTreeMap<String, PathBuf> = BTreeMap::new();
        walk(&self.root, &self.root, &mut disk)?;

        let mut report = RescanReport::default();
        let mut claimed: HashSet<String> = HashSet::new();
        // Indices of entries whose path is gone from disk; candidates for re-association.
        let mut orphans: Vec<usize> = Vec::new();

        for (i, entry) in self.entries.iter_mut().enumerate() {
            let Some(abs) = disk.get(&entry.path) else {
                orphans.push(i);
                continue;
            };
            claimed.insert(entry.path.clone());
            let meta = std::fs::metadata(abs).ok();
            let size = meta.as_ref().map(|m| m.len());
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(system_time_ms);
            // Fast path: size and mtime both as recorded → trust the recorded hash.
            if !entry.missing
                && size == Some(entry.size)
                && mtime.is_some()
                && mtime == entry.modified_ms
            {
                continue;
            }
            let Some(bytes) = read_bytes(abs)? else {
                orphans.push(i);
                claimed.remove(&entry.path);
                continue;
            };
            let fp = FileFingerprint::of(&bytes, meta.as_ref());
            if entry.missing {
                entry.missing = false;
                report.restored.push(entry.id.clone());
            } else if fp.sha256 != entry.sha256 {
                report.changed.push(entry.id.clone());
            }
            entry.set_fingerprint(fp);
        }

        // New files, in path order so re-association is deterministic.
        let mut new_entries = Vec::new();
        for (key, abs) in &disk {
            if claimed.contains(key) {
                continue;
            }
            let Some(bytes) = read_bytes(abs)? else {
                continue;
            };
            let meta = std::fs::metadata(abs).ok();
            let fp = FileFingerprint::of(&bytes, meta.as_ref());
            let matched = if bytes.is_empty() {
                None
            } else {
                orphans
                    .iter()
                    .position(|&i| self.entries[i].sha256 == fp.sha256)
                    .map(|pos| orphans.remove(pos))
            };
            match matched {
                Some(i) => {
                    let entry = &mut self.entries[i];
                    if entry.missing {
                        report.restored.push(entry.id.clone());
                    }
                    report.moved.push(ScriptMove {
                        id: entry.id.clone(),
                        from: std::mem::replace(&mut entry.path, key.clone()),
                        to: key.clone(),
                    });
                    entry.set_fingerprint(fp);
                    entry.missing = false;
                }
                None => {
                    let entry = IndexEntry {
                        id: ScriptId::new(),
                        path: key.clone(),
                        tags: Vec::new(),
                        sha256: fp.sha256,
                        size: fp.size,
                        modified_ms: fp.modified_ms,
                        missing: false,
                    };
                    report.added.push(entry.id.clone());
                    new_entries.push(entry);
                }
            }
        }
        self.entries.extend(new_entries);

        for i in orphans {
            let entry = &mut self.entries[i];
            if !entry.missing {
                entry.missing = true;
                report.missing.push(entry.id.clone());
            }
        }

        if !report.is_empty() {
            self.persist()?;
        } else {
            self.sort();
        }
        Ok(report)
    }

    fn entry(&self, id: &ScriptId) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| &e.id == id)
    }

    fn entry_mut(&mut self, id: &ScriptId) -> Option<&mut IndexEntry> {
        self.entries.iter_mut().find(|e| &e.id == id)
    }

    fn abs(&self, key: &str) -> PathBuf {
        let mut p = self.root.clone();
        p.extend(key.split('/'));
        p
    }

    fn sort(&mut self) {
        self.entries
            .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
    }

    fn persist(&mut self) -> AppResult<()> {
        self.sort();
        let file = IndexFile {
            version: INDEX_VERSION,
            scripts: self.entries.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).map_err(|e| {
            AppError::internal("could not serialize the library index").with_detail(e.to_string())
        })?;
        bytes.push(b'\n');
        write_file_atomic(&self.index_path(), &bytes)
    }
}

fn load_index(path: &Path) -> AppResult<Vec<IndexEntry>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(fs_error(path, "read", e)),
    };
    let file: IndexFile = serde_json::from_slice(&bytes).map_err(|e| {
        AppError::new(
            ErrorKind::Config,
            format!("{} is not a valid pgWhy library index", path.display()),
        )
        .with_hint("if it has merge-conflict markers, resolve them and refocus pgWhy")
        .with_detail(e.to_string())
    })?;
    if file.version > INDEX_VERSION {
        return Err(AppError::new(
            ErrorKind::Config,
            format!("{} was written by a newer pgWhy", path.display()),
        )
        .with_hint("update pgWhy to open this library"));
    }
    // A hand-edited or merged index may carry invalid or duplicate rows; keep the first of
    // each id and each path rather than refusing the whole library.
    let mut ids = HashSet::new();
    let mut paths = HashSet::new();
    let mut entries = Vec::with_capacity(file.scripts.len());
    for mut e in file.scripts {
        let valid = rel_key(Path::new(&e.path)).is_ok_and(|k| k == e.path);
        if !valid || !ids.insert(e.id.clone()) || !paths.insert(e.path.clone()) {
            tracing::warn!(path = %e.path, id = %e.id, "skipping invalid or duplicate library index entry");
            continue;
        }
        e.tags = normalize_tags(&e.tags);
        entries.push(e);
    }
    Ok(entries)
}

/// Collect library files (`.sql`, `.txt`, `.md`) under `dir` keyed by library-relative `/` path.
fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) -> AppResult<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if dir == root => return Err(fs_error(dir, "read library folder", e)),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "skipping unreadable library folder");
            return Ok(());
        }
    };
    for item in read {
        let item = match item {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "skipping unreadable entry");
                continue;
            }
        };
        let name = item.file_name();
        let Some(name) = name.to_str() else {
            tracing::warn!(dir = %dir.display(), "skipping non-UTF-8 file name");
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = item.path();
        let Ok(ft) = item.file_type() else { continue };
        if ft.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        let is_file = ft.is_file() || (ft.is_symlink() && path.is_file());
        if !is_file || !has_library_ext(name) {
            continue;
        }
        if let Some(key) = path.strip_prefix(root).ok().and_then(|r| rel_key(r).ok()) {
            out.insert(key, path);
        }
    }
    Ok(())
}

/// Validate a library-relative path and turn it into the index key (`/`-joined components).
fn rel_key(path: &Path) -> AppResult<String> {
    let bad = |why: &str| {
        AppError::new(
            ErrorKind::Config,
            format!(
                "{} is not a valid library script path: {why}",
                path.display()
            ),
        )
    };
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_str().ok_or_else(|| bad("names must be UTF-8"))?;
                if s.starts_with('.') {
                    return Err(bad("names starting with '.' are hidden from the library"));
                }
                if s.contains('/') || s.contains('\\') {
                    return Err(bad("names cannot contain slashes"));
                }
                parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(bad("'..' is not allowed")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(bad("it must be relative to the library folder"))
            }
        }
    }
    let Some(last) = parts.last() else {
        return Err(bad("it is empty"));
    };
    if !has_library_ext(last) || strip_library_ext(last).is_empty() {
        return Err(bad("it must name a .sql, .txt or .md file"));
    }
    Ok(parts.join("/"))
}

/// File types the library holds: SQL scripts, plus plain-text and Markdown notes.
const LIBRARY_EXTS: [&str; 3] = [".sql", ".txt", ".md"];

fn has_ext(name: &str, ext: &str) -> bool {
    name.len() > ext.len()
        && name.is_char_boundary(name.len() - ext.len())
        && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

fn has_sql_ext(name: &str) -> bool {
    has_ext(name, ".sql")
}

fn has_library_ext(name: &str) -> bool {
    LIBRARY_EXTS.iter().any(|ext| has_ext(name, ext))
}

fn strip_library_ext(name: &str) -> &str {
    LIBRARY_EXTS
        .iter()
        .find(|ext| has_ext(name, ext))
        .map_or(name, |ext| &name[..name.len() - ext.len()])
}

fn strip_sql_ext(name: &str) -> &str {
    if has_sql_ext(name) {
        &name[..name.len() - 4]
    } else {
        name
    }
}

fn with_sql_ext(path: &Path) -> PathBuf {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(n) if has_library_ext(n) => path.to_path_buf(),
        Some(n) => path.with_file_name(format!("{n}.sql")),
        None => path.to_path_buf(),
    }
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut seen = BTreeMap::new();
    for t in tags {
        let t = t.trim();
        if !t.is_empty() {
            seen.entry(t.to_lowercase())
                .or_insert_with(|| t.to_string());
        }
    }
    seen.into_values().collect()
}

fn unknown_id(id: &ScriptId) -> AppError {
    AppError::new(
        ErrorKind::Config,
        format!("script {id} is not in the library"),
    )
    .with_hint("it may have been deleted; rescan the library or pick the script again")
}

/// Whether two existing paths are the same file (e.g. a case-only rename on a
/// case-insensitive filesystem).
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x.to_string_lossy().to_lowercase() == y.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_key_validation() {
        assert_eq!(rel_key(Path::new("a/b.sql")).unwrap(), "a/b.sql");
        assert_eq!(rel_key(Path::new("./a/B.SQL")).unwrap(), "a/B.SQL");
        assert_eq!(rel_key(Path::new("notes/x.txt")).unwrap(), "notes/x.txt");
        assert_eq!(rel_key(Path::new("README.MD")).unwrap(), "README.MD");
        for bad in [
            "../x.sql",
            "/abs.sql",
            ".hidden/x.sql",
            "a/.x.sql",
            "x.csv",
            ".sql",
            ".md",
            "",
        ] {
            assert!(rel_key(Path::new(bad)).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn sql_extension_is_appended_when_absent() {
        assert_eq!(
            with_sql_ext(Path::new("a/daily")),
            PathBuf::from("a/daily.sql")
        );
        assert_eq!(with_sql_ext(Path::new("x.SQL")), PathBuf::from("x.SQL"));
        assert_eq!(with_sql_ext(Path::new("v1.2")), PathBuf::from("v1.2.sql"));
        assert_eq!(
            with_sql_ext(Path::new("notes.md")),
            PathBuf::from("notes.md")
        );
        assert_eq!(
            with_sql_ext(Path::new("a/todo.TXT")),
            PathBuf::from("a/todo.TXT")
        );
        assert_eq!(
            with_sql_ext(Path::new("data.csv")),
            PathBuf::from("data.csv.sql")
        );
    }

    #[test]
    fn tags_normalize() {
        let tags = ["  Ops ".into(), "ops".into(), "".into(), "Billing".into()];
        assert_eq!(normalize_tags(&tags), ["Billing", "Ops"]);
    }

    #[test]
    fn info_derives_name_and_folder() {
        let e = IndexEntry {
            id: ScriptId("x".into()),
            path: "reports/q1/daily.sql".into(),
            tags: vec![],
            sha256: String::new(),
            size: 0,
            modified_ms: None,
            missing: false,
        };
        let info = e.info();
        assert_eq!(
            (info.name.as_str(), info.folder.as_str()),
            ("daily", "reports/q1")
        );
        // notes keep their extension so `x.md` and `x.sql` stay distinguishable
        let note = IndexEntry {
            path: "runbooks/restore.md".into(),
            ..e
        };
        assert_eq!(note.info().name, "restore.md");
    }
}
