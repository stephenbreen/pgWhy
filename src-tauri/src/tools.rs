//! Glue between core connection entries and pgwhy-pgtools inputs (the two crates never
//! depend on each other): tool discovery cache, endpoint/password mapping, run contexts.

use crate::state::AppState;
use pgwhy_core::config::ConnectionEntry;
use pgwhy_core::conn::{Endpoint, TunnelLease, TunnelProvider};
use pgwhy_core::secret::{SecretKey, SecretKind};
use pgwhy_core::AppResult;
use pgwhy_pgtools::{
    discover, DiscoveryOptions, PgPassDir, RunContext, ToolEndpoint, ToolPassword, ToolRegistry,
};
use std::sync::Arc;

/// Discovered pg client tools, found once and re-scanned on demand.
#[derive(Default)]
pub struct Tools {
    registry: tokio::sync::Mutex<Option<Arc<ToolRegistry>>>,
}

impl Tools {
    pub async fn registry(
        &self,
        extra_dirs: Vec<std::path::PathBuf>,
        rescan: bool,
    ) -> Arc<ToolRegistry> {
        let mut guard = self.registry.lock().await;
        if rescan || guard.is_none() {
            let opts = DiscoveryOptions {
                extra_dirs,
                ..DiscoveryOptions::default()
            };
            let reg = tokio::task::spawn_blocking(move || discover(&opts))
                .await
                .unwrap_or_default();
            *guard = Some(Arc::new(reg));
        }
        guard.clone().expect("set above")
    }
}

impl AppState {
    pub async fn tool_registry(&self, rescan: bool) -> Arc<ToolRegistry> {
        let dirs = self
            .settings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pg_tools_dirs
            .clone();
        self.tools.registry(dirs, rescan).await
    }

    pub fn pgpass_dir(&self) -> AppResult<PgPassDir> {
        PgPassDir::new(self.data_dir.join("pgpass"))
    }

    pub fn run_context(&self) -> AppResult<RunContext> {
        Ok(RunContext::new(self.pgpass_dir()?))
    }

    /// What a pg_dump/pg_restore child needs to reach `entry`'s `database`. Tunneled entries
    /// return the lease, which the caller must hold for the child's whole lifetime.
    pub async fn tool_endpoint(
        &self,
        entry: &ConnectionEntry,
        database: &str,
    ) -> AppResult<(ToolEndpoint, Option<ToolPassword>, Option<TunnelLease>)> {
        let lease = match &entry.tunnel {
            Some(_) => Some(self.tunnels.acquire(entry).await?),
            None => None,
        };
        let ep = Endpoint::for_entry(entry, lease.as_ref());
        let endpoint = ToolEndpoint {
            host: ep.host,
            hostaddr: ep.hostaddr,
            port: ep.port,
            user: entry.user.clone(),
            dbname: database.to_string(),
            sslmode: entry.sslmode.as_libpq().to_string(),
            sslrootcert: entry.ssl_root_cert.clone(),
        };
        let (secret, _) = self
            .vault
            .resolve(&SecretKey::new(&entry.id, SecretKind::DbPassword))?;
        Ok((
            endpoint,
            secret.map(|s| ToolPassword::new(s.expose())),
            lease,
        ))
    }
}
