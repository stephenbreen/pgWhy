//! Credential storage facade.
//!
//! Secrets live only in the OS credential store (Keychain, Windows Credential Manager,
//! Secret Service), keyed by connection id so renames keep their binding. When the store is
//! unavailable pgWhy refuses to persist and offers session-only credentials held in memory —
//! there is never a file fallback.
//!
//! The credential flow is one-way: secrets go *into* the store from the UI and come *out*
//! only to open connections; no command response ever carries one.

use crate::config::ConnectionId;
use crate::{AppError, AppResult, ErrorKind};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

/// Keychain service name for every pgWhy entry.
pub const SERVICE: &str = "dev.pgwhy.app";

/// A secret string whose Debug output is redacted and whose memory is cleared on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // best-effort scrub; String's buffer is ours to overwrite
        unsafe { self.0.as_bytes_mut() }.fill(0);
    }
}

/// Which secret of a connection entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    DbPassword,
    SshKeyPassphrase,
    SshPassword,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::DbPassword => "db-password",
            SecretKind::SshKeyPassphrase => "ssh-key-passphrase",
            SecretKind::SshPassword => "ssh-password",
        }
    }

    pub const ALL: [SecretKind; 3] = [
        SecretKind::DbPassword,
        SecretKind::SshKeyPassphrase,
        SecretKind::SshPassword,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretKey {
    pub connection: ConnectionId,
    pub kind: SecretKind,
}

impl SecretKey {
    pub fn new(connection: &ConnectionId, kind: SecretKind) -> Self {
        Self {
            connection: connection.clone(),
            kind,
        }
    }

    /// Reverse-DNS-ish account name under [`SERVICE`].
    pub fn account(&self) -> String {
        format!("connection.{}.{}", self.connection, self.kind.suffix())
    }
}

/// A place secrets can be persisted.
pub trait SecretStore: Send + Sync {
    /// Whether this store can persist secrets at all.
    fn available(&self) -> bool;
    fn get(&self, key: &SecretKey) -> AppResult<Option<Secret>>;
    fn set(&self, key: &SecretKey, secret: &Secret) -> AppResult<()>;
    /// Deleting a missing entry is not an error.
    fn delete(&self, key: &SecretKey) -> AppResult<()>;
}

/// The OS credential store via the `keyring` crate.
#[derive(Debug, Default)]
pub struct KeyringStore;

fn keyring_error(action: &str, e: keyring::Error) -> AppError {
    AppError::new(
        ErrorKind::Credentials,
        format!("the system credential store could not {action}"),
    )
    .with_detail(e.to_string())
}

impl SecretStore for KeyringStore {
    fn available(&self) -> bool {
        keyring::Entry::store_status().is_ok()
    }

    fn get(&self, key: &SecretKey) -> AppResult<Option<Secret>> {
        let entry = keyring::Entry::new(SERVICE, &key.account())
            .map_err(|e| keyring_error("be opened", e))?;
        match entry.get_password() {
            Ok(p) => Ok(Some(Secret::new(p))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_error("read a saved credential", e)),
        }
    }

    fn set(&self, key: &SecretKey, secret: &Secret) -> AppResult<()> {
        let entry = keyring::Entry::new(SERVICE, &key.account())
            .map_err(|e| keyring_error("be opened", e))?;
        entry
            .set_password(secret.expose())
            .map_err(|e| keyring_error("save the credential", e))
    }

    fn delete(&self, key: &SecretKey) -> AppResult<()> {
        let entry = keyring::Entry::new(SERVICE, &key.account())
            .map_err(|e| keyring_error("be opened", e))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_error("delete the credential", e)),
        }
    }
}

/// In-memory store: session-only credentials and tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: Mutex<HashMap<SecretKey, Secret>>,
    /// When false, behaves like an unavailable OS store (tests of the degraded path).
    unavailable: bool,
}

impl MemoryStore {
    pub fn unavailable() -> Self {
        Self {
            inner: Mutex::default(),
            unavailable: true,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretStore for MemoryStore {
    fn available(&self) -> bool {
        !self.unavailable
    }

    fn get(&self, key: &SecretKey) -> AppResult<Option<Secret>> {
        Ok(self.inner.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &SecretKey, secret: &Secret) -> AppResult<()> {
        if self.unavailable {
            return Err(AppError::new(
                ErrorKind::Credentials,
                "no credential store is available on this system",
            ));
        }
        self.inner
            .lock()
            .unwrap()
            .insert(key.clone(), secret.clone());
        Ok(())
    }

    fn delete(&self, key: &SecretKey) -> AppResult<()> {
        self.inner.lock().unwrap().remove(key);
        Ok(())
    }
}

/// How a credential reached a connection (recorded in run history — never the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    Keyring,
    Session,
    None,
}

/// The credential vault the app uses: the persistent store plus session-only overrides.
/// Session secrets win, so "use this password just for now" works even with a saved one.
pub struct Vault {
    persistent: Arc<dyn SecretStore>,
    session: MemoryStore,
}

impl Vault {
    pub fn new(persistent: Arc<dyn SecretStore>) -> Self {
        Self {
            persistent,
            session: MemoryStore::default(),
        }
    }

    pub fn persistent_available(&self) -> bool {
        self.persistent.available()
    }

    /// Save a secret. `persist = false`, or an unavailable store, keeps it for this session
    /// only; the return value says where it went.
    pub fn store(
        &self,
        key: &SecretKey,
        secret: Secret,
        persist: bool,
    ) -> AppResult<CredentialSource> {
        if persist && self.persistent.available() {
            self.persistent.set(key, &secret)?;
            self.session.delete(key)?;
            Ok(CredentialSource::Keyring)
        } else {
            self.session.set(key, &secret)?;
            Ok(CredentialSource::Session)
        }
    }

    pub fn resolve(&self, key: &SecretKey) -> AppResult<(Option<Secret>, CredentialSource)> {
        if let Some(s) = self.session.get(key)? {
            return Ok((Some(s), CredentialSource::Session));
        }
        if self.persistent.available() {
            if let Some(s) = self.persistent.get(key)? {
                return Ok((Some(s), CredentialSource::Keyring));
            }
        }
        Ok((None, CredentialSource::None))
    }

    pub fn forget(&self, key: &SecretKey) -> AppResult<()> {
        self.session.delete(key)?;
        if self.persistent.available() {
            self.persistent.delete(key)?;
        }
        Ok(())
    }

    /// Remove every secret of a deleted connection entry.
    pub fn forget_connection(&self, id: &ConnectionId) -> AppResult<()> {
        for kind in SecretKind::ALL {
            self.forget(&SecretKey::new(id, kind))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_secret() {
        assert_eq!(format!("{:?}", Secret::new("hunter2")), "Secret(***)");
    }

    #[test]
    fn unavailable_store_falls_back_to_session_only() {
        let store = Arc::new(MemoryStore::unavailable());
        let vault = Vault::new(store.clone());
        let key = SecretKey::new(&ConnectionId::new(), SecretKind::DbPassword);
        assert_eq!(
            vault.store(&key, Secret::new("pw"), true).unwrap(),
            CredentialSource::Session
        );
        assert!(
            store.is_empty(),
            "nothing persisted when the store is unavailable"
        );
        let (s, src) = vault.resolve(&key).unwrap();
        assert_eq!(s.unwrap().expose(), "pw");
        assert_eq!(src, CredentialSource::Session);
    }

    #[test]
    fn persistent_round_trip_and_forget_connection() {
        let store = Arc::new(MemoryStore::default());
        let vault = Vault::new(store.clone());
        let id = ConnectionId::new();
        let db = SecretKey::new(&id, SecretKind::DbPassword);
        let ssh = SecretKey::new(&id, SecretKind::SshKeyPassphrase);
        assert_eq!(
            vault.store(&db, Secret::new("a"), true).unwrap(),
            CredentialSource::Keyring
        );
        vault.store(&ssh, Secret::new("b"), true).unwrap();
        assert_eq!(vault.resolve(&db).unwrap().1, CredentialSource::Keyring);
        vault.forget_connection(&id).unwrap();
        assert!(store.is_empty());
        assert_eq!(vault.resolve(&db).unwrap().1, CredentialSource::None);
    }

    #[test]
    fn session_secret_overrides_saved_one() {
        let vault = Vault::new(Arc::new(MemoryStore::default()));
        let key = SecretKey::new(&ConnectionId::new(), SecretKind::DbPassword);
        vault.store(&key, Secret::new("saved"), true).unwrap();
        vault.store(&key, Secret::new("just-now"), false).unwrap();
        assert_eq!(vault.resolve(&key).unwrap().0.unwrap().expose(), "just-now");
    }

    #[test]
    fn account_names_are_keyed_by_stable_id() {
        let id = ConnectionId("1234".into());
        assert_eq!(
            SecretKey::new(&id, SecretKind::DbPassword).account(),
            "connection.1234.db-password"
        );
    }
}
