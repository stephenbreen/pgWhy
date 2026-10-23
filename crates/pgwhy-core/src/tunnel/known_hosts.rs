//! pgWhy's own SSH host-key trust store (`known_hosts.json` in the app config directory).
//!
//! Deliberately not `~/.ssh/known_hosts`: trust decisions made in pgWhy are explicit,
//! recorded actions (trust-on-first-use prompts, deliberate re-trust after a key change), and
//! keeping them in an app-owned file means pgWhy never silently inherits — or rewrites — the
//! user's OpenSSH trust. Entries are keyed `host:port` and pin one key per server: a server
//! presenting any other key is a hard failure until the user re-trusts on purpose.

use crate::config::ConfigStore;
use crate::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// File name under the config directory.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts.json";

/// A host key the user explicitly trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedKey {
    /// SSH key type as the server names it, e.g. `ssh-ed25519`.
    pub key_type: String,
    /// OpenSSH-style SHA-256 fingerprint, e.g. `SHA256:abc…` (unpadded base64).
    pub fingerprint: String,
    /// When the user trusted it, Unix seconds.
    pub trusted_at: u64,
}

/// A key that was trusted and later replaced by a deliberate re-trust (kept as an audit trail).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplacedKey {
    #[serde(flatten)]
    pub key: TrustedKey,
    /// When the re-trust replaced it, Unix seconds.
    pub replaced_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    #[serde(flatten)]
    pub current: TrustedKey,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaced: Vec<ReplacedKey>,
}

/// On-disk shape of `known_hosts.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHostsFile {
    #[serde(default)]
    pub version: u32,
    /// `host:port` → record. Hosts are lowercased; IPv6 literals are bracketed.
    #[serde(default)]
    pub hosts: BTreeMap<String, HostRecord>,
}

/// One entry as listed for the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedHost {
    pub host: String,
    pub port: u16,
    #[serde(flatten)]
    pub record: HostRecord,
}

/// Result of checking a presented key against the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyVerdict {
    Trusted,
    /// Never seen: the user must decide (TOFU prompt).
    Unknown,
    /// A different key was trusted for this host: possible MITM.
    Changed {
        previous: TrustedKey,
    },
}

/// Thread-safe trust store backed by `known_hosts.json`.
#[derive(Debug)]
pub struct HostKeyStore {
    config: ConfigStore,
    file: Mutex<KnownHostsFile>,
}

/// Store key for a server: `host:port`, host lowercased, IPv6 literals bracketed.
pub fn host_key_id(host: &str, port: u16) -> String {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let host = host.to_ascii_lowercase();
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn split_host_key_id(id: &str) -> Option<(String, u16)> {
    let (host, port) = id.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some((host.to_string(), port.parse().ok()?))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl HostKeyStore {
    /// Load the store from `config`'s directory (a missing file is an empty store).
    pub fn open(config: ConfigStore) -> AppResult<Self> {
        let file: KnownHostsFile = config.load_json(KNOWN_HOSTS_FILE)?;
        Ok(Self {
            config,
            file: Mutex::new(file),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, KnownHostsFile> {
        // A poisoned lock only means another thread panicked mid-update of an in-memory
        // copy that is re-saved whole; the data is still a valid file image.
        self.file.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn check(
        &self,
        host: &str,
        port: u16,
        key_type: &str,
        fingerprint: &str,
    ) -> HostKeyVerdict {
        let file = self.lock();
        match file.hosts.get(&host_key_id(host, port)) {
            None => HostKeyVerdict::Unknown,
            Some(r) if r.current.key_type == key_type && r.current.fingerprint == fingerprint => {
                HostKeyVerdict::Trusted
            }
            Some(r) => HostKeyVerdict::Changed {
                previous: r.current.clone(),
            },
        }
    }

    /// Record an explicit trust decision. Replacing a different key keeps it in the record's
    /// `replaced` history, so a re-trust after a key change is auditable.
    pub fn trust(&self, host: &str, port: u16, key_type: &str, fingerprint: &str) -> AppResult<()> {
        validate_fingerprint(fingerprint)?;
        if key_type.trim().is_empty() {
            return Err(AppError::new(
                ErrorKind::Config,
                "the host key type is missing",
            ));
        }
        let mut file = self.lock();
        let now = now_unix();
        let key = TrustedKey {
            key_type: key_type.to_string(),
            fingerprint: fingerprint.to_string(),
            trusted_at: now,
        };
        let id = host_key_id(host, port);
        let mut next = file.clone();
        next.version = 1;
        match next.hosts.get_mut(&id) {
            Some(r) if r.current.key_type == key_type && r.current.fingerprint == fingerprint => {
                return Ok(());
            }
            Some(r) => {
                let old = std::mem::replace(&mut r.current, key);
                r.replaced.push(ReplacedKey {
                    key: old,
                    replaced_at: now,
                });
            }
            None => {
                next.hosts.insert(
                    id,
                    HostRecord {
                        current: key,
                        replaced: Vec::new(),
                    },
                );
            }
        }
        self.config.save_json(KNOWN_HOSTS_FILE, &next)?;
        *file = next;
        Ok(())
    }

    /// Remove a host's trust entirely (its next connection prompts again).
    pub fn forget(&self, host: &str, port: u16) -> AppResult<bool> {
        let mut file = self.lock();
        let mut next = file.clone();
        let removed = next.hosts.remove(&host_key_id(host, port)).is_some();
        if removed {
            self.config.save_json(KNOWN_HOSTS_FILE, &next)?;
            *file = next;
        }
        Ok(removed)
    }

    pub fn list(&self) -> Vec<TrustedHost> {
        self.lock()
            .hosts
            .iter()
            .filter_map(|(id, r)| {
                let (host, port) = split_host_key_id(id)?;
                Some(TrustedHost {
                    host,
                    port,
                    record: r.clone(),
                })
            })
            .collect()
    }
}

fn validate_fingerprint(fp: &str) -> AppResult<()> {
    let ok = fp.strip_prefix("SHA256:").is_some_and(|b| {
        !b.is_empty()
            && b.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/')
    });
    if ok {
        Ok(())
    } else {
        Err(AppError::new(
            ErrorKind::Config,
            "host key fingerprints must be SHA256 fingerprints (SHA256:…)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP_A: &str = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const FP_B: &str = "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn store() -> (tempfile::TempDir, HostKeyStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = HostKeyStore::open(ConfigStore::new(dir.path())).unwrap();
        (dir, s)
    }

    #[test]
    fn unknown_then_trusted_and_persisted() {
        let (dir, s) = store();
        assert_eq!(
            s.check("Bastion", 22, "ssh-ed25519", FP_A),
            HostKeyVerdict::Unknown
        );
        s.trust("Bastion", 22, "ssh-ed25519", FP_A).unwrap();
        assert_eq!(
            s.check("bastion", 22, "ssh-ed25519", FP_A),
            HostKeyVerdict::Trusted
        );
        // a different port is a different server
        assert_eq!(
            s.check("bastion", 2222, "ssh-ed25519", FP_A),
            HostKeyVerdict::Unknown
        );
        let reopened = HostKeyStore::open(ConfigStore::new(dir.path())).unwrap();
        assert_eq!(
            reopened.check("bastion", 22, "ssh-ed25519", FP_A),
            HostKeyVerdict::Trusted
        );
        assert_eq!(reopened.list()[0].host, "bastion");
        assert_eq!(reopened.list()[0].port, 22);
    }

    #[test]
    fn changed_key_reports_previous_and_retrust_is_recorded() {
        let (_dir, s) = store();
        s.trust("h", 22, "ssh-ed25519", FP_A).unwrap();
        match s.check("h", 22, "ssh-ed25519", FP_B) {
            HostKeyVerdict::Changed { previous } => assert_eq!(previous.fingerprint, FP_A),
            v => panic!("expected Changed, got {v:?}"),
        }
        s.trust("h", 22, "ssh-ed25519", FP_B).unwrap();
        assert_eq!(
            s.check("h", 22, "ssh-ed25519", FP_B),
            HostKeyVerdict::Trusted
        );
        let rec = &s.list()[0].record;
        assert_eq!(rec.replaced.len(), 1);
        assert_eq!(rec.replaced[0].key.fingerprint, FP_A);
    }

    #[test]
    fn rejects_non_sha256_fingerprints_and_handles_ipv6() {
        let (_dir, s) = store();
        assert!(s.trust("h", 22, "ssh-ed25519", "MD5:aa:bb").is_err());
        assert_eq!(host_key_id("[::1]", 22), "[::1]:22");
        assert_eq!(host_key_id("::1", 22), "[::1]:22");
        s.trust("::1", 22, "ssh-ed25519", FP_A).unwrap();
        assert_eq!(s.list()[0].host, "::1");
        assert!(s.forget("[::1]", 22).unwrap());
        assert!(s.list().is_empty());
    }
}
