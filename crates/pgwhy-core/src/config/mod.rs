//! Connection entries and app settings, persisted as JSON files in the app config directory.
//!
//! Nothing in here ever holds a secret: passwords and key passphrases live only in the
//! credential store ([`crate::secret`]); entries record only *whether* one is saved.

mod store;

pub(crate) use store::write_atomic;
pub use store::{ConfigStore, ConnectionsFile};

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Stable identity of a connection entry. Survives renames and edits, so keyring bindings,
/// recents, workflows and tabs can refer to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionId(pub String);

impl ConnectionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Environment tag (R18). Untagged connections are treated as Production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvTag {
    Local,
    Dev,
    Staging,
    #[default]
    Production,
}

impl EnvTag {
    /// Whether a Refresh workflow may drop/create/rename databases on this connection.
    pub fn allows_destructive_target(self) -> bool {
        matches!(self, EnvTag::Local | EnvTag::Dev)
    }

    pub fn label(self) -> &'static str {
        match self {
            EnvTag::Local => "Local",
            EnvTag::Dev => "Dev",
            EnvTag::Staging => "Staging",
            EnvTag::Production => "Production",
        }
    }
}

/// libpq-compatible sslmode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    Disable,
    /// Try TLS without verifying the certificate; fall back to plaintext if the server
    /// doesn't offer TLS. The resulting state is always surfaced (R2 badge).
    #[default]
    Prefer,
    /// TLS required, certificate not verified. Never falls back.
    Require,
    /// TLS required, certificate chain verified, hostname not checked.
    VerifyCa,
    /// TLS required, chain and hostname verified (against the real host, also through tunnels).
    VerifyFull,
}

impl SslMode {
    pub fn as_libpq(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }

    pub fn verifies_chain(self) -> bool {
        matches!(self, SslMode::VerifyCa | SslMode::VerifyFull)
    }
}

/// How the SSH tunnel authenticates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum TunnelAuth {
    /// Private key file; the passphrase (if any) lives in the credential store.
    Key {
        key_path: PathBuf,
        #[serde(default)]
        passphrase_saved: bool,
    },
    /// SSH password, stored in the credential store.
    Password {
        #[serde(default)]
        password_saved: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    pub auth: TunnelAuth,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_pg_port() -> u16 {
    5432
}

fn default_maintenance_db() -> String {
    "postgres".into()
}

/// A saved connection (R1). The host is always the *real* server hostname — the one TLS
/// verifies and pgpass matches — even when the socket goes through a tunnel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEntry {
    #[serde(default)]
    pub id: ConnectionId,
    pub name: String,
    /// Group path shown in the connection list, e.g. "Clients/Acme". None = ungrouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub host: String,
    #[serde(default = "default_pg_port")]
    pub port: u16,
    /// Default database opened on connect.
    pub database: String,
    pub user: String,
    /// Database used for create/drop/rename and listing databases.
    #[serde(default = "default_maintenance_db")]
    pub maintenance_db: String,
    #[serde(default)]
    pub sslmode: SslMode,
    /// PEM file of trusted root certificates for verify-ca/verify-full. None = system roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_root_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<TunnelConfig>,
    #[serde(default)]
    pub env_tag: EnvTag,
    /// Whether a database password is saved in the credential store for this entry.
    #[serde(default)]
    pub password_saved: bool,
}

impl ConnectionEntry {
    /// Identity of the server this entry points at, for "same server" checks (run locks,
    /// duplicate-entry tag warnings). Tunneled entries are identified by the tunnel host too.
    pub fn server_key(&self) -> ServerKey {
        ServerKey {
            host: self.host.to_ascii_lowercase(),
            port: self.port,
            via: self
                .tunnel
                .as_ref()
                .map(|t| format!("{}@{}:{}", t.user, t.host.to_ascii_lowercase(), t.port)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ServerKey {
    pub host: String,
    pub port: u16,
    pub via: Option<String>,
}

impl fmt::Display for ServerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)?;
        if let Some(via) = &self.via {
            write!(f, " via {via}")?;
        }
        Ok(())
    }
}

/// App-level settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Rows shown by "View top N rows" (R6).
    pub top_n: u32,
    /// Backups kept per workflow/connection (R21).
    pub backup_retention: u32,
    /// Root folder for backups. None = `<data dir>/backups`.
    pub backup_dir: Option<PathBuf>,
    /// Close idle introspection connections after this many seconds.
    pub idle_close_secs: u64,
    /// lock_timeout for grid edit commits, milliseconds.
    pub grid_lock_timeout_ms: u64,
    /// Script library root. None = `<data dir>/scripts`.
    pub script_library_dir: Option<PathBuf>,
    /// Extra directories searched first for pg_dump/pg_restore.
    pub pg_tools_dirs: Vec<PathBuf>,
    /// Rows one result set retains before it is marked truncated (default 1,000,000).
    pub result_row_cap: usize,
    /// Megabytes one result set retains before it is marked truncated (default 256). A row cap
    /// alone does not bound memory: wide rows can cost gigabytes long before it is reached.
    pub result_byte_cap_mb: u64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            top_n: 100,
            backup_retention: 3,
            backup_dir: None,
            idle_close_secs: 600,
            grid_lock_timeout_ms: 5000,
            script_library_dir: None,
            pg_tools_dirs: Vec::new(),
            // same defaults as pgwhy_core::query::{DEFAULT_ROW_CAP, DEFAULT_BYTE_CAP}
            result_row_cap: 1_000_000,
            result_byte_cap_mb: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_entries_default_to_production() {
        let json = r#"{"name":"x","host":"h","database":"d","user":"u"}"#;
        let e: ConnectionEntry = serde_json::from_str(json).unwrap();
        assert_eq!(e.env_tag, EnvTag::Production);
        assert!(!e.env_tag.allows_destructive_target());
        assert_eq!(e.port, 5432);
        assert_eq!(e.maintenance_db, "postgres");
        assert_eq!(e.sslmode, SslMode::Prefer);
        assert!(!e.id.0.is_empty());
    }

    #[test]
    fn only_local_and_dev_allow_destructive_targets() {
        assert!(EnvTag::Local.allows_destructive_target());
        assert!(EnvTag::Dev.allows_destructive_target());
        assert!(!EnvTag::Staging.allows_destructive_target());
        assert!(!EnvTag::Production.allows_destructive_target());
    }

    #[test]
    fn server_key_includes_tunnel_and_ignores_case() {
        let mut e: ConnectionEntry = serde_json::from_str(
            r#"{"name":"x","host":"DB.example.com","database":"d","user":"u"}"#,
        )
        .unwrap();
        assert_eq!(e.server_key().to_string(), "db.example.com:5432");
        e.tunnel = Some(TunnelConfig {
            host: "Bastion".into(),
            port: 22,
            user: "me".into(),
            auth: TunnelAuth::Password {
                password_saved: false,
            },
        });
        assert_eq!(
            e.server_key().to_string(),
            "db.example.com:5432 via me@bastion:22"
        );
    }
}
