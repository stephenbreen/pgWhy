//! The seam between connections and SSH tunnels. The tunnel manager (U5) implements
//! [`TunnelProvider`]; connections and pg_dump/pg_restore children only ever see the local
//! forwarded port it hands out.

use crate::config::ConnectionEntry;
use crate::{AppError, AppResult, ErrorKind};
use std::any::Any;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A reference on an open tunnel. While any lease is alive the tunnel stays up; dropping the
/// last one lets the provider tear it down.
pub struct TunnelLease {
    pub local_port: u16,
    _hold: Box<dyn Any + Send + Sync>,
}

impl TunnelLease {
    pub fn new(local_port: u16, hold: impl Any + Send + Sync) -> Self {
        Self {
            local_port,
            _hold: Box::new(hold),
        }
    }
}

impl std::fmt::Debug for TunnelLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelLease")
            .field("local_port", &self.local_port)
            .finish()
    }
}

pub trait TunnelProvider: Send + Sync {
    /// Open (or share) the tunnel for `entry`. Only called for entries with a tunnel config.
    fn acquire<'a>(&'a self, entry: &'a ConnectionEntry) -> BoxFuture<'a, AppResult<TunnelLease>>;
}

/// Provider for builds/tests without SSH support: tunneled entries fail clearly.
#[derive(Debug, Default)]
pub struct NoTunnels;

impl TunnelProvider for NoTunnels {
    fn acquire<'a>(&'a self, entry: &'a ConnectionEntry) -> BoxFuture<'a, AppResult<TunnelLease>> {
        let name = entry.name.clone();
        Box::pin(async move {
            Err(AppError::new(
                ErrorKind::Config,
                format!("{name} uses an SSH tunnel, which is not available here"),
            ))
        })
    }
}

/// Where to reach a server: `host` is the real name (what TLS verifies and pgpass matches),
/// `hostaddr` the address actually dialed when it differs (the tunnel's loopback end).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub hostaddr: Option<IpAddr>,
    pub port: u16,
}

impl Endpoint {
    pub fn for_entry(entry: &ConnectionEntry, lease: Option<&TunnelLease>) -> Self {
        match lease {
            Some(l) => Self {
                host: entry.host.clone(),
                hostaddr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                port: l.local_port,
            },
            None => Self {
                host: entry.host.clone(),
                hostaddr: None,
                port: entry.port,
            },
        }
    }
}
