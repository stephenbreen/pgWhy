//! Per-target-server run lock: at most one Refresh run — or pending confirmation, or
//! recovery action — touches a server at a time, because two swaps on one server race on
//! session termination and renames. Process-local by design: it is sound only because the app
//! enforces a single instance.

use pgwhy_core::config::ServerKey;
use pgwhy_core::{AppError, AppResult, ErrorKind};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The lock table. Cheap to clone; clones share state.
#[derive(Debug, Clone, Default)]
pub struct RunLocks {
    held: Arc<Mutex<HashMap<ServerKey, String>>>,
}

impl RunLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the lock for `server`, or refuse naming who holds it. `holder` is a short
    /// user-facing description ("the Refresh \"Local app\" run").
    pub fn try_acquire(
        &self,
        server: &ServerKey,
        holder: impl Into<String>,
    ) -> AppResult<RunLockGuard> {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(who) = held.get(server) {
            return Err(AppError::new(
                ErrorKind::Refused,
                format!("{who} is already using the server {server}"),
            )
            .with_hint("wait for it to finish (or cancel it), then run again")
            .with_class(pgwhy_core::ErrorClass::Retriable));
        }
        held.insert(server.clone(), holder.into());
        Ok(RunLockGuard {
            locks: self.held.clone(),
            server: server.clone(),
        })
    }

    /// Who holds `server`'s lock, if anyone.
    pub fn holder(&self, server: &ServerKey) -> Option<String> {
        self.held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(server)
            .cloned()
    }
}

/// Releases the server's lock on drop.
#[derive(Debug)]
pub struct RunLockGuard {
    locks: Arc<Mutex<HashMap<ServerKey, String>>>,
    server: ServerKey,
}

impl RunLockGuard {
    pub fn server(&self) -> &ServerKey {
        &self.server
    }
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        self.locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(port: u16) -> ServerKey {
        ServerKey {
            host: "localhost".into(),
            port,
            via: None,
        }
    }

    #[test]
    fn one_holder_per_server_until_released() {
        let locks = RunLocks::new();
        let g = locks.try_acquire(&key(5432), "run A").unwrap();
        let err = locks.try_acquire(&key(5432), "run B").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Refused);
        assert!(err.message.contains("run A"), "{}", err.message);
        let other = locks.try_acquire(&key(5433), "run C").unwrap();
        drop(g);
        assert!(locks.holder(&key(5432)).is_none());
        locks.try_acquire(&key(5432), "run B").unwrap();
        drop(other);
    }
}
