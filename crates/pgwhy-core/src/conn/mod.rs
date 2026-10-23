//! Connections: TLS, credentials, tunnels (via [`TunnelProvider`]), and the allocation model.
//!
//! Allocation model (plan RV-5):
//! - one shared, read-only **introspection** connection per (entry, database), transparently
//!   health-checked and reopened — used by the tree, details panel and completion;
//! - one **dedicated** connection per editor tab, per open grid, and per workflow step —
//!   never silently reconnected; loss is reported as `Disconnected`.
//!
//! Everything multiplexes over the entry's single tunnel when it has one.

mod classify;
mod manager;
mod session;
mod tls;
mod tunnel_api;

pub use classify::classify;
pub use manager::{CloseReason, ConnEvent, ConnectionManager, ManagerOptions, QuiesceGuard};
pub use session::{ConnHandle, ConnInfo, Notice, PgConn, Purpose};
pub use tls::TlsState;
pub use tunnel_api::{BoxFuture, Endpoint, NoTunnels, TunnelLease, TunnelProvider};
