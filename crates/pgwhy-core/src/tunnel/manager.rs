//! The tunnel manager: one supervised russh session + loopback listener per connection entry,
//! handed out as refcounted [`TunnelLease`]s through the [`TunnelProvider`] seam.

use super::known_hosts::{HostKeyStore, TrustedHost};
use super::ssh::{self, AuthMaterial, EndedRx, SshHandle};
use super::{TunnelEvent, TunnelOptions, TunnelState, TunnelStatus};
use crate::config::{ConfigStore, ConnectionEntry, ConnectionId, TunnelConfig};
use crate::conn::{BoxFuture, TunnelLease, TunnelProvider};
use crate::secret::{SecretKind, Vault};
use crate::{AppError, AppResult, ErrorKind};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::JoinHandle;

/// What a tunnel forwards to; a change (entry edited) invalidates the running tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TunnelTarget {
    ssh: TunnelConfig,
    /// The Postgres host/port as seen from the SSH server.
    host: String,
    port: u16,
}

#[derive(Debug, Clone)]
struct Snapshot {
    state: TunnelState,
    error: Option<AppError>,
    attempt: Option<u32>,
}

/// The live session forwarding sockets go through. `dead` flips when it is replaced or torn
/// down so in-flight pumps stop (and their sockets close) instead of hanging.
struct SessionSlot {
    handle: Arc<SshHandle>,
    dead: watch::Sender<bool>,
}

struct Tunnel {
    id: ConnectionId,
    target: TunnelTarget,
    local_port: u16,
    leases: AtomicUsize,
    lease_changed: Notify,
    /// Set (under the manager's map lock) once the tunnel is no longer handed out.
    closed: AtomicBool,
    shutdown: watch::Sender<bool>,
    snapshot: watch::Sender<Snapshot>,
    events: broadcast::Sender<TunnelEvent>,
}

impl Tunnel {
    fn status(&self) -> TunnelStatus {
        let s = self.snapshot.borrow().clone();
        TunnelStatus {
            connection_id: self.id.clone(),
            state: s.state,
            local_port: Some(self.local_port),
            leases: self.leases.load(Ordering::Acquire),
            error: s.error,
            attempt: s.attempt,
        }
    }

    fn set(&self, state: TunnelState, error: Option<AppError>, attempt: Option<u32>) {
        self.snapshot.send_replace(Snapshot {
            state,
            error,
            attempt,
        });
        let _ = self.events.send(self.status());
    }
}

/// Held inside a [`TunnelLease`]; releasing the last one lets the supervisor close the tunnel.
struct LeaseGuard {
    tunnel: Arc<Tunnel>,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.tunnel.leases.fetch_sub(1, Ordering::AcqRel);
        self.tunnel.lease_changed.notify_one();
    }
}

struct Inner {
    vault: Arc<Vault>,
    known_hosts: Arc<HostKeyStore>,
    options: TunnelOptions,
    tunnels: Mutex<HashMap<ConnectionId, Arc<Tunnel>>>,
    events: broadcast::Sender<TunnelEvent>,
}

impl Inner {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<ConnectionId, Arc<Tunnel>>> {
        self.tunnels.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Owns every SSH tunnel. Cheap to clone; clones share state. Implements [`TunnelProvider`]
/// for the [`ConnectionManager`](crate::conn::ConnectionManager); pg-tools children get a
/// port the same way (`acquire` → `lease.local_port`).
#[derive(Clone)]
pub struct TunnelManager {
    inner: Arc<Inner>,
}

impl TunnelManager {
    /// `config` locates the app config directory holding `known_hosts.json`.
    pub fn new(vault: Arc<Vault>, config: ConfigStore, options: TunnelOptions) -> AppResult<Self> {
        Ok(Self::with_known_hosts(
            vault,
            Arc::new(HostKeyStore::open(config)?),
            options,
        ))
    }

    pub fn with_known_hosts(
        vault: Arc<Vault>,
        known_hosts: Arc<HostKeyStore>,
        options: TunnelOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                vault,
                known_hosts,
                options,
                tunnels: Mutex::default(),
                events: broadcast::channel(256).0,
            }),
        }
    }

    /// Status changes of every tunnel (drives the UI's per-connection badge).
    pub fn subscribe(&self) -> broadcast::Receiver<TunnelEvent> {
        self.inner.events.subscribe()
    }

    /// Current status of an entry's tunnel; `Down` with no port when none is open.
    pub fn status(&self, id: &ConnectionId) -> TunnelStatus {
        match self.inner.map().get(id) {
            Some(t) => t.status(),
            None => TunnelStatus {
                connection_id: id.clone(),
                state: TunnelState::Down,
                local_port: None,
                leases: 0,
                error: None,
                attempt: None,
            },
        }
    }

    pub fn statuses(&self) -> Vec<TunnelStatus> {
        self.inner.map().values().map(|t| t.status()).collect()
    }

    /// The trust store (for listing/forgetting entries in settings).
    pub fn known_hosts(&self) -> &Arc<HostKeyStore> {
        &self.inner.known_hosts
    }

    /// Record the user's explicit decision to trust `fingerprint` for `host:port` — after a
    /// TOFU prompt, or a deliberate re-trust after a "host key changed" failure (the replaced
    /// key is kept in the record's history). Pass the values from the error's `data`.
    pub fn trust_host_key(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        fingerprint: &str,
    ) -> AppResult<()> {
        self.inner
            .known_hosts
            .trust(host, port, key_type, fingerprint)
    }

    pub fn forget_host_key(&self, host: &str, port: u16) -> AppResult<bool> {
        self.inner.known_hosts.forget(host, port)
    }

    pub fn trusted_host_keys(&self) -> Vec<TrustedHost> {
        self.inner.known_hosts.list()
    }

    /// Preflight (EC-10): which SSH secrets starting `entry`'s tunnel would need but are not
    /// available. Empty when the tunnel can start without prompting (or the entry has none).
    /// Reads the key file; never touches the network.
    pub fn missing_secrets(&self, entry: &ConnectionEntry) -> AppResult<Vec<SecretKind>> {
        if entry.tunnel.is_none() {
            return Ok(Vec::new());
        }
        ssh::missing_secrets(entry, &self.inner.vault)
    }

    /// Close an entry's tunnel. Refused (`ErrorKind::Refused`, `data.leases`) while anything
    /// still holds a lease — open tabs, grids and workflow runs pin it — unless `force`, in
    /// which case every connection through it fails.
    pub fn disconnect(&self, id: &ConnectionId, force: bool) -> AppResult<()> {
        let tunnel = {
            let mut map = self.inner.map();
            let Some(t) = map.get(id) else {
                return Ok(());
            };
            let leases = t.leases.load(Ordering::Acquire);
            if leases > 0 && !force {
                return Err(AppError::new(
                    ErrorKind::Refused,
                    format!(
                        "the SSH tunnel is in use by {leases} connection{}",
                        if leases == 1 { "" } else { "s" }
                    ),
                )
                .with_hint("close the tabs, grids and runs using this connection first, or force the disconnect")
                .with_data(json!({ "connection_id": id, "leases": leases })));
            }
            let t = map.remove(id).expect("checked above");
            t.closed.store(true, Ordering::Release);
            t
        };
        tunnel.shutdown.send_replace(true);
        Ok(())
    }

    /// Force-close every tunnel (app exit).
    pub fn close_all(&self) {
        let all: Vec<_> = {
            let mut map = self.inner.map();
            map.drain().map(|(_, t)| t).collect()
        };
        for t in all {
            t.closed.store(true, Ordering::Release);
            t.shutdown.send_replace(true);
        }
    }

    /// Open or share the tunnel for `entry` and wait until it forwards.
    pub async fn acquire_lease(&self, entry: &ConnectionEntry) -> AppResult<TunnelLease> {
        let tunnel_cfg = ssh::tunnel_config(entry)?.clone();
        let target = TunnelTarget {
            ssh: tunnel_cfg,
            host: entry.host.clone(),
            port: entry.port,
        };
        let guard = match self.try_share(&entry.id, &target) {
            Some(g) => g,
            None => self.start(entry, target)?,
        };
        self.wait_ready(&guard.tunnel).await?;
        let port = guard.tunnel.local_port;
        Ok(TunnelLease::new(port, guard))
    }

    /// Lease an existing usable tunnel; invalidate one whose target changed.
    fn try_share(&self, id: &ConnectionId, target: &TunnelTarget) -> Option<LeaseGuard> {
        let mut map = self.inner.map();
        let t = map.get(id)?;
        if &t.target == target && !t.closed.load(Ordering::Acquire) {
            t.leases.fetch_add(1, Ordering::AcqRel);
            return Some(LeaseGuard { tunnel: t.clone() });
        }
        if &t.target != target {
            let old = map.remove(id).expect("present");
            old.closed.store(true, Ordering::Release);
            old.shutdown.send_replace(true);
        }
        None
    }

    fn start(&self, entry: &ConnectionEntry, target: TunnelTarget) -> AppResult<LeaseGuard> {
        let auth = match ssh::resolve_auth(entry, &self.inner.vault) {
            Ok(a) => a,
            Err(e) => {
                let _ = self.inner.events.send(TunnelStatus {
                    connection_id: entry.id.clone(),
                    state: TunnelState::Down,
                    local_port: None,
                    leases: 0,
                    error: Some(e.clone()),
                    attempt: None,
                });
                return Err(e);
            }
        };
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.set_nonblocking(true).map(|_| l))
            .map_err(|e| {
                AppError::new(
                    ErrorKind::Tunnel,
                    "could not open a local port for the SSH tunnel",
                )
                .with_detail(e.to_string())
            })?;
        let listener = TcpListener::from_std(std_listener).map_err(|e| {
            AppError::internal("could not register the tunnel's local port")
                .with_detail(e.to_string())
        })?;
        let local_port = listener
            .local_addr()
            .map_err(|e| {
                AppError::internal("tunnel listener has no address").with_detail(e.to_string())
            })?
            .port();

        let mut map = self.inner.map();
        // Another acquire may have started one while we were reading the key file.
        if let Some(t) = map.get(&entry.id) {
            if t.target == target && !t.closed.load(Ordering::Acquire) {
                t.leases.fetch_add(1, Ordering::AcqRel);
                return Ok(LeaseGuard { tunnel: t.clone() });
            }
            let old = map.remove(&entry.id).expect("present");
            old.closed.store(true, Ordering::Release);
            old.shutdown.send_replace(true);
        }
        let tunnel = Arc::new(Tunnel {
            id: entry.id.clone(),
            target,
            local_port,
            leases: AtomicUsize::new(1),
            lease_changed: Notify::new(),
            closed: AtomicBool::new(false),
            shutdown: watch::channel(false).0,
            snapshot: watch::channel(Snapshot {
                state: TunnelState::Starting,
                error: None,
                attempt: None,
            })
            .0,
            events: self.inner.events.clone(),
        });
        map.insert(entry.id.clone(), tunnel.clone());
        drop(map);
        tunnel.set(TunnelState::Starting, None, None);

        let (current_tx, current_rx) = watch::channel::<Option<Arc<SessionSlot>>>(None);
        let accept = tokio::spawn(accept_loop(
            listener,
            current_rx,
            tunnel.target.host.clone(),
            tunnel.target.port,
            self.inner.options.ready_wait,
        ));
        tokio::spawn(supervise(
            Arc::downgrade(&self.inner),
            tunnel.clone(),
            auth,
            current_tx,
            accept,
        ));
        Ok(LeaseGuard { tunnel })
    }

    async fn wait_ready(&self, tunnel: &Tunnel) -> AppResult<()> {
        let mut rx = tunnel.snapshot.subscribe();
        let waited = tokio::time::timeout(
            self.inner.options.ready_wait,
            rx.wait_for(|s| matches!(s.state, TunnelState::Up | TunnelState::Down)),
        )
        .await;
        match waited {
            Ok(Ok(s)) if s.state == TunnelState::Up => Ok(()),
            Ok(Ok(s)) => Err(s
                .error
                .clone()
                .unwrap_or_else(|| AppError::new(ErrorKind::Tunnel, "the SSH tunnel was closed"))),
            Ok(Err(_)) => Err(AppError::new(
                ErrorKind::Tunnel,
                "the SSH tunnel was closed",
            )),
            Err(_) => {
                let last = tunnel.snapshot.borrow().error.clone();
                let mut e = AppError::new(
                    ErrorKind::Tunnel,
                    "the SSH tunnel is not up yet (still reconnecting)",
                )
                .with_hint("try again in a moment");
                if let Some(last) = last {
                    e = e.with_detail(last.message);
                }
                Err(e)
            }
        }
    }
}

impl TunnelProvider for TunnelManager {
    fn acquire<'a>(&'a self, entry: &'a ConnectionEntry) -> BoxFuture<'a, AppResult<TunnelLease>> {
        Box::pin(self.acquire_lease(entry))
    }
}

/// Forward every accepted loopback socket over the current session as a `direct-tcpip`
/// channel. Sockets accepted while reconnecting wait (bounded) for the session to return.
async fn accept_loop(
    listener: TcpListener,
    current: watch::Receiver<Option<Arc<SessionSlot>>>,
    host: String,
    port: u16,
    ready_wait: std::time::Duration,
) {
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "tunnel accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let mut current = current.clone();
        let host = host.clone();
        tokio::spawn(async move {
            let slot =
                match tokio::time::timeout(ready_wait, current.wait_for(|s| s.is_some())).await {
                    Ok(Ok(s)) => s.clone().expect("waited for Some"),
                    _ => return, // tunnel down or still reconnecting: closing the socket fails the client
                };
            pump(socket, peer.port(), slot, &host, port).await;
        });
    }
}

async fn pump(
    mut socket: TcpStream,
    peer_port: u16,
    slot: Arc<SessionSlot>,
    host: &str,
    port: u16,
) {
    let mut dead = slot.dead.subscribe();
    if *dead.borrow() {
        return;
    }
    let channel = match slot
        .handle
        .channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", u32::from(peer_port))
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, target = %format!("{host}:{port}"), "direct-tcpip channel refused");
            return;
        }
    };
    let mut stream = channel.into_stream();
    tokio::select! {
        r = tokio::io::copy_bidirectional(&mut socket, &mut stream) => {
            if let Err(e) = r {
                tracing::debug!(error = %e, "tunnel pump ended with an error");
            }
        }
        _ = dead.wait_for(|d| *d) => {}
    }
}

/// Owns the tunnel's lifecycle: first connect, reconnect with backoff while leased, linger
/// and teardown when released, forced shutdown.
async fn supervise(
    manager: Weak<Inner>,
    tunnel: Arc<Tunnel>,
    auth: AuthMaterial,
    current: watch::Sender<Option<Arc<SessionSlot>>>,
    accept: JoinHandle<()>,
) {
    let Some(inner) = manager.upgrade() else {
        accept.abort();
        return;
    };
    let options = inner.options.clone();
    let store = inner.known_hosts.clone();
    drop(inner);
    let mut shutdown = tunnel.shutdown.subscribe();

    let teardown = |error: Option<AppError>| {
        if let Some(inner) = manager.upgrade() {
            let mut map = inner.map();
            if map.get(&tunnel.id).is_some_and(|t| Arc::ptr_eq(t, &tunnel)) {
                map.remove(&tunnel.id);
            }
        }
        tunnel.closed.store(true, Ordering::Release);
        accept.abort();
        if let Some(slot) = current.send_replace(None) {
            slot.dead.send_replace(true);
            let handle = slot.handle.clone();
            tokio::spawn(async move {
                let _ = handle
                    .disconnect(russh::Disconnect::ByApplication, "tunnel closed", "en")
                    .await;
            });
        }
        tunnel.set(TunnelState::Down, error, None);
    };
    // Close only if nothing re-leased it in the meantime (checked under the map lock, which
    // is where leases are handed out).
    let release_if_unleased = || -> bool {
        let Some(inner) = manager.upgrade() else {
            return true;
        };
        let _map = inner.map();
        if tunnel.leases.load(Ordering::Acquire) > 0 && !tunnel.closed.load(Ordering::Acquire) {
            return false;
        }
        tunnel.closed.store(true, Ordering::Release);
        true
    };

    let connect = ssh::open_session(&tunnel.target.ssh, &auth, store.clone(), &options);
    let mut ended: EndedRx = tokio::select! {
        r = connect => match r {
            Ok(s) => install(&current, s),
            Err(e) => { teardown(Some(e)); return; }
        },
        _ = until_set(&mut shutdown) => { teardown(None); return; }
    };
    tunnel.set(TunnelState::Up, None, None);

    loop {
        let mut stop = tunnel.shutdown.subscribe();
        tokio::select! {
            _ = &mut ended => {
                if let Some(slot) = current.send_replace(None) {
                    slot.dead.send_replace(true);
                }
                if release_if_unleased() {
                    teardown(None);
                    return;
                }
                tracing::info!(connection = %tunnel.id, "ssh session dropped; reconnecting");
                match reconnect(&tunnel, &auth, &store, &options, &mut shutdown, &current).await {
                    Ok(rx) => {
                        ended = rx;
                        tunnel.set(TunnelState::Up, None, None);
                    }
                    Err(e) => { teardown(e); return; }
                }
            }
            _ = tunnel.lease_changed.notified() => {
                if tunnel.leases.load(Ordering::Acquire) > 0 {
                    continue;
                }
                tokio::select! {
                    _ = tokio::time::sleep(options.linger) => {}
                    _ = until_set(&mut shutdown) => { teardown(None); return; }
                }
                if release_if_unleased() {
                    teardown(None);
                    return;
                }
            }
            _ = until_set(&mut stop) => { teardown(None); return; }
        }
    }
}

fn install(current: &watch::Sender<Option<Arc<SessionSlot>>>, s: ssh::Session) -> EndedRx {
    current.send_replace(Some(Arc::new(SessionSlot {
        handle: s.handle,
        dead: watch::channel(false).0,
    })));
    s.ended
}

/// Retry until up, no longer leased (`Err(None)`), shut down, or a failure the user must fix
/// (`Err(Some(e))`: host key changed, credentials rejected).
async fn reconnect(
    tunnel: &Tunnel,
    auth: &AuthMaterial,
    store: &Arc<HostKeyStore>,
    options: &TunnelOptions,
    shutdown: &mut watch::Receiver<bool>,
    current: &watch::Sender<Option<Arc<SessionSlot>>>,
) -> Result<EndedRx, Option<AppError>> {
    let mut delay = options.backoff_initial;
    let mut last: Option<AppError> =
        Some(AppError::new(ErrorKind::Tunnel, "the SSH session dropped"));
    for attempt in 1u32.. {
        tunnel.set(TunnelState::Reconnecting, last.clone(), Some(attempt));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = until_set(shutdown) => return Err(None),
        }
        if tunnel.leases.load(Ordering::Acquire) == 0 || tunnel.closed.load(Ordering::Acquire) {
            return Err(None);
        }
        let r = tokio::select! {
            r = ssh::open_session(&tunnel.target.ssh, auth, store.clone(), options) => r,
            _ = until_set(shutdown) => return Err(None),
        };
        match r {
            Ok(s) => return Ok(install(current, s)),
            Err(e) if !e.is_retriable() => return Err(Some(e)),
            Err(e) => {
                tracing::debug!(connection = %tunnel.id, attempt, error = %e, "tunnel reconnect failed");
                last = Some(e);
                delay = (delay * 2).min(options.backoff_max);
            }
        }
    }
    Err(last)
}

/// Resolves once the flag is (or becomes) true, or its sender is gone. Releases the watch
/// guard immediately so callers can use it in `select!` arms across awaits.
async fn until_set(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}
