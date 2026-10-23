//! SSH tunnels (U5): refcounted russh tunnels per connection entry, known-hosts trust, status
//! events.
//!
//! Every consumer of a tunneled entry — introspection, editor tabs, grids, pg_dump/pg_restore
//! children — dials one loopback port that [`TunnelManager`] owns for that entry; each accepted
//! socket becomes its own `direct-tcpip` channel over the entry's single SSH session, so they
//! all multiplex (plan: "in-process russh tunnels, refcounted per connection entry").
//!
//! Lifecycle (plan SM-9): `starting → up`, `up → reconnecting → up` with exponential backoff
//! while anything still holds a lease (the local port never changes, so leases and child
//! connection strings stay valid), and `→ down` when the last lease is released, the user
//! disconnects, or a failure needs the user (host key, credentials). A disconnect request on a
//! tunnel that still has leases is refused unless forced — workflows and open tabs pin it.
//!
//! Trust is fail-closed: an unknown host key is an [`ErrorKind::HostKey`](crate::ErrorKind)
//! error whose `data` carries what a trust prompt shows, and a changed key is a hard failure
//! naming both fingerprints. See [`known_hosts`].

pub mod known_hosts;
mod manager;
mod ssh;

pub use known_hosts::{HostKeyStore, HostKeyVerdict, TrustedHost, TrustedKey, KNOWN_HOSTS_FILE};
pub use manager::TunnelManager;

use crate::config::ConnectionId;
use crate::AppError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Tunnel lifecycle state (plan SM-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelState {
    /// First SSH connect + authentication in progress.
    Starting,
    /// Session authenticated; the local port forwards.
    Up,
    /// The session dropped while leased; retrying with backoff on the same local port.
    Reconnecting,
    /// Closed (released, disconnected, or failed — `error` says which).
    Down,
}

/// A tunnel's status, as queried by [`TunnelManager::status`] and broadcast on every change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelStatus {
    pub connection_id: ConnectionId,
    pub state: TunnelState,
    /// The loopback port consumers dial; stable across reconnects. None when no tunnel exists.
    pub local_port: Option<u16>,
    /// Live leases (connections, children, runs) pinning the tunnel.
    pub leases: usize,
    /// Why the tunnel is down or reconnecting, when it is because of a failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AppError>,
    /// Reconnect attempt number while `reconnecting`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Status change events carry the full status.
pub type TunnelEvent = TunnelStatus;

#[derive(Debug, Clone)]
pub struct TunnelOptions {
    /// TCP connect + SSH handshake limit per attempt.
    pub connect_timeout: Duration,
    /// SSH keepalive interval; a dead session is noticed after `keepalive_max` misses.
    pub keepalive_interval: Duration,
    pub keepalive_max: usize,
    /// First reconnect delay; doubles per failed attempt up to `backoff_max`.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// Keep an unleased tunnel open this long before closing it, so a connection that is
    /// immediately replaced (transparent introspection reopen) doesn't redo the handshake.
    pub linger: Duration,
    /// How long `acquire` — and a socket accepted while reconnecting — waits for the tunnel
    /// to come up before failing retriably.
    pub ready_wait: Duration,
}

impl Default for TunnelOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            keepalive_interval: Duration::from_secs(15),
            keepalive_max: 3,
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            linger: Duration::from_secs(3),
            ready_wait: Duration::from_secs(30),
        }
    }
}
