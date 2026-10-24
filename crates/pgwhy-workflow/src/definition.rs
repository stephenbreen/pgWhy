//! Refresh workflow definitions: what to copy where, and how. One editable TOML file per
//! workflow under `<config>/workflows/`, so definitions are hand-editable and git-friendly —
//! which is also why every run re-checks them (see `preflight`).

use pgwhy_core::config::{ConnectionEntry, ConnectionId};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowId(pub String);

impl WorkflowId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for WorkflowId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorkflowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A database on a saved connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DbRef {
    pub connection: ConnectionId,
    pub database: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceStrategy {
    /// Restore into a staging database and swap it in only after everything succeeded.
    #[default]
    StagingSwap,
    /// Drop the target first and restore into its name (needs one copy of disk space; a
    /// failed run leaves no working local database).
    DropFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPolicy {
    /// `pg_restore --exit-on-error`.
    #[default]
    StopOnFirst,
    /// Restore everything possible, then fail the run naming every object that failed.
    ContinueThenFail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptMode {
    /// The post-restore script runs in one transaction (the default).
    #[default]
    Transaction,
    /// For scripts with CONCURRENTLY / VACUUM etc.; a failure leaves staging partially mutated.
    NoTransaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefreshOptions {
    pub strategy: ReplaceStrategy,
    /// Tables whose data is skipped (`--exclude-table-data`), as `schema.table` patterns.
    pub exclude_table_data: Vec<String>,
    /// `pg_restore -j`; None = min(cpu count, 4).
    pub jobs: Option<u32>,
    pub no_owner: bool,
    pub no_privileges: bool,
    pub error_policy: ErrorPolicy,
    pub script_mode: ScriptMode,
    /// Replaces the default verify check (connect + at least one user table). Passes when the
    /// query succeeds.
    pub sanity_query: Option<String>,
}

impl Default for RefreshOptions {
    fn default() -> Self {
        Self {
            strategy: ReplaceStrategy::StagingSwap,
            exclude_table_data: Vec::new(),
            jobs: None,
            no_owner: true,
            no_privileges: true,
            error_policy: ErrorPolicy::StopOnFirst,
            script_mode: ScriptMode::Transaction,
            sanity_query: None,
        }
    }
}

impl RefreshOptions {
    pub fn effective_jobs(&self) -> u32 {
        self.jobs.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1)
                .clamp(1, 4)
        })
    }
}

/// A confirmation the user asked pgWhy to remember: valid only while the workflow's
/// confirmation fingerprint is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedConfirmation {
    pub fingerprint: String,
    pub remembered_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshWorkflow {
    #[serde(default)]
    pub id: WorkflowId,
    pub name: String,
    pub source: DbRef,
    pub target: DbRef,
    /// Stable library script id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_restore_script: Option<String>,
    #[serde(default)]
    pub options: RefreshOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remembered_confirmation: Option<RememberedConfirmation>,
    /// Set after the first successful run; remembering a confirmation requires it.
    #[serde(default)]
    pub has_succeeded: bool,
}

impl RefreshWorkflow {
    /// Fingerprint of everything a confirmation vouches for: source, target, strategy and the
    /// post-restore script (id + content hash). Any change invalidates a remembered one.
    pub fn confirmation_fingerprint(&self, script_hash: Option<&str>) -> String {
        let mut h = Sha256::new();
        for part in [
            self.source.connection.0.as_str(),
            &self.source.database,
            self.target.connection.0.as_str(),
            &self.target.database,
            match self.options.strategy {
                ReplaceStrategy::StagingSwap => "staging_swap",
                ReplaceStrategy::DropFirst => "drop_first",
            },
            self.post_restore_script.as_deref().unwrap_or(""),
            script_hash.unwrap_or(""),
        ] {
            h.update(part.as_bytes());
            h.update([0]);
        }
        hex(h.finalize().as_slice())
    }

    /// Whether the single confirmation may be skipped for this run.
    pub fn confirmation_remembered(
        &self,
        target: &ConnectionEntry,
        script_hash: Option<&str>,
    ) -> bool {
        target.env_tag == pgwhy_core::config::EnvTag::Local
            && self.has_succeeded
            && self
                .remembered_confirmation
                .as_ref()
                .is_some_and(|r| r.fingerprint == self.confirmation_fingerprint(script_hash))
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Save-time and run-time validation that doesn't need a server: the environment-tag gate and
/// source ≠ target.
pub fn validate(
    wf: &RefreshWorkflow,
    source: &ConnectionEntry,
    target: &ConnectionEntry,
) -> AppResult<()> {
    if wf.name.trim().is_empty() {
        return Err(AppError::new(
            ErrorKind::Config,
            "the workflow needs a name",
        ));
    }
    if wf.source.database.trim().is_empty() || wf.target.database.trim().is_empty() {
        return Err(AppError::new(
            ErrorKind::Config,
            "choose both a source and a target database",
        ));
    }
    if !target.env_tag.allows_destructive_target() {
        return Err(AppError::new(
            ErrorKind::Refused,
            format!(
                "\"{}\" is tagged {}; a Refresh workflow can only replace databases on Local or Dev connections",
                target.name,
                target.env_tag.label()
            ),
        )
        .with_hint("re-tag the connection if it really is a local or dev server"));
    }
    if source.server_key() == target.server_key() && wf.source.database == wf.target.database {
        return Err(AppError::new(
            ErrorKind::Refused,
            "the source and target are the same database",
        ));
    }
    if wf.target.database == target.maintenance_db {
        return Err(AppError::new(
            ErrorKind::Refused,
            format!("the target database {} is the connection's maintenance database", wf.target.database),
        )
        .with_hint("set a different maintenance database on the target connection (renames can't run from inside the target)"));
    }
    if wf
        .options
        .sanity_query
        .as_deref()
        .is_some_and(|q| q.trim().is_empty())
    {
        return Err(AppError::new(
            ErrorKind::Config,
            "the sanity query is empty",
        ));
    }
    Ok(())
}

/// Workflow files under one directory.
#[derive(Debug, Clone)]
pub struct WorkflowStore {
    dir: PathBuf,
}

impl WorkflowStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &WorkflowId) -> PathBuf {
        self.dir.join(format!("{id}.toml"))
    }

    pub fn list(&self) -> AppResult<Vec<RefreshWorkflow>> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&self.dir, e)),
        };
        let mut out = Vec::new();
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().is_some_and(|x| x == "toml") {
                out.push(read(&p)?);
            }
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }

    pub fn get(&self, id: &WorkflowId) -> AppResult<RefreshWorkflow> {
        read(&self.path(id))
    }

    pub fn save(&self, wf: &RefreshWorkflow) -> AppResult<()> {
        let text = toml::to_string_pretty(wf).map_err(|e| {
            AppError::internal("could not serialize the workflow").with_detail(e.to_string())
        })?;
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, e))?;
        let tmp = self.dir.join(format!(".{}.tmp", wf.id));
        std::fs::write(&tmp, text).map_err(|e| io(&tmp, e))?;
        std::fs::rename(&tmp, self.path(&wf.id)).map_err(|e| io(&self.path(&wf.id), e))
    }

    pub fn delete(&self, id: &WorkflowId) -> AppResult<()> {
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(&self.path(id), e)),
        }
    }
}

fn read(path: &Path) -> AppResult<RefreshWorkflow> {
    let text = std::fs::read_to_string(path).map_err(|e| io(path, e))?;
    toml::from_str(&text).map_err(|e| {
        AppError::new(
            ErrorKind::Config,
            format!("{} is not a valid workflow file", path.display()),
        )
        .with_detail(e.to_string())
    })
}

fn io(path: &Path, e: std::io::Error) -> AppError {
    AppError::new(
        ErrorKind::Config,
        format!("could not access {}", path.display()),
    )
    .with_detail(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgwhy_core::config::EnvTag;

    fn conn(name: &str, tag: EnvTag) -> ConnectionEntry {
        let mut e: ConnectionEntry = serde_json::from_value(serde_json::json!({
            "name": name, "host": format!("{name}.example"), "database": "app", "user": "me"
        }))
        .unwrap();
        e.env_tag = tag;
        e
    }

    fn wf(source: &ConnectionEntry, target: &ConnectionEntry) -> RefreshWorkflow {
        RefreshWorkflow {
            id: WorkflowId::new(),
            name: "Refresh local".into(),
            source: DbRef {
                connection: source.id.clone(),
                database: "app".into(),
            },
            target: DbRef {
                connection: target.id.clone(),
                database: "app".into(),
            },
            post_restore_script: None,
            options: RefreshOptions::default(),
            remembered_confirmation: None,
            has_succeeded: false,
        }
    }

    #[test]
    fn production_and_staging_targets_are_refused() {
        let src = conn("remote", EnvTag::Production);
        for tag in [EnvTag::Production, EnvTag::Staging] {
            let tgt = conn("local", tag);
            let err = validate(&wf(&src, &tgt), &src, &tgt).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Refused);
            assert!(err.message.contains("Local or Dev"), "{}", err.message);
        }
        let tgt = conn("local", EnvTag::Local);
        validate(&wf(&src, &tgt), &src, &tgt).unwrap();
    }

    #[test]
    fn source_equal_to_target_and_maintenance_target_are_refused() {
        let local = conn("local", EnvTag::Local);
        assert!(validate(&wf(&local, &local), &local, &local).is_err());
        let src = conn("remote", EnvTag::Production);
        let mut w = wf(&src, &local);
        w.target.database = "postgres".into();
        assert!(validate(&w, &src, &local)
            .unwrap_err()
            .message
            .contains("maintenance"));
    }

    #[test]
    fn remembered_confirmation_is_invalidated_by_any_vouched_change() {
        let src = conn("remote", EnvTag::Production);
        let tgt = conn("local", EnvTag::Local);
        let mut w = wf(&src, &tgt);
        w.post_restore_script = Some("script-1".into());
        w.has_succeeded = true;
        w.remembered_confirmation = Some(RememberedConfirmation {
            fingerprint: w.confirmation_fingerprint(Some("h1")),
            remembered_at: "now".into(),
        });
        assert!(w.confirmation_remembered(&tgt, Some("h1")));
        assert!(
            !w.confirmation_remembered(&tgt, Some("h2")),
            "script content changed"
        );
        let mut drop_first = w.clone();
        drop_first.options.strategy = ReplaceStrategy::DropFirst;
        assert!(
            !drop_first.confirmation_remembered(&tgt, Some("h1")),
            "strategy changed"
        );
        let mut retargeted = w.clone();
        retargeted.target.database = "other".into();
        assert!(!retargeted.confirmation_remembered(&tgt, Some("h1")));
        let dev = conn("local", EnvTag::Dev);
        assert!(
            !w.confirmation_remembered(&dev, Some("h1")),
            "Local targets only"
        );
    }

    #[test]
    fn store_round_trips_toml() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkflowStore::new(dir.path());
        let src = conn("remote", EnvTag::Production);
        let tgt = conn("local", EnvTag::Local);
        let mut w = wf(&src, &tgt);
        w.options.exclude_table_data = vec!["public.audit_log".into()];
        store.save(&w).unwrap();
        assert_eq!(store.list().unwrap(), vec![w.clone()]);
        assert_eq!(store.get(&w.id).unwrap(), w);
        store.delete(&w.id).unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}
