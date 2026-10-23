//! Process-wide app state, created once in `setup` and shared with commands via
//! `tauri::State<AppState>`.

use pgwhy_core::config::{AppSettings, ConfigStore, ConnectionId, ConnectionsFile};
use pgwhy_core::conn::{ConnectionManager, ManagerOptions};
use pgwhy_core::introspect::{ExpansionStore, Introspector};
use pgwhy_core::scripts::{Library, Recents};
use pgwhy_core::secret::{KeyringStore, SecretStore, Vault};
use pgwhy_core::tunnel::{TunnelManager, TunnelOptions};
use pgwhy_core::AppResult;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub struct AppState {
    pub config: ConfigStore,
    /// Saved connections + recents; the source of truth persisted to `connections.json`.
    pub connections: Mutex<ConnectionsFile>,
    /// Shared so the engine's script lookup follows library-folder changes.
    pub settings: Arc<Mutex<AppSettings>>,
    pub vault: Arc<Vault>,
    pub tunnels: TunnelManager,
    pub manager: ConnectionManager,
    pub introspector: Arc<Introspector>,
    pub expansion: ExpansionStore,
    /// App data directory (backups, run history, library defaults live under it).
    pub data_dir: PathBuf,
    /// The script library, opened lazily (its folder may not exist until first use).
    pub library: Arc<Mutex<Option<Library>>>,
    pub script_recents: Mutex<Recents>,
    pub editors: crate::commands::editor::Editors,
    pub tools: crate::tools::Tools,
    pub backups: crate::commands::backup::Backups,
    pub results: Arc<pgwhy_core::query::ResultRegistry>,
    pub result_channels: crate::commands::results::ResultChannels,
    pub grids: crate::commands::grid::Grids,
    /// Rebuilt when backup folder, retention or tool folders change (see [`Self::rebuild_engine`]).
    engine: RwLock<pgwhy_workflow::Engine>,
    /// Paths picked in backend-shown dialogs (see `paths`).
    pub grants: crate::paths::PathGrants,
}

impl AppState {
    pub fn load(config_dir: PathBuf, data_dir: PathBuf) -> AppResult<Self> {
        Self::with_secrets(config_dir, data_dir, Arc::new(KeyringStore))
    }

    pub fn with_secrets(
        config_dir: PathBuf,
        data_dir: PathBuf,
        secrets: Arc<dyn SecretStore>,
    ) -> AppResult<Self> {
        let config = ConfigStore::new(config_dir);
        let connections = config.load_connections()?;
        let settings = config.load_settings()?;
        let registry_opts = registry_options(&settings);
        let vault = Arc::new(Vault::new(secrets));
        let tunnels = TunnelManager::new(vault.clone(), config.clone(), TunnelOptions::default())?;
        let manager = ConnectionManager::new(
            vault.clone(),
            Arc::new(tunnels.clone()),
            ManagerOptions {
                idle_close: Duration::from_secs(settings.idle_close_secs),
                ..ManagerOptions::default()
            },
        );
        manager.set_entries(connections.connections.iter().cloned());
        let script_recents = Recents::load(&config)?;
        let library: Arc<Mutex<Option<Library>>> = Arc::default();
        let settings = Arc::new(Mutex::new(settings));
        let engine = build_engine(
            &config, &data_dir, &settings, &manager, &tunnels, &library, None,
        )?;
        Ok(Self {
            introspector: Arc::new(Introspector::new(manager.clone())),
            expansion: ExpansionStore::new(config.clone()),
            config,
            connections: Mutex::new(connections),
            settings,
            vault,
            tunnels,
            manager,
            data_dir,
            library,
            script_recents: Mutex::new(script_recents),
            editors: Default::default(),
            tools: Default::default(),
            backups: Default::default(),
            results: Arc::new(pgwhy_core::query::ResultRegistry::new(registry_opts)),
            result_channels: Default::default(),
            grids: Default::default(),
            engine: RwLock::new(engine),
            grants: Default::default(),
        })
    }

    /// The engine as of now. Clones share state, so a run in flight keeps the engine it began on.
    pub fn engine(&self) -> pgwhy_workflow::Engine {
        self.engine
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Swap in an engine built from the current settings (backup folder, retention, tool
    /// folders), so workflow backups follow the same folder as manual ones. The new engine
    /// shares the old one's locks, confirmations and cancel handles, so an operation in flight
    /// still excludes new ones. Refused while a Refresh run is active, so a run never straddles
    /// two backup folders.
    pub fn rebuild_engine(&self) -> AppResult<()> {
        let current = self.engine();
        if !current.active_runs().is_empty() {
            return Err(pgwhy_core::AppError::new(
                pgwhy_core::ErrorKind::Refused,
                "a refresh is running; changing the backup folder, retention or tool folders waits until it finishes",
            ));
        }
        let engine = build_engine(
            &self.config,
            &self.data_dir,
            &self.settings,
            &self.manager,
            &self.tunnels,
            &self.library,
            Some(&current),
        )?;
        *self.engine.write().unwrap_or_else(|p| p.into_inner()) = engine;
        Ok(())
    }

    pub fn library_dir(&self) -> PathBuf {
        self.settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .script_library_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("scripts"))
    }

    /// Run `f` with the library open (creating its folder on first use).
    pub fn with_library<T>(&self, f: impl FnOnce(&mut Library) -> AppResult<T>) -> AppResult<T> {
        let mut guard = self.library.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = Some(Library::create(self.library_dir())?);
        }
        f(guard.as_mut().expect("library opened above"))
    }

    /// Persist the connections file after a mutation.
    pub fn save_connections(&self) -> AppResult<()> {
        let file = self
            .connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        self.config.save_connections(&file)
    }

    /// Everything an entry change or delete must reset: its catalog cache.
    pub fn forget_catalog(&self, id: &ConnectionId) {
        self.introspector.invalidate_server(id);
    }
}

/// The workflow engine over the settings as they are now.
/// Result retention limits: the per-owner budget is two result sets' worth, so a tab can hold
/// its current and previous result before eviction starts.
pub(crate) fn registry_options(settings: &AppSettings) -> pgwhy_core::query::RegistryOptions {
    pgwhy_core::query::RegistryOptions {
        owner_row_budget: settings.result_row_cap.saturating_mul(2),
        owner_byte_budget: (settings.result_byte_cap_mb as usize)
            .saturating_mul(1024 * 1024)
            .saturating_mul(2),
    }
}

fn build_engine(
    config: &ConfigStore,
    data_dir: &std::path::Path,
    settings: &Arc<Mutex<AppSettings>>,
    manager: &ConnectionManager,
    tunnels: &TunnelManager,
    library: &Arc<Mutex<Option<Library>>>,
    base: Option<&pgwhy_workflow::Engine>,
) -> AppResult<pgwhy_workflow::Engine> {
    let current = settings.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let scripts = {
        let lib = library.clone();
        let settings = settings.clone();
        let default_dir = data_dir.join("scripts");
        move |id: &str| -> AppResult<pgwhy_workflow::LoadedScript> {
            let mut guard = lib.lock().unwrap_or_else(|p| p.into_inner());
            if guard.is_none() {
                let dir = settings
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .script_library_dir
                    .clone()
                    .unwrap_or_else(|| default_dir.clone());
                *guard = Some(Library::create(dir)?);
            }
            let library = guard.as_ref().expect("opened above");
            let sid = pgwhy_core::scripts::ScriptId(id.to_string());
            let info = library.get(&sid).ok_or_else(|| {
                pgwhy_core::AppError::new(
                    pgwhy_core::ErrorKind::Config,
                    "the post-restore script is no longer in the library",
                )
            })?;
            let file = library.read(&sid)?;
            Ok(pgwhy_workflow::LoadedScript {
                id: id.to_string(),
                name: info.name,
                content: file.content,
                sha256: file.fingerprint.sha256,
            })
        }
    };
    let backup_root = current
        .backup_dir
        .clone()
        .unwrap_or_else(|| data_dir.join("backups"));
    let mut cfg = pgwhy_workflow::EngineConfig::new(data_dir.join("runs"), backup_root);
    cfg.retention_keep = current.backup_retention.max(1) as usize;
    cfg.workflows = Some(pgwhy_workflow::WorkflowStore::new(
        config.dir().join("workflows"),
    ));
    let deps = pgwhy_workflow::RealDeps {
        manager: manager.clone(),
        tunnels: Arc::new(tunnels.clone()),
        scripts: Arc::new(scripts),
        discovery: pgwhy_pgtools::DiscoveryOptions {
            extra_dirs: current.pg_tools_dirs.clone(),
            ..Default::default()
        },
        pgpass_dir: pgwhy_pgtools::PgPassDir::new(data_dir.join("pgpass"))?,
    };
    Ok(match base {
        Some(engine) => engine.rebuilt(cfg, deps),
        None => pgwhy_workflow::Engine::new(cfg, deps),
    })
}
