//! One russh client session: algorithm floor, host-key verification against the trust store,
//! credential resolution, and russh errors → the pgWhy taxonomy.

use super::known_hosts::{HostKeyStore, HostKeyVerdict};
use super::TunnelOptions;
use crate::config::{ConnectionEntry, TunnelAuth, TunnelConfig};
use crate::secret::{Secret, SecretKey, SecretKind, Vault};
use crate::{AppError, AppResult, ErrorClass, ErrorKind};
use russh::client::{self, DisconnectReason, Handle};
use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use serde_json::json;
use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Credentials for a tunnel, resolved once when it starts so reconnects never need a prompt.
#[derive(Clone)]
pub(super) enum AuthMaterial {
    Key(Arc<PrivateKey>),
    Password(Secret),
}

/// Resolve the SSH credential for `entry` (reading and decrypting the key file if needed).
/// A missing passphrase/password is a `Credentials` error with a `data` payload the UI turns
/// into a prompt; nothing is attempted against the server.
pub(super) fn resolve_auth(entry: &ConnectionEntry, vault: &Vault) -> AppResult<AuthMaterial> {
    let tunnel = tunnel_config(entry)?;
    match &tunnel.auth {
        TunnelAuth::Password { .. } => {
            let (pw, _) = vault.resolve(&SecretKey::new(&entry.id, SecretKind::SshPassword))?;
            match pw {
                Some(pw) => Ok(AuthMaterial::Password(pw)),
                None => Err(missing_secret(entry, tunnel, SecretKind::SshPassword)),
            }
        }
        TunnelAuth::Key { key_path, .. } => {
            let text = std::fs::read_to_string(key_path).map_err(|e| {
                AppError::new(
                    ErrorKind::Config,
                    format!("could not read the SSH key file {}", key_path.display()),
                )
                .with_detail(e.to_string())
                .with_hint("check the key path on the connection's SSH tab")
            })?;
            match russh::keys::decode_secret_key(&text, None) {
                Ok(key) => checked_user_key(key, key_path),
                Err(e) if looks_encrypted(&e, &text) => {
                    let (pass, _) =
                        vault.resolve(&SecretKey::new(&entry.id, SecretKind::SshKeyPassphrase))?;
                    let Some(pass) = pass else {
                        return Err(missing_secret(entry, tunnel, SecretKind::SshKeyPassphrase));
                    };
                    match russh::keys::decode_secret_key(&text, Some(pass.expose())) {
                        Ok(key) => checked_user_key(key, key_path),
                        Err(_) => Err(AppError::new(
                            ErrorKind::Credentials,
                            format!(
                                "the passphrase for SSH key {} is incorrect",
                                key_path.display()
                            ),
                        )
                        .with_hint("enter the key's passphrase again")
                        .with_data(json!({
                            "connection_id": entry.id,
                            "secret": "ssh_key_passphrase",
                            "reason": "incorrect",
                        }))),
                    }
                }
                Err(e) => Err(AppError::new(
                    ErrorKind::Config,
                    format!("{} is not a usable SSH private key", key_path.display()),
                )
                .with_detail(e.to_string())
                .with_hint(
                    "use an OpenSSH, PEM or PuTTY private key file (ed25519, ECDSA or RSA)",
                )),
            }
        }
    }
}

/// Which secrets starting this entry's tunnel would need that are not available — the
/// "resolve before run" check workflows run at preflight. Does not touch the network.
pub(super) fn missing_secrets(
    entry: &ConnectionEntry,
    vault: &Vault,
) -> AppResult<Vec<SecretKind>> {
    match resolve_auth(entry, vault) {
        Ok(_) => Ok(Vec::new()),
        Err(e) if e.kind == ErrorKind::Credentials => {
            let kind = match e
                .data
                .as_ref()
                .and_then(|d| d.get("secret"))
                .and_then(|s| s.as_str())
            {
                Some("ssh_password") => SecretKind::SshPassword,
                _ => SecretKind::SshKeyPassphrase,
            };
            Ok(vec![kind])
        }
        Err(e) => Err(e),
    }
}

fn looks_encrypted(e: &russh::keys::Error, text: &str) -> bool {
    matches!(e, russh::keys::Error::KeyIsEncrypted)
        || text.contains("ENCRYPTED")
        || text.contains("Encryption: aes")
}

fn checked_user_key(key: PrivateKey, path: &Path) -> AppResult<AuthMaterial> {
    if matches!(key.algorithm(), Algorithm::Dsa) {
        return Err(AppError::new(
            ErrorKind::Config,
            format!(
                "{} is a DSA key, which pgWhy refuses as too weak",
                path.display()
            ),
        )
        .with_hint("generate an ed25519 key (ssh-keygen -t ed25519)"));
    }
    Ok(AuthMaterial::Key(Arc::new(key)))
}

fn missing_secret(entry: &ConnectionEntry, tunnel: &TunnelConfig, kind: SecretKind) -> AppError {
    let (what, secret) = match kind {
        SecretKind::SshPassword => ("the SSH password", "ssh_password"),
        _ => ("the SSH key passphrase", "ssh_key_passphrase"),
    };
    AppError::new(
        ErrorKind::Credentials,
        format!(
            "{what} for {}@{} is needed to open the tunnel",
            tunnel.user, tunnel.host
        ),
    )
    .with_hint(format!("enter {what}"))
    .with_data(json!({
        "connection_id": entry.id,
        "secret": secret,
        "reason": "missing",
    }))
}

pub(super) fn tunnel_config(entry: &ConnectionEntry) -> AppResult<&TunnelConfig> {
    entry
        .tunnel
        .as_ref()
        .ok_or_else(|| AppError::internal(format!("{} has no SSH tunnel configured", entry.name)))
}

/// Client config with the algorithm floor applied: no `ssh-rsa` (SHA-1) host-key signatures.
/// DSA is not compiled in at all (russh's `dsa` feature is off); SHA-1 MACs are not in
/// russh's defaults.
pub(super) fn client_config(opts: &TunnelOptions) -> client::Config {
    let mut preferred = russh::Preferred::default();
    let keys: Vec<Algorithm> = preferred
        .key
        .iter()
        .filter(|a| host_key_algorithm_allowed(a))
        .cloned()
        .collect();
    preferred.key = Cow::Owned(keys);
    client::Config {
        preferred,
        keepalive_interval: Some(opts.keepalive_interval),
        keepalive_max: opts.keepalive_max,
        nodelay: true,
        ..Default::default()
    }
}

fn host_key_algorithm_allowed(a: &Algorithm) -> bool {
    match a {
        Algorithm::Ed25519 | Algorithm::Ecdsa { .. } => true,
        Algorithm::Rsa { hash } => hash.is_some(),
        _ => false,
    }
}

/// Signals the end of a session: sent with a reason from `disconnected`, or simply dropped
/// with the handler when russh's session task ends any other way.
pub(super) type EndedRx = oneshot::Receiver<Option<String>>;

pub(super) struct ClientHandler {
    store: Arc<HostKeyStore>,
    host: String,
    port: u16,
    verdict: Arc<Mutex<Option<AppError>>>,
    ended: Option<oneshot::Sender<Option<String>>>,
}

impl ClientHandler {
    fn evaluate(&self, presented: &PublicKeyOrCertificate) -> Result<(), AppError> {
        let PublicKeyOrCertificate::PublicKey { key, hash_alg } = presented else {
            return Err(AppError::new(
                ErrorKind::HostKey,
                format!(
                    "{}:{} presented an SSH host certificate; pgWhy trusts plain host keys only",
                    self.host, self.port
                ),
            ));
        };
        let algorithm = key.algorithm();
        let allowed = match &algorithm {
            Algorithm::Rsa { .. } => hash_alg.is_some(),
            a => host_key_algorithm_allowed(a),
        };
        if !allowed {
            return Err(AppError::new(
                ErrorKind::HostKey,
                format!(
                    "{}:{} offered a {} host key signature, which pgWhy refuses as too weak",
                    self.host,
                    self.port,
                    if matches!(algorithm, Algorithm::Rsa { .. }) {
                        "ssh-rsa (SHA-1)"
                    } else {
                        algorithm.as_str()
                    }
                ),
            )
            .with_hint(
                "configure the SSH server with an ed25519 or ECDSA host key, or rsa-sha2-256/512",
            ));
        }
        let key_type = algorithm.as_str().to_string();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        match self.store.check(&self.host, self.port, &key_type, &fingerprint) {
            HostKeyVerdict::Trusted => Ok(()),
            HostKeyVerdict::Unknown => Err(AppError::new(
                ErrorKind::HostKey,
                format!(
                    "the SSH host key of {}:{} is not trusted yet",
                    self.host, self.port
                ),
            )
            .with_hint("compare the fingerprint with the server's, then trust it to connect")
            .with_data(json!({
                "host": self.host,
                "port": self.port,
                "key_type": key_type,
                "fingerprint": fingerprint,
                "status": "unknown",
            }))),
            HostKeyVerdict::Changed { previous } => Err(AppError::new(
                ErrorKind::HostKey,
                format!(
                    "host key changed — possible MITM: {}:{} presented {} {} but {} {} is trusted",
                    self.host,
                    self.port,
                    key_type,
                    fingerprint,
                    previous.key_type,
                    previous.fingerprint
                ),
            )
            .with_hint(
                "do not connect unless the server's administrator confirms the key was replaced; then re-trust the new key",
            )
            .with_data(json!({
                "host": self.host,
                "port": self.port,
                "key_type": key_type,
                "fingerprint": fingerprint,
                "status": "changed",
                "previous_key_type": previous.key_type,
                "previous_fingerprint": previous.fingerprint,
                "previous_trusted_at": previous.trusted_at,
            }))),
        }
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match self.evaluate(server_public_key) {
            Ok(()) => Ok(true),
            Err(e) => {
                *self.verdict.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                Ok(false)
            }
        }
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let text = match &reason {
            DisconnectReason::ReceivedDisconnect(info) => {
                format!("the SSH server closed the session: {}", info.message)
            }
            DisconnectReason::Error(e) => format!("the SSH session failed: {e}"),
        };
        if let Some(tx) = self.ended.take() {
            let _ = tx.send(Some(text));
        }
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(e) => Err(e),
        }
    }
}

pub(super) type SshHandle = Handle<ClientHandler>;

/// An authenticated session plus its end-of-life signal.
pub(super) struct Session {
    pub handle: Arc<SshHandle>,
    pub ended: EndedRx,
}

/// Connect, verify the host key, and authenticate.
pub(super) async fn open_session(
    tunnel: &TunnelConfig,
    auth: &AuthMaterial,
    store: Arc<HostKeyStore>,
    opts: &TunnelOptions,
) -> AppResult<Session> {
    let verdict: Arc<Mutex<Option<AppError>>> = Arc::default();
    let (ended_tx, ended_rx) = oneshot::channel();
    let handler = ClientHandler {
        store,
        host: tunnel.host.clone(),
        port: tunnel.port,
        verdict: verdict.clone(),
        ended: Some(ended_tx),
    };
    let target = format!("{}@{}:{}", tunnel.user, tunnel.host, tunnel.port);
    let config = Arc::new(client_config(opts));
    let connect = client::connect(config, (tunnel.host.as_str(), tunnel.port), handler);
    let mut handle = match tokio::time::timeout(opts.connect_timeout, connect).await {
        Err(_) => {
            return Err(AppError::new(
                ErrorKind::Tunnel,
                format!(
                    "could not reach the SSH server {}:{}: the connection timed out",
                    tunnel.host, tunnel.port
                ),
            ))
        }
        Ok(Err(e)) => {
            if let Some(v) = verdict.lock().unwrap_or_else(|p| p.into_inner()).take() {
                return Err(v);
            }
            return Err(map_russh(
                &e,
                &format!("connect to the SSH server {}:{}", tunnel.host, tunnel.port),
            ));
        }
        Ok(Ok(h)) => h,
    };

    let result = match auth {
        AuthMaterial::Password(pw) => handle
            .authenticate_password(tunnel.user.clone(), pw.expose())
            .await
            .map_err(|e| map_russh(&e, &format!("authenticate as {target}")))?,
        AuthMaterial::Key(key) => {
            let hash = if key.algorithm().is_rsa() {
                // Never let an RSA user key fall back to ssh-rsa (SHA-1) signatures.
                match handle.best_supported_rsa_hash().await {
                    Ok(Some(Some(h))) => Some(h),
                    Ok(Some(None)) => {
                        return Err(AppError::new(
                            ErrorKind::Auth,
                            format!("{target} only accepts SHA-1 (ssh-rsa) signatures for RSA keys, which pgWhy refuses"),
                        )
                        .with_hint("use an ed25519 key, or enable rsa-sha2-256/512 on the server"));
                    }
                    // server didn't advertise server-sig-algs: try SHA-256
                    _ => Some(HashAlg::Sha256),
                }
            } else {
                None
            };
            handle
                .authenticate_publickey(
                    tunnel.user.clone(),
                    PrivateKeyWithHashAlg::new(key.clone(), hash),
                )
                .await
                .map_err(|e| map_russh(&e, &format!("authenticate as {target}")))?
        }
    };
    if !result.success() {
        let what = match auth {
            AuthMaterial::Password(_) => "password",
            AuthMaterial::Key(_) => "key",
        };
        return Err(AppError::new(
            ErrorKind::Auth,
            format!("the SSH server rejected the {what} for {target}"),
        )
        .with_hint(format!(
            "check the SSH user and {what} on the connection's SSH tab"
        )));
    }
    Ok(Session {
        handle: Arc::new(handle),
        ended: ended_rx,
    })
}

/// russh error → AppError. Transport problems are retriable `Tunnel` errors; negotiation
/// failures (no acceptable algorithm) need the server reconfigured.
pub(super) fn map_russh(e: &russh::Error, context: &str) -> AppError {
    use russh::Error as E;
    match e {
        E::IO(io) => AppError::new(
            ErrorKind::Tunnel,
            format!("could not {context}: {}", io_phrase(io)),
        )
        .with_detail(io.to_string()),
        E::NoCommonAlgo { kind, .. } => AppError::new(
            ErrorKind::Tunnel,
            format!("could not {context}: the server offers no {kind:?} algorithm pgWhy accepts"),
        )
        .with_class(ErrorClass::FixThenRetry)
        .with_detail(e.to_string())
        .with_hint("pgWhy refuses SHA-1 and DSA; the server needs modern algorithms (ed25519/ECDSA/rsa-sha2)"),
        E::UnknownKey | E::WrongServerSig => AppError::new(
            ErrorKind::HostKey,
            format!("could not {context}: the server's host key could not be verified"),
        )
        .with_detail(e.to_string()),
        E::Keys(k) => AppError::new(ErrorKind::Tunnel, format!("could not {context}"))
            .with_class(ErrorClass::FixThenRetry)
            .with_detail(k.to_string()),
        _ => AppError::new(ErrorKind::Tunnel, format!("could not {context}"))
            .with_detail(e.to_string()),
    }
}

fn io_phrase(io: &std::io::Error) -> &'static str {
    use std::io::ErrorKind as K;
    match io.kind() {
        K::ConnectionRefused => "the connection was refused (is sshd running on that port?)",
        K::TimedOut => "the connection timed out",
        K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof => {
            "the connection was reset"
        }
        K::HostUnreachable | K::NetworkUnreachable => "the server is unreachable",
        K::NotFound | K::AddrNotAvailable => "the host name could not be resolved",
        _ => "a network error occurred",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithm_floor_drops_sha1_rsa_and_dsa() {
        let cfg = client_config(&TunnelOptions::default());
        assert!(cfg.preferred.key.contains(&Algorithm::Ed25519));
        assert!(cfg.preferred.key.contains(&Algorithm::Rsa {
            hash: Some(HashAlg::Sha256)
        }));
        assert!(!cfg.preferred.key.contains(&Algorithm::Rsa { hash: None }));
        assert!(!cfg.preferred.key.contains(&Algorithm::Dsa));
    }

    fn handler(store: Arc<HostKeyStore>) -> ClientHandler {
        ClientHandler {
            store,
            host: "bastion".into(),
            port: 22,
            verdict: Arc::default(),
            ended: None,
        }
    }

    #[test]
    fn host_key_verdicts_carry_the_tofu_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(HostKeyStore::open(crate::config::ConfigStore::new(dir.path())).unwrap());
        let key =
            PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519).unwrap();
        let h = handler(store.clone());
        // ed25519, unknown → TOFU payload
        let e = h
            .evaluate(&PublicKeyOrCertificate::from(key.public_key().clone()))
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::HostKey);
        let data = e.data.unwrap();
        assert_eq!(data["status"], "unknown");
        assert_eq!(data["key_type"], "ssh-ed25519");
        assert!(data["fingerprint"].as_str().unwrap().starts_with("SHA256:"));
        // trusted → ok
        store
            .trust(
                "bastion",
                22,
                "ssh-ed25519",
                data["fingerprint"].as_str().unwrap(),
            )
            .unwrap();
        h.evaluate(&PublicKeyOrCertificate::from(key.public_key().clone()))
            .unwrap();
    }
}
