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

    /// Reject IDs that could escape a directory when used as a file or folder name.
    pub fn validate(&self) -> AppResult<()> {
        if self.0.is_empty()
            || self.0.len() > 128
            || !self
                .0
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("invalid workflow id \"{}\"", self.0),
            ));
        }
        Ok(())
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

impl Default for DbRef {
    fn default() -> Self {
        Self {
            connection: ConnectionId(String::new()),
            database: String::new(),
        }
    }
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
    /// Default toggle for pausing after Verify so the user can inspect staging before swapping.
    #[serde(default)]
    pub pause_before_swap: bool,
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
            pause_before_swap: false,
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

fn default_v1_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshWorkflow {
    /// Schema version (`1` for legacy v1 TOML files; `2` for unified Workflows v2 pipelines).
    #[serde(default = "default_v1_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub id: WorkflowId,
    pub name: String,
    /// Legacy v1 source reference (also synced with `FromRemoteDb` for backward compatibility).
    #[serde(default)]
    pub source: DbRef,
    /// Workflows v2 entry mode (`from_remote_db`, `from_backup_file`, or `in_place_target`).
    /// When `None`, defaults to `FromRemoteDb { source }` for v1 compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_mode: Option<crate::pipeline::WorkflowSource>,
    pub target: DbRef,
    /// Legacy v1 stable library script id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_restore_script: Option<String>,
    /// Workflows v2 typed runtime parameters (`{{param_name}}`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<crate::pipeline::WorkflowParam>,
    /// Workflows v2 ordered visual / SQL / verify steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<crate::pipeline::WorkflowStep>,
    #[serde(default)]
    pub options: RefreshOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remembered_confirmation: Option<RememberedConfirmation>,
    /// Set after the first successful run; remembering a confirmation requires it.
    #[serde(default)]
    pub has_succeeded: bool,
}

impl RefreshWorkflow {
    /// Resolve the effective entry mode, auto-mapping legacy v1 definitions to `FromRemoteDb`.
    pub fn effective_source(&self) -> crate::pipeline::WorkflowSource {
        self.source_mode
            .clone()
            .unwrap_or_else(|| crate::pipeline::WorkflowSource::FromRemoteDb {
                source: self.source.clone(),
            })
    }

    /// Resolve the effective ordered steps, auto-mapping legacy v1 `post_restore_script` into a
    /// single linked-library `Sql` step when `steps` is empty.
    pub fn effective_steps(&self) -> Vec<crate::pipeline::WorkflowStep> {
        if !self.steps.is_empty() {
            return self.steps.clone();
        }
        if let Some(sid) = &self.post_restore_script {
            return vec![crate::pipeline::WorkflowStep {
                id: "legacy_post_restore".into(),
                name: "Post-restore script".into(),
                enabled: true,
                no_transaction: self.options.script_mode == ScriptMode::NoTransaction,
                kind: crate::pipeline::StepKind::Sql {
                    body: crate::pipeline::SqlStepBody::Library {
                        script_id: sid.clone(),
                    },
                },
            }];
        }
        Vec::new()
    }

    /// True if this workflow uses Workflows v2 features (non-remote entry mode, visual/multi steps,
    /// or runtime parameters).
    pub fn is_v2_pipeline(&self) -> bool {
        self.source_mode.is_some() || !self.steps.is_empty() || !self.parameters.is_empty()
    }

    /// True when this workflow targets an existing database in place (no restore/swap).
    pub fn is_in_place(&self) -> bool {
        matches!(
            self.effective_source(),
            crate::pipeline::WorkflowSource::InPlaceTarget { .. }
        )
    }

    /// Fingerprint of everything a confirmation vouches for: source/mode, target, strategy,
    /// steps, parameters, and linked script hashes. Any change invalidates a remembered one.
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
        if self.is_v2_pipeline() {
            if let Ok(json) = serde_json::to_string(&(
                &self.source_mode,
                &self.parameters,
                &self.steps,
                self.options.pause_before_swap,
            )) {
                h.update(json.as_bytes());
                h.update([0]);
            }
        }
        hex(h.finalize().as_slice())
    }

    /// Whether the single confirmation may be skipped for this run.
    /// Strictly limited to `Local` targets after at least one successful run — never allowed on
    /// `Dev`, `Staging`, or `Production`.
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

    /// Export this workflow as a shareable v2 TOML string, stripping local confirmation state and
    /// ensuring no `Secret` parameter defaults are ever exported.
    pub fn export_toml(&self) -> AppResult<String> {
        let mut clean = self.clone();
        clean.schema_version = crate::pipeline::WORKFLOW_SCHEMA_VERSION;
        clean.remembered_confirmation = None;
        clean.has_succeeded = false;
        if clean.source_mode.is_none() {
            clean.source_mode = Some(clean.effective_source());
        }
        if clean.steps.is_empty() && clean.post_restore_script.is_some() {
            clean.steps = clean.effective_steps();
            clean.post_restore_script = None;
        }
        for p in &mut clean.parameters {
            if p.kind == crate::pipeline::ParamType::Secret {
                p.default = None;
            }
        }
        toml::to_string_pretty(&clean).map_err(|e| {
            AppError::internal("could not export the workflow as TOML").with_detail(e.to_string())
        })
    }

    /// Parse and sanitize a shareable v1 or v2 workflow TOML string for import.
    pub fn import_toml(text: &str) -> AppResult<Self> {
        let mut wf: Self = toml::from_str(text).map_err(|e| {
            AppError::new(ErrorKind::Config, "not a valid workflow TOML file")
                .with_detail(e.to_string())
        })?;
        wf.id.validate()?;
        wf.schema_version = crate::pipeline::WORKFLOW_SCHEMA_VERSION;
        wf.remembered_confirmation = None;
        wf.has_succeeded = false;
        if wf.source_mode.is_none() {
            wf.source_mode = Some(crate::pipeline::WorkflowSource::FromRemoteDb {
                source: wf.source.clone(),
            });
        }
        if wf.steps.is_empty() && wf.post_restore_script.is_some() {
            wf.steps = wf.effective_steps();
            wf.post_restore_script = None;
        }
        for p in &mut wf.parameters {
            if p.kind == crate::pipeline::ParamType::Secret {
                p.default = None;
            }
            p.validate()?;
        }
        Ok(wf)
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Save-time and run-time validation that doesn't need a server (legacy v1 signature).
pub fn validate(
    wf: &RefreshWorkflow,
    source: &ConnectionEntry,
    target: &ConnectionEntry,
) -> AppResult<()> {
    validate_workflow(wf, Some(source), target)
}

/// Save-time and run-time validation for Workflows v1 & v2.
///
/// Workflows are allowed on every environment (`Local`, `Dev`, `Staging`, `Production`); Staging
/// and Production runs are protected by backend-enforced typed database-name confirmation in
/// preflight and start.
pub fn validate_workflow(
    wf: &RefreshWorkflow,
    source: Option<&ConnectionEntry>,
    target: &ConnectionEntry,
) -> AppResult<()> {
    use crate::pipeline::{BackupSource, WorkflowSource};

    wf.id.validate()?;
    if wf.name.trim().is_empty() {
        return Err(AppError::new(
            ErrorKind::Config,
            "the workflow needs a name",
        ));
    }
    if wf.target.database.trim().is_empty() {
        return Err(AppError::new(ErrorKind::Config, "choose a target database"));
    }
    let effective_source = wf.effective_source();
    match &effective_source {
        WorkflowSource::FromRemoteDb { source: src_ref } => {
            if src_ref.database.trim().is_empty() {
                return Err(AppError::new(
                    ErrorKind::Config,
                    "choose both a source and a target database",
                ));
            }
            let src = source.ok_or_else(|| {
                AppError::new(
                    ErrorKind::Config,
                    "choose a source connection for a remote database workflow",
                )
            })?;
            if src.server_key() == target.server_key() && src_ref.database == wf.target.database {
                return Err(AppError::new(
                    ErrorKind::Refused,
                    "the source and target are the same database",
                ));
            }
        }
        WorkflowSource::FromBackupFile { archive } => match archive {
            BackupSource::FixedPath { path } if path.as_os_str().is_empty() => {
                return Err(AppError::new(
                    ErrorKind::Config,
                    "choose a backup archive path",
                ));
            }
            BackupSource::LatestFromHistory { database, .. } if database.trim().is_empty() => {
                return Err(AppError::new(
                    ErrorKind::Config,
                    "choose the source database whose latest backup should be restored",
                ));
            }
            _ => {}
        },
        WorkflowSource::InPlaceTarget { .. } => {}
    }

    // Renames / drops cannot run while connected to the target database as maintenance_db.
    if !matches!(effective_source, WorkflowSource::InPlaceTarget { .. })
        && wf.target.database == target.maintenance_db
    {
        return Err(AppError::new(
            ErrorKind::Refused,
            format!(
                "the target database {} is the connection's maintenance database",
                wf.target.database
            ),
        )
        .with_hint(
            "set a different maintenance database on the target connection (renames can't run from inside the target)",
        ));
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

    // Validate declared parameters (syntax, secret no-default rule, uniqueness).
    let mut seen_params = std::collections::HashSet::new();
    for p in &wf.parameters {
        p.validate()?;
        if !seen_params.insert(p.name.as_str()) {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("duplicate workflow parameter \"{}\"", p.name),
            ));
        }
    }

    // Validate step ids and names.
    let mut seen_steps = std::collections::HashSet::new();
    for step in &wf.steps {
        if step.id.trim().is_empty() {
            return Err(AppError::new(
                ErrorKind::Config,
                "every workflow step needs an id",
            ));
        }
        if !seen_steps.insert(step.id.as_str()) {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("duplicate workflow step id \"{}\"", step.id),
            ));
        }
        if step.name.trim().is_empty() {
            return Err(AppError::new(
                ErrorKind::Config,
                format!("step \"{}\" needs a name", step.id),
            ));
        }
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
        out.sort_by_key(|a| a.name.to_lowercase());
        Ok(out)
    }

    pub fn get(&self, id: &WorkflowId) -> AppResult<RefreshWorkflow> {
        id.validate()?;
        read(&self.path(id))
    }

    pub fn save(&self, wf: &RefreshWorkflow) -> AppResult<()> {
        wf.id.validate()?;
        let text = toml::to_string_pretty(wf).map_err(|e| {
            AppError::internal("could not serialize the workflow").with_detail(e.to_string())
        })?;
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, e))?;
        let tmp = self.dir.join(format!(".{}.tmp", wf.id));
        std::fs::write(&tmp, text).map_err(|e| io(&tmp, e))?;
        std::fs::rename(&tmp, self.path(&wf.id)).map_err(|e| io(&self.path(&wf.id), e))
    }

    pub fn delete(&self, id: &WorkflowId) -> AppResult<()> {
        id.validate()?;
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
            schema_version: crate::pipeline::WORKFLOW_SCHEMA_VERSION,
            id: WorkflowId::new(),
            name: "Refresh local".into(),
            source: DbRef {
                connection: source.id.clone(),
                database: "app".into(),
            },
            source_mode: None,
            target: DbRef {
                connection: target.id.clone(),
                database: "app".into(),
            },
            post_restore_script: None,
            parameters: Vec::new(),
            steps: Vec::new(),
            options: RefreshOptions::default(),
            remembered_confirmation: None,
            has_succeeded: false,
        }
    }

    #[test]
    fn workflows_allowed_on_all_environments_but_remember_only_on_local() {
        let src = conn("remote", EnvTag::Production);
        for tag in [
            EnvTag::Production,
            EnvTag::Staging,
            EnvTag::Dev,
            EnvTag::Local,
        ] {
            let tgt = conn("target", tag);
            let mut w = wf(&src, &tgt);
            validate(&w, &src, &tgt).unwrap();
            w.has_succeeded = true;
            w.remembered_confirmation = Some(RememberedConfirmation {
                fingerprint: w.confirmation_fingerprint(None),
                remembered_at: "now".into(),
            });
            assert_eq!(
                w.confirmation_remembered(&tgt, None),
                tag == EnvTag::Local,
                "remembered confirmation must only apply on Local (not {tag:?})"
            );
        }
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

    #[test]
    fn v1_toml_loads_and_maps_to_v2_pipeline() {
        let v1_toml = r#"
id = "wf-legacy"
name = "Legacy Refresh"
post_restore_script = "scrub-users"
has_succeeded = true

[source]
connection = "src-1"
database = "prod_app"

[target]
connection = "tgt-1"
database = "local_app"

[options]
strategy = "staging_swap"
"#;
        let wf: RefreshWorkflow = toml::from_str(v1_toml).unwrap();
        assert_eq!(wf.schema_version, 1);
        assert_eq!(
            wf.effective_source(),
            crate::pipeline::WorkflowSource::FromRemoteDb {
                source: DbRef {
                    connection: ConnectionId("src-1".into()),
                    database: "prod_app".into(),
                }
            }
        );
        let steps = wf.effective_steps();
        assert_eq!(steps.len(), 1);
        assert!(matches!(
            &steps[0].kind,
            crate::pipeline::StepKind::Sql {
                body: crate::pipeline::SqlStepBody::Library { script_id }
            } if script_id == "scrub-users"
        ));
    }

    #[test]
    fn export_and_import_strip_secrets_and_local_confirmations() {
        let mut wf = crate::pipeline::starter_templates()[0].workflow.clone();
        wf.has_succeeded = true;
        wf.remembered_confirmation = Some(RememberedConfirmation {
            fingerprint: "abc".into(),
            remembered_at: "now".into(),
        });
        wf.parameters[0].default = Some("should-never-be-exported".into());
        let exported = wf.export_toml().unwrap();
        assert!(!exported.contains("should-never-be-exported"));
        assert!(!exported.contains("remembered_confirmation"));
        let imported = RefreshWorkflow::import_toml(&exported).unwrap();
        assert_eq!(
            imported.schema_version,
            crate::pipeline::WORKFLOW_SCHEMA_VERSION
        );
        assert!(!imported.has_succeeded);
        assert!(imported.remembered_confirmation.is_none());
        assert!(imported.parameters[0].default.is_none());
    }
}
