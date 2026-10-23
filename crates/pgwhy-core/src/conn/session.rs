//! One live server connection: the client, its cancel token, notices, and liveness.

use super::classify::classify;
use super::tls::{self, TlsState};
use super::tunnel_api::{Endpoint, TunnelLease};
use crate::config::{ConnectionEntry, ConnectionId};
use crate::secret::Secret;
use crate::AppResult;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_postgres::{AsyncMessage, CancelToken, Client};

/// Process-unique id of one live connection.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ConnHandle(pub u64);

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

impl ConnHandle {
    pub(crate) fn next() -> Self {
        Self(NEXT_HANDLE.fetch_add(1, Ordering::Relaxed))
    }
}

/// What a connection is for (allocation model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Introspection,
    EditorTab,
    Grid,
    Workflow,
    Maintenance,
    Test,
}

impl Purpose {
    fn application_name(self) -> &'static str {
        match self {
            Purpose::Introspection => "pgWhy (explorer)",
            Purpose::EditorTab => "pgWhy (editor)",
            Purpose::Grid => "pgWhy (grid)",
            Purpose::Workflow => "pgWhy (workflow)",
            Purpose::Maintenance => "pgWhy (maintenance)",
            Purpose::Test => "pgWhy (test)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnInfo {
    pub handle: ConnHandle,
    pub connection_id: ConnectionId,
    pub database: String,
    pub purpose: Purpose,
    pub server_version_num: i32,
    pub backend_pid: i32,
    pub tls: TlsState,
}

impl ConnInfo {
    pub fn server_major(&self) -> u32 {
        (self.server_version_num / 10000) as u32
    }
}

/// A server NOTICE/WARNING/INFO message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Notice {
    pub severity: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Liveness shared between the connection, its driver task and the manager's registry.
#[derive(Debug)]
pub(crate) struct ConnShared {
    closed: AtomicBool,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
    last_used: Mutex<Instant>,
    closed_tx: watch::Sender<bool>,
    lost_error: Mutex<Option<String>>,
}

impl ConnShared {
    fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            abort: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
            closed_tx: watch::channel(false).0,
            lost_error: Mutex::new(None),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        self.closed_tx.send_replace(true);
    }

    /// Close client-side: drops the socket by aborting the driver task.
    pub(crate) fn close(&self) {
        if let Some(h) = self.abort.lock().unwrap().take() {
            h.abort();
        }
        self.mark_closed();
    }

    pub(crate) fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }

    pub(crate) fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    pub(crate) fn lost_error(&self) -> Option<String> {
        self.lost_error.lock().unwrap().clone()
    }
}

/// Called once when the server (not pgWhy) ends a connection, with the connection's handle
/// (allocated before the driver starts, so even a loss during setup can name it) and the error
/// text if any.
pub(crate) type LostCallback = Box<dyn FnOnce(ConnHandle, Option<String>) + Send + 'static>;

/// A live connection. Dropping it closes the socket.
pub struct PgConn {
    pub client: Client,
    pub info: ConnInfo,
    cancel: CancelToken,
    /// The same TLS policy the connection used: a cancel request opens a fresh socket and must
    /// negotiate TLS exactly like the session did (query::CancelHandle).
    cancel_tls: tokio_postgres_rustls::MakeRustlsConnect,
    notices: Mutex<Option<mpsc::UnboundedReceiver<Notice>>>,
    pub(crate) shared: Arc<ConnShared>,
    _lease: Option<TunnelLease>,
}

impl std::fmt::Debug for PgConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConn")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl PgConn {
    pub fn handle(&self) -> ConnHandle {
        self.info.handle
    }

    /// True once the server or the client side has closed the connection.
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed() || self.client.is_closed()
    }

    /// Resolves when the connection closes (for tab "Disconnected" states).
    pub fn closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.shared.closed_tx.subscribe();
        async move {
            let _ = rx.wait_for(|c| *c).await;
        }
    }

    /// The protocol-level cancel token (psql's Ctrl+C).
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// The TLS connector a cancel request must use (paired with [`Self::cancel_token`]).
    pub(crate) fn cancel_tls(&self) -> tokio_postgres_rustls::MakeRustlsConnect {
        self.cancel_tls.clone()
    }

    /// Take the notice stream (one consumer: the executor or the editor tab).
    pub fn take_notices(&self) -> Option<mpsc::UnboundedReceiver<Notice>> {
        self.notices.lock().unwrap().take()
    }

    pub fn touch(&self) {
        self.shared.touch();
    }

    pub(crate) async fn connect(
        entry: &ConnectionEntry,
        database: &str,
        purpose: Purpose,
        password: Option<&Secret>,
        lease: Option<TunnelLease>,
        on_lost: Option<LostCallback>,
    ) -> AppResult<Self> {
        let endpoint = Endpoint::for_entry(entry, lease.as_ref());
        let mut config = tokio_postgres::Config::new();
        config
            .host(&endpoint.host)
            .port(endpoint.port)
            .user(&entry.user)
            .dbname(database)
            .application_name(purpose.application_name())
            .connect_timeout(Duration::from_secs(10))
            .keepalives(true)
            .keepalives_idle(Duration::from_secs(30));
        if let Some(addr) = endpoint.hostaddr {
            config.hostaddr(addr);
        }
        if let Some(pw) = password {
            config.password(pw.expose());
        }
        let setup = tls::setup(entry.sslmode, entry.ssl_root_cert.as_deref())?;
        config.ssl_mode(setup.mode);

        let context = format!("connect to {}:{}/{}", entry.host, entry.port, database);
        let (client, mut connection) = config
            .connect(setup.connector.clone())
            .await
            .map_err(|e| classify(&e, &context))?;

        let handle = ConnHandle::next();
        let shared = Arc::new(ConnShared::new());
        let (ntx, nrx) = mpsc::unbounded_channel();
        let driver_shared = shared.clone();
        let task = tokio::spawn(async move {
            let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
            while let Some(msg) = messages.next().await {
                match msg {
                    Ok(AsyncMessage::Notice(n)) => {
                        let _ = ntx.send(Notice {
                            severity: n.severity().to_string(),
                            code: n.code().code().to_string(),
                            message: n.message().to_string(),
                            detail: n.detail().map(str::to_string),
                            hint: n.hint().map(str::to_string),
                        });
                    }
                    Ok(_) => {}
                    Err(e) => {
                        *driver_shared.lost_error.lock().unwrap() = Some(e.to_string());
                        break;
                    }
                }
            }
            // Reached only when the server side ended the connection: a client-side close
            // aborts this task before it gets here.
            driver_shared.mark_closed();
            if let Some(cb) = on_lost {
                cb(handle, driver_shared.lost_error());
            }
        });
        *shared.abort.lock().unwrap() = Some(task.abort_handle());

        let row = client
            .query_one(
                "select current_setting('server_version_num')::int4, pg_backend_pid(), \
                 coalesce((select ssl from pg_stat_ssl where pid = pg_backend_pid()), false)",
                &[],
            )
            .await
            .map_err(|e| classify(&e, &context))?;
        let encrypted: bool = row.get(2);
        let info = ConnInfo {
            handle,
            connection_id: entry.id.clone(),
            database: database.to_string(),
            purpose,
            server_version_num: row.get(0),
            backend_pid: row.get(1),
            tls: tls::resulting_state(entry.sslmode, encrypted),
        };
        let cancel = client.cancel_token();
        Ok(Self {
            client,
            info,
            cancel,
            cancel_tls: setup.connector,
            notices: Mutex::new(Some(nrx)),
            shared,
            _lease: lease,
        })
    }

    /// Cheap round trip used before reusing an idle shared connection.
    pub(crate) async fn ping(&self) -> bool {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), self.client.simple_query("")).await,
            Ok(Ok(_))
        )
    }
}

impl Drop for PgConn {
    fn drop(&mut self) {
        self.shared.close();
    }
}
