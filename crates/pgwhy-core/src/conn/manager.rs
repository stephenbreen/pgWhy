//! The connection manager: entry registry, the allocation model, idle policy, and the quiesce
//! API the Refresh workflow's swap uses to get pgWhy's own connections out of the way.

use super::session::{ConnHandle, ConnInfo, ConnShared, LostCallback, PgConn, Purpose};
use super::tunnel_api::TunnelProvider;
use crate::config::{ConnectionEntry, ConnectionId, ServerKey};
use crate::secret::{Secret, SecretKey, SecretKind, Vault};
use crate::{AppError, AppResult, ErrorKind};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tokio::sync::broadcast;

/// Why pgWhy closed (or lost) a connection — drives tab/grid dispositions in the UI.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum CloseReason {
    /// The server or network ended it.
    Lost { error: Option<String> },
    /// A Refresh swap is replacing this database.
    Quiesced,
    /// Idle introspection connection closed by policy (reopens transparently).
    Idle,
    /// The user disconnected the entry.
    Disconnected,
    /// The entry was edited or deleted.
    EntryChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ConnEvent {
    Opened {
        info: ConnInfo,
    },
    Closed {
        handle: ConnHandle,
        connection_id: ConnectionId,
        database: String,
        purpose: Purpose,
        close: CloseReason,
    },
}

#[derive(Debug, Clone)]
pub struct ManagerOptions {
    /// Close shared introspection connections idle longer than this.
    pub idle_close: Duration,
    /// Ping a shared connection before reuse when idle longer than this.
    pub health_check_after: Duration,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            idle_close: Duration::from_secs(600),
            health_check_after: Duration::from_secs(30),
        }
    }
}

struct Live {
    info: ConnInfo,
    server: ServerKey,
    shared: Weak<ConnShared>,
}

type IntrospectionKey = (ConnectionId, String);

/// The registry of live connections plus the per-entry generation that guards registration.
///
/// `open` connects without holding any lock, so the world can change while it waits: the entry
/// may be edited (stale host/TLS settings) or deleted, or a quiesce may start. Every such
/// change bumps the entry's generation here, and `open` registers its connection only if the
/// generation it started with is still current — checked under the same lock as the insert, so
/// a change either precedes the check (the connection is discarded) or follows the insert (the
/// change's `close_where` finds the connection).
#[derive(Default)]
struct LiveSet {
    conns: HashMap<ConnHandle, Live>,
    generation: HashMap<ConnectionId, u64>,
}

impl LiveSet {
    fn generation(&self, id: &ConnectionId) -> u64 {
        self.generation.get(id).copied().unwrap_or(0)
    }

    fn bump(&mut self, id: &ConnectionId) {
        *self.generation.entry(id.clone()).or_default() += 1;
    }
}

struct QuiesceScope {
    id: u64,
    server: ServerKey,
    databases: HashSet<String>,
}

type EditLock = Arc<tokio::sync::RwLock<()>>;

struct Inner {
    entries: RwLock<HashMap<ConnectionId, ConnectionEntry>>,
    vault: Arc<Vault>,
    tunnels: Arc<dyn TunnelProvider>,
    /// read on every use, so [`ConnectionManager::set_idle_close`] applies without a restart
    options: RwLock<ManagerOptions>,
    /// shared introspection connections by (entry, database)
    /// (the lock is only ever held briefly: never across network I/O)
    introspection: tokio::sync::Mutex<HashMap<IntrospectionKey, Arc<PgConn>>>,
    /// one gate per introspection key, so concurrent callers for the same key open one
    /// connection between them while other keys proceed independently
    introspection_gates: Mutex<HashMap<IntrospectionKey, Arc<tokio::sync::Mutex<()>>>>,
    /// every live connection, for quiesce/disconnect and dispositions
    live: Mutex<LiveSet>,
    quiesced: Mutex<Vec<QuiesceScope>>,
    next_scope: Mutex<u64>,
    edit_locks: Mutex<HashMap<(ServerKey, String), EditLock>>,
    events: broadcast::Sender<ConnEvent>,
}

/// Cheap to clone; all clones share state.
#[derive(Clone)]
pub struct ConnectionManager {
    inner: Arc<Inner>,
}

impl ConnectionManager {
    pub fn new(
        vault: Arc<Vault>,
        tunnels: Arc<dyn TunnelProvider>,
        options: ManagerOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: RwLock::default(),
                vault,
                tunnels,
                options: RwLock::new(options),
                introspection: tokio::sync::Mutex::default(),
                introspection_gates: Mutex::default(),
                live: Mutex::default(),
                quiesced: Mutex::default(),
                next_scope: Mutex::new(1),
                edit_locks: Mutex::default(),
                events: broadcast::channel(256).0,
            }),
        }
    }

    pub fn vault(&self) -> &Arc<Vault> {
        &self.inner.vault
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ConnEvent> {
        self.inner.events.subscribe()
    }

    // ------------------------------------------------------------------ entries

    pub fn set_entries(&self, entries: impl IntoIterator<Item = ConnectionEntry>) {
        let mut map = self.inner.entries.write().unwrap();
        *map = entries.into_iter().map(|e| (e.id.clone(), e)).collect();
    }

    /// Insert or replace an entry. Connection-relevant changes close its live connections
    /// (they'd be using stale host/user/TLS settings).
    pub async fn upsert_entry(&self, entry: ConnectionEntry) {
        let changed = {
            let mut map = self.inner.entries.write().unwrap();
            let changed = map
                .get(&entry.id)
                .is_some_and(|old| connection_relevant_change(old, &entry));
            map.insert(entry.id.clone(), entry.clone());
            changed
        };
        if changed {
            self.inner.live.lock().unwrap().bump(&entry.id);
            self.close_where(
                |l| l.info.connection_id == entry.id,
                CloseReason::EntryChanged,
            )
            .await;
        }
    }

    pub async fn remove_entry(&self, id: &ConnectionId) {
        self.inner.entries.write().unwrap().remove(id);
        self.inner.live.lock().unwrap().bump(id);
        self.close_where(|l| &l.info.connection_id == id, CloseReason::EntryChanged)
            .await;
    }

    pub fn entry(&self, id: &ConnectionId) -> AppResult<ConnectionEntry> {
        self.inner
            .entries
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::new(ErrorKind::Config, "that connection no longer exists"))
    }

    // ------------------------------------------------------------------ allocation

    /// The shared introspection connection for (entry, database): health-checked before
    /// reuse and transparently reopened when the server dropped it.
    pub async fn introspection(&self, id: &ConnectionId, database: &str) -> AppResult<Arc<PgConn>> {
        let key = (id.clone(), database.to_string());
        // Network I/O (ping, tunnel, connect) happens outside the map lock, so a dead host
        // only stalls callers of its own key; the gate makes those callers share one open.
        let gate = self
            .inner
            .introspection_gates
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .clone();
        let _gate = gate.lock().await;
        let existing = self.inner.introspection.lock().await.get(&key).cloned();
        if let Some(conn) = existing {
            let healthy = !conn.is_closed()
                && (conn.shared.idle_for() < self.options().health_check_after
                    || conn.ping().await);
            if healthy {
                conn.touch();
                return Ok(conn);
            }
            let mut map = self.inner.introspection.lock().await;
            if map.get(&key).is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                map.remove(&key);
            }
        }
        let conn = Arc::new(self.open(id, database, Purpose::Introspection).await?);
        self.inner
            .introspection
            .lock()
            .await
            .insert(key, conn.clone());
        Ok(conn)
    }

    /// A dedicated connection (editor tab, grid, workflow, maintenance). Never reconnected:
    /// when it closes the owner sees `Disconnected` and decides what to do.
    pub async fn open(
        &self,
        id: &ConnectionId,
        database: &str,
        purpose: Purpose,
    ) -> AppResult<PgConn> {
        // generation first, then the entry: an edit landing in between leaves the generation
        // older than the entry, which only ever errs towards discarding the connection
        let generation = self.inner.live.lock().unwrap().generation(id);
        let entry = self.entry(id)?;
        self.ensure_not_quiesced(&entry.server_key(), database)?;
        let (password, _) = self
            .inner
            .vault
            .resolve(&SecretKey::new(id, SecretKind::DbPassword))?;
        let conn = self
            .connect_entry(&entry, database, purpose, password.as_ref())
            .await?;
        // The entry may have been edited or deleted, or a quiesce started, while connecting.
        // Check and register under one lock; see [`LiveSet`].
        let early_loss = {
            let mut live = self.inner.live.lock().unwrap();
            if live.generation(id) != generation || self.entry(id).is_err() {
                drop(live);
                drop(conn);
                return Err(AppError::new(
                    ErrorKind::Refused,
                    "the connection's settings changed while connecting; try again",
                )
                .with_class(crate::ErrorClass::Retriable));
            }
            if let Err(e) = self.ensure_not_quiesced(&entry.server_key(), database) {
                drop(live);
                drop(conn);
                return Err(e);
            }
            live.conns.insert(
                conn.handle(),
                Live {
                    info: conn.info.clone(),
                    server: entry.server_key(),
                    shared: Arc::downgrade(&conn.shared),
                },
            );
            // The driver marks the connection closed before it reports the loss, so a loss
            // that beat the insert above (its callback found nothing to remove) shows here.
            conn.shared.is_closed()
        };
        let _ = self.inner.events.send(ConnEvent::Opened {
            info: conn.info.clone(),
        });
        if early_loss {
            self.forget_live(
                conn.handle(),
                CloseReason::Lost {
                    error: conn.shared.lost_error(),
                },
            );
        }
        Ok(conn)
    }

    /// Connect with an unsaved entry and an explicit password (the "Test" button).
    pub async fn test(
        &self,
        entry: &ConnectionEntry,
        password: Option<Secret>,
    ) -> AppResult<ConnInfo> {
        let password = match password {
            Some(p) => Some(p),
            None => {
                self.inner
                    .vault
                    .resolve(&SecretKey::new(&entry.id, SecretKind::DbPassword))?
                    .0
            }
        };
        let conn = self
            .connect_entry(entry, &entry.database, Purpose::Test, password.as_ref())
            .await?;
        Ok(conn.info.clone())
    }

    async fn connect_entry(
        &self,
        entry: &ConnectionEntry,
        database: &str,
        purpose: Purpose,
        password: Option<&Secret>,
    ) -> AppResult<PgConn> {
        let lease = match &entry.tunnel {
            Some(_) => Some(self.inner.tunnels.acquire(entry).await?),
            None => None,
        };
        let weak = Arc::downgrade(&self.inner);
        let on_lost: LostCallback = Box::new(move |handle, error| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let live = inner.live.lock().unwrap().conns.remove(&handle);
            if let Some(l) = live {
                let _ = inner.events.send(ConnEvent::Closed {
                    handle,
                    connection_id: l.info.connection_id,
                    database: l.info.database,
                    purpose: l.info.purpose,
                    close: CloseReason::Lost { error },
                });
            }
        });
        PgConn::connect(entry, database, purpose, password, lease, Some(on_lost)).await
    }

    // ------------------------------------------------------------------ closing

    /// Close every connection of an entry (user disconnect).
    pub async fn disconnect(&self, id: &ConnectionId) {
        self.close_where(|l| &l.info.connection_id == id, CloseReason::Disconnected)
            .await;
    }

    /// Close shared introspection connections idle past the policy; they reopen lazily.
    pub async fn close_idle(&self) -> usize {
        let limit = self.options().idle_close;
        let mut map = self.inner.introspection.lock().await;
        let idle: Vec<_> = map
            .iter()
            .filter(|(_, c)| c.shared.idle_for() >= limit)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &idle {
            if let Some(conn) = map.remove(k) {
                self.forget_live(conn.handle(), CloseReason::Idle);
                conn.shared.close();
            }
        }
        idle.len()
    }

    /// Run [`close_idle`](Self::close_idle) periodically on the current runtime.
    pub fn spawn_idle_sweeper(&self) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                // the period follows the current policy, so a changed idle_close takes effect
                // within one (old) period
                let every = {
                    let Some(inner) = weak.upgrade() else { return };
                    (ConnectionManager { inner }.options().idle_close / 4)
                        .max(Duration::from_secs(1))
                };
                tokio::time::sleep(every).await;
                let Some(inner) = weak.upgrade() else { return };
                ConnectionManager { inner }.close_idle().await;
            }
        })
    }

    fn options(&self) -> ManagerOptions {
        self.inner
            .options
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Change how long a shared introspection connection may sit idle before it is closed
    /// (Settings). Applies from the sweeper's next pass.
    pub fn set_idle_close(&self, idle_close: Duration) {
        self.inner
            .options
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .idle_close = idle_close;
    }

    async fn close_where(&self, pred: impl Fn(&Live) -> bool, reason: CloseReason) {
        // shared introspection connections are owned by the map: drop them from it
        let handles: Vec<ConnHandle> = self
            .inner
            .live
            .lock()
            .unwrap()
            .conns
            .iter()
            .filter(|(_, l)| pred(l))
            .map(|(h, _)| *h)
            .collect();
        if handles.is_empty() {
            return;
        }
        let set: HashSet<_> = handles.iter().copied().collect();
        self.inner
            .introspection
            .lock()
            .await
            .retain(|_, c| !set.contains(&c.handle()));
        for h in handles {
            let shared = self
                .inner
                .live
                .lock()
                .unwrap()
                .conns
                .get(&h)
                .and_then(|l| l.shared.upgrade());
            self.forget_live(h, reason.clone());
            if let Some(s) = shared {
                s.close();
            }
        }
    }

    fn forget_live(&self, handle: ConnHandle, reason: CloseReason) {
        let live = self.inner.live.lock().unwrap().conns.remove(&handle);
        if let Some(l) = live {
            let _ = self.inner.events.send(ConnEvent::Closed {
                handle,
                connection_id: l.info.connection_id,
                database: l.info.database,
                purpose: l.info.purpose,
                close: reason,
            });
        }
    }

    /// Live connections (diagnostics, tests, the swap's "who is still connected" report).
    pub fn live_connections(&self) -> Vec<ConnInfo> {
        let mut live = self.inner.live.lock().unwrap();
        live.conns
            .retain(|_, l| l.shared.upgrade().is_some_and(|s| !s.is_closed()));
        let mut out: Vec<_> = live.conns.values().map(|l| l.info.clone()).collect();
        out.sort_by_key(|i| i.handle);
        out
    }

    // ------------------------------------------------------------------ quiesce

    fn ensure_not_quiesced(&self, server: &ServerKey, database: &str) -> AppResult<()> {
        let scopes = self.inner.quiesced.lock().unwrap();
        if scopes
            .iter()
            .any(|s| &s.server == server && s.databases.contains(database))
        {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("{database} is being replaced by a Refresh workflow; it will be available when the swap finishes"),
            )
            .with_class(crate::ErrorClass::Retriable));
        }
        Ok(())
    }

    fn edit_lock(&self, server: &ServerKey, database: &str) -> EditLock {
        self.inner
            .edit_locks
            .lock()
            .unwrap()
            .entry((server.clone(), database.to_string()))
            .or_default()
            .clone()
    }

    /// Held by a grid while it commits edits. A quiesce waits for in-flight commits and
    /// blocks new ones until released.
    pub async fn edit_guard(
        &self,
        server: &ServerKey,
        database: &str,
    ) -> AppResult<tokio::sync::OwnedRwLockReadGuard<()>> {
        // refuse up front rather than queue behind a swap that will replace the database
        self.ensure_not_quiesced(server, database)?;
        let guard = self.edit_lock(server, database).read_owned().await;
        self.ensure_not_quiesced(server, database)?;
        Ok(guard)
    }

    /// Get pgWhy's own connections out of the way of a swap: wait for in-flight grid
    /// commits, refuse new allocations (no transparent reopens either) for `databases` on
    /// `server`, and close every existing one client-side with a `Quiesced` disposition.
    /// Everything is released when the guard drops.
    pub async fn quiesce(&self, server: &ServerKey, databases: &[String]) -> QuiesceGuard {
        let mut edit_guards = Vec::new();
        let mut sorted: Vec<_> = databases.to_vec();
        sorted.sort();
        sorted.dedup();
        for db in &sorted {
            edit_guards.push(self.edit_lock(server, db).write_owned().await);
        }
        let id = {
            let mut next = self.inner.next_scope.lock().unwrap();
            *next += 1;
            *next
        };
        self.inner.quiesced.lock().unwrap().push(QuiesceScope {
            id,
            server: server.clone(),
            databases: sorted.iter().cloned().collect(),
        });
        let dbs: HashSet<String> = sorted.into_iter().collect();
        self.close_where(
            |l| &l.server == server && dbs.contains(&l.info.database),
            CloseReason::Quiesced,
        )
        .await;
        QuiesceGuard {
            manager: Arc::downgrade(&self.inner),
            id,
            _edit_guards: edit_guards,
        }
    }
}

/// Releases a quiesce on drop.
pub struct QuiesceGuard {
    manager: Weak<Inner>,
    id: u64,
    _edit_guards: Vec<tokio::sync::OwnedRwLockWriteGuard<()>>,
}

impl Drop for QuiesceGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.manager.upgrade() {
            inner.quiesced.lock().unwrap().retain(|s| s.id != self.id);
        }
    }
}

fn connection_relevant_change(old: &ConnectionEntry, new: &ConnectionEntry) -> bool {
    old.host != new.host
        || old.port != new.port
        || old.user != new.user
        || old.sslmode != new.sslmode
        || old.ssl_root_cert != new.ssl_root_cert
        || old.tunnel != new.tunnel
}
